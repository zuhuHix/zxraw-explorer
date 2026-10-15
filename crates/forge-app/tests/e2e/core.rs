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
    // picks a host the other tests (which use .1) do not. While the rename is still waiting on
    // the share, Undo must not act on an older record: it says it is waiting instead.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let share = format!(r"\\192.0.2.{}\x", 2 + nanos % 250);
    let script = format!(
        "dismiss-notices\nnavigate {sandbox}\n\
         ui browser offline rename {share}\\a.txt | b.txt\n\
         ui sidebar responsive 30 | 1000\n\
         cmd Undo\nassert-toast Waiting for the rename to finish\n\
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

/// Scenarios that use the real Windows clipboard run one at a time.
fn clipboard_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
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
    let _clipboard = clipboard_lock();
    let sandbox = tempfile::tempdir().unwrap();
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
    let _clipboard = clipboard_lock();
    let sandbox = tempfile::tempdir().unwrap();
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
    let _clipboard = clipboard_lock();
    let sandbox = tempfile::tempdir().unwrap();
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

/// An older ZXraw copy must not shadow a newer cut from Explorer. Paste here is the
/// command the background context menu runs.
#[test]
fn explorer_cut_after_an_in_app_copy_pastes_the_explorer_items() {
    let _clipboard = clipboard_lock();
    let sandbox = tempfile::tempdir().unwrap();
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
    let _clipboard = clipboard_lock();
    let sandbox = tempfile::tempdir().unwrap();
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
