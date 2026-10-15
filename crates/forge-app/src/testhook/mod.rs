//! Automation hook for UI verification, screenshots and end-to-end scripts.
//!
//! * `ZXRAW_CMDS=ViewGrid,ToggleDualPane` — run commands (by `Cmd` variant name) once the
//!   first folder has loaded.
//! * `ZXRAW_SELECT=name.jpg` — select that entry in the focused pane before the commands.
//! * `ZXRAW_SCREENSHOT=out.png` — after the UI settles, capture the rendered frame via
//!   egui's own screenshot path (works even where OS screen capture can't see GPU
//!   swapchains), save it, and exit.
//! * `ZXRAW_SCRIPT=steps.txt` — run a step script, one step per line (see [`parse_script`]
//!   for the grammar). Takes precedence over the three variables above. The outcome goes to
//!   `ZXRAW_SCRIPT_RESULT` (default `<script>.result`): `OK`, or `SCRIPT ERROR: …` and exit
//!   code 2. Release builds have no console, so that result file is the reliable record.
//! * `ZXRAW_PROFILE_DIR=dir` — profile (settings, session, library, caches) to use. Any
//!   automated run without it gets a unique throwaway profile, never the user's profile.
//! * `ZXRAW_SCRIPT_SANDBOX=dir` — allowed root for navigation and asserted paths; defaults
//!   to the script file's directory.
//! * `ZXRAW_SCREENSHOT_DIR=dir` — a second allowed root, for `screenshot` PNGs only.
//! * `ZXRAW_SHOW_WINDOW=1` — show the window normally. Otherwise automated runs open on a
//!   private, invisible desktop and ignore real mouse and keyboard input ([`hide_window`]).
//!
//! Timing: commands run once the first listing finishes (or after 30 s regardless); the
//! capture happens ~90 frames *after* the commands; a run is closed after 120 s at most.
//! Script steps run one per frame, after every pane has finished loading, except waiting
//! steps which block until their condition holds (each step gives up after 30 s).
//!
//! The hook is compiled only for debug builds or with the non-default `automation` feature
//! (see `main.rs`); release builds get the inert stub in `testhook_off.rs`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use forge_core::ops::{ConflictDecision, Resolution};

use crate::actions;
use crate::app::{Side, ZxrawApp};
use crate::commands::Cmd;
use crate::modal::Modal;
use crate::pane::Pane;

// Area-specific script verbs (`ui <area> <verb> <args>`). Each area module owns its verbs.
mod browser;
mod ingest;
mod library;
mod media;
mod perf;
mod related;
mod sidebar;
mod system;
mod tools;
mod viewer;

/// Script verbs that inject input or measure frames need state outside `Script`.
#[derive(Default)]
pub(crate) struct InputQueue {
    /// Events to hand to egui at the start of the next frame.
    pub pending: Vec<egui::Event>,
    /// Paste key presses delivered with `pending`, as the Win32 message hook would see them.
    pub paste_presses: u32,
    /// Frames that have started since the last injection (0 = not delivered yet).
    pub delivered_frames: u32,
    /// Where the mouse pointer was last placed by a script (for wheel events).
    pub pointer: Option<egui::Pos2>,
    /// Modifier state held across scripted pointer steps.
    pub modifiers: egui::Modifiers,
}

/// Per-step limit for steps that wait (listings, dialogs, jobs unless given explicitly).
const STEP_LIMIT: Duration = Duration::from_secs(30);
/// Whole-script limit; the app is stopped with `SCRIPT ERROR` past this.
const SCRIPT_LIMIT: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct TestHook {
    screenshot: Option<PathBuf>,
    cmds: Vec<Cmd>,
    select: Option<String>,
    frames: u32,
    ran_cmds: bool,
    frames_after_cmds: u32,
    requested: bool,
    started: Option<Instant>,
    script: Option<Script>,
    /// Every toast shown during the run (`(text, is_error)`), for `assert-toast`.
    toast_log: Vec<(String, bool)>,
    /// Fixed at startup: the script is taken out of `self` while a step runs, so
    /// "is a script present" cannot answer "is this an automated run".
    automated: bool,
    /// Fixed at startup: see [`hide_window`].
    hidden: bool,
    animations_off: bool,
    pub(crate) input: InputQueue,
    pub(crate) perf: perf::FrameClock,
}

/// True when the environment asks for an automated run (checked before the app starts so
/// the profile can be isolated).
pub fn env_requests_automation() -> bool {
    [
        "ZXRAW_SCREENSHOT",
        "ZXRAW_CMDS",
        "ZXRAW_SELECT",
        "ZXRAW_SCRIPT",
    ]
    .iter()
    .any(|v| std::env::var_os(v).is_some())
}

/// True when an automated run should keep its window out of the user's way: it opens on a
/// private Windows desktop that is never shown, and ignores real mouse and keyboard input,
/// so working on the PC while tests run can neither disturb a script nor be disturbed by it. Scripts inject their own input, so they run the same either way.
/// `ZXRAW_SHOW_WINDOW=1` shows the window normally, to watch a script run.
pub fn hide_window() -> bool {
    env_requests_automation() && std::env::var_os("ZXRAW_SHOW_WINDOW").is_none()
}

impl TestHook {
    pub fn from_env() -> Self {
        let cmds = std::env::var("ZXRAW_CMDS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|n| {
                Cmd::ALL
                    .iter()
                    .find(|c| format!("{c:?}") == n.trim())
                    .copied()
            })
            .collect();
        let mut hook = Self {
            screenshot: std::env::var_os("ZXRAW_SCREENSHOT").map(PathBuf::from),
            cmds,
            select: std::env::var("ZXRAW_SELECT").ok(),
            script: std::env::var_os("ZXRAW_SCRIPT").map(|path| Script::load(PathBuf::from(path))),
            ..Default::default()
        };
        hook.automated = hook.script.is_some()
            || hook.screenshot.is_some()
            || !hook.cmds.is_empty()
            || hook.select.is_some();
        hook.hidden = hook.automated && hide_window();
        RECORD_RECTS.store(hook.automated, std::sync::atomic::Ordering::Relaxed);
        hook
    }

    pub fn record_toast(&mut self, text: &str, error: bool) {
        if self.active() {
            self.toast_log.push((text.to_string(), error));
        }
    }

    pub fn active(&self) -> bool {
        self.automated
    }
}

/// Called by eframe before every frame: delivers injected input and starts the frame clock.
pub fn raw_input(app: &mut ZxrawApp, raw: &mut egui::RawInput) {
    let hook = &mut app.testhook;
    if !hook.active() {
        return;
    }
    hook.perf.frame_started();
    if hook.hidden {
        // Only the script drives a hidden window; keep what the OS reports that isn't
        // user input (screenshot results, focus changes).
        raw.events.retain(|e| {
            matches!(
                e,
                egui::Event::Screenshot { .. } | egui::Event::WindowFocused(_)
            )
        });
        raw.hovered_files.clear();
        raw.dropped_files.clear();
    }
    if !hook.input.pending.is_empty() {
        raw.events.append(&mut hook.input.pending);
        hook.input.delivered_frames = 0;
    }
    for _ in 0..std::mem::take(&mut hook.input.paste_presses) {
        crate::paste_key::note_press();
    }
    hook.input.delivered_frames = hook.input.delivered_frames.saturating_add(1);
}

pub fn tick(app: &mut ZxrawApp, ctx: &egui::Context) {
    if !app.testhook.active() {
        return;
    }
    if !app.testhook.animations_off {
        // Screenshots must show settled UI, not windows halfway through a fade-in.
        ctx.all_styles_mut(|style| style.animation_time = 0.0);
        app.testhook.animations_off = true;
    }
    app.testhook.perf.frame_finished();
    if app.testhook.script.is_some() {
        script_tick(app, ctx);
        return;
    }
    let loading = app.tabs.iter().any(|t| t.left.is_loading());
    let hook = &mut app.testhook;
    hook.frames += 1;
    let elapsed = hook.started.get_or_insert_with(Instant::now).elapsed();
    if elapsed > Duration::from_secs(120) {
        tracing::error!("test hook: giving up after 120 s without a screenshot");
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        return;
    }
    let listing_done = !loading || elapsed > Duration::from_secs(30);
    if !hook.ran_cmds && listing_done && hook.frames > 5 {
        hook.ran_cmds = true;
        if let Some(name) = hook.select.take() {
            app.pane_mut().select_name(&name);
        }
        let hook = &mut app.testhook;
        let cmds = std::mem::take(&mut hook.cmds);
        for c in cmds {
            app.run(c, ctx);
        }
    }
    let hook = &mut app.testhook;
    if hook.ran_cmds {
        hook.frames_after_cmds += 1;
    }
    // Give async listings/thumbnails a moment to land, then capture.
    if hook.ran_cmds && hook.frames_after_cmds > 90 && !hook.requested && hook.screenshot.is_some()
    {
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        hook.requested = true;
    }
    let shot = screenshot_event(ctx);
    if let (Some(img), Some(path)) = (shot, hook.screenshot.clone()) {
        if let Err(e) = save_png(&img, &path) {
            tracing::error!("screenshot save failed: {e}");
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
    ctx.request_repaint();
}

// ---------------------------------------------------------------------------
// Script mode
// ---------------------------------------------------------------------------

/// One parsed script line. Grammar (verbs are case-sensitive, `#` starts a comment line):
///
/// ```text
/// navigate <path>            open <path> in the focused pane (a file opens its folder)
/// select <name>[, <name>…]   select entries by name in the focused pane (comma-separated)
/// select-all                 select every entry of the focused pane
/// focus left|right           move focus to a pane of the active tab
/// cmd <CmdVariant>           run a `Cmd` (its debug name, e.g. `CopyToOtherPane`)
/// type <text>                replace the text of the active Prompt dialog
/// accept                     accept the active dialog (Prompt/Confirm/Report/batch rename)
/// cancel                     dismiss the active dialog without applying it
/// conflict <decision> [all]  answer the oldest file conflict: keep-both | skip | replace |
///                            replace-if-newer | cancel; `all` applies it to the rest
/// wait-jobs [seconds]        wait until no operation is running (default 30 s)
/// wait-frames <n>            wait for n frames
/// screenshot <png>           save the rendered frame
/// assert-tabs <n>            fail unless the active tab count is n
/// assert-path <path>         fail unless the focused pane shows <path>
/// assert-exists <path>       fail unless the path exists on disk
/// assert-missing <path>      fail unless the path does not exist on disk
/// exit                       stop here (optional; the script also ends after its last line)
///
/// limit <seconds>            raise the whole-script limit (default 60 s, at most 600 s)
/// sleep <ms>                 wait for wall-clock time (frames keep running)
/// key <combo>                inject a key press through egui's real input path, e.g. `key 3`,
///                            `key ArrowRight`, `key ctrl+shift+F`, `key Escape`
/// text <text>                inject typed text (verbatim, like `type`)
/// pointer <x> <y>            move the mouse pointer (window points)
/// click <x> <y>              press and release the primary button there (over two frames)
/// marquee <from> <to> [ctrl|shift] drag from empty space below the view to two drawn entries
/// release-pointer            release the primary button after a `marquee` step
/// scroll <dy>                mouse wheel at the pointer; positive scrolls content down
/// dialog <path> | cancel     queue the answer for the next native file/folder dialog
/// wait-exists <path> [| s]   wait until the path exists (default 30 s)
/// wait-missing <path> [| s]  wait until the path is gone (default 30 s)
/// assert-same <a> | <b>      fail unless both files exist and have identical bytes
/// assert-contains <p> | <t>  fail unless the UTF-8 (lossy) file contains the text
/// assert-entry <name>        fail unless the focused pane's listing has that entry
/// assert-selection <a>, <b>  fail unless exactly those entries are selected
/// assert-no-entry <name>     fail if the focused pane's listing has that entry
/// wait-entry <name> [| s]    wait until the focused pane lists the entry (no manual refresh)
/// wait-no-entry <name> [| s] wait until the focused pane no longer lists the entry
/// assert-toast <text>        fail unless some toast so far contained the text
/// assert-no-error-toast      fail if any error toast was shown so far
/// assert-notice <kind>       crash | welcome | none: which startup notices are open
/// dismiss-notices            close the startup notices as the user would
/// ui <area> <verb> [args]    area-specific verbs: viewer, ingest, library, tools, perf,
///                            browser, media, related, sidebar, system
///                            (documented in the area modules under `testhook/`)
/// ```
#[derive(Debug, Clone)]
enum Step {
    Navigate(PathBuf),
    Select(Vec<String>),
    SelectAll,
    Focus(Side),
    Cmd(Cmd),
    Type(String),
    Accept,
    Cancel,
    Conflict {
        decision: ConflictDecision,
        all: bool,
    },
    WaitJobs(Duration),
    WaitFrames(u32),
    Screenshot(PathBuf),
    AssertTabs(usize),
    /// Number of selected entries the focused pane lists.
    AssertSelected(usize),
    /// The exact set of selected entry names, case-insensitive.
    AssertSelection(Vec<String>),
    AssertPath(PathBuf),
    AssertExists(PathBuf),
    AssertMissing(PathBuf),
    Exit,
    Limit(Duration),
    Sleep(Duration),
    /// Pre-built press/release events (delivered in one frame).
    Key(Vec<egui::Event>),
    Text(String),
    Pointer(egui::Pos2),
    Click(egui::Pos2),
    Marquee {
        from: String,
        to: String,
        modifiers: egui::Modifiers,
    },
    ReleasePointer,
    /// Real pointer clicks on a listed entry's row or tile (`double` = two clicks,
    /// `secondary` = one right-click).
    ClickEntry {
        name: String,
        double: bool,
        secondary: bool,
    },
    /// A real click on the list area below the last row or tile.
    ClickEmpty,
    Scroll(f32),
    Dialog(Option<PathBuf>),
    WaitExists(PathBuf, Duration),
    WaitMissing(PathBuf, Duration),
    AssertSame(PathBuf, PathBuf),
    AssertContains(PathBuf, String),
    AssertEntry(String),
    AssertNoEntry(String),
    WaitEntry(String, Duration),
    WaitNoEntry(String, Duration),
    AssertToast(String),
    AssertNoErrorToast,
    AssertNotice(String),
    DismissNotices,
    Ui {
        area: Area,
        verb: String,
        args: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Area {
    Viewer,
    Ingest,
    Library,
    Tools,
    Perf,
    Browser,
    Media,
    Related,
    Sidebar,
    System,
}

pub struct Script {
    steps: Vec<(usize, Step)>,
    next: usize,
    result: PathBuf,
    begun: Instant,
    limit: Duration,
    warmup_frames: u32,
    step: StepState,
    done: bool,
}

/// Per-step scratch state; reset when a step completes. Area modules may use any `pub` field.
#[derive(Default)]
pub(crate) struct StepState {
    pub started: Option<Instant>,
    pub frames: u32,
    shot_requested: bool,
    batch_requested: bool,
    /// Free-form phase counter for multi-frame steps.
    pub phase: u32,
    /// Free-form per-step data for area modules.
    #[allow(dead_code)]
    pub data: Option<Box<dyn std::any::Any>>,
}

pub(crate) enum Flow {
    Done,
    Exit,
    Wait(String),
}

impl Script {
    /// Reads and parses the script. A bad script stops the process before the window opens.
    fn load(path: PathBuf) -> Self {
        let mut result = path.clone().into_os_string();
        result.push(".result");
        let result = std::env::var_os("ZXRAW_SCRIPT_RESULT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(result));
        // A result file left by an earlier run must not be mistaken for this run's outcome.
        let _ = std::fs::remove_file(&result);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => fail(
                &result,
                format!("cannot read script {}: {e}", path.display()),
            ),
        };
        let sandbox_root = match resolve_sandbox_root(&path) {
            Ok(root) => root,
            Err(error) => fail(&result, error),
        };
        let _ = SANDBOX_ROOT.set(sandbox_root.clone());
        match parse_script(&text, &sandbox_root) {
            Ok(steps) => Self {
                steps,
                next: 0,
                result,
                begun: Instant::now(),
                limit: SCRIPT_LIMIT,
                warmup_frames: 0,
                step: StepState::default(),
                done: false,
            },
            Err(e) => fail(&result, e),
        }
    }
}

fn resolve_sandbox_root(script_path: &Path) -> Result<PathBuf, String> {
    let script_dir = script_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let requested = std::env::var_os("ZXRAW_SCRIPT_SANDBOX")
        .map(PathBuf::from)
        .unwrap_or_else(|| script_dir.to_path_buf());
    let requested = if requested.is_absolute() {
        requested
    } else {
        script_dir.join(requested)
    };
    let root = std::fs::canonicalize(&requested).map_err(|error| {
        format!(
            "cannot resolve script sandbox {}: {error}",
            requested.display()
        )
    })?;
    if !root.is_dir() {
        return Err(format!(
            "script sandbox is not a directory: {}",
            root.display()
        ));
    }
    Ok(root)
}

/// Resolves through the nearest existing ancestor so symlinks and junctions cannot make a
/// lexically contained target escape the sandbox.
fn sandbox_path(root: &Path, requested: &Path) -> Result<PathBuf, String> {
    if requested
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "path {} contains a parent-directory component",
            requested.display()
        ));
    }
    // Compare against the canonical root: on Windows `canonicalize` yields `\\?\` paths
    // (and normalised case), so a raw root such as a tempdir would never prefix-match.
    let root = std::fs::canonicalize(root)
        .map_err(|error| format!("cannot resolve script sandbox {}: {error}", root.display()))?;
    let root = root.as_path();
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let mut ancestor = candidate.as_path();
    let mut missing = Vec::<std::ffi::OsString>::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                if resolved.starts_with(root) {
                    // Contained: hand the app the ordinary form. `canonicalize` yields
                    // verbatim `\\?\` paths, which the app would otherwise store and show.
                    return Ok(without_verbatim_prefix(resolved));
                }
                return Err(format!(
                    "path {} is outside script sandbox {}",
                    requested.display(),
                    root.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Something exists here but doesn't resolve: a dangling link or junction.
                // A later write through it would land wherever it points, so refuse.
                if std::fs::symlink_metadata(ancestor).is_ok() {
                    return Err(format!(
                        "path {} goes through an unresolvable link ({})",
                        requested.display(),
                        ancestor.display()
                    ));
                }
                let Some(name) = ancestor.file_name() else {
                    return Err(format!(
                        "cannot verify path {} is inside script sandbox {}: {error}",
                        requested.display(),
                        root.display()
                    ));
                };
                missing.push(name.to_os_string());
                let Some(parent) = ancestor.parent() else {
                    return Err(format!(
                        "cannot verify path {} is inside script sandbox {}: {error}",
                        requested.display(),
                        root.display()
                    ));
                };
                if parent == ancestor {
                    return Err(format!(
                        "cannot verify path {} is inside script sandbox {}: {error}",
                        requested.display(),
                        root.display()
                    ));
                }
                ancestor = parent;
            }
            Err(error) => {
                return Err(format!(
                    "cannot verify path {} is inside script sandbox {}: {error}",
                    requested.display(),
                    root.display()
                ));
            }
        }
    }
}

/// Screenshots go inside the script sandbox, or into `ZXRAW_SCREENSHOT_DIR` when it is set
/// (PNG files only), so test runs can collect their captures in one place.
fn screenshot_path(sandbox_root: &Path, rest: &str) -> Result<PathBuf, String> {
    if rest.is_empty() {
        return Err("expected a path".into());
    }
    let requested = Path::new(rest);
    let is_png = requested
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("png"));
    if is_png
        && let Some(dir) = std::env::var_os("ZXRAW_SCREENSHOT_DIR")
        && let Ok(path) = sandbox_path(Path::new(&dir), requested)
    {
        return Ok(path);
    }
    sandbox_path(sandbox_root, requested)
}

/// Where a step may write a JSON report (e.g. `ui perf end`): inside the script sandbox,
/// or under `ZXRAW_SCREENSHOT_DIR`, the same collection folder screenshots may use.
pub(crate) fn report_path(requested: &Path) -> Result<PathBuf, String> {
    let is_json = requested
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if is_json
        && let Some(dir) = std::env::var_os("ZXRAW_SCREENSHOT_DIR")
        && let Ok(path) = sandbox_path(Path::new(&dir), requested)
    {
        return Ok(path);
    }
    sandboxed_entry(requested)
}

/// `\\?\C:\x` → `C:\x` and `\\?\UNC\server\share` → `\\server\share`; other paths unchanged.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

static SANDBOX_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Resolves a path argument of an area verb inside the script sandbox. Every verb that
/// writes, renames or deletes files must use this (or only touch the profile/app state).
#[allow(dead_code)] // used by area verbs that touch paths
pub(crate) fn sandboxed(requested: &Path) -> Result<PathBuf, String> {
    let root = SANDBOX_ROOT.get().ok_or("no script sandbox is active")?;
    sandbox_path(root, requested)
}

/// [`sandboxed`] for a path whose last component must not be resolved, because the step
/// creates, renames or deletes that entry itself (it may be a link): the parent folder is
/// resolved and checked, and the name is kept as given.
pub(crate) fn sandboxed_entry(requested: &Path) -> Result<PathBuf, String> {
    let name = requested
        .file_name()
        .ok_or_else(|| format!("{} has no file name", requested.display()))?;
    let parent = requested.parent().unwrap_or(Path::new(""));
    Ok(sandboxed(parent)?.join(name))
}

/// Recycle Bin commands only run while the automated recycle sandbox is active, so a
/// script can never reach the user's real Recycle Bin (checked when the step runs).
fn script_command_allowed(command: Cmd) -> bool {
    !format!("{command:?}")
        .to_ascii_lowercase()
        .contains("recycle")
        || forge_win::recycle_sandbox_active()
}

/// Parses a script. Errors name the 1-based line number. Paths are resolved inside
/// `sandbox_root` (see [`sandbox_path`]); names and free text are not paths.
fn parse_script(text: &str, sandbox_root: &Path) -> Result<Vec<(usize, Step)>, String> {
    let mut steps = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let raw = raw.trim_end_matches('\r');
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (verb, rest) = match trimmed.split_once(char::is_whitespace) {
            Some((verb, rest)) => (verb, rest.trim()),
            None => (trimmed, ""),
        };
        // `type` keeps the text verbatim (leading and trailing spaces are part of it).
        let verbatim = &raw.trim_start()[verb.len()..];
        let verbatim = verbatim.strip_prefix(' ').unwrap_or(verbatim);
        let step = parse_step(verb, rest, verbatim, sandbox_root)
            .map_err(|e| format!("line {line_no} (`{verb}`): {e}"))?;
        steps.push((line_no, step));
    }
    Ok(steps)
}

fn parse_step(verb: &str, rest: &str, verbatim: &str, sandbox_root: &Path) -> Result<Step, String> {
    let path = || {
        if rest.is_empty() {
            Err("expected a path".to_string())
        } else {
            sandbox_path(sandbox_root, Path::new(rest))
        }
    };
    let inside = |p: &Path| sandbox_path(sandbox_root, p);
    let number = |what: &str| {
        rest.parse::<u64>()
            .map_err(|_| format!("expected {what}, got `{rest}`"))
    };
    Ok(match verb {
        "navigate" => Step::Navigate(path()?),
        "select" => {
            let names: Vec<String> = rest
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            if names.is_empty() {
                return Err("expected at least one name".into());
            }
            Step::Select(names)
        }
        "select-all" => Step::SelectAll,
        "focus" => match rest {
            "left" => Step::Focus(Side::Left),
            "right" => Step::Focus(Side::Right),
            _ => return Err("expected `left` or `right`".into()),
        },
        "cmd" => Step::Cmd(
            Cmd::ALL
                .iter()
                .find(|c| format!("{c:?}") == rest)
                .copied()
                .ok_or_else(|| format!("unknown command `{rest}`"))?,
        ),
        "type" => Step::Type(verbatim.to_string()),
        "accept" => Step::Accept,
        "cancel" => Step::Cancel,
        "exit" => Step::Exit,
        "conflict" => {
            let mut parts = rest.split_whitespace();
            let decision = match parts.next() {
                Some("keep-both") => ConflictDecision::KeepBoth,
                Some("skip") => ConflictDecision::Skip,
                Some("replace") => ConflictDecision::Overwrite,
                Some("replace-if-newer") => ConflictDecision::OverwriteIfNewer,
                Some("cancel") => ConflictDecision::Cancel,
                other => return Err(format!("unknown conflict decision {other:?}")),
            };
            let all = match parts.next() {
                None => false,
                Some("all") => true,
                Some(other) => return Err(format!("expected `all`, got `{other}`")),
            };
            if parts.next().is_some() {
                return Err("too many arguments".into());
            }
            Step::Conflict { decision, all }
        }
        "wait-jobs" => {
            let secs = if rest.is_empty() {
                30
            } else {
                number("seconds")?
            };
            Step::WaitJobs(Duration::from_secs(secs))
        }
        "wait-frames" => Step::WaitFrames(number("a frame count")? as u32),
        "screenshot" => Step::Screenshot(screenshot_path(sandbox_root, rest)?),
        "assert-tabs" => Step::AssertTabs(number("a tab count")? as usize),
        "assert-selected" => Step::AssertSelected(number("a selection count")? as usize),
        "assert-selection" => {
            let names: Vec<String> = rest
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(String::from)
                .collect();
            if names.is_empty() {
                return Err("expected at least one selected entry name".into());
            }
            Step::AssertSelection(names)
        }
        "assert-path" => Step::AssertPath(path()?),
        "assert-exists" => Step::AssertExists(path()?),
        "assert-missing" => Step::AssertMissing(path()?),
        "limit" => {
            let secs = number("seconds")?;
            if !(1..=600).contains(&secs) {
                return Err("the limit must be 1..=600 seconds".into());
            }
            Step::Limit(Duration::from_secs(secs))
        }
        "sleep" => Step::Sleep(Duration::from_millis(number("milliseconds")?)),
        "key" => Step::Key(parse_key(rest)?),
        "text" => Step::Text(verbatim.to_string()),
        "pointer" => Step::Pointer(parse_pos(rest)?),
        "click" => Step::Click(parse_pos(rest)?),
        "marquee" => {
            let mut args = rest.split_whitespace();
            let from = args
                .next()
                .ok_or("expected a start and end entry")?
                .to_string();
            let to = args
                .next()
                .ok_or("expected a start and end entry")?
                .to_string();
            let mut modifiers = egui::Modifiers::NONE;
            match args.next() {
                None => {}
                Some("ctrl") => {
                    modifiers.ctrl = true;
                    modifiers.command = true;
                }
                Some("shift") => modifiers.shift = true,
                Some(other) => return Err(format!("expected `ctrl` or `shift`, got `{other}`")),
            }
            if args.next().is_some() {
                return Err("too many marquee arguments".into());
            }
            Step::Marquee {
                from,
                to,
                modifiers,
            }
        }
        "release-pointer" => {
            if !rest.is_empty() {
                return Err("release-pointer takes no arguments".into());
            }
            Step::ReleasePointer
        }
        "click-entry" | "double-click-entry" | "right-click-entry" => Step::ClickEntry {
            name: non_empty(rest)?,
            double: verb == "double-click-entry",
            secondary: verb == "right-click-entry",
        },
        "click-empty" => Step::ClickEmpty,
        "scroll" => Step::Scroll(
            rest.parse::<f32>()
                .map_err(|_| format!("expected a number, got `{rest}`"))?,
        ),
        "dialog" => match rest {
            "" => return Err("expected a path or `cancel`".into()),
            "cancel" => Step::Dialog(None),
            _ => Step::Dialog(Some(inside(Path::new(rest))?)),
        },
        "wait-exists" => {
            let (p, d) = path_and_secs(rest)?;
            Step::WaitExists(inside(&p)?, d)
        }
        "wait-missing" => {
            let (p, d) = path_and_secs(rest)?;
            Step::WaitMissing(inside(&p)?, d)
        }
        "assert-same" => {
            let (a, b) = two_parts(rest)?;
            Step::AssertSame(inside(Path::new(a))?, inside(Path::new(b))?)
        }
        "assert-contains" => {
            let (a, b) = two_parts(rest)?;
            Step::AssertContains(inside(Path::new(a))?, b.to_string())
        }
        "assert-entry" => Step::AssertEntry(non_empty(rest)?),
        "assert-no-entry" => Step::AssertNoEntry(non_empty(rest)?),
        "wait-entry" => {
            let (p, d) = path_and_secs(rest)?;
            Step::WaitEntry(p.to_string_lossy().into_owned(), d)
        }
        "wait-no-entry" => {
            let (p, d) = path_and_secs(rest)?;
            Step::WaitNoEntry(p.to_string_lossy().into_owned(), d)
        }
        "assert-toast" => Step::AssertToast(non_empty(rest)?),
        "assert-no-error-toast" => Step::AssertNoErrorToast,
        "assert-notice" => match rest {
            "crash" | "welcome" | "none" => Step::AssertNotice(rest.to_string()),
            _ => return Err("expected `crash`, `welcome` or `none`".into()),
        },
        "dismiss-notices" => Step::DismissNotices,
        "ui" => {
            let (area, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let area = match area {
                "viewer" => Area::Viewer,
                "ingest" => Area::Ingest,
                "library" => Area::Library,
                "tools" => Area::Tools,
                "perf" => Area::Perf,
                "browser" => Area::Browser,
                "media" => Area::Media,
                "related" => Area::Related,
                "sidebar" => Area::Sidebar,
                "system" => Area::System,
                other => return Err(format!("unknown area `{other}`")),
            };
            let rest = rest.trim();
            let (verb, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if verb.is_empty() {
                return Err("expected a verb after the area".into());
            }
            Step::Ui {
                area,
                verb: verb.to_string(),
                args: args.trim().to_string(),
            }
        }
        other => return Err(format!("unknown step `{other}`")),
    })
}

fn non_empty(rest: &str) -> Result<String, String> {
    if rest.is_empty() {
        Err("expected an argument".into())
    } else {
        Ok(rest.to_string())
    }
}

/// `<a> | <b>` with both parts non-empty.
fn two_parts(rest: &str) -> Result<(&str, &str), String> {
    match rest.split_once(" | ") {
        Some((a, b)) if !a.trim().is_empty() && !b.trim().is_empty() => Ok((a.trim(), b.trim())),
        _ => Err("expected `<first> | <second>`".into()),
    }
}

/// `<path>` or `<path> | <seconds>`.
fn path_and_secs(rest: &str) -> Result<(PathBuf, Duration), String> {
    let (path, secs) = match rest.rsplit_once(" | ") {
        Some((p, s)) => (
            p.trim(),
            s.trim()
                .parse::<u64>()
                .map_err(|_| format!("expected seconds after `|`, got `{s}`"))?,
        ),
        None => (rest, 30),
    };
    if path.is_empty() {
        return Err("expected a path or name".into());
    }
    Ok((PathBuf::from(path), Duration::from_secs(secs)))
}

fn parse_pos(rest: &str) -> Result<egui::Pos2, String> {
    let mut it = rest.split_whitespace().map(str::parse::<f32>);
    match (it.next(), it.next(), it.next()) {
        (Some(Ok(x)), Some(Ok(y)), None) => Ok(egui::pos2(x, y)),
        _ => Err(format!("expected `<x> <y>`, got `{rest}`")),
    }
}

/// `ctrl+shift+F` → a press and a release event for `F` with those modifiers.
fn parse_key(rest: &str) -> Result<Vec<egui::Event>, String> {
    if rest.is_empty() {
        return Err("expected a key".into());
    }
    let mut modifiers = egui::Modifiers::NONE;
    let parts: Vec<&str> = rest.split('+').map(str::trim).collect();
    let (name, mods) = parts.split_last().ok_or("expected a key")?;
    for m in mods {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" => {
                modifiers.ctrl = true;
                modifiers.command = true;
            }
            "shift" => modifiers.shift = true,
            "alt" => modifiers.alt = true,
            other => return Err(format!("unknown modifier `{other}`")),
        }
    }
    let key = egui::Key::from_name(name).ok_or_else(|| format!("unknown key `{name}`"))?;
    Ok([true, false]
        .into_iter()
        .map(|pressed| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers,
        })
        .collect())
}

/// Key events as egui-winit delivers them from a real keyboard: Ctrl+X/C/V (and the
/// Shift+Delete, Ctrl+Insert, Shift+Insert forms) become `Cut`, `Copy` and `Paste` with no key
/// press, and a paste is reported only when the clipboard holds text (its content is not
/// used). Also returns the number of paste presses the Win32 message hook would see.
fn as_winit_delivers(events: &[egui::Event], clipboard_has_text: bool) -> (Vec<egui::Event>, u32) {
    use egui::Key;
    let mut out = Vec::new();
    let mut pastes = 0;
    for event in events {
        let egui::Event::Key {
            key,
            pressed: true,
            modifiers: m,
            ..
        } = event
        else {
            out.push(event.clone());
            continue;
        };
        let (key, m) = (*key, *m);
        if (m.command && key == Key::X) || (m.shift && key == Key::Delete) {
            out.push(egui::Event::Cut);
        } else if (m.command && key == Key::C) || (m.ctrl && key == Key::Insert) {
            out.push(egui::Event::Copy);
        } else if (m.command && key == Key::V) || (m.shift && key == Key::Insert) {
            // The message hook counts exactly Ctrl+V and Shift+Insert.
            let exact = !m.alt
                && ((key == Key::V && !m.shift) || (key == Key::Insert && !m.ctrl));
            if exact {
                pastes += 1;
            }
            if clipboard_has_text {
                out.push(egui::Event::Paste("text".into()));
            }
        } else {
            out.push(event.clone());
        }
    }
    (out, pastes)
}

fn script_tick(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(mut script) = app.testhook.script.take() else {
        return;
    };
    if !script.done {
        match advance(app, ctx, &mut script) {
            Ok(true) => {
                write_result(&script.result, "OK");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                script.done = true;
            }
            Ok(false) => {}
            Err(message) => fail(&script.result, message),
        }
    }
    app.testhook.script = Some(script);
    ctx.request_repaint();
}

/// Runs at most one step. Returns `Ok(true)` when the script has finished.
fn advance(app: &mut ZxrawApp, ctx: &egui::Context, script: &mut Script) -> Result<bool, String> {
    if script.begun.elapsed() > script.limit {
        let at = script
            .steps
            .get(script.next)
            .map(|(line, _)| format!(" (at line {line})"))
            .unwrap_or_default();
        return Err(format!(
            "script ran longer than {} s{at}",
            script.limit.as_secs()
        ));
    }
    // Let the first folder listing and session restore start before the first step.
    if script.warmup_frames < 5 {
        script.warmup_frames += 1;
        return Ok(false);
    }
    let Some((line, step)) = script.steps.get(script.next).cloned() else {
        return Ok(true);
    };
    let limit = match &step {
        Step::WaitJobs(limit)
        | Step::WaitExists(_, limit)
        | Step::WaitMissing(_, limit)
        | Step::WaitEntry(_, limit)
        | Step::WaitNoEntry(_, limit) => *limit,
        Step::Sleep(d) => *d + STEP_LIMIT,
        // Area steps enforce their own limits where they need longer; the script limit
        // still applies.
        Step::Ui { .. } => Duration::from_secs(600),
        _ => STEP_LIMIT,
    };
    if let Step::Limit(limit) = step {
        script.limit = limit;
        script.next += 1;
        return Ok(script.next >= script.steps.len());
    }
    if let Some(unexpected) = crate::dialogs::take_unexpected() {
        return Err(format!(
            "line {line}: a native dialog ({unexpected}) was opened without a queued `dialog` answer"
        ));
    }
    let started = *script.step.started.get_or_insert_with(Instant::now);
    match exec(app, ctx, &step, &mut script.step) {
        Ok(Flow::Done) => {
            script.next += 1;
            script.step = StepState::default();
            Ok(script.next >= script.steps.len())
        }
        Ok(Flow::Exit) => Ok(true),
        Ok(Flow::Wait(reason)) if started.elapsed() > limit => Err(format!(
            "line {line} ({step:?}): timed out after {} s; {reason}",
            limit.as_secs()
        )),
        Ok(Flow::Wait(_)) => Ok(false),
        Err(e) => Err(format!("line {line} ({step:?}): {e}")),
    }
}

fn needs_listing(step: &Step) -> bool {
    !matches!(
        step,
        Step::WaitFrames(_)
            | Step::WaitJobs(_)
            | Step::Exit
            | Step::AssertTabs(_)
            | Step::AssertSelected(_)
            | Step::AssertPath(_)
            | Step::AssertExists(_)
            | Step::AssertMissing(_)
            | Step::Limit(_)
            | Step::Sleep(_)
            | Step::Key(_)
            | Step::Text(_)
            | Step::Pointer(_)
            | Step::Click(_)
            | Step::ClickEntry { .. }
            | Step::ClickEmpty
            | Step::ReleasePointer
            | Step::Scroll(_)
            | Step::Dialog(_)
            | Step::WaitExists(..)
            | Step::WaitMissing(..)
            | Step::AssertSame(..)
            | Step::AssertContains(..)
            | Step::WaitEntry(..)
            | Step::WaitNoEntry(..)
            | Step::AssertToast(_)
            | Step::AssertNoErrorToast
            | Step::AssertNotice(_)
            | Step::DismissNotices
            | Step::Ui { .. }
    )
}

/// Queues `events` for the next frame, then waits until a frame has processed them.
/// Uses `st.phase` 0 → 1; callers that need several injections reset it.
/// Set when the run is automated, so the views record where entries are drawn (and pay
/// nothing in an ordinary session).
static RECORD_RECTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn entry_rect_id(path: &Path) -> egui::Id {
    egui::Id::new(("testhook-entry-rect", path))
}

fn view_rect_id() -> egui::Id {
    egui::Id::new("testhook-view-rect")
}

fn note_rect(ui: &egui::Ui, id: egui::Id, rect: egui::Rect) {
    if !RECORD_RECTS.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let visible = rect.intersect(ui.clip_rect());
    if visible.is_positive() {
        let frame = ui.ctx().cumulative_frame_nr();
        ui.ctx().data_mut(|d| d.insert_temp(id, (frame, visible)));
    }
}

/// Records where a row or tile was drawn (the visible part), in points, so scripts can
/// click it with real pointer input at any display scale.
pub(crate) fn note_entry_rect(ui: &egui::Ui, path: &Path, rect: egui::Rect) {
    note_rect(ui, entry_rect_id(path), rect);
}

/// Records where a folder's disclosure arrow was drawn (details view).
pub(crate) fn note_arrow_rect(ui: &egui::Ui, path: &Path, rect: egui::Rect) {
    note_rect(ui, arrow_rect_id(path), rect);
}

/// Records where a named control (a toggle or button) was drawn, for `ui browser click-control`.
pub(crate) fn note_control_rect(ui: &egui::Ui, name: &str, rect: egui::Rect) {
    note_rect(ui, control_rect_id(name), rect);
}

fn control_rect_id(name: &str) -> egui::Id {
    egui::Id::new(("testhook-control-rect", name))
}

fn arrow_rect_id(path: &Path) -> egui::Id {
    egui::Id::new(("testhook-arrow-rect", path))
}

/// Records the file list's area (where a click on empty space lands).
pub(crate) fn note_view_rect(ui: &egui::Ui, rect: egui::Rect) {
    note_rect(ui, view_rect_id(), rect);
}

/// A rect recorded during the last couple of frames, so a row that scrolled away or
/// a view that closed is not clicked at a stale position.
fn drawn_rect(ctx: &egui::Context, id: egui::Id) -> Option<egui::Rect> {
    let (frame, rect) = ctx.data(|d| d.get_temp::<(u64, egui::Rect)>(id))?;
    (ctx.cumulative_frame_nr().saturating_sub(frame) <= 2).then_some(rect)
}

/// `clicks` real `button` clicks at `pos`, each press and release in its own frame,
/// back to back so two clicks register as a double-click.
fn pointer_clicks(
    app: &mut ZxrawApp,
    st: &mut StepState,
    pos: egui::Pos2,
    clicks: u32,
    button: egui::PointerButton,
) -> Result<Flow, String> {
    app.testhook.input.pointer = Some(pos);
    let pressed = st.frames.is_multiple_of(2);
    let mut events = Vec::new();
    if st.frames == 0 {
        events.push(egui::Event::PointerMoved(pos));
    }
    events.push(egui::Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers: egui::Modifiers::NONE,
    });
    match inject(app, st, &events) {
        Flow::Done if st.frames + 1 < clicks * 2 => {
            st.frames += 1;
            st.phase = 0;
            Ok(Flow::Wait("clicking".into()))
        }
        other => Ok(other),
    }
}

fn marquee_drag(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    st: &mut StepState,
    from: &str,
    to: &str,
    modifiers: egui::Modifiers,
) -> Result<Flow, String> {
    if st.data.is_none() {
        let find_path = |name: &str| {
            app.pane()
                .entries
                .iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(name))
                .map(|entry| entry.path.clone())
                .ok_or_else(|| format!("`{name}` is not listed"))
        };
        let from_path = find_path(from)?;
        let to_path = find_path(to)?;
        let Some(from_rect) = drawn_rect(ctx, entry_rect_id(&from_path)) else {
            return Ok(Flow::Wait(format!("`{from}` is not on screen")));
        };
        let Some(to_rect) = drawn_rect(ctx, entry_rect_id(&to_path)) else {
            return Ok(Flow::Wait(format!("`{to}` is not on screen")));
        };
        let Some(view) = drawn_rect(ctx, view_rect_id()) else {
            return Ok(Flow::Wait("the file list is not drawn yet".into()));
        };
        let start = egui::pos2(from_rect.left() + 8.0, view.bottom() - 6.0);
        let covered = app.pane().entries.iter().any(|entry| {
            drawn_rect(ctx, entry_rect_id(&entry.path)).is_some_and(|rect| rect.contains(start))
        });
        if covered {
            return Err("the list has no empty area below its rows or tiles".into());
        }
        st.data = Some(Box::new((start, to_rect.center(), modifiers)));
    }
    let (start, end, modifiers) = *st
        .data
        .as_ref()
        .unwrap()
        .downcast_ref::<(egui::Pos2, egui::Pos2, egui::Modifiers)>()
        .unwrap();

    match st.frames {
        0 => {
            app.testhook.input.pointer = Some(start);
            let mut events = vec![egui::Event::PointerMoved(start)];
            if modifiers != egui::Modifiers::NONE {
                events.push(egui::Event::ModifiersChanged(modifiers));
                app.testhook.input.modifiers = modifiers;
            }
            events.push(egui::Event::PointerButton {
                pos: start,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers,
            });
            match inject(app, st, &events) {
                Flow::Done => {
                    st.frames = 1;
                    st.phase = 0;
                    Ok(Flow::Wait("marquee button pressed".into()))
                }
                other => Ok(other),
            }
        }
        1 => {
            app.testhook.input.pointer = Some(end);
            match inject(app, st, &[egui::Event::PointerMoved(end)]) {
                Flow::Done => {
                    st.frames = 2;
                    st.phase = 0;
                    Ok(Flow::Done)
                }
                other => Ok(other),
            }
        }
        _ => Ok(Flow::Done),
    }
}

pub(crate) fn inject(app: &mut ZxrawApp, st: &mut StepState, events: &[egui::Event]) -> Flow {
    let input = &mut app.testhook.input;
    if st.phase == 0 {
        input.pending.extend(events.iter().cloned());
        input.delivered_frames = 0;
        st.phase = 1;
        return Flow::Wait("input queued".into());
    }
    if input.pending.is_empty() && input.delivered_frames >= 1 {
        Flow::Done
    } else {
        Flow::Wait("input not delivered yet".into())
    }
}

fn listed(app: &ZxrawApp, name: &str) -> bool {
    app.pane()
        .entries
        .iter()
        .any(|e| e.name.eq_ignore_ascii_case(name))
}

fn any_loading(app: &ZxrawApp) -> bool {
    app.tabs
        .iter()
        .any(|t| t.left.is_loading() || t.right.as_ref().is_some_and(Pane::is_loading))
}

fn exec(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    step: &Step,
    st: &mut StepState,
) -> Result<Flow, String> {
    if needs_listing(step) && any_loading(app) {
        return Ok(Flow::Wait("a folder listing is still loading".into()));
    }
    match step {
        Step::Navigate(path) => {
            app.navigate(path.clone());
            Ok(Flow::Done)
        }
        Step::Select(names) => {
            select_names(app.pane_mut(), names)?;
            Ok(Flow::Done)
        }
        Step::SelectAll => {
            app.pane_mut().select_all();
            Ok(Flow::Done)
        }
        Step::Focus(side) => {
            let tab = app.tab_mut();
            if *side == Side::Right && tab.right.is_none() {
                return Err("there is no second pane; run `cmd ToggleDualPane` first".into());
            }
            tab.focus = *side;
            Ok(Flow::Done)
        }
        Step::Cmd(cmd) => {
            if !script_command_allowed(*cmd) {
                return Err("Recycle Bin commands need the automated recycle sandbox".into());
            }
            app.run(*cmd, ctx);
            Ok(Flow::Done)
        }
        Step::Type(text) => match app.ops.modal.as_mut() {
            Some(Modal::Prompt(prompt)) => {
                prompt.text = text.clone();
                prompt.error = None;
                Ok(Flow::Done)
            }
            Some(_) => Err("the active dialog has no text field".into()),
            None => Ok(Flow::Wait("no dialog is open".into())),
        },
        Step::Accept => {
            if let Some(batch) = app.ops.batch_rename.as_mut() {
                // The dialog applies on the next frame, through its own Rename button path.
                if !st.batch_requested {
                    batch.auto_apply = true;
                    st.batch_requested = true;
                }
                return Ok(Flow::Wait(
                    "batch rename has not finished (is the Rename button enabled?)".into(),
                ));
            }
            if st.batch_requested {
                return Ok(Flow::Done);
            }
            match app.ops.modal.take() {
                None => Ok(Flow::Wait("no dialog is open".into())),
                Some(modal) => match actions::accept(app, modal) {
                    None => Ok(Flow::Done),
                    Some(modal) => {
                        let why = match &modal {
                            Modal::Prompt(prompt) => prompt.error.clone().unwrap_or_default(),
                            _ => String::new(),
                        };
                        app.ops.modal = Some(modal);
                        Err(format!("the dialog stayed open: {why}"))
                    }
                },
            }
        }
        Step::Cancel => {
            if app.ops.batch_rename.take().is_some() || app.ops.modal.take().is_some() {
                Ok(Flow::Done)
            } else {
                Ok(Flow::Wait("no dialog is open".into()))
            }
        }
        Step::Conflict { decision, all } => match app.ops.conflicts.pop_front() {
            Some(question) => {
                let _ = question.reply.send(Resolution {
                    decision: *decision,
                    apply_to_all: *all,
                });
                Ok(Flow::Done)
            }
            None => Ok(Flow::Wait(
                "no file conflict is waiting for an answer".into(),
            )),
        },
        Step::WaitJobs(_) => match app.ops.jobs.running_count() {
            0 => Ok(Flow::Done),
            n => Ok(Flow::Wait(format!("{n} operation(s) still running"))),
        },
        Step::WaitFrames(frames) => {
            st.frames += 1;
            if st.frames > *frames {
                Ok(Flow::Done)
            } else {
                Ok(Flow::Wait(format!("{} of {frames} frames", st.frames)))
            }
        }
        Step::Screenshot(path) => match screenshot_event(ctx) {
            Some(img) if st.shot_requested => {
                save_png(&img, path)?;
                Ok(Flow::Done)
            }
            _ if st.shot_requested => {
                // A capture request can be lost (e.g. a frame that was never presented while
                // the window was busy or covered): ask again every two seconds rather than
                // waiting out the whole step limit for one request.
                let resend = st
                    .data
                    .as_ref()
                    .and_then(|d| d.downcast_ref::<Instant>())
                    .is_none_or(|at| at.elapsed() > Duration::from_secs(2));
                if resend {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                    st.data = Some(Box::new(Instant::now()));
                }
                ctx.request_repaint();
                Ok(Flow::Wait("waiting for the frame capture".into()))
            }
            _ => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                st.shot_requested = true;
                st.data = Some(Box::new(Instant::now()));
                ctx.request_repaint();
                Ok(Flow::Wait("capture requested".into()))
            }
        },
        Step::AssertSelected(expected) => {
            let found = app.pane().selected_entries().len();
            if found == *expected {
                Ok(Flow::Done)
            } else {
                Err(format!("expected {expected} selected, found {found}"))
            }
        }
        Step::AssertSelection(expected) => {
            let mut expected: Vec<String> =
                expected.iter().map(|name| name.to_lowercase()).collect();
            expected.sort();
            let mut found: Vec<String> = app
                .pane()
                .selected_entries()
                .iter()
                .map(|entry| entry.name.to_lowercase())
                .collect();
            found.sort();
            if found == expected {
                Ok(Flow::Done)
            } else {
                Err(format!("expected selection {expected:?}, found {found:?}"))
            }
        }
        Step::AssertTabs(expected) => {
            let found = app.tabs.len();
            if found == *expected {
                Ok(Flow::Done)
            } else {
                Err(format!("expected {expected} tab(s), found {found}"))
            }
        }
        Step::AssertPath(expected) => {
            let found = app.pane().path.clone();
            if forge_core::paths::paths_equal(&found, expected) {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "expected the focused pane at {}, found {}",
                    expected.display(),
                    found.display()
                ))
            }
        }
        Step::AssertExists(path) => {
            if path.exists() {
                Ok(Flow::Done)
            } else {
                Err(format!("{} does not exist", path.display()))
            }
        }
        Step::AssertMissing(path) => {
            if path.exists() {
                Err(format!("{} still exists", path.display()))
            } else {
                Ok(Flow::Done)
            }
        }
        Step::Exit => Ok(Flow::Exit),
        Step::Limit(_) => Ok(Flow::Done),
        Step::Sleep(d) => {
            let started = *st.started.get_or_insert_with(Instant::now);
            if started.elapsed() >= *d {
                Ok(Flow::Done)
            } else {
                Ok(Flow::Wait("sleeping".into()))
            }
        }
        Step::Key(events) => {
            if st.phase == 0 {
                let (events, pastes) = as_winit_delivers(events, forge_win::clipboard_has_text());
                app.testhook.input.paste_presses += pastes;
                Ok(inject(app, st, &events))
            } else {
                Ok(inject(app, st, &[]))
            }
        }
        Step::Text(text) => Ok(inject(app, st, &[egui::Event::Text(text.clone())])),
        Step::Pointer(pos) => {
            app.testhook.input.pointer = Some(*pos);
            Ok(inject(app, st, &[egui::Event::PointerMoved(*pos)]))
        }
        Step::Click(pos) => {
            app.testhook.input.pointer = Some(*pos);
            let button = |pressed| egui::Event::PointerButton {
                pos: *pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            // Press and release land in different frames, like a real click.
            // `st.frames` counts the injections that have completed.
            let events = if st.frames == 0 {
                vec![egui::Event::PointerMoved(*pos), button(true)]
            } else {
                vec![button(false)]
            };
            match inject(app, st, &events) {
                Flow::Done if st.frames == 0 => {
                    st.frames = 1;
                    st.phase = 0;
                    Ok(Flow::Wait("button pressed".into()))
                }
                other => Ok(other),
            }
        }
        Step::ClickEntry {
            name,
            double,
            secondary,
        } => {
            if st.data.is_none() {
                let Some(path) = app
                    .pane()
                    .entries
                    .iter()
                    .find(|e| e.name.eq_ignore_ascii_case(name))
                    .map(|e| e.path.clone())
                else {
                    return Err(format!("`{name}` is not listed"));
                };
                let Some(rect) = drawn_rect(ctx, entry_rect_id(&path)) else {
                    return Ok(Flow::Wait(format!("`{name}` is not on screen")));
                };
                st.data = Some(Box::new(rect.center()));
            }
            let pos = *st
                .data
                .as_ref()
                .unwrap()
                .downcast_ref::<egui::Pos2>()
                .unwrap();
            let button = if *secondary {
                egui::PointerButton::Secondary
            } else {
                egui::PointerButton::Primary
            };
            pointer_clicks(app, st, pos, if *double { 2 } else { 1 }, button)
        }
        Step::ClickEmpty => {
            if st.data.is_none() {
                let Some(view) = drawn_rect(ctx, view_rect_id()) else {
                    return Ok(Flow::Wait("the file list is not drawn yet".into()));
                };
                // Just above the bottom of the list area, which the rows don't reach in a
                // short listing.
                let pos = egui::pos2(view.center().x, view.bottom() - 6.0);
                let covered = app.pane().entries.iter().any(|e| {
                    drawn_rect(ctx, entry_rect_id(&e.path)).is_some_and(|r| r.contains(pos))
                });
                if covered {
                    return Err("the list has no empty area to click".into());
                }
                st.data = Some(Box::new(pos));
            }
            let pos = *st
                .data
                .as_ref()
                .unwrap()
                .downcast_ref::<egui::Pos2>()
                .unwrap();
            pointer_clicks(app, st, pos, 1, egui::PointerButton::Primary)
        }
        Step::Marquee {
            from,
            to,
            modifiers,
        } => marquee_drag(app, ctx, st, from, to, *modifiers),
        Step::ReleasePointer => {
            let Some(pos) = app.testhook.input.pointer else {
                return Err("there is no pointer position to release".into());
            };
            let mut events = vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }];
            if app.testhook.input.modifiers != egui::Modifiers::NONE {
                events.push(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
                app.testhook.input.modifiers = egui::Modifiers::NONE;
            }
            Ok(inject(app, st, &events))
        }
        Step::Scroll(dy) => {
            let mut events = Vec::new();
            if let Some(pos) = app.testhook.input.pointer {
                events.push(egui::Event::PointerMoved(pos));
            }
            events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -*dy),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            });
            Ok(inject(app, st, &events))
        }
        Step::Dialog(answer) => {
            crate::dialogs::queue_answer(answer.clone());
            Ok(Flow::Done)
        }
        Step::WaitExists(path, _) => Ok(if path.exists() {
            Flow::Done
        } else {
            Flow::Wait(format!("{} does not exist yet", path.display()))
        }),
        Step::WaitMissing(path, _) => Ok(if path.exists() {
            Flow::Wait(format!("{} still exists", path.display()))
        } else {
            Flow::Done
        }),
        Step::AssertSame(a, b) => {
            let read = |p: &PathBuf| {
                std::fs::read(p).map_err(|e| format!("cannot read {}: {e}", p.display()))
            };
            if read(a)? == read(b)? {
                Ok(Flow::Done)
            } else {
                Err(format!("{} and {} differ", a.display(), b.display()))
            }
        }
        Step::AssertContains(path, text) => {
            let bytes =
                std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            if String::from_utf8_lossy(&bytes).contains(text.as_str()) {
                Ok(Flow::Done)
            } else {
                Err(format!("{} does not contain `{text}`", path.display()))
            }
        }
        Step::AssertEntry(name) => {
            if listed(app, name) {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "`{name}` is not listed in {}",
                    app.pane().path.display()
                ))
            }
        }
        Step::AssertNoEntry(name) => {
            if listed(app, name) {
                Err(format!(
                    "`{name}` is still listed in {}",
                    app.pane().path.display()
                ))
            } else {
                Ok(Flow::Done)
            }
        }
        Step::WaitEntry(name, _) => Ok(if listed(app, name) {
            Flow::Done
        } else {
            Flow::Wait(format!("`{name}` is not listed yet"))
        }),
        Step::WaitNoEntry(name, _) => Ok(if listed(app, name) {
            Flow::Wait(format!("`{name}` is still listed"))
        } else {
            Flow::Done
        }),
        Step::AssertToast(text) => {
            if app
                .testhook
                .toast_log
                .iter()
                .any(|(t, _)| t.contains(text.as_str()))
            {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "no toast contained `{text}`; toasts so far: {:?}",
                    app.testhook.toast_log
                ))
            }
        }
        Step::AssertNoErrorToast => {
            let errors: Vec<&String> = app
                .testhook
                .toast_log
                .iter()
                .filter(|(_, e)| *e)
                .map(|(t, _)| t)
                .collect();
            if errors.is_empty() {
                Ok(Flow::Done)
            } else {
                Err(format!("error toast(s) were shown: {errors:?}"))
            }
        }
        Step::AssertNotice(kind) => {
            let n = &app.notices;
            let ok = match kind.as_str() {
                "crash" => n.crash.is_some(),
                "welcome" => n.welcome.is_some(),
                _ => n.crash.is_none() && n.welcome.is_none(),
            };
            if ok {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "expected notice `{kind}`; crash open: {}, welcome open: {}",
                    n.crash.is_some(),
                    n.welcome.is_some()
                ))
            }
        }
        Step::DismissNotices => {
            app.notices.crash = None;
            if app.notices.welcome.is_some() {
                app.notices.mark_welcome_done();
            }
            Ok(Flow::Done)
        }
        Step::Ui { area, verb, args } => match area {
            Area::Viewer => viewer::step(app, ctx, verb, args, st),
            Area::Ingest => ingest::step(app, ctx, verb, args, st),
            Area::Library => library::step(app, ctx, verb, args, st),
            Area::Tools => tools::step(app, ctx, verb, args, st),
            Area::Perf => perf::step(app, ctx, verb, args, st),
            Area::Browser => browser::step(app, ctx, verb, args, st),
            Area::Media => media::step(app, ctx, verb, args, st),
            Area::Related => related::step(app, ctx, verb, args, st),
            Area::Sidebar => sidebar::step(app, ctx, verb, args, st),
            Area::System => system::step(app, ctx, verb, args, st),
        },
    }
}

/// Selects the named entries of `pane`: the first replaces the selection, the rest add to it.
fn select_names(pane: &mut Pane, names: &[String]) -> Result<(), String> {
    for (i, name) in names.iter().enumerate() {
        let visible = pane
            .visible
            .iter()
            .position(|&idx| pane.entries[idx].name.eq_ignore_ascii_case(name));
        let Some(visible) = visible else {
            return Err(format!(
                "no entry named `{name}` in {}",
                pane.path.display()
            ));
        };
        pane.click(visible, i > 0, false);
    }
    Ok(())
}

fn screenshot_event(ctx: &egui::Context) -> Option<std::sync::Arc<egui::ColorImage>> {
    ctx.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        })
    })
}

fn save_png(img: &egui::ColorImage, path: &Path) -> Result<(), String> {
    let [w, h] = img.size;
    let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
    let buf = image::RgbaImage::from_raw(w as u32, h as u32, bytes)
        .ok_or("screenshot buffer size mismatch")?;
    buf.save(path)
        .map_err(|e| format!("screenshot save failed: {e}"))
}

fn write_result(path: &Path, text: &str) {
    if let Err(e) = std::fs::write(path, format!("{text}\n")) {
        tracing::error!("cannot write script result {}: {e}", path.display());
    }
}

/// Reports a script failure on stderr and in the result file, then stops the process.
fn fail(result: &Path, message: impl std::fmt::Display) -> ! {
    let line = format!("SCRIPT ERROR: {message}");
    eprintln!("{line}");
    tracing::error!("{line}");
    write_result(result, &line);
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> Result<Step, String> {
        let sandbox = std::env::temp_dir();
        parse_script(line, &sandbox).map(|mut steps| steps.remove(0).1)
    }

    fn in_temp(rel: &str) -> PathBuf {
        without_verbatim_prefix(std::fs::canonicalize(std::env::temp_dir()).unwrap()).join(rel)
    }

    #[test]
    fn parses_generic_verbs() {
        assert!(matches!(one("limit 120"), Ok(Step::Limit(d)) if d.as_secs() == 120));
        assert!(one("limit 0").is_err() && one("limit 601").is_err());
        assert!(matches!(one("sleep 250"), Ok(Step::Sleep(d)) if d.as_millis() == 250));
        assert!(matches!(one("dialog cancel"), Ok(Step::Dialog(None))));
        assert!(
            matches!(one(r"dialog a b\c"), Ok(Step::Dialog(Some(p))) if p == in_temp(r"a b\c"))
        );
        assert!(
            matches!(one(r"wait-exists x y | 5"), Ok(Step::WaitExists(p, d)) if p == in_temp("x y") && d.as_secs() == 5)
        );
        assert!(
            one(r"dialog ..\escape").is_err(),
            "dialog answers stay inside the sandbox"
        );
        assert!(
            matches!(one("wait-entry a.txt"), Ok(Step::WaitEntry(n, d)) if n == "a.txt" && d.as_secs() == 30)
        );
        assert!(matches!(one("assert-same a | b"), Ok(Step::AssertSame(..))));
        assert!(one("assert-same a").is_err());
        assert!(
            matches!(one("assert-contains r.txt | 2 files"), Ok(Step::AssertContains(_, t)) if t == "2 files")
        );
        assert!(matches!(one("click 10 20.5"), Ok(Step::Click(p)) if p == egui::pos2(10.0, 20.5)));
        assert!(one("click 10").is_err());
        assert!(
            matches!(one("marquee 01.txt 02.txt ctrl"), Ok(Step::Marquee { from, to, modifiers }) if from == "01.txt" && to == "02.txt" && modifiers.ctrl && modifiers.command)
        );
        assert!(
            matches!(one("marquee 01.txt 02.txt shift"), Ok(Step::Marquee { modifiers, .. }) if modifiers.shift)
        );
        assert!(one("marquee 01.txt 02.txt ctrl shift").is_err());
        assert!(matches!(one("release-pointer"), Ok(Step::ReleasePointer)));
        assert!(
            matches!(one("assert-selection a.txt, b.txt"), Ok(Step::AssertSelection(names)) if names == ["a.txt", "b.txt"])
        );
        assert!(matches!(one("scroll -120"), Ok(Step::Scroll(dy)) if dy == -120.0));
        assert!(matches!(one("text  two spaces"), Ok(Step::Text(t)) if t == " two spaces"));
        assert!(matches!(
            one("ui ingest start fast"),
            Ok(Step::Ui { area: Area::Ingest, verb, args }) if verb == "start" && args == "fast"
        ));
        assert!(
            one("ui nowhere start")
                .unwrap_err()
                .contains("unknown area")
        );
        assert!(one("ui viewer").is_err());
    }

    #[test]
    fn parses_key_combos() {
        let Ok(Step::Key(events)) = one("key ctrl+shift+F") else {
            panic!("not a key step");
        };
        assert_eq!(events.len(), 2);
        let egui::Event::Key {
            key,
            pressed,
            modifiers,
            ..
        } = &events[0]
        else {
            panic!("not a key event");
        };
        assert_eq!(*key, egui::Key::F);
        assert!(
            *pressed && modifiers.ctrl && modifiers.command && modifiers.shift && !modifiers.alt
        );
        assert!(matches!(
            &events[1],
            egui::Event::Key { pressed: false, .. }
        ));
        assert!(
            matches!(one("key 3"), Ok(Step::Key(e)) if matches!(e[0], egui::Event::Key { key: egui::Key::Num3, .. }))
        );
        assert!(one("key hyper+X").unwrap_err().contains("modifier"));
        assert!(one("key NotAKey").unwrap_err().contains("unknown key"));
    }

    #[test]
    fn relative_and_missing_paths_are_resolved_inside_the_sandbox() {
        let sandbox = tempfile::tempdir().unwrap();
        let canonical_root =
            without_verbatim_prefix(std::fs::canonicalize(sandbox.path()).unwrap());
        let path = sandbox_path(sandbox.path(), Path::new("new/subfolder")).unwrap();
        assert_eq!(path, canonical_root.join("new/subfolder"));

        let steps =
            parse_script("navigate .\nassert-missing future.txt\n", sandbox.path()).unwrap();
        assert!(matches!(
            &steps[0].1,
            Step::Navigate(path) if path == &canonical_root
        ));
        assert!(matches!(
            &steps[1].1,
            Step::AssertMissing(path) if path == &canonical_root.join("future.txt")
        ));
    }

    #[test]
    fn script_rejects_paths_outside_the_sandbox() {
        let sandbox = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("not-allowed.txt");
        for line in [
            format!("navigate {}", outside.path().display()),
            format!("assert-exists {}", outside_file.display()),
            "assert-missing ../outside.txt".to_owned(),
        ] {
            let error = parse_script(&line, sandbox.path()).unwrap_err();
            assert!(
                error.contains("outside script sandbox")
                    || error.contains("parent-directory component"),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn verbatim_prefixes_are_removed_from_resolved_paths() {
        let p = |s: &str| without_verbatim_prefix(PathBuf::from(s));
        assert_eq!(p(r"\\?\C:\Temp\a b"), PathBuf::from(r"C:\Temp\a b"));
        assert_eq!(p(r"\\?\UNC\srv\share\x"), PathBuf::from(r"\\srv\share\x"));
        assert_eq!(p(r"C:\plain"), PathBuf::from(r"C:\plain"));
        assert_eq!(p(r"\\?\Volume{abc}\x"), PathBuf::from(r"\\?\Volume{abc}\x"));
        let sandbox = tempfile::tempdir().unwrap();
        let resolved = sandbox_path(sandbox.path(), Path::new("sub")).unwrap();
        assert!(
            !resolved.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            resolved.display()
        );
    }

    #[cfg(windows)]
    #[test]
    fn dangling_junctions_inside_the_sandbox_are_refused() {
        let sandbox = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("gone");
        std::fs::create_dir(&target).unwrap();
        let link = sandbox.path().join("link");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .is_ok_and(|o| o.status.success());
        if !made {
            eprintln!("mklink unavailable; skipping");
            return;
        }
        std::fs::remove_dir(&target).unwrap();
        let err = sandbox_path(sandbox.path(), &link.join("file.txt")).unwrap_err();
        // Refused either way: as a dangling link, or because it can't be resolved at all.
        assert!(
            err.contains("unresolvable link") || err.contains("cannot verify"),
            "{err}"
        );
    }

    #[test]
    fn recycle_commands_need_the_recycle_sandbox() {
        forge_win::set_recycle_sandbox(None);
        assert!(!script_command_allowed(Cmd::Recycle));
        assert!(script_command_allowed(Cmd::Copy));
        forge_win::set_recycle_sandbox(Some(std::env::temp_dir().join("zxraw-hook-test-bin")));
        assert!(script_command_allowed(Cmd::Recycle));
        forge_win::set_recycle_sandbox(None);
    }

    #[test]
    fn errors_name_the_line() {
        let err =
            parse_script("# comment\n\nnavigate .\nfrobnicate", &std::env::temp_dir()).unwrap_err();
        assert!(err.starts_with("line 4"), "{err}");
    }
}
