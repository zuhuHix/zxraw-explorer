//! End-to-end UI scenarios.
//!
//! Each test starts the real `zxraw` window with `ZXRAW_SCRIPT` pointing at a small step
//! script (grammar: `crates/forge-app/src/testhook/mod.rs` and the area modules next to
//! it), drives it through the same command and dialog code paths the user reaches with
//! menus and keys, and then asserts on the filesystem. Every run gets a fresh profile, so
//! no user settings, session or library data is read or written. Automated runs never
//! show native file dialogs (answers come from `dialog` steps) and recycle into a sandbox
//! folder inside the profile, never the real Recycle Bin.
//!
//! Each app window opens on its own private, invisible desktop (named after the test
//! process), ignores real input, and uses its own profile, temp folders and ephemeral ports,
//! so runs are isolated from each other. Up to N app runs execute at once, where N is
//! `ZXRAW_E2E_JOBS` (default: `min(3, available_parallelism / 2)`, at least 1; `1` restores
//! the old one-at-a-time behaviour). A counting gate ([`run_app_with`]) enforces the limit.
//! Tests whose numbers depend on the machine being otherwise idle (frame timings in `perf`
//! and the grouping frame-time test in `browser`) call [`exclusive`] first, which waits for
//! every permit, so they run alone. `ZXRAW_SHOW_WINDOW=1` shows a window to watch it (use
//! `ZXRAW_E2E_JOBS=1` with that). Areas live in their own modules; run one with e.g.
//! `cargo test -p forge-app --test e2e viewer::`.

#![allow(dead_code)]

mod browser;
mod core;
mod ingest;
mod library;
mod media;
mod perf;
mod related;
mod sidebar;
mod system;
mod tools;
mod viewer;

use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Default hard limit for one app run. Most scripts finish in a few seconds.
pub const RUN_LIMIT: Duration = Duration::from_secs(75);

/// Counting gate for app runs: `free` permits are available out of `total`.
struct Gate {
    free: Mutex<usize>,
    changed: Condvar,
    total: usize,
}

static GATE: OnceLock<Gate> = OnceLock::new();

thread_local! {
    /// Set on a test thread that holds [`exclusive`], whose app runs must not take a permit
    /// again (it already holds all of them).
    static EXCLUSIVE: Cell<bool> = Cell::new(false);
}

/// Number of app runs allowed at once: `ZXRAW_E2E_JOBS`, else `min(3, cores / 2)`, at least 1.
pub fn e2e_jobs() -> usize {
    let default = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).min(3))
        .unwrap_or(1)
        .max(1);
    std::env::var("ZXRAW_E2E_JOBS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(default)
}

fn gate() -> &'static Gate {
    GATE.get_or_init(|| {
        let total = e2e_jobs();
        Gate {
            free: Mutex::new(total),
            changed: Condvar::new(),
            total,
        }
    })
}

/// Permits taken from the gate; they are returned on drop.
struct Permit(usize);

fn acquire(n: usize) -> Permit {
    let g = gate();
    let mut free = g.free.lock().unwrap_or_else(|p| p.into_inner());
    while *free < n {
        free = g.changed.wait(free).unwrap_or_else(|p| p.into_inner());
    }
    *free -= n;
    Permit(n)
}

impl Drop for Permit {
    fn drop(&mut self) {
        let g = gate();
        let mut free = g.free.lock().unwrap_or_else(|p| p.into_inner());
        *free += self.0;
        g.changed.notify_all();
    }
}

/// Held by a test that must run alone (see [`exclusive`]).
pub struct Exclusive {
    _permit: Permit,
}

impl Drop for Exclusive {
    fn drop(&mut self) {
        EXCLUSIVE.with(|c| c.set(false));
    }
}

/// Waits until no other app run is in progress and keeps new ones out until the returned
/// guard is dropped. Use it first in any test whose assertions or reports depend on timing.
pub fn exclusive() -> Exclusive {
    let permit = acquire(gate().total);
    EXCLUSIVE.with(|c| c.set(true));
    Exclusive { _permit: permit }
}

pub struct Outcome {
    pub status: ExitStatus,
    /// Contents of the script result file, trimmed: `OK` or `SCRIPT ERROR: …`.
    pub result: String,
    /// Tail of the app's stderr log.
    pub stderr: String,
}

/// Extra options for [`run_app_with`].
#[derive(Default)]
pub struct RunOpts {
    /// Hard limit for the process (defaults to [`RUN_LIMIT`]). Scripts that need longer
    /// must also raise their own limit with a `limit <seconds>` step.
    pub limit: Option<Duration>,
    /// Extra environment variables for the app process.
    pub env: Vec<(String, String)>,
    /// Command-line arguments for the app (e.g. a folder to open).
    pub args: Vec<OsString>,
}

/// Folder scenarios save screenshots into (`%TEMP%\zxfr-shots`), created on demand.
pub fn shots_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("zxfr-shots");
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs `script` in a fresh window with the given profile and waits for it to finish.
/// Panics with a clear message when the window never ran the script.
pub fn run_app(work: &Path, profile: &Path, script: &str) -> Outcome {
    run_app_with(work, profile, script, &RunOpts::default())
}

pub fn run_app_with(work: &Path, profile: &Path, script: &str, opts: &RunOpts) -> Outcome {
    // A test that holds `exclusive()` already has every permit.
    let _permit = (!EXCLUSIVE.with(|c| c.get())).then(|| acquire(1));
    let limit = opts.limit.unwrap_or(RUN_LIMIT);
    fs::create_dir_all(work).unwrap();
    let script_path = work.join("steps.txt");
    fs::write(&script_path, script).unwrap();
    let result_path = work.join("steps.result");
    let stderr_path = work.join("stderr.log");
    let stderr = fs::File::create(&stderr_path).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_zxraw"));
    command
        .env("ZXRAW_SCRIPT", &script_path)
        .env("ZXRAW_SCRIPT_RESULT", &result_path)
        // Scripts may only touch paths under the test's sandbox (the profile's parent).
        .env(
            "ZXRAW_SCRIPT_SANDBOX",
            profile.parent().expect("profile sandbox"),
        )
        .env("ZXRAW_PROFILE_DIR", profile)
        // Scenarios collect their screenshots here for review (PNG files only).
        .env("ZXRAW_SCREENSHOT_DIR", shots_dir())
        .args(&opts.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    for (k, v) in &opts.env {
        command.env(k, v);
    }
    let mut child = command
        .spawn()
        .unwrap_or_else(|e| panic!("cannot start zxraw: {e}"));

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "zxraw did not finish within {} s; stderr tail:\n{}",
                limit.as_secs(),
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
pub fn expect_ok(outcome: &Outcome) {
    assert!(
        outcome.status.success() && outcome.result == "OK",
        "script failed: exit {}, result `{}`\nstderr tail:\n{}",
        outcome.status,
        outcome.result,
        outcome.stderr
    );
}

/// Asserts that the script failed, with `needle` in the error message.
pub fn expect_script_error(outcome: &Outcome, needle: &str) {
    assert!(
        !outcome.status.success()
            && outcome.result.starts_with("SCRIPT ERROR")
            && outcome.result.contains(needle),
        "expected a script error containing `{needle}`, got exit {}, result `{}`",
        outcome.status,
        outcome.result
    );
}

pub fn tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let start = text.len().saturating_sub(3000);
    let start = (start..=text.len())
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(text.len());
    text[start..].to_string()
}

/// Fixture bytes: deterministic pseudo-random data so copies can be compared byte for byte.
pub fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (state >> 16) as u8
        })
        .collect()
}

pub fn write_file(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

pub fn shown(path: &Path) -> String {
    path.display().to_string()
}

/// Crash reports (`crash-*.txt`) the app wrote into `profile`, with their contents.
pub fn crash_reports(profile: &Path) -> Vec<(PathBuf, String)> {
    let crashes = profile.join("data").join("crashes");
    fs::read_dir(&crashes)
        .map(|d| {
            d.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("crash-"))
                })
                .map(|p| {
                    let body = fs::read_to_string(&p).unwrap_or_default();
                    (p, body.chars().take(2000).collect())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Panics with the report contents if the app wrote any crash report into `profile`.
pub fn assert_no_crash(profile: &Path) {
    let reports = crash_reports(profile);
    let details: String = reports
        .iter()
        .map(|(p, body)| format!("{}:\n{body}\n", p.display()))
        .collect();
    assert!(
        reports.is_empty(),
        "the app wrote {} crash report(s):\n{details}",
        reports.len()
    );
}

/// The sandbox the app recycles into during automated runs (never the real Recycle Bin).
pub fn recycle_sandbox(profile: &Path) -> PathBuf {
    profile.join("data").join("recycle-sandbox")
}

/// Scenario setup for the copy/move tests: a source folder and an empty destination folder,
/// shown side by side in dual-pane mode (left = source, right = destination).
pub fn dual_pane_script(src: &Path, dst: &Path, steps: &[&str]) -> String {
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
