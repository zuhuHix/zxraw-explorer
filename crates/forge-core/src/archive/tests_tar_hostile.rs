//! Hostile tars, built header by header: names that escape, links, extension headers that
//! change a name or grow without bound, broken numbers, sparse maps, truncation, bombs.

use super::tests_sevenz::tree_files;
use super::*;
use forge_jobs::{CancelToken, Progress};
use std::io::Write;

/// A raw 512-byte header; `edit` may change any field before the checksum is set.
fn header(name: &[u8], flag: u8, size: u64, edit: impl FnOnce(&mut [u8; 512])) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name);
    h[100..108].copy_from_slice(b"0000644\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    h[136..148].copy_from_slice(b"14000000000\0");
    h[156] = flag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    edit(&mut h);
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

/// Builds a tar member by member.
#[derive(Default)]
struct Tar(Vec<u8>);

impl Tar {
    fn raw(mut self, h: [u8; 512], data: &[u8]) -> Self {
        self.0.extend_from_slice(&h);
        self.0.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        self.0.resize(self.0.len() + pad, 0);
        self
    }
    fn file(self, name: &str, data: &[u8]) -> Self {
        let h = header(name.as_bytes(), b'0', data.len() as u64, |_| {});
        self.raw(h, data)
    }
    fn link(self, name: &str, flag: u8, target: &str) -> Self {
        let h = header(name.as_bytes(), flag, 0, |h| {
            h[157..157 + target.len()].copy_from_slice(target.as_bytes())
        });
        self.raw(h, &[])
    }
    fn pax(self, records: &[u8]) -> Self {
        let h = header(b"PaxHeaders/x", b'x', records.len() as u64, |_| {});
        self.raw(h, records)
    }
    fn end(mut self) -> Vec<u8> {
        self.0.resize(self.0.len() + 1024, 0);
        self.0
    }
}

fn record(key: &str, value: &str) -> Vec<u8> {
    let body = key.len() + value.len() + 3;
    let mut len = body + 1;
    while len != body + len.to_string().len() {
        len = body + len.to_string().len();
    }
    format!("{len} {key}={value}\n").into_bytes()
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

fn gz(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

fn try_extract(archive: &Path, dest: &Path, limits: ExtractLimits) -> Result<ExtractReport> {
    let (c, p) = (CancelToken::new(), Progress::default());
    let opts = ExtractOptions {
        limits,
        ..Default::default()
    };
    extract(archive, dest, &opts, None, &JobCtx::new(&c, &p))
}

fn dest(d: &Path) -> PathBuf {
    let x = d.join("dest");
    fs::create_dir(&x).unwrap();
    x
}

#[test]
fn escaping_names_refuse_the_archive() {
    let d = tempfile::tempdir().unwrap();
    for (i, name) in [
        "../evil.txt",
        "a/../../evil.txt",
        "/etc/evil",
        "C:/evil",
        "a/b:c",
    ]
    .into_iter()
    .enumerate()
    {
        let tar = Tar::default()
            .file("ok.txt", b"ok")
            .file(name, b"evil")
            .end();
        let p = write(d.path(), &format!("n{i}.tar"), &tar);
        let x = d.path().join(format!("x{i}"));
        fs::create_dir(&x).unwrap();
        let err = try_extract(&p, &x, ExtractLimits::default()).unwrap_err();
        assert!(matches!(err, CoreError::UnsafePath(_)), "{name}: {err:?}");
        assert!(tree_files(&x).is_empty(), "{name}: nothing written");
    }
    assert!(!d.path().join("evil.txt").exists());
}

#[test]
fn pax_paths_are_validated_like_any_name() {
    let d = tempfile::tempdir().unwrap();
    // The ustar name is harmless; the PAX path that replaces it is not.
    let tar = Tar::default()
        .pax(&record("path", "../../escape.txt"))
        .file("safe.txt", b"x")
        .end();
    let p = write(d.path(), "pax.tar", &tar);
    let x = dest(d.path());
    let err = try_extract(&p, &x, ExtractLimits::default()).unwrap_err();
    assert!(matches!(err, CoreError::UnsafePath(_)), "{err:?}");
    let listing = list(&p).unwrap();
    assert_eq!(listing.entries[0].name, "../../escape.txt");
    assert!(!listing.entries[0].safe_name);
    // A good PAX path, size and time win over the header's.
    let tar = Tar::default()
        .pax(
            &[
                record("path", "dir/naïve name.txt"),
                record("mtime", "1700000000.5"),
            ]
            .concat(),
        )
        .file("x", b"pax data")
        .end();
    let p = write(d.path(), "good.tar", &tar);
    let l = list(&p).unwrap();
    assert_eq!(l.entries[0].name, "dir/naïve name.txt");
    assert_eq!(
        l.entries[0].modified,
        Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_500))
    );
}

#[test]
fn links_are_never_followed_or_created() {
    let d = tempfile::tempdir().unwrap();
    let outside = d.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let target = outside.to_string_lossy().replace('\\', "/");
    // A symlink to a folder outside, then a file "through" it; a hard link outside.
    let tar = Tar::default()
        .link("a", b'2', &target)
        .file("a/evil.txt", b"through the link?")
        .link("h", b'1', "../../outside/secret")
        .link("a2", b'2', "../outside")
        .file("a2/x.txt", b"x")
        .end();
    let p = write(d.path(), "links.tar", &tar);
    let x = dest(d.path());
    let rep = try_extract(&p, &x, ExtractLimits::default()).unwrap();
    assert!(rep.failed.is_empty(), "{rep:?}");
    // The files land in real folders inside the destination.
    assert_eq!(
        fs::read(x.join("a/evil.txt")).unwrap(),
        b"through the link?"
    );
    assert!(fs::symlink_metadata(x.join("a")).unwrap().is_dir());
    assert!(
        !fs::symlink_metadata(x.join("a"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(x.join("a2/x.txt")).unwrap(), b"x");
    assert!(!x.join("h").exists());
    assert!(
        fs::read_dir(&outside).unwrap().next().is_none(),
        "nothing outside"
    );
    // The links were reported: skipped, never made.
    assert!(
        rep.skipped
            .iter()
            .any(|s| s.reason.contains("symbolic link"))
    );
    assert!(rep.skipped.iter().any(|s| s.reason.contains("hard link")));
}

#[test]
fn duplicate_names_last_wins_even_through_a_link_name() {
    let d = tempfile::tempdir().unwrap();
    let tar = Tar::default()
        .file("dup.txt", b"first")
        .link("dir", b'2', "/tmp")
        .file("dup.txt", b"second")
        .file("dir", b"a file now")
        .end();
    let p = write(d.path(), "dup.tar", &tar);
    let l = list(&p).unwrap();
    assert_eq!(l.entries.len(), 2, "{:?}", l.entries);
    let x = dest(d.path());
    let rep = try_extract(&p, &x, ExtractLimits::default()).unwrap();
    assert!(rep.failed.is_empty() && rep.skipped.is_empty(), "{rep:?}");
    assert_eq!(fs::read(x.join("dup.txt")).unwrap(), b"second");
    assert_eq!(fs::read(x.join("dir")).unwrap(), b"a file now");
}

#[test]
fn extension_headers_are_bounded() {
    let d = tempfile::tempdir().unwrap();
    // A PAX header over 1 MiB.
    let big = vec![b'a'; 2 << 20];
    let mut rec = record("comment", "");
    rec.pop();
    let mut huge = Vec::new();
    huge.extend(format!("{} comment=", big.len() + 20).as_bytes());
    huge.extend(&big);
    let tar = Tar::default().pax(&huge).file("x", b"x").end();
    let p = write(d.path(), "bigpax.tar", &tar);
    let err = list(&p).unwrap_err();
    assert!(err.to_string().contains("too large"), "{err}");
    // A GNU long name over 64 KiB.
    let long = vec![b'n'; 70_000];
    let h = header(b"././@LongLink", b'L', long.len() as u64, |_| {});
    let tar = Tar::default().raw(h, &long).file("x", b"x").end();
    let p = write(d.path(), "longname.tar", &tar);
    assert!(list(&p).unwrap_err().to_string().contains("too large"));
    // Endless extension headers before a member.
    let mut t = Tar::default();
    for _ in 0..40 {
        t = t.pax(&record("comment", "again"));
    }
    let p = write(d.path(), "many.tar", &t.file("x", b"x").end());
    assert!(
        list(&p)
            .unwrap_err()
            .to_string()
            .contains("too many extension headers")
    );
    // Extension headers and then the end.
    let p = write(
        d.path(),
        "dangling.tar",
        &Tar::default().pax(&record("path", "x")).end(),
    );
    assert!(list(&p).is_err());
    // A record whose length lies.
    let p = write(
        d.path(),
        "badrec.tar",
        &Tar::default().pax(b"999 path=x\n").file("x", b"x").end(),
    );
    assert!(list(&p).unwrap_err().to_string().contains("PAX"));
}

#[test]
fn numbers_are_checked() {
    let d = tempfile::tempdir().unwrap();
    // Bad octal in the size.
    let h = header(b"bad.txt", b'0', 0, |h| {
        h[124..136].copy_from_slice(b"0000000012x\0")
    });
    let p = write(d.path(), "octal.tar", &Tar::default().raw(h, b"").end());
    let err = list(&p).unwrap_err();
    assert!(err.to_string().contains("size"), "{err}");
    // A checksum that doesn't match.
    let mut h = header(b"x.txt", b'0', 1, |_| {});
    h[0] = b'y';
    let p = write(d.path(), "sum.tar", &Tar::default().raw(h, b"1").end());
    assert!(list(&p).unwrap_err().to_string().contains("checksum"));
    // Base-256 sizes: a small one reads; a negative one and one past the data don't.
    let b256 = |v: &[u8]| {
        let v = v.to_vec();
        move |h: &mut [u8; 512]| {
            h[124..136].fill(0);
            h[124..136].copy_from_slice(&v);
        }
    };
    let mut small = [0u8; 12];
    small[0] = 0x80;
    small[11] = 5;
    let tar = Tar::default()
        .raw(header(b"b256.txt", b'0', 0, b256(&small)), b"12345")
        .end();
    let p = write(d.path(), "b256.tar", &tar);
    let x = dest(d.path());
    try_extract(&p, &x, ExtractLimits::default()).unwrap();
    assert_eq!(fs::read(x.join("b256.txt")).unwrap(), b"12345");
    let p = write(
        d.path(),
        "neg.tar",
        &Tar::default()
            .raw(header(b"n.txt", b'0', 0, b256(&[0xff; 12])), b"")
            .end(),
    );
    assert!(list(&p).is_err());
    let mut huge = [0u8; 12];
    huge[0] = 0x80;
    huge[4] = 0x7f;
    let p = write(
        d.path(),
        "huge.tar",
        &Tar::default()
            .raw(header(b"h.txt", b'0', 0, b256(&huge)), b"")
            .end(),
    );
    // A member past the total limit stops the walk before its data would be skipped (or,
    // through a compressor, decoded).
    let err = list(&p).unwrap_err();
    assert!(err.to_string().contains("expands to more than"), "{err}");
    let p = write(d.path(), "huge.tar.gz", &gz(&fs::read(&p).unwrap()));
    assert!(
        list(&p)
            .unwrap_err()
            .to_string()
            .contains("expands to more than")
    );
    // Within the limit but past the data: truncated.
    let mut big = [0u8; 12];
    big[0] = 0x80;
    big[8] = 0x01; // 4 GiB
    let p = write(
        d.path(),
        "big.tar",
        &Tar::default()
            .raw(header(b"b.txt", b'0', 0, b256(&big)), b"")
            .end(),
    );
    assert!(list(&p).unwrap_err().to_string().contains("truncated"));
}

/// An old GNU sparse member ('S'): `regions` (offset, length), data stored densely.
fn gnu_sparse(name: &str, real: u64, regions: &[(u64, u64)], extra_blocks: usize) -> Tar {
    let stored: u64 = regions.iter().map(|r| r.1).sum();
    let h = header(name.as_bytes(), b'S', stored, |h| {
        h[257..265].copy_from_slice(b"ustar  \0");
        for (i, (off, len)) in regions.iter().take(4).enumerate() {
            let at = 386 + i * 24;
            h[at..at + 12].copy_from_slice(format!("{off:011o}\0").as_bytes());
            h[at + 12..at + 24].copy_from_slice(format!("{len:011o}\0").as_bytes());
        }
        h[482] = u8::from(extra_blocks > 0);
        if real <= 0o777_7777_7777 {
            h[483..495].copy_from_slice(format!("{real:011o}\0").as_bytes());
        } else {
            // Base-256, as GNU tar writes big numbers.
            h[483..495].fill(0);
            h[487..495].copy_from_slice(&real.to_be_bytes());
            h[483] = 0x80;
        }
    });
    let mut t = Tar::default();
    t.0.extend_from_slice(&h);
    for b in 0..extra_blocks {
        let mut ext = [0u8; 512];
        // 21 regions of one byte each, far apart; more blocks follow until the last.
        for i in 0..21u64 {
            let at = (i * 24) as usize;
            let off = 1_000_000 + (b as u64 * 21 + i) * 10;
            ext[at..at + 12].copy_from_slice(format!("{off:011o}\0").as_bytes());
            ext[at + 12..at + 24].copy_from_slice(b"00000000001\0");
        }
        ext[504] = u8::from(b + 1 < extra_blocks);
        t.0.extend_from_slice(&ext);
    }
    let data: Vec<u8> = (0..stored).map(|i| b'a' + (i % 26) as u8).collect();
    t.0.extend_from_slice(&data);
    let pad = (512 - data.len() % 512) % 512;
    t.0.resize(t.0.len() + pad, 0);
    t
}

#[test]
fn sparse_files_read_with_their_holes() {
    let d = tempfile::tempdir().unwrap();
    // Old GNU sparse.
    let tar = gnu_sparse("sparse.bin", 10_000, &[(0, 3), (4096, 5), (9_998, 2)], 0).end();
    let p = write(d.path(), "gnu-sparse.tar", &tar);
    let l = list(&p).unwrap();
    assert_eq!(l.entries[0].size, 10_000);
    assert!(l.entries[0].method.contains("sparse"));
    let x = dest(d.path());
    try_extract(&p, &x, ExtractLimits::default()).unwrap();
    let got = fs::read(x.join("sparse.bin")).unwrap();
    let mut want = vec![0u8; 10_000];
    want[..3].copy_from_slice(b"abc");
    want[4096..4101].copy_from_slice(b"defgh");
    want[9_998..].copy_from_slice(b"ij");
    assert_eq!(got, want);

    // PAX sparse 1.0: the map leads the data; the real name in GNU.sparse.name.
    let map = b"2\n0\n4\n100\n2\n";
    let mut data = map.to_vec();
    data.resize(512, 0);
    data.extend_from_slice(b"WXYZuv");
    let pax = [
        record("GNU.sparse.major", "1"),
        record("GNU.sparse.minor", "0"),
        record("GNU.sparse.name", "real/name.bin"),
        record("GNU.sparse.realsize", "200"),
    ]
    .concat();
    let tar = Tar::default()
        .pax(&pax)
        .raw(
            header(b"GNUSparseFile.0/name.bin", b'0', data.len() as u64, |_| {}),
            &data,
        )
        .file("after.txt", b"after")
        .end();
    let p = write(d.path(), "pax-sparse.tar", &tar);
    let l = list(&p).unwrap();
    assert_eq!(names_of(&l), ["after.txt", "real/name.bin"]);
    let x = d.path().join("x10");
    fs::create_dir(&x).unwrap();
    try_extract(&p, &x, ExtractLimits::default()).unwrap();
    let mut want = vec![0u8; 200];
    want[..4].copy_from_slice(b"WXYZ");
    want[100..102].copy_from_slice(b"uv");
    assert_eq!(fs::read(x.join("real/name.bin")).unwrap(), want);
    assert_eq!(fs::read(x.join("after.txt")).unwrap(), b"after");

    // PAX sparse 0.1 (one map record), through xz.
    let pax = [
        record("GNU.sparse.map", "10,2,50,1"),
        record("GNU.sparse.size", "60"),
        record("path", "s01.bin"),
    ]
    .concat();
    let tar = Tar::default()
        .pax(&pax)
        .raw(header(b"x", b'0', 3, |_| {}), b"PQR")
        .end();
    let mut xz = Vec::new();
    let mut w = lzma_rust2::XzWriter::new(&mut xz, lzma_rust2::XzOptions::with_preset(1)).unwrap();
    w.write_all(&tar).unwrap();
    w.finish().unwrap();
    let p = write(d.path(), "s01.tar.xz", &xz);
    let x = d.path().join("x01");
    fs::create_dir(&x).unwrap();
    try_extract(&p, &x, ExtractLimits::default()).unwrap();
    let mut want = vec![0u8; 60];
    want[10..12].copy_from_slice(b"PQ");
    want[50] = b'R';
    assert_eq!(fs::read(x.join("s01.bin")).unwrap(), want);
}

fn names_of(l: &ArchiveListing) -> Vec<String> {
    let mut n: Vec<String> = l.entries.iter().map(|e| e.name.clone()).collect();
    n.sort();
    n
}

#[test]
fn sparse_maps_are_bounded() {
    let d = tempfile::tempdir().unwrap();
    // A map of more than 100,000 regions (extension blocks without end).
    let tar = gnu_sparse("bomb.bin", 1 << 40, &[(0, 1)], 5_000).end();
    let p = write(d.path(), "map-bomb.tar", &tar);
    let err = list(&p).unwrap_err();
    assert!(err.to_string().contains("too many regions"), "{err}");
    // Overlapping regions, and a region past the end.
    let p = write(
        d.path(),
        "overlap.tar",
        &gnu_sparse("o.bin", 100, &[(0, 10), (5, 10)], 0).end(),
    );
    assert!(list(&p).unwrap_err().to_string().contains("overlap"));
    let p = write(
        d.path(),
        "past.tar",
        &gnu_sparse("p.bin", 100, &[(95, 10)], 0).end(),
    );
    assert!(list(&p).is_err());
    // A sparse file of a petabyte from 1 byte: listed, but refused before anything is written.
    let p = write(
        d.path(),
        "pb.tar",
        &gnu_sparse("pb.bin", 1 << 50, &[(0, 1)], 0).end(),
    );
    assert_eq!(list(&p).unwrap().entries[0].size, 1 << 50);
    let x = dest(d.path());
    assert!(try_extract(&p, &x, ExtractLimits::default()).is_err());
    assert!(tree_files(&x).is_empty());
    // A PAX 1.0 map that claims too many regions.
    let pax = [
        record("GNU.sparse.major", "1"),
        record("GNU.sparse.minor", "0"),
        record("GNU.sparse.realsize", "10"),
    ]
    .concat();
    let mut data = b"999999999\n".to_vec();
    data.resize(512, 0);
    let tar = Tar::default()
        .pax(&pax)
        .raw(header(b"m", b'0', 512, |_| {}), &data)
        .end();
    let p = write(d.path(), "map10.tar", &tar);
    assert!(
        list(&p)
            .unwrap_err()
            .to_string()
            .contains("too many regions")
    );
}

#[test]
fn truncated_archives_are_reported() {
    let d = tempfile::tempdir().unwrap();
    let data = vec![b'q'; 5000];
    let tar = Tar::default()
        .file("a.txt", &data)
        .file("b.txt", b"b")
        .end();
    // Cut inside a's data: plain and gzip.
    let p = write(d.path(), "cut.tar", &tar[..2000]);
    assert!(list(&p).unwrap_err().to_string().contains("truncated"));
    let full = gz(&tar);
    let p = write(d.path(), "cut.tar.gz", &full[..full.len() / 2]);
    let err = list(&p).unwrap_err();
    assert!(
        err.to_string().contains("truncated") || err.to_string().contains("damaged"),
        "{err}"
    );
}

#[test]
fn devices_and_fifos_are_listed_not_extracted() {
    let d = tempfile::tempdir().unwrap();
    let tar = Tar::default()
        .link("dev/null", b'3', "")
        .link("dev/sda", b'4', "")
        .link("fifo", b'6', "")
        .file("ok.txt", b"ok")
        .end();
    let p = write(d.path(), "dev.tar", &tar);
    let x = dest(d.path());
    let rep = try_extract(&p, &x, ExtractLimits::default()).unwrap();
    assert_eq!(rep.skipped.len(), 3, "{rep:?}");
    assert_eq!(tree_files(&x), vec![("ok.txt".to_owned(), b"ok".to_vec())]);
}

#[test]
fn compression_bombs_hit_the_limits() {
    let d = tempfile::tempdir().unwrap();
    let zeros = vec![0u8; 4 << 20];
    let tar = Tar::default().file("zeros.bin", &zeros).end();
    let p = write(d.path(), "bomb.tar.gz", &gz(&tar));
    let x = dest(d.path());
    let limits = ExtractLimits {
        max_ratio: 50,
        ..Default::default()
    };
    let err = try_extract(&p, &x, limits).unwrap_err();
    assert!(err.to_string().contains("ratio"), "{err}");
    let limits = ExtractLimits {
        max_file_bytes: 1 << 20,
        ..Default::default()
    };
    assert!(try_extract(&p, &x, limits).is_err());
    assert!(tree_files(&x).is_empty());
}

#[test]
fn hostile_gzip_names_are_not_used() {
    let d = tempfile::tempdir().unwrap();
    for (i, (fname, want)) in [
        (&b"../../evil.txt"[..], "evil.txt"),
        (b"C:\\Windows\\win.ini", "win.ini"),
        (b"CON", "file9.bin"),
        (b"..", "file9.bin"),
        (b"a\x01b", "file9.bin"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut e = flate2::GzBuilder::new()
            .filename(fname)
            .write(Vec::new(), flate2::Compression::fast());
        e.write_all(b"payload").unwrap();
        let bytes = e.finish().unwrap();
        let sub = d.path().join(format!("d{i}"));
        fs::create_dir(&sub).unwrap();
        let p = write(&sub, "file9.bin.gz", &bytes);
        let l = list(&p).unwrap();
        assert_eq!(
            l.entries[0].name,
            want,
            "{:?}",
            String::from_utf8_lossy(fname)
        );
        let x = sub.join("x");
        fs::create_dir(&x).unwrap();
        try_extract(&p, &x, ExtractLimits::default()).unwrap();
        assert_eq!(fs::read(x.join(want)).unwrap(), b"payload");
    }
}

#[test]
fn a_checksum_counts_only_when_every_frame_has_one() {
    let d = tempfile::tempdir().unwrap();
    let tar = Tar::default().file("a.txt", &[b'a'; 3000]).end();
    // First frame with a checksum (one block of the tar), the rest without.
    let mut first = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    first.include_checksum(true).unwrap();
    first.write_all(&tar[..512]).unwrap();
    let mut bytes = first.finish().unwrap();
    let mut rest = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    rest.include_checksum(false).unwrap();
    rest.write_all(&tar[512..]).unwrap();
    bytes.extend(rest.finish().unwrap());
    let p = write(d.path(), "mixed.tar.zst", &bytes);
    let l = list(&p).unwrap();
    let a = &l.entries[0];
    assert!(!a.checked_by_block && a.unchecked, "{a:?}");
    // Both frames checked: checked.
    let mut all = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    all.include_checksum(true).unwrap();
    all.write_all(&tar[..512]).unwrap();
    let mut bytes = all.finish().unwrap();
    let mut all = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    all.include_checksum(true).unwrap();
    all.write_all(&tar[512..]).unwrap();
    bytes.extend(all.finish().unwrap());
    let p = write(d.path(), "both.tar.zst", &bytes);
    assert!(list(&p).unwrap().entries[0].checked_by_block);
    // xz: a second stream without a check.
    let xz = |data: &[u8], check: lzma_rust2::CheckType| {
        let mut o = lzma_rust2::XzOptions::with_preset(1);
        o.set_check_sum_type(check);
        let mut w = lzma_rust2::XzWriter::new(Vec::new(), o).unwrap();
        w.write_all(data).unwrap();
        w.finish().unwrap()
    };
    let mut bytes = xz(&tar[..512], lzma_rust2::CheckType::Crc64);
    bytes.extend(xz(&tar[512..], lzma_rust2::CheckType::None));
    let p = write(d.path(), "mixed.tar.xz", &bytes);
    assert!(!list(&p).unwrap().entries[0].checked_by_block);
    let mut bytes = xz(&tar[..512], lzma_rust2::CheckType::Crc64);
    bytes.extend([0u8; 8]); // stream padding
    bytes.extend(xz(&tar[512..], lzma_rust2::CheckType::Crc32));
    let p = write(d.path(), "both.tar.xz", &bytes);
    assert!(list(&p).unwrap().entries[0].checked_by_block);
}

#[test]
fn a_volume_label_with_data_survives_an_edit() {
    let d = tempfile::tempdir().unwrap();
    let label = vec![b'L'; 600];
    let tar = Tar::default()
        .raw(
            header(b"MY VOLUME", b'V', label.len() as u64, |_| {}),
            &label,
        )
        .file("keep.txt", b"keep")
        .file("drop.txt", b"drop")
        .end();
    let p = write(d.path(), "label.tar", &tar);
    assert_eq!(names_of(&list(&p).unwrap()), ["drop.txt", "keep.txt"]);
    let (c, pr) = (CancelToken::new(), Progress::default());
    let edit = ArchiveEdit {
        add: vec![],
        delete: vec!["drop.txt".into()],
    };
    update(&p, &edit, &CreateOptions::default(), &JobCtx::new(&c, &pr)).unwrap();
    assert_eq!(names_of(&list(&p).unwrap()), ["keep.txt"]);
    let x = dest(d.path());
    try_extract(&p, &x, ExtractLimits::default()).unwrap();
    assert_eq!(fs::read(x.join("keep.txt")).unwrap(), b"keep");
}

/// An xz stream of `data` whose (only) block header declares a 256 MiB dictionary: the
/// header's LZMA2 property is changed and its CRC made to match again.
fn xz_with_big_dictionary(data: &[u8]) -> Vec<u8> {
    let mut w =
        lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
    w.write_all(data).unwrap();
    let mut xz = w.finish().unwrap();
    let len = (usize::from(xz[12]) + 1) * 4;
    let header = 12..12 + len;
    let at = xz[header.clone()]
        .windows(2)
        .position(|w| w == [0x21, 0x01])
        .unwrap()
        + 12
        + 2;
    xz[at] = 32; // 2 << (32 / 2 + 11) = 256 MiB
    let crc = crc32fast::hash(&xz[12..12 + len - 4]);
    xz[12 + len - 4..12 + len].copy_from_slice(&crc.to_le_bytes());
    xz
}

fn small_memory() -> ExtractLimits {
    ExtractLimits {
        max_decoder_memory: 64 << 20,
        ..Default::default()
    }
}

/// Listing within `limits`, extracting and testing under them are all refused for memory,
/// before anything is written.
fn refused_for_memory(p: &Path, d: &Path) {
    let limits = small_memory();
    let backend = backend_for_file(p).unwrap();
    let err = backend.list_progress(p, &limits, None, None).unwrap_err();
    assert!(
        matches!(&err, CoreError::UnsafePath(m) if m.contains("memory")),
        "{}: {err:?}",
        p.display()
    );
    let x = d.join(format!("x-{}", paths::name_of(p)));
    fs::create_dir(&x).unwrap();
    let err = try_extract(p, &x, limits.clone()).unwrap_err();
    assert!(err.to_string().contains("memory"), "{err}");
    assert!(tree_files(&x).is_empty());
    let (c, pr) = (CancelToken::new(), Progress::default());
    let err = test_archive(p, None, &limits, &JobCtx::new(&c, &pr)).unwrap_err();
    assert!(err.to_string().contains("memory"), "{err}");
}

#[test]
fn decoder_memory_limits_are_checked_before_decoding() {
    let d = tempfile::tempdir().unwrap();
    let tar = Tar::default().file("a.txt", b"hello").end();
    // xz with a 256 MiB dictionary: a tar, and a single file.
    let p = write(d.path(), "dict.tar.xz", &xz_with_big_dictionary(&tar));
    refused_for_memory(&p, d.path());
    // Listed with the default limit (1 GiB), the cached listing is still checked against a
    // lower one.
    assert_eq!(list(&p).unwrap().entries.len(), 1);
    let x = d.path().join("after-cache");
    fs::create_dir(&x).unwrap();
    assert!(try_extract(&p, &x, small_memory()).is_err());
    assert!(tree_files(&x).is_empty());
    let p = write(
        d.path(),
        "dict.bin.xz",
        &xz_with_big_dictionary(b"single file"),
    );
    refused_for_memory(&p, d.path());
    // Zstandard frames with a 128 MiB window: a tar, and a single file.
    let zstd_big = |data: &[u8]| {
        let mut e = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        e.window_log(27).unwrap();
        e.write_all(data).unwrap();
        e.finish().unwrap()
    };
    let p = write(d.path(), "window.tar.zst", &zstd_big(&tar));
    refused_for_memory(&p, d.path());
    let p = write(d.path(), "window.bin.zst", &zstd_big(b"single file"));
    refused_for_memory(&p, d.path());
    // Within the limit, all is well; below Zstandard's smallest window, refused.
    let p = write(
        d.path(),
        "ok.tar.zst",
        &zstd::bulk::compress(&tar, 3).unwrap(),
    );
    let x = d.path().join("ok");
    fs::create_dir(&x).unwrap();
    try_extract(&p, &x, small_memory()).unwrap();
    assert_eq!(fs::read(x.join("a.txt")).unwrap(), b"hello");
    let tiny = ExtractLimits {
        max_decoder_memory: 512,
        ..Default::default()
    };
    let y = d.path().join("tiny");
    fs::create_dir(&y).unwrap();
    assert!(try_extract(&p, &y, tiny).is_err());
}

#[test]
fn many_long_names_hit_the_metadata_budget() {
    let d = tempfile::tempdir().unwrap();
    let mut t = Tar::default();
    for i in 0..40 {
        let long = format!("{i:03}/{}", "n".repeat(60_000));
        let h = header(b"././@LongLink", b'L', long.len() as u64 + 1, |_| {});
        let mut data = long.into_bytes();
        data.push(0);
        t = t.raw(h, &data).file("short", b"x");
    }
    let p = write(d.path(), "names.tar", &t.end());
    tar_backend::METADATA_CAP.with(|c| c.set(Some(1 << 20)));
    let err = list(&p).unwrap_err();
    tar_backend::METADATA_CAP.with(|c| c.set(None));
    assert!(err.to_string().contains("list of names"), "{err}");
    // Within the budget the same archive lists, each name kept once (by the entry).
    let l = list(&p).unwrap();
    assert_eq!(l.entries.len(), 40);
}

#[test]
fn the_scan_cache_stays_within_its_budget() {
    use std::sync::atomic::Ordering;
    let d = tempfile::tempdir().unwrap();
    let cap = 300_000u64;
    tar_backend::CACHE_CAP.store(cap, Ordering::Relaxed);
    for i in 0..6 {
        let mut t = Tar::default();
        for j in 0..300 {
            t = t.file(&format!("dir{i}/file-{j:04}-{}.txt", "x".repeat(40)), b"x");
        }
        let p = write(d.path(), &format!("c{i}.tar"), &t.end());
        list(&p).unwrap();
        assert!(tar_backend::cache_weight() <= cap, "after {i}");
    }
    // A scan heavier than the whole budget is used but not kept.
    let mut t = Tar::default();
    for j in 0..3000 {
        t = t.file(&format!("big/file-{j:05}-{}.txt", "y".repeat(80)), b"y");
    }
    let p = write(d.path(), "big.tar", &t.end());
    assert_eq!(list(&p).unwrap().entries.len(), 3000);
    assert!(tar_backend::cache_weight() <= cap);
    tar_backend::CACHE_CAP.store(0, Ordering::Relaxed);
}
