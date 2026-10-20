//! Bridges UI commands to the engine crates: file operations, dialogs, previews, jobs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use egui::{RichText, Ui};

use forge_core::ops::OpKind;

use forge_core::query::{ColorLabel, PickState};
use forge_db::AnnotationChange;

use crate::app::{Side, Tab, ZxrawApp};
use crate::commands::Cmd;
use crate::media_meta::{self, MediaMeta};
use crate::modal::{self, Confirm, ConfirmKind, Modal, Outcome, Prompt, PromptKind};
use crate::pane::Pane;
use crate::preview::{self, TextPreview};
use crate::settings::{Paths, Settings, ViewMode};
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
    /// A rename or new folder/file from the name prompt finished: select `name` in the
    /// focused pane, but only if it still shows `dir` (it may have moved on meanwhile).
    SelectNew {
        dir: PathBuf,
        name: String,
    },
    /// A name prompt's new name was taken: the prompt opens again with the error (once no
    /// other dialog is open).
    NameConflict(crate::modal::Prompt),
    /// An undo failed without changing anything: its record goes back where it was, so it
    /// can be retried. `slot` says where (see [`UndoSlot`]).
    UndoRestore { slot: UndoSlot, entry: UndoEntry },
    /// A job of the name lane finished. Sent after all its other events, so once the count of
    /// pending name jobs is back to zero, their undo records are already on the stack.
    NameJobDone,
    /// The clipboard worker finished a request (see [`crate::clip_worker`]).
    ClipboardDone(crate::clip_worker::Done),
    /// The cut watcher learned what the cut's items are (`None`: could not be read).
    CutIdentities {
        generation: u64,
        revision: u64,
        items: Vec<(PathBuf, Option<crate::cut_watch::Identity>)>,
    },
    /// These items of the cut left their folders (pasted in Explorer, moved or deleted some
    /// other way) or were replaced by a different item: they are no longer cut.
    CutItemsGone {
        generation: u64,
        revision: u64,
        paths: Vec<PathBuf>,
    },
    /// Paste: the Windows clipboard as read off the UI thread.
    PasteSystemRead {
        destination: PathBuf,
        sequence: u32,
        /// The file list on the clipboard, `Ok(None)` if it holds no files.
        files: Result<Option<(Vec<PathBuf>, forge_win::ClipboardEffect)>, String>,
        /// The owner token if the clipboard holds a publish of ours.
        owner: Option<forge_win::ClipboardToken>,
        /// What is at the in-app cut's paths now, if there is a cut.
        cut: Option<CutCheck>,
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
    /// A finished job asks something (a delete that could not use the Recycle Bin, for
    /// example). Shown once no other dialog is open.
    AskConfirm(crate::modal::Confirm),
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
    /// For a cut, what each item is (same order as `paths`); empty for a copy.
    ids: Vec<crate::cut_watch::IdState>,
    pub cut: bool,
    generation: u64,
    /// Bumped whenever the cut's items change; requests and reports carry it.
    revision: u64,
    /// Windows clipboard sequence before we published (to detect later external copies).
    baseline_sequence: u32,
    /// `None` while the clipboard worker is on it; `Some(Ok(token))` once our list is on
    /// the Windows clipboard; `Some(Err(()))` if it could not be put there or is no longer
    /// there.
    published: Option<Result<forge_win::ClipboardToken, ()>>,
    /// Stops the cut watcher once this cut is replaced or cleared.
    _watch: Option<crate::cut_watch::WatchGuard>,
}

/// A paste into `dest` of the cut `generation` at `revision`, waiting (since `since`) for
/// the cut's identities. Anything newer (another paste, copy or cut, a change of the cut)
/// drops it, and so does [`PASTE_WAIT`] going by: it never fires into an old destination.
struct PasteWait {
    dest: PathBuf,
    generation: u64,
    revision: u64,
    since: std::time::Instant,
}

/// How long a paste waits for the cut's identities: longer than one check may take.
const PASTE_WAIT: Duration = Duration::from_secs(8);

impl PasteWait {
    /// Whether this paste should run now that the cut `generation` at `revision` has its
    /// identities: only for that very cut, and only in time.
    fn fires_for(&self, generation: u64, revision: u64) -> bool {
        self.generation == generation && self.revision == revision && !self.expired()
    }

    fn expired(&self) -> bool {
        self.since.elapsed() >= PASTE_WAIT
    }
}

/// A cut checked for a paste, off the UI thread.
pub struct CutCheck {
    generation: u64,
    revision: u64,
    /// Some item's identity was still being read: nothing is checked.
    pending: bool,
    screened: crate::cut_watch::Screened,
}

impl FileClipboard {
    /// Still being placed on the Windows clipboard.
    #[allow(dead_code)] // used by the automation hook
    pub fn publishing(&self) -> bool {
        self.published.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasteSource {
    InApp,
    System,
    Empty,
    /// The Windows clipboard changed since our copy and could not be read.
    Unreadable,
    /// The clipboard holds an old list of ours that the app no longer has (a used-up cut).
    Stale,
}

/// What reading the Windows clipboard found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemFiles {
    Files,
    NoFiles,
    Unreadable,
}

/// Which clipboard a paste should use. The newest copy wins: our in-app clipboard while
/// its publish to Windows is still pending or is still what Windows holds; the Windows
/// clipboard once something else has been copied since (e.g. in Explorer). A changed
/// clipboard that could not be read never falls back to our older items.
fn choose_paste_source(
    in_app: Option<&FileClipboard>,
    system_sequence: u32,
    owner: Option<forge_win::ClipboardToken>,
    system: SystemFiles,
) -> PasteSource {
    // Content this app put there is never pasted as another app's files: the in-app
    // clipboard (whose items a paste checks first) is pasted instead, or, with none left (a
    // used-up cut whose list could not be cleared), nothing is.
    if owner.is_some_and(forge_win::ClipboardToken::is_from_this_process) {
        return if in_app.is_some() {
            PasteSource::InApp
        } else {
            PasteSource::Stale
        };
    }
    let Some(cb) = in_app else {
        return match system {
            SystemFiles::Files => PasteSource::System,
            SystemFiles::NoFiles => PasteSource::Empty,
            SystemFiles::Unreadable => PasteSource::Unreadable,
        };
    };
    let ours = match cb.published {
        // Our publish has not landed yet: ours, unless another app has put something on
        // the clipboard since the copy.
        None => system_sequence == cb.baseline_sequence,
        Some(Ok(token)) => owner == Some(token),
        Some(Err(())) => system_sequence == cb.baseline_sequence,
    };
    if ours {
        return PasteSource::InApp;
    }
    match system {
        SystemFiles::Files => PasteSource::System,
        SystemFiles::NoFiles => PasteSource::InApp,
        SystemFiles::Unreadable => PasteSource::Unreadable,
    }
}

pub struct Actions {
    pub modal: Option<Modal>,
    pub undo: Vec<UndoEntry>,
    /// Rename, new-item and rename-undo jobs, one lane per location (see [`lane_key`]): in
    /// order within a lane, so a hung share holds up only its own lane.
    name_lanes: std::collections::HashMap<String, NameLane>,
    /// Records removed from the bottom of `undo` by the size cap, ever. Keeps an
    /// [`UndoSlot`] valid while newer records arrive.
    undo_trimmed: u64,
    /// Conflict prompts that arrived while another dialog was open, shown in turn.
    pending_prompts: std::collections::VecDeque<crate::modal::Prompt>,
    /// Name jobs started and not yet reported done ([`Event::NameJobDone`]). Undo waits
    /// for zero, so it never acts on the record before the one a pending rename will push.
    name_jobs_pending: usize,
    pub conflicts: std::collections::VecDeque<crate::ops_ui::ConflictQuestion>,
    pub clipboard: Option<FileClipboard>,
    clipboard_generation: u64,
    /// Started with the first cut; see [`crate::cut_watch`].
    cut_watcher: Option<crate::cut_watch::CutWatcher>,
    /// Started with the first copy or cut; see [`crate::clip_worker`].
    clip_worker: Option<crate::clip_worker::ClipWorker>,
    /// A paste waiting for the cut's identities to be read.
    paste_waiting: Option<PasteWait>,
    pub show_jobs: bool,
    pub delete_ui: crate::ops_ui::DeleteUi,
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
    pub git: crate::git_status::GitStatus,
    pub folder_sizes: crate::folder_sizes::FolderSizes,
    folder_wants: FolderWants,
}

/// Grid thumbnail source that overlays library annotations (ratings/labels/picks).
pub struct GridProvider<'a> {
    thumbs: &'a mut crate::thumbs::ThumbManager,
    annotations: &'a std::collections::HashMap<PathBuf, crate::views::Annotation>,
    git: &'a crate::git_status::GitStatus,
    folder_sizes: &'a crate::folder_sizes::FolderSizes,
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
    fn git_badge(&self, path: &Path, is_dir: bool) -> Option<crate::git_status::GitBadge> {
        self.git.badge(path, is_dir)
    }
    fn folder_size(&self, path: &Path) -> crate::folder_sizes::FolderSize {
        self.folder_sizes.get(path)
    }
}

impl Actions {
    pub fn new(paths: &Paths, settings: &Settings, ctx: egui::Context) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            modal: None,
            undo: Vec::new(),
            name_lanes: Default::default(),
            undo_trimmed: 0,
            pending_prompts: Default::default(),
            name_jobs_pending: 0,
            conflicts: Default::default(),
            clipboard: None,
            clipboard_generation: 0,
            cut_watcher: None,
            clip_worker: None,
            paste_waiting: None,
            show_jobs: false,
            delete_ui: Default::default(),
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
            git: crate::git_status::GitStatus::new(ctx.clone()),
            folder_sizes: crate::folder_sizes::FolderSizes::new(ctx.clone()),
            folder_wants: Default::default(),
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
            git: &self.git,
            folder_sizes: &self.folder_sizes,
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
            let name = unique_listed_name(app, &dir, "New folder");
            app.ops.modal = Some(Modal::Prompt(Prompt::new(
                PromptKind::NewFolder { dir },
                "New folder",
                name,
            )));
        }
        Cmd::NewFile => {
            let name = unique_listed_name(app, &dir, "New file.txt");
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
        Cmd::OpenTerminal | Cmd::OpenTerminalAdmin => {
            // A folder the command was invoked on, otherwise the folder shown.
            let target = app.pane().action_targets().into_iter().next();
            let choice = *app.settings.terminal;
            let elevated = cmd == Cmd::OpenTerminalAdmin;
            let (tx, ctx) = (app.ops.tx.clone(), app.ops.ctx.clone());
            // Off the UI thread: probing the folder and starting the shell can block.
            std::thread::spawn(move || {
                let folder = target.filter(|t| t.is_dir()).unwrap_or(dir);
                if let Err(e) = crate::terminal::open(choice, &folder, elevated) {
                    let _ = tx.send(Event::Error(format!("Open terminal failed: {e}")));
                    ctx.request_repaint();
                }
            });
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
                app.ops.paste_waiting = None;
                let generation = app.ops.clipboard_generation;
                let baseline_sequence = forge_win::clipboard_sequence();
                let op = crate::clip_worker::Op::Publish {
                    paths: paths.clone(),
                    effect,
                    baseline: baseline_sequence,
                };
                clip(app, generation, 0, op);
                // A cut pasted elsewhere (Explorer, the desktop) is noticed by its items
                // leaving their folders, as nothing tells the source app about that paste.
                // The watcher reads what each item is right away; until then a paste here
                // waits (see `paste_with`).
                let ids = if cut {
                    vec![crate::cut_watch::IdState::Pending; paths.len()]
                } else {
                    Vec::new()
                };
                let watch = cut.then(|| {
                    let watcher = cut_watcher(app);
                    let items = paths.iter().cloned().zip(ids.iter().copied()).collect();
                    watcher.watch(generation, 0, items);
                    watcher.guard(generation)
                });
                app.ops.clipboard = Some(FileClipboard {
                    paths,
                    ids,
                    cut,
                    generation,
                    revision: 0,
                    baseline_sequence,
                    published: None,
                    _watch: watch,
                });
            }
        }
        Cmd::Paste => start_paste(app, dir),
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
        // While a rename (or an undo) is still running, its undo record is not on the stack
        // yet: undoing now would act on the operation before it. Undo always means "the last
        // thing done", so this waits for every pending name job, not just those on the same
        // paths: a per-path check would let Undo reach past the running rename.
        Cmd::Undo if app.ops.name_jobs_pending > 0 => {
            app.toast("Waiting for the rename to finish; try Undo again in a moment")
        }
        Cmd::Undo => match app.ops.undo.pop() {
            None => app.toast("Nothing to undo"),
            Some(UndoEntry::Ops(records)) => crate::ops_ui::undo_ops(app, records),
            Some(UndoEntry::TextReplace(records)) => crate::replace_ui::undo(app, records),
            Some(UndoEntry::Rename(pairs)) => {
                let slot = UndoSlot {
                    index: app.ops.undo.len(),
                    trimmed: app.ops.undo_trimmed,
                };
                undo_rename(app, pairs, slot);
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

/// A free name like `New folder (2)` in the focused pane's folder `dir`, judged from its
/// listing: asking the disk could block the UI on a slow or offline share. A name the
/// listing missed fails safely when created (`create_dir` and `create_new` never replace).
fn unique_listed_name(app: &ZxrawApp, dir: &Path, name: &str) -> String {
    let taken: std::collections::HashSet<String> = app
        .pane()
        .entries
        .iter()
        .filter(|e| e.path.parent() == Some(dir))
        .map(|e| e.name.to_lowercase())
        .collect();
    let free = forge_core::paths::unique_name_with(dir, name, true, |p| {
        p.file_name()
            .is_some_and(|n| taken.contains(&n.to_string_lossy().to_lowercase()))
    });
    forge_core::paths::name_of(&free)
}

/// What an accepted name prompt does on disk. It always runs on a worker (see
/// [`spawn_name_job`]): on a slow or offline share any file call can block for a long time.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NameJob {
    NewFolder(PathBuf),
    NewFile(PathBuf),
    Rename { from: PathBuf, to: PathBuf },
}

impl NameJob {
    fn verb(&self) -> &'static str {
        match self {
            NameJob::NewFolder(_) => "New folder",
            NameJob::NewFile(_) => "New file",
            NameJob::Rename { .. } => "Rename",
        }
    }

    /// Touches the disk: call it off the UI thread only.
    fn run(&self) -> std::io::Result<()> {
        match self {
            NameJob::NewFolder(p) => std::fs::create_dir(p),
            NameJob::NewFile(p) => std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(p)
                .map(|_| ()),
            // The same name again: nothing to do (not "that name is taken").
            NameJob::Rename { from, to } if from == to => Ok(()),
            NameJob::Rename { from, to } => {
                // A case-only rename ("a.txt" -> "A.txt") finds itself at the target.
                let case_only = to != from && forge_core::paths::paths_equal(to, from);
                if forge_core::paths::exists_no_follow(to) && !case_only {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "a file with that name already exists",
                    ))
                } else {
                    rename_no_replace_one(from, to, case_only)
                }
            }
        }
    }

    /// The prompt to show again when the new name turned out to be taken.
    fn conflict_prompt(&self, name: &str, error: String) -> crate::modal::Prompt {
        let parent = |p: &Path| p.parent().map(Path::to_path_buf).unwrap_or_default();
        let kind = match self {
            NameJob::NewFolder(p) => PromptKind::NewFolder { dir: parent(p) },
            NameJob::NewFile(p) => PromptKind::NewFile { dir: parent(p) },
            NameJob::Rename { from, .. } => PromptKind::Rename { path: from.clone() },
        };
        let mut prompt = crate::modal::Prompt::new(kind, self.verb(), name);
        prompt.error = Some(error);
        prompt
    }
}

/// Renames `from` to `to` without ever replacing an entry at `to`, even one created after
/// any check (two renames racing for one name: the second fails). A case-only rename goes
/// through a temporary name, with rollback; see [`forge_core::rename::rename_paths`].
fn rename_no_replace_one(from: &Path, to: &Path, case_only: bool) -> std::io::Result<()> {
    let report = forge_core::rename::rename_paths(&[(from.to_path_buf(), to.to_path_buf())]);
    rename_outcome(report, case_only, || forge_core::paths::exists_no_follow(to))
}

/// What a finished single rename means for the user. `target_exists` is asked only when the
/// rename failed and put everything back, to tell "that name is taken" from other errors.
fn rename_outcome(
    report: forge_core::rename::RenameReport,
    case_only: bool,
    target_exists: impl FnOnce() -> bool,
) -> std::io::Result<()> {
    use forge_core::rename::RenameStatus;
    match report.status {
        RenameStatus::Completed | RenameStatus::NothingToDo => Ok(()),
        // Checked before the conflict below: the file could not be put back, so it sits under
        // a temporary name, and the user must hear where, not "that name is taken".
        RenameStatus::RollbackIncomplete { error, .. } => {
            Err(std::io::Error::other(stranded_message(&error, &report.stranded)))
        }
        // Refused by the move itself: the name was taken after (or despite) the check above.
        _ if !case_only && target_exists() => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "a file with that name already exists",
        )),
        RenameStatus::RolledBack { error, .. } => Err(std::io::Error::other(error)),
        other => Err(std::io::Error::other(format!("{other:?}"))),
    }
}

/// The error for a rename or undo that failed and could not move everything back: which
/// files are left where, and the name each should get back.
fn stranded_message(error: &str, stranded: &[forge_core::rename::StrandedFile]) -> String {
    let files: Vec<String> = stranded
        .iter()
        .map(|s| {
            format!(
                "{} is left at {} (rename it back to {})",
                forge_core::paths::name_of(&s.original),
                s.current.display(),
                s.original.display()
            )
        })
        .collect();
    format!("{error}; could not put everything back: {}", files.join("; "))
}

/// Which name lane runs jobs on `path`: one per network share (`\\server\share`, or the
/// letter of a mapped drive), one for everything local. A rename stays within one folder, so
/// all jobs on the same file or folder share a lane and run in order; a hung share holds up
/// only jobs on that share. Lexical, apart from asking Windows the type of a drive letter.
fn lane_key(path: &Path) -> String {
    if let Some(root) = forge_core::netpath::share_root(path) {
        return root.to_string_lossy().to_lowercase();
    }
    if forge_core::netpath::is_network_path(path)
        && let Some(prefix) = path.components().next()
    {
        return prefix.as_os_str().to_string_lossy().to_lowercase();
    }
    "local".into()
}

/// Where an undo record sat on the stack when Undo took it: `index` from the bottom, at a time
/// when the size cap had removed `trimmed` records. Records pushed meanwhile stay above it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UndoSlot {
    index: usize,
    trimmed: u64,
}

impl UndoSlot {
    /// The index to put the record back at, given the stack's length and trim count now.
    fn index_now(self, len: usize, trimmed: u64) -> usize {
        let shift = usize::try_from(trimmed.saturating_sub(self.trimmed)).unwrap_or(usize::MAX);
        self.index.saturating_sub(shift).min(len)
    }
}

/// A worker that runs rename, new-item and rename-undo work for one location (see
/// [`lane_key`]) strictly in the order it was submitted from the UI thread. Two renames in one
/// folder never race each other, and an undo always runs after the operation it undoes.
struct NameLane {
    tx: crossbeam_channel::Sender<Box<dyn FnOnce() + Send>>,
}

impl NameLane {
    fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<Box<dyn FnOnce() + Send>>();
        let spawned = std::thread::Builder::new()
            .name("name-jobs".into())
            .spawn(move || {
                for work in rx {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("name job lane unavailable, running name jobs on their own threads: {e}");
        }
        Self { tx }
    }

    fn submit(&self, work: Box<dyn FnOnce() + Send>) {
        if let Err(crossbeam_channel::SendError(work)) = self.tx.send(work) {
            // No lane thread: run it anyway rather than drop a user's action.
            std::thread::spawn(work);
        }
    }
}

/// Queues `work` on the name lane and shows it in the Operations panel as a job titled
/// `title`, which runs until the work is done. `work` returns the job's result and sends its
/// own events; [`Event::NameJobDone`] follows them.
fn submit_name_job(
    app: &mut ZxrawApp,
    lane: String,
    kind: &str,
    title: String,
    work: impl FnOnce() -> Result<(), String> + Send + 'static,
) {
    let (tx, ctx) = (app.ops.tx.clone(), app.ops.ctx.clone());
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    app.ops.name_jobs_pending += 1;
    let lane = app.ops.name_lanes.entry(lane).or_insert_with(NameLane::new);
    lane.submit(Box::new(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
            .unwrap_or_else(|_| Err("internal error".to_string()));
        let _ = tx.send(Event::NameJobDone);
        ctx.request_repaint();
        let _ = done_tx.send(result);
    }));
    app.ops.jobs.spawn(kind, title, move |_job| {
        done_rx
            .recv()
            .unwrap_or_else(|_| Err("internal error".to_string()))
    });
}

/// Automation: keeps the name lane of `path` busy for `hold`, as a rename stuck on a slow
/// share would, so tests can check what waits for it and what does not.
#[cfg(any(debug_assertions, feature = "automation"))]
pub(crate) fn hold_name_lane(app: &mut ZxrawApp, path: &Path, hold: std::time::Duration) {
    submit_name_job(app, lane_key(path), "test", "Holding the name lane".into(), move || {
        std::thread::sleep(hold);
        Ok(())
    });
}

/// Runs `job` as a background operation. Success refreshes `dir` and selects `name` in it (a
/// rename also records its undo and moves the library rows); failure shows an error toast.
fn spawn_name_job(app: &mut ZxrawApp, job: NameJob, dir: PathBuf, name: String) {
    let (tx, ctx) = (app.ops.tx.clone(), app.ops.ctx.clone());
    let (kind, title) = match &job {
        NameJob::Rename { .. } => ("rename", format!("Renaming to {name}")),
        NameJob::NewFolder(_) => ("create", format!("Creating folder {name}")),
        NameJob::NewFile(_) => ("create", format!("Creating file {name}")),
    };
    let lane = lane_key(match &job {
        NameJob::NewFolder(p) | NameJob::NewFile(p) => p,
        NameJob::Rename { from, .. } => from,
    });
    submit_name_job(app, lane, kind, title, move || {
        let result = job.run();
        match &result {
            Ok(()) => {
                if let NameJob::Rename { from, to } = &job {
                    let _ = tx.send(Event::FilesMoved(vec![(from.clone(), to.clone())]));
                    let _ = tx.send(Event::Undo(UndoEntry::Rename(vec![(
                        to.clone(),
                        from.clone(),
                    )])));
                }
                let _ = tx.send(Event::Changed(dir.clone()));
                let _ = tx.send(Event::SelectNew { dir, name });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = tx.send(Event::NameConflict(job.conflict_prompt(&name, e.to_string())));
            }
            Err(e) => {
                let _ = tx.send(Event::Error(format!("{} failed: {e}", job.verb())));
            }
        }
        ctx.request_repaint();
        result.map_err(|e| e.to_string())
    });
}

/// Undoes a rename on a worker: the library follows the pairs that really moved back, the
/// folders involved refresh, and a toast reports the outcome. An undo that fails without
/// changing anything puts its record back at `slot`, so it can be retried once the obstacle
/// is gone; one that moved some files but could not finish names the files left behind.
fn undo_rename(app: &mut ZxrawApp, pairs: Vec<(PathBuf, PathBuf)>, slot: UndoSlot) {
    let (tx, ctx) = (app.ops.tx.clone(), app.ops.ctx.clone());
    let n = pairs.len();
    let lane = pairs
        .first()
        .map_or_else(|| "local".to_string(), |(current, _)| lane_key(current));
    submit_name_job(
        app,
        lane,
        "undo",
        format!("Undoing rename of {n} item(s)"),
        move || {
            use forge_core::rename::RenameStatus;
            let r = forge_core::rename::undo(&pairs);
            let completed = r.status == RenameStatus::Completed;
            // Only the pairs that really moved back: a partial undo follows just those.
            let undone: Vec<(PathBuf, PathBuf)> = if completed {
                pairs.clone()
            } else {
                pairs
                    .iter()
                    .filter(|(current, original)| original.exists() && !current.exists())
                    .cloned()
                    .collect()
            };
            if !undone.is_empty() {
                let _ = tx.send(Event::FilesMoved(undone));
            }
            let dirs: std::collections::BTreeSet<PathBuf> = pairs
                .iter()
                .filter_map(|(_, o)| o.parent().map(Path::to_path_buf))
                .collect();
            for d in dirs {
                let _ = tx.send(Event::Changed(d));
            }
            let result = match &r.status {
                RenameStatus::Completed => {
                    let _ = tx.send(Event::Toast(format!("Undid rename of {n} item(s)")));
                    Ok(())
                }
                // Everything is as it was before the undo: keep the record for a retry.
                RenameStatus::RolledBack { failed, error } => {
                    let msg = format!(
                        "Undo failed: {error} ({}); nothing was changed, so Undo can be \
                         tried again once that is resolved",
                        failed.display()
                    );
                    let _ = tx.send(Event::UndoRestore {
                        slot,
                        entry: UndoEntry::Rename(pairs.clone()),
                    });
                    let _ = tx.send(Event::Error(msg.clone()));
                    Err(msg)
                }
                RenameStatus::RollbackIncomplete { error, .. } => {
                    let msg = format!("Undo failed: {}", stranded_message(error, &r.stranded));
                    let _ = tx.send(Event::Error(msg.clone()));
                    Err(msg)
                }
                other => {
                    let msg = format!("Undo failed: {other:?}");
                    let _ = tx.send(Event::Error(msg.clone()));
                    Err(msg)
                }
            };
            ctx.request_repaint();
            result
        },
    );
}

fn annotate_targets(app: &mut ZxrawApp, change: AnnotationChange) {
    // The listed kind, not the disk: `is_file` could block the UI on an offline share.
    let pane = app.pane();
    let all = pane.action_targets();
    let kinds = pane.kinds_of(&all);
    let targets: Vec<PathBuf> = all
        .into_iter()
        .zip(kinds)
        .filter(|(_, kind)| *kind == Some(forge_core::EntryKind::File))
        .map(|(p, _)| p)
        .collect();
    if targets.is_empty() {
        app.toast("Select files to rate or label");
        return;
    }
    app.ops.library.annotate(targets, change);
}

/// The app's one cut watcher, started on first use. Its reports arrive as events.
fn cut_watcher(app: &mut ZxrawApp) -> &crate::cut_watch::CutWatcher {
    let (tx, ctx) = (app.ops.tx.clone(), app.ops.ctx.clone());
    app.ops.cut_watcher.get_or_insert_with(|| {
        crate::cut_watch::CutWatcher::start(move |report| {
            let event = match report {
                crate::cut_watch::Report::Identities {
                    generation,
                    revision,
                    items,
                } => Event::CutIdentities {
                    generation,
                    revision,
                    items,
                },
                crate::cut_watch::Report::Gone {
                    generation,
                    revision,
                    paths,
                } => Event::CutItemsGone {
                    generation,
                    revision,
                    paths,
                },
            };
            let _ = tx.send(event);
            ctx.request_repaint();
        })
    })
}

/// Sends `op` to the app's one clipboard worker (started on first use), on behalf of the
/// in-app clipboard `generation` at `revision`.
fn clip(app: &mut ZxrawApp, generation: u64, revision: u64, op: crate::clip_worker::Op) {
    let (tx, ctx) = (app.ops.tx.clone(), app.ops.ctx.clone());
    app.ops
        .clip_worker
        .get_or_insert_with(|| {
            crate::clip_worker::ClipWorker::start(move |done| {
                let _ = tx.send(Event::ClipboardDone(done));
                ctx.request_repaint();
            })
        })
        .send(generation, revision, op);
}

/// Pastes into `dir`. Opening the Windows clipboard can block briefly (another app holding
/// it), and checking the cut's items can block on a slow drive, so both happen off the UI
/// thread; `paste_with` decides once the result arrives.
fn start_paste(app: &mut ZxrawApp, dir: PathBuf) {
    app.ops.paste_waiting = None;
    let checker = app.ops.cut_watcher.as_ref().map(|w| w.checker());
    let (tx, ctx) = (app.ops.tx.clone(), app.ops.ctx.clone());
    let cut = app.ops.clipboard.as_ref().filter(|cb| cb.cut).map(|cb| {
        let items: Vec<_> = cb.paths.iter().cloned().zip(cb.ids.iter().copied()).collect();
        (cb.generation, cb.revision, items)
    });
    std::thread::spawn(move || {
        let (files, owner, sequence) = match forge_win::get_files_and_owner() {
            Ok(read) => (Ok(read.files), read.owner, read.sequence),
            Err(e) => (Err(e.to_string()), None, forge_win::clipboard_sequence()),
        };
        let cut = cut.map(|(generation, revision, items)| {
            check_cut(checker, generation, revision, &items)
        });
        let _ = tx.send(Event::PasteSystemRead {
            destination: dir,
            sequence,
            files,
            owner,
            cut,
        });
        ctx.request_repaint();
    });
}

/// Bounded identity checks that share the cut watcher's slots, if it is running.
pub fn cut_checker(app: &ZxrawApp) -> Option<crate::cut_watch::Checker> {
    app.ops.cut_watcher.as_ref().map(|w| w.checker())
}

/// The known identities of the cut `generation`'s items (empty if that cut is gone). A
/// paste of the cut moves only these items, and only while they are still these.
pub fn cut_identities(
    app: &ZxrawApp,
    generation: u64,
) -> Vec<(PathBuf, crate::cut_watch::Identity)> {
    app.ops
        .clipboard
        .as_ref()
        .filter(|cb| cb.cut && cb.generation == generation)
        .map(|cb| {
            cb.paths
                .iter()
                .zip(&cb.ids)
                .filter_map(|(p, id)| match id {
                    crate::cut_watch::IdState::Known(id) => Some((p.clone(), *id)),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What is at the cut's paths now, for a paste. Runs off the UI thread; a check that takes
/// longer than [`crate::cut_watch::PASS_TIMEOUT`] counts as "could not tell".
fn check_cut(
    checker: Option<crate::cut_watch::Checker>,
    generation: u64,
    revision: u64,
    items: &[(PathBuf, crate::cut_watch::IdState)],
) -> CutCheck {
    use crate::cut_watch::{IdState, Observation, PASS_TIMEOUT, screen};
    let pending = items.iter().any(|(_, id)| *id == IdState::Pending);
    let screened = if pending {
        Default::default()
    } else {
        // Shares the watcher's check slots: one check per cut, a few in all.
        let paths: Vec<PathBuf> = items.iter().map(|(p, _)| p.clone()).collect();
        let seen = checker
            .and_then(|c| c.check(generation, paths, PASS_TIMEOUT))
            .unwrap_or_else(|| vec![Observation::Unknown; items.len()]);
        screen(items, &seen)
    };
    CutCheck {
        generation,
        revision,
        pending,
        screened,
    }
}

/// The cut `generation` now has `items` (some used up, renamed, or replaced): a new
/// revision. The Windows clipboard follows through the clipboard worker, which changes
/// only our own content: cleared when nothing is left, else given the remaining items.
/// `refresh` folders are listed again.
fn set_cut_items(
    app: &mut ZxrawApp,
    generation: u64,
    items: Vec<(PathBuf, crate::cut_watch::IdState)>,
    refresh: Vec<PathBuf>,
) {
    let Some(cb) = app
        .ops
        .clipboard
        .as_mut()
        .filter(|cb| cb.cut && cb.generation == generation)
    else {
        return;
    };
    cb.revision += 1;
    let revision = cb.revision;
    app.ops.paste_waiting = None;
    (cb.paths, cb.ids) = items.iter().cloned().unzip();
    if cb.paths.is_empty() {
        app.ops.clipboard = None;
        clip(app, generation, revision, crate::clip_worker::Op::ClearOurs);
    } else {
        cb.published = None;
        let paths = cb.paths.clone();
        let effect = forge_win::ClipboardEffect::Move;
        clip(app, generation, revision, crate::clip_worker::Op::Update { paths, effect });
        if let Some(watcher) = app.ops.cut_watcher.as_ref() {
            watcher.watch(generation, revision, items);
        }
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    for dir in refresh {
        if !seen.contains(&dir) {
            app.refresh_all_showing(&dir);
            seen.push(dir);
        }
    }
}

/// Items of the cut `generation` are no longer cut (pasted elsewhere, moved, deleted,
/// replaced): dropped from it, as Explorer drops its own cut after a paste. The folders
/// they left are listed again.
fn drop_cut_items(app: &mut ZxrawApp, generation: u64, gone: &[PathBuf]) {
    let Some(cb) = app
        .ops
        .clipboard
        .as_ref()
        .filter(|cb| cb.cut && cb.generation == generation)
    else {
        return;
    };
    let is_gone = |p: &PathBuf| gone.iter().any(|g| forge_core::paths::paths_equal(g, p));
    if !cb.paths.iter().any(is_gone) {
        return;
    }
    let refresh = cb
        .paths
        .iter()
        .filter(|p| is_gone(p))
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect();
    let items = cb
        .paths
        .iter()
        .cloned()
        .zip(cb.ids.iter().copied())
        .filter(|(p, _)| !is_gone(p))
        .collect();
    set_cut_items(app, generation, items, refresh);
}

/// This app moved or renamed `pairs`: see [`crate::cut_watch::follow`]. A followed item's
/// identity is read again, so a move to another volume does not look like a replacement.
fn follow_cut(app: &mut ZxrawApp, pairs: &[(PathBuf, PathBuf)]) {
    let Some(cb) = app.ops.clipboard.as_ref().filter(|cb| cb.cut) else {
        return;
    };
    let items: Vec<_> = cb.paths.iter().cloned().zip(cb.ids.iter().copied()).collect();
    let (kept, used) = crate::cut_watch::follow(&items, pairs);
    if kept == items {
        return;
    }
    let _ = used; // A move refreshes its own folders.
    set_cut_items(app, cb.generation, kept, Vec::new());
}

fn paste_with(
    app: &mut ZxrawApp,
    dest: PathBuf,
    sequence: u32,
    owner: Option<forge_win::ClipboardToken>,
    files: Result<Option<(Vec<PathBuf>, forge_win::ClipboardEffect)>, String>,
    cut: Option<CutCheck>,
) {
    // Every way out of here either starts a transfer or says why it did not.
    let (files, read_error) = match files {
        Ok(files) => (files.filter(|(paths, _)| !paths.is_empty()), None),
        Err(e) => (None, Some(e)),
    };
    let system = match (&files, &read_error) {
        (Some(_), _) => SystemFiles::Files,
        (None, None) => SystemFiles::NoFiles,
        (None, Some(_)) => SystemFiles::Unreadable,
    };
    match choose_paste_source(app.ops.clipboard.as_ref(), sequence, owner, system) {
        PasteSource::Empty => app.toast("Nothing to paste: the clipboard has no files"),
        PasteSource::Stale => app.toast("Nothing to paste: those items were moved already"),
        PasteSource::Unreadable => app.toast_err(format!(
            "Could not read the clipboard: {}",
            read_error.unwrap_or_default()
        )),
        PasteSource::System => {
            let Some((paths, effect)) = files else {
                app.toast_err("Could not read the files on the clipboard");
                return;
            };
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
                app.toast("Nothing to paste: the clipboard has no files");
                return;
            };
            if !cb.cut {
                let paths = cb.paths.clone();
                crate::ops_ui::start_transfer(app, paths, dest, OpKind::Copy);
                return;
            }
            paste_cut(app, dest, cut);
        }
    }
}

/// Moves the in-app cut into `dest`, but only items still known to be the ones cut. Fails
/// closed: an item that was replaced, whose identity was never read, or that could not be
/// checked now is left out, and nothing moves while identities are still being read.
fn paste_cut(app: &mut ZxrawApp, dest: PathBuf, check: Option<CutCheck>) {
    let Some(cb) = app.ops.clipboard.as_ref() else {
        return;
    };
    let generation = cb.generation;
    let Some(check) = check.filter(|c| c.generation == generation && c.revision == cb.revision)
    else {
        app.toast("The cut has just changed. Paste again.");
        return;
    };
    if check.pending {
        // Right after the cut: nothing moves before the watcher has read what the items
        // are. If it has by now, check again at once; else wait for it.
        if !cb.ids.contains(&crate::cut_watch::IdState::Pending) {
            start_paste(app, dest);
            return;
        }
        app.ops.paste_waiting = Some(PasteWait {
            dest,
            generation,
            revision: cb.revision,
            since: std::time::Instant::now(),
        });
        app.ops
            .ctx
            .request_repaint_after(PASTE_WAIT + Duration::from_millis(100));
        return;
    }
    let s = check.screened;
    let gone: Vec<PathBuf> = s.replaced.iter().chain(&s.missing).cloned().collect();
    drop_cut_items(app, generation, &gone);
    let mut left_out = Vec::new();
    if !s.replaced.is_empty() {
        left_out.push(format!(
            "{} replaced by another item since the cut",
            s.replaced.len()
        ));
    }
    if !s.unverified.is_empty() {
        left_out.push(format!("{} that could not be checked", s.unverified.len()));
    }
    if !left_out.is_empty() {
        app.toast_err(format!("Left out {}", left_out.join(" and ")));
    }
    if s.keep.is_empty() {
        if left_out.is_empty() {
            app.toast("Nothing to paste: the cut items are gone");
        }
        return;
    }
    // Taking the cut off the Windows clipboard afterwards is the clipboard worker's job
    // (see `Event::MoveFinished`), so no sequence number is passed on.
    if !crate::ops_ui::start_clipboard_move(app, s.keep, dest, Some(generation), None) {
        app.toast("Nothing to move: the items are already in this folder");
    }
}

pub fn recycle(app: &mut ZxrawApp, targets: Vec<PathBuf>) {
    crate::ops_ui::recycle(app, targets);
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

/// The folders the folder size service is asked to measure, kept between frames. It is
/// rebuilt only when a listing, a view or the focused selection changes, so a steady frame
/// costs one comparison and no 50,000-entry walk.
#[derive(Default)]
pub struct FolderWants {
    key: Option<WantKey>,
    folders: Vec<PathBuf>,
    /// Changes only when `folders` does, so the service skips its own comparison.
    generation: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct WantKey {
    /// Listing revisions of the left and right pane (0 without a right pane).
    listings: (u64, u64),
    /// Viewport revisions of both panes: the rows on screen are measured first.
    viewports: (u64, u64),
    /// Which panes are in Details view, where the Size column shows folder totals.
    details: (bool, bool),
    right_focused: bool,
    /// Revision of the focused pane's selection.
    selection: u64,
}

impl FolderWants {
    /// The folders of the tab at `active`: the listed folders of each pane in Details view
    /// (the Size column), then the selected folders of the focused pane (the status bar
    /// totals them in any view).
    /// Returns the folders and their generation.
    fn refresh(&mut self, tabs: &mut [Tab], active: usize) -> (u64, &[PathBuf]) {
        let Some(tab) = tabs.get_mut(active) else {
            if self.key.take().is_some() || !self.folders.is_empty() {
                self.folders.clear();
                self.generation += 1;
            }
            return (self.generation, &self.folders);
        };
        let key = WantKey {
            listings: (
                tab.left.listing_rev(),
                tab.right.as_ref().map_or(0, Pane::listing_rev),
            ),
            viewports: (
                tab.left.viewport_rev(),
                tab.right.as_ref().map_or(0, Pane::viewport_rev),
            ),
            details: (
                tab.left.view == ViewMode::Details,
                tab.right.as_ref().is_some_and(|p| p.view == ViewMode::Details),
            ),
            right_focused: tab.focus == Side::Right && tab.right.is_some(),
            selection: tab.focused().selected.revision(),
        };
        if self.key != Some(key) {
            self.key = Some(key);
            // Listing revisions also move when totals are written into the rows (a few times
            // a second while measuring), mostly leaving the same folders: compared here, at
            // most once per change, so the generation moves only when the folders do.
            let mut folders = Vec::with_capacity(self.folders.len());
            for pane in std::iter::once(&mut tab.left).chain(tab.right.as_mut()) {
                if pane.view == ViewMode::Details {
                    folders.extend_from_slice(pane.listed_folders());
                }
            }
            let focused = tab.focused();
            folders.extend(
                focused
                    .entries
                    .iter()
                    .filter(|e| e.is_dir() && focused.selected.contains(&e.path))
                    .map(|e| e.path.clone()),
            );
            if folders != self.folders {
                self.folders = folders;
                self.generation += 1;
            }
        }
        (self.generation, &self.folders)
    }
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
    // Folder sizes: the settings, the folders to measure, then the totals written into the
    // listings. Every pane of every tab, so a background tab's Size sort stays right too.
    app.ops
        .folder_sizes
        .configure(app.settings.folder_sizes, app.settings.folder_sizes_network);
    for d in &due {
        app.ops.folder_sizes.invalidate(d);
    }
    let (generation, wanted) = app.ops.folder_wants.refresh(&mut app.tabs, app.active);
    app.ops.folder_sizes.want_at(generation, wanted, &folders);
    let mut sizes_changed = false;
    let mut retry: Option<Duration> = None;
    for tab in app.tabs.iter_mut() {
        for pane in std::iter::once(&mut tab.left).chain(tab.right.as_mut()) {
            sizes_changed |= pane.apply_folder_sizes(&app.ops.folder_sizes);
            if let Some(after) = pane.folder_sizes_retry_in() {
                retry = Some(retry.map_or(after, |r| r.min(after)));
            }
        }
    }
    if let Some(after) = retry {
        // A held-back total may be the last result, which brings no repaint of its own.
        app.ops.ctx.request_repaint_after(after);
    }
    app.ops.git.set_enabled(app.settings.git_badges);
    app.ops.git.track(&folders);
    let git_changed = app.ops.git.poll();
    for d in &due {
        app.ops.git.invalidate(d);
        app.ops.thumbs.invalidate_dir(d);
        app.refresh_all_showing(d);
    }
    let mut changed = app.ops.thumbs.begin_frame(folders)
        || !due.is_empty()
        || git_changed
        || sizes_changed;
    changed |= app.ops.thumbs.upload_ready(&app.ops.ctx);
    // A finished "Select originals/exports" match (computed off the UI thread).
    changed |= crate::export_assoc::poll(app);
    if app
        .ops
        .paste_waiting
        .take_if(|w| w.expired())
        .is_some()
    {
        app.toast_err("Could not check the cut items in time, so nothing was pasted");
        changed = true;
    }
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
            Event::ClipboardDone(done) => {
                use crate::clip_worker::Outcome;
                if let Outcome::Failed(e) = &done.outcome {
                    tracing::warn!("writing the Windows clipboard failed: {e}");
                }
                if let Some(cb) = app.ops.clipboard.as_mut().filter(|cb| {
                    cb.generation == done.generation && cb.revision == done.revision
                }) {
                    match done.outcome {
                        Outcome::Published(token) => cb.published = Some(Ok(token)),
                        Outcome::NotOurs | Outcome::Failed(_) => cb.published = Some(Err(())),
                        Outcome::Cleared => {}
                    }
                }
            }
            Event::CutIdentities {
                generation,
                revision,
                items,
            } => {
                if let Some(cb) = app.ops.clipboard.as_mut().filter(|cb| {
                    cb.generation == generation && cb.revision == revision
                }) {
                    for (path, id) in items {
                        if let Some(i) = cb.paths.iter().position(|p| *p == path) {
                            cb.ids[i] = match id {
                                Some(id) => crate::cut_watch::IdState::Known(id),
                                None => crate::cut_watch::IdState::Unknown,
                            };
                        }
                    }
                    let ready = !cb.ids.contains(&crate::cut_watch::IdState::Pending);
                    let (generation, revision) = (cb.generation, cb.revision);
                    if ready
                        && let Some(wait) = app
                            .ops
                            .paste_waiting
                            .take_if(|w| w.fires_for(generation, revision))
                    {
                        start_paste(app, wait.dest);
                    }
                }
            }
            Event::CutItemsGone {
                generation,
                revision,
                paths,
            } => {
                if app
                    .ops
                    .clipboard
                    .as_ref()
                    .is_some_and(|cb| cb.revision == revision)
                {
                    drop_cut_items(app, generation, &paths);
                }
            }
            Event::PasteSystemRead {
                destination,
                sequence,
                files,
                owner,
                cut,
            } => paste_with(app, destination, sequence, owner, files, cut),
            Event::MoveFinished {
                sources,
                ok,
                clipboard_generation,
                expected_sequence,
            } => {
                // A cut is consumed only by a complete, successful move (like Explorer); a
                // failed or cancelled move leaves it on the clipboard to retry.
                if ok {
                    if let Some(cb) = app
                        .ops
                        .clipboard
                        .take_if(|cb| clipboard_generation.is_some_and(|g| cb.generation == g))
                    {
                        let revision = cb.revision + 1;
                        clip(app, cb.generation, revision, crate::clip_worker::Op::ClearOurs);
                    }
                    // Another app's cut (Explorer's) pasted here: emptied, as Explorer does.
                    if let Some(sequence) = expected_sequence {
                        let op = crate::clip_worker::Op::ClearForeign {
                            paths: sources,
                            sequence,
                        };
                        clip(app, 0, 0, op);
                    }
                }
            }
            Event::FfmpegInstalled(dir) => {
                crate::settings_ui::use_ffmpeg_dir(app, Some(dir));
                app.notices.video_tools_changed();
                app.save_all();
                app.toast("FFmpeg installed: video thumbnails and clip metadata are on");
            }
            Event::AskConfirm(confirm) => crate::ops_ui::ask(app, confirm),
            Event::UpdateChecked(checked) => crate::updater::checked(app, checked),
            Event::UpdateDownloaded(result) => crate::updater::downloaded(app, result),
            Event::Undo(u) => {
                if let UndoEntry::Ops(records) = &u {
                    follow_moves(app, undo_record_moves(records));
                }
                push_undo(app, u);
            }
            Event::UndoRestore { slot, entry } => {
                let at = slot.index_now(app.ops.undo.len(), app.ops.undo_trimmed);
                app.ops.undo.insert(at, entry);
                trim_undo(app);
            }
            Event::FilesMoved(pairs) => follow_moves(app, pairs),
            Event::NameJobDone => {
                app.ops.name_jobs_pending = app.ops.name_jobs_pending.saturating_sub(1);
            }
            // Shown at once if no dialog is open, else as soon as it closes (see below).
            Event::NameConflict(prompt) => app.ops.pending_prompts.push_back(prompt),
            Event::SelectNew { dir, name } => {
                let pane = app.pane_mut();
                if pane.path == dir || pane.shows_children_of(&dir) {
                    pane.set_pending_select(name);
                }
            }
        }
    }
    let ops = &mut app.ops;
    if show_pending_prompt(&mut ops.modal, ops.batch_rename.is_some(), &mut ops.pending_prompts) {
        changed = true;
    }
    changed |= crate::ops_ui::show_pending_question(app);
    changed
}

/// Opens the next conflict prompt that is waiting, if no dialog is open (`other_dialog`: the
/// batch rename window). Returns whether one opened.
fn show_pending_prompt(
    modal: &mut Option<Modal>,
    other_dialog: bool,
    pending: &mut std::collections::VecDeque<crate::modal::Prompt>,
) -> bool {
    if modal.is_some() || other_dialog {
        return false;
    }
    let Some(prompt) = pending.pop_front() else {
        return false;
    };
    *modal = Some(Modal::Prompt(prompt));
    true
}

/// The undo stack keeps this many records; older ones are dropped.
const UNDO_LIMIT: usize = 50;

fn push_undo(app: &mut ZxrawApp, entry: UndoEntry) {
    app.ops.undo.push(entry);
    trim_undo(app);
}

fn trim_undo(app: &mut ZxrawApp) {
    while app.ops.undo.len() > UNDO_LIMIT {
        app.ops.undo.remove(0);
        app.ops.undo_trimmed += 1;
    }
}

/// ZXraw moved or renamed files (`from` -> `to`): the note being edited and the library rows
/// (notes, ratings, tags) follow them.
pub fn follow_moves(app: &mut ZxrawApp, pairs: Vec<(PathBuf, PathBuf)>) {
    follow_cut(app, &pairs);
    let relinked = app.ops.library.follow_moves(pairs.clone());
    app.ops.tags_ui.follow_moves(&pairs, relinked);
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
    crate::sidebar::network::dialog(app, ui.ctx());
    crate::ops_ui::jobs_window(app, ui.ctx());
    crate::ops_ui::progress_popup(app, ui.ctx());
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
                    push_undo(app, UndoEntry::Rename(undo));
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
            PromptKind::RenameNetworkLocation { .. } => {
                crate::sidebar::network::accept_label(app, p)
            }
            _ => crate::layouts::accept_label(app, p),
        },
        Modal::Prompt(mut p) => {
            let name = p.text.trim().to_string();
            if let Err(e) = forge_core::paths::validate_file_name(&name) {
                p.error = Some(e.to_string());
                return Some(Modal::Prompt(p));
            }
            // Nothing here touches the disk: the work runs on a worker and reports back
            // through events (an error becomes a toast; the prompt is already closed).
            let (job, dir) = match &p.kind {
                PromptKind::NewFolder { dir } => (NameJob::NewFolder(dir.join(&name)), dir.clone()),
                PromptKind::NewFile { dir } => (NameJob::NewFile(dir.join(&name)), dir.clone()),
                PromptKind::Rename { path } => {
                    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
                    let to = dir.join(&name);
                    if to == *path {
                        return None; // The name did not change.
                    }
                    (
                        NameJob::Rename {
                            from: path.clone(),
                            to,
                        },
                        dir,
                    )
                }
                PromptKind::SaveLayout
                | PromptKind::RenameLayout { .. }
                | PromptKind::RenamePin { .. }
                | PromptKind::RenameNetworkLocation { .. } => {
                    unreachable!("labels are accepted above")
                }
            };
            spawn_name_job(app, job, dir, name);
            None
        }
        Modal::Confirm(c) => {
            match c.kind {
                ConfirmKind::Recycle(t) => recycle(app, t),
                ConfirmKind::DeletePermanently(t) => delete_permanently(app, t),
                ConfirmKind::DeleteWithWindows(t) => crate::ops_ui::delete_with_windows(app, t),
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
            ids: vec![crate::cut_watch::IdState::Pending],
            cut: true,
            generation: 1,
            revision: 0,
            baseline_sequence: baseline,
            published: published.map(|r| r.map(|seq| token(u64::from(seq)))),
            _watch: None,
        }
    }

    fn token(raw: u64) -> forge_win::ClipboardToken {
        forge_win::ClipboardToken::from_raw(raw)
    }

    fn ours(sequence: u32) -> Option<forge_win::ClipboardToken> {
        Some(forge_win::ClipboardToken::from_raw(u64::from(sequence)))
    }

    #[test]
    fn paste_source_prefers_newest_copy() {
        // Nothing copied in-app: the Windows clipboard decides.
        assert_eq!(
            choose_paste_source(None, 5, None, SystemFiles::Files),
            PasteSource::System
        );
        assert_eq!(
            choose_paste_source(None, 5, None, SystemFiles::NoFiles),
            PasteSource::Empty
        );
        // Our publish is still pending and nothing else was copied since: ours, never the
        // stale Explorer paths still on the clipboard.
        assert_eq!(
            choose_paste_source(Some(&cb(None, 4)), 4, None, SystemFiles::Files),
            PasteSource::InApp
        );
        // Published and our token still on the clipboard: ours.
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Ok(7)), 4)), 7, ours(7), SystemFiles::Files),
            PasteSource::InApp
        );
        // Something was copied in Explorer afterwards: theirs.
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Ok(7)), 4)), 9, None, SystemFiles::Files),
            PasteSource::System
        );
        // Text copied afterwards (no files): fall back to ours rather than doing nothing.
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Ok(7)), 4)), 9, None, SystemFiles::NoFiles),
            PasteSource::InApp
        );
        // Publish failed: compare against the sequence from before our copy.
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Err(())), 4)), 4, None, SystemFiles::Files),
            PasteSource::InApp
        );
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Err(())), 4)), 6, None, SystemFiles::Files),
            PasteSource::System
        );
    }

    /// Another app put files on the clipboard just after our publish, so the sequence we
    /// read matches theirs: without our token it is not ours.
    #[test]
    fn a_matching_sequence_without_our_token_is_not_ours() {
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Ok(7)), 4)), 7, None, SystemFiles::Files),
            PasteSource::System
        );
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Ok(7)), 4)), 7, ours(8), SystemFiles::Files),
            PasteSource::System,
            "an older publish of ours"
        );
    }

    #[test]
    fn an_unreadable_clipboard_never_pastes_stale_in_app_items() {
        let unreadable = SystemFiles::Unreadable;
        assert_eq!(
            choose_paste_source(None, 5, None, unreadable),
            PasteSource::Unreadable
        );
        // Changed since our copy (e.g. a cut in Explorer) but unreadable: stop, say why.
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Ok(7)), 4)), 9, None, unreadable),
            PasteSource::Unreadable
        );
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Err(())), 4)), 6, None, unreadable),
            PasteSource::Unreadable
        );
        // Published, but our token cannot be read: fail closed rather than guess from a
        // sequence number.
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Ok(7)), 4)), 7, None, unreadable),
            PasteSource::Unreadable
        );
        // Still publishing and nothing changed since the copy: ours.
        assert_eq!(
            choose_paste_source(Some(&cb(None, 4)), 4, None, unreadable),
            PasteSource::InApp
        );
    }

    /// A list this app put on the clipboard is never pasted as another app's files, even
    /// when updating it failed (the in-app items, checked first, are pasted instead).
    #[test]
    fn our_own_content_never_takes_the_system_path() {
        let mine = token((u64::from(std::process::id()) << 32) | 78);
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Err(())), 4)), 9, Some(mine), SystemFiles::Files),
            PasteSource::InApp
        );
        assert_eq!(
            choose_paste_source(Some(&cb(Some(Ok(7)), 4)), 9, Some(mine), SystemFiles::Files),
            PasteSource::InApp,
            "an older publish of ours"
        );
        assert_eq!(
            choose_paste_source(None, 9, Some(mine), SystemFiles::Files),
            PasteSource::Stale,
            "a used-up cut whose list is still there"
        );
    }

    #[test]
    fn a_waiting_paste_fires_only_for_its_cut_and_in_time() {
        let wait = PasteWait {
            dest: PathBuf::from(r"C:\dest"),
            generation: 3,
            revision: 1,
            since: std::time::Instant::now(),
        };
        assert!(wait.fires_for(3, 1));
        assert!(!wait.fires_for(3, 2), "the cut changed since");
        assert!(!wait.fires_for(4, 1), "another cut");
        let old = PasteWait {
            since: std::time::Instant::now() - PASTE_WAIT,
            ..wait
        };
        assert!(old.expired());
        assert!(!old.fires_for(3, 1), "never fires into an old destination");
    }

    /// A pending publish is not ownership: if another app has put files on the clipboard
    /// since the copy, those are newer.
    #[test]
    fn a_pending_publish_yields_to_files_copied_since() {
        let mine = token((u64::from(std::process::id()) << 32) | 77);
        assert_eq!(
            choose_paste_source(Some(&cb(None, 4)), 6, None, SystemFiles::Files),
            PasteSource::System
        );
        assert_eq!(
            choose_paste_source(Some(&cb(None, 4)), 6, ours(3), SystemFiles::Files),
            PasteSource::System,
            "another process's token"
        );
        // Our own content (this publish just landed, or an older one of ours): ours.
        assert_eq!(
            choose_paste_source(Some(&cb(None, 4)), 6, Some(mine), SystemFiles::Files),
            PasteSource::InApp
        );
        // Text copied since: nothing better to paste than ours.
        assert_eq!(
            choose_paste_source(Some(&cb(None, 4)), 6, None, SystemFiles::NoFiles),
            PasteSource::InApp
        );
    }
}

#[cfg(test)]
mod name_job_tests {
    use super::*;

    #[test]
    fn rename_moves_the_file_and_refuses_an_existing_name() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            d.path().join("a.txt"),
            d.path().join("b.txt"),
            d.path().join("c.txt"),
        );
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();

        let onto_b = NameJob::Rename {
            from: a.clone(),
            to: b.clone(),
        };
        let err = onto_b.run().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(err.to_string(), "a file with that name already exists");
        assert_eq!(
            std::fs::read(&b).unwrap(),
            b"b",
            "the existing file is untouched"
        );

        NameJob::Rename {
            from: a.clone(),
            to: c.clone(),
        }
        .run()
        .unwrap();
        assert!(!a.exists());
        assert_eq!(std::fs::read(&c).unwrap(), b"a");
    }

    #[test]
    fn a_case_only_rename_is_allowed() {
        let d = tempfile::tempdir().unwrap();
        let (lower, upper) = (d.path().join("photo.jpg"), d.path().join("PHOTO.jpg"));
        std::fs::write(&lower, b"x").unwrap();
        NameJob::Rename {
            from: lower,
            to: upper,
        }
        .run()
        .unwrap();
        let names: Vec<String> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["PHOTO.jpg"]);
    }

    #[test]
    fn two_concurrent_renames_to_one_name_end_in_one_success_and_one_conflict() {
        for _ in 0..20 {
            let d = tempfile::tempdir().unwrap();
            let target = d.path().join("final.txt");
            let sources = [d.path().join("a.txt"), d.path().join("b.txt")];
            std::fs::write(&sources[0], b"from a").unwrap();
            std::fs::write(&sources[1], b"from b").unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let handles: Vec<_> = sources
                .iter()
                .map(|from| {
                    let job = NameJob::Rename {
                        from: from.clone(),
                        to: target.clone(),
                    };
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        job.run()
                    })
                })
                .collect();
            let results: Vec<std::io::Result<()>> =
                handles.into_iter().map(|h| h.join().unwrap()).collect();
            assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "{results:?}");
            let conflict = results.iter().find_map(|r| r.as_ref().err()).unwrap();
            assert_eq!(conflict.kind(), std::io::ErrorKind::AlreadyExists);
            // Nothing lost: one file has the final name, the other is still under its own.
            let mut contents: Vec<Vec<u8>> = std::fs::read_dir(d.path())
                .unwrap()
                .map(|e| std::fs::read(e.unwrap().path()).unwrap())
                .collect();
            contents.sort();
            assert_eq!(contents, [b"from a".to_vec(), b"from b".to_vec()]);
        }
    }

    #[test]
    fn the_name_lane_runs_work_in_submission_order() {
        let lane = NameLane::new();
        let (tx, rx) = crossbeam_channel::unbounded();
        for i in 0..50u32 {
            let tx = tx.clone();
            lane.submit(Box::new(move || {
                // Early items take longer: a lane that ran them side by side would reorder.
                std::thread::sleep(std::time::Duration::from_micros(u64::from(50 - i) * 20));
                let _ = tx.send(i);
            }));
        }
        let got: Vec<u32> = (0..50)
            .map(|_| rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap())
            .collect();
        assert_eq!(got, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn a_rename_conflict_reopens_the_prompt_for_that_item() {
        let job = NameJob::Rename {
            from: PathBuf::from(r"C:\work\a.txt"),
            to: PathBuf::from(r"C:\work\b.txt"),
        };
        let prompt = job.conflict_prompt("b.txt", "a file with that name already exists".into());
        assert!(matches!(&prompt.kind, PromptKind::Rename { path } if path == Path::new(r"C:\work\a.txt")));
        assert_eq!(prompt.text, "b.txt");
        assert_eq!(prompt.error.as_deref(), Some("a file with that name already exists"));
        let folder = NameJob::NewFolder(PathBuf::from(r"C:\work\Fresh"));
        let prompt = folder.conflict_prompt("Fresh", "exists".into());
        assert!(matches!(&prompt.kind, PromptKind::NewFolder { dir } if dir == Path::new(r"C:\work")));
    }

    #[test]
    fn renaming_to_the_same_name_is_a_no_op() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        std::fs::write(&a, b"a").unwrap();
        NameJob::Rename {
            from: a.clone(),
            to: a.clone(),
        }
        .run()
        .unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), b"a");
    }

    #[test]
    fn a_failed_rollback_names_the_stranded_file_not_a_conflict() {
        use forge_core::rename::{RenameReport, RenameStatus, StrandedFile};
        let report = RenameReport {
            status: RenameStatus::RollbackIncomplete {
                failed: PathBuf::from(r"C:\work\a.txt"),
                error: "access denied".into(),
            },
            renamed: Vec::new(),
            reverted: Vec::new(),
            stranded: vec![StrandedFile {
                current: PathBuf::from(r"C:\work\.forge-rename-1-0-0-0.tmp"),
                original: PathBuf::from(r"C:\work\a.txt"),
                error: "access denied".into(),
            }],
            undo: Vec::new(),
        };
        // The target "exists" too: the stranded report must still win.
        let err = rename_outcome(report, false, || true).unwrap_err();
        assert_ne!(err.kind(), std::io::ErrorKind::AlreadyExists);
        let text = err.to_string();
        assert!(text.contains(r"C:\work\.forge-rename-1-0-0-0.tmp"), "{text}");
        assert!(text.contains(r"rename it back to C:\work\a.txt"), "{text}");

        let rolled_back = RenameReport {
            status: RenameStatus::RolledBack {
                failed: PathBuf::from(r"C:\work\a.txt"),
                error: "destination already exists".into(),
            },
            renamed: Vec::new(),
            reverted: Vec::new(),
            stranded: Vec::new(),
            undo: Vec::new(),
        };
        let err = rename_outcome(rolled_back, false, || true).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn each_share_and_the_local_disks_get_their_own_lane() {
        let local = lane_key(Path::new(r"C:\photos\a.jpg"));
        assert_eq!(local, lane_key(Path::new(r"D:\other\b.jpg")));
        let share = lane_key(Path::new(r"\\nas\photos\2024\a.jpg"));
        assert_eq!(share, lane_key(Path::new(r"\\NAS\Photos\2025\b.jpg")));
        assert_ne!(share, local);
        assert_ne!(share, lane_key(Path::new(r"\\nas\video\a.mov")));
    }

    #[test]
    fn a_stuck_lane_does_not_hold_up_another() {
        let (stuck, other) = (NameLane::new(), NameLane::new());
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
        stuck.submit(Box::new(move || {
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(10));
        }));
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        other.submit(Box::new(move || {
            let _ = done_tx.send(());
        }));
        assert!(done_rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok());
        let _ = release_tx.send(());
    }

    #[test]
    fn a_restored_undo_record_goes_back_below_newer_ones() {
        // Taken from the top of a stack of 3 (index 2); one record arrived meanwhile.
        let slot = UndoSlot {
            index: 2,
            trimmed: 0,
        };
        assert_eq!(slot.index_now(3, 0), 2);
        // The cap dropped one old record meanwhile: one lower.
        assert_eq!(slot.index_now(3, 1), 1);
        // Never past the end.
        assert_eq!(slot.index_now(1, 0), 1);
    }

    #[test]
    fn a_conflict_prompt_waits_for_the_open_dialog_to_close() {
        let prompt = |name: &str| {
            crate::modal::Prompt::new(
                PromptKind::NewFolder {
                    dir: PathBuf::from(r"C:\work"),
                },
                "New folder",
                name,
            )
        };
        let mut pending = std::collections::VecDeque::from([prompt("one"), prompt("two")]);
        let mut modal = Some(Modal::Report {
            title: "Busy".into(),
            lines: Vec::new(),
        });
        assert!(!show_pending_prompt(&mut modal, false, &mut pending));
        assert_eq!(pending.len(), 2);
        modal = None;
        assert!(!show_pending_prompt(&mut modal, true, &mut pending));
        assert!(show_pending_prompt(&mut modal, false, &mut pending));
        assert!(matches!(&modal, Some(Modal::Prompt(p)) if p.text == "one"));
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn new_folder_and_file_never_replace_an_existing_entry() {
        let d = tempfile::tempdir().unwrap();
        let folder = d.path().join("Fresh");
        let file = d.path().join("note.txt");
        NameJob::NewFolder(folder.clone()).run().unwrap();
        NameJob::NewFile(file.clone()).run().unwrap();
        assert!(folder.is_dir() && file.is_file());
        std::fs::write(&file, b"keep").unwrap();
        assert!(NameJob::NewFolder(folder).run().is_err());
        assert!(NameJob::NewFile(file.clone()).run().is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    }
}
