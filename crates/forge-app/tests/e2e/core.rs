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
        "navigate {}\ncmd NewFolder\ntype Fresh folder\naccept\nassert-exists {}\nexit\n",
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
         assert-exists {renamed}\n\
         assert-missing {original}\n\
         cmd Undo\n\
         assert-exists {original}\n\
         assert-missing {renamed}\n\
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
