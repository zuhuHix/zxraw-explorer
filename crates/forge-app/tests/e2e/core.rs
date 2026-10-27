//! Core file-manager scenarios: folders, rename/undo, copy/move, delete, batch rename,
//! session restore, and the script hook's own generic verbs.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::*;

#[test]
fn new_folder_is_created_from_the_prompt() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    fs::create_dir_all(&dir).unwrap();
    let script = format!(
        "navigate {}\ncmd NewFolder\ntype Fresh folder\naccept\nwait-exists {}\nexit\n",
        shown(&dir),
        shown(&dir.join("Fresh folder"))
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert!(dir.join("Fresh folder").is_dir());
    assert!(
        !dir.join("New folder").exists(),
        "the default name must not be left behind"
    );
}

#[test]
fn rename_then_undo_restores_the_original_name() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    let bytes = noise(4096, 1);
    write_file(&dir.join("original.txt"), &bytes);
    let script = format!(
        "navigate {dir}\n\
         select original.txt\n\
         cmd Rename\n\
         type renamed.txt\n\
         accept\n\
         wait-exists {renamed}\n\
         assert-missing {original}\n\
         # The rename ran on a worker; its undo record and selection arrive as events.\n\
         wait-jobs\n\
         wait-frames 3\n\
         wait-entry renamed.txt\n\
         wait-frames 3\n\
         assert-selection renamed.txt\n\
         cmd Undo\n\
         wait-exists {original}\n\
         assert-missing {renamed}\n\
         wait-jobs\n\
         wait-frames 3\n\
         assert-toast Undid rename of 1 item(s)\n\
         assert-no-error-toast\n\
         exit\n",
        dir = shown(&dir),
        renamed = shown(&dir.join("renamed.txt")),
        original = shown(&dir.join("original.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dir.join("original.txt")).unwrap(), bytes);
    assert!(!dir.join("renamed.txt").exists());
}

#[test]
fn an_undo_blocked_by_a_new_file_keeps_its_record_for_a_retry() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    write_file(&dir.join("a.txt"), b"a");
    // After renaming a.txt to b.txt, a new a.txt blocks the undo. Undo must change nothing
    // and keep its record; once the new file is gone, Undo works.
    let script = format!(
        "navigate {dir}\n\
         select a.txt\n\
         cmd Rename\n\
         type b.txt\n\
         accept\n\
         wait-exists {b}\n\
         wait-jobs\n\
         wait-frames 3\n\
         cmd NewFile\n\
         type a.txt\n\
         accept\n\
         wait-exists {a}\n\
         wait-jobs\n\
         wait-frames 3\n\
         cmd Undo\n\
         wait-jobs\n\
         wait-frames 3\n\
         assert-toast nothing was changed, so Undo can be tried again\n\
         assert-exists {b}\n\
         wait-entry a.txt\n\
         select a.txt\n\
         cmd DeletePermanently\n\
         accept\n\
         wait-jobs 20\n\
         assert-missing {a}\n\
         wait-frames 3\n\
         cmd Undo\n\
         wait-exists {a}\n\
         wait-jobs\n\
         wait-frames 3\n\
         assert-toast Undid rename of 1 item(s)\n\
         exit\n",
        dir = shown(&dir),
        a = shown(&dir.join("a.txt")),
        b = shown(&dir.join("b.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"a");
    assert!(!dir.join("b.txt").exists());
}

#[test]
fn renaming_onto_an_existing_name_reopens_the_prompt_with_the_error() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    write_file(&dir.join("a.txt"), b"a");
    write_file(&dir.join("b.txt"), b"b");
    let script = format!(
        "navigate {dir}\n\
         select a.txt\n\
         cmd Rename\n\
         type b.txt\n\
         accept\n\
         wait-jobs\n\
         wait-frames 3\n\
         ui browser assert-prompt-error a file with that name already exists\n\
         cancel\n\
         ui browser assert-no-dialog\n\
         assert-exists {a}\n\
         cmd Undo\n\
         assert-toast Nothing to undo\n\
         exit\n",
        dir = shown(&dir),
        a = shown(&dir.join("a.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"a");
    assert_eq!(fs::read(dir.join("b.txt")).unwrap(), b"b");
}

#[test]
fn new_folder_and_file_suggest_a_free_name_from_the_listing() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    fs::create_dir_all(dir.join("New folder")).unwrap();
    write_file(&dir.join("New file.txt"), b"keep");
    let script = format!(
        "navigate {dir}\n\
         cmd NewFolder\naccept\nwait-exists {folder2}\n\
         cmd NewFile\naccept\nwait-exists {file2}\n\
         wait-jobs\nwait-frames 3\nassert-no-error-toast\nexit\n",
        dir = shown(&dir),
        folder2 = shown(&dir.join("New folder (2)")),
        file2 = shown(&dir.join("New file (2).txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert!(dir.join("New folder (2)").is_dir());
    assert_eq!(fs::read(dir.join("New file.txt")).unwrap(), b"keep");
}

#[test]
fn rename_undo_and_create_on_an_offline_share_never_wait_on_the_ui_thread() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    // 192.0.2.0/24 is TEST-NET-1 (RFC 5737): a file call on it hangs about 20 s until the
    // network gives up, so each `offline` step fails if its UI call took longer than 250 ms.
    // Windows remembers an unreachable host for a while and then fails fast, so each run
    // picks a host the other tests (which use .1) do not.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let share = format!(r"\\192.0.2.{}\x", 2 + nanos % 250);
    let script = format!(
        "dismiss-notices\nnavigate {sandbox}\n\
         ui browser offline rename {share}\\a.txt | b.txt\n\
         ui sidebar responsive 30 | 1000\n\
         ui browser offline undo-rename {share}\\a.txt | b.txt\n\
         ui sidebar responsive 30 | 1000\n\
         ui sidebar go {share}\n\
         ui browser offline new-folder Fresh\n\
         ui browser offline new-file note.txt\n\
         ui sidebar responsive 30 | 1000\n\
         navigate {sandbox}\nassert-path {sandbox}\nexit\n",
        sandbox = shown(sandbox.path()),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn undo_waits_for_a_stuck_rename_while_local_renames_keep_going() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    write_file(&dir.join("local.txt"), b"local");
    // The share's lane is held as a rename stuck on an unresponsive NAS would hold it (the
    // path is only used to pick the lane, never opened). Undo must wait for it, since its
    // record is not on the stack yet; a local rename runs on its own lane and finishes.
    let script = format!(
        "navigate {dir}\n\
         ui browser hold-name-lane \\\\192.0.2.250\\x | 8000\n\
         cmd Undo\n\
         assert-toast Waiting for the rename to finish\n\
         wait-entry local.txt\n\
         select local.txt\n\
         cmd Rename\n\
         type moved.txt\n\
         accept\n\
         wait-exists {moved} | 5\n\
         exit\n",
        dir = shown(&dir),
        moved = shown(&dir.join("moved.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dir.join("moved.txt")).unwrap(), b"local");
}

#[test]
fn copy_to_other_pane_duplicates_files_byte_for_byte() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("src");
    let dst = sandbox.path().join("dst");
    let text = b"hello, copy\n".repeat(1000);
    let binary = noise(300 * 1024, 7);
    write_file(&src.join("a.txt"), &text);
    write_file(&src.join("b.bin"), &binary);
    fs::create_dir_all(&dst).unwrap();
    let script = dual_pane_script(
        &src,
        &dst,
        &["select a.txt, b.bin", "cmd CopyToOtherPane", "wait-jobs 20"],
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dst.join("a.txt")).unwrap(), text);
    assert_eq!(fs::read(dst.join("b.bin")).unwrap(), binary);
    assert_eq!(
        fs::read(src.join("a.txt")).unwrap(),
        text,
        "copy must keep the source"
    );
    assert_eq!(
        fs::read(src.join("b.bin")).unwrap(),
        binary,
        "copy must keep the source"
    );
}

#[test]
fn a_copy_started_while_a_dialog_is_open_is_refused_not_lost() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("src");
    let dst = sandbox.path().join("dst");
    write_file(&src.join("a.txt"), b"keep me");
    fs::create_dir_all(&dst).unwrap();
    let script = dual_pane_script(
        &src,
        &dst,
        &[
            "select a.txt",
            // Any open dialog holds the single dialog slot.
            "cmd DeletePermanently",
            "cmd CopyToOtherPane",
            "assert-toast The copy did not start: close the open dialog first",
            // A move within one volume needs no cloud check, and is refused all the same.
            "cmd MoveToOtherPane",
            "assert-toast The move did not start: close the open dialog first",
            "cancel",
            "wait-jobs 20",
        ],
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert!(!dst.join("a.txt").exists(), "the refused copy and move never ran");
    assert_eq!(fs::read(src.join("a.txt")).unwrap(), b"keep me");
}

#[test]
fn copy_with_conflict_keep_both_writes_a_numbered_copy() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("src");
    let dst = sandbox.path().join("dst");
    write_file(&src.join("name.txt"), b"incoming");
    write_file(&dst.join("name.txt"), b"existing");
    let script = dual_pane_script(
        &src,
        &dst,
        &[
            "select name.txt",
            "cmd CopyToOtherPane",
            "conflict keep-both",
            "wait-jobs 20",
        ],
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dst.join("name.txt")).unwrap(), b"existing");
    assert_eq!(fs::read(dst.join("name (2).txt")).unwrap(), b"incoming");
}

#[test]
fn queued_copies_wait_and_a_cancelled_one_never_runs() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("src");
    let dst = sandbox.path().join("dst");
    let a = noise(64 * 1024, 11);
    let b = noise(64 * 1024, 12);
    let c = noise(96 * 1024, 13);
    let c2 = noise(8 * 1024, 14);
    write_file(&src.join("a.txt"), &a);
    write_file(&src.join("b.txt"), &b);
    write_file(&src.join("c.txt"), &c);
    write_file(&src.join("c2.txt"), &c2);
    fs::create_dir_all(&dst).unwrap();
    // Titles read "Copying N item(s) to dst", so the queued copies get distinct item counts
    // (a: 1, b: 1, c: 2) that `assert-queue-order` can tell apart.
    let script = dual_pane_script(
        &src,
        &dst,
        &[
            "ui system pause-queue",
            "select a.txt",
            "cmd CopyToOtherPane",
            "select b.txt",
            "cmd CopyToOtherPane",
            "select c.txt, c2.txt",
            "cmd CopyToOtherPane",
            "ui system assert-queued 3",
            "sleep 300",
            &format!("assert-missing {}", shown(&dst.join("a.txt"))),
            "ui system assert-queue-order 1 item(s), 1 item(s), 2 item(s)",
            "ui system queue-cancel 2",
            "ui system assert-queued 2",
            "ui system assert-queue-order 1 item(s), 2 item(s)",
            "ui system queue-up 2",
            "ui system assert-queue-order 2 item(s), 1 item(s)",
            "ui system resume-queue",
            "wait-jobs 20",
        ],
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dst.join("a.txt")).unwrap(), a);
    assert_eq!(fs::read(dst.join("c.txt")).unwrap(), c);
    assert_eq!(fs::read(dst.join("c2.txt")).unwrap(), c2);
    assert!(
        !dst.join("b.txt").exists(),
        "a cancelled queued copy must never run"
    );
}

#[test]
fn queued_copy_resolves_a_conflict_created_while_it_waited() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("src");
    let dst = sandbox.path().join("dst");
    write_file(&src.join("name.txt"), b"incoming");
    fs::create_dir_all(&dst).unwrap();
    let script = dual_pane_script(
        &src,
        &dst,
        &[
            "ui system pause-queue",
            "select name.txt",
            "cmd CopyToOtherPane",
            &format!("ui tools external create {}", shown(&dst.join("name.txt"))),
            "ui system resume-queue",
            "conflict keep-both",
            "wait-jobs 20",
        ],
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(
        fs::read(dst.join("name.txt")).unwrap(),
        vec![b'x'; 4096],
        "the file created while the copy waited must be kept"
    );
    assert_eq!(fs::read(dst.join("name (2).txt")).unwrap(), b"incoming");
}

#[test]
fn queued_copy_skips_a_source_deleted_while_waiting() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("src");
    let dst = sandbox.path().join("dst");
    write_file(&src.join("x.txt"), b"x stays");
    write_file(&src.join("y.txt"), b"y goes away");
    fs::create_dir_all(&dst).unwrap();
    let script = dual_pane_script(
        &src,
        &dst,
        &[
            "ui system pause-queue",
            "select x.txt, y.txt",
            "cmd CopyToOtherPane",
            &format!("ui tools external delete {}", shown(&src.join("y.txt"))),
            "ui system resume-queue",
            "wait-jobs 20",
        ],
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dst.join("x.txt")).unwrap(), b"x stays");
    assert!(
        !dst.join("y.txt").exists(),
        "a source that vanished while queued must be skipped"
    );
}

#[test]
fn queued_move_leaves_a_source_replaced_while_waiting() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("src");
    let dst = sandbox.path().join("dst");
    write_file(&src.join("x.txt"), b"x is moved");
    write_file(&src.join("y.txt"), b"the y the user picked");
    fs::create_dir_all(&dst).unwrap();
    let y = shown(&src.join("y.txt"));
    let script = dual_pane_script(
        &src,
        &dst,
        &[
            "ui system pause-queue",
            "select x.txt, y.txt",
            "cmd MoveToOtherPane",
            // Another program replaces y.txt with a different file of the same name.
            &format!("ui tools external delete {y}"),
            &format!("ui tools external create {y}"),
            "ui system resume-queue",
            "wait-jobs 20",
        ],
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dst.join("x.txt")).unwrap(), b"x is moved");
    assert!(!src.join("x.txt").exists());
    assert!(
        !dst.join("y.txt").exists(),
        "a source replaced while queued must not be moved"
    );
    assert_eq!(
        fs::metadata(src.join("y.txt")).unwrap().len(),
        4096,
        "the replacement stays where it is"
    );
}

#[test]
fn closing_with_queued_copies_warns() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("src");
    let dst = sandbox.path().join("dst");
    let bytes = noise(16 * 1024, 21);
    write_file(&src.join("f.txt"), &bytes);
    fs::create_dir_all(&dst).unwrap();
    let script = dual_pane_script(
        &src,
        &dst,
        &[
            "ui system pause-queue",
            "select f.txt",
            "cmd CopyToOtherPane",
            "ui system request-close",
            "ui system assert-close-warning",
            "ui system resume-queue",
            "wait-jobs 20",
        ],
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dst.join("f.txt")).unwrap(), bytes);
}

#[test]
fn move_to_other_pane_removes_the_source() {
    let sandbox = tempfile::tempdir().unwrap();
    let src = sandbox.path().join("src");
    let dst = sandbox.path().join("dst");
    let bytes = noise(64 * 1024, 3);
    write_file(&src.join("move.txt"), &bytes);
    fs::create_dir_all(&dst).unwrap();
    let script = dual_pane_script(
        &src,
        &dst,
        &["select move.txt", "cmd MoveToOtherPane", "wait-jobs 20"],
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert!(
        !src.join("move.txt").exists(),
        "the source must be gone after a move"
    );
    assert_eq!(fs::read(dst.join("move.txt")).unwrap(), bytes);
}

#[test]
fn permanent_delete_removes_the_file_after_confirmation() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    write_file(&dir.join("doomed.txt"), b"bye");
    let script = format!(
        "navigate {}\nselect doomed.txt\ncmd DeletePermanently\naccept\nwait-jobs 20\nassert-missing {}\nexit\n",
        shown(&dir),
        shown(&dir.join("doomed.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert!(!dir.join("doomed.txt").exists());
}

#[test]
fn permanent_delete_cancelled_keeps_the_file() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    write_file(&dir.join("keep.txt"), b"stay");
    let script = format!(
        "navigate {}\nselect keep.txt\ncmd DeletePermanently\ncancel\nwait-frames 10\nassert-exists {}\nexit\n",
        shown(&dir),
        shown(&dir.join("keep.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dir.join("keep.txt")).unwrap(), b"stay");
}

#[test]
fn batch_rename_with_default_template_numbers_the_selection() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    for name in ["a.txt", "b.txt", "c.txt"] {
        write_file(&dir.join(name), name.as_bytes());
    }
    let script = format!(
        "navigate {}\nselect-all\ncmd BatchRename\naccept\nexit\n",
        shown(&dir)
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dir.join("a_001.txt")).unwrap(), b"a.txt");
    assert_eq!(fs::read(dir.join("b_002.txt")).unwrap(), b"b.txt");
    assert_eq!(fs::read(dir.join("c_003.txt")).unwrap(), b"c.txt");
    for name in ["a.txt", "b.txt", "c.txt"] {
        assert!(!dir.join(name).exists(), "{name} should have been renamed");
    }
}

#[test]
fn corrupt_jpeg_in_grid_and_preview_does_not_crash() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    write_file(&dir.join("broken.jpg"), &noise(8192, 42));
    write_file(&dir.join("notes.txt"), b"plain text next to it");
    let shot = sandbox.path().join("shot.png");
    let profile = sandbox.path().join("profile");
    let script = format!(
        "navigate {}\ncmd ViewGrid\ncmd TogglePreview\nselect broken.jpg\nwait-frames 120\nscreenshot {}\nexit\n",
        shown(&dir),
        shown(&shot),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert!(
        fs::metadata(&shot).map(|m| m.len() > 0).unwrap_or(false),
        "no screenshot was saved"
    );
    let crashes = profile.join("data").join("crashes");
    // `crash-*.txt` files are reports; `.last-startup-nanos` is the startup marker.
    let reports: Vec<PathBuf> = fs::read_dir(&crashes)
        .map(|d| {
            d.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("crash-"))
                })
                .collect()
        })
        .unwrap_or_default();
    let details: String = reports
        .iter()
        .map(|r| {
            let body = fs::read_to_string(r).unwrap_or_default();
            format!(
                "{}:\n{}\n",
                r.display(),
                body.chars().take(2000).collect::<String>()
            )
        })
        .collect();
    assert!(
        reports.is_empty(),
        "the app wrote {} crash report(s):\n{details}",
        reports.len()
    );
}

#[test]
fn tabs_are_restored_on_the_next_start() {
    let sandbox = tempfile::tempdir().unwrap();
    let first = sandbox.path().join("first");
    let second = sandbox.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    let profile = sandbox.path().join("profile");

    let outcome = run_app(
        &sandbox.path().join("run1"),
        &profile,
        &format!(
            "navigate {}\ncmd NewTab\nnavigate {}\nexit\n",
            shown(&first),
            shown(&second)
        ),
    );
    expect_ok(&outcome);

    let outcome = run_app(
        &sandbox.path().join("run2"),
        &profile,
        &format!("assert-tabs 2\nassert-path {}\nexit\n", shown(&second)),
    );
    expect_ok(&outcome);
}

#[test]
fn delete_key_recycles_into_the_sandbox_not_the_real_recycle_bin() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    write_file(&dir.join("binned.txt"), b"recycle me");
    write_file(&dir.join("kept.txt"), b"stay");
    let profile = sandbox.path().join("profile");
    let script = format!(
        // Recycling asks for confirmation only when `confirm_recycle` is on (off by default).
        "navigate {dir}\nselect binned.txt\nkey Delete\nwait-jobs 20\n\
         wait-missing {binned}\nwait-no-entry binned.txt | 10\nassert-entry kept.txt\n\
         assert-toast Recycle Bin\nassert-no-error-toast\nexit\n",
        dir = shown(&dir),
        binned = shown(&dir.join("binned.txt")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    let recycled = recycle_sandbox(&profile).join("0").join("binned.txt");
    assert_eq!(
        fs::read(&recycled).unwrap(),
        b"recycle me",
        "the item must be in the sandbox"
    );
    assert_eq!(fs::read(dir.join("kept.txt")).unwrap(), b"stay");
}

#[test]
fn an_item_the_recycle_bin_refuses_is_deleted_only_after_asking() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    // The recycle sandbox refuses names starting with "not-recyclable", as the real Recycle
    // Bin refuses items too big for it.
    write_file(&dir.join("not-recyclable-big.bin"), b"too big");
    write_file(&dir.join("binned.txt"), b"recycle me");
    let profile = sandbox.path().join("profile");
    let script = format!(
        "navigate {dir}
select not-recyclable-big.bin, binned.txt
key Delete
wait-jobs 20
         wait-missing {binned}
assert-exists {big}
         accept
wait-jobs 20
wait-missing {big}
wait-no-entry not-recyclable-big.bin | 10
         assert-toast Permanently deleted 1 item
assert-no-error-toast
exit
",
        dir = shown(&dir),
        binned = shown(&dir.join("binned.txt")),
        big = shown(&dir.join("not-recyclable-big.bin")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    let bin = recycle_sandbox(&profile);
    assert_eq!(
        fs::read(bin.join("0").join("binned.txt")).unwrap(),
        b"recycle me",
        "the recyclable item must be in the sandbox"
    );
    assert!(
        !bin.join("1").exists(),
        "the refused item was deleted permanently, not recycled"
    );
}

#[test]
fn an_item_the_recycle_bin_refuses_stays_when_the_question_is_declined() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    write_file(&dir.join("not-recyclable-big.bin"), b"too big");
    let profile = sandbox.path().join("profile");
    let script = format!(
        "navigate {dir}
select not-recyclable-big.bin
key Delete
wait-jobs 20
         cancel
wait-jobs 5
wait-entry not-recyclable-big.bin | 10
exit
",
        dir = shown(&dir),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(dir.join("not-recyclable-big.bin")).unwrap(), b"too big");
}

#[test]
fn keyboard_shortcut_runs_through_real_input() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    fs::create_dir_all(&dir).unwrap();
    let script = format!(
        "navigate {}\nassert-tabs 1\nkey ctrl+T\nassert-tabs 2\nexit\n",
        shown(&dir)
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
}

#[test]
fn a_failing_step_is_reported_with_its_line() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    fs::create_dir_all(&dir).unwrap();
    let script = format!(
        "navigate {}\nassert-entry not-there.txt\nexit\n",
        shown(&dir)
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_script_error(&outcome, "line 2");
}

#[test]
fn first_run_welcome_shows_once_per_profile() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    fs::create_dir_all(&dir).unwrap();
    let profile = sandbox.path().join("profile");
    let shot = sandbox.path().join("welcome.png");
    let opts = RunOpts {
        env: vec![("ZXRAW_FIRST_RUN".into(), "1".into())],
        ..Default::default()
    };

    let first = run_app_with(
        &sandbox.path().join("run1"),
        &profile,
        &format!(
            "navigate {}\nassert-notice welcome\nwait-frames 10\nscreenshot {}\ndismiss-notices\nassert-notice none\nexit\n",
            shown(&dir),
            shown(&shot)
        ),
        &opts,
    );
    expect_ok(&first);
    assert!(profile.join("config").join("first-run.done").is_file());

    let second = run_app_with(
        &sandbox.path().join("run2"),
        &profile,
        &format!("navigate {}\nassert-notice none\nexit\n", shown(&dir)),
        &opts,
    );
    expect_ok(&second);
    if let Ok(keep) = std::env::var("ZXFR_SHOTS") {
        let _ = fs::copy(&shot, PathBuf::from(keep).join("welcome.png"));
    }
}

#[test]
fn crash_report_from_the_last_session_is_announced_once() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("work");
    fs::create_dir_all(&dir).unwrap();
    let profile = sandbox.path().join("profile");
    write_file(
        &profile.join("data").join("crashes").join("crash-20261011-000000.txt"),
        b"ZXraw Explorer crash report\npanic: synthetic test report\n",
    );
    let shot = sandbox.path().join("crash-notice.png");

    let first = run_app(
        &sandbox.path().join("run1"),
        &profile,
        &format!(
            "navigate {}\nassert-notice crash\nwait-frames 10\nscreenshot {}\nexit\n",
            shown(&dir),
            shown(&shot)
        ),
    );
    expect_ok(&first);

    let second = run_app(
        &sandbox.path().join("run2"),
        &profile,
        &format!("navigate {}\nassert-notice none\nexit\n", shown(&dir)),
    );
    expect_ok(&second);
    if let Ok(keep) = std::env::var("ZXFR_SHOTS") {
        let _ = fs::copy(&shot, PathBuf::from(keep).join("crash-notice.png"));
    }
}

// ---------------------------------------------------------------------------
// Real pointer clicks on rows and tiles. Positions come from where the view drew each
// entry (in points), so these hold at fractional display scaling: at 1.2 the view's
// background widget used to sit above the rows and swallow every click.
// ---------------------------------------------------------------------------

fn folder_with_subfolder(sandbox: &Path) -> (PathBuf, PathBuf) {
    let dir = sandbox.join("work");
    let sub = dir.join("Shoot 2024");
    fs::create_dir_all(&sub).unwrap();
    write_file(&sub.join("inside.txt"), b"x");
    write_file(&dir.join("notes.txt"), b"y");
    (dir, sub)
}

fn click_scenario(view: &str, steps: &str) {
    let sandbox = tempfile::tempdir().unwrap();
    let (dir, sub) = folder_with_subfolder(sandbox.path());
    let script = format!(
        "navigate {dir}\ncmd {view}\nwait-frames 5\n{steps}\nwait-entry inside.txt\nassert-path {sub}\nexit\n",
        dir = shown(&dir),
        sub = shown(&sub),
    );
    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);
    expect_ok(&outcome);
}

#[test]
fn double_clicking_a_folder_row_opens_it() {
    click_scenario("ViewDetails", "double-click-entry Shoot 2024");
}

#[test]
fn double_clicking_a_folder_tile_opens_it() {
    click_scenario("ViewGrid", "double-click-entry Shoot 2024");
}

#[test]
fn clicking_a_folder_row_then_enter_opens_it() {
    click_scenario(
        "ViewDetails",
        "click-entry Shoot 2024\nassert-selected 1\nkey Enter",
    );
}

#[test]
fn clicking_empty_space_below_the_rows_clears_the_selection() {
    let sandbox = tempfile::tempdir().unwrap();
    let (dir, _) = folder_with_subfolder(sandbox.path());
    let script = format!(
        "navigate {dir}\ncmd ViewDetails\nwait-frames 5\nclick-entry notes.txt\nassert-selected 1\n\
         click-empty\nassert-selected 0\nassert-path {dir}\nexit\n",
        dir = shown(&dir),
    );
    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);
    expect_ok(&outcome);
}

#[test]
fn dragging_a_marquee_selects_virtual_rows_and_grid_tiles() {
    let sandbox = tempfile::tempdir().unwrap();
    let details = sandbox.path().join("details");
    let grid = sandbox.path().join("grid");
    fs::create_dir_all(&details).unwrap();
    fs::create_dir_all(&grid).unwrap();
    for number in 1..=8 {
        write_file(&details.join(format!("{number:02}.txt")), b"details");
    }
    for number in 1..=2 {
        write_file(&grid.join(format!("{number:02}.txt")), b"grid");
    }

    let details_shot = shots_dir().join("marquee-details-mid-drag.png");
    let grid_shot = shots_dir().join("marquee-grid-mid-drag.png");
    let script = format!(
        "limit 180\nnavigate {details}\ncmd ViewDetails\nwait-frames 5\n\
         marquee 06.txt 02.txt\nscreenshot {details_shot}\nrelease-pointer\
         \nassert-selection 02.txt, 03.txt, 04.txt, 05.txt, 06.txt, 07.txt, 08.txt\n\
         navigate {grid}\ncmd ViewGrid\nwait-frames 5\n\
         marquee 01.txt 02.txt\nscreenshot {grid_shot}\nrelease-pointer\
         \nassert-selection 01.txt, 02.txt\nexit\n",
        details = shown(&details),
        grid = shown(&grid),
        details_shot = shown(&details_shot),
        grid_shot = shown(&grid_shot),
    );
    let options = RunOpts {
        limit: Some(Duration::from_secs(180)),
        ..RunOpts::default()
    };
    let outcome = run_app_with(
        sandbox.path(),
        &sandbox.path().join("profile"),
        &script,
        &options,
    );

    expect_ok(&outcome);
    assert!(
        details_shot.is_file(),
        "missing screenshot: {}",
        details_shot.display()
    );
    assert!(
        grid_shot.is_file(),
        "missing screenshot: {}",
        grid_shot.display()
    );
}

/// Runs a drag-and-drop script in its own sandbox profile; the caller checks the outcome.
fn drop_script_run(sandbox: &Path, script: &str) -> Outcome {
    let options = RunOpts {
        limit: Some(Duration::from_secs(120)),
        ..RunOpts::default()
    };
    run_app_with(sandbox, &sandbox.join("profile"), script, &options)
}

/// A folder `list` with a `Target` folder, and `outside/a.txt` on the same volume.
fn drop_sandbox(sandbox: &Path) -> (PathBuf, PathBuf) {
    let list = sandbox.join("list");
    let outside = sandbox.join("outside");
    fs::create_dir_all(list.join("Target")).unwrap();
    write_file(&outside.join("a.txt"), b"dropped");
    (list, outside)
}

#[test]
fn external_drop_onto_a_folder_row_moves_the_file() {
    let sandbox = tempfile::tempdir().unwrap();
    let (list, outside) = drop_sandbox(sandbox.path());
    let script = format!(
        "limit 120\nnavigate {list}\ncmd ViewDetails\nwait-entry Target\nwait-frames 5\n\
         drop-files Target | {file}\nwait-exists {moved}\nwait-jobs\n\
         assert-missing {file}\nassert-no-error-toast\nexit\n",
        list = shown(&list),
        file = shown(&outside.join("a.txt")),
        moved = shown(&list.join("Target").join("a.txt")),
    );
    expect_ok(&drop_script_run(sandbox.path(), &script));
}

#[test]
fn external_drop_with_ctrl_copies() {
    let sandbox = tempfile::tempdir().unwrap();
    let (list, outside) = drop_sandbox(sandbox.path());
    let script = format!(
        "limit 120\nnavigate {list}\ncmd ViewDetails\nwait-entry Target\nwait-frames 5\n\
         drop-files Target | {file} | ctrl\nwait-exists {copied}\nwait-jobs\n\
         assert-exists {file}\nassert-no-error-toast\nexit\n",
        list = shown(&list),
        file = shown(&outside.join("a.txt")),
        copied = shown(&list.join("Target").join("a.txt")),
    );
    expect_ok(&drop_script_run(sandbox.path(), &script));
}

#[test]
fn external_drop_on_empty_area_goes_to_the_pane_folder() {
    let sandbox = tempfile::tempdir().unwrap();
    let (list, outside) = drop_sandbox(sandbox.path());
    let script = format!(
        "limit 120\nnavigate {list}\ncmd ViewDetails\nwait-entry Target\nwait-frames 5\n\
         drop-files bg | {file}\nwait-exists {moved}\nwait-jobs\n\
         assert-missing {file}\nassert-no-error-toast\nexit\n",
        list = shown(&list),
        file = shown(&outside.join("a.txt")),
        moved = shown(&list.join("a.txt")),
    );
    expect_ok(&drop_script_run(sandbox.path(), &script));
}

/// Opens `list/Sub` in place (its child `inner.txt` is listed after the expansion).
fn expanded_sub_script(list: &Path) -> String {
    format!(
        "limit 120\nnavigate {list}\ncmd ViewDetails\nwait-entry Sub\nwait-frames 5\n\
         select Sub\nkey ArrowRight\nwait-entry inner.txt\nwait-frames 5\n",
        list = shown(list),
    )
}

#[test]
fn internal_drop_onto_a_child_of_an_expanded_folder_lands_in_that_folder() {
    let sandbox = tempfile::tempdir().unwrap();
    let list = sandbox.path().join("list");
    write_file(&list.join("Sub").join("inner.txt"), b"inner");
    write_file(&list.join("top.txt"), b"top");
    let script = format!(
        "{}drag top.txt Sub/inner.txt\nwait-exists {moved}\nwait-jobs\n\
         assert-missing {top}\nassert-no-error-toast\nexit\n",
        expanded_sub_script(&list),
        moved = shown(&list.join("Sub").join("top.txt")),
        top = shown(&list.join("top.txt")),
    );
    expect_ok(&drop_script_run(sandbox.path(), &script));
}

#[test]
fn dropping_a_folder_onto_its_own_child_does_nothing() {
    let sandbox = tempfile::tempdir().unwrap();
    let list = sandbox.path().join("list");
    write_file(&list.join("Sub").join("inner.txt"), b"inner");
    let script = format!(
        "{}drag Sub Sub/inner.txt\nwait-frames 30\nwait-jobs\n\
         assert-exists {inner}\nassert-exists {folder}\nassert-no-error-toast\nexit\n",
        expanded_sub_script(&list),
        inner = shown(&list.join("Sub").join("inner.txt")),
        folder = shown(&list.join("Sub")),
    );
    expect_ok(&drop_script_run(sandbox.path(), &script));
}

#[path = "../../../forge-win/src/clipboard_test_support.rs"]
mod clipboard_test_support;

use clipboard_test_support::{ClipboardLock, Snapshot};

/// Scenarios that use the real Windows clipboard run one at a time, also against other
/// test processes (forge-win's clipboard unit tests, e2e runs in other worktrees): see
/// [`ClipboardLock`].
fn clipboard_lock() -> ClipboardLock {
    ClipboardLock::acquire()
}

/// [`clipboard_lock`] plus the user's clipboard, saved now and put back when the guard drops.
///
/// Fails closed: `None` (the caller skips the test, before writing to the clipboard) when
/// the clipboard holds something that cannot be put back exactly (see [`Snapshot::save`]).
///
/// A watcher, started from the snapshot's own clipboard sequence, checks every change; the
/// clipboard is put back only if each one was the test's own (empty, or files inside
/// `sandbox` only) and, checked in the same clipboard open as the restore, it still is. So a
/// copy the user makes during the test is kept, and nothing is restored. Remaining window: a
/// user copy that lands between two of the test's own clipboard writes less than one
/// watcher poll (50 ms) apart, or a user emptying the clipboard, looks like the test's own.
fn clipboard_guard(sandbox: &Path) -> Option<ClipboardGuard> {
    let lock = clipboard_lock();
    let saved = match Snapshot::save() {
        Ok(saved) => saved,
        Err(why) => {
            eprintln!(
                "skipped: the clipboard holds {why}, which this test can't restore; \
                 clear it or copy some text and rerun"
            );
            return None;
        }
    };
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let stop = stop.clone();
        let sandbox = sandbox.to_path_buf();
        let start = saved.sequence;
        std::thread::spawn(move || watch_clipboard(&sandbox, start, &stop))
    };
    Some(ClipboardGuard {
        saved,
        sandbox: sandbox.to_path_buf(),
        stop,
        watcher: Some(watcher),
        _lock: lock,
    })
}

/// Checks every clipboard change after sequence `start` until `stop`. Returns whether one
/// was not the test's, and the last sequence seen that was.
fn watch_clipboard(
    sandbox: &Path,
    start: u32,
    stop: &std::sync::atomic::AtomicBool,
) -> (bool, u32) {
    let mut seen = start;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
        if clipboard_test_support::sequence() == seen {
            continue;
        }
        match clipboard_test_support::check_files_in(sandbox) {
            Some((sequence, true)) => seen = sequence,
            Some((_, false)) => return (true, seen),
            None => {} // busy: look again next time
        }
    }
    (false, seen)
}

struct ClipboardGuard {
    saved: Snapshot,
    sandbox: PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    watcher: Option<std::thread::JoinHandle<(bool, u32)>>,
    _lock: ClipboardLock,
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let (foreign, seen) = self
            .watcher
            .take()
            .map_or((true, 0), |w| w.join().unwrap_or((true, 0)));
        if foreign {
            eprintln!("warning: something other than the test changed the clipboard; not restored");
            return;
        }
        let unchanged = std::cell::Cell::new(false);
        let restored = self.saved.restore_if(|| {
            let now = clipboard_test_support::sequence();
            unchanged.set(now == self.saved.sequence);
            !unchanged.get()
                && (now == seen || clipboard_test_support::holds_only_files_in(&self.sandbox))
        });
        match restored {
            Ok(false) if !unchanged.get() => eprintln!(
                "warning: something other than the test changed the clipboard; not restored"
            ),
            Err(e) => eprintln!("warning: could not restore the clipboard: {e}"),
            _ => {}
        }
    }
}

/// The current clipboard text (empty if it has none), or `None` if it cannot be saved.
fn clipboard_text() -> Option<String> {
    Snapshot::save().ok().map(|s| s.text().unwrap_or_default())
}

/// The guard puts the user's clipboard back after a test that left its own cut there.
#[test]
fn clipboard_guard_restores_the_clipboard_after_a_test_cut() {
    let sandbox = tempfile::tempdir().unwrap();
    let cut = sandbox.path().join("cut.txt");
    write_file(&cut, b"cut");
    // Held across the whole test (the guard takes it again on this thread). The user's
    // clipboard is saved here and put back at the end.
    let _lock = clipboard_lock();
    let Ok(user) = Snapshot::save() else {
        eprintln!("skipped: the clipboard holds something this test can't restore");
        return;
    };
    let probe = "ZXraw clipboard guard probe";
    Snapshot::of_text(probe).restore_if(|| true).unwrap();
    let guard = clipboard_guard(sandbox.path()).expect("plain text can be saved");
    forge_win::set_files(std::slice::from_ref(&cut), forge_win::ClipboardEffect::Move).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    drop(guard);
    let after = clipboard_test_support::sequence();
    let text = clipboard_text();
    let _ = user.restore_if(|| clipboard_test_support::sequence() == after);
    assert_eq!(text.as_deref(), Some(probe));
}

/// It leaves alone a clipboard that something else changed during the test.
#[test]
fn clipboard_guard_keeps_a_newer_clipboard() {
    let sandbox = tempfile::tempdir().unwrap();
    let _lock = clipboard_lock();
    let Ok(user) = Snapshot::save() else {
        eprintln!("skipped: the clipboard holds something this test can't restore");
        return;
    };
    Snapshot::of_text("old").restore_if(|| true).unwrap();
    let guard = clipboard_guard(sandbox.path()).expect("plain text can be saved");
    Snapshot::of_text("newer").restore_if(|| true).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    drop(guard);
    let after = clipboard_test_support::sequence();
    let text = clipboard_text();
    let _ = user.restore_if(|| clipboard_test_support::sequence() == after);
    assert_eq!(text.as_deref(), Some("newer"));
}

/// A clipboard holding files is never overwritten: the guard refuses to start.
#[test]
fn clipboard_guard_refuses_a_clipboard_holding_files() {
    let sandbox = tempfile::tempdir().unwrap();
    let file = sandbox.path().join("copied.txt");
    write_file(&file, b"copied");
    let _lock = clipboard_lock();
    let Ok(user) = Snapshot::save() else {
        eprintln!("skipped: the clipboard holds something this test can't restore");
        return;
    };
    forge_win::set_files(
        std::slice::from_ref(&file),
        forge_win::ClipboardEffect::Copy,
    )
    .unwrap();
    let after = clipboard_test_support::sequence();
    let guard = clipboard_guard(sandbox.path());
    let unchanged = clipboard_test_support::sequence() == after;
    let _ = user.restore_if(|| clipboard_test_support::sequence() == after);
    assert!(guard.is_none(), "the guard took a clipboard holding files");
    assert!(unchanged, "the guard wrote to the clipboard");
}
/// Two folders with a file each under `<sandbox>/Desktop`, and an empty `<sandbox>/work`.
fn clipboard_fixture(sandbox: &Path) -> (PathBuf, PathBuf, [PathBuf; 2]) {
    let desktop = sandbox.join("Desktop");
    let work = sandbox.join("work");
    write_file(&desktop.join("Shoot A").join("a.txt"), b"folder a");
    write_file(
        &desktop.join("Shoot B").join("nested").join("b.txt"),
        b"folder b",
    );
    fs::create_dir_all(&work).unwrap();
    let folders = [desktop.join("Shoot A"), desktop.join("Shoot B")];
    (desktop, work, folders)
}

fn assert_folders_moved(work: &Path, folders: &[PathBuf; 2]) {
    for folder in folders {
        assert!(!folder.exists(), "{} was not moved away", folder.display());
    }
    assert_eq!(
        fs::read(work.join("Shoot A").join("a.txt")).unwrap(),
        b"folder a"
    );
    assert_eq!(
        fs::read(work.join("Shoot B").join("nested").join("b.txt")).unwrap(),
        b"folder b"
    );
}

/// The reported bug: folders cut in Explorer (`CF_HDROP` + "Preferred DropEffect" = move, and
/// no text) and pasted with a real Ctrl+V on the same volume. egui-winit drops Ctrl+V when
/// the clipboard has no text, so nothing happened at all.
#[test]
fn explorer_cut_of_two_folders_pastes_with_ctrl_v() {
    let sandbox = tempfile::tempdir().unwrap();
    let Some(_clipboard) = clipboard_guard(sandbox.path()) else {
        return;
    };
    let (_, work, folders) = clipboard_fixture(sandbox.path());
    let script = format!(
        "navigate {work}\n\
         click-empty\n\
         ui system clipboard-files cut {a}, {b}\n\
         key ctrl+V\n\
         wait-exists {moved}\n\
         wait-jobs 20\n\
         assert-no-error-toast\n\
         exit\n",
        work = shown(&work),
        a = shown(&folders[0]),
        b = shown(&folders[1]),
        moved = shown(&work.join("Shoot B").join("nested").join("b.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_folders_moved(&work, &folders);
}

/// The same with Shift+Insert, before anything in the window was clicked, and a copy rather
/// than a cut: the sources stay.
#[test]
fn explorer_copy_pastes_with_shift_insert_without_a_click() {
    let sandbox = tempfile::tempdir().unwrap();
    let Some(_clipboard) = clipboard_guard(sandbox.path()) else {
        return;
    };
    let (_, work, folders) = clipboard_fixture(sandbox.path());
    let script = format!(
        "navigate {work}\n\
         ui system clipboard-files copy {a}, {b}\n\
         key shift+Insert\n\
         wait-exists {copied}\n\
         wait-jobs 20\n\
         assert-no-error-toast\n\
         exit\n",
        work = shown(&work),
        a = shown(&folders[0]),
        b = shown(&folders[1]),
        copied = shown(&work.join("Shoot B").join("nested").join("b.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert!(
        folders.iter().all(|f| f.is_dir()),
        "a copy keeps the sources"
    );
    assert_eq!(
        fs::read(work.join("Shoot A").join("a.txt")).unwrap(),
        b"folder a"
    );
    let _ = forge_win::clear_if_owned_files(&folders, None);
}

/// Cut and paste inside ZXraw with the real shortcuts: a file and a folder.
#[test]
fn in_app_cut_pastes_with_ctrl_v() {
    let sandbox = tempfile::tempdir().unwrap();
    let Some(_clipboard) = clipboard_guard(sandbox.path()) else {
        return;
    };
    let (desktop, work, folders) = clipboard_fixture(sandbox.path());
    write_file(&desktop.join("loose.txt"), b"loose");
    let script = format!(
        "navigate {desktop}\n\
         select Shoot A, loose.txt\n\
         key ctrl+X\n\
         navigate {work}\n\
         click-empty\n\
         key ctrl+V\n\
         wait-exists {moved}\n\
         wait-jobs 20\n\
         assert-no-error-toast\n\
         exit\n",
        desktop = shown(&desktop),
        work = shown(&work),
        moved = shown(&work.join("loose.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert!(!folders[0].exists() && !desktop.join("loose.txt").exists());
    assert_eq!(
        fs::read(work.join("Shoot A").join("a.txt")).unwrap(),
        b"folder a"
    );
    assert_eq!(fs::read(work.join("loose.txt")).unwrap(), b"loose");
    assert!(folders[1].is_dir(), "an item that was not cut stays");
}

/// The reported bug: a cut made in ZXraw and pasted in Explorer (here its real "paste" verb,
/// as on the Desktop) moved the folder, but ZXraw kept showing "1 cut" and the stale cut
/// stayed on the Windows clipboard. Explorer drops its own cut after such a paste.
#[test]
fn a_cut_pasted_in_explorer_is_used_up_in_zxraw() {
    let sandbox = tempfile::tempdir().unwrap();
    let Some(_clipboard) = clipboard_guard(sandbox.path()) else {
        return;
    };
    let (desktop, work, folders) = clipboard_fixture(sandbox.path());
    // Explorer's paste verb runs in another process, so wait until the cut is on the
    // clipboard, then paste it into `work` the way Explorer does.
    let explorer = {
        let cut = folders[0].clone();
        let work = work.clone();
        std::thread::spawn(move || -> bool {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            loop {
                if let Ok(Some((paths, forge_win::ClipboardEffect::Move))) =
                    forge_win::get_files()
                    && paths == vec![cut.clone()]
                {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            let verb = format!(
                "$f=(New-Object -ComObject Shell.Application).NameSpace('{}'); \
                 $f.Self.InvokeVerb('paste'); Start-Sleep 5",
                work.display()
            );
            std::process::Command::new("powershell.exe")
                .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-STA", "-Command"])
                .arg(verb)
                .status()
                .is_ok()
        })
    };
    let script = format!(
        "navigate {desktop}\n\
         select Shoot A\n\
         key ctrl+X\n\
         wait-missing {moved_away}\n\
         wait-exists {arrived}\n\
         ui system wait-no-cut\n\
         wait-no-entry Shoot A\n\
         assert-no-error-toast\n\
         exit\n",
        desktop = shown(&desktop),
        moved_away = shown(&folders[0]),
        arrived = shown(&work.join("Shoot A").join("a.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    let saw_cut = explorer.join().unwrap();
    assert!(saw_cut, "the cut never reached the Windows clipboard");
    assert!(!folders[0].exists(), "Shoot A was not moved away");
    assert_eq!(
        fs::read(work.join("Shoot A").join("a.txt")).unwrap(),
        b"folder a"
    );
    assert!(folders[1].is_dir(), "an item that was not cut stays");
    assert_ne!(
        forge_win::get_files()
            .ok()
            .flatten()
            .map(|(paths, _)| paths),
        Some(vec![folders[0].clone()]),
        "the stale cut is still on the clipboard"
    );
}

/// Waits (up to 60 s) until the Windows clipboard holds a cut of exactly `paths`.
fn wait_for_move_cut(paths: Vec<PathBuf>) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(Some((found, forge_win::ClipboardEffect::Move))) = forge_win::get_files()
            && found == paths
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Explorer moves one of two cut folders away: ZXraw drops it from the cut and the clipboard
/// keeps only the item that is still there.
#[test]
fn an_item_moved_away_leaves_the_rest_of_the_cut() {
    let sandbox = tempfile::tempdir().unwrap();
    let Some(_clipboard) = clipboard_guard(sandbox.path()) else {
        return;
    };
    let (desktop, work, folders) = clipboard_fixture(sandbox.path());
    let mover = {
        let folders = folders.clone();
        let work = work.clone();
        std::thread::spawn(move || -> bool {
            if !wait_for_move_cut(folders.to_vec()) {
                return false;
            }
            fs::rename(&folders[0], work.join("Shoot A")).is_ok()
        })
    };
    let script = format!(
        "navigate {desktop}\n\
         select Shoot A, Shoot B\n\
         key ctrl+X\n\
         ui system wait-cut 1\n\
         wait-no-entry Shoot A\n\
         assert-no-error-toast\n\
         exit\n",
        desktop = shown(&desktop),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert!(
        mover.join().unwrap(),
        "the cut never reached the clipboard or Shoot A did not move"
    );
    assert_eq!(
        forge_win::get_files().unwrap(),
        Some((vec![folders[1].clone()], forge_win::ClipboardEffect::Move)),
        "the clipboard should keep only the item that is still there"
    );
    let _ = forge_win::clear_if_owned_files(&[folders[1].clone()], None);
}

/// Explorer replaces a cut folder with a different one of the same name (a new file ID). The
/// cut must drop it rather than move the replacement.
#[test]
fn a_replaced_cut_item_is_never_moved() {
    let sandbox = tempfile::tempdir().unwrap();
    let Some(_clipboard) = clipboard_guard(sandbox.path()) else {
        return;
    };
    let (desktop, work, folders) = clipboard_fixture(sandbox.path());
    let replacer = {
        let folders = folders.clone();
        let desktop = desktop.clone();
        std::thread::spawn(move || -> bool {
            if !wait_for_move_cut(vec![folders[0].clone()]) {
                return false;
            }
            // Give the cut watcher time to record the original folder first.
            std::thread::sleep(Duration::from_secs(2));
            if fs::rename(&folders[0], desktop.join("Shoot A old")).is_err() {
                return false;
            }
            write_file(&folders[0].join("new.txt"), b"replacement");
            true
        })
    };
    let script = format!(
        "navigate {desktop}\n\
         select Shoot A\n\
         key ctrl+X\n\
         ui system wait-cut 0\n\
         navigate {work}\n\
         click-empty\n\
         key ctrl+V\n\
         sleep 1500\n\
         wait-jobs 20\n\
         exit\n",
        desktop = shown(&desktop),
        work = shown(&work),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert!(replacer.join().unwrap(), "the replacement was not made");
    assert_eq!(
        fs::read(folders[0].join("new.txt")).unwrap(),
        b"replacement",
        "the replacement folder was moved"
    );
    assert!(
        !work.join("Shoot A").exists(),
        "the replacement was pasted into work"
    );
    assert_eq!(
        fs::read(desktop.join("Shoot A old").join("a.txt")).unwrap(),
        b"folder a"
    );
}

/// Renaming a cut item in ZXraw (F2) keeps the cut on the new name, so a later paste moves it.
#[test]
fn renaming_a_cut_item_in_zxraw_keeps_it_cut() {
    let sandbox = tempfile::tempdir().unwrap();
    let Some(_clipboard) = clipboard_guard(sandbox.path()) else {
        return;
    };
    let (desktop, work, folders) = clipboard_fixture(sandbox.path());
    let renamed = desktop.join("Shoot Renamed");
    let script = format!(
        "navigate {desktop}\n\
         select Shoot A\n\
         key ctrl+X\n\
         ui system wait-cut 1\n\
         sleep 1000\n\
         key F2\n\
         type Shoot Renamed\n\
         accept\n\
         wait-exists {renamed}\n\
         sleep 2500\n\
         ui system wait-cut 1\n\
         ui system assert-cut-path {renamed}\n\
         navigate {work}\n\
         click-empty\n\
         key ctrl+V\n\
         wait-exists {moved}\n\
         wait-jobs 20\n\
         ui system wait-no-cut\n\
         assert-no-error-toast\n\
         exit\n",
        desktop = shown(&desktop),
        work = shown(&work),
        renamed = shown(&renamed),
        moved = shown(&work.join("Shoot Renamed").join("a.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(
        fs::read(work.join("Shoot Renamed").join("a.txt")).unwrap(),
        b"folder a"
    );
    assert!(!renamed.exists(), "the renamed folder was left in the Desktop");
    assert!(folders[1].is_dir(), "an item that was not cut stays");
    let clip = forge_win::get_files().ok().flatten().map(|(paths, _)| paths);
    assert!(
        !clip.is_some_and(|paths| paths.contains(&renamed)),
        "the clipboard still holds the renamed cut"
    );
}

/// An older ZXraw copy must not shadow a newer cut from Explorer. Paste here is the
/// command the background context menu runs.
#[test]
fn explorer_cut_after_an_in_app_copy_pastes_the_explorer_items() {
    let sandbox = tempfile::tempdir().unwrap();
    let Some(_clipboard) = clipboard_guard(sandbox.path()) else {
        return;
    };
    let (desktop, work, folders) = clipboard_fixture(sandbox.path());
    write_file(&desktop.join("stale.txt"), b"stale");
    let script = format!(
        "navigate {desktop}\n\
         select stale.txt\n\
         cmd Copy\n\
         ui system clipboard-files cut {a}, {b}\n\
         navigate {work}\n\
         cmd Paste\n\
         wait-exists {moved}\n\
         wait-jobs 20\n\
         assert-no-error-toast\n\
         exit\n",
        desktop = shown(&desktop),
        work = shown(&work),
        a = shown(&folders[0]),
        b = shown(&folders[1]),
        moved = shown(&work.join("Shoot B").join("nested").join("b.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_folders_moved(&work, &folders);
    assert!(
        !work.join("stale.txt").exists(),
        "the stale in-app copy was pasted"
    );
}

/// Ctrl+Shift+V is another shortcut: with text on the clipboard egui still reports it as a
/// paste, but it must not paste files. Ctrl+V then does.
#[test]
fn ctrl_shift_v_does_not_paste_files() {
    let sandbox = tempfile::tempdir().unwrap();
    let Some(_clipboard) = clipboard_guard(sandbox.path()) else {
        return;
    };
    let (desktop, work, _) = clipboard_fixture(sandbox.path());
    write_file(&desktop.join("loose.txt"), b"loose");
    let script = format!(
        "navigate {desktop}\n\
         select loose.txt\n\
         key ctrl+X\n\
         navigate {work}\n\
         click-empty\n\
         key ctrl+shift+V\n\
         wait-frames 20\n\
         sleep 500\n\
         assert-missing {moved}\n\
         key ctrl+V\n\
         wait-exists {moved}\n\
         wait-jobs 20\n\
         assert-no-error-toast\n\
         exit\n",
        desktop = shown(&desktop),
        work = shown(&work),
        moved = shown(&work.join("loose.txt")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_eq!(fs::read(work.join("loose.txt")).unwrap(), b"loose");
}

/// The files of the folder tree that the cut-to-Desktop scenarios cut, relative to it.
const TREE_FILES: [&str; 4] = ["one.txt", "two.bin", "sub/three.txt", "sub/deeper/four.txt"];

/// `<sandbox>/source` with `Shoot C` (files and nested subfolders, see [`TREE_FILES`]) and
/// `Shoot D`, and empty `<sandbox>/Desktop` and `<sandbox>/elsewhere` folders. The
/// "Desktop" is a temp folder, never the real one.
fn tree_fixture(sandbox: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let source = sandbox.join("source");
    for (i, file) in TREE_FILES.iter().enumerate() {
        write_file(&source.join("Shoot C").join(file), &noise(4096, i as u32));
    }
    write_file(&source.join("Shoot D").join("keep.txt"), b"not cut");
    let desktop = sandbox.join("Desktop");
    let elsewhere = sandbox.join("elsewhere");
    fs::create_dir_all(&desktop).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    (source, desktop, elsewhere)
}

/// `Shoot C` left `source` and arrived in `dest` byte for byte; `Shoot D` stayed,
/// `elsewhere` is still empty and the clipboard no longer holds the cut.
fn assert_tree_moved(source: &Path, dest: &Path, elsewhere: &Path) {
    assert!(
        !source.join("Shoot C").exists(),
        "Shoot C was not moved away"
    );
    for (i, file) in TREE_FILES.iter().enumerate() {
        assert_eq!(
            fs::read(dest.join("Shoot C").join(file)).ok(),
            Some(noise(4096, i as u32)),
            "{file} did not arrive intact"
        );
    }
    assert_eq!(
        fs::read(source.join("Shoot D").join("keep.txt")).unwrap(),
        b"not cut",
        "an item that was not cut stays"
    );
    assert_eq!(
        fs::read_dir(elsewhere).unwrap().count(),
        0,
        "the later Ctrl+V pasted something"
    );
    let clip = forge_win::get_files()
        .ok()
        .flatten()
        .map(|(paths, _)| paths);
    assert!(
        !clip.is_some_and(|paths| paths.contains(&source.join("Shoot C"))),
        "the stale cut is still on the clipboard"
    );
}

/// After the cut was used up: a later Ctrl+V in `elsewhere` must paste nothing and show
/// no error ("Left out", a failed move), only that there is nothing to paste.
fn later_ctrl_v_steps(elsewhere: &Path) -> String {
    format!(
        "navigate {elsewhere}\n\
         click-empty\n\
         key ctrl+V\n\
         wait-frames 20\n\
         sleep 1500\n\
         wait-jobs 20\n\
         assert-toast Nothing to paste\n\
         assert-no-error-toast\n",
        elsewhere = shown(elsewhere),
    )
}

/// What the Desktop does with a cut on Ctrl+V, from another thread with its own OLE
/// apartment: it reads `CF_HDROP` and "Preferred DropEffect" through the OLE clipboard,
/// moves the items with `IFileOperation` (a rename on the same volume), reports "Performed
/// DropEffect" and "Paste Succeeded" back to the data object, and (with `clear`) empties
/// the clipboard if it still holds that cut.
///
/// The clipboard is shared with the user's session, so this refuses to move anything
/// unless the clipboard holds exactly the test's own cut.
mod desktop {
    use std::path::{Path, PathBuf};

    use windows::Win32::Foundation::{GlobalFree, HGLOBAL};
    use windows::Win32::System::Com::{
        CLSCTX_ALL, CoCreateInstance, DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM,
        STGMEDIUM_0, TYMED_HGLOBAL,
    };
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
    };
    use windows::Win32::System::Ole::{
        DROPEFFECT_MOVE, OleGetClipboard, OleInitialize, OleUninitialize, ReleaseStgMedium,
    };
    use windows::Win32::UI::Shell::{
        DragQueryFileW, FOF_NOCONFIRMATION, FOF_NOCONFIRMMKDIR, FOF_NOERRORUI, FOF_SILENT,
        FileOperation, HDROP, IFileOperation, IShellItem, SHCreateItemFromParsingName,
    };
    use windows::core::{HSTRING, PCWSTR, w};

    fn format(name: PCWSTR) -> FORMATETC {
        format_id(unsafe { RegisterClipboardFormatW(name) } as u16)
    }

    fn format_id(id: u16) -> FORMATETC {
        FORMATETC {
            cfFormat: id,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        }
    }

    fn read_dword(data: &IDataObject, name: PCWSTR) -> Result<u32, String> {
        unsafe {
            let mut medium = data.GetData(&format(name)).map_err(|e| e.to_string())?;
            let global = medium.u.hGlobal;
            let value = if GlobalSize(global) < 4 {
                Err("shorter than 4 bytes".to_string())
            } else {
                let ptr = GlobalLock(global) as *const u32;
                if ptr.is_null() {
                    Err("GlobalLock failed".to_string())
                } else {
                    let value = ptr.read_unaligned();
                    let _ = GlobalUnlock(global);
                    Ok(value)
                }
            };
            ReleaseStgMedium(&mut medium);
            value
        }
    }

    /// The paths in `CF_HDROP`. ZXraw puts no "Shell IDList Array" on the clipboard, so the
    /// shell's paste works from this list.
    fn read_hdrop(data: &IDataObject) -> Result<Vec<PathBuf>, String> {
        const CF_HDROP: u16 = 15;
        unsafe {
            let mut medium = data
                .GetData(&format_id(CF_HDROP))
                .map_err(|e| e.to_string())?;
            let drop = HDROP(medium.u.hGlobal.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            let paths = (0..count)
                .map(|i| {
                    let mut buf = vec![0u16; DragQueryFileW(drop, i, None) as usize + 1];
                    let len = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
                    PathBuf::from(String::from_utf16_lossy(&buf[..len]))
                })
                .collect();
            ReleaseStgMedium(&mut medium);
            Ok(paths)
        }
    }

    fn write_dword(data: &IDataObject, name: PCWSTR, value: u32) -> windows::core::Result<()> {
        unsafe {
            let handle: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, 4)?;
            let ptr = GlobalLock(handle) as *mut u32;
            if ptr.is_null() {
                let error = windows::core::Error::from_thread();
                let _ = GlobalFree(Some(handle));
                return Err(error);
            }
            ptr.write_unaligned(value);
            let _ = GlobalUnlock(handle);
            let medium = STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: handle },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            };
            let result = data.SetData(&format(name), &medium, true);
            if result.is_err() {
                // The data object takes the memory only when it accepts it.
                let _ = GlobalFree(Some(handle));
            }
            result
        }
    }

    /// Moves `expected` into `dest` if the clipboard holds exactly that cut. Returns what it
    /// did, for the test output.
    pub fn paste(dest: &Path, expected: &[PathBuf], clear: bool) -> Result<String, String> {
        unsafe {
            OleInitialize(None).map_err(|e| format!("OleInitialize: {e}"))?;
            let result = paste_in_apartment(dest, expected, clear);
            OleUninitialize();
            result
        }
    }

    /// One consistent read of the clipboard: the data object, the dropped paths, the drop
    /// effect, and the clipboard sequence they belong to (unchanged across the read).
    unsafe fn read_clipboard() -> Result<(IDataObject, Vec<PathBuf>, u32, u32), String> {
        unsafe {
            let before = forge_win::clipboard_sequence();
            let data = OleGetClipboard().map_err(|e| format!("OleGetClipboard: {e}"))?;
            let paths = read_hdrop(&data).map_err(|e| format!("CF_HDROP: {e}"))?;
            let effect = read_dword(&data, w!("Preferred DropEffect"))
                .map_err(|e| format!("Preferred DropEffect: {e}"))?;
            if forge_win::clipboard_sequence() != before {
                return Err("the clipboard changed while it was read".into());
            }
            Ok((data, paths, effect, before))
        }
    }

    unsafe fn paste_in_apartment(
        dest: &Path,
        expected: &[PathBuf],
        clear: bool,
    ) -> Result<String, String> {
        unsafe {
            // ZXraw's clipboard worker may hold or rewrite the clipboard for a moment.
            let mut tries = 0;
            let (data, paths, effect, sequence) = loop {
                match read_clipboard() {
                    Ok(read) => break read,
                    Err(e) if tries >= 50 => return Err(e),
                    Err(_) => {
                        tries += 1;
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            };
            if paths != expected {
                return Err(format!(
                    "the clipboard holds {paths:?}, not the test's cut {expected:?}; \
                     nothing was moved"
                ));
            }
            if effect & DROPEFFECT_MOVE.0 == 0 {
                return Err(format!("Preferred DropEffect is {effect}, not a move"));
            }
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)
                .map_err(|e| format!("IFileOperation: {e}"))?;
            op.SetOperationFlags(
                FOF_SILENT | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_NOCONFIRMMKDIR,
            )
            .map_err(|e| format!("SetOperationFlags: {e}"))?;
            let target: IShellItem =
                SHCreateItemFromParsingName(&HSTRING::from(dest.as_os_str()), None)
                    .map_err(|e| format!("destination: {e}"))?;
            // Only the known fixture paths are moved, never what was read.
            for path in expected {
                let item: IShellItem =
                    SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None)
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                op.MoveItem(&item, &target, PCWSTR::null(), None)
                    .map_err(|e| format!("MoveItem: {e}"))?;
            }
            op.PerformOperations()
                .map_err(|e| format!("PerformOperations: {e}"))?;
            if op.GetAnyOperationsAborted().is_ok_and(|b| b.as_bool()) {
                return Err("the move was aborted".into());
            }
            // ZXraw's cut is plain clipboard data rather than a live data object, so these
            // may be refused; the shell carries on either way.
            let performed = write_dword(&data, w!("Performed DropEffect"), DROPEFFECT_MOVE.0);
            let succeeded = write_dword(&data, w!("Paste Succeeded"), DROPEFFECT_MOVE.0);
            // Empties the clipboard only if it still holds this cut, checked while it is
            // open. ZXraw may have noticed the move and changed it first; that is fine, but
            // the clear must not fail while the cut is still there.
            let cleared =
                clear && forge_win::clear_if_owned_files(expected, Some(sequence)).is_some();
            if clear && !cleared && forge_win::clipboard_sequence() == sequence {
                return Err("the clipboard still holds the cut but was not cleared".into());
            }
            Ok(format!(
                "moved {expected:?}; Performed DropEffect: {performed:?}; \
                 Paste Succeeded: {succeeded:?}; cleared: {cleared}"
            ))
        }
    }
}

/// Cut a folder tree in ZXraw, let `paste` paste the cut (the paths it is given, all inside
/// the sandbox) into the "Desktop" folder once the cut is on the clipboard, then press
/// Ctrl+V in ZXraw somewhere else.
fn cut_pasted_on_the_desktop(
    paste: impl FnOnce(&Path, &[PathBuf]) -> Result<String, String> + Send + 'static,
) {
    let sandbox = tempfile::tempdir().unwrap();
    let Some(_clipboard) = clipboard_guard(sandbox.path()) else {
        return;
    };
    let (source, desktop, elsewhere) = tree_fixture(sandbox.path());
    let shell = {
        let cut = vec![source.join("Shoot C")];
        let desktop = desktop.clone();
        std::thread::spawn(move || -> Result<String, String> {
            if !wait_for_move_cut(cut.clone()) {
                return Err("the cut never reached the Windows clipboard".into());
            }
            paste(&desktop, &cut)
        })
    };
    let script = format!(
        "navigate {source}\n\
         select Shoot C\n\
         key ctrl+X\n\
         wait-missing {moved_away}\n\
         wait-exists {arrived}\n\
         ui system wait-no-cut\n\
         wait-no-entry Shoot C\n\
         wait-entry Shoot D\n\
         assert-no-error-toast\n\
         {later}\
         exit\n",
        source = shown(&source),
        moved_away = shown(&source.join("Shoot C")),
        arrived = shown(&desktop.join("Shoot C").join(TREE_FILES[3])),
        later = later_ctrl_v_steps(&elsewhere),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    let pasted = shell.join().unwrap();
    eprintln!("desktop paste: {pasted:?}");
    expect_ok(&outcome);
    assert!(pasted.is_ok(), "the Desktop paste failed");
    assert_tree_moved(&source, &desktop, &elsewhere);
}

/// The user's report: Ctrl+X on a folder in ZXraw, paste on the Desktop. Here the Desktop
/// leaves the clipboard alone, so ZXraw must notice by itself that its cut was used up.
#[test]
fn a_folder_cut_in_zxraw_pastes_on_the_desktop() {
    cut_pasted_on_the_desktop(|dest, cut| desktop::paste(dest, cut, false));
}

/// The same when the shell empties the clipboard after the paste.
#[test]
fn a_folder_cut_in_zxraw_pastes_on_a_desktop_that_clears_the_clipboard() {
    cut_pasted_on_the_desktop(|dest, cut| desktop::paste(dest, cut, true));
}

/// The same tree cut and pasted inside ZXraw; a later Ctrl+V pastes nothing.
#[test]
fn a_folder_tree_cut_in_zxraw_pastes_once_in_zxraw() {
    let sandbox = tempfile::tempdir().unwrap();
    let Some(_clipboard) = clipboard_guard(sandbox.path()) else {
        return;
    };
    let (source, desktop, elsewhere) = tree_fixture(sandbox.path());
    let script = format!(
        "navigate {source}\n\
         select Shoot C\n\
         key ctrl+X\n\
         ui system wait-cut 1\n\
         navigate {desktop}\n\
         click-empty\n\
         key ctrl+V\n\
         wait-exists {arrived}\n\
         wait-jobs 20\n\
         ui system wait-no-cut\n\
         wait-entry Shoot C\n\
         assert-no-error-toast\n\
         {later}\
         exit\n",
        source = shown(&source),
        desktop = shown(&desktop),
        arrived = shown(&desktop.join("Shoot C").join(TREE_FILES[3])),
        later = later_ctrl_v_steps(&elsewhere),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_tree_moved(&source, &desktop, &elsewhere);
}
