//! End-to-end UI scenarios.
//!
//! Each test starts the real `zxraw` window with `ZXRAW_SCRIPT` pointing at a small step
//! script (grammar: `crates/forge-app/src/testhook.rs`), drives it through the same command
//! and dialog code paths the user reaches with menus and keys, and then asserts on the
//! filesystem. Every run gets a fresh profile, so no user settings, session or library data
//! is read or written.
//!
//! The app opens a real window, so these tests run one at a time (a static lock enforces
//! it; `--test-threads=1` is still recommended). Recycle Bin behaviour is deliberately not
//! covered: these tests must never touch the user's real Recycle Bin.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Hard limit for one app run. The scripts themselves finish in a few seconds.
const RUN_LIMIT: Duration = Duration::from_secs(75);

static WINDOW: Mutex<()> = Mutex::new(());

struct Outcome {
    status: ExitStatus,
    /// Contents of the script result file, trimmed: `OK` or `SCRIPT ERROR: …`.
    result: String,
    /// Tail of the app's stderr log.
    stderr: String,
}

/// Runs `script` in a fresh window with the given profile and waits for it to finish.
/// Panics with a clear message when the window never ran the script.
fn run_app(work: &Path, profile: &Path, script: &str) -> Outcome {
    let _window = WINDOW.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    fs::create_dir_all(work).unwrap();
    let script_path = work.join("steps.txt");
    fs::write(&script_path, script).unwrap();
    let result_path = work.join("steps.result");
    let stderr_path = work.join("stderr.log");
    let stderr = fs::File::create(&stderr_path).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_zxraw"))
        .env("ZXRAW_SCRIPT", &script_path)
        .env("ZXRAW_SCRIPT_RESULT", &result_path)
        .env("ZXRAW_PROFILE_DIR", profile)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap_or_else(|e| panic!("cannot start zxraw: {e}"));

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > RUN_LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "zxraw did not finish within {} s; stderr tail:\n{}",
                RUN_LIMIT.as_secs(),
                tail(&stderr_path)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let result = fs::read_to_string(&result_path).unwrap_or_default();
    if result.trim().is_empty() {
        panic!(
            "zxraw exited with {status} without writing a script result. Either no window could \
             be created (GPU or display problem) or the app crashed. stderr tail:\n{}",
            tail(&stderr_path)
        );
    }
    Outcome {
        status,
        result: result.trim().to_string(),
        stderr: tail(&stderr_path),
    }
}

/// Asserts that the script ran to the end without a step failing.
fn expect_ok(outcome: &Outcome) {
    assert!(
        outcome.status.success() && outcome.result == "OK",
        "script failed: exit {}, result `{}`\nstderr tail:\n{}",
        outcome.status,
        outcome.result,
        outcome.stderr
    );
}

fn tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let start = text.len().saturating_sub(3000);
    let start = (start..=text.len())
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(text.len());
    text[start..].to_string()
}

/// Fixture bytes: deterministic pseudo-random data so copies can be compared byte for byte.
fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (state >> 16) as u8
        })
        .collect()
}

fn write_file(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn shown(path: &Path) -> String {
    path.display().to_string()
}

/// Scenario setup for the copy/move tests: a source folder and an empty destination folder,
/// shown side by side in dual-pane mode (left = source, right = destination).
fn dual_pane_script(src: &Path, dst: &Path, steps: &[&str]) -> String {
    let mut script = format!(
        "navigate {}\ncmd ToggleDualPane\nfocus right\nnavigate {}\nfocus left\n",
        shown(src),
        shown(dst)
    );
    for step in steps {
        script.push_str(step);
        script.push('\n');
    }
    script.push_str("exit\n");
    script
}

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
    assert!(!dir.join("New folder").exists(), "the default name must not be left behind");
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
    assert_eq!(fs::read(src.join("a.txt")).unwrap(), text, "copy must keep the source");
    assert_eq!(fs::read(src.join("b.bin")).unwrap(), binary, "copy must keep the source");
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
    assert!(!src.join("move.txt").exists(), "the source must be gone after a move");
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
    assert!(fs::metadata(&shot).map(|m| m.len() > 0).unwrap_or(false), "no screenshot was saved");
    let crashes = profile.join("data").join("crashes");
    // `crash-*.txt` files are reports; `.last-startup-nanos` is the startup marker.
    let reports: Vec<PathBuf> = fs::read_dir(&crashes)
        .map(|d| {
            d.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("crash-")))
                .collect()
        })
        .unwrap_or_default();
    let details: String = reports
        .iter()
        .map(|r| {
            let body = fs::read_to_string(r).unwrap_or_default();
            format!("{}:\n{}\n", r.display(), body.chars().take(2000).collect::<String>())
        })
        .collect();
    assert!(reports.is_empty(), "the app wrote {} crash report(s):\n{details}", reports.len());
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
        &format!(
            "assert-tabs 2\nassert-path {}\nexit\n",
            shown(&second)
        ),
    );
    expect_ok(&outcome);
}
