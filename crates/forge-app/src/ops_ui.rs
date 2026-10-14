//! Copy/move/delete jobs, the interactive conflict dialog and the operations panel.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::{Sender, bounded};
use egui::{RichText, Ui};
use forge_core::model::format_size;
use forge_core::ops::{
    self, ConflictDecision, ConflictInfo, ConflictPolicy, JobCtx, OpKind, OpOptions, OpReport,
    Resolution,
};
use forge_jobs::{JobHandle, JobState};

use crate::actions::{Event, UndoEntry};
use crate::app::ZxrawApp;
use crate::views;

/// A conflict waiting for the user, with the channel the job thread is blocked on.
pub struct ConflictQuestion {
    pub info: ConflictInfo,
    pub job_title: String,
    pub reply: Sender<Resolution>,
    pub apply_all: bool,
}

struct ClipboardMoveCompletion {
    sources: Vec<PathBuf>,
    generation: Option<u64>,
    expected_sequence: Option<u32>,
}

/// Start a copy or move of `sources` into `dest` as a background job.
pub fn start_transfer(app: &mut ZxrawApp, sources: Vec<PathBuf>, dest: PathBuf, kind: OpKind) {
    // Items already in `dest`: moving them there is a no-op, and copying them there must
    // never be offered as "replace the file with itself" — Explorer makes a renamed copy.
    let in_dest = |p: &PathBuf| {
        p.parent()
            .is_some_and(|d| forge_core::paths::paths_equal(d, &dest))
    };
    let (same, sources): (Vec<PathBuf>, Vec<PathBuf>) = sources.into_iter().partition(in_dest);
    if !same.is_empty() && kind == OpKind::Copy {
        start_transfer_with(
            app,
            same,
            dest.clone(),
            kind,
            ConflictPolicy::KeepBoth,
            None,
        );
    }
    if sources.is_empty() {
        return;
    }
    start_transfer_with(app, sources, dest, kind, ConflictPolicy::Ask, None);
}

/// Start a move originating from the system or in-app file clipboard. The clipboard is
/// consumed only after this job reports a complete, uncancelled move.
pub fn start_clipboard_move(
    app: &mut ZxrawApp,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    generation: Option<u64>,
    expected_sequence: Option<u32>,
) -> bool {
    let clipboard_sources = sources.clone();
    let in_dest = |path: &PathBuf| {
        path.parent()
            .is_some_and(|parent| forge_core::paths::paths_equal(parent, &dest))
    };
    let sources: Vec<PathBuf> = sources.into_iter().filter(|path| !in_dest(path)).collect();
    if sources.is_empty() {
        return false;
    }
    let completion = ClipboardMoveCompletion {
        sources: clipboard_sources,
        generation,
        expected_sequence,
    };
    start_transfer_with(
        app,
        sources,
        dest,
        OpKind::Move,
        ConflictPolicy::Ask,
        Some(completion),
    );
    true
}

fn start_transfer_with(
    app: &mut ZxrawApp,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    kind: OpKind,
    policy: ConflictPolicy,
    completion: Option<ClipboardMoveCompletion>,
) {
    if sources.is_empty() {
        return;
    }
    let verify = app.settings.verify_copies;
    let verb = if kind == OpKind::Copy {
        "Copying"
    } else {
        "Moving"
    };
    let title = format!(
        "{verb} {} item(s) to {}",
        sources.len(),
        dest.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dest.display().to_string())
    );
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    let title2 = title.clone();
    // Automation can explicitly arm a read fault (only script steps arm it). Force the
    // move-copy path in that case so tests exercise the same verified fallback used for
    // cross-volume moves. Not `testhook.active()`: it is false while a script step runs.
    let force_copy_for_move = kind == OpKind::Move && forge_core::fault::is_armed();
    app.ops
        .jobs
        .transfers()
        .set_serial(app.settings.transfer_queue);
    let disks = transfer_disks(&sources, &dest);
    // A move deletes its sources, so it must act on the items the user picked: what is
    // at a source path when the job starts must still be the same item.
    let identities: Vec<Option<SourceIdentity>> = if kind == OpKind::Move {
        sources.iter().map(|s| source_identity(s)).collect()
    } else {
        Vec::new()
    };
    // Taken by whichever runs: the job, or the callback for a job cancelled while queued.
    let completion = std::sync::Arc::new(parking_lot::Mutex::new(completion));
    let unstarted = {
        let (completion, tx, ctx) = (completion.clone(), tx.clone(), ctx.clone());
        let title = title.clone();
        move || {
            let _ = tx.send(Event::Toast(format!(
                "{title} — cancelled before it started"
            )));
            if let Some(completion) = completion.lock().take() {
                let _ = tx.send(Event::MoveFinished {
                    sources: completion.sources,
                    ok: false,
                    clipboard_generation: completion.generation,
                    expected_sequence: completion.expected_sequence,
                });
            }
            ctx.request_repaint();
        }
    };
    let job_body = move |job: &JobHandle| {
        let completion = completion.lock().take();
        // A queued job starts later: anything that vanished meanwhile is skipped, not
        // failed, and a moved source replaced by another item is left alone. Conflicts
        // are looked at only now, when the job runs.
        let mut changed = Vec::new();
        let sources: Vec<PathBuf> = sources
            .into_iter()
            .enumerate()
            .filter_map(|(i, source)| match identities.get(i) {
                Some(Some(then)) if source_identity(&source).is_some_and(|now| now != *then) => {
                    changed.push(source);
                    None
                }
                _ => Some(source),
            })
            .collect();
        let (sources, mut vanished): (Vec<PathBuf>, Vec<PathBuf>) = sources
            .into_iter()
            .partition(|s| forge_core::paths::exists_no_follow(s));
        let opts = OpOptions {
            policy,
            verify,
            force_copy_for_move,
        };
        let plan = match ops::plan(kind, &sources, &dest, opts, &job.cancel) {
            Ok(mut plan) => {
                // A source that vanished after the check above is not a failure either.
                plan.issues.retain(|issue| {
                    let gone = sources.contains(&issue.path)
                        && !forge_core::paths::exists_no_follow(&issue.path);
                    if gone {
                        vanished.push(issue.path.clone());
                    }
                    !gone
                });
                plan
            }
            Err(e) => {
                let _ = tx.send(Event::Error(e.to_string()));
                if let Some(completion) = completion {
                    let _ = tx.send(Event::MoveFinished {
                        sources: completion.sources,
                        ok: false,
                        clipboard_generation: completion.generation,
                        expected_sequence: completion.expected_sequence,
                    });
                }
                ctx.request_repaint();
                return Err(e.to_string());
            }
        };
        job.progress.add_total_items(plan.files + plan.links);
        job.progress.add_total_bytes(plan.bytes);

        // Conflicts are forwarded to the UI; this thread blocks until the user answers
        // (or the job is cancelled).
        let ui_tx = tx.clone();
        let ui_ctx = ctx.clone();
        let cancel = job.cancel.clone();
        let mut resolver = move |info: &ConflictInfo| -> Resolution {
            let (rtx, rrx) = bounded(1);
            let _ = ui_tx.send(Event::Conflict(ConflictQuestion {
                info: info.clone(),
                job_title: title2.clone(),
                reply: rtx,
                apply_all: false,
            }));
            ui_ctx.request_repaint();
            loop {
                match rrx.recv_timeout(Duration::from_millis(200)) {
                    Ok(r) => return r,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) if !cancel.is_cancelled() => {
                        continue;
                    }
                    _ => return Resolution::once(ConflictDecision::Cancel),
                }
            }
        };
        let mut report = ops::execute(&plan, job, &mut resolver);
        for path in vanished {
            report
                .skipped
                .push(ops::OpIssue::new(path, "no longer exists"));
        }
        for path in changed {
            report
                .skipped
                .push(ops::OpIssue::new(path, "changed while queued, not moved"));
        }
        let move_ok = report.failed.is_empty() && report.skipped.is_empty() && !report.cancelled;
        finish_report(
            job,
            &report,
            &tx,
            &[dest.clone()],
            sources
                .iter()
                .filter_map(|s| s.parent().map(Path::to_path_buf))
                .collect(),
        );
        if let Some(completion) = completion {
            let _ = tx.send(Event::MoveFinished {
                sources: completion.sources,
                ok: move_ok,
                clipboard_generation: completion.generation,
                expected_sequence: completion.expected_sequence,
            });
        }
        ctx.request_repaint();
        Ok(())
    };
    app.ops
        .jobs
        .spawn_queued("transfer", title, disks, job_body, unstarted);
}

/// What a source path pointed at when a move was queued.
#[derive(PartialEq)]
enum SourceIdentity {
    /// Volume serial and file index (Windows file ID).
    Id {
        kind: std::fs::FileType,
        id: (u32, u64),
    },
    /// Fallback when the file ID cannot be read.
    Meta {
        kind: std::fs::FileType,
        len: u64,
        modified: Option<std::time::SystemTime>,
    },
}

/// `None` if nothing is at `path`. Does not follow a link at the end.
fn source_identity(path: &Path) -> Option<SourceIdentity> {
    let md = std::fs::symlink_metadata(path).ok()?;
    let kind = md.file_type();
    Some(match forge_win::file_id(path) {
        Ok(id) => SourceIdentity::Id { kind, id },
        Err(_) => SourceIdentity::Meta {
            kind,
            len: md.len(),
            modified: md.modified().ok(),
        },
    })
}

/// Physical disks a transfer reads and writes, or `None` if any of them is unknown
/// (network paths). Looked up per drive letter and cached briefly, because a drive
/// letter can be given to another card reader or USB disk.
fn transfer_disks(sources: &[PathBuf], dest: &Path) -> forge_jobs::DiskSet {
    use std::collections::HashMap;
    use std::path::Component;
    use std::sync::LazyLock;
    use std::time::Instant;

    static CACHE: LazyLock<parking_lot::Mutex<HashMap<String, (Instant, Option<Vec<u32>>)>>> =
        LazyLock::new(Default::default);
    let mut disks = Vec::new();
    for path in sources.iter().map(PathBuf::as_path).chain([dest]) {
        let Some(Component::Prefix(prefix)) = path.components().next() else {
            return None;
        };
        let key = prefix.as_os_str().to_string_lossy().to_lowercase();
        let found = {
            let mut cache = CACHE.lock();
            match cache.get(&key) {
                Some((at, found)) if at.elapsed() < Duration::from_secs(30) => found.clone(),
                _ => {
                    let found = forge_win::physical_disks(path);
                    cache.insert(key, (Instant::now(), found.clone()));
                    found
                }
            }
        };
        disks.extend(found?);
    }
    disks.sort_unstable();
    disks.dedup();
    Some(disks)
}

fn finish_report(
    job: &JobHandle,
    report: &OpReport,
    tx: &Sender<Event>,
    dests: &[PathBuf],
    mut sources: Vec<PathBuf>,
) {
    sources.sort();
    sources.dedup();
    for d in dests.iter().chain(sources.iter()) {
        let _ = tx.send(Event::Changed(d.clone()));
    }
    if !report.undo.is_empty() {
        let _ = tx.send(Event::Undo(UndoEntry::Ops(report.undo.clone())));
    }
    let mut lines = Vec::new();
    lines.push(format!(
        "{} done ({}), {} skipped, {} failed{}",
        report.done.len(),
        format_size(report.bytes),
        report.skipped.len(),
        report.failed.len(),
        if report.cancelled { ", cancelled" } else { "" }
    ));
    for f in &report.failed {
        lines.push(format!("FAILED  {}: {}", f.path.display(), f.reason));
    }
    for s in &report.skipped {
        lines.push(format!("skipped {}: {}", s.path.display(), s.reason));
    }
    for l in &lines {
        job.log(l.clone());
    }
    for _ in 0..report.failed.len() {
        job.progress.add_error();
    }
    if !report.failed.is_empty() {
        let _ = tx.send(Event::Report {
            title: format!("{} — completed with errors", job.title),
            lines,
        });
    } else if report.cancelled {
        let _ = tx.send(Event::Toast(format!("{} — cancelled", job.title)));
    } else {
        let _ = tx.send(Event::Toast(lines.remove(0)));
    }
}

pub fn delete_permanently(app: &mut ZxrawApp, targets: Vec<PathBuf>) {
    let n = targets.len();
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops.jobs.spawn(
        "delete",
        format!("Permanently deleting {n} item(s)"),
        move |job| {
            let report = ops::delete_permanently(&targets, &JobCtx::from_job(job));
            let parents: Vec<PathBuf> = targets
                .iter()
                .filter_map(|t| t.parent().map(Path::to_path_buf))
                .collect();
            finish_report(job, &report, &tx, &[], parents);
            ctx.request_repaint();
            Ok(())
        },
    );
}

pub fn undo_ops(app: &mut ZxrawApp, records: Vec<ops::UndoRecord>) {
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops.jobs.spawn(
        "undo",
        format!("Undoing {} change(s)", records.len()),
        move |job| {
            let mut errors = Vec::new();
            for r in records.iter().rev() {
                if let Err(e) = ops::undo(r) {
                    errors.push(format!("{}: {e}", r.describe()));
                    job.progress.add_error();
                }
                let touched: Vec<PathBuf> = match r {
                    ops::UndoRecord::Renamed { from, to } | ops::UndoRecord::Moved { from, to } => {
                        vec![from.clone(), to.clone()]
                    }
                    ops::UndoRecord::CreatedFolder { path }
                    | ops::UndoRecord::CreatedFile { path }
                    | ops::UndoRecord::Copied { path, .. } => vec![path.clone()],
                };
                for p in touched {
                    if let Some(d) = p.parent() {
                        let _ = tx.send(Event::Changed(d.to_path_buf()));
                    }
                }
            }
            if errors.is_empty() {
                let _ = tx.send(Event::Toast(format!("Undid {} change(s)", records.len())));
            } else {
                let _ = tx.send(Event::Report {
                    title: "Undo was incomplete".into(),
                    lines: errors,
                });
            }
            ctx.request_repaint();
            Ok(())
        },
    );
}

/// Conflict dialog. Returns true while a question is being shown.
pub fn conflict_dialog(app: &mut ZxrawApp, ctx: &egui::Context) -> bool {
    let Some(q) = app.ops.conflicts.front_mut() else {
        return false;
    };
    let mut answer = None;
    egui::Modal::new(egui::Id::new("conflict")).show(ctx, |ui| {
        ui.set_width(520.0);
        ui.heading("File already exists");
        ui.label(RichText::new(&q.job_title).weak());
        ui.add_space(6.0);
        let name = q
            .info
            .dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        ui.label(format!("The destination already contains “{name}”."));
        ui.add_space(4.0);
        egui::Grid::new("cmp")
            .num_columns(3)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                ui.label("");
                ui.label(RichText::new("Size").strong());
                ui.label(RichText::new("Modified").strong());
                ui.end_row();
                let newer = |a: Option<std::time::SystemTime>, b: Option<std::time::SystemTime>| {
                    a.zip(b).is_some_and(|(a, b)| a > b)
                };
                ui.label("Incoming");
                ui.label(format_size(q.info.source_size));
                ui.label(format!(
                    "{}{}",
                    views::format_time(q.info.source_modified),
                    if newer(q.info.source_modified, q.info.dest_modified) {
                        "  (newer)"
                    } else {
                        ""
                    }
                ));
                ui.end_row();
                ui.label("Existing");
                ui.label(format_size(q.info.dest_size));
                ui.label(format!(
                    "{}{}",
                    views::format_time(q.info.dest_modified),
                    if newer(q.info.dest_modified, q.info.source_modified) {
                        "  (newer)"
                    } else {
                        ""
                    }
                ));
                ui.end_row();
            });
        ui.add_space(8.0);
        ui.checkbox(&mut q.apply_all, "Do this for all remaining conflicts");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .button("Keep both")
                .on_hover_text("The incoming file gets a new name like “name (2)”")
                .clicked()
            {
                answer = Some(ConflictDecision::KeepBoth);
            }
            if ui.button("Skip").clicked() {
                answer = Some(ConflictDecision::Skip);
            }
            if ui.button("Replace if newer").clicked() {
                answer = Some(ConflictDecision::OverwriteIfNewer);
            }
            let replace = egui::Button::new(RichText::new("Replace").color(egui::Color32::WHITE))
                .fill(egui::Color32::from_rgb(170, 60, 50));
            if ui
                .add(replace)
                .on_hover_text("The existing file is overwritten. This cannot be undone.")
                .clicked()
            {
                answer = Some(ConflictDecision::Overwrite);
            }
            if ui.button("Cancel operation").clicked() {
                answer = Some(ConflictDecision::Cancel);
            }
        });
    });
    if let Some(decision) = answer {
        let q = app.ops.conflicts.pop_front().expect("front exists");
        let _ = q.reply.send(Resolution {
            decision,
            apply_to_all: q.apply_all,
        });
    }
    true
}

/// What the user asked for in a queued row.
enum QueueAction {
    Up,
    Down,
    StartNow,
}

pub fn jobs_window(app: &mut ZxrawApp, ctx: &egui::Context) {
    close_guard(app, ctx);
    let queue = app.ops.jobs.transfers().clone();
    queue.set_serial(app.settings.transfer_queue);
    let mut open = app.ops.show_jobs;
    egui::Window::new("Operations")
        .open(&mut open)
        .default_width(520.0)
        .resizable(true)
        .show(ctx, |ui| {
            let jobs = app.ops.jobs.list();
            if jobs.is_empty() {
                ui.label(RichText::new("No operations yet.").weak());
            }
            let waiting = queue.waiting();
            let paused = queue.is_paused();
            if !waiting.is_empty() || paused || app.ops.jobs.pending_transfers() > 0 {
                ui.horizontal(|ui| {
                    let text = match (waiting.len(), paused) {
                        (0, true) => "Queue paused".to_string(),
                        (n, true) => format!("Queue paused · {n} waiting"),
                        (n, false) => format!("{n} waiting in the queue"),
                    };
                    ui.label(RichText::new(text).strong());
                    let label = if paused {
                        "Resume queue"
                    } else {
                        "Pause queue"
                    };
                    if ui
                        .button(label)
                        .on_hover_text(
                            "Pausing holds waiting copies and moves and pauses running ones",
                        )
                        .clicked()
                    {
                        queue.set_paused(!paused);
                    }
                });
                ui.separator();
            }
            let mut action = None;
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .show(ui, |ui| {
                    // Waiting transfers first, in the order they will run.
                    for (i, id) in waiting.iter().enumerate() {
                        let Some(j) = jobs.iter().find(|j| j.id == *id) else {
                            continue;
                        };
                        let place = (i + 1, waiting.len());
                        if let Some(a) = job_row(ui, &app.ops.jobs, j, Some(place)) {
                            action = Some((j.id, a));
                        }
                        ui.separator();
                    }
                    for j in jobs.iter().rev().filter(|j| !waiting.contains(&j.id)) {
                        job_row(ui, &app.ops.jobs, j, None);
                        ui.separator();
                    }
                });
            match action {
                Some((id, QueueAction::Up)) => _ = queue.move_by(id, true),
                Some((id, QueueAction::Down)) => _ = queue.move_by(id, false),
                Some((id, QueueAction::StartNow)) => _ = queue.start_now(id),
                None => {}
            }
            if ui.button("Clear finished").clicked() {
                app.ops.jobs.clear_finished();
            }
        });
    app.ops.show_jobs = open;
}

/// Lets the close guard run in automation (scripts that test it turn it on).
pub static CLOSE_GUARD_IN_AUTOMATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn close_warning_id() -> egui::Id {
    egui::Id::new("close-with-transfers")
}

/// True while the "copies still waiting" warning is shown.
pub fn close_warning_open(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(close_warning_id()))
        .unwrap_or(false)
}

/// Warns before the window closes while copies or moves are queued or running: closing
/// cancels them (running copies remove their partial files).
fn close_guard(app: &mut ZxrawApp, ctx: &egui::Context) {
    let guarded = !app.testhook.active()
        || CLOSE_GUARD_IN_AUTOMATION.load(std::sync::atomic::Ordering::Relaxed);
    let pending = app.ops.jobs.pending_transfers();
    let confirmed_id = egui::Id::new("close-with-transfers-confirmed");
    let confirmed = ctx
        .data(|d| d.get_temp::<bool>(confirmed_id))
        .unwrap_or(false);
    if guarded && pending > 0 && !confirmed && ctx.input(|i| i.viewport().close_requested()) {
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.data_mut(|d| d.insert_temp(close_warning_id(), true));
    }
    if !close_warning_open(ctx) {
        return;
    }
    if pending == 0 {
        // Everything finished while the warning was open.
        ctx.data_mut(|d| d.insert_temp(close_warning_id(), false));
        return;
    }
    let waiting = app.ops.jobs.transfers().waiting().len();
    let mut choice = None;
    egui::Modal::new(close_warning_id()).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading("Copies are not finished");
        ui.add_space(4.0);
        let running = pending - waiting;
        ui.label(format!(
            "{running} copy or move operation(s) running and {waiting} waiting in the queue. \
             Closing cancels them; files already copied stay where they are."
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Keep open").clicked() {
                choice = Some(false);
            }
            if ui.button("Show operations").clicked() {
                app.ops.show_jobs = true;
                choice = Some(false);
            }
            let close =
                egui::Button::new(RichText::new("Close anyway").color(egui::Color32::WHITE))
                    .fill(egui::Color32::from_rgb(170, 60, 50));
            if ui.add(close).clicked() {
                choice = Some(true);
            }
        });
    });
    if let Some(close) = choice {
        ctx.data_mut(|d| d.insert_temp(close_warning_id(), false));
        if close {
            ctx.data_mut(|d| d.insert_temp(confirmed_id, true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

fn job_row(
    ui: &mut Ui,
    jobs: &forge_jobs::JobRegistry,
    j: &JobHandle,
    place: Option<(usize, usize)>,
) -> Option<QueueAction> {
    let snap = j.progress.snapshot();
    let state = j.state();
    let mut action = None;
    ui.horizontal(|ui| {
        if let Some((n, _)) = place {
            ui.label(RichText::new(format!("{n}.")).weak());
        }
        ui.label(RichText::new(j.title.as_ref()).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !state.is_finished() {
                if ui.small_button("Cancel").clicked() {
                    jobs.cancel(j);
                }
                if let Some((n, len)) = place {
                    if ui
                        .small_button("Start now")
                        .on_hover_text("Start this one without waiting for its turn")
                        .clicked()
                    {
                        action = Some(QueueAction::StartNow);
                    }
                    if ui
                        .add_enabled(n < len, egui::Button::new("⏷").small())
                        .on_hover_text("Run later")
                        .clicked()
                    {
                        action = Some(QueueAction::Down);
                    }
                    if ui
                        .add_enabled(n > 1, egui::Button::new("⏶").small())
                        .on_hover_text("Run sooner")
                        .clicked()
                    {
                        action = Some(QueueAction::Up);
                    }
                } else if state != JobState::Queued {
                    // The user's own pause; a job held only by the paused queue offers
                    // "Pause" and resumes with the queue.
                    let paused = j.cancel.is_paused_itself();
                    if ui
                        .small_button(if paused { "Resume" } else { "Pause" })
                        .clicked()
                    {
                        j.cancel.set_paused(!paused);
                    }
                }
            }
            ui.label(RichText::new(format!("{state:?}")).weak());
        });
    });
    if state == JobState::Queued {
        ui.label(
            RichText::new("Waiting for its turn. Conflicts are checked when it starts.")
                .small()
                .weak(),
        );
        return action;
    }
    let frac = snap.fraction();
    let secs = j.elapsed().as_secs_f64().max(0.001);
    let rate = snap.bytes_done as f64 / secs;
    let text = if snap.bytes_total > 0 {
        let eta = if rate > 0.0 && !state.is_finished() {
            let remain = (snap.bytes_total.saturating_sub(snap.bytes_done)) as f64 / rate;
            format!(", ~{}s left", remain.round() as u64)
        } else {
            String::new()
        };
        format!(
            "{} / {}  ({}/s{eta})",
            format_size(snap.bytes_done),
            format_size(snap.bytes_total),
            format_size(rate as u64)
        )
    } else if snap.items_total > 0 {
        format!("{} / {} items", snap.items_done, snap.items_total)
    } else {
        format!("{} items", snap.items_done)
    };
    match frac {
        Some(f) if !state.is_finished() => {
            ui.add(egui::ProgressBar::new(f).text(text));
        }
        None if !state.is_finished() => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(text);
            });
        }
        _ => {
            ui.label(RichText::new(text).weak());
        }
    }
    if !snap.current.is_empty() && !state.is_finished() {
        ui.label(RichText::new(&snap.current).small().weak());
    }
    if snap.errors > 0 {
        ui.label(
            RichText::new(format!("{} error(s)", snap.errors)).color(ui.visuals().error_fg_color),
        );
    }
    let log = j.log_lines();
    if !log.is_empty() {
        ui.collapsing(format!("Report ({} lines)", log.len()), |ui| {
            for l in log.iter().take(500) {
                ui.label(RichText::new(l).small().monospace());
            }
        });
    }
    action
}
