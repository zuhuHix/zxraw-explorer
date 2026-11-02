//! Codec round trips, passwords, integrity tests, temp open and parallel creation.

use super::*;
use forge_jobs::{CancelToken, Progress};

fn ctx_parts() -> (CancelToken, Progress) {
    (CancelToken::new(), Progress::default())
}

/// Deterministic pseudo-random bytes (LCG), so the data does not compress.
fn lcg_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (x >> 33) as u8
        })
        .collect()
}

/// Repetitive text, which compresses well.
fn text_bytes(len: usize) -> Vec<u8> {
    let line = b"The quick brown fox jumps over the lazy dog, again and again.\n";
    line.iter().cycle().take(len).copied().collect()
}

/// Write `(relative path, bytes)` pairs under `root`.
fn write_files(root: &Path, files: &[(&str, Vec<u8>)]) {
    for (rel, data) in files {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
}

/// Check that `dest` holds exactly the bytes in `files` (paths relative to `dest`).
fn assert_files_in(dest: &Path, files: &[(&str, Vec<u8>)]) {
    for (rel, data) in files {
        let got = fs::read(dest.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert!(got == data.as_slice(), "{rel} differs after extraction");
    }
}

fn entry<'a>(listing: &'a ArchiveListing, name: &str) -> &'a ArchiveEntry {
    listing
        .entries
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no entry {name}"))
}

fn extract_all(archive: &Path, dest: &Path) -> ExtractReport {
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let r = extract(archive, dest, &ExtractOptions::default(), None, &ctx).unwrap();
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    r
}

fn assert_test_ok(archive: &Path) {
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let rep = test_archive(archive, None, &ExtractLimits::default(), &ctx).unwrap();
    assert!(rep.is_ok(), "{:?}", rep.entries);
}

fn matrix_files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("src/text.txt", text_bytes(200_000)),
        ("src/empty.dat", Vec::new()),
        ("src/random.bin", lcg_bytes(100_000, 7)),
    ]
}

// ---------------------------------------------------------------------------
// 1. Codec round-trip matrix
// ---------------------------------------------------------------------------

const MATRIX: &[(Method, Option<i64>, &str)] = &[
    (Method::Stored, None, "Stored"),
    (Method::Deflate, Some(6), "Deflate"),
    (Method::Deflate, Some(24), "Deflate"),
    (Method::Zstd, Some(3), "Zstandard"),
    (Method::Bzip2, Some(6), "BZip2"),
    (Method::Xz, None, "XZ"),
];

#[test]
fn every_write_method_round_trips() {
    let root = tempfile::tempdir().unwrap();
    let files = matrix_files();
    write_files(root.path(), &files);
    let src = root.path().join("src");
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    for &(method, level, name) in MATRIX {
        let out = root.path().join(format!("{name}-{level:?}.zip"));
        let opts = CreateOptions {
            method: Some((method, level)),
            auto_store: false,
            threads: 1,
            ..Default::default()
        };
        create(std::slice::from_ref(&src), &out, &opts, &ctx)
            .unwrap_or_else(|e| panic!("{name} {level:?}: {e}"));

        let listing = list(&out).unwrap();
        for (rel, data) in &files {
            let e = entry(&listing, rel);
            if !data.is_empty() {
                assert_eq!(e.method, name, "{rel} written with {method:?} {level:?}");
            }
        }

        let dest = tempfile::tempdir().unwrap();
        extract_all(&out, dest.path());
        assert_files_in(dest.path(), &files);
        assert_test_ok(&out);
    }
}

#[test]
fn parallel_creation_round_trips_many_small_files() {
    let root = tempfile::tempdir().unwrap();
    let files: Vec<(String, Vec<u8>)> = (0..50)
        .map(|i| {
            let data = if i % 2 == 0 {
                format!("file {i}\n").repeat(100 + i).into_bytes()
            } else {
                lcg_bytes(3_000, i as u64)
            };
            (format!("many/f{i:02}.dat"), data)
        })
        .collect();
    let borrowed: Vec<(&str, Vec<u8>)> =
        files.iter().map(|(n, d)| (n.as_str(), d.clone())).collect();
    write_files(root.path(), &borrowed);

    let out = root.path().join("many.zip");
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let opts = CreateOptions {
        method: Some((Method::Deflate, Some(6))),
        auto_store: false,
        threads: 4,
        ..Default::default()
    };
    let rep = create(&[root.path().join("many")], &out, &opts, &ctx).unwrap();
    assert_eq!(rep.files, 50);

    let snap = p.snapshot();
    let on_disk = fs::metadata(&out).unwrap().len();
    assert!(snap.bytes_out > 0);
    assert_eq!(rep.bytes_out, on_disk);
    assert_eq!(snap.bytes_out, rep.bytes_out);

    let dest = tempfile::tempdir().unwrap();
    extract_all(&out, dest.path());
    assert_files_in(dest.path(), &borrowed);
    assert_test_ok(&out);
}

// ---------------------------------------------------------------------------
// 2. Presets
// ---------------------------------------------------------------------------

#[test]
fn every_preset_round_trips_and_applies_auto_store() {
    let root = tempfile::tempdir().unwrap();
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("src/notes.txt", text_bytes(20_000)),
        ("src/photo.jpg", lcg_bytes(10_000, 3)),
    ];
    write_files(root.path(), &files);
    let src = root.path().join("src");
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);

    for preset in Preset::ALL {
        let expected_txt = if matches!(preset, Preset::Zstd | Preset::Ultra) {
            "Zstandard"
        } else {
            "Deflate"
        };

        // Default options: auto_store is on, so the jpg is stored.
        let out = root.path().join(format!("{preset:?}-auto.zip"));
        let opts = CreateOptions {
            preset,
            threads: 1,
            ..Default::default()
        };
        create(std::slice::from_ref(&src), &out, &opts, &ctx).unwrap();
        let listing = list(&out).unwrap();
        assert_eq!(
            entry(&listing, "src/photo.jpg").method,
            "Stored",
            "{preset:?}"
        );
        assert_eq!(
            entry(&listing, "src/notes.txt").method,
            expected_txt,
            "{preset:?}"
        );
        let dest = tempfile::tempdir().unwrap();
        extract_all(&out, dest.path());
        assert_files_in(dest.path(), &files);
        assert_test_ok(&out);

        // auto_store off: the jpg is compressed like everything else.
        let out = root.path().join(format!("{preset:?}-nostore.zip"));
        let opts = CreateOptions {
            preset,
            auto_store: false,
            threads: 1,
            ..Default::default()
        };
        create(std::slice::from_ref(&src), &out, &opts, &ctx).unwrap();
        let listing = list(&out).unwrap();
        assert_ne!(
            entry(&listing, "src/photo.jpg").method,
            "Stored",
            "{preset:?}"
        );
        let dest = tempfile::tempdir().unwrap();
        extract_all(&out, dest.path());
        assert_files_in(dest.path(), &files);
    }
}

// ---------------------------------------------------------------------------
// 3. Passwords
// ---------------------------------------------------------------------------

fn secret_files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("secret/letter.txt", text_bytes(5_000)),
        ("secret/key.bin", lcg_bytes(4_000, 11)),
    ]
}

fn assert_nothing_written(dest: &Path) {
    let n = fs::read_dir(dest).unwrap().count();
    assert_eq!(n, 0, "dest should be empty");
}

fn check_encrypted_archive(enc: Encryption, expected: EntryEncryption, wrong: &str) {
    let root = tempfile::tempdir().unwrap();
    let files = secret_files();
    write_files(root.path(), &files);
    let out = root.path().join("enc.zip");
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let opts = CreateOptions {
        encryption: enc,
        threads: 1,
        ..Default::default()
    };
    create(&[root.path().join("secret")], &out, &opts, &ctx).unwrap();

    let listing = list(&out).unwrap();
    assert!(listing.any_encrypted());
    for (rel, _) in &files {
        let e = entry(&listing, rel);
        assert!(e.encrypted, "{rel}");
        assert_eq!(e.encryption, Some(expected), "{rel}");
    }

    // No password.
    let dest = tempfile::tempdir().unwrap();
    let err = extract(&out, dest.path(), &ExtractOptions::default(), None, &ctx).unwrap_err();
    assert!(matches!(err, CoreError::PasswordRequired(_)), "{err:?}");
    assert_nothing_written(dest.path());
    let err = test_archive(&out, None, &ExtractLimits::default(), &ctx).unwrap_err();
    assert!(matches!(err, CoreError::PasswordRequired(_)), "{err:?}");

    // Wrong password.
    let dest = tempfile::tempdir().unwrap();
    let bad = ExtractOptions {
        password: Some(Password::new(wrong)),
        ..Default::default()
    };
    match extract(&out, dest.path(), &bad, None, &ctx) {
        Err(err) => assert!(matches!(err, CoreError::WrongPassword(_)), "{err:?}"),
        // ZipCrypto's one-byte check lets a wrong password through 1 time in 256 (the
        // header is random per archive); the data then fails its checksum, so still no
        // file may be written.
        Ok(r) => {
            assert_eq!(
                expected,
                EntryEncryption::ZipCrypto,
                "AES must detect it up front"
            );
            assert!(r.extracted.is_empty(), "{:?}", r.extracted);
            assert!(!r.failed.is_empty());
        }
    }
    assert!(
        fs::read_dir(dest.path())
            .unwrap()
            .flatten()
            .all(|e| e.file_type().unwrap().is_dir()),
        "no file may be written with a wrong password"
    );

    // Right password.
    let dest = tempfile::tempdir().unwrap();
    let good = ExtractOptions {
        password: Some(Password::new("s3cret")),
        ..Default::default()
    };
    let r = extract(&out, dest.path(), &good, None, &ctx).unwrap();
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_files_in(dest.path(), &files);
    let rep = test_archive(
        &out,
        Some(&Password::new("s3cret")),
        &ExtractLimits::default(),
        &ctx,
    )
    .unwrap();
    assert!(rep.is_ok(), "{:?}", rep.entries);
}

#[test]
fn aes256_archives_need_the_right_password() {
    check_encrypted_archive(
        Encryption::Aes256(Password::new("s3cret")),
        EntryEncryption::Aes,
        "s3cre7",
    );
}

#[test]
fn zipcrypto_archives_need_the_right_password() {
    check_encrypted_archive(
        Encryption::ZipCryptoLegacy(Password::new("s3cret")),
        EntryEncryption::ZipCrypto,
        "s3cre7",
    );
}

#[test]
fn password_debug_output_is_redacted() {
    let dbg = format!("{:?}", Password::new("s3cret"));
    assert!(!dbg.contains("s3cret"), "{dbg}");
}

// ---------------------------------------------------------------------------
// 4. Integrity
// ---------------------------------------------------------------------------

#[test]
fn corrupt_stored_entry_is_reported_and_leaves_nothing_behind() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a.txt");
    let b = root.path().join("b.txt");
    let a_data = b"ALPHA-UNIQUE-PAYLOAD-0001 with some padding to make it longer".to_vec();
    let b_data = b"BRAVO-UNIQUE-PAYLOAD-0002 with other padding".to_vec();
    fs::write(&a, &a_data).unwrap();
    fs::write(&b, &b_data).unwrap();
    let out = root.path().join("stored.zip");
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let opts = CreateOptions {
        method: Some((Method::Stored, None)),
        auto_store: false,
        threads: 1,
        ..Default::default()
    };
    create(&[a.clone(), b.clone()], &out, &opts, &ctx).unwrap();

    // Flip one byte inside a.txt's stored data.
    let mut bytes = fs::read(&out).unwrap();
    let pos = bytes
        .windows(a_data.len())
        .position(|w| w == a_data.as_slice())
        .expect("a.txt data in archive");
    assert_eq!(
        bytes
            .windows(a_data.len())
            .filter(|w| *w == a_data.as_slice())
            .count(),
        1
    );
    bytes[pos + 5] ^= 0x01;
    fs::write(&out, &bytes).unwrap();

    let rep = test_archive(&out, None, &ExtractLimits::default(), &ctx).unwrap();
    assert_eq!(rep.entries.len(), 2);
    let bad: Vec<&EntryTest> = rep.failures().collect();
    assert_eq!(bad.len(), 1, "{:?}", rep.entries);
    assert_eq!(bad[0].name, "a.txt");
    assert!(bad[0].error.is_some());
    let good = rep.entries.iter().find(|e| e.name == "b.txt").unwrap();
    assert!(good.error.is_none(), "{:?}", good.error);
    assert!(!rep.is_ok());

    let dest = tempfile::tempdir().unwrap();
    let r = extract(&out, dest.path(), &ExtractOptions::default(), None, &ctx).unwrap();
    assert_eq!(r.failed.len(), 1, "{:?}", r.failed);
    assert!(r.failed[0].path.to_string_lossy().contains("a.txt"));
    assert_eq!(r.extracted.len(), 1);
    assert!(
        !dest.path().join("a.txt").exists(),
        "corrupt file must not be left"
    );
    assert_eq!(fs::read(dest.path().join("b.txt")).unwrap(), b_data);
    for e in fs::read_dir(dest.path()).unwrap() {
        let name = e.unwrap().file_name();
        assert!(
            !name.to_string_lossy().starts_with(".forge-partial-"),
            "temp file left behind: {name:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Opening a single entry to temp
// ---------------------------------------------------------------------------

#[test]
fn extract_entry_to_temp_reuses_copies_and_checks_limits() {
    let root = tempfile::tempdir().unwrap();
    let content = b"temp open content, small enough to preview".to_vec();
    let big = lcg_bytes(64 * 1024, 5);
    write_files(
        root.path(),
        &[("sub/a.txt", content.clone()), ("sub/big.bin", big)],
    );
    let out = root.path().join("temp.zip");
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    create(
        &[root.path().join("sub")],
        &out,
        &CreateOptions::default(),
        &ctx,
    )
    .unwrap();

    let path = extract_entry_to_temp(&out, "sub/a.txt", None, 1 << 20, &ctx).unwrap();
    assert_eq!(path.file_name().unwrap(), "a.txt");
    assert_eq!(fs::read(&path).unwrap(), content);

    let again = extract_entry_to_temp(&out, "sub/a.txt", None, 1 << 20, &ctx).unwrap();
    assert_eq!(again, path, "a second open reuses the same copy");

    assert!(
        extract_entry_to_temp(&out, "sub/big.bin", None, 1024, &ctx).is_err(),
        "entry over max_bytes must fail"
    );
    assert!(
        extract_entry_to_temp(&out, "sub/missing.txt", None, 1 << 20, &ctx).is_err(),
        "missing entry must fail"
    );

    // Remove this archive's temp folder again.
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
