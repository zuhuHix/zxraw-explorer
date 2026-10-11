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
//!   automated run without it uses `%TEMP%\zxraw-test-profile`, never the user's profile.
//!
//! Timing: commands run once the first listing finishes (or after 30 s regardless); the
//! capture happens ~90 frames *after* the commands; a run is closed after 120 s at most.
//! Script steps run one per frame, after every pane has finished loading, except waiting
//! steps which block until their condition holds (each step gives up after 30 s).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use forge_core::ops::{ConflictDecision, Resolution};

use crate::actions;
use crate::app::{Side, ZxrawApp};
use crate::commands::Cmd;
use crate::modal::Modal;
use crate::pane::Pane;

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
}

/// True when the environment asks for an automated run (checked before the app starts so
/// the profile can be isolated).
pub fn env_requests_automation() -> bool {
    ["ZXRAW_SCREENSHOT", "ZXRAW_CMDS", "ZXRAW_SELECT", "ZXRAW_SCRIPT"]
        .iter()
        .any(|v| std::env::var_os(v).is_some())
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
        Self {
            screenshot: std::env::var_os("ZXRAW_SCREENSHOT").map(PathBuf::from),
            cmds,
            select: std::env::var("ZXRAW_SELECT").ok(),
            script: std::env::var_os("ZXRAW_SCRIPT").map(|path| Script::load(PathBuf::from(path))),
            ..Default::default()
        }
    }

    pub fn active(&self) -> bool {
        self.script.is_some()
            || self.screenshot.is_some()
            || !self.cmds.is_empty()
            || self.select.is_some()
    }
}

pub fn tick(app: &mut ZxrawApp, ctx: &egui::Context) {
    if !app.testhook.active() {
        return;
    }
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
    if hook.ran_cmds && hook.frames_after_cmds > 90 && !hook.requested {
        if hook.screenshot.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            hook.requested = true;
        }
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
    AssertPath(PathBuf),
    AssertExists(PathBuf),
    AssertMissing(PathBuf),
    Exit,
}

pub struct Script {
    steps: Vec<(usize, Step)>,
    next: usize,
    result: PathBuf,
    begun: Instant,
    warmup_frames: u32,
    step: StepState,
    done: bool,
}

#[derive(Default)]
struct StepState {
    started: Option<Instant>,
    frames: u32,
    shot_requested: bool,
    batch_requested: bool,
}

enum Flow {
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
            Err(e) => fail(&result, format!("cannot read script {}: {e}", path.display())),
        };
        match parse_script(&text) {
            Ok(steps) => Self {
                steps,
                next: 0,
                result,
                begun: Instant::now(),
                warmup_frames: 0,
                step: StepState::default(),
                done: false,
            },
            Err(e) => fail(&result, e),
        }
    }
}

/// Parses a script. Errors name the 1-based line number.
fn parse_script(text: &str) -> Result<Vec<(usize, Step)>, String> {
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
        let step = parse_step(verb, rest, verbatim)
            .map_err(|e| format!("line {line_no} (`{verb}`): {e}"))?;
        steps.push((line_no, step));
    }
    Ok(steps)
}

fn parse_step(verb: &str, rest: &str, verbatim: &str) -> Result<Step, String> {
    let path = || {
        if rest.is_empty() {
            Err("expected a path".to_string())
        } else {
            Ok(PathBuf::from(rest))
        }
    };
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
            let secs = if rest.is_empty() { 30 } else { number("seconds")? };
            Step::WaitJobs(Duration::from_secs(secs))
        }
        "wait-frames" => Step::WaitFrames(number("a frame count")? as u32),
        "screenshot" => Step::Screenshot(path()?),
        "assert-tabs" => Step::AssertTabs(number("a tab count")? as usize),
        "assert-path" => Step::AssertPath(path()?),
        "assert-exists" => Step::AssertExists(path()?),
        "assert-missing" => Step::AssertMissing(path()?),
        other => return Err(format!("unknown step `{other}`")),
    })
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
    if script.begun.elapsed() > SCRIPT_LIMIT {
        let at = script
            .steps
            .get(script.next)
            .map(|(line, _)| format!(" (at line {line})"))
            .unwrap_or_default();
        return Err(format!("script ran longer than {} s{at}", SCRIPT_LIMIT.as_secs()));
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
        Step::WaitJobs(limit) => *limit,
        _ => STEP_LIMIT,
    };
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
            | Step::AssertPath(_)
            | Step::AssertExists(_)
            | Step::AssertMissing(_)
    )
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
            None => Ok(Flow::Wait("no file conflict is waiting for an answer".into())),
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
            _ if st.shot_requested => Ok(Flow::Wait("waiting for the frame capture".into())),
            _ => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                st.shot_requested = true;
                Ok(Flow::Wait("capture requested".into()))
            }
        },
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
            return Err(format!("no entry named `{name}` in {}", pane.path.display()));
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
    buf.save(path).map_err(|e| format!("screenshot save failed: {e}"))
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
