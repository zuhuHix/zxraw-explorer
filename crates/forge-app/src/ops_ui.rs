//! Copy/move/delete jobs, the interactive conflict dialog and the operations panel.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::{Sender, bounded};
use egui::{RichText, Ui};
use forge_core::model::format_size;
use forge_core::ops::{
    self, ConflictDecision, ConflictInfo, ConflictPolicy, JobCtx, OpKind, OpOptions, OpReport, Resolution,
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

/// Start a copy or move of `sources` into `dest` as a background job.
pub fn start_transfer(app: &mut ZxrawApp, sources: Vec<PathBuf>, dest: PathBuf, kind: OpKind) {
    if sources.is_empty() {
        return;
    }
    let verify = app.settings.verify_copies;
    let verb = if kind == OpKind::Copy { "Copying" } else { "Moving" };
    let title = format!(
        "{verb} {} item(s) to {}",
        sources.len(),
        dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dest.display().to_string())
    );
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    let title2 = title.clone();
    app.ops.jobs.spawn("transfer", title, move |job| {
        let opts = OpOptions {
            policy: ConflictPolicy::Ask,
            verify,
            force_copy_for_move: false,
        };
        let plan = ops::plan(kind, &sources, &dest, opts, &job.cancel).map_err(|e| {
            let _ = tx.send(Event::Error(e.to_string()));
            ctx.request_repaint();
            e.to_string()
        })?;
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
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) if !cancel.is_cancelled() => continue,
                    _ => return Resolution::once(ConflictDecision::Cancel),
                }
            }
        };
        let report = ops::execute(&plan, job, &mut resolver);
        finish_report(job, &report, &tx, &[dest.clone()], sources.iter().filter_map(|s| s.parent().map(Path::to_path_buf)).collect());
        ctx.request_repaint();
        Ok(())
    });
}

fn finish_report(job: &JobHandle, report: &OpReport, tx: &Sender<Event>, dests: &[PathBuf], mut sources: Vec<PathBuf>) {
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
    app.ops.jobs.spawn("delete", format!("Permanently deleting {n} item(s)"), move |job| {
        let report = ops::delete_permanently(&targets, &JobCtx::from_job(job));
        let parents: Vec<PathBuf> = targets.iter().filter_map(|t| t.parent().map(Path::to_path_buf)).collect();
        finish_report(job, &report, &tx, &[], parents);
        ctx.request_repaint();
        Ok(())
    });
}

pub fn undo_ops(app: &mut ZxrawApp, records: Vec<ops::UndoRecord>) {
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops.jobs.spawn("undo", format!("Undoing {} change(s)", records.len()), move |job| {
        let mut errors = Vec::new();
        for r in records.iter().rev() {
            if let Err(e) = ops::undo(r) {
                errors.push(format!("{}: {e}", r.describe()));
                job.progress.add_error();
            }
            let touched: Vec<PathBuf> = match r {
                ops::UndoRecord::Renamed { from, to } | ops::UndoRecord::Moved { from, to } => vec![from.clone(), to.clone()],
                ops::UndoRecord::CreatedFolder { path } | ops::UndoRecord::CreatedFile { path } | ops::UndoRecord::Copied { path, .. } => vec![path.clone()],
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
            let _ = tx.send(Event::Report { title: "Undo was incomplete".into(), lines: errors });
        }
        ctx.request_repaint();
        Ok(())
    });
}

/// Conflict dialog. Returns true while a question is being shown.
pub fn conflict_dialog(app: &mut ZxrawApp, ctx: &egui::Context) -> bool {
    let Some(q) = app.ops.conflicts.front_mut() else { return false };
    let mut answer = None;
    egui::Modal::new(egui::Id::new("conflict")).show(ctx, |ui| {
        ui.set_width(520.0);
        ui.heading("File already exists");
        ui.label(RichText::new(&q.job_title).weak());
        ui.add_space(6.0);
        let name = q.info.dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        ui.label(format!("The destination already contains “{name}”."));
        ui.add_space(4.0);
        egui::Grid::new("cmp").num_columns(3).spacing([16.0, 4.0]).show(ui, |ui| {
            ui.label("");
            ui.label(RichText::new("Size").strong());
            ui.label(RichText::new("Modified").strong());
            ui.end_row();
            let newer = |a: Option<std::time::SystemTime>, b: Option<std::time::SystemTime>| a.zip(b).is_some_and(|(a, b)| a > b);
            ui.label("Incoming");
            ui.label(format_size(q.info.source_size));
            ui.label(format!(
                "{}{}",
                views::format_time(q.info.source_modified),
                if newer(q.info.source_modified, q.info.dest_modified) { "  (newer)" } else { "" }
            ));
            ui.end_row();
            ui.label("Existing");
            ui.label(format_size(q.info.dest_size));
            ui.label(format!(
                "{}{}",
                views::format_time(q.info.dest_modified),
                if newer(q.info.dest_modified, q.info.source_modified) { "  (newer)" } else { "" }
            ));
            ui.end_row();
        });
        ui.add_space(8.0);
        ui.checkbox(&mut q.apply_all, "Do this for all remaining conflicts");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("Keep both").on_hover_text("The incoming file gets a new name like “name (2)”").clicked() {
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
            if ui.add(replace).on_hover_text("The existing file is overwritten. This cannot be undone.").clicked() {
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
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
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
                if ui.small_button(if paused { "Resume" } else { "Pause" }).clicked() {
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
        ui.label(RichText::new(format!("{} error(s)", snap.errors)).color(ui.visuals().error_fg_color));
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
