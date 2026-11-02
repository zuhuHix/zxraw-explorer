//! Browsing archives like folders: a pane sent to `D:\x\a.zip\sub` lists the archive's
//! folder from its cached index (see `forge_core::archive`). The paths shown are virtual:
//! nothing that reads or changes files may reach them, so the file services skip archive
//! panes and the file commands are refused with a toast ([`block_in_archive`]). Copying in
//! and out and deleting are in `archive_edit`. Small images get thumbnails and a preview
//! from temp copies made in the background ([`Materialize`]).
//!
//! Opening a file inside extracts it to a private temp folder first (a job), asking for the
//! archive's password when an entry is encrypted. Passwords are kept for the session only
//! ([`ArchivePasswords`]), never logged and never saved.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{Key, RichText};
use forge_core::archive::{self, ArchiveIndex, ArchiveNode, Password};
use forge_core::ops::JobCtx;
use forge_core::{CoreError, EntryKind, FileEntry, MediaKind};

use crate::app::ZxrawApp;
use crate::commands::Cmd;

/// Largest entry opened from an archive (extracted to temp first).
const OPEN_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest image entry given a thumbnail and preview (from a temp copy).
pub const PREVIEW_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Temp copies for previews made at once, and waiting at most.
const MATERIALIZE_WORKERS: usize = 2;
const MATERIALIZE_QUEUE: usize = 64;

/// What a pane in an archive shows: the archive file, the folder inside it (`/`-separated,
/// `""` for the root) and the archive's index.
#[derive(Clone)]
pub struct ArchiveView {
    pub archive: PathBuf,
    pub inner: String,
    pub index: Arc<ArchiveIndex>,
}

impl ArchiveView {
    /// Inner path of a listed path (`None` for paths outside the archive or the archive).
    pub fn inner_of(&self, path: &Path) -> Option<String> {
        let rest = path.strip_prefix(&self.archive).ok()?;
        let inner: Vec<String> = rest
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        (!inner.is_empty()).then(|| inner.join("/"))
    }

    /// The archive node of a listed path.
    pub fn node_of(&self, path: &Path) -> Option<&ArchiveNode> {
        self.index.node(&self.inner_of(path)?)
    }
}

/// The result of listing a path inside an archive (on a listing thread).
pub enum ArchiveListing {
    Folder {
        view: ArchiveView,
        entries: Vec<FileEntry>,
    },
    /// The path is a file inside the archive: show its folder with it selected.
    File,
    Failed(String),
}

/// Lists `path` when it points into an archive; `None` for ordinary paths. Touches the disk
/// only for path components with an archive extension (see `split_archive_path`), so call
/// it off the UI thread.
pub fn list_archive_path(path: &Path) -> Option<ArchiveListing> {
    let (file, inner) = archive::split_archive_path(path)?;
    let index = match archive::index_cached(&file) {
        Ok(index) => index,
        Err(e) => return Some(ArchiveListing::Failed(e.to_string())),
    };
    let Some(children) = index.children(&inner) else {
        return Some(if index.node(&inner).is_some() {
            ArchiveListing::File
        } else {
            ArchiveListing::Failed(format!("{inner}: not found in the archive"))
        });
    };
    let entries = children.iter().map(|n| file_entry(&file, n)).collect();
    let view = ArchiveView {
        archive: file,
        inner,
        index: index.clone(),
    };
    Some(ArchiveListing::Folder { view, entries })
}

/// The listing row of an archive node.
fn file_entry(archive: &Path, node: &ArchiveNode) -> FileEntry {
    let path = archive::inner_path(archive, &node.path);
    FileEntry {
        ext: if node.is_dir {
            String::new()
        } else {
            forge_core::model::ext_of(&path)
        },
        path,
        name: node.name.clone(),
        kind: if node.is_dir {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
        size: node.size,
        modified: node.modified,
        created: None,
        hidden: false,
        readonly: false,
        system: false,
        online_only: false,
    }
}

/// Whether `path` may point into an archive, judged by name only: some component has an
/// archive extension. The listing finds out for sure.
pub fn maybe_in_archive(path: &Path) -> bool {
    path.ancestors()
        .any(|p| p.file_name().is_some() && archive::is_archive_name(p))
}

// ---------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------

/// The focused pane shows an archive's contents: `what` is refused with a toast. Returns
/// true when refused (the caller returns). Each file command calls this until it works
/// inside archives.
pub fn block_in_archive(app: &mut ZxrawApp, what: &str) -> bool {
    if !app.pane().in_archive() {
        return false;
    }
    app.toast(format!("{what}: not available inside an archive yet"));
    true
}

/// Like [`block_in_archive`], for an operation on `paths` and `target` that may come from
/// any pane (a drag, a drop, the other pane): refused when one of them is inside an archive
/// some pane shows.
pub fn block_paths(app: &mut ZxrawApp, what: &str, paths: &[PathBuf], target: &Path) -> bool {
    let inside = std::iter::once(target)
        .chain(paths.iter().map(PathBuf::as_path))
        .any(|p| is_virtual(app, p));
    if inside {
        app.toast(format!("{what}: not available inside an archive yet"));
    }
    inside
}

/// Whether `path` is inside an archive shown by a pane (lexical; no disk access).
pub fn is_virtual(app: &ZxrawApp, path: &Path) -> bool {
    app.tabs
        .iter()
        .flat_map(|t| std::iter::once(&t.left).chain(t.right.as_ref()))
        .any(|pane| match &pane.archive {
            Some(view) => path != view.archive && path.starts_with(&view.archive),
            None => pane.in_archive() && path.starts_with(&pane.path),
        })
}

/// Commands that read or change the listed items as files (tools, metadata, ratings):
/// refused inside an archive.
pub fn needs_real_files(cmd: Cmd) -> bool {
    use Cmd::*;
    matches!(
        cmd,
        IndexFolder
            | AddToCollection
            | GenerateProxies
            | ContactSheet
            | BrowseArchive
            | CreateArchive
            | SplitFile
            | JoinFiles
            | GroupSimilar
            | ExportMetadata
            | LocateProxies
            | SelectOriginals
            | SelectExports
            | BatchExport
            | Deliver
            | EditMetadata
            | SyncFolders
            | FindReplace
            | VerifyBackup
            | FolderSize
            | FindDuplicates
            | ChecksumFiles
            | CompareFolders
            | OpenIn
    )
}

/// The list views' thumbnail source for a pane in an archive: no thumbnails, annotations,
/// Git badges or folder sizes (all would read the virtual paths as files).
pub struct NoFileServices;

impl crate::views::ThumbProvider for NoFileServices {
    fn thumb(&mut self, _: &egui::Ui, _: &FileEntry, _: u32) -> Option<egui::TextureHandle> {
        None
    }
}

// ---------------------------------------------------------------------------
// Temp copies for thumbnails and the preview
// ---------------------------------------------------------------------------

/// Whether a listed entry gets a thumbnail and preview from a temp copy: an image of at
/// most [`PREVIEW_MAX_BYTES`], not encrypted unless the archive's password is known.
pub fn wants_temp_copy(e: &FileEntry, node: &ArchiveNode, password_known: bool) -> bool {
    e.kind == EntryKind::File
        && !node.is_dir
        && matches!(e.media_kind(), MediaKind::Image | MediaKind::Raw)
        && node.size <= PREVIEW_MAX_BYTES
        && (!node.encrypted || password_known)
}

struct TempCopyJob {
    listed: PathBuf,
    archive: PathBuf,
    entry: String,
    password: Option<Password>,
}

/// Temp copies of single small entries (`archive::extract_entry_to_temp`), made by at most
/// [`MATERIALIZE_WORKERS`] threads, newest request first, so a thumbnail or the preview
/// can read a real file. Never the whole archive.
pub struct Materialize {
    /// By listed path: the temp copy, or `None` when it could not be made.
    done: HashMap<PathBuf, Option<PathBuf>>,
    queued: VecDeque<TempCopyJob>,
    running: HashSet<PathBuf>,
    tx: Sender<(PathBuf, Option<PathBuf>)>,
    rx: Receiver<(PathBuf, Option<PathBuf>)>,
}

impl Default for Materialize {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self {
            done: HashMap::new(),
            queued: VecDeque::new(),
            running: HashSet::new(),
            tx,
            rx,
        }
    }
}

impl Materialize {
    /// The temp copy of the entry `listed` (inner path `entry` of `archive`): `Some` once
    /// made; else it is queued (once) and `None` is returned.
    pub fn request(
        &mut self,
        listed: &Path,
        archive: &Path,
        entry: &str,
        password: Option<&Password>,
        ctx: &egui::Context,
    ) -> Option<PathBuf> {
        if let Some(done) = self.done.get(listed) {
            return done.clone();
        }
        if self.running.contains(listed) || self.queued.iter().any(|j| j.listed == listed) {
            return None;
        }
        if self.queued.len() >= MATERIALIZE_QUEUE {
            // The oldest requests are the ones scrolled away from.
            self.queued.pop_front();
        }
        self.queued.push_back(TempCopyJob {
            listed: listed.to_path_buf(),
            archive: archive.to_path_buf(),
            entry: entry.to_owned(),
            password: password.cloned(),
        });
        self.pump(ctx);
        None
    }

    /// Whether the entry's temp copy was made (`Some(false)`: it could not be).
    pub fn state(&self, listed: &Path) -> Option<bool> {
        self.done.get(listed).map(Option::is_some)
    }

    fn pump(&mut self, ctx: &egui::Context) {
        while self.running.len() < MATERIALIZE_WORKERS {
            let Some(job) = self.queued.pop_back() else {
                return;
            };
            self.running.insert(job.listed.clone());
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            let spawned = std::thread::Builder::new()
                .name("archive-preview".into())
                .spawn(move || {
                    let cancel = forge_jobs::CancelToken::new();
                    let progress = forge_jobs::Progress::new();
                    let job_ctx = JobCtx::new(&cancel, progress.as_ref());
                    let made = archive::extract_entry_to_temp(
                        &job.archive,
                        &job.entry,
                        job.password.as_ref(),
                        PREVIEW_MAX_BYTES,
                        &job_ctx,
                    );
                    if let Err(e) = &made {
                        tracing::debug!("no preview copy of {}: {e}", job.entry);
                    }
                    let _ = tx.send((job.listed, made.ok()));
                    ctx.request_repaint();
                });
            if spawned.is_err() {
                return;
            }
        }
    }

    /// Finished copies (call every frame). Returns true if any arrived.
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        let mut changed = false;
        while let Ok((listed, made)) = self.rx.try_recv() {
            self.running.remove(&listed);
            self.done.insert(listed, made);
            changed = true;
        }
        if changed {
            self.pump(ctx);
        }
        changed
    }

    /// Forget the copies of an archive that changed.
    pub fn forget(&mut self, archive: &Path) {
        self.done.retain(|p, _| !p.starts_with(archive));
        self.queued.retain(|j| !j.listed.starts_with(archive));
    }
}

/// The temp copy of a listed archive entry for its thumbnail or preview, requested when
/// missing; `None` while it is made or when the entry does not get one.
pub fn temp_copy_of(
    materialize: &mut Materialize,
    passwords: &ArchivePasswords,
    view: &ArchiveView,
    e: &FileEntry,
    ctx: &egui::Context,
) -> Option<FileEntry> {
    let node = view.node_of(&e.path)?;
    let password = passwords.get(&view.archive);
    if !wants_temp_copy(e, node, password.is_some()) {
        return None;
    }
    let entry = node.path.clone();
    let temp = materialize.request(&e.path, &view.archive, &entry, password, ctx)?;
    Some(FileEntry {
        path: temp,
        ..e.clone()
    })
}

/// The grid's thumbnail source in an archive pane: small images from their temp copies;
/// no annotations, Git badges or folder sizes.
pub struct ArchiveThumbs<'a> {
    pub thumbs: &'a mut crate::thumbs::ThumbManager,
    pub archive: &'a mut ArchiveUi,
    pub view: ArchiveView,
}

impl crate::views::ThumbProvider for ArchiveThumbs<'_> {
    fn thumb(&mut self, ui: &egui::Ui, e: &FileEntry, size: u32) -> Option<egui::TextureHandle> {
        let ui_state = &mut *self.archive;
        let real = temp_copy_of(
            &mut ui_state.materialize,
            &ui_state.passwords,
            &self.view,
            e,
            ui.ctx(),
        )?;
        self.thumbs.thumb(ui, &real, size)
    }
}

/// The preview panel's details of an entry inside an archive, from the archive's index.
pub fn info_grid(ui: &mut egui::Ui, e: &FileEntry, view: &ArchiveView) {
    let node = view.node_of(&e.path);
    egui::Grid::new("archive-info")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            let row = |ui: &mut egui::Ui, k: &str, v: String| {
                ui.label(RichText::new(k).color(ui.visuals().weak_text_color()));
                ui.add(egui::Label::new(v).wrap());
                ui.end_row();
            };
            row(ui, "Type", crate::views::type_label(e));
            let size = forge_core::model::format_size(e.size);
            row(ui, "Size", format!("{size} ({} bytes)", e.size));
            if let Some(n) = node {
                row(
                    ui,
                    "Compressed",
                    forge_core::model::format_size(n.compressed_size),
                );
                if let Some(ratio) = n.ratio_percent().filter(|_| !n.is_dir) {
                    row(ui, "Ratio", format!("{ratio}%"));
                }
                if !n.method.is_empty() {
                    row(ui, "Method", n.method.clone());
                }
                if n.encrypted {
                    row(ui, "Encrypted", "yes".into());
                }
            }
            row(ui, "Modified", crate::views::format_time(e.modified));
            row(ui, "Archive", view.archive.display().to_string());
            if !view.inner.is_empty() {
                row(ui, "Folder inside", view.inner.clone());
            }
        });
}

// ---------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------

/// Passwords of encrypted archives, by archive path, for this session only. Never logged
/// (`Password`'s `Debug` is redacted) and never saved; wiped when dropped.
#[derive(Default)]
pub struct ArchivePasswords(HashMap<PathBuf, Password>);

impl ArchivePasswords {
    fn key(archive: &Path) -> PathBuf {
        PathBuf::from(
            forge_core::paths::lexical_normalize(archive)
                .to_string_lossy()
                .to_lowercase(),
        )
    }
    pub fn get(&self, archive: &Path) -> Option<&Password> {
        self.0.get(&Self::key(archive))
    }
    pub fn set(&mut self, archive: &Path, password: Password) {
        self.0.insert(Self::key(archive), password);
    }
    pub fn forget(&mut self, archive: &Path) {
        self.0.remove(&Self::key(archive));
    }
}

// ---------------------------------------------------------------------------
// Opening files inside
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenHow {
    /// With the file's default app.
    Default,
    /// With Windows' "Open with" dialog.
    OpenWith,
    /// A nested archive: browse its temp copy in the pane.
    Browse,
}

#[derive(Debug, Clone)]
pub(crate) struct OpenRequest {
    archive: PathBuf,
    /// Inner path of the entry.
    entry: String,
    name: String,
    how: OpenHow,
    /// The pane's folder when asked (a nested archive opens only if the pane is still there).
    from: PathBuf,
}

/// A job that stopped for a password: run again once one is typed.
#[derive(Debug, Clone)]
pub(crate) enum Retry {
    Open(OpenRequest),
    Extract(crate::archive_extract::ExtractRequest),
    Test(crate::archive_extract::TestRequest),
    Update(crate::archive_edit::UpdateRequest),
}

impl Retry {
    fn archive(&self) -> &Path {
        match self {
            Retry::Open(r) => &r.archive,
            Retry::Extract(r) => &r.archive,
            Retry::Test(r) => &r.archive,
            Retry::Update(r) => &r.archive,
        }
    }

    /// What the dialog says is encrypted.
    fn what(&self) -> String {
        let archive = forge_core::paths::name_of(self.archive());
        match self {
            Retry::Open(r) => format!("{} in {archive} is encrypted.", r.name),
            Retry::Extract(_) | Retry::Test(_) => format!("{archive} is encrypted."),
            Retry::Update(_) => format!(
                "{archive} is encrypted. Its password is needed to change it (new files are                  encrypted with it)."
            ),
        }
    }
}

pub(crate) enum Msg {
    NeedPassword {
        retry: Retry,
        wrong: bool,
    },
    Browse {
        from: PathBuf,
        path: PathBuf,
    },
    /// An extraction would replace files: ask once how to handle them.
    Conflicts(crate::archive_extract::ConflictAsk),
    /// A "Test archive" job finished.
    Tested(crate::archive_extract::Tested),
    /// An archive was edited (see `archive_edit`): its temp copies are out of date.
    Updated(PathBuf),
}

struct PasswordPrompt {
    retry: Retry,
    text: String,
    wrong: bool,
    focused: bool,
}

/// Archive state of the app (in `app.ops.archive`).
pub struct ArchiveUi {
    pub passwords: ArchivePasswords,
    prompt: Option<PasswordPrompt>,
    /// Jobs waiting for the password dialog (several archives may need one), with whether
    /// the password they had was wrong.
    waiting: VecDeque<(Retry, bool)>,
    /// Extract questions and test results (see `archive_extract`).
    pub(crate) extract: crate::archive_extract::ExtractUi,
    /// Entries copied with Ctrl+C in an archive pane (see `archive_edit`).
    pub copied: Option<crate::archive_edit::ArchiveCopy>,
    /// Temp copies of small images for thumbnails and the preview.
    pub materialize: Materialize,
    pub(crate) tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Default for ArchiveUi {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self {
            passwords: ArchivePasswords::default(),
            prompt: None,
            waiting: VecDeque::new(),
            extract: Default::default(),
            copied: None,
            materialize: Materialize::default(),
            tx,
            rx,
        }
    }
}

impl ArchiveUi {
    /// The password dialog or an extract question is showing.
    pub fn prompt_open(&self) -> bool {
        self.prompt.is_some() || self.extract.question_open()
    }
}

impl Drop for PasswordPrompt {
    fn drop(&mut self) {
        wipe(&mut self.text);
    }
}

/// Open a file listed in the focused archive pane: extract it to temp, then open it `how`.
/// A nested archive is browsed instead of opened with an app.
pub fn open_entry(app: &mut ZxrawApp, e: &FileEntry, how: OpenHow) {
    let pane = app.pane();
    let Some(view) = &pane.archive else { return };
    let Some(node) = view.node_of(&e.path).filter(|n| !n.is_dir) else {
        return;
    };
    let how = if how == OpenHow::Default && archive::is_archive_name(&e.path) {
        OpenHow::Browse
    } else {
        how
    };
    let request = OpenRequest {
        archive: view.archive.clone(),
        entry: node.path.clone(),
        name: node.name.clone(),
        how,
        from: pane.path.clone(),
    };
    start_open(app, request);
}

fn start_open(app: &mut ZxrawApp, request: OpenRequest) {
    let password = app.ops.archive.passwords.get(&request.archive).cloned();
    let archive_name = forge_core::paths::name_of(&request.archive);
    let title = format!("Opening {} from {archive_name}", request.name);
    let msgs = app.ops.archive.tx.clone();
    let events = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops.jobs.spawn("archive-open", title, move |job| {
        let job_ctx = JobCtx::from_job(job);
        let r = archive::extract_entry_to_temp(
            &request.archive,
            &request.entry,
            password.as_ref(),
            OPEN_MAX_BYTES,
            &job_ctx,
        );
        let result = match r {
            Ok(path) => match request.how {
                OpenHow::Browse => {
                    let from = request.from.clone();
                    let _ = msgs.send(Msg::Browse { from, path });
                    Ok(())
                }
                OpenHow::Default => forge_win::open_default(&path).map_err(|e| e.to_string()),
                OpenHow::OpenWith => forge_win::open_with_dialog(&path).map_err(|e| e.to_string()),
            },
            Err(CoreError::PasswordRequired(_)) => {
                let _ = msgs.send(Msg::NeedPassword {
                    retry: Retry::Open(request),
                    wrong: false,
                });
                Ok(())
            }
            Err(CoreError::WrongPassword(_)) => {
                let _ = msgs.send(Msg::NeedPassword {
                    retry: Retry::Open(request),
                    wrong: true,
                });
                Ok(())
            }
            Err(CoreError::Cancelled) => Ok(()),
            Err(e) => Err(e.to_string()),
        };
        if let Err(e) = &result {
            let _ = events.send(crate::actions::Event::Error(format!("Open failed: {e}")));
        }
        ctx.request_repaint();
        result
    });
}

/// Results of open, extract and test jobs (call every frame). Returns true if anything
/// changed.
pub fn poll(app: &mut ZxrawApp) -> bool {
    let mut changed = false;
    while let Ok(msg) = app.ops.archive.rx.try_recv() {
        changed = true;
        match msg {
            Msg::NeedPassword { retry, wrong } => {
                if wrong {
                    app.ops.archive.passwords.forget(retry.archive());
                }
                // One prompt at a time; the others wait their turn.
                app.ops.archive.waiting.push_back((retry, wrong));
            }
            Msg::Browse { from, path } => {
                // Only where it was asked: the user may have moved on meanwhile.
                if app.pane().path == from {
                    app.pane_mut().navigate(path);
                }
            }
            Msg::Conflicts(ask) => app.ops.archive.extract.ask(ask),
            Msg::Tested(tested) => app.ops.archive.extract.tested(tested),
            Msg::Updated(archive) => app.ops.archive.materialize.forget(&archive),
        }
    }
    changed |= app.ops.archive.materialize.poll(&app.ops.ctx);
    let ui = &mut app.ops.archive;
    if ui.prompt.is_none()
        && let Some((retry, wrong)) = ui.waiting.pop_front()
    {
        changed = true;
        // Another job of this archive got its password meanwhile: just run it again.
        if !wrong && ui.passwords.get(retry.archive()).is_some() {
            run_retry(app, retry);
        } else {
            ui.prompt = Some(PasswordPrompt {
                retry,
                text: String::new(),
                wrong,
                focused: false,
            });
        }
    }
    changed
}

fn run_retry(app: &mut ZxrawApp, retry: Retry) {
    match retry {
        Retry::Open(r) => start_open(app, r),
        Retry::Extract(r) => crate::archive_extract::start_extract(app, r),
        Retry::Test(r) => crate::archive_extract::start_test(app, r),
        Retry::Update(r) => crate::archive_edit::start_update(app, r),
    }
}

/// Ask for an archive's password before running `retry` (queued behind other prompts).
pub(crate) fn ask_password(app: &mut ZxrawApp, retry: Retry, wrong: bool) {
    let _ = app.ops.archive.tx.send(Msg::NeedPassword { retry, wrong });
}

/// Overwrite the typed password before its memory is freed.
pub(crate) fn wipe(text: &mut String) {
    let mut bytes = std::mem::take(text).into_bytes();
    bytes.iter_mut().for_each(|b| *b = 0);
    std::hint::black_box(&bytes);
}

/// egui keeps each text field's state, with an undo history of what was typed, under the
/// field's id: forget it for password fields once they are done with.
pub(crate) fn forget_text_state(ctx: &egui::Context, id: egui::Id) {
    ctx.data_mut(|d| {
        if let Some(mut state) = d.get_persisted::<egui::text_edit::TextEditState>(id) {
            // The undo history is shared with any copy egui still holds.
            state.clear_undoer();
        }
        d.remove::<egui::text_edit::TextEditState>(id);
    });
}

fn password_field_id() -> egui::Id {
    egui::Id::new("archive-password-field")
}

/// The password dialog for an encrypted entry (drawn every frame while open).
pub fn password_modal(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(mut prompt) = app.ops.archive.prompt.take() else {
        return;
    };
    let enter = ctx.input(|i| i.key_pressed(Key::Enter));
    let esc = ctx.input(|i| i.key_pressed(Key::Escape));
    let mut accept = false;
    let mut cancel = false;
    let resp = egui::Modal::new(egui::Id::new("archive-password")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading("Password needed");
        ui.add_space(6.0);
        ui.label(prompt.retry.what());
        if prompt.wrong {
            ui.label(
                RichText::new("That password is not right. Try again.")
                    .color(ui.visuals().error_fg_color),
            );
        }
        ui.add_space(6.0);
        let r = ui.add(
            egui::TextEdit::singleline(&mut prompt.text)
                .id(password_field_id())
                .password(true)
                .hint_text("Password")
                .desired_width(f32::INFINITY),
        );
        if !prompt.focused {
            r.request_focus();
            prompt.focused = true;
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("OK").clicked() || (enter && !prompt.text.is_empty()) {
                accept = true;
            }
            if ui.button("Cancel").clicked() || esc {
                cancel = true;
            }
        });
    });
    if accept && !prompt.text.is_empty() {
        let password = Password::new(&prompt.text);
        wipe(&mut prompt.text);
        forget_text_state(ctx, password_field_id());
        app.ops
            .archive
            .passwords
            .set(prompt.retry.archive(), password);
        run_retry(app, prompt.retry.clone());
    } else if cancel || resp.should_close() {
        wipe(&mut prompt.text);
        forget_text_state(ctx, password_field_id());
        // Other jobs of the same archive wait for the same password: give them up too.
        let archive = prompt.retry.archive().to_path_buf();
        app.ops
            .archive
            .waiting
            .retain(|(r, _)| !forge_core::paths::paths_equal(r.archive(), &archive));
    } else {
        app.ops.archive.prompt = Some(prompt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_kept_per_archive_ignoring_case() {
        let mut store = ArchivePasswords::default();
        store.set(Path::new(r"C:\x\A.zip"), Password::new("pw"));
        assert_eq!(
            store.get(Path::new(r"c:\x\a.ZIP")).map(Password::as_bytes),
            Some(&b"pw"[..])
        );
        assert!(store.get(Path::new(r"C:\x\b.zip")).is_none());
        store.forget(Path::new(r"C:\X\a.zip"));
        assert!(store.get(Path::new(r"C:\x\A.zip")).is_none());
        assert!(!format!("{:?}", Password::new("secret")).contains("secret"));
    }

    #[test]
    fn typed_password_state_is_forgotten() {
        let ctx = egui::Context::default();
        let id = password_field_id();
        let mut text = String::new();
        let mut frame = |events: Vec<egui::Event>, text: &mut String| {
            let input = egui::RawInput {
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let r = ui.add(egui::TextEdit::singleline(text).id(id).password(true));
                r.request_focus();
            });
            out.textures_delta.clear();
        };
        frame(Vec::new(), &mut text);
        frame(vec![egui::Event::Text("hunter2".into())], &mut text);
        assert_eq!(text, "hunter2");
        let kept = egui::text_edit::TextEditState::load(&ctx, id).expect("egui keeps state");
        forget_text_state(&ctx, id);
        assert!(egui::text_edit::TextEditState::load(&ctx, id).is_none());
        // The undo history shared with copies egui may still hold is emptied too.
        assert!(!kept.undoer().has_undo(&(Default::default(), String::new())));
    }

    #[test]
    fn archive_names_are_found_by_name_only() {
        assert!(maybe_in_archive(Path::new(r"C:\x\a.zip\sub")));
        assert!(maybe_in_archive(Path::new(r"C:\x\a.zip")));
        assert!(!maybe_in_archive(Path::new(r"C:\x\zip\sub")));
        assert!(!maybe_in_archive(Path::new(r"C:\")));
    }

    #[test]
    fn only_small_images_get_temp_copies() {
        let node = |name: &str, size, encrypted| ArchiveNode {
            name: name.into(),
            path: name.into(),
            is_dir: false,
            size,
            compressed_size: size,
            modified: None,
            encrypted,
            method: String::new(),
            entry: Some(0),
        };
        let archive = Path::new(r"C:\x\a.zip");
        let check = |n: ArchiveNode, known| wants_temp_copy(&file_entry(archive, &n), &n, known);
        assert!(check(node("p.jpg", 10, false), false));
        assert!(!check(node("notes.txt", 10, false), false));
        assert!(!check(node("p.jpg", PREVIEW_MAX_BYTES + 1, false), false));
        // Encrypted: only with the archive's password.
        assert!(!check(node("p.jpg", 10, true), false));
        assert!(check(node("p.jpg", 10, true), true));
    }

    #[test]
    fn inner_paths_of_listed_rows() {
        let view = ArchiveView {
            archive: PathBuf::from(r"C:\x\a.zip"),
            inner: String::new(),
            index: Arc::new(ArchiveIndex::build(forge_core::archive::ArchiveListing {
                format: forge_core::archive::ArchiveFormat::Zip,
                entries: Vec::new(),
                comment: String::new(),
            })),
        };
        assert_eq!(
            view.inner_of(Path::new(r"C:\x\a.zip\sub\f.txt")).as_deref(),
            Some("sub/f.txt")
        );
        assert_eq!(view.inner_of(Path::new(r"C:\x\a.zip")), None);
        assert_eq!(view.inner_of(Path::new(r"C:\x\b.zip\f.txt")), None);
    }
}
