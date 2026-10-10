//! End-to-end tests that run the built `zxraw-cli` binary against temporary folders.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zxraw-cli"))
        .args(args)
        .output()
        .expect("failed to run zxraw-cli")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("terminated by signal")
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 temp path")
}

fn write(p: &Path, data: &[u8]) {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, data).unwrap();
}

#[test]
fn hash_verify_roundtrip_detects_corruption() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(&root.join("a.txt"), b"alpha");
    write(&root.join("sub/b.txt"), b"bravo");
    let manifest = root.join("out.b3");

    let out = cli(&["hash", s(root), "--manifest", s(&manifest)]);
    assert_eq!(
        code(&out),
        0,
        "hash failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = fs::read_to_string(&manifest).unwrap();
    assert_eq!(text.lines().count(), 2, "manifest: {text}");
    assert!(
        text.contains("sub/b.txt"),
        "paths must be relative and slash-separated: {text}"
    );
    assert!(!text.contains("out.b3"), "manifest must not hash itself");

    let ok = cli(&["verify", s(&manifest)]);
    assert_eq!(
        code(&ok),
        0,
        "verify of intact tree failed: {}",
        String::from_utf8_lossy(&ok.stdout)
    );

    // Corrupt one file and remove another: verify must fail with exit 1.
    write(&root.join("sub/b.txt"), b"bravX");
    fs::remove_file(root.join("a.txt")).unwrap();
    let bad = cli(&["verify", s(&manifest)]);
    assert_eq!(code(&bad), 1);
    let stdout = String::from_utf8_lossy(&bad.stdout);
    assert!(stdout.contains("MISMATCH  sub/b.txt"), "{stdout}");
    assert!(stdout.contains("MISSING   a.txt"), "{stdout}");
}

#[test]
fn hash_prints_b3sum_style_lines_without_manifest() {
    let dir = TempDir::new().unwrap();
    let f = dir.path().join("x.bin");
    write(&f, b"hello");
    let out = cli(&["hash", s(&f)]);
    assert_eq!(code(&out), 0);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let expected = blake3::hash(b"hello").to_hex().to_string();
    assert!(stdout.starts_with(&expected), "{stdout}");
}

#[test]
fn dupes_finds_pair_and_deletes_nothing() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let a = root.join("a.bin");
    let copy = root.join("sub/copy.bin");
    let other = root.join("other.bin");
    write(&a, &[7u8; 4096]);
    write(&copy, &[7u8; 4096]);
    write(&other, &[8u8; 4096]);

    let out = cli(&["dupes", s(root), "--json", "--min-size", "1"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let groups = report["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{report}");
    assert_eq!(groups[0]["files"].as_array().unwrap().len(), 2);

    let byte = cli(&["dupes", s(root), "--byte-compare", "--min-size", "1mb"]);
    assert_eq!(code(&byte), 0);
    assert!(String::from_utf8_lossy(&byte.stdout).contains("0 duplicate group(s)"));

    // Nothing was deleted or modified.
    assert!(a.exists() && copy.exists() && other.exists());
    assert_eq!(fs::read(&copy).unwrap(), vec![7u8; 4096]);
}

#[test]
fn rename_dry_run_changes_nothing_and_apply_renames() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let a = root.join("IMG_A.JPG");
    let b = root.join("IMG_B.JPG");
    write(&a, b"one");
    write(&b, b"two");
    let rule = "template:photo_{n:3}.{ext}";

    let dry = cli(&["rename", s(&a), s(&b), "--rule", rule]);
    assert_eq!(code(&dry), 0, "{}", String::from_utf8_lossy(&dry.stderr));
    assert!(String::from_utf8_lossy(&dry.stdout).contains("photo_001.JPG"));
    assert!(a.exists() && b.exists(), "dry run must not rename");
    assert!(!root.join("photo_001.JPG").exists());

    let applied = cli(&["rename", s(&a), s(&b), "--rule", rule, "--apply"]);
    assert_eq!(
        code(&applied),
        0,
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(!a.exists() && !b.exists());
    assert_eq!(fs::read(root.join("photo_001.JPG")).unwrap(), b"one");
    assert_eq!(fs::read(root.join("photo_002.JPG")).unwrap(), b"two");
}

#[test]
fn rename_refuses_on_problems() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a.txt");
    write(&a, b"x");
    // A template producing an invalid Windows file name must be refused, even with --apply.
    let out = cli(&["rename", s(&a), "--rule", "template:bad|name", "--apply"]);
    assert_eq!(code(&out), 1);
    assert!(a.exists());
}

#[test]
fn ingest_dry_run_copies_nothing_apply_copies_and_verifies() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("card");
    let dst = dir.path().join("library");
    write(&src.join("clip_one.txt"), b"first file");
    write(&src.join("sub/clip_two.txt"), b"second file");

    let src_s = s(&src).to_string();
    let dst_s = s(&dst).to_string();
    let base = |extra: &[&str]| -> Output {
        let mut v = vec![
            "ingest",
            "--from",
            src_s.as_str(),
            "--to",
            dst_s.as_str(),
            "--folders",
            "{YYYY}",
            "--names",
            "{orig_name}",
        ];
        v.extend_from_slice(extra);
        cli(&v)
    };

    let plan = base(&[]);
    assert_eq!(code(&plan), 0, "{}", String::from_utf8_lossy(&plan.stderr));
    assert!(String::from_utf8_lossy(&plan.stdout).contains("2 file(s) to copy"));
    assert!(!dst.exists(), "dry run must not create the destination");

    let applied = base(&["--apply"]);
    assert_eq!(
        code(&applied),
        0,
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let year_dirs: Vec<PathBuf> = fs::read_dir(&dst)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(year_dirs.len(), 1, "one year folder expected");
    let names: Vec<_> = fs::read_dir(&year_dirs[0])
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().any(|n| n == "clip_one.txt"), "{names:?}");
    assert!(names.iter().any(|n| n == "clip_two.txt"), "{names:?}");
    assert_eq!(
        fs::read(year_dirs[0].join("clip_one.txt")).unwrap(),
        b"first file"
    );

    // Sources are never modified.
    assert!(src.join("clip_one.txt").exists() && src.join("sub/clip_two.txt").exists());

    // The written manifest verifies the copied files.
    let manifest = fs::read_dir(&dst)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "b3"))
        .expect("ingest manifest");
    let verify = cli(&["verify", s(&manifest)]);
    assert_eq!(
        code(&verify),
        0,
        "{}",
        String::from_utf8_lossy(&verify.stdout)
    );
}

#[test]
fn search_after_index_finds_file() {
    let dir = TempDir::new().unwrap();
    let photos = dir.path().join("photos");
    let db = dir.path().join("test.db");
    write(&photos.join("holiday_beach.txt"), b"sand");
    write(&photos.join("other.txt"), b"x");

    let idx = cli(&["index", s(&photos), "--db", s(&db)]);
    assert_eq!(code(&idx), 0, "{}", String::from_utf8_lossy(&idx.stderr));

    let out = cli(&["search", "holiday_beach", "--db", s(&db), "--json"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let hits: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let hits = hits.as_array().unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["name"], "holiday_beach.txt");
}

#[test]
fn usage_errors_exit_2() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("does-not-exist");
    let out = cli(&["hash", s(&missing)]);
    assert_eq!(code(&out), 2);
    let out = cli(&["search", "x", "--db", s(&missing)]);
    assert_eq!(code(&out), 2);
}
