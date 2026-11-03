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

/// Replace every occurrence of `from` in `bytes` with `to` (same length); returns the count.
fn patch_all(bytes: &mut [u8], from: &[u8], to: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i + from.len() <= bytes.len() {
        if &bytes[i..i + from.len()] == from {
            bytes[i..i + from.len()].copy_from_slice(to);
            n += 1;
            i += from.len();
        } else {
            i += 1;
        }
    }
    n
}

#[test]
fn encrypted_entries_are_held_to_the_decoder_memory_limit() {
    use zip::unstable::write::FileOptionsExt;
    let d = tempfile::tempdir().unwrap();
    // LZMA properties declaring a 2 GiB dictionary, then filler.
    let mut plain = vec![5, 2, 5, 0, 0x5D, 0, 0, 0, 0x80];
    plain.extend([0u8; 64]);
    for legacy in [false, true] {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let o = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let o = if legacy {
            o.with_deprecated_encryption(b"pw").unwrap()
        } else {
            o.with_aes_encryption(zip::AesMode::Aes256, "pw")
        };
        zw.start_file("model.bin", o).unwrap();
        std::io::Write::write_all(&mut zw, &plain).unwrap();
        let mut bytes = zw.finish().unwrap().into_inner();
        // Declare the entry LZMA (method 14).
        let patched = if legacy {
            // Method field of the local (offset 8) and central (offset 10) headers.
            let local = 8;
            let central = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap() + 10;
            for at in [local, central] {
                assert_eq!(&bytes[at..at + 2], &[0, 0]);
                bytes[at] = 14;
            }
            2
        } else {
            // The AES extra field: id 0x9901, size 7, AE-1 or AE-2, "AE", AES-256, method 0.
            (1..=2u8)
                .map(|v| {
                    patch_all(
                        &mut bytes,
                        &[0x01, 0x99, 7, 0, v, 0, b'A', b'E', 3, 0, 0],
                        &[0x01, 0x99, 7, 0, v, 0, b'A', b'E', 3, 14, 0],
                    )
                })
                .sum()
        };
        assert_eq!(patched, 2, "legacy {legacy}");
        let z = write_archive(d.path(), &format!("enc-lzma-{legacy}.zip"), &bytes);
        let dest = fresh_dest(d.path(), &format!("dest-{legacy}"));
        let opts = ExtractOptions {
            password: Some(Password::new("pw")),
            ..Default::default()
        };
        match ctx_run(&z, &dest, &opts) {
            Err(CoreError::UnsafePath(msg)) => assert!(msg.contains("decoder memory"), "{msg}"),
            other => panic!("legacy {legacy}: expected a decoder memory refusal, got {other:?}"),
        }
        assert!(is_empty_dir(&dest));
        let test = test_archive(&z, opts.password.as_ref(), &ExtractLimits::default(), &{
            static C: std::sync::OnceLock<(forge_jobs::CancelToken, forge_jobs::Progress)> =
                std::sync::OnceLock::new();
            let (c, p) = C.get_or_init(|| (forge_jobs::CancelToken::new(), Default::default()));
            JobCtx::new(c, p)
        });
        assert!(matches!(test, Err(CoreError::UnsafePath(_))), "{test:?}");
    }
}

#[cfg(windows)]
#[test]
fn a_destination_through_a_junction_is_resolved_and_pinned_for_the_job() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let d = tempfile::tempdir().unwrap();
    let real = d.path().join("real");
    fs::create_dir_all(real.join("dest")).unwrap();
    let link = d.path().join("link");
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&real)
        .output()
        .is_ok_and(|o| o.status.success());
    if !made {
        eprintln!("mklink unavailable; skipping");
        return;
    }
    let mut zw = ZipWriter::new(File::create(d.path().join("a.zip")).unwrap());
    zw.start_file("sub/x.txt", SimpleFileOptions::default())
        .unwrap();
    zw.write_all(b"inside").unwrap();
    zw.finish().unwrap();

    // While the job creates `sub`, the real destination's folders can't be moved.
    let moved = Arc::new(AtomicBool::new(true));
    let probe = Arc::clone(&moved);
    let (real_hook, d_hook) = (real.clone(), d.path().to_path_buf());
    super::safety::BEFORE_PARENT_CREATE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |_| {
            let renamed = fs::rename(&real_hook, d_hook.join("elsewhere")).is_ok();
            probe.store(renamed, Ordering::SeqCst);
        }))
    });
    let dest = link.join("dest");
    let report = ctx_run(&d.path().join("a.zip"), &dest, &ExtractOptions::default());
    super::safety::BEFORE_PARENT_CREATE.with(|hook| *hook.borrow_mut() = None);
    let report = report.unwrap();
    assert!(
        !moved.load(Ordering::SeqCst),
        "an ancestor was renamed during the job"
    );
    assert!(report.failed.is_empty(), "{report:?}");
    // Written to the real folder, reported beneath the path the user chose.
    assert_eq!(report.extracted, vec![dest.join("sub").join("x.txt")]);
    assert_eq!(fs::read(real.join("dest/sub/x.txt")).unwrap(), b"inside");
    // Released after the job.
    fs::rename(&real, d.path().join("elsewhere")).unwrap();
}

fn zip_of(path: &Path, files: &[(&str, &[u8])]) {
    let mut zw = ZipWriter::new(File::create(path).unwrap());
    for (name, data) in files {
        zw.start_file(*name, SimpleFileOptions::default()).unwrap();
        zw.write_all(data).unwrap();
    }
    zw.finish().unwrap();
}

#[cfg(windows)]
fn junction(link: &Path, target: &Path) -> bool {
    std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(windows)]
type HookKey =
    std::thread::LocalKey<std::cell::RefCell<Option<super::safety::BeforeParentCreateHook>>>;

/// Sets a safety hook for the life of the guard.
#[cfg(windows)]
struct Hook(&'static HookKey);

#[cfg(windows)]
impl Hook {
    fn set(key: &'static HookKey, f: impl Fn(&Path) + 'static) -> Self {
        key.with(|h| *h.borrow_mut() = Some(Box::new(f)));
        Self(key)
    }
}

#[cfg(windows)]
impl Drop for Hook {
    fn drop(&mut self) {
        self.0.with(|h| *h.borrow_mut() = None);
    }
}

#[cfg(windows)]
#[test]
fn the_destinations_grandparent_cannot_be_renamed_mid_job() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let d = tempfile::tempdir().unwrap();
    let top = d.path().join("top");
    let dest = top.join("mid").join("dest");
    fs::create_dir_all(&dest).unwrap();
    zip_of(&d.path().join("a.zip"), &[("sub/x.txt", b"inside")]);
    // While the job creates `sub`: neither the parent nor the grandparent can be renamed.
    let renamed = Arc::new(AtomicUsize::new(0));
    let (probe, top_h, d_h) = (Arc::clone(&renamed), top.clone(), d.path().to_path_buf());
    let hook = Hook::set(&super::safety::BEFORE_PARENT_CREATE, move |_| {
        let moves = [
            (top_h.join("mid"), top_h.join("mid2")),
            (top_h.clone(), d_h.join("top2")),
        ];
        for (from, to) in moves {
            if fs::rename(&from, &to).is_ok() {
                probe.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    let report = ctx_run(&d.path().join("a.zip"), &dest, &ExtractOptions::default());
    drop(hook);
    let report = report.unwrap();
    assert_eq!(
        renamed.load(Ordering::SeqCst),
        0,
        "an ancestor was renamed mid-job"
    );
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(fs::read(dest.join("sub/x.txt")).unwrap(), b"inside");
    // Released after the job.
    fs::rename(&top, d.path().join("top2")).unwrap();
}

#[cfg(windows)]
#[test]
fn the_destination_is_pinned_before_the_archive_is_planned() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let victim = fresh_dest(d.path(), "victim");
    zip_of(&d.path().join("a.zip"), &[("x.txt", b"inside")]);
    // Before listing and planning (which can take minutes for a big solid block), someone
    // tries to move the destination away and put a junction to another folder in its place.
    let swapped = Arc::new(AtomicBool::new(false));
    let (probe, dest_h, d_h, victim_h) = (
        Arc::clone(&swapped),
        dest.clone(),
        d.path().to_path_buf(),
        victim.clone(),
    );
    let hook = Hook::set(&super::safety::AFTER_DEST_PINNED, move |_| {
        if fs::rename(&dest_h, d_h.join("moved")).is_ok() {
            probe.store(true, Ordering::SeqCst);
            junction(&dest_h, &victim_h);
        }
    });
    let report = ctx_run(&d.path().join("a.zip"), &dest, &ExtractOptions::default());
    drop(hook);
    let report = report.unwrap();
    assert!(
        !swapped.load(Ordering::SeqCst),
        "the destination was moved before writing"
    );
    assert!(is_empty_dir(&victim), "{report:?}");
    assert_eq!(fs::read(dest.join("x.txt")).unwrap(), b"inside");
}

#[cfg(windows)]
#[test]
fn a_folder_the_extraction_makes_is_pinned_from_its_creation() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let d = tempfile::tempdir().unwrap();
    let parent = fresh_dest(d.path(), "parent");
    let victim = fresh_dest(d.path(), "victim");
    let archive = d.path().join("photos.zip");
    zip_of(&archive, &[("x.txt", b"inside")]);
    let made = parent.join("photos");
    // Before anything is written, someone swaps the new folder (or its parent) for a
    // junction to another folder.
    let swapped = Arc::new(AtomicBool::new(false));
    let (probe, made_h, parent_h, d_h, victim_h) = (
        Arc::clone(&swapped),
        made.clone(),
        parent.clone(),
        d.path().to_path_buf(),
        victim.clone(),
    );
    let hook = Hook::set(&super::safety::AFTER_DEST_PINNED, move |_| {
        for (from, to) in [
            (&made_h, d_h.join("moved")),
            (&parent_h, d_h.join("moved2")),
        ] {
            if fs::rename(from, &to).is_ok() {
                probe.store(true, Ordering::SeqCst);
                junction(from, &victim_h);
            }
        }
    });
    let (cancel, progress) = (CancelToken::new(), Progress::default());
    let ctx = JobCtx::new(&cancel, &progress);
    let report = extract_to(
        &archive,
        &parent,
        ExtractTarget::NamedFolder,
        &ExtractOptions::default(),
        &ctx,
    );
    drop(hook);
    let report = report.unwrap();
    assert!(
        !swapped.load(Ordering::SeqCst),
        "the new folder was swapped before writing"
    );
    assert!(report.created_dest);
    assert!(is_empty_dir(&victim), "{report:?}");
    assert_eq!(report.extracted, vec![made.join("x.txt")]);
    assert_eq!(fs::read(made.join("x.txt")).unwrap(), b"inside");
}

#[cfg(windows)]
#[test]
fn an_existing_junction_named_like_the_folder_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let parent = fresh_dest(d.path(), "parent");
    let victim = fresh_dest(d.path(), "victim");
    if !junction(&parent.join("photos"), &victim) {
        eprintln!("mklink unavailable; skipping");
        return;
    }
    let archive = d.path().join("photos.zip");
    zip_of(&archive, &[("x.txt", b"inside")]);
    let (cancel, progress) = (CancelToken::new(), Progress::default());
    let ctx = JobCtx::new(&cancel, &progress);
    let folder = NewFolder {
        parent: &parent,
        name: "photos",
        reuse: true,
    };
    let opts = ExtractOptions::default();
    let result = extract_into_folder(&archive, folder, &opts, None, &ctx, |_| true);
    assert!(
        matches!(result, Err(CoreError::UnsafePath(_))),
        "{result:?}"
    );
    assert!(is_empty_dir(&victim));
    fs::remove_dir(parent.join("photos")).unwrap();
}

#[cfg(windows)]
#[test]
fn taking_back_a_cancelled_extraction_stays_in_its_folder() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let d = tempfile::tempdir().unwrap();
    let parent = fresh_dest(d.path(), "parent");
    let victim = fresh_dest(d.path(), "victim");
    fs::write(victim.join("a.txt"), b"keep me").unwrap();
    let archive = d.path().join("photos.zip");
    zip_of(&archive, &[("a.txt", b"first"), ("sub/b.txt", b"second")]);
    let made = parent.join("photos");
    let (cancel, progress) = (CancelToken::new(), Progress::default());
    // Cancelled once `a.txt` is in place, as `sub` is made.
    let token = cancel.clone();
    let cancel_hook = Hook::set(&super::safety::BEFORE_PARENT_CREATE, move |_| {
        token.cancel()
    });
    // Before what it wrote is taken back, someone swaps the folder for a junction to a
    // folder holding a file of the same name.
    let swapped = Arc::new(AtomicBool::new(false));
    let (probe, made_h, d_h, victim_h) = (
        Arc::clone(&swapped),
        made.clone(),
        d.path().to_path_buf(),
        victim.clone(),
    );
    let cleanup_hook = Hook::set(&super::safety::BEFORE_CREATED_CLEANUP, move |_| {
        if fs::rename(&made_h, d_h.join("moved")).is_ok() {
            probe.store(true, Ordering::SeqCst);
            junction(&made_h, &victim_h);
        }
    });
    let ctx = JobCtx::new(&cancel, &progress);
    let report = extract_to(
        &archive,
        &parent,
        ExtractTarget::NamedFolder,
        &ExtractOptions::default(),
        &ctx,
    );
    drop((cancel_hook, cleanup_hook));
    let report = report.unwrap();
    assert!(report.cancelled, "{report:?}");
    assert_eq!(report.extracted, vec![made.join("a.txt")], "{report:?}");
    assert!(
        !swapped.load(Ordering::SeqCst),
        "the folder was swapped before the clean-up"
    );
    assert_eq!(fs::read(victim.join("a.txt")).unwrap(), b"keep me");
    // Everything it wrote, and the folder it made, are gone.
    assert!(!made.exists());
}

#[cfg(windows)]
#[test]
fn taking_back_a_cancelled_extraction_leaves_a_replaced_file_alone() {
    let d = tempfile::tempdir().unwrap();
    let parent = fresh_dest(d.path(), "parent");
    let archive = d.path().join("photos.zip");
    zip_of(
        &archive,
        &[
            ("a.txt", b"first"),
            ("b.txt", b"second"),
            ("sub/c.txt", b"third"),
        ],
    );
    let made = parent.join("photos");
    let (cancel, progress) = (CancelToken::new(), Progress::default());
    // Cancelled once `a.txt` and `b.txt` are in place, as `sub` is made.
    let token = cancel.clone();
    let cancel_hook = Hook::set(&super::safety::BEFORE_PARENT_CREATE, move |_| {
        token.cancel()
    });
    // Before what it wrote is taken back, another program replaces `a.txt` with a file of
    // its own under the same name (a different file, not the one extracted).
    let made_h = made.clone();
    let cleanup_hook = Hook::set(&super::safety::BEFORE_CREATED_CLEANUP, move |_| {
        fs::remove_file(made_h.join("a.txt")).unwrap();
        fs::write(made_h.join("a.txt"), b"not from the archive").unwrap();
    });
    let ctx = JobCtx::new(&cancel, &progress);
    let report = extract_to(
        &archive,
        &parent,
        ExtractTarget::NamedFolder,
        &ExtractOptions::default(),
        &ctx,
    );
    drop((cancel_hook, cleanup_hook));
    let report = report.unwrap();
    assert!(report.cancelled, "{report:?}");
    // The replacement survives byte for byte; what the job wrote is gone.
    assert_eq!(
        fs::read(made.join("a.txt")).unwrap(),
        b"not from the archive"
    );
    assert!(!made.join("b.txt").exists());
    assert!(!made.join("sub").exists());
    let names: Vec<_> = fs::read_dir(&made)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("a.txt")]);
    // And it is reported.
    let issue = report
        .failed
        .iter()
        .find(|i| i.path == made.join("a.txt"))
        .unwrap_or_else(|| panic!("not reported: {report:?}"));
    assert!(
        issue.reason.contains("changed by another program"),
        "{issue:?}"
    );
}

#[cfg(windows)]
#[test]
fn a_destination_folder_swapped_while_it_is_pinned_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let dest = d.path().join("a").join("dest");
    fs::create_dir_all(&dest).unwrap();
    zip_of(&d.path().join("x.zip"), &[("x.txt", b"inside")]);
    // Between resolving the destination and pinning its folders, another program moves a
    // folder of its path away and puts a different real folder (with a `dest`) at its name.
    let (a, d_h) = (d.path().join("a"), d.path().to_path_buf());
    let hook = Hook::set(&super::safety::BEFORE_BASE_PIN, move |_| {
        fs::rename(&a, d_h.join("a_old")).unwrap();
        fs::create_dir_all(a.join("dest")).unwrap();
    });
    let result = ctx_run(&d.path().join("x.zip"), &dest, &ExtractOptions::default());
    drop(hook);
    assert!(
        matches!(result, Err(CoreError::UnsafePath(_))),
        "{result:?}"
    );
    // Nothing was written to the substitute, nor to the original.
    assert!(is_empty_dir(&dest), "{result:?}");
    assert!(is_empty_dir(&d.path().join("a_old").join("dest")));
    // Unhindered, the same destination works.
    ctx_run(&d.path().join("x.zip"), &dest, &ExtractOptions::default()).unwrap();
    assert_eq!(fs::read(dest.join("x.txt")).unwrap(), b"inside");
}

/// Set the method of the entry `name` to `method` in its local and central headers.
fn set_method(bytes: &mut [u8], name: &[u8], method: u16) {
    let mut set = 0;
    for (sig, name_at, method_at) in [(b"PK\x03\x04", 30, 8), (b"PK\x01\x02", 46, 10)] {
        let mut i = 0;
        while let Some(p) = bytes[i..].windows(4).position(|w| w == sig) {
            let at = i + p;
            if bytes.get(at + name_at..at + name_at + name.len()) == Some(name) {
                bytes[at + method_at..at + method_at + 2].copy_from_slice(&method.to_le_bytes());
                set += 1;
            }
            i = at + 4;
        }
    }
    assert_eq!(set, 2);
}

#[test]
fn zipcrypto_entries_with_another_password_are_not_parsed_with_this_one() {
    use zip::unstable::write::FileOptionsExt;
    let d = tempfile::tempdir().unwrap();
    let mut model = vec![5, 2, 5, 0, 0x5D, 0, 0, 0, 0x80];
    model.extend([0u8; 64]);
    // The second entry has another password. With the first one its header check byte
    // fails 255 times in 256; build until it does, so the outcome is certain.
    for attempt in 0..64 {
        let mut zw = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let one = SimpleFileOptions::default()
            .with_deprecated_encryption(b"one")
            .unwrap();
        zw.start_file("a.txt", one).unwrap();
        zw.write_all(b"first entry").unwrap();
        let two = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .with_deprecated_encryption(b"two")
            .unwrap();
        zw.start_file("model.bin", two).unwrap();
        zw.write_all(&model).unwrap();
        let mut bytes = zw.finish().unwrap().into_inner();
        // Declare the second entry LZMA with a 2 GiB dictionary.
        set_method(&mut bytes, b"model.bin", 14);
        let (raw, check) = {
            let mut za = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
            let mut f = za.by_index_raw(1).unwrap();
            let check = (f.last_modified().unwrap().timepart() >> 8) as u8;
            let mut raw = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut raw).unwrap();
            (raw, check)
        };
        if super::crypto_peek::zipcrypto_head(&raw, b"one", check, 64).is_some() {
            continue; // the 1 in 256: try another header
        }
        let z = write_archive(d.path(), &format!("mixed-{attempt}.zip"), &bytes);
        let dest = fresh_dest(d.path(), &format!("dest-{attempt}"));
        let opts = ExtractOptions {
            password: Some(Password::new("one")),
            ..Default::default()
        };
        // Not refused as a decoder-memory bomb: the second entry fails as a wrong password.
        let report = ctx_run(&z, &dest, &opts).unwrap();
        assert_eq!(report.extracted, vec![dest.join("a.txt")], "{report:?}");
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(report.failed[0].reason.contains("password"), "{report:?}");
        assert!(!dest.join("model.bin").exists());
        // With its own password the bomb header is read, and refused.
        let own = ExtractOptions {
            password: Some(Password::new("two")),
            ..Default::default()
        };
        let other = fresh_dest(d.path(), &format!("other-{attempt}"));
        match ctx_run(&z, &other, &own) {
            Err(CoreError::UnsafePath(msg)) => assert!(msg.contains("decoder memory"), "{msg}"),
            other => panic!("expected the decoder memory limit, got {other:?}"),
        }
        return;
    }
    panic!("no header failed the check in 64 tries");
}
