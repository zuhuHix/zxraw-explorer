//! Tool-window scenarios: duplicates to the recycle sandbox, folder compare with Update sync,
//! and the pane's refresh after changes made outside the UI.
//!
//! Screenshots go to `%TEMP%\zxfr-shots\tools\`. Every removal goes to the per-profile
//! recycle sandbox (see `recycle_sandbox`), never to the real Recycle Bin.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::*;

fn shot(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("zxfr-shots").join("tools");
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// First file called `name` anywhere below `root` (the sandbox nests each removal in its own
/// numbered folder).
fn find_named(root: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_named(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
    }
    None
}

fn set_modified(path: &Path, time: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

// ---------------------------------------------------------------------------
// Duplicates
// ---------------------------------------------------------------------------

#[test]
fn duplicates_keep_first_copy_and_recycle_the_rest_into_the_sandbox() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("photos");
    let profile = sandbox.path().join("profile");
    let original = noise(64 * 1024, 11);
    write_file(&root.join("a").join("keep.bin"), &original);
    write_file(&root.join("b").join("copy.bin"), &original);
    write_file(&root.join("unique.bin"), &noise(2048, 12));
    let script = format!(
        "ui tools duplicates open {root}\n\
         ui tools duplicates wait 30\n\
         ui tools duplicates assert-groups 1\n\
         ui tools duplicates keep-first\n\
         screenshot {shot}\n\
         ui tools duplicates recycle\n\
         wait-jobs 20\n\
         assert-exists {keep}\n\
         assert-missing {copy}\n\
         exit\n",
        root = shown(&root),
        shot = shown(&shot("duplicates-keep-first.png")),
        keep = shown(&root.join("a").join("keep.bin")),
        copy = shown(&root.join("b").join("copy.bin")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(fs::read(root.join("a").join("keep.bin")).unwrap(), original);
    assert!(
        root.join("unique.bin").exists(),
        "non-duplicates are never touched"
    );
    let removed = find_named(&recycle_sandbox(&profile), "copy.bin")
        .expect("the extra copy must be in the recycle sandbox");
    assert_eq!(
        fs::read(removed).unwrap(),
        original,
        "sandbox copy keeps its bytes"
    );
}

#[test]
fn duplicate_changed_after_the_scan_is_not_recycled() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("photos");
    let profile = sandbox.path().join("profile");
    let original = noise(32 * 1024, 21);
    write_file(&root.join("a").join("keep.bin"), &original);
    write_file(&root.join("b").join("copy.bin"), &original);
    let script = format!(
        "ui tools duplicates open {root}\n\
         ui tools duplicates wait 30\n\
         ui tools external append {copy} | edited after the scan\n\
         ui tools duplicates keep-first\n\
         ui tools duplicates recycle\n\
         wait-jobs 20\n\
         assert-exists {copy}\n\
         exit\n",
        root = shown(&root),
        copy = shown(&root.join("b").join("copy.bin")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    let mut expected = original.clone();
    expected.extend_from_slice(b"edited after the scan");
    assert_eq!(
        fs::read(root.join("b").join("copy.bin")).unwrap(),
        expected,
        "a copy changed after the scan must stay where it is, unchanged"
    );
    assert_eq!(fs::read(root.join("a").join("keep.bin")).unwrap(), original);
    assert!(
        find_named(&recycle_sandbox(&profile), "copy.bin").is_none(),
        "the changed copy must not reach the sandbox"
    );
}

/// Two copies of one photo, the second one older, in folders `a` and `b`, plus an unrelated
/// file. Returns the newer copy, the older copy and the shared bytes.
fn newest_and_older_copies(root: &Path) -> (PathBuf, PathBuf, Vec<u8>) {
    let original = noise(32 * 1024, 31);
    let newest = root.join("a").join("newest.bin");
    let older = root.join("b").join("older.bin");
    write_file(&newest, &original);
    write_file(&older, &original);
    set_modified(&older, SystemTime::now() - Duration::from_secs(86_400));
    write_file(&root.join("unique.bin"), &noise(2048, 32));
    (newest, older, original)
}

#[test]
fn duplicates_scan_and_recycle_through_the_window_buttons() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("photos");
    let profile = sandbox.path().join("profile");
    let (newest, older, original) = newest_and_older_copies(&root);
    let script = format!(
        "navigate {root}\n\
         cmd FindDuplicates\n\
         ui tools click-button duplicates:Scan\n\
         ui tools duplicates wait 30\n\
         ui tools click-button duplicates:Keep newest\n\
         ui tools click-button duplicates:Move ticked to Recycle Bin\n\
         ui tools click-button duplicates:Confirm Move to Recycle Bin\n\
         wait-jobs 20\n\
         screenshot {shot}\n\
         exit\n",
        root = shown(&root),
        shot = shown(&shot("duplicates-buttons-recycled.png")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(!older.exists(), "the older copy must be recycled");
    assert_eq!(
        fs::read(&newest).unwrap(),
        original,
        "the newest copy is kept"
    );
    assert!(
        root.join("unique.bin").exists(),
        "non-duplicates are never touched"
    );
    let removed = find_named(&recycle_sandbox(&profile), "older.bin")
        .expect("the older copy must be in the recycle sandbox");
    assert_eq!(fs::read(removed).unwrap(), original);
}

#[test]
fn duplicates_keep_copies_button_recycles_nothing() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("photos");
    let profile = sandbox.path().join("profile");
    let (newest, older, original) = newest_and_older_copies(&root);
    let script = format!(
        "navigate {root}\n\
         cmd FindDuplicates\n\
         ui tools click-button duplicates:Scan\n\
         ui tools duplicates wait 30\n\
         ui tools click-button duplicates:Keep newest\n\
         ui tools click-button duplicates:Move ticked to Recycle Bin\n\
         ui tools click-button duplicates:Keep copies\n\
         wait-frames 10\n\
         exit\n",
        root = shown(&root),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(
        fs::read(&older).unwrap(),
        original,
        "cancelled: the copy stays"
    );
    assert_eq!(fs::read(&newest).unwrap(), original);
    assert!(
        find_named(&recycle_sandbox(&profile), "older.bin").is_none(),
        "nothing may reach the recycle sandbox after Keep copies"
    );
}

// ---------------------------------------------------------------------------
// Folder compare and Update sync
// ---------------------------------------------------------------------------

/// Left and right trees covering every state: newer on left, newer on right, only on each
/// side, identical, and a nested folder. Returns the left and right roots.
fn compare_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let left = root.join("left");
    let right = root.join("right");
    let base = SystemTime::now() - Duration::from_secs(86_400);
    let newer = base + Duration::from_secs(3600);

    write_file(&left.join("newer-on-left.txt"), b"left version, newer");
    write_file(&right.join("newer-on-left.txt"), b"right version, older");
    set_modified(&left.join("newer-on-left.txt"), newer);
    set_modified(&right.join("newer-on-left.txt"), base);

    write_file(&left.join("right-newer.txt"), b"left version, older");
    write_file(&right.join("right-newer.txt"), b"right version, newer");
    set_modified(&left.join("right-newer.txt"), base);
    set_modified(&right.join("right-newer.txt"), newer);

    write_file(&left.join("identical.txt"), b"same bytes");
    write_file(&right.join("identical.txt"), b"same bytes");
    set_modified(&left.join("identical.txt"), base);
    set_modified(&right.join("identical.txt"), base);

    write_file(&left.join("only-left.txt"), b"only on the left");
    write_file(&right.join("only-right.txt"), b"only on the right");
    write_file(
        &left.join("nested").join("deep").join("leaf.txt"),
        &noise(4096, 31),
    );
    (left, right)
}

#[test]
fn update_copies_only_newer_and_missing_files_and_never_deletes() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let (left, right) = compare_fixture(sandbox.path());
    let leaf = noise(4096, 31);
    let right_newer_before = fs::read(right.join("right-newer.txt")).unwrap();
    let script = format!(
        "ui tools compare open {left}\n\
         ui tools compare set-right {right}\n\
         ui tools compare start\n\
         ui tools compare wait 30\n\
         ui tools compare assert-entries same=1 different=2 right-only=1\n\
         ui tools compare plan-update\n\
         ui tools compare assert-copy newer-on-left.txt\n\
         ui tools compare assert-copy only-left.txt\n\
         ui tools compare assert-copy nested/deep/leaf.txt\n\
         ui tools compare assert-no-copy right-newer.txt\n\
         ui tools compare assert-no-copy only-right.txt\n\
         ui tools compare assert-no-copy identical.txt\n\
         ui tools compare assert-skipped right-newer.txt\n\
         ui tools compare assert-plan copies=3 deletes=0 skipped=1\n\
         screenshot {shot}\n\
         ui tools compare execute-update\n\
         ui tools compare wait-sync 30\n\
         ui tools compare assert-sync done=3 failed=0\n\
         wait-jobs 20\n\
         exit\n",
        left = shown(&left),
        right = shown(&right),
        shot = shown(&shot("compare-update-plan.png")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(
        fs::read(right.join("newer-on-left.txt")).unwrap(),
        b"left version, newer",
        "a newer left file replaces the older right copy"
    );
    assert_eq!(
        fs::read(right.join("only-left.txt")).unwrap(),
        b"only on the left"
    );
    assert_eq!(fs::read(right.join("nested/deep/leaf.txt")).unwrap(), leaf);
    assert_eq!(
        fs::read(right.join("right-newer.txt")).unwrap(),
        right_newer_before,
        "a newer right file is never overwritten"
    );
    assert_eq!(
        fs::read(right.join("only-right.txt")).unwrap(),
        b"only on the right"
    );
    assert_eq!(
        fs::read(right.join("identical.txt")).unwrap(),
        b"same bytes"
    );
    assert_eq!(
        fs::read(left.join("right-newer.txt")).unwrap(),
        b"left version, older"
    );
    assert!(
        !recycle_sandbox(&profile).exists(),
        "Update never deletes, so nothing reaches the sandbox"
    );
}

#[test]
fn two_way_copies_each_sides_only_file_to_the_other_side() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let left = sandbox.path().join("left");
    let right = sandbox.path().join("right");
    fs::create_dir_all(&left).unwrap();
    fs::create_dir_all(&right).unwrap();
    write_file(&left.join("a.txt"), b"alpha from the left");
    write_file(&right.join("b.txt"), b"beta from the right");
    let script = format!(
        "ui tools compare open {left}\n\
         ui tools compare set-right {right}\n\
         ui tools compare start\n\
         ui tools compare wait 30\n\
         ui tools compare plan-two-way\n\
         ui tools compare execute-two-way\n\
         ui tools compare wait-sync 30\n\
         ui tools compare assert-sync done=2 failed=0\n\
         wait-jobs 20\n\
         exit\n",
        left = shown(&left),
        right = shown(&right),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(
        fs::read(right.join("a.txt")).unwrap(),
        b"alpha from the left"
    );
    assert_eq!(
        fs::read(left.join("a.txt")).unwrap(),
        b"alpha from the left"
    );
    assert_eq!(
        fs::read(left.join("b.txt")).unwrap(),
        b"beta from the right"
    );
    assert_eq!(
        fs::read(right.join("b.txt")).unwrap(),
        b"beta from the right"
    );
    assert!(
        !recycle_sandbox(&profile).exists(),
        "Two-way never deletes, so nothing reaches the sandbox"
    );
}

#[test]
fn update_refuses_a_left_file_changed_after_the_preview() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let (left, right) = compare_fixture(sandbox.path());
    let script = format!(
        "ui tools compare open {left}\n\
         ui tools compare set-right {right}\n\
         ui tools compare start\n\
         ui tools compare wait 30\n\
         ui tools compare plan-update\n\
         ui tools external append {only} | added after the preview\n\
         ui tools compare execute-update\n\
         ui tools compare wait-sync 30\n\
         ui tools compare assert-sync failed=1\n\
         wait-jobs 20\n\
         exit\n",
        left = shown(&left),
        right = shown(&right),
        only = shown(&left.join("only-left.txt")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(
        !right.join("only-left.txt").exists(),
        "a stale preview must not copy the changed file"
    );
    let mut changed = b"only on the left".to_vec();
    changed.extend_from_slice(b"added after the preview");
    assert_eq!(fs::read(left.join("only-left.txt")).unwrap(), changed);
}

// ---------------------------------------------------------------------------
// Auto-refresh after changes made outside the UI
// ---------------------------------------------------------------------------

#[test]
fn pane_follows_external_create_rename_append_and_delete_without_a_refresh() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let dir = sandbox.path().join("watched");
    write_file(&dir.join("seed.txt"), b"seed");
    let script = format!(
        "navigate {dir}\n\
         wait-entry seed.txt | 10\n\
         ui tools external create {dir}/new.txt\n\
         wait-entry new.txt | 15\n\
         ui tools external rename {dir}/new.txt | {dir}/renamed.txt\n\
         wait-entry renamed.txt | 15\n\
         wait-no-entry new.txt | 15\n\
         ui tools external append {dir}/renamed.txt | more\n\
         ui tools wait-entry-size renamed.txt 4100 15\n\
         screenshot {shot}\n\
         ui tools external delete {dir}/renamed.txt\n\
         wait-no-entry renamed.txt | 15\n\
         assert-entry seed.txt\n\
         exit\n",
        dir = shown(&dir),
        shot = shown(&shot("refresh-after-append.png")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(!dir.join("renamed.txt").exists());
    assert_eq!(fs::read(dir.join("seed.txt")).unwrap(), b"seed");
}

#[test]
fn burst_of_500_external_files_converges_without_freezing() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let dir = sandbox.path().join("burst");
    fs::create_dir_all(&dir).unwrap();
    let script = format!(
        "limit 120\n\
         navigate {dir}\n\
         wait-frames 10\n\
         ui tools external create-many {dir} 500\n\
         ui tools wait-listing 500 90\n\
         assert-entry burst-00000.txt\n\
         assert-entry burst-00499.txt\n\
         screenshot {shot}\n\
         exit\n",
        dir = shown(&dir),
        shot = shown(&shot("burst-500.png")),
    );

    let outcome = run_app_with(
        sandbox.path(),
        &profile,
        &script,
        &RunOpts {
            limit: Some(Duration::from_secs(150)),
            ..Default::default()
        },
    );

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 500);
}

#[test]
fn deleting_the_shown_folder_externally_shows_a_message_without_crashing() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let parent = sandbox.path().join("parent");
    let shown_dir = parent.join("inner");
    write_file(&shown_dir.join("inside.txt"), b"inside");
    let script = format!(
        "navigate {inner}\n\
         wait-entry inside.txt | 10\n\
         ui tools external delete {inner}\n\
         wait-missing {inner} | 15\n\
         wait-frames 30\n\
         screenshot {shot}\n\
         assert-path {inner}\n\
         exit\n",
        inner = shown(&shown_dir),
        shot = shown(&shot("shown-folder-deleted.png")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(!shown_dir.exists());
}

// ---------------------------------------------------------------------------
// ZIP archives
// ---------------------------------------------------------------------------

/// Every file below `root` as (slash-separated relative path, bytes), sorted.
fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(read) = fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// A ZIP file with stored (uncompressed) entries whose names are written exactly as given,
/// so the test can include names a well-behaved writer would never produce.
fn raw_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let fields = |buf: &mut Vec<u8>| {
            buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
            buf.extend_from_slice(&0u16.to_le_bytes()); // flags
            buf.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            buf.extend_from_slice(&0u16.to_le_bytes()); // time
            buf.extend_from_slice(&0x21u16.to_le_bytes()); // date: 1980-01-01
            buf.extend_from_slice(&crc.to_le_bytes());
            buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
            buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
            buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
            buf.extend_from_slice(&0u16.to_le_bytes()); // extra length
        };
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        fields(&mut out);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        fields(&mut central);
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        central.extend_from_slice(&0u32.to_le_bytes()); // external attributes
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[test]
fn zip_round_trip_of_nested_and_unicode_names_is_byte_identical() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let src = sandbox.path().join("src");
    write_file(
        &src.join("nested").join("deep").join("ü-file.txt"),
        b"nested text\n",
    );
    write_file(
        &src.join("nested").join("noise.bin"),
        &noise(300 * 1024, 41),
    );
    write_file(&src.join("plain.txt"), b"plain\n");
    write_file(&src.join("Ωmega.bin"), &noise(70 * 1024, 42));
    let out = sandbox.path().join("out");
    let dest = sandbox.path().join("restored");
    fs::create_dir_all(&out).unwrap();
    let script = format!(
        "ui tools archive create {out} | round.zip | {nested} | {plain} | {omega}\n\
         wait-jobs 30\n\
         ui tools archive open {zip}\n\
         ui tools archive wait 30\n\
         ui tools archive assert-entry nested/deep/ü-file.txt\n\
         ui tools archive assert-entry Ωmega.bin\n\
         screenshot {shot}\n\
         ui tools archive extract {dest}\n\
         wait-jobs 30\n\
         exit\n",
        out = shown(&out),
        nested = shown(&src.join("nested")),
        plain = shown(&src.join("plain.txt")),
        omega = shown(&src.join("Ωmega.bin")),
        zip = shown(&out.join("round.zip")),
        dest = shown(&dest),
        shot = shown(&shot("archive-browser.png")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(
        tree(&dest),
        tree(&src),
        "extracted tree must match the source byte for byte"
    );
}

#[test]
fn hostile_zip_entries_are_rejected_and_nothing_escapes_the_target() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let zip = sandbox.path().join("hostile.zip");
    let dest = sandbox.path().join("dest");
    fs::create_dir_all(&dest).unwrap();
    fs::write(
        &zip,
        raw_zip(&[
            ("safe/ok.txt", b"fine"),
            ("../evil.txt", b"escaped"),
            ("/abs/absolute.txt", b"absolute"),
            ("C:/drive.txt", b"drive"),
        ]),
    )
    .unwrap();
    let script = format!(
        "ui tools archive open {zip}\n\
         ui tools archive wait 30\n\
         ui tools archive assert-count 4\n\
         ui tools archive extract {dest}\n\
         wait-jobs 30\n\
         exit\n",
        zip = shown(&zip),
        dest = shown(&dest),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    // Documented policy (archive.rs): one unsafe name rejects the whole archive before any
    // entry is written, so not even the safe entry may appear.
    assert_eq!(
        tree(&dest),
        Vec::new(),
        "a hostile archive must write nothing"
    );
    assert!(
        !sandbox.path().join("evil.txt").exists(),
        "`../` must not escape"
    );
    assert!(
        !Path::new("/abs/absolute.txt").exists(),
        "absolute paths must not be written"
    );
}

#[test]
fn extracting_over_an_existing_file_never_replaces_it() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let zip = sandbox.path().join("shared.zip");
    let dest = sandbox.path().join("dest");
    fs::write(
        &zip,
        raw_zip(&[("keep.txt", b"from the archive"), ("new.txt", b"new")]),
    )
    .unwrap();
    write_file(&dest.join("keep.txt"), b"my own version");
    let script = format!(
        "ui tools archive open {zip}\n\
         ui tools archive wait 30\n\
         ui tools archive extract {dest}\n\
         wait-jobs 30\n\
         exit\n",
        zip = shown(&zip),
        dest = shown(&dest),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(
        fs::read(dest.join("keep.txt")).unwrap(),
        b"my own version",
        "an existing file is never replaced by extraction"
    );
    assert_eq!(fs::read(dest.join("new.txt")).unwrap(), b"new");
}

#[test]
fn archive_extract_through_the_window_buttons_keeps_existing_files() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let folder = sandbox.path().join("zips");
    let zip = folder.join("shared.zip");
    let dest = sandbox.path().join("dest");
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        &zip,
        raw_zip(&[("keep.txt", b"from the archive"), ("new.txt", b"new")]),
    )
    .unwrap();
    write_file(&dest.join("keep.txt"), b"my own version");
    let script = format!(
        "navigate {folder}\n\
         select shared.zip\n\
         cmd BrowseArchive\n\
         ui tools archive wait 30\n\
         dialog {dest}\n\
         ui tools click-button archive:Choose…\n\
         ui tools click-button archive:Extract all\n\
         wait-jobs 30\n\
         screenshot {shot}\n\
         exit\n",
        folder = shown(&folder),
        dest = shown(&dest),
        shot = shown(&shot("archive-buttons-extracted.png")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(fs::read(dest.join("new.txt")).unwrap(), b"new");
    assert_eq!(
        fs::read(dest.join("keep.txt")).unwrap(),
        b"my own version",
        "an existing file is never replaced by the window's Extract all"
    );
}

#[test]
fn zipping_a_folder_with_a_junction_does_not_follow_it() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let src = sandbox.path().join("tree");
    let target = sandbox.path().join("outside");
    write_file(&src.join("real.txt"), b"real");
    write_file(&target.join("secret.txt"), b"must not be archived");
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(src.join("link"))
        .arg(&target)
        .output();
    if !made.is_ok_and(|out| out.status.success()) {
        eprintln!("skipped: junction creation is not permitted here");
        return;
    }
    let out = sandbox.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let script = format!(
        "ui tools archive create {out} | with-junction.zip | {src}\n\
         wait-jobs 30\n\
         exit\n",
        out = shown(&out),
        src = shown(&src),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    let entries = forge_core::archive::list_zip(&out.join("with-junction.zip")).unwrap();
    let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
    assert!(names.iter().any(|n| n.ends_with("real.txt")), "{names:?}");
    assert!(
        names.iter().all(|n| !n.contains("secret.txt")),
        "the junction target must not be archived: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Split and join
// ---------------------------------------------------------------------------

/// Part size for the split scenarios: 5 MiB becomes three parts.
const PART: u64 = 2 * 1024 * 1024;

/// The `.b3split` manifest in `dir` and what it describes.
fn manifest_of(dir: &Path) -> (PathBuf, forge_core::split::SplitManifest) {
    let manifest = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("b3split"))
        })
        .expect("the split must write a .b3split manifest");
    let parsed = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    (manifest, parsed)
}

fn split_script(file: &Path) -> String {
    format!(
        "ui tools split {file} {PART}\nwait-jobs 60\nexit\n",
        file = shown(file)
    )
}

#[test]
fn split_then_join_restores_a_5_mib_file_byte_for_byte() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let dir = sandbox.path().join("media");
    let file = dir.join("big.bin");
    let original = noise(5 * 1024 * 1024, 51);
    write_file(&file, &original);
    let script = format!(
        "ui tools split {file} {PART}\n\
         wait-jobs 60\n\
         screenshot {shot}\n\
         exit\n",
        file = shown(&file),
        shot = shown(&shot("split-5mib.png")),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    let (manifest, parsed) = manifest_of(&dir);
    assert_eq!(parsed.parts.len(), 3, "5 MiB in 2 MiB parts is three parts");

    fs::remove_file(&file).unwrap();
    let script = format!(
        "ui tools join {manifest}\nwait-jobs 60\nexit\n",
        manifest = shown(&manifest)
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(
        fs::read(&file).unwrap(),
        original,
        "joined bytes must match the original"
    );
}

#[test]
fn join_reports_a_missing_part_and_publishes_nothing() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let dir = sandbox.path().join("media");
    let file = dir.join("big.bin");
    write_file(&file, &noise(5 * 1024 * 1024, 52));
    expect_ok(&run_app(sandbox.path(), &profile, &split_script(&file)));
    let (manifest, parsed) = manifest_of(&dir);
    fs::remove_file(&file).unwrap();
    fs::remove_file(dir.join(&parsed.parts[1].name)).unwrap();

    let script = format!(
        "ui tools join {manifest}\nwait-jobs 60\nassert-toast missing part {part}\nassert-missing {file}\nexit\n",
        manifest = shown(&manifest),
        part = parsed.parts[1].name,
        file = shown(&file),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(
        !file.exists(),
        "a join with a missing part must not publish output"
    );
}

#[test]
fn join_detects_a_corrupted_part_and_publishes_nothing() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let dir = sandbox.path().join("media");
    let file = dir.join("big.bin");
    write_file(&file, &noise(5 * 1024 * 1024, 53));
    expect_ok(&run_app(sandbox.path(), &profile, &split_script(&file)));
    let (manifest, parsed) = manifest_of(&dir);
    fs::remove_file(&file).unwrap();
    let part = dir.join(&parsed.parts[2].name);
    let mut bytes = fs::read(&part).unwrap();
    bytes[10] ^= 0xFF;
    fs::write(&part, bytes).unwrap();

    let script = format!(
        "ui tools join {manifest}\nwait-jobs 60\nassert-toast corrupt part {part}: BLAKE3 mismatch\nassert-missing {file}\nexit\n",
        manifest = shown(&manifest),
        part = parsed.parts[2].name,
        file = shown(&file),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(!file.exists(), "a corrupted part must not produce output");
}

#[test]
fn join_never_replaces_an_existing_output_file() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let dir = sandbox.path().join("media");
    let file = dir.join("big.bin");
    write_file(&file, &noise(5 * 1024 * 1024, 54));
    expect_ok(&run_app(sandbox.path(), &profile, &split_script(&file)));
    let (manifest, _) = manifest_of(&dir);
    fs::write(&file, b"my own file, not the split source").unwrap();

    let script = format!(
        "ui tools join {manifest}\nwait-jobs 60\nexit\n",
        manifest = shown(&manifest)
    );
    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(
        fs::read(&file).unwrap(),
        b"my own file, not the split source",
        "join must not replace an existing file"
    );
}

// ---------------------------------------------------------------------------
// Find and replace
// ---------------------------------------------------------------------------

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// UTF-16 LE text with its byte-order mark, as Notepad saves "Unicode" files.
fn utf16_le_bom(text: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// Every file below `root`, in any subfolder.
fn files_below(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(root).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_below(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn find_replace_preview_then_replace_keeps_encodings_and_line_endings() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("notes");
    let profile = sandbox.path().join("profile");
    let photo = [b"cat".as_slice(), &noise(4096, 41)].concat();
    let nul = b"cat\0 text\n".to_vec();
    // 1.1 MiB: over the 1 MiB limit the script sets.
    let big = "cat line\n".repeat(130_000).into_bytes();
    write_file(&root.join("a.txt"), b"cat and cat\nthe end\n");
    write_file(
        &root.join("b.txt"),
        &[UTF8_BOM, b"a cat\r\nsecond line\r\n"].concat(),
    );
    write_file(&root.join("c.txt"), &utf16_le_bom("cat here\r\nsecond\r\n"));
    write_file(&root.join("photo.cr3"), &photo);
    write_file(&root.join("nul.txt"), &nul);
    write_file(&root.join("big.log"), &big);
    write_file(&root.join("sub").join("d.md"), b"cat in a subfolder\n");
    let script = format!(
        "ui tools replace open {root}\n\
         ui tools replace set find cat\n\
         ui tools replace set replace dog\n\
         ui tools replace set whole-word on\n\
         ui tools replace set max-mib 1\n\
         ui tools replace preview\n\
         ui tools replace wait-preview 30\n\
         ui tools replace assert-preview files=4 matches=5 skipped=3\n\
         ui tools replace assert-skipped photo.cr3 binary-extension\n\
         ui tools replace assert-skipped nul.txt binary\n\
         ui tools replace assert-skipped big.log too-large\n\
         ui tools replace untick d.md\n\
         ui tools replace run\n\
         ui tools replace wait-apply 30\n\
         ui tools replace assert-apply files=3 replacements=4 skipped=0 failed=0\n\
         wait-jobs 20\n\
         exit\n",
        root = shown(&root),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(
        fs::read(root.join("a.txt")).unwrap(),
        b"dog and dog\nthe end\n"
    );
    assert_eq!(
        fs::read(root.join("b.txt")).unwrap(),
        [UTF8_BOM, b"a dog\r\nsecond line\r\n"].concat(),
        "UTF-8 BOM and CRLF are kept"
    );
    assert_eq!(
        fs::read(root.join("c.txt")).unwrap(),
        utf16_le_bom("dog here\r\nsecond\r\n"),
        "UTF-16 LE with BOM and CRLF are kept"
    );
    assert_eq!(
        fs::read(root.join("sub").join("d.md")).unwrap(),
        b"cat in a subfolder\n",
        "an unticked file is not changed"
    );
    assert_eq!(fs::read(root.join("photo.cr3")).unwrap(), photo);
    assert_eq!(fs::read(root.join("nul.txt")).unwrap(), nul);
    assert_eq!(fs::read(root.join("big.log")).unwrap(), big);
    let leftovers: Vec<PathBuf> = files_below(&root)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

#[test]
fn find_replace_undo_restores_the_original_bytes() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("ranges");
    let profile = sandbox.path().join("profile");
    let ranges = b"10-20\n30-45\nno numbers here\n".to_vec();
    let crlf = [UTF8_BOM, b"1-2\r\n3-4\r\n"].concat();
    write_file(&root.join("ranges.txt"), &ranges);
    write_file(&root.join("crlf.txt"), &crlf);
    write_file(&root.join("external.txt"), b"5-6\n");
    let script = format!(
        "ui tools replace open {root}\n\
         ui tools replace set find (\\d+)-(\\d+)\n\
         ui tools replace set replace $2-$1\n\
         ui tools replace set regex on\n\
         ui tools replace preview\n\
         ui tools replace wait-preview 30\n\
         ui tools replace assert-preview files=3 matches=5 hidden=0 skipped=0\n\
         ui tools replace run\n\
         ui tools replace wait-apply 30\n\
         ui tools replace assert-apply files=3 replacements=5 skipped=0 failed=0\n\
         ui tools external append {external} | late edit\n\
         ui tools replace undo\n\
         wait-jobs 20\n\
         exit\n",
        root = shown(&root),
        external = shown(&root.join("external.txt")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(fs::read(root.join("ranges.txt")).unwrap(), ranges);
    assert_eq!(fs::read(root.join("crlf.txt")).unwrap(), crlf);
    assert_eq!(
        fs::read(root.join("external.txt")).unwrap(),
        b"6-5\nlate edit",
        "a file edited after the replace is not restored by undo"
    );
}

#[test]
fn find_replace_through_the_window_buttons() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("notes");
    let profile = sandbox.path().join("profile");
    let file = root.join("a.txt");
    write_file(&file, b"hello world\nhello again\n");
    let script = format!(
        "navigate {root}\n\
         cmd FindReplace\n\
         ui tools replace set find hello\n\
         ui tools replace set replace bye\n\
         ui tools click-button replace:Preview\n\
         ui tools replace wait-preview 30\n\
         ui tools click-button replace:Replace\n\
         ui tools replace wait-apply 30\n\
         wait-jobs 20\n\
         screenshot {shot}\n\
         assert-contains {file} | bye world\n\
         assert-contains {file} | bye again\n\
         exit\n",
        root = shown(&root),
        file = shown(&file),
        shot = shown(&shot("find-replace-buttons.png")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(fs::read(&file).unwrap(), b"bye world\nbye again\n");
}

#[test]
fn find_replace_skips_online_only_files() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("cloud");
    let profile = sandbox.path().join("profile");
    let local = root.join("local.txt");
    let cloud = root.join("cloud.txt");
    write_file(&local, b"cat\n");
    write_file(&cloud, b"cat in the cloud\n");
    let script = format!(
        "ui tools simulate-online-only {cloud}\n\
         ui tools replace open {root}\n\
         ui tools replace set find cat\n\
         ui tools replace set replace dog\n\
         ui tools replace preview\n\
         ui tools replace wait-preview 30\n\
         ui tools replace assert-skipped cloud.txt online-only\n\
         ui tools replace assert-preview files=1 matches=1 skipped=1\n\
         ui tools replace run\n\
         ui tools replace wait-apply 30\n\
         ui tools replace assert-apply files=1 replacements=1 skipped=0 failed=0\n\
         wait-jobs 20\n\
         exit\n",
        cloud = shown(&cloud),
        root = shown(&root),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(fs::read(&local).unwrap(), b"dog\n");
    assert_eq!(
        fs::read(&cloud).unwrap(),
        b"cat in the cloud\n",
        "an online-only file is never read, so never changed"
    );
}

#[test]
fn checksums_ask_before_hashing_online_only_files() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("shoot");
    let local_only = sandbox.path().join("local");
    let profile = sandbox.path().join("profile");
    let cloud = root.join("cloud.mov");
    write_file(&root.join("local.txt"), b"local");
    write_file(&cloud, b"online-only");
    write_file(&local_only.join("a.txt"), b"a");
    write_file(&local_only.join("b.txt"), b"b");
    let script = format!(
        "ui tools checksums open {local_only}\n\
         ui tools checksums wait 30\n\
         ui tools checksums assert entries=2 skipped=0\n\
         ui tools simulate-online-only {cloud}\n\
         ui tools checksums open {root}\n\
         ui tools checksums wait-prompt 1 30\n\
         ui tools click-button checksums:Skip online-only\n\
         ui tools checksums wait 30\n\
         ui tools checksums assert entries=1 skipped=1\n\
         ui tools checksums open {root}\n\
         ui tools checksums wait-prompt 1 30\n\
         ui tools click-button checksums:Download and hash\n\
         ui tools checksums wait 30\n\
         ui tools checksums assert entries=2 skipped=0\n\
         wait-jobs 20\n\
         exit\n",
        cloud = shown(&cloud),
        root = shown(&root),
        local_only = shown(&local_only),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
}

// ---------------------------------------------------------------------------
// Backup check
// ---------------------------------------------------------------------------

/// Original and backup folders covering every status: same, changed with the same size and
/// date, missing, extra, a renamed file (moved), and an online-only file. Returns both roots.
/// The root holds no subfolders, so the counts stay flat.
fn backup_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let original = root.join("original");
    let backup = root.join("backup");
    let base = SystemTime::now() - Duration::from_secs(86_400);
    write_file(&original.join("same.txt"), b"same bytes");
    write_file(&backup.join("same.txt"), b"same bytes");
    write_file(&original.join("changed.bin"), &noise(4096, 41));
    write_file(&backup.join("changed.bin"), &noise(4096, 42));
    for path in [
        original.join("same.txt"),
        backup.join("same.txt"),
        original.join("changed.bin"),
        backup.join("changed.bin"),
    ] {
        set_modified(&path, base);
    }
    write_file(&original.join("missing.txt"), b"only in the original");
    write_file(
        &backup.join("extra.txt"),
        b"only in the backup, longer name",
    );
    write_file(&original.join("old-name.bin"), &noise(8192, 51));
    write_file(&backup.join("new-name.bin"), &noise(8192, 51));
    write_file(&original.join("cloud.txt"), b"online-only in the original");
    write_file(&backup.join("cloud.txt"), b"online-only in the original");
    (original, backup)
}

#[test]
fn verify_backup_reports_missing_differs_extra_and_moved() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let (original, backup) = backup_fixture(sandbox.path());
    let csv = sandbox.path().join("report.csv");
    let script = format!(
        "ui tools simulate-online-only {cloud}\n\
         ui tools verify open {original}\n\
         ui tools verify set-backup {backup}\n\
         ui tools verify options full=on moves=on\n\
         ui tools verify start\n\
         ui tools verify wait 60\n\
         ui tools verify assert match=1 missing=1 differs=1 extra=1 moved=1 not-checked=1 errors=0\n\
         ui tools verify assert-status same.txt match\n\
         ui tools verify assert-status changed.bin differs\n\
         ui tools verify assert-status missing.txt missing\n\
         ui tools verify assert-status old-name.bin moved\n\
         ui tools verify assert-status cloud.txt not-checked\n\
         ui tools verify export {csv} csv\n\
         assert-exists {csv}\n\
         assert-contains {csv} | missing\n\
         assert-contains {csv} | missing.txt\n\
         assert-contains {csv} | new-name.bin\n\
         exit\n",
        cloud = shown(&original.join("cloud.txt")),
        original = shown(&original),
        backup = shown(&backup),
        csv = shown(&csv),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(
        fs::read(original.join("changed.bin")).unwrap(),
        noise(4096, 41),
        "the check only reads"
    );
    assert!(csv.exists());
}

#[test]
fn verify_backup_quick_mode_trusts_size_and_date() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let original = sandbox.path().join("original");
    let backup = sandbox.path().join("backup");
    let base = SystemTime::now() - Duration::from_secs(86_400);
    write_file(&original.join("same.txt"), b"same bytes");
    write_file(&backup.join("same.txt"), b"same bytes");
    write_file(&original.join("changed.bin"), &noise(4096, 41));
    write_file(&backup.join("changed.bin"), &noise(4096, 42));
    for path in [
        original.join("same.txt"),
        backup.join("same.txt"),
        original.join("changed.bin"),
        backup.join("changed.bin"),
    ] {
        set_modified(&path, base);
    }
    // Both runs use the same windows: the second changes the mode, not the folders.
    let script = format!(
        "ui tools verify open {original}\n\
         ui tools verify set-backup {backup}\n\
         ui tools verify options full=off moves=off\n\
         ui tools verify start\n\
         ui tools verify wait 60\n\
         ui tools verify assert match=2 differs=0 errors=0\n\
         ui tools verify options full=on moves=off\n\
         ui tools verify start\n\
         ui tools verify wait 60\n\
         ui tools verify assert match=1 differs=1 errors=0\n\
         ui tools verify assert-status changed.bin differs\n\
         exit\n",
        original = shown(&original),
        backup = shown(&backup),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn verify_backup_can_be_cancelled() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let original = sandbox.path().join("original");
    let backup = sandbox.path().join("backup");
    // 24 pairs of 8 MiB, 384 MB in all: enough work to be cancelled mid-run, within the size
    // budget. The bytes are shared, so each copy is identical and every pair is hashed.
    let block = noise(8 * 1024 * 1024, 61);
    for index in 0..24 {
        write_file(&original.join(format!("photo-{index:02}.raw")), &block);
        write_file(&backup.join(format!("photo-{index:02}.raw")), &block);
    }
    // Cancel right after the start: the check is still hashing, and the next step runs one
    // frame later. A sleep would let the check finish first.
    let script = format!(
        "limit 120\n\
         ui tools verify open {original}\n\
         ui tools verify set-backup {backup}\n\
         ui tools verify options full=on moves=off\n\
         ui tools verify start\n\
         ui tools verify cancel\n\
         ui tools verify wait-cancelled 60\n\
         exit\n",
        original = shown(&original),
        backup = shown(&backup),
    );

    let outcome = run_app_with(
        sandbox.path(),
        &profile,
        &script,
        &RunOpts {
            limit: Some(Duration::from_secs(120)),
            ..Default::default()
        },
    );

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(
        fs::read(original.join("photo-00.raw")).unwrap(),
        block,
        "a cancelled check changes nothing"
    );
}
