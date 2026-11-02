//! Extraction and creation tests carried over from the single-file module, plus new ones.

use super::safety::BEFORE_PARENT_CREATE;
use super::*;
use std::fs::File;
use std::io::Write;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;
use forge_jobs::{CancelToken, Progress};
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};

fn ctx_parts() -> (CancelToken, Progress) {
    (CancelToken::new(), Progress::default())
}

fn raw_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zw = ZipWriter::new(File::create(path).unwrap());
    for (name, data) in entries {
        zw.start_file(*name, SimpleFileOptions::default()).unwrap();
        zw.write_all(data).unwrap();
    }
    zw.finish().unwrap();
}

#[test]
fn create_list_extract_roundtrip() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("Проект 日本");
    fs::create_dir_all(src.join("sub/empty")).unwrap();
    fs::write(src.join("a.txt"), b"hello hello hello").unwrap();
    fs::write(src.join("sub/b.jpg"), vec![9u8; 5000]).unwrap();
    let single = d.path().join("single.md");
    fs::write(&single, b"# hi").unwrap();
    let out = d.path().join("out.zip");
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let rep = create_zip(&[src.clone(), single], &out, &ctx).unwrap();
    assert_eq!(rep.files, 3);
    assert_eq!(rep.dirs, 3);
    assert!(
        create_zip(std::slice::from_ref(&src), &out, &ctx).is_err(),
        "no overwrite"
    );

    let list = list_zip(&out).unwrap();
    assert!(list.iter().all(|e| e.safe_name));
    assert!(
        list.iter()
            .any(|e| e.name == "Проект 日本/sub/b.jpg" && e.size == 5000)
    );

    let x = d.path().join("x");
    fs::create_dir(&x).unwrap();
    let r = extract_zip(&out, &x, &ExtractOptions::default(), &ctx).unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(r.extracted.len(), 3);
    assert_eq!(
        fs::read(x.join("Проект 日本/a.txt")).unwrap(),
        b"hello hello hello"
    );
    assert!(x.join("Проект 日本/sub/empty").is_dir());
    // Second extraction: existing files skipped by default, KeepBoth renames.
    let r = extract_zip(&out, &x, &ExtractOptions::default(), &ctx).unwrap();
    assert_eq!(r.skipped.len(), 3);
    let kb = ExtractOptions {
        policy: ConflictPolicy::KeepBoth,
        ..Default::default()
    };
    extract_zip(&out, &x, &kb, &ctx).unwrap();
    assert!(x.join("single (2).md").exists());
}

#[test]
fn listing_stops_at_the_entry_cap() {
    let d = tempfile::tempdir().unwrap();
    let z = d.path().join("many.zip");
    raw_zip(&z, &[("a", b"a"), ("b", b"b"), ("c", b"c")]);
    let error = list_zip_with_limit(&z, 2).unwrap_err().to_string();
    assert!(error.contains("listing limit 2"), "{error}");
}

#[test]
fn selected_extraction_writes_only_requested_entries_and_checks_all_names() {
    let d = tempfile::tempdir().unwrap();
    let z = d.path().join("selected.zip");
    raw_zip(
        &z,
        &[("chosen.txt", b"chosen"), ("ignored.txt", b"ignored")],
    );
    let dest = d.path().join("selected");
    fs::create_dir(&dest).unwrap();
    let (cancel, progress) = ctx_parts();
    let ctx = JobCtx::new(&cancel, &progress);

    let report = extract_zip_entries(
        &z,
        &dest,
        &["chosen.txt".to_owned()],
        &ExtractOptions::default(),
        &ctx,
    )
    .unwrap();
    assert_eq!(report.extracted, vec![dest.join("chosen.txt")]);
    assert_eq!(fs::read(dest.join("chosen.txt")).unwrap(), b"chosen");
    assert!(!dest.join("ignored.txt").exists());

    raw_zip(&z, &[("safe.txt", b"safe"), ("../escape.txt", b"unsafe")]);
    let malicious_dest = d.path().join("malicious");
    fs::create_dir(&malicious_dest).unwrap();
    let error = extract_zip_entries(
        &z,
        &malicious_dest,
        &["safe.txt".to_owned()],
        &ExtractOptions::default(),
        &ctx,
    )
    .unwrap_err();
    assert!(matches!(error, CoreError::UnsafePath(_)));
    assert_eq!(fs::read_dir(&malicious_dest).unwrap().count(), 0);
    assert!(!d.path().join("escape.txt").exists());
}

#[cfg(windows)]
#[test]
fn extraction_rejects_a_junction_created_during_parent_creation() {
    let d = tempfile::tempdir().unwrap();
    let archive = d.path().join("race.zip");
    raw_zip(&archive, &[("parent/escaped.txt", b"must stay inside")]);
    let dest = d.path().join("extract");
    let outside = d.path().join("outside");
    fs::create_dir(&dest).unwrap();
    fs::create_dir(&outside).unwrap();
    let junction = dest.join("parent");
    let created = Arc::new(AtomicBool::new(false));
    let created_hook = Arc::clone(&created);
    let hook_junction = junction.clone();
    let hook_target = outside.clone();
    BEFORE_PARENT_CREATE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |path| {
            if path == hook_junction.as_path() && !created_hook.load(Ordering::SeqCst) {
                let made = std::process::Command::new("cmd")
                    .args(["/C", "mklink", "/J"])
                    .arg(&hook_junction)
                    .arg(&hook_target)
                    .output()
                    .is_ok_and(|output| output.status.success());
                created_hook.store(made, Ordering::SeqCst);
            }
        }))
    });
    let (cancel, progress) = ctx_parts();

    let report = extract_zip(
        &archive,
        &dest,
        &ExtractOptions::default(),
        &JobCtx::new(&cancel, &progress),
    )
    .unwrap();
    BEFORE_PARENT_CREATE.with(|hook| *hook.borrow_mut() = None);

    if !created.load(Ordering::SeqCst) {
        eprintln!("mklink unavailable; skipping junction race assertion");
        return;
    }
    assert!(report.extracted.is_empty(), "{report:?}");
    assert!(
        report
            .failed
            .iter()
            .any(|issue| issue.reason.contains("link")),
        "{report:?}"
    );
    assert!(
        !outside.join("escaped.txt").exists(),
        "archive data must not be written through a junction"
    );
}

#[test]
fn zip_slip_is_rejected_before_writing() {
    let d = tempfile::tempdir().unwrap();
    let x = d.path().join("x");
    fs::create_dir(&x).unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    for evil in [
        "../evil.txt",
        "ok/../../evil.txt",
        "/abs.txt",
        "C:/Windows/evil.txt",
        "C:evil.txt",
        "\\\\server\\share\\evil.txt",
        "..\\evil.txt",
        "file.txt:ads",
        "CON",
    ] {
        let z = d.path().join("evil.zip");
        raw_zip(&z, &[("good.txt", b"fine"), (evil, b"pwned")]);
        let r = extract_zip(&z, &x, &ExtractOptions::default(), &ctx);
        assert!(r.is_err(), "{evil:?} must be rejected");
        assert!(list_zip(&z).unwrap().iter().any(|e| !e.safe_name));
        assert_eq!(
            fs::read_dir(&x).unwrap().count(),
            0,
            "nothing written for {evil:?}"
        );
        assert!(!d.path().join("evil.txt").exists());
        fs::remove_file(&z).unwrap();
    }
}

#[test]
fn zip_bomb_limits() {
    let d = tempfile::tempdir().unwrap();
    let x = d.path().join("x");
    fs::create_dir(&x).unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let z = d.path().join("bomb.zip");
    let zeros = vec![0u8; 20 * 1024 * 1024];
    raw_zip(&z, &[("zeros.bin", &zeros)]);
    // 20 MiB of zeros deflates ~1000:1.
    let strict = ExtractOptions {
        limits: ExtractLimits {
            max_ratio: 100,
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(matches!(
        extract_zip(&z, &x, &strict, &ctx),
        Err(CoreError::UnsafePath(_))
    ));
    let small = ExtractOptions {
        limits: ExtractLimits {
            max_total_bytes: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    raw_zip(&z, &[("a.txt", b"0123456789abc")]);
    assert!(extract_zip(&z, &x, &small, &ctx).is_err());
    let few = ExtractOptions {
        limits: ExtractLimits {
            max_entries: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    raw_zip(&z, &[("a", b"1"), ("b", b"2")]);
    assert!(extract_zip(&z, &x, &few, &ctx).is_err());
    assert_eq!(fs::read_dir(&x).unwrap().count(), 0);
}

#[test]
fn symlink_entries_skipped() {
    let d = tempfile::tempdir().unwrap();
    let z = d.path().join("l.zip");
    let mut zw = ZipWriter::new(File::create(&z).unwrap());
    zw.add_symlink("link", "/etc/passwd", SimpleFileOptions::default())
        .unwrap();
    zw.start_file("real.txt", SimpleFileOptions::default())
        .unwrap();
    zw.write_all(b"x").unwrap();
    zw.finish().unwrap();
    let x = d.path().join("x");
    fs::create_dir(&x).unwrap();
    let (c, p) = ctx_parts();
    let r = extract_zip(&z, &x, &ExtractOptions::default(), &JobCtx::new(&c, &p)).unwrap();
    assert_eq!(r.skipped.len(), 1);
    assert_eq!(r.extracted, vec![x.join("real.txt")]);
    assert!(!paths::exists_no_follow(&x.join("link")));
}
