//! Self-extracting archives (see `sfx`): zip and 7z payloads behind a PE stub, fake
//! signatures in the stub, plain programs, truncated files, and the edit refusal.

use super::sfx::{self, SfxKind};
use super::*;
use forge_jobs::{CancelToken, Progress};

fn ctx_parts() -> (CancelToken, Progress) {
    (CancelToken::new(), Progress::default())
}

/// A minimal PE: "MZ" with `e_lfanew` 0x40, "PE\0\0", one section whose raw data is `body`
/// at 0x80. The image ends at 0x80 + body.len(); a payload goes right after it.
fn fake_pe(body: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 0x80];
    v[0..2].copy_from_slice(b"MZ");
    v[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    v[0x40..0x44].copy_from_slice(b"PE\0\0");
    v[0x46..0x48].copy_from_slice(&1u16.to_le_bytes()); // NumberOfSections
    v[0x54..0x56].copy_from_slice(&0u16.to_le_bytes()); // SizeOfOptionalHeader
    v[0x68..0x6C].copy_from_slice(&(body.len() as u32).to_le_bytes()); // SizeOfRawData
    v[0x6C..0x70].copy_from_slice(&0x80u32.to_le_bytes()); // PointerToRawData
    v.extend_from_slice(body);
    v
}

/// A stub of `n` bytes of program-like filler with nothing archive-like in it.
fn filler(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31) | 1).collect()
}

/// A real zip and a real 7z of a small tree, made with the crate's own writers.
fn payloads(dir: &Path) -> (Vec<u8>, Vec<u8>) {
    let src = dir.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a.txt"), b"alpha alpha alpha").unwrap();
    fs::write(src.join("sub").join("b.txt"), b"beta").unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let zip = dir.join("p.zip");
    create(
        std::slice::from_ref(&src),
        &zip,
        &CreateOptions::default(),
        &ctx,
    )
    .unwrap();
    let seven = dir.join("p.7z");
    create(
        std::slice::from_ref(&src),
        &seven,
        &CreateOptions::default(),
        &ctx,
    )
    .unwrap();
    (fs::read(zip).unwrap(), fs::read(seven).unwrap())
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn a_zip_behind_a_pe_stub_is_detected_and_listed() {
    let d = tempfile::tempdir().unwrap();
    let (zip, _) = payloads(d.path());
    let stub = fake_pe(&filler(3000));
    let image_end = stub.len() as u64;
    let mut bytes = stub;
    bytes.extend_from_slice(&zip);
    let exe = write(d.path(), "setup.exe", &bytes);

    assert_eq!(
        sfx::detect(&exe).unwrap(),
        Some(SfxKind::Zip { start: image_end })
    );
    assert_eq!(backend_for_file(&exe).unwrap().format(), ArchiveFormat::Zip);
    let names: Vec<String> = list(&exe)
        .unwrap()
        .entries
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert!(names.iter().any(|n| n.ends_with("a.txt")), "{names:?}");
    assert!(names.iter().any(|n| n.ends_with("b.txt")), "{names:?}");
}

#[test]
fn a_7z_behind_a_pe_stub_is_detected_but_not_opened_yet() {
    let d = tempfile::tempdir().unwrap();
    let (_, seven) = payloads(d.path());
    let stub = fake_pe(&filler(5000));
    let image_end = stub.len() as u64;
    let mut bytes = stub;
    bytes.extend_from_slice(&seven);
    let exe = write(d.path(), "setup.exe", &bytes);

    assert_eq!(
        sfx::detect(&exe).unwrap(),
        Some(SfxKind::SevenZip { start: image_end })
    );
    let Err(e) = sfx::sfx_backend(&exe) else {
        panic!("a 7z payload must not open yet")
    };
    assert!(matches!(e, CoreError::Unsupported(_)), "{e}");
    assert!(e.to_string().contains("7z self-extracting"), "{e}");
    assert!(backend_for_file(&exe).is_err());
    // Edits are refused too, before anything is written.
    let (c, p) = ctx_parts();
    let edit = ArchiveEdit {
        add: vec![],
        delete: vec!["a.txt".into()],
    };
    let err = update(&exe, &edit, &CreateOptions::default(), &JobCtx::new(&c, &p)).unwrap_err();
    assert!(
        err.to_string()
            .contains("Self-extracting archives can't be edited"),
        "{err}"
    );
    assert_eq!(fs::read(&exe).unwrap(), bytes, "untouched");
}

#[test]
fn a_fake_7z_signature_in_the_stub_loses_to_a_real_zip() {
    let d = tempfile::tempdir().unwrap();
    let (zip, _) = payloads(d.path());
    // A 7z signature with a start header that doesn't check out, and an end-of-directory
    // signature with nothing behind it, both in the stub.
    let mut body = filler(200);
    body.extend_from_slice(b"7z\xBC\xAF\x27\x1C\x00\x04");
    body.extend_from_slice(&[0xAA; 24]);
    body.extend_from_slice(b"PK\x05\x06");
    body.extend_from_slice(&filler(100));
    let mut bytes = fake_pe(&body);
    let stub_end = bytes.len() as u64;
    bytes.extend_from_slice(&zip);
    let exe = write(d.path(), "setup.exe", &bytes);

    assert_eq!(
        sfx::detect(&exe).unwrap(),
        Some(SfxKind::Zip { start: stub_end })
    );
    assert_eq!(backend_for_file(&exe).unwrap().format(), ArchiveFormat::Zip);
}

#[test]
fn a_7z_inside_the_image_is_not_searched_and_the_payload_after_it_wins() {
    let d = tempfile::tempdir().unwrap();
    let (zip, seven) = payloads(d.path());
    // A genuine 7z inside the stub's own section data, then the zip payload after the image.
    let mut body = filler(64);
    body.extend_from_slice(&seven);
    let mut bytes = fake_pe(&body);
    let image_end = bytes.len() as u64;
    bytes.extend_from_slice(&zip);
    let exe = write(d.path(), "setup.exe", &bytes);

    assert_eq!(
        sfx::detect(&exe).unwrap(),
        Some(SfxKind::Zip { start: image_end })
    );
}

#[test]
fn a_plain_program_is_not_an_archive() {
    let d = tempfile::tempdir().unwrap();
    let exe = write(d.path(), "tool.exe", &fake_pe(&filler(4096)));
    assert_eq!(sfx::detect(&exe).unwrap(), None);
    assert!(sfx::sfx_backend(&exe).unwrap().is_none());
    assert!(backend_for_file(&exe).is_err());
    assert_eq!(fs::read(&exe).unwrap().len(), 0x80 + 4096, "not changed");
}

#[test]
fn a_truncated_or_malformed_pe_does_not_panic() {
    let d = tempfile::tempdir().unwrap();
    let (zip, seven) = payloads(d.path());
    let mut bytes = fake_pe(&filler(300));
    bytes.extend_from_slice(&seven);
    // Every prefix of a 7z SFX, and of a zip SFX: never a panic, and an error only when
    // reading fails.
    for n in (0..bytes.len()).step_by(37) {
        let p = write(d.path(), "cut.exe", &bytes[..n]);
        let _ = sfx::detect(&p).unwrap();
    }
    let mut zbytes = fake_pe(&filler(300));
    zbytes.extend_from_slice(&zip);
    for n in (0..zbytes.len()).step_by(41) {
        let p = write(d.path(), "cut.exe", &zbytes[..n]);
        let _ = sfx::detect(&p).unwrap();
    }
    // A PE header that points past the file, and sections that reach past it.
    let mut wild = fake_pe(&filler(64));
    wild[0x3C..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        sfx::detect(&write(d.path(), "wild.exe", &wild)).unwrap(),
        None
    );
    let mut far = fake_pe(&filler(64));
    far[0x6C..0x70].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        sfx::detect(&write(d.path(), "far.exe", &far)).unwrap(),
        None
    );
    // "MZ" alone, and an empty file.
    assert_eq!(
        sfx::detect(&write(d.path(), "mz.exe", b"MZ")).unwrap(),
        None
    );
    assert_eq!(
        sfx::detect(&write(d.path(), "none.exe", b"")).unwrap(),
        None
    );
}

#[test]
fn an_exe_is_never_an_archive_by_name_until_the_user_opens_it() {
    let exe = Path::new(r"C:\x\setup.exe");
    assert!(!is_archive_name(exe));
    assert!(!is_archive_path(exe));
    assert_eq!(split_archive_path(exe), None);

    sfx::mark_open_as_archive(exe);
    assert!(!is_archive_name(exe), "double-click still runs it");
    assert!(is_archive_path(exe));
    assert!(is_archive_path(Path::new(r"c:/X/SETUP.exe")));
    assert!(!is_archive_path(Path::new(r"C:\x\other.exe")));
}

#[test]
fn a_marked_zip_sfx_splits_into_archive_and_inner_path() {
    let d = tempfile::tempdir().unwrap();
    let (zip, _) = payloads(d.path());
    let mut bytes = fake_pe(&filler(128));
    bytes.extend_from_slice(&zip);
    let exe = write(d.path(), "marked.exe", &bytes);
    sfx::mark_open_as_archive(&exe);
    let inner = exe.join("sub").join("b.txt");
    let (file, inside) = split_archive_path(&inner).expect("split");
    assert_eq!(file, exe);
    assert_eq!(inside, "sub/b.txt");
}
