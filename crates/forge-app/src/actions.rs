//! Bridges UI commands to the engine crates: file operations, dialogs, previews, jobs.

use std::path::{Path, PathBuf};

use egui::{RichText, Ui};

use forge_core::ops::OpKind;

use crate::app::ZxrawApp;
use crate::commands::Cmd;
use crate::modal::{self, Confirm, ConfirmKind, Modal, Outcome, Prompt, PromptKind};
use crate::preview::{self, TextPreview};
use crate::settings::{Paths, Settings};
use crate::views::ThumbProvider;

/// Results reported back to the UI thread from background work.
pub enum Event {
    Toast(String),
    Error(String),
    /// A folder's contents changed; refresh panes showing it.
    Changed(PathBuf),
    Report { title: String, lines: Vec<String> },
    Conflict(crate::ops_ui::ConflictQuestion),
    Undo(UndoEntry),
}

/// Operations that can genuinely be reverted. Copies, overwrites and permanent deletes are
/// never recorded here because they cannot be undone.
pub enum UndoEntry {
    /// `(current, original)` pairs.
    Rename(Vec<(PathBuf, PathBuf)>),
    /// Records from the ops engine (moves on the same volume, created items, new copies).
    Ops(Vec<forge_core::ops::UndoRecord>),
}

/// In-app clipboard for files (Ctrl+C / Ctrl+X then Ctrl+V).
pub struct FileClipboard {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

pub struct Actions {
    pub modal: Option<Modal>,
    pub undo: Vec<UndoEntry>,
    pub conflicts: std::collections::VecDeque<crate::ops_ui::ConflictQuestion>,
    pub clipboard: Option<FileClipboard>,
    pub show_jobs: bool,
    pub batch_rename: Option<crate::batch_rename::BatchRename>,
    pub text_preview: TextPreview,
    pub jobs: forge_jobs::JobRegistry,
    pub tx: crossbeam_channel::Sender<Event>,
    rx: crossbeam_channel::Receiver<Event>,
    pub ctx: egui::Context,
    pub thumbs: crate::thumbs::ThumbManager,
}

impl Actions {
    pub fn new(paths: &Paths, settings: &Settings, ctx: egui::Context) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            modal: None,
            undo: Vec::new(),
            conflicts: Default::default(),
            clipboard: None,
            show_jobs: false,
            batch_rename: None,
            text_preview: TextPreview::default(),
            jobs: forge_jobs::JobRegistry::new(),
            tx,
            rx,
            ctx,
            thumbs: crate::thumbs::ThumbManager::new(
                paths.cache.join("thumbs"),
                settings.thumb_mem_cache_mb,
                settings.ffmpeg_dir.as_deref(),
            ),
        }
    }

    /// Run a short shell action on a background thread (COM calls can block on slow
    /// shell extensions; they must never freeze the UI).
    pub fn spawn_shell(&self, what: &'static str, f: impl FnOnce() -> Result<(), forge_win::WinError> + Send + 'static) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            if let Err(e) = f() {
                let _ = tx.send(Event::Error(format!("{what} failed: {e}")));
                ctx.request_repaint();
            }
        });
    }

    pub fn thumb_provider(&mut self) -> &mut dyn ThumbProvider {
        &mut self.thumbs
    }
}

pub fn modal_open(app: &ZxrawApp) -> bool {
    app.ops.modal.is_some() || app.ops.batch_rename.is_some() || !app.ops.conflicts.is_empty()
}

pub fn run(app: &mut ZxrawApp, cmd: Cmd, _ctx: &egui::Context) {
    let dir = app.pane().path.clone();
    match cmd {
        Cmd::NewFolder => {
            let name = forge_core::paths::unique_name(&dir, "New folder");
            let name = forge_core::paths::name_of(&name);
            app.ops.modal = Some(Modal::Prompt(Prompt::new(PromptKind::NewFolder { dir }, "New folder", name)));
        }
        Cmd::NewFile => {
            let name = forge_core::paths::unique_name(&dir, "New file.txt");
            let name = forge_core::paths::name_of(&name);
            app.ops.modal = Some(Modal::Prompt(Prompt::new(PromptKind::NewFile { dir }, "New file", name)));
        }
        Cmd::Rename => {
            if let Some(e) = app.pane().cursor_entry().cloned() {
                app.ops.modal = Some(Modal::Prompt(Prompt::new(
                    PromptKind::Rename { path: e.path.clone() },
                    "Rename",
                    e.name.clone(),
                )));
            }
        }
        Cmd::DeletePermanently => {
            let targets = app.pane().action_targets();
            if targets.is_empty() {
                return;
            }
            app.ops.modal = Some(Modal::Confirm(Confirm {
                body: format!(
                    "Permanently delete {} item(s)?\n\n{}\n\nThis bypasses the Recycle Bin and cannot be undone.",
                    targets.len(),
                    modal::describe_items(&targets)
                ),
                kind: ConfirmKind::DeletePermanently(targets),
                title: "Delete permanently".into(),
                confirm_label: "Delete permanently".into(),
                danger: true,
            }));
        }
        Cmd::Recycle => {
            let targets = app.pane().action_targets();
            if targets.is_empty() {
                return;
            }
            if app.settings.confirm_recycle {
                app.ops.modal = Some(Modal::Confirm(Confirm {
                    body: format!("Move {} item(s) to the Recycle Bin?

{}", targets.len(), modal::describe_items(&targets)),
                    kind: ConfirmKind::Recycle(targets),
                    title: "Delete".into(),
                    confirm_label: "Move to Recycle Bin".into(),
                    danger: false,
                }));
            } else {
                recycle(app, targets);
            }
        }
        Cmd::Properties => {
            let t = app.pane().action_targets();
            let p = t.into_iter().next().unwrap_or_else(|| dir.clone());
            app.ops.spawn_shell("Properties", move || forge_win::show_properties(&p));
        }
        Cmd::OpenWith => {
            if let Some(p) = app.pane().action_targets().into_iter().next() {
                app.ops.spawn_shell("Open with", move || forge_win::open_with_dialog(&p));
            }
        }
        Cmd::RevealInExplorer => {
            let p = app.pane().action_targets().into_iter().next().unwrap_or(dir);
            app.ops.spawn_shell("Show in Explorer", move || forge_win::reveal_in_explorer(&p));
        }
        Cmd::Copy | Cmd::Cut => {
            let paths = app.pane().action_targets();
            if !paths.is_empty() {
                let cut = cmd == Cmd::Cut;
                app.toast(format!("{} {} item(s)", if cut { "Cut" } else { "Copied" }, paths.len()));
                app.ops.clipboard = Some(FileClipboard { paths, cut });
            }
        }
        Cmd::Paste => {
            if let Some(cb) = app.ops.clipboard.take() {
                let kind = if cb.cut { OpKind::Move } else { OpKind::Copy };
                crate::ops_ui::start_transfer(app, cb.paths.clone(), dir, kind);
                // A cut is consumed by pasting; a copy can be pasted again.
                if !cb.cut {
                    app.ops.clipboard = Some(cb);
                }
            } else {
                app.toast("Clipboard is empty");
            }
        }
        Cmd::CopyToOtherPane | Cmd::MoveToOtherPane => {
            let Some(other) = app.tab().other().map(|p| p.path.clone()) else {
                app.toast("Open a second pane first (F3)");
                return;
            };
            let paths = app.pane().action_targets();
            let kind = if cmd == Cmd::CopyToOtherPane { OpKind::Copy } else { OpKind::Move };
            crate::ops_ui::start_transfer(app, paths, other, kind);
        }
        Cmd::ShowJobs => app.ops.show_jobs = !app.ops.show_jobs,
        Cmd::Undo => match app.ops.undo.pop() {
            None => app.toast("Nothing to undo"),
            Some(UndoEntry::Ops(records)) => crate::ops_ui::undo_ops(app, records),
            Some(UndoEntry::Rename(pairs)) => {
                let r = forge_core::rename::undo(&pairs);
                let dirs: std::collections::BTreeSet<PathBuf> =
                    pairs.iter().filter_map(|(_, o)| o.parent().map(Path::to_path_buf)).collect();
                for d in dirs {
                    app.refresh_all_showing(&d);
                }
                match r.status {
                    forge_core::rename::RenameStatus::Completed => app.toast(format!("Undid rename of {} item(s)", pairs.len())),
                    other => app.toast_err(format!("Undo failed: {other:?}")),
                }
            }
        },
        Cmd::BatchRename => {
            let items: Vec<PathBuf> = app.pane().action_targets();
            if items.is_empty() {
                app.toast("Select files to rename first");
            } else {
                app.ops.batch_rename = Some(crate::batch_rename::BatchRename::new(items));
            }
        }
        other => app.toast(format!("“{}” is not available yet", other.label())),
    }
}

pub fn recycle(app: &mut ZxrawApp, targets: Vec<PathBuf>) {
    let n = targets.len();
    let dirs: Vec<PathBuf> = targets.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops.jobs.spawn("delete", format!("Deleting {n} item(s)"), move |h| {
        h.progress.add_total_items(n as u64);
        let r = forge_win::recycle(&targets);
        h.progress.add_items(n as u64);
        let mut seen = std::collections::HashSet::new();
        for d in dirs {
            if seen.insert(d.clone()) {
                let _ = tx.send(Event::Changed(d));
            }
        }
        let msg = match &r {
            Ok(()) => Event::Toast(format!("Moved {n} item(s) to the Recycle Bin")),
            Err(forge_win::WinError::Aborted { .. }) => Event::Toast("Delete was cancelled; some items were not deleted".into()),
            Err(e) => Event::Error(format!("Delete failed: {e}")),
        };
        let _ = tx.send(msg);
        ctx.request_repaint();
        r.map_err(|e| e.to_string())
    });
}

pub fn transfer(app: &mut ZxrawApp, paths: Vec<PathBuf>, target: PathBuf, copy: bool) {
    let kind = if copy { OpKind::Copy } else { OpKind::Move };
    crate::ops_ui::start_transfer(app, paths, target, kind);
}

pub fn open_file(app: &mut ZxrawApp, path: &Path) {
    let p = path.to_path_buf();
    app.ops.spawn_shell("Open", move || forge_win::open_default(&p));
}

pub fn poll(app: &mut ZxrawApp) -> bool {
    let folders: Vec<PathBuf> = app
        .tabs
        .get(app.active)
        .map(|t| std::iter::once(t.left.path.clone()).chain(t.right.as_ref().map(|r| r.path.clone())).collect())
        .unwrap_or_default();
    let mut changed = app.ops.thumbs.begin_frame(folders);
    changed |= app.ops.thumbs.upload_ready(&app.ops.ctx);
    while let Ok(ev) = app.ops.rx.try_recv() {
        changed = true;
        match ev {
            Event::Toast(t) => app.toast(t),
            Event::Error(e) => app.toast_err(e),
            Event::Changed(d) => {
                app.ops.thumbs.invalidate_dir(&d);
                app.refresh_all_showing(&d);
            }
            Event::Report { title, lines } => {
                if app.ops.modal.is_none() {
                    app.ops.modal = Some(Modal::Report { title, lines });
                } else {
                    app.toast_err(format!("{title} — see Operations panel"));
                }
            }
            Event::Conflict(q) => app.ops.conflicts.push_back(q),
            Event::Undo(u) => {
                app.ops.undo.push(u);
                if app.ops.undo.len() > 50 {
                    app.ops.undo.remove(0);
                }
            }
        }
    }
    changed
}

pub fn status_widgets(app: &mut ZxrawApp, ui: &mut Ui) {
    let running = app.ops.jobs.running_count();
    let label = if running > 0 {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        let fr = app
            .ops
            .jobs
            .list()
            .iter()
            .filter(|j| !j.state().is_finished())
            .filter_map(|j| j.progress.snapshot().fraction())
            .next();
        match fr {
            Some(f) => format!("⟳ {running} operation(s) · {:.0}%", f * 100.0),
            None => format!("⟳ {running} operation(s)"),
        }
    } else {
        "Operations".to_string()
    };
    if ui.selectable_label(app.ops.show_jobs, label).clicked() {
        app.ops.show_jobs = !app.ops.show_jobs;
    }
    if let Some(cb) = &app.ops.clipboard {
        ui.label(RichText::new(format!("📋 {} {}", cb.paths.len(), if cb.cut { "cut" } else { "copied" })).weak());
    }
}

pub fn shutdown(app: &mut ZxrawApp) {
    // Cancel running jobs; copies clean up their partial files on cancel.
    app.ops.jobs.cancel_all();
    for q in app.ops.conflicts.drain(..) {
        let _ = q.reply.send(forge_core::ops::Resolution::once(forge_core::ops::ConflictDecision::Cancel));
    }
    let t0 = std::time::Instant::now();
    while app.ops.jobs.running_count() > 0 && t0.elapsed() < std::time::Duration::from_secs(3) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

pub fn modals(app: &mut ZxrawApp, ui: &mut Ui) {
    crate::ops_ui::jobs_window(app, ui.ctx());
    if crate::ops_ui::conflict_dialog(app, ui.ctx()) {
        return;
    }
    if let Some(mut br) = app.ops.batch_rename.take() {
        match br.show(ui.ctx()) {
            crate::batch_rename::BrOutcome::Open => app.ops.batch_rename = Some(br),
            crate::batch_rename::BrOutcome::Closed => {}
            crate::batch_rename::BrOutcome::Applied { dirs, report, undo } => {
                if !undo.is_empty() {
                    app.ops.undo.push(UndoEntry::Rename(undo));
                }
                for d in dirs {
                    app.refresh_all_showing(&d);
                }
                app.ops.modal = Some(Modal::Report { title: "Batch rename".into(), lines: report });
            }
        }
    }
    let Some(mut m) = app.ops.modal.take() else { return };
    match modal::show(&mut m, ui.ctx()) {
        Outcome::None => app.ops.modal = Some(m),
        Outcome::Cancel => {}
        Outcome::Accept => {
            if let Some(m) = accept(app, m) {
                app.ops.modal = Some(m);
            }
        }
    }
}

/// Apply an accepted dialog. Returns the dialog again (with an error) if it must stay open.
fn accept(app: &mut ZxrawApp, m: Modal) -> Option<Modal> {
    match m {
        Modal::Prompt(mut p) => {
            let name = p.text.trim().to_string();
            if let Err(e) = forge_core::paths::validate_file_name(&name) {
                p.error = Some(e.to_string());
                return Some(Modal::Prompt(p));
            }
            let (res, dir) = match &p.kind {
                PromptKind::NewFolder { dir } => (std::fs::create_dir(dir.join(&name)), dir.clone()),
                PromptKind::NewFile { dir } => (
                    std::fs::OpenOptions::new().write(true).create_new(true).open(dir.join(&name)).map(|_| ()),
                    dir.clone(),
                ),
                PromptKind::Rename { path } => {
                    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
                    let target = dir.join(&name);
                    let case_only = target != *path && forge_core::paths::paths_equal(&target, path);
                    let res = if target.exists() && !case_only {
                        Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "a file with that name already exists"))
                    } else {
                        std::fs::rename(path, &target)
                    };
                    if res.is_ok() {
                        app.ops.undo.push(UndoEntry::Rename(vec![(target.clone(), path.clone())]));
                    }
                    (res, dir)
                }
            };
            match res {
                Ok(()) => {
                    app.refresh_all_showing(&dir);
                    app.pane_mut().set_pending_select(name);
                    None
                }
                Err(e) => {
                    p.error = Some(e.to_string());
                    Some(Modal::Prompt(p))
                }
            }
        }
        Modal::Confirm(c) => {
            match c.kind {
                ConfirmKind::Recycle(t) => recycle(app, t),
                ConfirmKind::DeletePermanently(t) => delete_permanently(app, t),
            }
            None
        }
        Modal::Report { .. } => None,
    }
}

pub fn preview_panel(app: &mut ZxrawApp, ui: &mut Ui) {
    let Some(e) = app.pane().cursor_entry().cloned() else {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| ui.label(RichText::new("Select a file to preview").weak()));
        return;
    };
    preview::header(ui, &e);
    ui.separator();
    preview::info_grid(ui, &e);
    if TextPreview::is_text(&e) {
        ui.separator();
        match app.ops.text_preview.get(&e.path, ui.ctx()) {
            Some(Ok(t)) => {
                egui::ScrollArea::both().show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(t).monospace().size(11.5)).extend());
                });
            }
            Some(Err(err)) => {
                ui.label(RichText::new(err).weak());
            }
            None => {
                ui.spinner();
            }
        }
    }
}

fn delete_permanently(app: &mut ZxrawApp, targets: Vec<PathBuf>) {
    crate::ops_ui::delete_permanently(app, targets);
}
