//! Find and replace in files: a window that previews every match before anything is changed.
//!
//! Safety rules:
//! * Nothing is written until the user has run a preview and the options still match it.
//! * Only the files ticked in the preview are changed, and [`forge_core::text_replace::apply`]
//!   refuses any file whose bytes changed since the preview was made.
//! * Every replaced file keeps a backup in `replace-backups/<timestamp>` under the profile data
//!   folder. The replace is undoable (Ctrl+Z or the "Undo replace" button), and undo refuses
//!   files edited since. Backups older than 30 days are removed when the window opens.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};
use crossbeam_channel::Sender;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText};
use forge_core::ops::JobCtx;
use forge_core::text_replace::{
    self, ApplyReport, FilePreview, ReplaceUndo, ScanOptions, ScanReport, SearchSpec,
};
use forge_jobs::JobHandle;
use parking_lot::Mutex;

use crate::actions::{Event, UndoEntry};
use crate::app::ZxrawApp;
use crate::tools_ui::register_click_target;

/// Height of one row of the preview list. Every row is this tall, so the list can be
/// virtualised.
const ROW_HEIGHT: f32 = 22.0;
/// Backup folders older than this are removed when the window opens.
const BACKUP_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const OLD_BACKGROUND: Color32 = Color32::from_rgba_premultiplied(81, 25, 25, 90);
const NEW_BACKGROUND: Color32 = Color32::from_rgba_premultiplied(25, 67, 35, 90);

static BACKUP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Every setting in the form. A preview is current only while the form equals the options it
/// was made with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    find: String,
    replace: String,
    regex: bool,
    case_sensitive: bool,
    whole_word: bool,
    recursive: bool,
    include: String,
    exclude: String,
    max_mib: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            find: String::new(),
            replace: String::new(),
            regex: false,
            case_sensitive: false,
            whole_word: false,
            recursive: true,
            include: String::new(),
            exclude: String::new(),
            max_mib: "16".into(),
        }
    }
}

impl Options {
    fn search_spec(&self) -> SearchSpec {
        SearchSpec {
            pattern: self.find.clone(),
            replacement: self.replace.clone(),
            regex: self.regex,
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
        }
    }

    fn scan_options(&self) -> Result<ScanOptions, String> {
        let mib = self
            .max_mib
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                "Skip files larger than must be a whole number of MiB above 0.".to_string()
            })?;
        Ok(ScanOptions {
            include: ScanOptions::parse_patterns(&self.include),
            exclude: ScanOptions::parse_patterns(&self.exclude),
            recursive: self.recursive,
            max_file_size: mib.saturating_mul(1024 * 1024),
        })
    }

    /// Validates the whole form: the regex (or empty search text) and the size limit.
    fn check(&self) -> Result<ScanOptions, String> {
        self.search_spec()
            .compile()
            .map_err(|error| error.to_string())?;
        self.scan_options()
    }
}

/// True when a preview made with `previewed` no longer describes what the form asks for.
fn is_stale(previewed: &Options, current: &Options) -> bool {
    previewed != current
}

type Outcome<T> = Arc<Mutex<Option<Result<T, String>>>>;

struct ScanRun {
    handle: JobHandle,
    outcome: Outcome<ScanReport>,
    options: Options,
}

struct ApplyRun {
    handle: JobHandle,
    outcome: Outcome<ApplyReport>,
}

/// A finished preview, the files the user has unticked, and the row list drawn from the result.
struct Preview {
    options: Options,
    result: Result<ScanReport, String>,
    unticked: HashSet<PathBuf>,
    rows: Vec<PreviewRow>,
}

impl Preview {
    /// Stores a finished preview. The rows are built here, once, so drawing only indexes them.
    fn new(options: Options, result: Result<ScanReport, String>) -> Self {
        let rows = result.as_ref().map(preview_rows).unwrap_or_default();
        Self {
            options,
            result,
            unticked: HashSet::new(),
            rows,
        }
    }
}

/// The form's validation result, kept for the options it was computed from. `check` compiles
/// the regex, so it runs only when the options change, not every frame.
#[derive(Default)]
struct Validation {
    cached: Option<(Options, Result<ScanOptions, String>)>,
    /// How many times `check` has run. Tests use it to prove the cache is hit.
    #[cfg(test)]
    computations: usize,
}

impl Validation {
    fn get(&mut self, options: &Options) -> &Result<ScanOptions, String> {
        let fresh = matches!(&self.cached, Some((key, _)) if key == options);
        if !fresh {
            #[cfg(test)]
            {
                self.computations += 1;
            }
            self.cached = Some((options.clone(), options.check()));
        }
        &self.cached.as_ref().expect("filled above").1
    }
}

#[derive(Default)]
struct ReplaceState {
    open: bool,
    roots: Vec<PathBuf>,
    options: Options,
    validation: Validation,
    scan: Option<ScanRun>,
    preview: Option<Preview>,
    apply: Option<ApplyRun>,
    last_apply: Option<Result<ApplyReport, String>>,
}

static UI: OnceLock<Mutex<ReplaceState>> = OnceLock::new();

fn ui_state() -> &'static Mutex<ReplaceState> {
    UI.get_or_init(|| Mutex::new(ReplaceState::default()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Preview,
    Replace,
    Undo,
}

/// `Cmd::FindReplace`. Roots are the selected items, or the focused folder when nothing is
/// selected.
pub fn open(app: &mut ZxrawApp) {
    let selected: Vec<PathBuf> = app
        .pane()
        .selected_entries()
        .into_iter()
        .map(|entry| entry.path.clone())
        .collect();
    let roots = if selected.is_empty() {
        vec![app.pane().path.clone()]
    } else {
        selected
    };
    let mut state = ui_state().lock();
    open_with(app, &mut state, roots);
}

fn open_with(app: &ZxrawApp, state: &mut ReplaceState, roots: Vec<PathBuf>) {
    state.open = true;
    state.roots = roots;
    state.preview = None;
    prune_old_backups(app.paths.data.join("replace-backups"));
}

/// Called every frame by the app shell.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    // The state is moved out for the frame so the window closure can borrow it freely.
    let mut state = std::mem::take(&mut *ui_state().lock());
    poll(&mut state);
    if state.open {
        draw(app, ctx, &mut state);
    }
    *ui_state().lock() = state;
}

/// `Cmd::Undo` for a find-and-replace.
pub fn undo(app: &mut ZxrawApp, records: Vec<ReplaceUndo>) {
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    let count = records.len();
    app.ops.jobs.spawn(
        "undo-replace",
        format!("Undoing find and replace in {count} file(s)"),
        move |job| {
            job.progress.add_total_items(count as u64);
            let mut errors = Vec::new();
            let mut restored = 0usize;
            for record in records.iter().rev() {
                job.progress.set_current(record.path.display().to_string());
                match text_replace::undo(record) {
                    Ok(()) => {
                        restored += 1;
                        job.progress.add_items(1);
                    }
                    Err(error) => {
                        errors.push(format!("{}: {error}", record.path.display()));
                        job.progress.add_error();
                    }
                }
                if let Some(parent) = record.path.parent() {
                    let _ = tx.send(Event::Changed(parent.to_path_buf()));
                }
            }
            if errors.is_empty() {
                let _ = tx.send(Event::Toast(format!("Restored {restored} file(s)")));
            } else {
                let _ = tx.send(Event::Report {
                    title: "Undo replace was incomplete".into(),
                    lines: errors,
                });
            }
            ctx.request_repaint();
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// Moves finished job outcomes into the window state.
fn poll(state: &mut ReplaceState) {
    let scan_result = state.scan.as_ref().and_then(|run| {
        // Read the job state first: a finished job has already stored its outcome.
        let finished = run.handle.state().is_finished();
        let outcome = run.outcome.lock().take();
        outcome.or_else(|| finished.then(|| Err("The preview stopped unexpectedly.".to_string())))
    });
    if let Some(result) = scan_result
        && let Some(run) = state.scan.take()
    {
        state.preview = Some(Preview::new(run.options, result));
    }

    let apply_result = state.apply.as_ref().and_then(|run| {
        let finished = run.handle.state().is_finished();
        let outcome = run.outcome.lock().take();
        outcome.or_else(|| finished.then(|| Err("The replace stopped unexpectedly.".to_string())))
    });
    if let Some(result) = apply_result {
        state.apply = None;
        // The files changed, so the preview no longer describes them.
        state.preview = None;
        state.last_apply = Some(result);
    }
}

fn start_preview(app: &mut ZxrawApp, state: &mut ReplaceState) -> Result<(), String> {
    if state.scan.is_some() {
        return Err("A preview is already running.".into());
    }
    if state.apply.is_some() {
        return Err("Wait for the replace to finish first.".into());
    }
    if state.roots.is_empty() {
        return Err("Nothing to search in.".into());
    }
    let options = state.options.clone();
    let scan_options = state.validation.get(&options).clone()?;
    let spec = options.search_spec();
    let roots = state.roots.clone();
    let title = format!("Finding “{}” in {}", options.find, scope_label(&roots));
    let outcome: Outcome<ScanReport> = Arc::new(Mutex::new(None));
    let slot = outcome.clone();
    let ctx = app.ops.ctx.clone();
    let handle = app.ops.jobs.spawn("replace-preview", title, move |job| {
        let result = text_replace::scan(&roots, &spec, &scan_options, &JobCtx::from_job(job))
            .map_err(|error| error.to_string());
        let result = match result {
            Err(_) if job.cancel.is_cancelled() => Err("Preview cancelled".to_string()),
            other => other,
        };
        let failure = result.as_ref().err().cloned();
        *slot.lock() = Some(result);
        ctx.request_repaint();
        failure.map_or(Ok(()), Err)
    });
    state.preview = None;
    state.scan = Some(ScanRun {
        handle,
        outcome,
        options,
    });
    Ok(())
}

fn start_replace(app: &mut ZxrawApp, state: &mut ReplaceState) -> Result<(), String> {
    if state.apply.is_some() {
        return Err("A replace is already running.".into());
    }
    if state.scan.is_some() {
        return Err("Wait for the preview to finish first.".into());
    }
    let Some(preview) = state.preview.as_ref() else {
        return Err("Preview the replace first.".into());
    };
    if is_stale(&preview.options, &state.options) {
        return Err("Options changed: preview again before replacing.".into());
    }
    let Ok(report) = &preview.result else {
        return Err("Preview the replace first.".into());
    };
    let files: Vec<FilePreview> = report
        .files
        .iter()
        .filter(|file| !preview.unticked.contains(&file.path))
        .cloned()
        .collect();
    if files.is_empty() {
        return Err("No files are ticked.".into());
    }
    let spec = preview.options.search_spec();
    let backup_dir = app
        .paths
        .data
        .join("replace-backups")
        .join(next_backup_name());
    let title = format!("Replacing in {} {}", files.len(), files_word(files.len()));

    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    let outcome: Outcome<ApplyReport> = Arc::new(Mutex::new(None));
    let slot = outcome.clone();
    let handle = app.ops.jobs.spawn("replace-apply", title, move |job| {
        let result = text_replace::apply(&files, &spec, &backup_dir, &JobCtx::from_job(job))
            .map_err(|error| error.to_string());
        let failure = match &result {
            Ok(report) => {
                report_applied(&tx, report);
                None
            }
            Err(error) => {
                let _ = tx.send(Event::Error(format!("Replace failed: {error}")));
                Some(error.clone())
            }
        };
        *slot.lock() = Some(result);
        ctx.request_repaint();
        failure.map_or(Ok(()), Err)
    });
    state.last_apply = None;
    state.apply = Some(ApplyRun { handle, outcome });
    Ok(())
}

/// Sends the events that follow a finished replace: undo entry, folder refreshes and the
/// summary, plus a report for files that were skipped or failed.
fn report_applied(tx: &Sender<Event>, report: &ApplyReport) {
    if !report.undo.is_empty() {
        let _ = tx.send(Event::Undo(UndoEntry::TextReplace(report.undo.clone())));
    }
    let folders: BTreeSet<PathBuf> = report
        .undo
        .iter()
        .filter_map(|undo| undo.path.parent().map(Path::to_path_buf))
        .collect();
    for folder in folders {
        let _ = tx.send(Event::Changed(folder));
    }
    let mut message = if report.undo.is_empty() {
        "No matches were replaced".to_string()
    } else {
        format!(
            "Replaced {} matches in {} {}",
            report.replacements(),
            report.undo.len(),
            files_word(report.undo.len())
        )
    };
    if report.cancelled {
        message.push_str(" (cancelled)");
    }
    let _ = tx.send(Event::Toast(message));
    if !report.skipped.is_empty() || !report.failed.is_empty() {
        let mut lines: Vec<String> = report
            .skipped
            .iter()
            .map(|(path, reason)| format!("{}: {}", path.display(), reason.describe()))
            .collect();
        lines.extend(
            report
                .failed
                .iter()
                .map(|(path, error)| format!("{}: {error}", path.display())),
        );
        let _ = tx.send(Event::Report {
            title: "Some files were not replaced".into(),
            lines,
        });
    }
}

/// `Undo replace`: runs the undo for the last finished replace and drops its entry from the
/// global undo stack when that entry is the newest one.
fn undo_last(app: &mut ZxrawApp, state: &mut ReplaceState) -> Result<(), String> {
    if state.apply.is_some() {
        return Err("Wait for the replace to finish first.".into());
    }
    let records = match state.last_apply.as_ref() {
        Some(Ok(report)) if !report.undo.is_empty() => report.undo.clone(),
        _ => return Err("There is no find and replace to undo.".into()),
    };
    state.last_apply = None;
    if matches!(
        app.ops.undo.last(),
        Some(UndoEntry::TextReplace(stacked)) if *stacked == records
    ) {
        app.ops.undo.pop();
    }
    undo(app, records);
    Ok(())
}

/// Removes backup folders older than [`BACKUP_RETENTION`]. Best effort, on a background thread.
fn prune_old_backups(dir: PathBuf) {
    std::thread::spawn(move || {
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        let Some(cutoff) = SystemTime::now().checked_sub(BACKUP_RETENTION) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let old = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| modified < cutoff);
            if old {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    });
}

/// A backup folder name that is unique per call: the time to the millisecond plus a counter.
fn backup_name_at(at: DateTime<Local>, sequence: u64) -> String {
    format!(
        "{}-{:03}-{sequence}",
        at.format("%Y%m%d-%H%M%S"),
        at.timestamp_subsec_millis()
    )
}

fn next_backup_name() -> String {
    backup_name_at(
        Local::now(),
        BACKUP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------

fn draw(app: &mut ZxrawApp, ctx: &egui::Context, state: &mut ReplaceState) {
    let mut action: Option<Action> = None;
    let mut is_open = state.open;
    let scanning = state.scan.as_ref().map(|run| run.handle.clone());
    let applying = state.apply.as_ref().map(|run| run.handle.clone());
    let busy = scanning.is_some() || applying.is_some();
    if busy {
        ctx.request_repaint_after(Duration::from_millis(250));
    }
    // Errors only: a valid form needs no clone of its options.
    let check_error = state.validation.get(&state.options).as_ref().err().cloned();
    let find_empty = state.options.find.is_empty();
    let scope = scope_label(&state.roots);

    egui::Window::new("Find and replace")
        .collapsible(false)
        .default_width(640.0)
        .open(&mut is_open)
        .show(ctx, |ui| {
            ui.label(format!("In: {scope}"));
            ui.separator();
            egui::Grid::new("replace:form")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Find");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.options.find).desired_width(420.0),
                    );
                    ui.end_row();
                    ui.label("Replace with");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.options.replace).desired_width(420.0),
                    );
                    ui.end_row();
                    ui.label("Include files");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.options.include)
                            .hint_text("*.txt; *.xmp")
                            .desired_width(420.0),
                    );
                    ui.end_row();
                    ui.label("Exclude files");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.options.exclude).desired_width(420.0),
                    );
                    ui.end_row();
                    ui.label("Skip files larger than");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut state.options.max_mib)
                                .desired_width(60.0),
                        );
                        ui.label("MiB");
                    });
                    ui.end_row();
                });
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut state.options.regex, "Regular expression");
                ui.checkbox(&mut state.options.case_sensitive, "Match case");
                ui.checkbox(&mut state.options.whole_word, "Whole word");
                ui.checkbox(&mut state.options.recursive, "Include subfolders");
            });
            ui.label(
                RichText::new(
                    "Only known text files are searched (.txt, .md, .log, .xmp, code and \
                     similar); other types are skipped unless their pattern is in Include \
                     files. Binary files, RAW/images/video/archives, files above the size limit \
                     and online-only cloud files are skipped.",
                )
                .weak(),
            );
            if !find_empty && let Some(error) = &check_error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }

            ui.separator();
            ui.horizontal(|ui| {
                let can_preview = !find_empty && check_error.is_none() && !busy;
                let response = ui.add_enabled(can_preview, egui::Button::new("Preview"));
                register_click_target(ui.ctx(), "replace:Preview", &response);
                if response.clicked() {
                    action = Some(Action::Preview);
                }
            });
            if let Some(handle) = &scanning
                && draw_progress(ui, handle, "Finding…")
            {
                handle.cancel.cancel();
            }
            draw_results(ui, state, busy, &mut action);
        });

    state.open = is_open;
    let result = match action {
        None => return,
        Some(Action::Preview) => start_preview(app, state),
        Some(Action::Replace) => start_replace(app, state),
        Some(Action::Undo) => undo_last(app, state),
    };
    if let Err(error) = result {
        app.toast_err(error);
    }
}

/// Progress block for a running job. Returns true when Cancel was clicked.
fn draw_progress(ui: &mut egui::Ui, handle: &JobHandle, title: &str) -> bool {
    let progress = handle.progress.snapshot();
    ui.label(RichText::new(title).strong());
    match progress.fraction() {
        Some(fraction) => {
            ui.add(egui::ProgressBar::new(fraction).show_percentage());
        }
        None => {
            ui.add(egui::ProgressBar::new(0.0).animate(true));
        }
    }
    ui.label(format!(
        "{} / {} items · {} error(s)",
        progress.items_done, progress.items_total, progress.errors
    ));
    if !progress.current.is_empty() {
        ui.label(RichText::new(progress.current).monospace().weak());
    }
    ui.button("Cancel").clicked()
}

fn draw_results(
    ui: &mut egui::Ui,
    state: &mut ReplaceState,
    busy: bool,
    action: &mut Option<Action>,
) {
    let stale = state
        .preview
        .as_ref()
        .is_some_and(|preview| is_stale(&preview.options, &state.options));
    let roots = state.roots.clone();
    let applying = state.apply.as_ref().map(|run| run.handle.clone());
    if let Some(handle) = &applying {
        ui.separator();
        if draw_progress(ui, handle, "Replacing…") {
            handle.cancel.cancel();
        }
    }
    if let Some(preview) = state.preview.as_mut() {
        draw_preview(ui, preview, &roots, stale, busy, action);
    }
    if let Some(last) = state.last_apply.as_ref() {
        ui.separator();
        match last {
            Ok(report) => {
                if report.undo.is_empty() {
                    ui.label("The last replace changed no files.");
                } else {
                    ui.label(format!(
                        "Last replace: {} matches in {} {}.",
                        report.replacements(),
                        report.undo.len(),
                        files_word(report.undo.len())
                    ));
                }
                let undoable = !report.undo.is_empty();
                ui.horizontal(|ui| {
                    let response =
                        ui.add_enabled(undoable && !busy, egui::Button::new("Undo replace"));
                    register_click_target(ui.ctx(), "replace:Undo", &response);
                    if response.clicked() {
                        *action = Some(Action::Undo);
                    }
                });
            }
            Err(message) => {
                ui.colored_label(ui.visuals().error_fg_color, message);
            }
        }
    }
}

fn draw_preview(
    ui: &mut egui::Ui,
    preview: &mut Preview,
    roots: &[PathBuf],
    stale: bool,
    busy: bool,
    action: &mut Option<Action>,
) {
    let Preview {
        result,
        unticked,
        rows,
        ..
    } = preview;
    ui.separator();
    let report = match result {
        Ok(report) => report,
        Err(message) => {
            ui.colored_label(ui.visuals().error_fg_color, message.as_str());
            return;
        }
    };
    if stale {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "Options changed — preview again",
        );
    }
    ui.label(format!(
        "{} matches in {} {} · {} files scanned · {} skipped",
        report.total_matches(),
        report.files.len(),
        files_word(report.files.len()),
        report.files_scanned,
        report.skipped.len()
    ));
    let ticked = report
        .files
        .iter()
        .filter(|file| !unticked.contains(&file.path))
        .count();
    ui.horizontal(|ui| {
        if ui.button("Tick all").clicked() {
            unticked.clear();
        }
        if ui.button("Untick all").clicked() {
            unticked.extend(report.files.iter().map(|file| file.path.clone()));
        }
        let response = ui.add_enabled(
            !stale && ticked > 0 && !busy,
            egui::Button::new(format!("Replace in {ticked} {}", files_word(ticked))),
        );
        register_click_target(ui.ctx(), "replace:Replace", &response);
        if response.clicked() {
            *action = Some(Action::Replace);
        }
    });

    // Only the visible rows are laid out, so a preview of many files stays responsive.
    egui::ScrollArea::vertical()
        .id_salt("replace:results")
        .max_height(340.0)
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
            for row in &rows[range] {
                match *row {
                    PreviewRow::File(index) => {
                        let file = &report.files[index];
                        ui.horizontal(|ui| {
                            let mut on = !unticked.contains(&file.path);
                            if ui
                                .checkbox(
                                    &mut on,
                                    RichText::new(display_path(roots, &file.path)).strong(),
                                )
                                .changed()
                            {
                                if on {
                                    unticked.remove(&file.path);
                                } else {
                                    unticked.insert(file.path.clone());
                                }
                            }
                            ui.label(
                                RichText::new(format!(
                                    "{} matches · {} · {}",
                                    file.match_count,
                                    file.encoding.label(),
                                    file.line_ending.label()
                                ))
                                .weak(),
                            );
                        });
                    }
                    PreviewRow::Line { file, line, new } => {
                        let line = &report.files[file].matches[line];
                        let color = ui.visuals().text_color();
                        let (job, number) = if new {
                            (
                                with_text_color(
                                    highlight_job(
                                        &line.replaced,
                                        &line.replaced_ranges,
                                        NEW_BACKGROUND,
                                    ),
                                    color,
                                ),
                                RichText::new(format!("{}: ", line.line_number))
                                    .monospace()
                                    .weak(),
                            )
                        } else {
                            (
                                with_text_color(
                                    highlight_job(&line.line, &line.ranges, OLD_BACKGROUND),
                                    color,
                                ),
                                RichText::new(format!("{}: ", line.line_number)).monospace(),
                            )
                        };
                        ui.horizontal(|ui| {
                            ui.label(number);
                            // Long lines are cut to the row; the full line is in the file.
                            ui.add(egui::Label::new(job).truncate());
                        });
                    }
                    PreviewRow::More(count) => {
                        ui.label(RichText::new(format!("… and {count} more lines")).weak());
                    }
                }
            }
        });

    if !report.skipped.is_empty() {
        egui::CollapsingHeader::new(format!("Skipped ({})", report.skipped.len()))
            .id_salt("replace:skipped")
            .default_open(false)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("replace:skipped-list")
                    .max_height(160.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (path, reason) in &report.skipped {
                            ui.label(format!(
                                "{} — {}",
                                display_path(roots, path),
                                reason.describe()
                            ));
                        }
                    });
            });
    }
}

/// One row of the preview list. Rows hold indexes into the report they were built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewRow {
    /// The file's checkbox and summary: index into `report.files`.
    File(usize),
    /// A matching line, before the replace (`new` false) or after it. `file` indexes
    /// `report.files` and `line` indexes that file's `matches`.
    Line { file: usize, line: usize, new: bool },
    /// Matching lines of the file that are not stored in the preview.
    More(usize),
}

/// The preview flattened into rows: a header per file, the old and new form of each stored
/// line, and a count row when the file has more matching lines than are stored.
fn preview_rows(report: &ScanReport) -> Vec<PreviewRow> {
    let mut rows = Vec::new();
    for (index, file) in report.files.iter().enumerate() {
        rows.push(PreviewRow::File(index));
        for line in 0..file.matches.len() {
            rows.push(PreviewRow::Line {
                file: index,
                line,
                new: false,
            });
            rows.push(PreviewRow::Line {
                file: index,
                line,
                new: true,
            });
        }
        let more = file.lines_with_matches.saturating_sub(file.matches.len());
        if more > 0 {
            rows.push(PreviewRow::More(more));
        }
    }
    rows
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Builds the job for one line: `ranges` get `background`, everything else is plain. The
/// sections cover the whole text in order. Ranges that are unsorted, overlapping, out of
/// bounds or not on character boundaries are skipped.
fn highlight_job(text: &str, ranges: &[Range<usize>], background: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let plain = TextFormat {
        font_id: FontId::monospace(13.0),
        ..Default::default()
    };
    let marked = TextFormat {
        background,
        ..plain.clone()
    };
    let mut position = 0;
    for range in ranges {
        let valid = range.start >= position
            && range.start <= range.end
            && range.end <= text.len()
            && text.is_char_boundary(range.start)
            && text.is_char_boundary(range.end);
        if !valid {
            continue;
        }
        if range.start > position {
            job.append(&text[position..range.start], 0.0, plain.clone());
        }
        if range.end > range.start {
            job.append(&text[range.clone()], 0.0, marked.clone());
        }
        position = range.end;
    }
    if position < text.len() {
        job.append(&text[position..], 0.0, plain);
    }
    job
}

fn with_text_color(mut job: LayoutJob, color: Color32) -> LayoutJob {
    for section in &mut job.sections {
        section.format.color = color;
    }
    job
}

/// What the window is searching: the single folder, or a count of selected items.
fn scope_label(roots: &[PathBuf]) -> String {
    match roots {
        [] => "nothing".into(),
        [folder] if folder.is_dir() => folder.display().to_string(),
        _ => {
            let count = roots.len();
            if count == 1 {
                "1 selected item".into()
            } else {
                format!("{count} selected items")
            }
        }
    }
}

/// A file path relative to the root it was found under, when possible.
fn display_path(roots: &[PathBuf], path: &Path) -> String {
    for root in roots {
        if let Ok(relative) = path.strip_prefix(root) {
            if relative.as_os_str().is_empty() {
                return path.file_name().map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                );
            }
            return relative.display().to_string();
        }
    }
    path.display().to_string()
}

fn files_word(count: usize) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

// ---------------------------------------------------------------------------
// Automation hooks (driven by the e2e test hook, through the same code as the buttons)
// ---------------------------------------------------------------------------

pub(crate) fn automation_open(app: &mut ZxrawApp, roots: Vec<PathBuf>) {
    let mut state = ui_state().lock();
    open_with(app, &mut state, roots);
}

pub(crate) fn automation_set(_app: &mut ZxrawApp, key: &str, value: &str) -> Result<(), String> {
    fn on_off(value: &str) -> Result<bool, String> {
        match value {
            "on" => Ok(true),
            "off" => Ok(false),
            other => Err(format!("expected on or off, got {other:?}")),
        }
    }
    let mut state = ui_state().lock();
    let options = &mut state.options;
    match key {
        "find" => options.find = value.to_string(),
        "replace" => options.replace = value.to_string(),
        "regex" => options.regex = on_off(value)?,
        "case" => options.case_sensitive = on_off(value)?,
        "whole-word" => options.whole_word = on_off(value)?,
        "recursive" => options.recursive = on_off(value)?,
        "include" => options.include = value.to_string(),
        "exclude" => options.exclude = value.to_string(),
        "max-mib" => options.max_mib = value.to_string(),
        other => return Err(format!("unknown find-and-replace setting {other:?}")),
    }
    Ok(())
}

pub(crate) fn automation_preview(app: &mut ZxrawApp) -> Result<(), String> {
    let mut state = ui_state().lock();
    start_preview(app, &mut state)
}

/// `None` while the preview runs (or none was started).
pub(crate) fn automation_preview_result(_app: &mut ZxrawApp) -> Option<Result<ScanReport, String>> {
    let mut state = ui_state().lock();
    poll(&mut state);
    if state.scan.is_some() {
        return None;
    }
    state.preview.as_ref().map(|preview| preview.result.clone())
}

pub(crate) fn automation_untick(_app: &mut ZxrawApp, file_name: &str) -> Result<(), String> {
    let mut state = ui_state().lock();
    poll(&mut state);
    let Some(Preview {
        result: Ok(report),
        unticked,
        ..
    }) = state.preview.as_mut()
    else {
        return Err("There is no preview to untick files in.".into());
    };
    let paths: Vec<PathBuf> = report
        .files
        .iter()
        .filter(|file| {
            file.path
                .file_name()
                .is_some_and(|name| name.to_string_lossy() == file_name)
        })
        .map(|file| file.path.clone())
        .collect();
    if paths.is_empty() {
        return Err(format!("No previewed file named {file_name:?}."));
    }
    unticked.extend(paths);
    Ok(())
}

pub(crate) fn automation_replace(app: &mut ZxrawApp) -> Result<(), String> {
    let mut state = ui_state().lock();
    start_replace(app, &mut state)
}

/// `None` while the replace runs (or none was started).
pub(crate) fn automation_apply_result(_app: &mut ZxrawApp) -> Option<Result<ApplyReport, String>> {
    let mut state = ui_state().lock();
    poll(&mut state);
    if state.apply.is_some() {
        return None;
    }
    state.last_apply.clone()
}

pub(crate) fn automation_undo(app: &mut ZxrawApp) -> Result<(), String> {
    let mut state = ui_state().lock();
    undo_last(app, &mut state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names_are_unique_within_one_millisecond() {
        let at = Local::now();
        assert_ne!(backup_name_at(at, 1), backup_name_at(at, 2));
        let first = next_backup_name();
        let second = next_backup_name();
        assert_ne!(first, second);
    }

    #[test]
    fn changing_any_option_makes_the_preview_stale() {
        let base = Options::default();
        assert!(!is_stale(&base, &base.clone()));
        let changes: Vec<fn(&mut Options)> = vec![
            |o| o.find.push('x'),
            |o| o.replace.push('x'),
            |o| o.regex = !o.regex,
            |o| o.case_sensitive = !o.case_sensitive,
            |o| o.whole_word = !o.whole_word,
            |o| o.recursive = !o.recursive,
            |o| o.include.push_str("*.txt"),
            |o| o.exclude.push_str("*.bak"),
            |o| o.max_mib.push('0'),
        ];
        for change in changes {
            let mut edited = base.clone();
            change(&mut edited);
            assert!(is_stale(&base, &edited), "{edited:?} should be stale");
        }
    }

    #[test]
    fn highlight_job_covers_exactly_the_ranges() {
        let text = "héllo world, wörld";
        let ranges = vec![0..1, 7..12];
        let color = Color32::RED;
        let job = highlight_job(text, &ranges, color);
        assert_eq!(job.text, text);

        let marked: Vec<Range<usize>> = job
            .sections
            .iter()
            .filter(|section| section.format.background == color)
            .map(|section| section.byte_range.start.0..section.byte_range.end.0)
            .collect();
        assert_eq!(marked, ranges);

        let mut position = 0;
        for section in &job.sections {
            assert_eq!(section.byte_range.start.0, position);
            position = section.byte_range.end.0;
        }
        assert_eq!(position, text.len());
    }

    // One range is the point of these cases.
    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn highlight_job_skips_ranges_inside_a_character() {
        let job = highlight_job("é", &[1..2], Color32::RED);
        assert_eq!(job.text, "é");
        assert!(
            job.sections
                .iter()
                .all(|section| section.format.background != Color32::RED)
        );
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn preview_rows_flatten_each_file_and_count_the_unstored_lines() {
        use forge_core::text_replace::{Encoding, Fingerprint, LineEnding, LineMatch};

        let line = LineMatch {
            line_number: 1,
            line: "a".into(),
            ranges: vec![0..1],
            replaced: "b".into(),
            replaced_ranges: vec![0..1],
        };
        let file = FilePreview {
            path: "x.txt".into(),
            root: "r".into(),
            rel: "x.txt".into(),
            encoding: Encoding::Utf8,
            line_ending: LineEnding::Lf,
            matches: vec![line],
            match_count: 3,
            lines_with_matches: 3,
            fingerprint: Fingerprint {
                size: 1,
                hash: [0; 32],
            },
            max_file_size: 1,
        };
        let report = ScanReport {
            files: vec![file],
            ..Default::default()
        };
        let preview = Preview::new(Options::default(), Ok(report));
        let rows = &preview.rows;
        assert_eq!(rows.len(), 4);
        assert!(matches!(rows[0], PreviewRow::File(0)));
        assert!(matches!(rows[1], PreviewRow::Line { new: false, .. }));
        assert!(matches!(rows[2], PreviewRow::Line { new: true, .. }));
        assert!(matches!(rows[3], PreviewRow::More(2)));
    }

    #[test]
    fn a_failed_preview_has_no_rows() {
        let preview = Preview::new(Options::default(), Err("boom".into()));
        assert!(preview.rows.is_empty());
    }

    #[test]
    fn validation_runs_check_once_for_unchanged_options() {
        let mut validation = Validation::default();
        let mut options = Options {
            find: "a".into(),
            ..Options::default()
        };
        // Two frames with the same options: one compile.
        assert!(validation.get(&options).is_ok());
        assert!(validation.get(&options).is_ok());
        assert_eq!(validation.computations, 1);

        // A changed option is checked again, and the error is cached too.
        options.regex = true;
        options.find = "(".into();
        assert!(validation.get(&options).is_err());
        assert!(validation.get(&options).is_err());
        assert_eq!(validation.computations, 2);
    }
}
