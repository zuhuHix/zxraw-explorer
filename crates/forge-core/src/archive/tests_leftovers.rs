//! Leftover temp files: removed by extraction from folders it writes into, and by the startup
//! prune of `%TEMP%\ZXraw Archive Temp`.

use super::*;
use crate::ops::leftover_tests::{junction, old, set_mtime};
use forge_jobs::{CancelToken, Progress};
use std::io::Write;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

/// Two days ago: older than the prune age used at startup (24 hours).
fn two_days_old() -> SystemTime {
    SystemTime::now() - Duration::from_secs(2 * 24 * 3600)
}

const PRUNE_AGE: Duration = Duration::from_secs(24 * 3600);

fn folder_with_file(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("f.bin"), b"x").unwrap();
}

#[test]
fn extraction_removes_old_leftovers_from_the_destination_and_its_folders() {
    let d = tempfile::tempdir().unwrap();
    let archive = d.path().join("a.zip");
    let mut zw = ZipWriter::new(fs::File::create(&archive).unwrap());
    zw.start_file("sub/file.txt", SimpleFileOptions::default())
        .unwrap();
    zw.write_all(b"data").unwrap();
    zw.finish().unwrap();

    let dest = d.path().join("out");
    fs::create_dir_all(dest.join("sub")).unwrap();
    let root_old = dest.join(".forge-partial-0123456789abcdef");
    let sub_old = dest.join("sub").join(ops::partial_name());
    let sub_young = dest.join("sub").join(ops::partial_name());
    let sub_notes = dest.join("sub").join(".forge-partial-notes.txt");
    for f in [&root_old, &sub_old, &sub_young, &sub_notes] {
        fs::write(f, b"leftover").unwrap();
    }
    for f in [&root_old, &sub_old, &sub_notes] {
        set_mtime(f, old());
    }

    let (c, p) = (CancelToken::new(), Progress::default());
    extract(
        &archive,
        &dest,
        &ExtractOptions::default(),
        None,
        &JobCtx::new(&c, &p),
    )
    .unwrap();

    assert!(
        !root_old.exists(),
        "old legacy leftover in the destination removed"
    );
    assert!(
        !sub_old.exists(),
        "old leftover in an existing folder removed"
    );
    assert!(sub_young.exists(), "a young leftover is kept");
    assert!(
        sub_notes.exists(),
        "a user's file with a near-miss name is kept"
    );
    assert_eq!(fs::read(dest.join("sub/file.txt")).unwrap(), b"data");
}

#[test]
fn startup_prune_keeps_live_stage_folders_and_removes_dead_old_ones() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let live = root.join(format!("out-{}-1", std::process::id()));
    let dead = root.join("out-4294967000-7");
    let young = root.join("out-4294967001-8");
    let opened = root.join("0123456789abcdef");
    for dir in [&live, &dead, &young, &opened] {
        folder_with_file(dir);
    }
    for dir in [&live, &dead, &opened] {
        set_mtime(&dir.join("f.bin"), two_days_old());
        set_mtime(dir, two_days_old());
    }

    prune_dir(root, PRUNE_AGE);

    assert!(live.is_dir(), "a stage folder of a running process is kept");
    assert!(
        !dead.exists(),
        "an old stage folder of a dead process is removed"
    );
    assert!(young.is_dir(), "a young folder is kept");
    assert!(
        !opened.exists(),
        "an old temp folder of an opened entry is removed"
    );
}

#[test]
fn startup_prune_keeps_a_folder_with_a_young_file() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("out-4294967002-9");
    folder_with_file(&dir);
    // The folder itself is old but a file in it was just written.
    set_mtime(&dir, two_days_old());
    prune_dir(d.path(), PRUNE_AGE);
    assert!(dir.is_dir(), "a folder still being written is kept");
}

#[test]
fn startup_prune_removes_stray_old_partials_in_the_root_only() {
    let d = tempfile::tempdir().unwrap();
    let old_file = d.path().join(ops::partial_name());
    let young_file = d.path().join(ops::partial_name());
    fs::write(&old_file, b"stray").unwrap();
    fs::write(&young_file, b"stray").unwrap();
    set_mtime(&old_file, two_days_old());
    prune_dir(d.path(), PRUNE_AGE);
    assert!(!old_file.exists(), "an old stray partial is removed");
    assert!(young_file.exists(), "a young stray partial is kept");
}

#[test]
fn startup_prune_never_follows_a_junction() {
    let d = tempfile::tempdir().unwrap();
    let outside = d.path().join("outside");
    folder_with_file(&outside);
    let root = d.path().join("root");
    fs::create_dir_all(&root).unwrap();
    // An old junction in the temp root, and one inside a dead stage folder.
    let link = root.join("link");
    let stage = root.join("out-4294967003-2");
    fs::create_dir_all(&stage).unwrap();
    let stage_link = stage.join("link");
    if junction(&link, &outside).is_none() || junction(&stage_link, &outside).is_none() {
        eprintln!("skipped: mklink /J unavailable");
        return;
    }
    set_mtime(&link, two_days_old());
    set_mtime(&stage_link, two_days_old());
    set_mtime(&stage, two_days_old());
    set_mtime(&root, two_days_old());

    prune_dir(&root, PRUNE_AGE);

    assert!(
        outside.join("f.bin").exists(),
        "a junction's target is never deleted"
    );
    assert!(
        link.symlink_metadata().is_err(),
        "the old junction itself is removed"
    );
    assert!(!stage.exists(), "the dead stage folder is removed");
}
