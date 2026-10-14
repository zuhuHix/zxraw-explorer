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
use crate::modal::{Confirm, ConfirmKind, Modal};
use crate::views;

/// Online-only sources at or above this count, or this total size, ask before a transfer
/// because reading them downloads everything from the cloud.
const CLOUD_WARN_COUNT: usize = 10;
const CLOUD_WARN_BYTES: u64 = 1 << 30;

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

/// Start a copy or move of `sources` into `dest` as a background job, after asking first
/// when it would download online-only files.
pub fn start_transfer(app: &mut ZxrawApp, sources: Vec<PathBuf>, dest: PathBuf, kind: OpKind) {
    if let Some(body) = cloud_warning_for(&sources, &dest, kind) {
        let pending = ConfirmKind::DownloadCloudFiles {
            sources,
            dest,
            kind,
            clipboard: None,
        };
        ask_cloud_download(app, body, kind, pending);
        return;
    }
    start_transfer_confirmed(app, sources, dest, kind);
}

/// [`start_transfer`] without the cloud check. Used when the user already accepted it.
pub fn start_transfer_confirmed(
    app: &mut ZxrawApp,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    kind: OpKind,
) {
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
    clipboard_move(app, sources, dest, generation, expected_sequence, true)
}

/// [`start_clipboard_move`] without the cloud check. Used when the user already accepted it.
pub fn start_clipboard_move_confirmed(
    app: &mut ZxrawApp,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    generation: Option<u64>,
    expected_sequence: Option<u32>,
) -> bool {
    clipboard_move(app, sources, dest, generation, expected_sequence, false)
}

fn clipboard_move(
    app: &mut ZxrawApp,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    generation: Option<u64>,
    expected_sequence: Option<u32>,
    check_cloud: bool,
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
    if check_cloud && let Some(body) = cloud_warning_for(&sources, &dest, OpKind::Move) {
        let pending = ConfirmKind::DownloadCloudFiles {
            sources: clipboard_sources,
            dest,
            kind: OpKind::Move,
            clipboard: Some((generation, expected_sequence)),
        };
        ask_cloud_download(app, body, OpKind::Move, pending);
        return true;
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

fn ask_cloud_download(app: &mut ZxrawApp, body: String, kind: OpKind, pending: ConfirmKind) {
    let verb = if kind == OpKind::Copy { "copy" } else { "move" };
    app.ops.modal = Some(Modal::Confirm(Confirm {
        kind: pending,
        title: "Download from the cloud?".into(),
        body,
        confirm_label: format!("Download and {verb}"),
        danger: false,
    }));
}

/// The confirmation text when a transfer of `sources` to `dest` would download online-only
/// files, or `None`.
fn cloud_warning_for(sources: &[PathBuf], dest: &Path, kind: OpKind) -> Option<String> {
    let same = sources.iter().all(|path| same_volume(path, dest));
    let (count, bytes) = online_only_totals(sources, dest, kind);
    cloud_download_warning(count, bytes, kind, same)
}

/// Top-level online-only files that the transfer would read, and their logical size.
/// Metadata only: placeholders report their full size without being opened. Folders are not
/// walked (listing a cloud folder is too slow), so online-only files inside a folder are
/// not counted.
fn online_only_totals(sources: &[PathBuf], dest: &Path, kind: OpKind) -> (usize, u64) {
    sources.iter().fold((0, 0), |(count, bytes), path| {
        if kind == OpKind::Move && same_volume(path, dest) {
            return (count, bytes);
        }
        match std::fs::symlink_metadata(path) {
            Ok(md) if md.is_file() && !forge_core::cloud::background_read_allowed(path) => {
                (count + 1, bytes + md.len())
            }
            _ => (count, bytes),
        }
    })
}

/// Are both paths on the same drive (or share the same UNC share / root)?
fn same_volume(a: &Path, b: &Path) -> bool {
    let root = |p: &Path| {
        p.components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
    };
    root(a) == root(b)
}

/// Pure decision: the body text of the download warning, or `None` when the transfer is
/// cheap enough to start at once. A move within one volume is a rename and downloads nothing.
fn cloud_download_warning(
    count: usize,
    bytes: u64,
    kind: OpKind,
    same_volume: bool,
) -> Option<String> {
    if kind == OpKind::Move && same_volume {
        return None;
    }
    if count < CLOUD_WARN_COUNT && bytes < CLOUD_WARN_BYTES {
        return None;
    }
    let (files, are, them) = if count == 1 {
        ("1 file".to_owned(), "is", "it")
    } else {
        (format!("{count} files"), "are", "them")
    };
    let verb = if kind == OpKind::Copy {
        "Copying"
    } else {
        "Moving"
    };
    Some(format!(
        "{files} ({}) {are} online-only. {verb} {them} downloads {them} from the cloud first.",
        format_size(bytes)
    ))
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
    app.ops.jobs.spawn("transfer", title, move |job| {
        let opts = OpOptions {
            policy,
            verify,
            force_copy_for_move,
        };
        let plan = match ops::plan(kind, &sources, &dest, opts, &job.cancel) {
            Ok(plan) => plan,
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
        let report = ops::execute(&plan, job, &mut resolver);
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
    });
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

pub fn jobs_window(app: &mut ZxrawApp, ctx: &egui::Context) {
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
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .show(ui, |ui| {
                    for j in jobs.iter().rev() {
                        job_row(ui, j);
                        ui.separator();
                    }
                });
            if ui.button("Clear finished").clicked() {
                app.ops.jobs.clear_finished();
            }
        });
    app.ops.show_jobs = open;
}

fn job_row(ui: &mut Ui, j: &JobHandle) {
    let snap = j.progress.snapshot();
    let state = j.state();
    ui.horizontal(|ui| {
        ui.label(RichText::new(j.title.as_ref()).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !state.is_finished() {
                if ui.small_button("Cancel").clicked() {
                    j.cancel.cancel();
                }
                let paused = state == JobState::Paused;
                if ui
                    .small_button(if paused { "Resume" } else { "Pause" })
                    .clicked()
                {
                    j.cancel.set_paused(!paused);
                }
            }
            ui.label(RichText::new(format!("{state:?}")).weak());
        });
    });
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
}

#[cfg(test)]
mod cloud_warning_tests {
    use super::*;

    #[test]
    fn small_transfers_do_not_warn() {
        assert_eq!(cloud_download_warning(0, 0, OpKind::Copy, false), None);
        assert_eq!(
            cloud_download_warning(CLOUD_WARN_COUNT - 1, 1024, OpKind::Copy, false),
            None
        );
        assert_eq!(
            cloud_download_warning(1, CLOUD_WARN_BYTES - 1, OpKind::Copy, false),
            None
        );
    }

    #[test]
    fn count_or_size_threshold_warns() {
        let body = cloud_download_warning(CLOUD_WARN_COUNT, 3, OpKind::Copy, false).unwrap();
        assert_eq!(
            body,
            "10 files (3 B) are online-only. Copying them downloads them from the cloud first."
        );
        assert!(cloud_download_warning(1, CLOUD_WARN_BYTES, OpKind::Copy, false).is_some());
        let single = cloud_download_warning(1, CLOUD_WARN_BYTES, OpKind::Move, false).unwrap();
        assert!(single.starts_with(&format!(
            "1 file ({}) is online-only. Moving it downloads it",
            format_size(CLOUD_WARN_BYTES)
        )));
    }

    #[test]
    fn move_within_one_volume_never_warns() {
        assert_eq!(
            cloud_download_warning(50, CLOUD_WARN_BYTES * 4, OpKind::Move, true),
            None
        );
        assert!(cloud_download_warning(50, 0, OpKind::Move, false).is_some());
        assert!(cloud_download_warning(50, 0, OpKind::Copy, true).is_some());
    }

    #[test]
    fn same_volume_compares_roots_case_insensitively() {
        assert!(same_volume(Path::new("C:/a/x.jpg"), Path::new("c:/b")));
        assert!(!same_volume(Path::new("C:/a/x.jpg"), Path::new("D:/b")));
    }

    #[test]
    fn totals_count_only_online_only_top_level_files() {
        let dir = tempfile::tempdir().expect("temp directory");
        let cloud = dir.path().join("cloud.bin");
        let local = dir.path().join("local.bin");
        let folder = dir.path().join("folder");
        std::fs::write(&cloud, b"abc").unwrap();
        std::fs::write(&local, b"hello").unwrap();
        std::fs::create_dir(&folder).unwrap();
        forge_core::cloud::simulate_online_only(cloud.clone());
        forge_core::cloud::simulate_online_only(folder.clone());
        let sources = [cloud.clone(), local, folder.clone()];
        let elsewhere = dir.path().join("elsewhere");
        assert_eq!(
            online_only_totals(&sources, &elsewhere, OpKind::Copy),
            (1, 3)
        );
        // A move within the same volume is a rename: nothing is read.
        assert_eq!(
            online_only_totals(&sources, dir.path(), OpKind::Move),
            (0, 0)
        );
        forge_core::cloud::clear_simulated_online_only(&cloud);
        forge_core::cloud::clear_simulated_online_only(&folder);
    }
}
