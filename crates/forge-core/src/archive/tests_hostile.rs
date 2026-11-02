//! Hostile archives: bombs, overlaps, header mismatches, names, collisions, MOTW.
//!
//! Archives are generated here: normal entries through `zip::ZipWriter`, and the malformed
//! ones by assembling local headers, central records and the end record by hand.

use super::*;
use forge_jobs::{CancelToken, Progress};
use std::fs::File;
use std::io::Write;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Bitwise CRC-32 (IEEE 802.3), table-less.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// One hand-written entry. Every header field can be patched after [`Ent::stored`].
struct Ent {
    name: Vec<u8>,
    /// Name in the local header when it differs from the central one.
    local_name: Option<Vec<u8>>,
    flags: u16,
    method: u16,
    crc: u32,
    csize: u32,
    size: u32,
    extra: Vec<u8>,
    data: Vec<u8>,
}

impl Ent {
    fn stored(name: &[u8], data: &[u8]) -> Self {
        Self {
            name: name.to_vec(),
            local_name: None,
            flags: 0,
            method: 0,
            crc: crc32(data),
            csize: data.len() as u32,
            size: data.len() as u32,
            extra: Vec::new(),
            data: data.to_vec(),
        }
    }
}

/// Assemble a ZIP: a local record per entry, then one central record per index in
/// `central` (repeat an index to point two records at the same data), then the end record.
fn zip_bytes(ents: &[Ent], central: &[usize]) -> Vec<u8> {
    let records: Vec<(usize, usize)> = central.iter().map(|&i| (i, i)).collect();
    zip_records(ents, &records)
}

/// As [`zip_bytes`], but each `(meta, local)` pair writes the central record of entry `meta`
/// (name, sizes, flags) with the local header offset of entry `local`.
fn zip_records(ents: &[Ent], records: &[(usize, usize)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offsets = Vec::new();
    for e in ents {
        offsets.push(out.len() as u32);
        let local_name = e.local_name.as_deref().unwrap_or(&e.name);
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(20u16.to_le_bytes()); // version needed
        out.extend(e.flags.to_le_bytes());
        out.extend(e.method.to_le_bytes());
        out.extend(0u16.to_le_bytes()); // time
        out.extend(0x21u16.to_le_bytes()); // date 1980-01-01
        out.extend(e.crc.to_le_bytes());
        out.extend(e.csize.to_le_bytes());
        out.extend(e.size.to_le_bytes());
        out.extend((local_name.len() as u16).to_le_bytes());
        out.extend((e.extra.len() as u16).to_le_bytes());
        out.extend(local_name);
        out.extend(&e.extra);
        out.extend(&e.data);
    }
    let cd_start = out.len();
    for &(meta, local) in records {
        let e = &ents[meta];
        out.extend(0x0201_4b50u32.to_le_bytes());
        out.extend(20u16.to_le_bytes()); // version made by
        out.extend(20u16.to_le_bytes()); // version needed
        out.extend(e.flags.to_le_bytes());
        out.extend(e.method.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0x21u16.to_le_bytes());
        out.extend(e.crc.to_le_bytes());
        out.extend(e.csize.to_le_bytes());
        out.extend(e.size.to_le_bytes());
        out.extend((e.name.len() as u16).to_le_bytes());
        out.extend((e.extra.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes()); // comment length
        out.extend(0u16.to_le_bytes()); // disk number
        out.extend(0u16.to_le_bytes()); // internal attributes
        out.extend(0u32.to_le_bytes()); // external attributes
        out.extend(offsets[local].to_le_bytes());
        out.extend(&e.name);
        out.extend(&e.extra);
    }
    let cd_len = out.len() - cd_start;
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((records.len() as u16).to_le_bytes());
    out.extend((records.len() as u16).to_le_bytes());
    out.extend((cd_len as u32).to_le_bytes());
    out.extend((cd_start as u32).to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

/// Info-ZIP Unicode Path extra field (0x7075) carrying `utf8`, tied to the raw name `raw`.
fn unicode_path_extra(raw: &[u8], utf8: &str) -> Vec<u8> {
    let mut body = vec![1u8];
    body.extend(crc32(raw).to_le_bytes());
    body.extend(utf8.as_bytes());
    let mut extra = 0x7075u16.to_le_bytes().to_vec();
    extra.extend((body.len() as u16).to_le_bytes());
    extra.extend(body);
    extra
}

fn ctx_run(archive: &Path, dest: &Path, opts: &ExtractOptions) -> Result<ExtractReport> {
    let (cancel, progress) = (CancelToken::new(), Progress::default());
    extract(archive, dest, opts, None, &JobCtx::new(&cancel, &progress))
}

fn ctx_test(archive: &Path) -> TestReport {
    let (cancel, progress) = (CancelToken::new(), Progress::default());
    test_archive(
        archive,
        None,
        &ExtractLimits::default(),
        &JobCtx::new(&cancel, &progress),
    )
    .unwrap()
}

fn opts_with(limits: ExtractLimits) -> ExtractOptions {
    ExtractOptions {
        limits,
        ..Default::default()
    }
}

fn write_archive(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn fresh_dest(dir: &Path, name: &str) -> PathBuf {
    let dest = dir.join(name);
    fs::create_dir(&dest).unwrap();
    dest
}

fn is_empty_dir(path: &Path) -> bool {
    fs::read_dir(path).unwrap().next().is_none()
}

#[test]
fn overlapping_central_records_are_rejected_before_writing() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    // Two different names, both central records pointing at the one local header.
    let ents = [
        Ent::stored(b"one.txt", b"payload"),
        Ent::stored(b"two.txt", b"payload"),
    ];
    let z = write_archive(
        d.path(),
        "overlap.zip",
        &zip_records(&ents, &[(0, 0), (1, 0)]),
    );
    let err = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap_err();
    assert!(matches!(err, CoreError::UnsafePath(_)), "{err:?}");
    assert!(is_empty_dir(&dest));
}

#[test]
fn ratio_and_size_bombs_are_rejected_before_writing() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let z = d.path().join("bomb.zip");
    let mut zw = ZipWriter::new(File::create(&z).unwrap());
    zw.start_file(
        "zeros.bin",
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
    )
    .unwrap();
    zw.write_all(&vec![0u8; 20 * 1024 * 1024]).unwrap();
    zw.finish().unwrap();

    let strict_ratio = opts_with(ExtractLimits {
        max_ratio: 100,
        ..Default::default()
    });
    assert!(matches!(
        ctx_run(&z, &dest, &strict_ratio),
        Err(CoreError::UnsafePath(_))
    ));
    assert!(is_empty_dir(&dest));

    // Size limits, with the ratio check out of the way.
    let no_ratio = ExtractLimits {
        max_ratio: u64::MAX,
        ..Default::default()
    };
    let total = ExtractLimits {
        max_total_bytes: 1024,
        ..no_ratio.clone()
    };
    assert!(ctx_run(&z, &dest, &opts_with(total)).is_err());
    let per_file = ExtractLimits {
        max_file_bytes: 1000,
        ..no_ratio.clone()
    };
    assert!(ctx_run(&z, &dest, &opts_with(per_file)).is_err());
    assert!(is_empty_dir(&dest));

    let two = zip_bytes(&[Ent::stored(b"a", b"1"), Ent::stored(b"b", b"2")], &[0, 1]);
    let two_z = write_archive(d.path(), "two.zip", &two);
    let few = ExtractLimits {
        max_entries: 1,
        ..Default::default()
    };
    assert!(ctx_run(&two_z, &dest, &opts_with(few)).is_err());
    assert!(is_empty_dir(&dest));
}

#[test]
fn lying_sizes_never_write_past_the_declared_size() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let data = vec![7u8; 1000];
    // Declares 10 bytes everywhere, so the reader is bounded to 10 bytes of the stored data.
    let mut lie = Ent::stored(b"lie.bin", &data);
    lie.size = 10;
    lie.csize = 10;
    lie.crc = crc32(&data[..10]);
    let z = write_archive(d.path(), "lie.zip", &zip_bytes(&[lie], &[0]));
    let report = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    let out = dest.join("lie.bin");
    if out.exists() {
        assert!(fs::read(&out).unwrap().len() <= 10);
    }
    assert!(report.bytes <= 10, "{report:?}");
}

#[test]
fn entries_shorter_than_declared_fail_and_are_not_published() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let data = vec![7u8; 1000];
    // 1000 bytes of data whose checksum matches, declared as 1010 bytes.
    let mut short = Ent::stored(b"short.bin", &data);
    short.size = 1010;
    short.crc = crc32(&data);
    let ents = [short, Ent::stored(b"good.txt", b"fine")];
    let z = write_archive(d.path(), "short.zip", &zip_bytes(&ents, &[0, 1]));
    let report = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(report.failed[0].reason.contains("declared"), "{report:?}");
    assert!(!dest.join("short.bin").exists());
    assert_eq!(report.extracted, vec![dest.join("good.txt")]);
    assert!(
        fs::read_dir(&dest)
            .unwrap()
            .flatten()
            .all(|e| !crate::ops::is_partial_name(&e.file_name().to_string_lossy()))
    );
}

#[test]
fn local_and_central_name_mismatch_fails_only_that_entry() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let mut bad = Ent::stored(b"abc.txt", b"local name differs");
    bad.local_name = Some(b"abd.txt".to_vec());
    let ents = [bad, Ent::stored(b"good.txt", b"fine")];
    let z = write_archive(d.path(), "mismatch.zip", &zip_bytes(&ents, &[0, 1]));

    let report = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(report.failed[0].path.ends_with("abc.txt"), "{report:?}");
    assert_eq!(report.extracted, vec![dest.join("good.txt")]);
    assert!(!dest.join("abc.txt").exists());

    let test = ctx_test(&z);
    assert!(
        test.failures()
            .any(|f| f.name == "abc.txt" && f.error.is_some()),
        "{test:?}"
    );
}

#[test]
fn lzma_dictionary_of_two_gib_is_refused_by_default() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    // Method 14 (LZMA) properties: version 5.2, props size 5, lc/lp/pb 0x5D, dictionary 2 GiB.
    let mut model = Ent::stored(
        b"model.bin",
        &[5, 2, 5, 0, 0x5D, 0, 0, 0, 0x80, 0, 0, 0, 0, 0, 0, 0],
    );
    model.method = 14;
    let z = write_archive(d.path(), "lzma.zip", &zip_bytes(&[model], &[0]));

    match ctx_run(&z, &dest, &ExtractOptions::default()) {
        Err(CoreError::UnsafePath(msg)) => {
            assert!(msg.contains("decoder memory"), "{msg}");
        }
        other => panic!("expected a decoder memory refusal, got {other:?}"),
    }
    assert!(is_empty_dir(&dest));

    let raised = opts_with(ExtractLimits {
        max_decoder_memory: 3u64 << 30,
        ..Default::default()
    });
    // The preflight passes; the data may still fail, which is fine, but not on memory.
    match ctx_run(&z, &dest, &raised) {
        Ok(_) => {}
        Err(e) => assert!(!e.to_string().contains("decoder memory"), "{e}"),
    }
}

#[test]
fn entry_names_decode_by_flag_then_unicode_path_then_cp437() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let mut utf8_flagged = Ent::stored("Ünïcode.txt".as_bytes(), b"utf8");
    utf8_flagged.flags = 0x0800;
    let mut unicode_path = Ent::stored(b"ja.txt", b"unicode path");
    unicode_path.extra = unicode_path_extra(b"ja.txt", "日本.txt");
    let ents = [
        Ent::stored(b"Gr\x81\xE1e.txt", b"cp437"),
        utf8_flagged,
        unicode_path,
        Ent::stored(b"dir\\file.txt", b"nested"),
    ];
    let z = write_archive(d.path(), "names.zip", &zip_bytes(&ents, &[0, 1, 2, 3]));

    let names: Vec<String> = list(&z)
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(
        names,
        ["Grüße.txt", "Ünïcode.txt", "日本.txt", "dir/file.txt"]
    );

    let report = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(fs::read(dest.join("Grüße.txt")).unwrap(), b"cp437");
    assert_eq!(fs::read(dest.join("Ünïcode.txt")).unwrap(), b"utf8");
    assert_eq!(fs::read(dest.join("日本.txt")).unwrap(), b"unicode path");
    assert_eq!(
        fs::read(dest.join("dir").join("file.txt")).unwrap(),
        b"nested"
    );
}

#[test]
fn malicious_names_reject_the_whole_archive() {
    let d = tempfile::tempdir().unwrap();
    let names: [&[u8]; 15] = [
        b"../x",
        b"..\\x",
        b"/abs",
        b"C:/x",
        b"C:x",
        b"\\\\server\\share\\x",
        b"\\\\?\\C:\\x",
        b"a.txt:ads",
        b"CON",
        b"nul.txt",
        b"COM1",
        b"LPT9.log",
        b"trailing.",
        b"trailing ",
        b"a\0b.txt",
    ];
    for (i, name) in names.iter().enumerate() {
        let dest = fresh_dest(d.path(), &format!("dest{i}"));
        let ents = [
            Ent::stored(b"good.txt", b"fine"),
            Ent::stored(name, b"pwned"),
        ];
        let z = write_archive(d.path(), "evil.zip", &zip_bytes(&ents, &[0, 1]));
        let label = String::from_utf8_lossy(name).into_owned();

        let listing = list(&z).unwrap();
        assert!(
            listing.entries.iter().any(|e| !e.safe_name),
            "{label:?} must be listed as unsafe"
        );
        assert!(
            ctx_run(&z, &dest, &ExtractOptions::default()).is_err(),
            "{label:?}"
        );
        assert!(is_empty_dir(&dest), "nothing written for {label:?}");
        assert!(!d.path().join("x").exists(), "{label:?} escaped");
    }
}

#[test]
fn case_insensitive_collisions_keep_the_first_and_folders_win() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "files");
    let z = write_archive(
        d.path(),
        "case.zip",
        &zip_bytes(
            &[
                Ent::stored(b"Readme.txt", b"first"),
                Ent::stored(b"README.TXT", b"second"),
            ],
            &[0, 1],
        ),
    );
    let report = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    assert_eq!(report.extracted.len(), 1, "{report:?}");
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert!(report.skipped[0].reason.contains("case"), "{report:?}");

    let dest = fresh_dest(d.path(), "folders");
    let z = write_archive(
        d.path(),
        "folder.zip",
        &zip_bytes(
            &[
                Ent::stored(b"a", b"file"),
                Ent::stored(b"A/b.txt", b"nested"),
            ],
            &[0, 1],
        ),
    );
    let report = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    assert!(
        report
            .skipped
            .iter()
            .any(|s| s.path == Path::new("a") && s.reason.contains("folder")),
        "{report:?}"
    );
    assert_eq!(report.extracted, vec![dest.join("A").join("b.txt")]);
    assert_eq!(fs::read(dest.join("A").join("b.txt")).unwrap(), b"nested");
}

#[test]
fn symlink_entries_are_skipped_never_created() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let z = d.path().join("links.zip");
    let mut zw = ZipWriter::new(File::create(&z).unwrap());
    zw.add_symlink("link", "/etc/passwd", SimpleFileOptions::default())
        .unwrap();
    zw.start_file("real.txt", SimpleFileOptions::default())
        .unwrap();
    zw.write_all(b"x").unwrap();
    zw.finish().unwrap();

    let report = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    assert!(
        report.skipped.iter().any(|s| s.path.ends_with("link")),
        "{report:?}"
    );
    assert_eq!(report.extracted, vec![dest.join("real.txt")]);
    assert!(!paths::exists_no_follow(&dest.join("link")));
}

#[cfg(windows)]
#[test]
fn entries_do_not_pass_through_a_preexisting_junction() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let outside = d.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let junction = dest.join("sub");
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .output()
        .is_ok_and(|output| output.status.success());
    if !made {
        eprintln!("mklink unavailable; skipping junction assertion");
        return;
    }
    let z = write_archive(
        d.path(),
        "junction.zip",
        &zip_bytes(&[Ent::stored(b"sub/x.txt", b"escaped")], &[0]),
    );
    let report = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    assert!(!outside.join("x.txt").exists(), "{report:?}");
    assert!(report.extracted.is_empty(), "{report:?}");
    assert!(
        report.failed.iter().any(|f| f.reason.contains("link")),
        "{report:?}"
    );
    fs::remove_dir(&junction).unwrap();
}

#[cfg(windows)]
fn zone_stream(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(":Zone.Identifier");
    PathBuf::from(s)
}

#[cfg(windows)]
#[test]
fn mark_of_the_web_follows_the_archive_zone() {
    let d = tempfile::tempdir().unwrap();
    let z = write_archive(
        d.path(),
        "web.zip",
        &zip_bytes(&[Ent::stored(b"doc.txt", b"web")], &[0]),
    );
    let zone =
        |id: u32| format!("[ZoneTransfer]\r\nZoneId={id}\r\nHostUrl=https://example.com/a.zip\r\n");

    fs::write(zone_stream(&z), zone(3)).unwrap();
    let dest = fresh_dest(d.path(), "marked");
    ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    let tag = fs::read_to_string(zone_stream(&dest.join("doc.txt"))).unwrap();
    assert!(
        tag.contains("ZoneId=3") && tag.contains("ReferrerUrl="),
        "{tag}"
    );

    let dest = fresh_dest(d.path(), "unmarked");
    let opts = ExtractOptions {
        mark_of_the_web: false,
        ..Default::default()
    };
    ctx_run(&z, &dest, &opts).unwrap();
    assert!(fs::read(zone_stream(&dest.join("doc.txt"))).is_err());

    fs::write(zone_stream(&z), zone(2)).unwrap();
    let dest = fresh_dest(d.path(), "zone2");
    ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    assert!(fs::read(zone_stream(&dest.join("doc.txt"))).is_err());
}
