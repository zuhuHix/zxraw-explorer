//! Bridges UI commands to the engine crates: file operations, dialogs, previews, jobs.

use std::path::{Path, PathBuf};

use egui::{RichText, Ui};

use forge_core::ops::OpKind;

use forge_core::query::{ColorLabel, PickState};
use forge_db::AnnotationChange;

use crate::app::ZxrawApp;
use crate::commands::Cmd;
use crate::media_meta::{self, MediaMeta};
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
    Report {
        title: String,
        lines: Vec<String>,
    },
    Conflict(crate::ops_ui::ConflictQuestion),
    Undo(UndoEntry),
    /// Files or folders were moved (`from` -> `to`) outside an undo record, or an undo moved
    /// them back: their library rows and notes follow.
    FilesMoved(Vec<(PathBuf, PathBuf)>),
    /// Our Copy/Cut reached the Windows clipboard (`Ok(sequence)`) or failed.
    ClipboardPublished {
        generation: u64,
        result: Result<u32, String>,
    },
    /// Paste: the Windows clipboard as read off the UI thread.
    PasteSystemRead {
        destination: PathBuf,
        sequence: u32,
        files: Option<(Vec<PathBuf>, forge_win::ClipboardEffect)>,
    },
    /// A clipboard-originated move finished (`ok` = everything moved, not cancelled).
    MoveFinished {
        sources: Vec<PathBuf>,
        ok: bool,
        clipboard_generation: Option<u64>,
        expected_sequence: Option<u32>,
    },
    /// "Download FFmpeg" finished; the folder holds ffmpeg.exe and ffprobe.exe.
    FfmpegInstalled(PathBuf),
    /// An update check finished.
    UpdateChecked(crate::updater::Checked),
    /// "Update now" downloaded and verified the installer, or failed.
    UpdateDownloaded(Result<PathBuf, String>),
}

/// Operations that can genuinely be reverted. Copies, overwrites and permanent deletes are
/// never recorded here because they cannot be undone.
pub enum UndoEntry {
    /// `(current, original)` pairs.
    Rename(Vec<(PathBuf, PathBuf)>),
    /// Records from the ops engine (moves on the same volume, created items, new copies).
    Ops(Vec<forge_core::ops::UndoRecord>),
    /// Find and replace: each file is restored from its backup if it is unchanged since.
    TextReplace(Vec<forge_core::text_replace::ReplaceUndo>),
}

/// In-app clipboard for files (Ctrl+C / Ctrl+X then Ctrl+V).
pub struct FileClipboard {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
    generation: u64,
    /// Windows clipboard sequence before we published (to detect later external copies).
    baseline_sequence: u32,
    /// `None` while publishing; `Some(Ok(seq))` once on the Windows clipboard.
    published: Option<Result<u32, ()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasteSource {
    InApp,
    System,
    Empty,
}

/// Which clipboard a paste should use. The newest copy wins: our in-app clipboard while
/// its publish to Windows is still pending or is still what Windows holds; the Windows
/// clipboard once something else has been copied since (e.g. in Explorer).
fn choose_paste_source(
    in_app: Option<&FileClipboard>,
    system_sequence: u32,
    system_has_files: bool,
) -> PasteSource {
    let Some(cb) = in_app else {
        return if system_has_files {
            PasteSource::System
        } else {
            PasteSource::Empty
        };
    };
    let ours = match cb.published {
        None => return PasteSource::InApp,
        Some(Ok(seq)) => seq,
        Some(Err(())) => cb.baseline_sequence,
    };
    if system_sequence != ours && system_has_files {
        PasteSource::System
    } else {
        PasteSource::InApp
    }
}

pub struct Actions {
    pub modal: Option<Modal>,
    pub undo: Vec<UndoEntry>,
    pub conflicts: std::collections::VecDeque<crate::ops_ui::ConflictQuestion>,
    pub clipboard: Option<FileClipboard>,
    clipboard_generation: u64,
    pub show_jobs: bool,
    pub batch_rename: Option<crate::batch_rename::BatchRename>,
    pub text_preview: TextPreview,
    pub meta: media_meta::MetaLoader,
    pub jobs: forge_jobs::JobRegistry,
    pub tx: crossbeam_channel::Sender<Event>,
    rx: crossbeam_channel::Receiver<Event>,
    pub ctx: egui::Context,
    pub thumbs: crate::thumbs::ThumbManager,
    pub library: crate::library::Library,
    pub ingest: crate::ingest_ui::IngestState,
    pub tools: crate::tools_ui::ToolsState,
    pub settings_ui: crate::settings_ui::SettingsUi,
    pub watcher: crate::watcher::FolderWatcher,
    pub collections_ui: crate::collections_ui::CollectionsUi,
    pub video_ui: crate::video_ui::VideoUi,
    pub extra_tools: crate::extra_tools_ui::ExtraToolsUi,
    pub tags_ui: crate::tags_ui::TagsUi,
    pub ffmpeg_setup: crate::ffmpeg_setup::FfmpegSetup,
    pub updater: crate::updater::Updater,
}

/// Grid thumbnail source that overlays library annotations (ratings/labels/picks).
pub struct GridProvider<'a> {
    thumbs: &'a mut crate::thumbs::ThumbManager,
    annotations: &'a std::collections::HashMap<PathBuf, crate::views::Annotation>,
}

impl ThumbProvider for GridProvider<'_> {
    fn thumb(
        &mut self,
        ui: &egui::Ui,
        e: &forge_core::FileEntry,
        size: u32,
    ) -> Option<egui::TextureHandle> {
        self.thumbs.thumb(ui, e, size)
    }
    fn annotation(&self, path: &Path) -> Option<crate::views::Annotation> {
        self.annotations
            .get(path)
            .copied()
            .filter(|a| *a != Default::default())
    }
}

impl Actions {
    pub fn new(paths: &Paths, settings: &Settings, ctx: egui::Context) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            modal: None,
            undo: Vec::new(),
            conflicts: Default::default(),
            clipboard: None,
            clipboard_generation: 0,
            show_jobs: false,
            batch_rename: None,
            text_preview: TextPreview::default(),
            meta: Default::default(),
            jobs: forge_jobs::JobRegistry::new(),
            tx,
            rx,
            ingest: Default::default(),
            tools: Default::default(),
            settings_ui: Default::default(),
            watcher: crate::watcher::FolderWatcher::new(ctx.clone()),
            collections_ui: Default::default(),
            video_ui: Default::default(),
            extra_tools: Default::default(),
            tags_ui: Default::default(),
            ffmpeg_setup: Default::default(),
            updater: Default::default(),
            library: crate::library::Library::open(paths.data.join("forge.db"), ctx.clone()),
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
    pub fn spawn_shell(
        &self,
        what: &'static str,
        f: impl FnOnce() -> Result<(), forge_win::WinError> + Send + 'static,
    ) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            if let Err(e) = f() {
                let _ = tx.send(Event::Error(format!("{what} failed: {e}")));
                ctx.request_repaint();
            }
        });
    }

    pub fn grid_provider(&mut self) -> GridProvider<'_> {
        GridProvider {
            thumbs: &mut self.thumbs,
            annotations: &self.library.annotations,
        }
    }
}

pub fn modal_open(app: &ZxrawApp) -> bool {
    app.ops.modal.is_some() || app.ops.batch_rename.is_some() || !app.ops.conflicts.is_empty()
}

/// Whether the selection is entirely hidden (filtered out, hidden files off, inside a
/// closed folder), so `verb` has nothing to act on; then a toast says why.
fn hidden_selection(app: &mut ZxrawApp, verb: &str) -> bool {
    let p = app.pane();
    let hidden = !p.selected.is_empty() && p.action_targets().is_empty();
    if hidden {
        app.toast(format!(
            "Nothing to {verb}: the selected items are hidden. Press Esc to clear the selection."
        ));
    }
    hidden
}

pub fn run(app: &mut ZxrawApp, cmd: Cmd, _ctx: &egui::Context) {
    let dir = app.pane().path.clone();
    match cmd {
        Cmd::NewFolder => {
            let name = forge_core::paths::unique_name(&dir, "New folder");
            let name = forge_core::paths::name_of(&name);
            app.ops.modal = Some(Modal::Prompt(Prompt::new(
                PromptKind::NewFolder { dir },
                "New folder",
                name,
            )));
        }
        Cmd::NewFile => {
            let name = forge_core::paths::unique_name(&dir, "New file.txt");
            let name = forge_core::paths::name_of(&name);
            app.ops.modal = Some(Modal::Prompt(Prompt::new(
                PromptKind::NewFile { dir },
                "New file",
                name,
            )));
        }
        Cmd::Rename => {
            if hidden_selection(app, "rename") {
                return;
            }
            if let Some(e) = app.pane().rename_target().cloned() {
                app.ops.modal = Some(Modal::Prompt(Prompt::new(
                    PromptKind::Rename {
                        path: e.path.clone(),
                    },
                    "Rename",
                    e.name.clone(),
                )));
            }
        }
        Cmd::DeletePermanently => {
            let targets = app.pane().action_targets();
            if targets.is_empty() {
                hidden_selection(app, "delete");
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
                hidden_selection(app, "delete");
                return;
            }
            if app.settings.confirm_recycle {
                app.ops.modal = Some(Modal::Confirm(Confirm {
                    body: format!(
                        "Move {} item(s) to the Recycle Bin?

{}",
                        targets.len(),
                        modal::describe_items(&targets)
                    ),
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
            // One item: the right-clicked (cursor) one when it is a target.
            let t = app.pane().rename_target().map(|e| e.path.clone());
            let p = t.unwrap_or_else(|| dir.clone());
            app.ops
                .spawn_shell("Properties", move || forge_win::show_properties(&p));
        }
        Cmd::OpenWith => {
            if let Some(p) = app.pane().rename_target().map(|e| e.path.clone()) {
                app.ops
                    .spawn_shell("Open with", move || forge_win::open_with_dialog(&p));
            }
        }
        Cmd::RevealInExplorer => {
            let p = app
                .pane()
                .action_targets()
                .into_iter()
                .next()
                .unwrap_or(dir);
            app.ops.spawn_shell("Show in Explorer", move || {
                forge_win::reveal_in_explorer(&p)
            });
        }
        Cmd::Copy | Cmd::Cut => {
            let paths = app.pane().action_targets();
            if !paths.is_empty() {
                let cut = cmd == Cmd::Cut;
                app.toast(format!(
                    "{} {} item(s)",
                    if cut { "Cut" } else { "Copied" },
                    paths.len()
                ));
                // Also publish to the Windows clipboard so Explorer and other apps can paste.
                let effect = if cut {
                    forge_win::ClipboardEffect::Move
                } else {
                    forge_win::ClipboardEffect::Copy
                };
                app.ops.clipboard_generation += 1;
                let generation = app.ops.clipboard_generation;
                let (tx, ctx, p2) = (app.ops.tx.clone(), app.ops.ctx.clone(), paths.clone());
                std::thread::spawn(move || {
                    let result = forge_win::set_files(&p2, effect)
                        .map(|()| forge_win::clipboard_sequence())
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Event::ClipboardPublished { generation, result });
                    ctx.request_repaint();
                });
                app.ops.clipboard = Some(FileClipboard {
                    paths,
                    cut,
                    generation,
                    baseline_sequence: forge_win::clipboard_sequence(),
                    published: None,
                });
            }
        }
        Cmd::Paste => {
            // Opening the Windows clipboard can block briefly (another app holding it), so
            // read it off the UI thread; `paste_with` decides once the result arrives.
            let (tx, ctx) = (app.ops.tx.clone(), app.ops.ctx.clone());
            std::thread::spawn(move || {
                let sequence = forge_win::clipboard_sequence();
                let files = forge_win::get_files().ok().flatten();
                let _ = tx.send(Event::PasteSystemRead {
                    destination: dir,
                    sequence,
                    files,
                });
                ctx.request_repaint();
            });
        }
        Cmd::CopyToOtherPane | Cmd::MoveToOtherPane => {
            let Some(other) = app.tab().other().map(|p| p.path.clone()) else {
                app.toast("Open a second pane first (F3)");
                return;
            };
            let paths = app.pane().action_targets();
            let kind = if cmd == Cmd::CopyToOtherPane {
                OpKind::Copy
            } else {
                OpKind::Move
            };
            crate::ops_ui::start_transfer(app, paths, other, kind);
        }
        Cmd::ShowJobs => app.ops.show_jobs = !app.ops.show_jobs,
        Cmd::Rate0 | Cmd::Rate1 | Cmd::Rate2 | Cmd::Rate3 | Cmd::Rate4 | Cmd::Rate5 => {
            let r = [
                Cmd::Rate0,
                Cmd::Rate1,
                Cmd::Rate2,
                Cmd::Rate3,
                Cmd::Rate4,
                Cmd::Rate5,
            ]
            .iter()
            .position(|c| *c == cmd)
            .unwrap_or(0) as u8;
            annotate_targets(
                app,
                AnnotationChange {
                    rating: Some(r),
                    ..Default::default()
                },
            );
        }
        Cmd::LabelRed
        | Cmd::LabelYellow
        | Cmd::LabelGreen
        | Cmd::LabelBlue
        | Cmd::LabelPurple
        | Cmd::LabelNone => {
            let label = match cmd {
                Cmd::LabelRed => Some(ColorLabel::Red),
                Cmd::LabelYellow => Some(ColorLabel::Yellow),
                Cmd::LabelGreen => Some(ColorLabel::Green),
                Cmd::LabelBlue => Some(ColorLabel::Blue),
                Cmd::LabelPurple => Some(ColorLabel::Purple),
                _ => None,
            };
            annotate_targets(
                app,
                AnnotationChange {
                    label: Some(label),
                    ..Default::default()
                },
            );
        }
        Cmd::MarkPick | Cmd::MarkReject | Cmd::Unflag => {
            let pick = match cmd {
                Cmd::MarkPick => PickState::Pick,
                Cmd::MarkReject => PickState::Reject,
                _ => PickState::None,
            };
            annotate_targets(
                app,
                AnnotationChange {
                    pick: Some(pick),
                    ..Default::default()
                },
            );
        }
        Cmd::IndexFolder => {
            let Some(db) = app.ops.library.db.clone() else {
                app.toast_err("Library database is not available");
                return;
            };
            let tx = app.ops.tx.clone();
            let ctx = app.ops.ctx.clone();
            crate::library::start_index_job(&app.ops.jobs, db, dir, move |msg| {
                let _ = tx.send(Event::Toast(msg));
                ctx.request_repaint();
            });
            app.toast("Indexing started — see Operations");
        }
        Cmd::Ingest => app.ops.ingest.open = true,
        Cmd::Settings => app.ops.settings_ui.open = true,
        Cmd::DownloadFfmpeg => crate::ffmpeg_setup::confirm(app),
        Cmd::CheckForUpdates => crate::updater::check_now(app),
        Cmd::OpenIn => crate::settings_ui::open_in(app),
        Cmd::Collections => app.ops.collections_ui.open = true,
        Cmd::AddToCollection => crate::collections_ui::add_selection(app),
        Cmd::GenerateProxies => crate::video_ui::generate_proxies(app),
        Cmd::ContactSheet => crate::extra_tools_ui::contact_sheet(app),
        Cmd::BrowseArchive => crate::extra_tools_ui::browse_archive(app),
        Cmd::CreateArchive => crate::extra_tools_ui::create_archive(app),
        Cmd::SplitFile => crate::extra_tools_ui::split_file(app),
        Cmd::JoinFiles => crate::extra_tools_ui::join_files(app),
        Cmd::GroupSimilar => crate::similar::toggle(app),
        Cmd::ToggleStickySelection => {
            let pane = app.pane_mut();
            pane.sticky = !pane.sticky;
        }
        Cmd::ExportMetadata => crate::meta_export::open(app),
        Cmd::LocateProxies => crate::video_ui::locate_proxies(app),
        Cmd::SelectOriginals => crate::export_assoc::select_originals(app),
        Cmd::SelectExports => crate::export_assoc::select_exports(app),
        Cmd::BatchExport => crate::batch_export::open(app),
        Cmd::Deliver => crate::batch_export::deliver(app),
        Cmd::EditMetadata => crate::meta_edit::open(app),
        Cmd::SyncFolders => crate::tools_ui::open_sync(app),
        Cmd::FindReplace => crate::replace_ui::open(app),
        Cmd::VerifyBackup => crate::tools_ui::open_verify_backup(app),
        Cmd::FolderSize | Cmd::FindDuplicates | Cmd::ChecksumFiles | Cmd::CompareFolders => {
            use crate::tools_ui::Tool;
            let tool = match cmd {
                Cmd::FolderSize => Tool::FolderSize,
                Cmd::FindDuplicates => Tool::Duplicates,
                Cmd::ChecksumFiles => Tool::Checksums,
                _ => Tool::CompareFolders,
            };
            let sel: Vec<PathBuf> = app.pane().selected_roots();
            let other = app.tab().other().map(|p| p.path.clone());
            app.ops.tools.open(tool, dir, sel, other);
        }
        Cmd::Search => {
            app.ops.library.search.open = true;
            app.ops.library.search.focus = true;
        }
        Cmd::Undo => match app.ops.undo.pop() {
            None => app.toast("Nothing to undo"),
            Some(UndoEntry::Ops(records)) => crate::ops_ui::undo_ops(app, records),
            Some(UndoEntry::TextReplace(records)) => crate::replace_ui::undo(app, records),
            Some(UndoEntry::Rename(pairs)) => {
                let r = forge_core::rename::undo(&pairs);
                // Only the pairs that really moved back: a partial undo follows just those.
                let undone: Vec<(PathBuf, PathBuf)> = match r.status {
                    forge_core::rename::RenameStatus::Completed => pairs.clone(),
                    _ => pairs
                        .iter()
                        .filter(|(current, original)| original.exists() && !current.exists())
                        .cloned()
                        .collect(),
                };
                follow_moves(app, undone);
                let dirs: std::collections::BTreeSet<PathBuf> = pairs
                    .iter()
                    .filter_map(|(_, o)| o.parent().map(Path::to_path_buf))
                    .collect();
                for d in dirs {
                    app.refresh_all_showing(&d);
                }
                match r.status {
                    forge_core::rename::RenameStatus::Completed => {
                        app.toast(format!("Undid rename of {} item(s)", pairs.len()))
                    }
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

fn annotate_targets(app: &mut ZxrawApp, change: AnnotationChange) {
    let targets: Vec<PathBuf> = app
        .pane()
        .action_targets()
        .into_iter()
        .filter(|p| p.is_file())
        .collect();
    if targets.is_empty() {
        app.toast("Select files to rate or label");
        return;
    }
    app.ops.library.annotate(targets, change);
}

fn paste_with(
    app: &mut ZxrawApp,
    dest: PathBuf,
    sequence: u32,
    files: Option<(Vec<PathBuf>, forge_win::ClipboardEffect)>,
) {
    match choose_paste_source(app.ops.clipboard.as_ref(), sequence, files.is_some()) {
        PasteSource::Empty => app.toast("Clipboard is empty"),
        PasteSource::System => {
            let Some((paths, effect)) = files else { return };
            if effect == forge_win::ClipboardEffect::Move {
                if !crate::ops_ui::start_clipboard_move(app, paths, dest, None, Some(sequence)) {
                    app.toast("Nothing to move: the items are already in this folder");
                }
            } else {
                crate::ops_ui::start_transfer(app, paths, dest, OpKind::Copy);
            }
        }
        PasteSource::InApp => {
            let Some(cb) = app.ops.clipboard.as_ref() else {
                return;
            };
            let (paths, cut, generation) = (cb.paths.clone(), cb.cut, cb.generation);
            let published = match cb.published {
                Some(Ok(seq)) => Some(seq),
                _ => None,
            };
            if cut {
                if !crate::ops_ui::start_clipboard_move(
                    app,
                    paths,
                    dest,
                    Some(generation),
                    published,
                ) {
                    app.toast("Nothing to move: the items are already in this folder");
                }
            } else {
                crate::ops_ui::start_transfer(app, paths, dest, OpKind::Copy);
            }
        }
    }
}

pub fn recycle(app: &mut ZxrawApp, targets: Vec<PathBuf>) {
    let n = targets.len();
    let dirs: Vec<PathBuf> = targets
        .iter()
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect();
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops
        .jobs
        .spawn("delete", format!("Deleting {n} item(s)"), move |h| {
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
                Err(forge_win::WinError::Aborted { .. }) => {
                    Event::Toast("Delete was cancelled; some items were not deleted".into())
                }
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
    app.ops
        .spawn_shell("Open", move || forge_win::open_default(&p));
}

pub fn poll(app: &mut ZxrawApp) -> bool {
    crate::tags_ui::tick(app);
    let folders: Vec<PathBuf> = app
        .tabs
        .get(app.active)
        .map(|t| {
            std::iter::once(t.left.path.clone())
                .chain(t.right.as_ref().map(|r| r.path.clone()))
                .collect()
        })
        .unwrap_or_default();
    for f in &folders {
        app.ops.library.ensure_dir(f);
    }
    for n in app.ops.library.poll() {
        match n {
            Ok(t) => app.toast(t),
            Err(e) => app.toast_err(e),
        }
    }
    // Folders opened in place are watched too (non-recursively, like the panes' folders),
    // and stop being watched when they are closed.
    let watched: Vec<PathBuf> = app
        .tabs
        .get(app.active)
        .map(|t| {
            std::iter::once(&t.left)
                .chain(t.right.as_ref())
                .flat_map(|p| p.open_folders().map(Path::to_path_buf))
                .collect()
        })
        .unwrap_or_default();
    let watched: Vec<PathBuf> = folders.iter().cloned().chain(watched).collect();
    app.ops.watcher.sync(&watched);
    let tabs = &app.tabs;
    let due = app.ops.watcher.due(|d| {
        tabs.iter()
            .flat_map(|t| std::iter::once(&t.left).chain(t.right.as_ref()))
            .find(|p| p.path == d)
            .map(|p| p.entries.len())
            .unwrap_or(0)
    });
    for d in &due {
        app.ops.thumbs.invalidate_dir(d);
        app.refresh_all_showing(d);
    }
    let mut changed = app.ops.thumbs.begin_frame(folders) || !due.is_empty();
    changed |= app.ops.thumbs.upload_ready(&app.ops.ctx);
    // A finished "Select originals/exports" match (computed off the UI thread).
    changed |= crate::export_assoc::poll(app);
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
            Event::ClipboardPublished { generation, result } => {
                if let Some(cb) = app
                    .ops
                    .clipboard
                    .as_mut()
                    .filter(|cb| cb.generation == generation)
                {
                    cb.published = Some(result.as_ref().map(|s| *s).map_err(|_| ()));
                }
                if let Err(e) = result {
                    tracing::warn!("publishing files to the Windows clipboard failed: {e}");
                }
            }
            Event::PasteSystemRead {
                destination,
                sequence,
                files,
            } => paste_with(app, destination, sequence, files),
            Event::MoveFinished {
                sources,
                ok,
                clipboard_generation,
                expected_sequence,
            } => {
                // A cut is consumed only by a complete, successful move (like Explorer); a
                // failed or cancelled move leaves it on the clipboard to retry.
                if ok {
                    if clipboard_generation.is_some()
                        && app.ops.clipboard.as_ref().map(|cb| cb.generation)
                            == clipboard_generation
                    {
                        app.ops.clipboard = None;
                    }
                    if let Some(seq) = expected_sequence {
                        std::thread::spawn(move || {
                            forge_win::clear_if_owned_files(&sources, Some(seq));
                        });
                    }
                }
            }
            Event::FfmpegInstalled(dir) => {
                crate::settings_ui::use_ffmpeg_dir(app, Some(dir));
                app.notices.video_tools_changed();
                app.save_all();
                app.toast("FFmpeg installed: video thumbnails and clip metadata are on");
            }
            Event::UpdateChecked(checked) => crate::updater::checked(app, checked),
            Event::UpdateDownloaded(result) => crate::updater::downloaded(app, result),
            Event::Undo(u) => {
                if let UndoEntry::Ops(records) = &u {
                    follow_moves(app, undo_record_moves(records));
                }
                app.ops.undo.push(u);
                if app.ops.undo.len() > 50 {
                    app.ops.undo.remove(0);
                }
            }
            Event::FilesMoved(pairs) => follow_moves(app, pairs),
        }
    }
    changed
}

/// ZXraw moved or renamed files (`from` -> `to`): the note being edited and the library rows
/// (notes, ratings, tags) follow them.
pub fn follow_moves(app: &mut ZxrawApp, pairs: Vec<(PathBuf, PathBuf)>) {
    app.ops.tags_ui.follow_moves(&pairs);
    app.ops.library.follow_moves(pairs);
}

/// The `(from, to)` moves recorded by an operation's undo records (renames and moves).
pub fn undo_record_moves(records: &[forge_core::ops::UndoRecord]) -> Vec<(PathBuf, PathBuf)> {
    records
        .iter()
        .filter_map(|r| match r {
            forge_core::ops::UndoRecord::Renamed { from, to }
            | forge_core::ops::UndoRecord::Moved { from, to } => Some((from.clone(), to.clone())),
            _ => None,
        })
        .collect()
}

pub fn status_widgets(app: &mut ZxrawApp, ui: &mut Ui) {
    let running = app.ops.jobs.running_count();
    let label = if running > 0 {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
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
        ui.label(
            RichText::new(format!(
                "📋 {} {}",
                cb.paths.len(),
                if cb.cut { "cut" } else { "copied" }
            ))
            .weak(),
        );
    }
}

pub fn shutdown(app: &mut ZxrawApp) {
    crate::tags_ui::flush_for_exit(app);
    // Cancel running jobs; copies clean up their partial files on cancel.
    app.ops.jobs.cancel_all();
    for q in app.ops.conflicts.drain(..) {
        let _ = q.reply.send(forge_core::ops::Resolution::once(
            forge_core::ops::ConflictDecision::Cancel,
        ));
    }
    let t0 = std::time::Instant::now();
    while app.ops.jobs.running_count() > 0 && t0.elapsed() < std::time::Duration::from_secs(3) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

pub fn modals(app: &mut ZxrawApp, ui: &mut Ui) {
    crate::ops_ui::jobs_window(app, ui.ctx());
    crate::search_ui::window(app, ui.ctx());
    crate::ingest_ui::window(app, ui.ctx());
    crate::tags_ui::window(app, ui.ctx());
    crate::tools_ui::windows(app, ui.ctx());
    crate::settings_ui::window(app, ui.ctx());
    crate::collections_ui::window(app, ui.ctx());
    crate::video_ui::window(app, ui.ctx());
    crate::extra_tools_ui::window(app, ui.ctx());
    crate::meta_export::window(app, ui.ctx());
    crate::batch_export::window(app, ui.ctx());
    crate::replace_ui::window(app, ui.ctx());
    crate::meta_edit::window(app, ui.ctx());
    crate::ops_ui::poll_cloud_check(app);
    if crate::ops_ui::conflict_dialog(app, ui.ctx()) {
        return;
    }
    if let Some(mut br) = app.ops.batch_rename.take() {
        match br.show(ui.ctx()) {
            crate::batch_rename::BrOutcome::Open => app.ops.batch_rename = Some(br),
            crate::batch_rename::BrOutcome::Closed => {}
            crate::batch_rename::BrOutcome::Applied { dirs, report, undo } => {
                // Undo pairs are (current, original): the files now sit at `current`.
                follow_moves(
                    app,
                    undo.iter()
                        .map(|(current, original)| (original.clone(), current.clone()))
                        .collect(),
                );
                if !undo.is_empty() {
                    app.ops.undo.push(UndoEntry::Rename(undo));
                }
                for d in dirs {
                    app.refresh_all_showing(&d);
                }
                app.ops.modal = Some(Modal::Report {
                    title: "Batch rename".into(),
                    lines: report,
                });
            }
        }
    }
    let Some(mut m) = app.ops.modal.take() else {
        return;
    };
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
pub(crate) fn accept(app: &mut ZxrawApp, m: Modal) -> Option<Modal> {
    match m {
        Modal::Prompt(p) if p.kind.is_label() => match p.kind {
            PromptKind::RenamePin { .. } => crate::sidebar::accept_label(app, p),
            _ => crate::layouts::accept_label(app, p),
        },
        Modal::Prompt(mut p) => {
            let name = p.text.trim().to_string();
            if let Err(e) = forge_core::paths::validate_file_name(&name) {
                p.error = Some(e.to_string());
                return Some(Modal::Prompt(p));
            }
            let (res, dir) = match &p.kind {
                PromptKind::NewFolder { dir } => {
                    (std::fs::create_dir(dir.join(&name)), dir.clone())
                }
                PromptKind::NewFile { dir } => (
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(dir.join(&name))
                        .map(|_| ()),
                    dir.clone(),
                ),
                PromptKind::Rename { path } => {
                    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
                    let target = dir.join(&name);
                    let case_only =
                        target != *path && forge_core::paths::paths_equal(&target, path);
                    let res = if target.exists() && !case_only {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            "a file with that name already exists",
                        ))
                    } else {
                        std::fs::rename(path, &target)
                    };
                    if res.is_ok() {
                        follow_moves(app, vec![(path.clone(), target.clone())]);
                        app.ops
                            .undo
                            .push(UndoEntry::Rename(vec![(target.clone(), path.clone())]));
                    }
                    (res, dir)
                }
                PromptKind::SaveLayout
                | PromptKind::RenameLayout { .. }
                | PromptKind::RenamePin { .. } => unreachable!("labels are accepted above"),
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
                ConfirmKind::DownloadFfmpeg => crate::ffmpeg_setup::start(app),
                ConfirmKind::DownloadCloudFiles {
                    sources,
                    dest,
                    kind,
                    clipboard: None,
                } => crate::ops_ui::start_transfer_confirmed(app, sources, dest, kind),
                ConfirmKind::DownloadCloudFiles {
                    sources,
                    dest,
                    clipboard: Some((generation, sequence)),
                    ..
                } => {
                    if !crate::ops_ui::start_clipboard_move_confirmed(
                        app, sources, dest, generation, sequence,
                    ) {
                        app.toast("Nothing to move: the items are already in this folder");
                    }
                }
                ConfirmKind::CheckingCloudFiles {
                    sources,
                    dest,
                    kind,
                    clipboard,
                    ..
                } => crate::ops_ui::start_pending_transfer(app, sources, dest, kind, clipboard),
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
    if preview::is_media(&e) {
        // Large preview from the thumbnail pipeline (largest bucket).
        let w = ui.available_width().min(512.0);
        if let Some(tex) = app.ops.thumbs.thumb(ui, &e, w as u32) {
            let sz = tex.size_vec2();
            let scale = (w / sz.x).min(w / sz.y);
            ui.vertical_centered(|ui| ui.image((tex.id(), sz * scale)));
        } else {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| ui.spinner());
        }
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .id_salt("preview-meta")
            .show(ui, |ui| {
                preview::info_grid(ui, &e);
                ui.separator();
                match app.ops.meta.get(&e, ui.ctx()) {
                    Some(Ok(MediaMeta::Image(m))) => media_meta::image_grid(ui, &m),
                    Some(Ok(MediaMeta::Video(m))) => {
                        media_meta::video_grid(ui, &m);
                    }
                    Some(Err(err)) => {
                        ui.label(RichText::new(format!("Metadata unavailable: {err}")).weak());
                    }
                    None => {
                        ui.spinner();
                    }
                }
                if e.media_kind() == forge_core::MediaKind::Video {
                    crate::video_ui::preview(app, ui, &e);
                }
                crate::gps_ui::panel(app, ui, &e);
                crate::export_assoc::panel(app, ui, &e);
                ui.separator();
                crate::tags_ui::panel(app, ui, &e);
            });
        return;
    }
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

#[cfg(test)]
mod clipboard_tests {
    use super::*;

    fn cb(published: Option<Result<u32, ()>>, baseline: u32) -> FileClipboard {
        FileClipboard {
            paths: vec![PathBuf::from(r"C:\a.jpg")],
            cut: true,
            generation: 1,
            baseline_sequence: baseline,
            published,
        }
    }

    #[test]
    fn paste_source_prefers_newest_copy() {
        // Nothing copied in-app: the Windows clipboard decides.
        assert_eq!(choose_paste_source(None, 5, true), PasteSource::System);
        assert_eq!(choose_paste_source(None, 5, false), PasteSource::Empty);
        // Our publish is still pending: never paste stale Explorer paths.
        assert_eq!(
            choose_paste_source(Some(&cb(None, 4)), 4, true),
            PasteSource::InApp
        );
        // Published and still current: ours.
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Ok(7)), 4)), 7, true),
            PasteSource::InApp
        );
        // Something was copied in Explorer afterwards: theirs.
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Ok(7)), 4)), 9, true),
            PasteSource::System
        );
        // Text copied afterwards (no files): fall back to ours rather than doing nothing.
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Ok(7)), 4)), 9, false),
            PasteSource::InApp
        );
        // Publish failed: compare against the sequence from before our copy.
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Err(())), 4)), 4, true),
            PasteSource::InApp
        );
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Err(())), 4)), 6, true),
            PasteSource::System
        );
    }
}
