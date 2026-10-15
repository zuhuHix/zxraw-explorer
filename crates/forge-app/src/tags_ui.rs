//! Tags/keywords and notes editor for the selection, shown in the preview panel (WP-T), and a
//! "Manage tags" window for renaming, merging and deleting tags.
//!
//! Reads run on worker threads. Edits go through the library's single writer so they keep the
//! write ordering and offline handling. The panel applies an edit to its cache straight away and
//! re-reads a moment later, because the writer may not have finished when a read is issued.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use egui::{Key, Modifiers, RichText, Ui};
use forge_core::FileEntry;
use forge_db::{Annotation, AnnotationChange, Db, TagCount};

use crate::app::ZxrawApp;
use crate::library::Library;
use crate::pane::Pane;

/// How long after an edit the cached tags are re-read.
const RELOAD_AFTER_EDIT: Duration = Duration::from_millis(1200);
const MAX_SUGGESTIONS: usize = 6;

enum Msg {
    Files {
        generation: u64,
        paths: Vec<PathBuf>,
        result: Result<Vec<(PathBuf, Annotation)>, String>,
    },
    Tags {
        generation: u64,
        result: Result<Vec<TagCount>, String>,
    },
    Manage(Result<String, String>),
}

/// The selection the panel shows, with its annotations once they have been read.
struct Selected {
    paths: Vec<PathBuf>,
    files: Option<Vec<(PathBuf, Annotation)>>,
}

/// A note is saved once its text has been unchanged for this long.
const NOTE_AUTOSAVE_AFTER: Duration = Duration::from_millis(800);
/// The note box grows with its text up to this many rows, then scrolls.
const NOTE_MAX_ROWS: f32 = 8.0;

/// Notes for the single selected file. Saved after a pause in typing, when the field loses
/// focus, when the selection moves to another file, when the panel is closed, and at exit.
struct NotesDraft {
    path: PathBuf,
    text: String,
    /// The text the database has confirmed.
    saved: String,
    /// When the text last changed while it differed from `saved`. Cleared by a save.
    changed_at: Option<Instant>,
    /// A save sent to the library writer and not answered yet.
    saving: Option<PendingSave>,
    /// Why the last save failed. Cleared by the next edit or save.
    error: Option<String>,
    /// A draft brought back from [`TagsUi::left_behind`] waits for a fresh read of the
    /// database before it may be saved: the note may have changed meanwhile (a move onto
    /// another annotated file merges both notes).
    check_db: bool,
    /// The database's note when it changed since this draft was taken. Nothing is saved until
    /// the user chooses which text to keep.
    conflict: Option<String>,
}

impl NotesDraft {
    fn new(path: PathBuf, text: String) -> Self {
        NotesDraft {
            path,
            saved: text.clone(),
            text,
            changed_at: None,
            saving: None,
            error: None,
            check_db: false,
            conflict: None,
        }
    }

    /// Compares the draft with the database's current `note`. A conflict found earlier is
    /// replaced by the latest note, which is what a merge or the recovery file must keep.
    fn check_against(&mut self, note: String) {
        self.check_db = false;
        if self.text == self.saved {
            // No unsaved edit: show what the database has now.
            self.conflict = None;
            if note.trim() != self.saved.trim() {
                self.text = note.clone();
                self.saved = note;
            }
            return;
        }
        if note.trim() == self.text.trim() {
            // The edit is already in the database.
            self.saved = self.text.clone();
            self.error = None;
            self.conflict = None;
            return;
        }
        // The note the edit is up against so far: unchanged since, nothing new to ask.
        let known = self.conflict.as_deref().unwrap_or(&self.saved);
        if note.trim() != known.trim() {
            self.conflict = Some(note);
        }
    }

    /// Settles a conflict: the database's note, the draft, or both joined. The database's
    /// note becomes what is confirmed, so a chosen draft is saved after the autosave pause.
    fn resolve(&mut self, choice: ConflictChoice, now: Instant) {
        let Some(note) = self.conflict.take() else {
            return;
        };
        self.error = None;
        match choice {
            ConflictChoice::KeepSaved => {
                self.text = note.clone();
                self.changed_at = None;
            }
            ConflictChoice::UseDraft => self.changed_at = Some(now),
            ConflictChoice::Merge => {
                self.text = merged_note(&self.text, &note);
                self.changed_at = Some(now);
            }
        }
        self.saved = note;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConflictChoice {
    KeepSaved,
    UseDraft,
    Merge,
}

use crate::library::merged_note;

struct PendingSave {
    text: String,
    result: crossbeam_channel::Receiver<Result<(), String>>,
}

/// Picks up the answer to the draft's save in flight, if any.
fn poll_draft(draft: &mut NotesDraft, ctx: &egui::Context) {
    let Some(pending) = draft.saving.as_ref() else {
        return;
    };
    match pending.result.try_recv() {
        Ok(Ok(())) => {
            draft.saved = pending.text.clone();
            draft.saving = None;
        }
        Ok(Err(error)) => {
            draft.error = Some(error);
            draft.saving = None;
        }
        Err(crossbeam_channel::TryRecvError::Disconnected) => {
            draft.error = Some("the library database is not available".into());
            draft.saving = None;
        }
        Err(crossbeam_channel::TryRecvError::Empty) => {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

/// What the note's status line says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoteStatus {
    Unsaved,
    Saving,
    Saved,
    Failed(String),
    Empty,
    /// A brought-back draft waits for the database's note.
    Checking,
    /// The database's note changed since the draft was taken.
    Conflict,
}

fn note_status(draft: &NotesDraft) -> NoteStatus {
    if draft.conflict.is_some() {
        NoteStatus::Conflict
    } else if draft.check_db && draft.saving.is_none() && draft.text != draft.saved {
        NoteStatus::Checking
    } else if draft.saving.is_some() {
        NoteStatus::Saving
    } else if draft.text != draft.saved {
        match &draft.error {
            Some(error) => NoteStatus::Failed(error.clone()),
            None => NoteStatus::Unsaved,
        }
    } else if draft.text.trim().is_empty() {
        NoteStatus::Empty
    } else {
        NoteStatus::Saved
    }
}

/// True when the text needs saving: it differs from what is confirmed and from any save in
/// flight. A draft not yet compared with the database, or in conflict with it, is never
/// saved.
fn note_dirty(draft: &NotesDraft) -> bool {
    draft.text != draft.saved
        && draft.saving.as_ref().is_none_or(|p| p.text != draft.text)
        && !draft.check_db
        && draft.conflict.is_none()
}

/// True when the text needs saving and has been left alone for the autosave pause. After a
/// failed save, autosave waits for the next edit (focus loss still retries).
fn autosave_due(draft: &NotesDraft, now: Instant) -> bool {
    note_dirty(draft)
        && draft.error.is_none()
        && draft
            .changed_at
            .is_some_and(|at| now.saturating_duration_since(at) >= NOTE_AUTOSAVE_AFTER)
}

struct Renaming {
    old: String,
    new: String,
}

enum ManageOp {
    Rename { old: String, new: String },
    Delete { name: String },
}

enum ManageAction {
    StartRename(String),
    CancelRename,
    Rename { old: String, new: String },
    AskDelete(String),
    CancelDelete,
    Delete(String),
}

#[derive(Default)]
struct ManageState {
    filter: String,
    renaming: Option<Renaming>,
    confirm_delete: Option<String>,
    busy: bool,
    notice: Option<Result<String, String>>,
}

pub struct TagsUi {
    pub manage_open: bool,
    cache: Option<Selected>,
    /// Bumped by every edit and selection change, so reads started before it are discarded.
    generation: u64,
    files_loading: bool,
    all_tags: Option<Vec<TagCount>>,
    tags_loading: bool,
    reload_at: Option<Instant>,
    /// Set when a read failed. Reads are not retried automatically; the panel offers Retry.
    error: Option<String>,
    input: String,
    highlight: Option<usize>,
    notes: Option<NotesDraft>,
    /// Drafts of files no longer selected whose save the database has not confirmed: in
    /// flight, or failed. Dropped once confirmed; a failed one stays until it is saved (select
    /// the file again to retry) and is kept in the recovery file at exit.
    left_behind: HashMap<PathBuf, NotesDraft>,
    /// Answered once the library has relinked the rows of files ZXraw moved. Until then, and
    /// until the selection is read again, no draft is compared with the database.
    relinked: Option<crossbeam_channel::Receiver<()>>,
    /// The frame the panel was last drawn in. A note left behind when the panel stops being
    /// drawn is saved by [`tick`].
    panel_frame: u64,
    manage: ManageState,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Default for TagsUi {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            manage_open: false,
            cache: None,
            generation: 0,
            files_loading: false,
            all_tags: None,
            tags_loading: false,
            reload_at: None,
            error: None,
            input: String::new(),
            highlight: None,
            notes: None,
            left_behind: HashMap::new(),
            relinked: None,
            panel_frame: 0,
            manage: ManageState::default(),
            tx,
            rx,
        }
    }
}

/// Tags on every file (`common`) and on some but not all (`mixed`), compared case-insensitively.
/// Each keeps the spelling of the first file that has it. Both lists are sorted.
pub fn summarize_tags(files: &[&[String]]) -> (Vec<String>, Vec<String>) {
    let mut seen: HashMap<String, (String, usize)> = HashMap::new();
    for tags in files {
        for tag in tags.iter() {
            seen.entry(tag.to_lowercase())
                .or_insert_with(|| (tag.clone(), 0))
                .1 += 1;
        }
    }
    let mut entries: Vec<(String, (String, usize))> = seen.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut common = Vec::new();
    let mut mixed = Vec::new();
    for (_, (name, count)) in entries {
        if count >= files.len() {
            common.push(name);
        } else {
            mixed.push(name);
        }
    }
    (common, mixed)
}

/// Existing tags that match `query`, best first: prefix matches, then substring matches, then
/// loose matches where the query's letters appear in order. Within a tier the most-used tag
/// comes first. Tags in `exclude` (case-insensitive) are skipped. An empty query lists the
/// most-used tags.
pub fn rank_suggestions<'a>(
    query: &str,
    tags: &'a [TagCount],
    exclude: &[String],
    limit: usize,
) -> Vec<&'a TagCount> {
    let q = query.trim().to_lowercase();
    let excluded: Vec<String> = exclude.iter().map(|t| t.to_lowercase()).collect();
    let mut scored: Vec<(u8, &TagCount)> = tags
        .iter()
        .filter_map(|tag| {
            let name = tag.name.to_lowercase();
            if excluded.contains(&name) {
                return None;
            }
            let tier = if name.starts_with(&q) {
                0
            } else if name.contains(&q) {
                1
            } else if is_subsequence(&q, &name) {
                2
            } else {
                return None;
            };
            Some((tier, tag))
        })
        .collect();
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(b.1.files.cmp(&a.1.files))
            .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
    });
    scored.into_iter().take(limit).map(|(_, tag)| tag).collect()
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

/// Tag names typed into the input. Commas separate several tags at once.
fn parse_tags(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Mirror the database's effect on the cached annotations: adds first, then removals.
fn apply_tag_change(files: &mut [(PathBuf, Annotation)], add: &[String], remove: &[String]) {
    for (_, annotation) in files.iter_mut() {
        for tag in add {
            if !annotation.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                annotation.tags.push(tag.clone());
            }
        }
        annotation
            .tags
            .retain(|t| !remove.iter().any(|r| r.eq_ignore_ascii_case(t)));
        annotation.tags.sort_by_key(|t| t.to_lowercase());
    }
}

/// The paths tag edits apply to: the selected items the list shows, or the file the panel
/// shows (the cursor) when none is. Hidden or filtered-out picks are never edited unseen.
fn selection_paths(pane: &Pane, cursor: &Path) -> Vec<PathBuf> {
    let shown: Vec<PathBuf> = pane.selected_entries().iter().map(|e| e.path.clone()).collect();
    let mut paths: Vec<PathBuf> = if shown.is_empty() {
        vec![cursor.to_path_buf()]
    } else {
        shown
    };
    paths.sort();
    paths
}

fn valid_tag_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(',')
}

fn read_files(db: &Db, paths: &[PathBuf]) -> Result<Vec<(PathBuf, Annotation)>, String> {
    let ids: HashMap<PathBuf, i64> = db
        .file_ids(paths)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    paths
        .iter()
        .map(|path| {
            let annotation = match ids.get(path) {
                Some(&id) => db.annotation(id).map_err(|e| e.to_string())?,
                None => Annotation::default(),
            };
            Ok((path.clone(), annotation))
        })
        .collect()
}

fn plural(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

impl TagsUi {
    fn poll(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Files {
                    generation,
                    paths,
                    result,
                } => {
                    self.files_loading = false;
                    if generation != self.generation {
                        continue;
                    }
                    let Some(cache) = self.cache.as_mut() else {
                        continue;
                    };
                    if cache.paths != paths {
                        continue;
                    }
                    match result {
                        Ok(files) => cache.files = Some(files),
                        Err(error) => self.error = Some(error),
                    }
                }
                Msg::Tags { generation, result } => {
                    self.tags_loading = false;
                    if generation != self.generation {
                        continue;
                    }
                    match result {
                        Ok(tags) => self.all_tags = Some(tags),
                        Err(error) => self.error = Some(error),
                    }
                }
                Msg::Manage(result) => {
                    self.manage.busy = false;
                    self.manage.renaming = None;
                    self.manage.confirm_delete = None;
                    self.manage.notice = Some(result);
                    // The database is already updated when this arrives; reload right away.
                    self.generation += 1;
                    self.reload_at = Some(Instant::now());
                }
            }
        }
    }

    /// Start the reads that are needed and not already running. Never blocks.
    fn drive_loads(&mut self, db: Option<&Arc<Db>>, ctx: &egui::Context) {
        let Some(db) = db else { return };
        let due = self.reload_at.is_some_and(|at| at <= Instant::now());
        let idle = !self.files_loading && !self.tags_loading;
        let forced = due && idle;
        if forced {
            self.reload_at = None;
            self.error = None;
        }
        if !self.files_loading
            && let Some(cache) = &self.cache
            && (forced || (cache.files.is_none() && self.error.is_none()))
        {
            self.files_loading = true;
            let paths = cache.paths.clone();
            let generation = self.generation;
            let tx = self.tx.clone();
            let repaint = ctx.clone();
            let db = db.clone();
            std::thread::spawn(move || {
                let result = read_files(&db, &paths);
                let _ = tx.send(Msg::Files {
                    generation,
                    paths,
                    result,
                });
                repaint.request_repaint();
            });
        }
        if !self.tags_loading && (forced || (self.all_tags.is_none() && self.error.is_none())) {
            self.tags_loading = true;
            let generation = self.generation;
            let tx = self.tx.clone();
            let repaint = ctx.clone();
            let db = db.clone();
            std::thread::spawn(move || {
                let result = db.tags().map_err(|e| e.to_string());
                let _ = tx.send(Msg::Tags { generation, result });
                repaint.request_repaint();
            });
        }
    }

    /// Make the cache follow the selection. Switching selection invalidates older reads.
    fn sync_selection(&mut self, paths: &[PathBuf]) {
        if self.cache.as_ref().is_some_and(|c| c.paths == paths) {
            return;
        }
        self.cache = Some(Selected {
            paths: paths.to_vec(),
            files: None,
        });
        self.generation += 1;
        self.error = None;
        self.highlight = None;
    }

    /// Mark the cache stale and schedule a re-read once the writer has had time to apply the edit.
    fn edited(&mut self, ctx: &egui::Context) {
        self.generation += 1;
        self.reload_at = Some(Instant::now() + RELOAD_AFTER_EDIT);
        ctx.request_repaint_after(RELOAD_AFTER_EDIT);
    }

    /// Per-file notes for the single selected file. Switching files saves the previous draft.
    fn sync_notes(&mut self, library: &mut Library, ctx: &egui::Context) {
        let want = match &self.cache {
            Some(c) if c.paths.len() == 1 => Some(c.paths[0].clone()),
            _ => None,
        };
        if self.notes.as_ref().map(|d| &d.path) == want.as_ref() {
            self.check_restored_note();
            return;
        }
        self.save_notes(library, ctx);
        if let Some(draft) = self.notes.take()
            && unconfirmed_note(&draft).is_some()
        {
            self.left_behind.insert(draft.path.clone(), draft);
        }
        let Some(path) = want else {
            return;
        };
        // Back on a file whose save is unconfirmed: show that draft, not the database text,
        // and compare it with what the database has now before saving it.
        if let Some(mut draft) = self.left_behind.remove(&path) {
            draft.check_db = true;
            self.notes = Some(draft);
            self.check_restored_note();
            return;
        }
        let Some(cache) = self.cache.as_ref() else {
            return;
        };
        let Some(files) = cache.files.as_ref() else {
            return;
        };
        let text = files
            .iter()
            .find(|(p, _)| *p == path)
            .and_then(|(_, a)| a.notes.clone())
            .unwrap_or_default();
        self.notes = Some(NotesDraft::new(path, text));
    }

    /// Compares a brought-back draft with the database once its save in flight is answered
    /// and a read of the file, started after the selection changed, is in.
    fn check_restored_note(&mut self) {
        if let Some(relinked) = &self.relinked {
            if relinked.try_recv() == Err(crossbeam_channel::TryRecvError::Empty) {
                return;
            }
            // The rows followed the move (a collision merges notes): read them again.
            self.relinked = None;
            if let Some(cache) = self.cache.as_mut() {
                cache.files = None;
            }
            self.generation += 1;
            self.error = None;
            return;
        }
        let Some(draft) = self.notes.as_mut().filter(|d| d.check_db) else {
            return;
        };
        if draft.saving.is_some() {
            return;
        }
        let Some(files) = self
            .cache
            .as_ref()
            .filter(|c| c.paths.len() == 1 && c.paths[0] == draft.path)
            .and_then(|c| c.files.as_ref())
        else {
            return;
        };
        let note = files
            .iter()
            .find(|(p, _)| *p == draft.path)
            .and_then(|(_, a)| a.notes.clone())
            .unwrap_or_default();
        draft.check_against(note);
    }

    /// Files ZXraw moved or renamed: the note being edited, and the selection it belongs to,
    /// follow their file, so an edit not saved yet is saved at the new path. A move onto an
    /// annotated file merges the two notes, so the moved draft is compared with the database
    /// again once the rows have followed (`relinked` is answered), and is not saved before.
    pub fn follow_moves(
        &mut self,
        pairs: &[(PathBuf, PathBuf)],
        relinked: crossbeam_channel::Receiver<()>,
    ) {
        use crate::library::moved_by;
        self.relinked = Some(relinked);
        if let Some(draft) = self.notes.as_mut()
            && let Some(to) = moved_by(&draft.path, pairs)
        {
            draft.path = to;
            draft.check_db = true;
        }
        let moved: Vec<(PathBuf, PathBuf)> = self
            .left_behind
            .keys()
            .filter_map(|path| moved_by(path, pairs).map(|to| (path.clone(), to)))
            .collect();
        let taken: Vec<NotesDraft> = moved
            .into_iter()
            .filter_map(|(from, to)| {
                let mut draft = self.left_behind.remove(&from)?;
                draft.path = to;
                Some(draft)
            })
            .collect();
        for draft in taken {
            self.left_behind.insert(draft.path.clone(), draft);
        }
        if let Some(cache) = self.cache.as_mut() {
            for path in cache.paths.iter_mut() {
                if let Some(to) = moved_by(path, pairs) {
                    *path = to;
                }
            }
            for (path, _) in cache.files.iter_mut().flatten() {
                if let Some(to) = moved_by(path, pairs) {
                    *path = to;
                }
            }
        }
    }

    /// Picks up the answers to saves in flight, for the open draft and the drafts left behind:
    /// `saved` moves only once the database confirms the write. A left-behind draft is dropped
    /// once confirmed and kept when its save failed.
    fn poll_note_save(&mut self, ctx: &egui::Context) {
        if let Some(draft) = self.notes.as_mut() {
            poll_draft(draft, ctx);
        }
        self.left_behind.retain(|path, draft| {
            let was_saving = draft.saving.is_some();
            poll_draft(draft, ctx);
            if was_saving && let (None, Some(error)) = (&draft.saving, &draft.error) {
                tracing::warn!(path = %path.display(), "a note could not be saved: {error}");
            }
            unconfirmed_note(draft).is_some()
        });
    }

    /// Notes of files no longer selected that could not be saved, for the panel to report.
    fn failed_left_behind(&self) -> Vec<(&Path, &str)> {
        let mut failed: Vec<(&Path, &str)> = self
            .left_behind
            .values()
            .filter(|d| d.saving.is_none())
            .filter_map(|d| d.error.as_deref().map(|e| (d.path.as_path(), e)))
            .collect();
        failed.sort();
        failed
    }

    /// Every note the database has not confirmed, the open draft last so it wins for its path.
    fn unconfirmed_notes(&self) -> Vec<crate::library::PendingNote> {
        let mut left: Vec<&NotesDraft> = self.left_behind.values().collect();
        left.sort_by(|a, b| a.path.cmp(&b.path));
        left.into_iter()
            .chain(self.notes.as_ref())
            .filter_map(unconfirmed_note)
            .collect()
    }

    fn save_notes(&mut self, library: &mut Library, ctx: &egui::Context) {
        let Some(draft) = self.notes.as_mut() else {
            return;
        };
        if !note_dirty(draft) {
            return;
        }
        let path = draft.path.clone();
        let trimmed = draft.text.trim().to_string();
        let text = draft.text.clone();
        draft.changed_at = None;
        draft.error = None;
        let notes = (!trimmed.is_empty()).then_some(trimmed);
        if let Some((_, annotation)) = self
            .cache
            .as_mut()
            .and_then(|c| c.files.as_mut())
            .and_then(|files| files.iter_mut().find(|(p, _)| *p == path))
        {
            annotation.notes = notes.clone();
        }
        let result = library.annotate_confirmed(
            vec![path],
            AnnotationChange {
                notes: Some(notes),
                ..Default::default()
            },
        );
        if let Some(draft) = self.notes.as_mut() {
            draft.saving = Some(PendingSave { text, result });
        }
        self.edited(ctx);
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn change_tags(
        &mut self,
        library: &mut Library,
        ctx: &egui::Context,
        add: Vec<String>,
        remove: Vec<String>,
    ) {
        let Some(cache) = self.cache.as_mut() else {
            return;
        };
        if let Some(files) = cache.files.as_mut() {
            apply_tag_change(files, &add, &remove);
        }
        library.annotate(
            cache.paths.clone(),
            AnnotationChange {
                add_tags: add,
                remove_tags: remove,
                ..Default::default()
            },
        );
        self.edited(ctx);
    }

    /// Tags common to the cached selection and tags on only some of it.
    fn summary(&self) -> Option<(Vec<String>, Vec<String>)> {
        let files = self.cache.as_ref()?.files.as_ref()?;
        let tags: Vec<&[String]> = files.iter().map(|(_, a)| a.tags.as_slice()).collect();
        Some(summarize_tags(&tags))
    }

    fn run_manage(&mut self, library: &Library, ctx: &egui::Context, op: ManageOp) {
        self.manage.busy = true;
        let wait_for_write: Box<dyn FnOnce() -> Result<String, String> + Send> = match op {
            ManageOp::Rename { old, new } => {
                let reply = library.rename_tag(old.clone(), new.clone());
                Box::new(move || {
                    reply
                        .recv()
                        .unwrap_or_else(|_| {
                            Err("Library writer stopped before saving the tag".into())
                        })
                        .map(|()| format!("Renamed '{old}' to '{new}'"))
                })
            }
            ManageOp::Delete { name } => {
                let reply = library.delete_tag(name.clone());
                Box::new(move || {
                    reply
                        .recv()
                        .unwrap_or_else(|_| {
                            Err("Library writer stopped before saving the tag".into())
                        })
                        .map(|deleted| {
                            if deleted {
                                format!("Deleted '{name}'")
                            } else {
                                format!("'{name}' was already gone")
                            }
                        })
                })
            }
        };
        let tx = self.tx.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = wait_for_write();
            let _ = tx.send(Msg::Manage(result));
            repaint.request_repaint();
        });
    }
}

/// Drawn in the preview panel below the file info for any file.
/// Runs every frame, drawn or not: picks up save answers, and saves a note left unsaved when
/// the panel stopped being drawn (closed, collapsed, or another view took its place).
pub fn tick(app: &mut ZxrawApp) {
    let ctx = app.ops.ctx.clone();
    let st = &mut app.ops.tags_ui;
    st.poll_note_save(&ctx);
    let hidden = ctx.cumulative_frame_nr() > st.panel_frame + 1;
    if hidden && st.notes.as_ref().is_some_and(note_dirty) {
        st.save_notes(&mut app.ops.library, &ctx);
    }
}

/// How long exit waits for the library writer: longer than the database's 5 s busy timeout,
/// so a write that is waiting on a lock still gets its answer.
const EXIT_FLUSH_WAIT: Duration = Duration::from_secs(7);

/// Sends the note being edited to the library now (used before ZXraw moves files in a job, so
/// the write is queued while the file is still at the path being edited).
pub fn save_note_now(app: &mut ZxrawApp) {
    let ctx = app.ops.ctx.clone();
    app.ops.tags_ui.save_notes(&mut app.ops.library, &ctx);
}

/// At exit: saves an unsaved note and waits for the library writer to confirm it and every
/// earlier save. Each note the database has not confirmed by then (the open draft and the
/// drafts left behind) is written to the recovery file and saved at the next start.
pub fn flush_for_exit(app: &mut ZxrawApp) {
    let ctx = app.ops.ctx.clone();
    let st = &mut app.ops.tags_ui;
    st.save_notes(&mut app.ops.library, &ctx);
    if !app.ops.library.flush(EXIT_FLUSH_WAIT) {
        tracing::warn!("the library writer did not finish before exit");
    }
    st.poll_note_save(&ctx);
    let pending = st.unconfirmed_notes();
    if pending.is_empty() {
        return;
    }
    let file = app.ops.library.notes_recovery_path().to_path_buf();
    match crate::library::write_note_recovery(&file, &pending) {
        Ok(()) => tracing::warn!(
            "{} unsaved note(s) kept in {}",
            pending.len(),
            file.display()
        ),
        Err(e) => tracing::error!("could not keep unsaved notes in {}: {e}", file.display()),
    }
}

/// The note to keep for the next start when the database has not confirmed the draft. A
/// draft in conflict keeps both texts, so the database's note is never overwritten unseen.
fn unconfirmed_note(draft: &NotesDraft) -> Option<crate::library::PendingNote> {
    (draft.text != draft.saved).then(|| {
        // Kept with the note it was based on: the next start merges rather than overwrites
        // when the database has changed since (a draft still being checked may be behind).
        let (text, base) = match &draft.conflict {
            Some(note) => (merged_note(&draft.text, note), note),
            None => (draft.text.trim().to_string(), &draft.saved),
        };
        crate::library::PendingNote {
            path: draft.path.clone(),
            note: Some(text).filter(|n| !n.is_empty()),
            base: Some(base.trim().to_string()),
        }
    })
}

/// The database's note changed since the unsaved edit was made: the user picks what to keep.
fn show_note_conflict(draft: &mut NotesDraft, ui: &mut Ui, now: Instant) {
    let Some(note) = &draft.conflict else {
        return;
    };
    let saved = if note.trim().is_empty() {
        "The saved note is now empty.".to_string()
    } else {
        format!("The saved note is now:\n{note}")
    };
    ui.label(
        RichText::new("This note changed since your unsaved edit.")
            .small()
            .color(ui.visuals().warn_fg_color),
    )
    .on_hover_text(saved);
    let mut choice = None;
    ui.horizontal_wrapped(|ui| {
        if ui
            .small_button("Keep saved version")
            .on_hover_text("Drop your edit and show the saved note.")
            .clicked()
        {
            choice = Some(ConflictChoice::KeepSaved);
        }
        if ui
            .small_button("Use my edit")
            .on_hover_text("Replace the saved note with your edit.")
            .clicked()
        {
            choice = Some(ConflictChoice::UseDraft);
        }
        if ui
            .small_button("Merge both")
            .on_hover_text("Keep your edit and the saved note, one after the other.")
            .clicked()
        {
            choice = Some(ConflictChoice::Merge);
        }
    });
    if let Some(choice) = choice {
        draft.resolve(choice, now);
    }
}

/// Notes of other files whose save failed: kept, and named so they can be selected and retried.
fn show_failed_left_behind(st: &TagsUi, ui: &mut Ui) {
    for (path, error) in st.failed_left_behind() {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        ui.label(
            RichText::new(format!("Note on {name} not saved"))
                .small()
                .color(ui.visuals().error_fg_color),
        )
        .on_hover_text(format!(
            "The note on {} could not be saved: {error}\nIt is kept until it is saved. Select the file to try again.",
            path.display()
        ));
    }
}

pub fn panel(app: &mut ZxrawApp, ui: &mut Ui, entry: &FileEntry) {
    let ctx = ui.ctx().clone();
    let selection = selection_paths(app.pane(), &entry.path);
    let db = app.ops.library.db.clone();
    let library = &mut app.ops.library;
    let st = &mut app.ops.tags_ui;
    st.poll();
    st.poll_note_save(&ctx);
    st.panel_frame = ctx.cumulative_frame_nr();

    ui.horizontal(|ui| {
        ui.label(RichText::new("Tags & notes").strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Manage tags…").clicked() {
                st.manage_open = true;
            }
        });
    });
    if db.is_none() {
        let text = match &library.open_error {
            Some(error) => format!("Library database is not available: {error}"),
            None => "Opening the library…".into(),
        };
        ui.label(RichText::new(text).weak());
        return;
    }

    st.sync_selection(&selection);
    st.drive_loads(db.as_ref(), &ctx);
    st.sync_notes(library, &ctx);

    let Some((common, mixed)) = st.summary() else {
        let retry = ui
            .horizontal(|ui| {
                if let Some(error) = &st.error {
                    ui.label(
                        RichText::new(format!("Could not read tags: {error}"))
                            .color(ui.visuals().error_fg_color),
                    );
                    ui.small_button("Retry").clicked()
                } else {
                    ui.spinner();
                    ui.label(RichText::new("Reading tags…").weak());
                    false
                }
            })
            .inner;
        if retry {
            st.reload_at = Some(Instant::now());
        }
        return;
    };

    let n = selection.len();
    ui.label(
        RichText::new(if n == 1 {
            entry.name.clone()
        } else {
            format!("{n} files selected")
        })
        .weak()
        .small(),
    );

    let mut add: Vec<String> = Vec::new();
    let mut remove: Vec<String> = Vec::new();
    let remove_hint = if n == 1 {
        "Remove this tag".to_string()
    } else {
        format!("Remove from all {n} selected files")
    };
    ui.horizontal_wrapped(|ui| {
        for tag in &common {
            ui.label(tag);
            if ui.small_button("×").on_hover_text(&remove_hint).clicked() {
                remove.push(tag.clone());
            }
        }
        for tag in &mixed {
            let add_button = ui
                .small_button(RichText::new(format!("{tag} +")).italics())
                .on_hover_text("On some of the selected files. Click to add it to all of them.");
            if add_button.clicked() {
                add.push(tag.clone());
            }
            if ui
                .small_button("×")
                .on_hover_text("Remove from all selected files")
                .clicked()
            {
                remove.push(tag.clone());
            }
        }
    });
    if common.is_empty() && mixed.is_empty() {
        ui.label(RichText::new("No tags yet").weak());
    }

    let input_id = egui::Id::new("tags-input");
    let focused = ui.memory(|m| m.has_focus(input_id));
    let suggestions: Vec<TagCount> = if focused || !st.input.trim().is_empty() {
        rank_suggestions(
            &st.input,
            st.all_tags.as_deref().unwrap_or_default(),
            &common,
            MAX_SUGGESTIONS,
        )
        .into_iter()
        .cloned()
        .collect()
    } else {
        Vec::new()
    };
    let (mut down, mut up, mut tab) = (false, false, false);
    if focused && !suggestions.is_empty() {
        ui.input_mut(|i| {
            down = i.consume_key(Modifiers::NONE, Key::ArrowDown);
            up = i.consume_key(Modifiers::NONE, Key::ArrowUp);
            tab = i.consume_key(Modifiers::NONE, Key::Tab);
        });
    }
    if down {
        st.highlight = Some(
            st.highlight
                .map_or(0, |i| (i + 1).min(suggestions.len() - 1)),
        );
    }
    if up {
        st.highlight = st.highlight.and_then(|i| i.checked_sub(1));
    }
    if tab {
        let pick = st.highlight.unwrap_or(0).min(suggestions.len() - 1);
        st.input = suggestions[pick].name.clone();
        st.highlight = None;
    }

    let response = ui.add(
        egui::TextEdit::singleline(&mut st.input)
            .id(input_id)
            .hint_text("Add tags, comma-separated")
            .desired_width(f32::INFINITY),
    );
    if response.changed() {
        st.highlight = None;
    }
    if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
        match st.highlight.and_then(|i| suggestions.get(i)) {
            Some(suggestion) => add.push(suggestion.name.clone()),
            None => add.extend(parse_tags(&st.input)),
        }
        st.input.clear();
        st.highlight = None;
        response.request_focus();
    }
    if focused {
        ui.label(
            RichText::new("Enter adds to all selected files. Up/Down chooses, Tab completes.")
                .weak()
                .small(),
        );
    }
    for (index, suggestion) in suggestions.iter().enumerate() {
        let picked = st.highlight == Some(index);
        let label = format!(
            "{}  ·  {} file{}",
            suggestion.name,
            suggestion.files,
            plural(suggestion.files)
        );
        if ui.selectable_label(picked, label).clicked() {
            add.push(suggestion.name.clone());
            st.input.clear();
            st.highlight = None;
        }
    }

    ui.add_space(4.0);
    if n == 1 {
        let now = Instant::now();
        let status = st.notes.as_ref().map_or(NoteStatus::Empty, note_status);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Note").weak());
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| match status {
                    NoteStatus::Unsaved => {
                        ui.label(
                            RichText::new("Unsaved changes").color(ui.visuals().warn_fg_color),
                        );
                    }
                    NoteStatus::Saving => {
                        ui.label(RichText::new("Saving…").weak());
                    }
                    NoteStatus::Saved => {
                        ui.label(RichText::new("Saved ✓").weak());
                    }
                    NoteStatus::Failed(error) => {
                        ui.label(RichText::new("Not saved").color(ui.visuals().error_fg_color))
                            .on_hover_text(format!(
                                "The note could not be saved: {error}
Edit it or leave the field to try again."
                            ));
                    }
                    NoteStatus::Checking => {
                        ui.label(RichText::new("Checking…").weak());
                    }
                    NoteStatus::Conflict => {
                        ui.label(
                            RichText::new("Changed elsewhere").color(ui.visuals().warn_fg_color),
                        );
                    }
                    NoteStatus::Empty => {}
                },
            );
        });
        show_failed_left_behind(st, ui);
        if let Some(draft) = st.notes.as_mut() {
            show_note_conflict(draft, ui, now);
        }
        let mut lost_focus = false;
        match st.notes.as_mut() {
            Some(draft) => {
                let row_height = ui.text_style_height(&egui::TextStyle::Body);
                let max_height = row_height * NOTE_MAX_ROWS + 8.0;
                egui::ScrollArea::vertical()
                    .id_salt("note-text")
                    .max_height(max_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let response = ui.add(
                            egui::TextEdit::multiline(&mut draft.text)
                                .hint_text("Add a note — saved automatically")
                                .desired_rows(3)
                                .desired_width(f32::INFINITY),
                        );
                        if response.changed() {
                            draft.changed_at = Some(now);
                            draft.error = None;
                        }
                        lost_focus = response.lost_focus();
                    });
            }
            None => {
                ui.add_enabled(
                    false,
                    egui::TextEdit::multiline(&mut String::new())
                        .hint_text("Loading…")
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
            }
        }
        let due = st.notes.as_ref().is_some_and(|d| autosave_due(d, now));
        if lost_focus || due {
            st.save_notes(library, &ctx);
        } else if let Some(draft) = st
            .notes
            .as_ref()
            .filter(|d| note_dirty(d) && d.error.is_none())
            && let Some(at) = draft.changed_at
        {
            // Wake up when the pause ends, so the save happens without more input.
            let left = NOTE_AUTOSAVE_AFTER.saturating_sub(now.saturating_duration_since(at));
            ctx.request_repaint_after(left);
        }
    } else {
        ui.label(RichText::new("Note").weak());
        show_failed_left_behind(st, ui);
        ui.label(
            RichText::new("Select one file to edit its notes.")
                .weak()
                .small(),
        );
    }

    if !add.is_empty() || !remove.is_empty() {
        st.change_tags(library, &ctx, add, remove);
    }
}

/// "Manage tags" window: rename (a rename onto an existing tag merges them) and delete, with the
/// number of files that will change. Opened from the panel; drawn each frame by the app shell.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    let library = &app.ops.library;
    let db = library.db.clone();
    let st = &mut app.ops.tags_ui;
    if !st.manage_open {
        return;
    }
    st.poll();
    st.drive_loads(db.as_ref(), ctx);

    let tags = st.all_tags.clone();
    let mut open = true;
    let mut action: Option<ManageAction> = None;
    egui::Window::new("Manage tags")
        .open(&mut open)
        .default_size([460.0, 520.0])
        .resizable(true)
        .show(ctx, |ui| {
            let Some(tags) = tags.as_ref() else {
                ui.spinner();
                return;
            };
            ui.horizontal(|ui| {
                ui.label(format!("{} tags", tags.len()));
                ui.add(
                    egui::TextEdit::singleline(&mut st.manage.filter)
                        .hint_text("Filter")
                        .desired_width(f32::INFINITY),
                );
            });
            if st.manage.busy {
                ui.spinner();
            }
            match &st.manage.notice {
                Some(Ok(text)) => ui.label(RichText::new(text).weak()),
                Some(Err(error)) => {
                    ui.label(RichText::new(error).color(ui.visuals().error_fg_color))
                }
                None => ui.label(""),
            };
            ui.separator();

            let filter = st.manage.filter.trim().to_lowercase();
            let busy = st.manage.busy;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for tag in tags
                        .iter()
                        .filter(|t| filter.is_empty() || t.name.to_lowercase().contains(&filter))
                    {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&tag.name).strong());
                            ui.label(
                                RichText::new(format!("{} file{}", tag.files, plural(tag.files)))
                                    .weak()
                                    .small(),
                            );
                            if let Some(renaming) =
                                st.manage.renaming.as_mut().filter(|r| r.old == tag.name)
                            {
                                let edit = ui.add(
                                    egui::TextEdit::singleline(&mut renaming.new)
                                        .desired_width(150.0),
                                );
                                let target = renaming.new.trim().to_string();
                                let merges = tags.iter().any(|t| {
                                    t.name.eq_ignore_ascii_case(&target) && t.name != tag.name
                                });
                                if merges {
                                    ui.label(RichText::new("merges into existing").weak().small());
                                }
                                let valid = valid_tag_name(&target) && target != tag.name;
                                let enter =
                                    edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                                let apply = ui
                                    .add_enabled(!busy && valid, egui::Button::new("Apply").small())
                                    .clicked();
                                if (apply || enter) && !busy && valid {
                                    action = Some(ManageAction::Rename {
                                        old: tag.name.clone(),
                                        new: target,
                                    });
                                }
                                if ui.small_button("Cancel").clicked() {
                                    action = Some(ManageAction::CancelRename);
                                }
                            } else if st.manage.confirm_delete.as_deref() == Some(tag.name.as_str())
                            {
                                ui.label(
                                    RichText::new(format!(
                                        "Delete from {} file{}?",
                                        tag.files,
                                        plural(tag.files)
                                    ))
                                    .color(ui.visuals().warn_fg_color),
                                );
                                if ui
                                    .add_enabled(!busy, egui::Button::new("Delete").small())
                                    .clicked()
                                {
                                    action = Some(ManageAction::Delete(tag.name.clone()));
                                }
                                if ui.small_button("Cancel").clicked() {
                                    action = Some(ManageAction::CancelDelete);
                                }
                            } else {
                                if ui
                                    .add_enabled(!busy, egui::Button::new("Rename…").small())
                                    .clicked()
                                {
                                    action = Some(ManageAction::StartRename(tag.name.clone()));
                                }
                                if ui
                                    .add_enabled(!busy, egui::Button::new("Delete…").small())
                                    .clicked()
                                {
                                    action = Some(ManageAction::AskDelete(tag.name.clone()));
                                }
                            }
                        });
                    }
                });
        });
    st.manage_open = open;

    match action {
        Some(ManageAction::StartRename(name)) => {
            st.manage.renaming = Some(Renaming {
                old: name.clone(),
                new: name,
            });
        }
        Some(ManageAction::CancelRename) => st.manage.renaming = None,
        Some(ManageAction::Rename { old, new }) => {
            st.manage.renaming = None;
            st.run_manage(library, ctx, ManageOp::Rename { old, new });
        }
        Some(ManageAction::AskDelete(name)) => st.manage.confirm_delete = Some(name),
        Some(ManageAction::CancelDelete) => st.manage.confirm_delete = None,
        Some(ManageAction::Delete(name)) => {
            st.manage.confirm_delete = None;
            st.run_manage(library, ctx, ManageOp::Delete { name });
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(name: &str, files: u64) -> TagCount {
        TagCount {
            name: name.into(),
            files,
        }
    }

    fn names(v: &[&TagCount]) -> Vec<String> {
        v.iter().map(|t| t.name.clone()).collect()
    }

    #[test]
    fn suggestions_rank_prefix_then_substring_then_fuzzy() {
        let tags = vec![
            tag("Portrait", 9),
            tag("wedding", 2),
            tag("bridal-wedding", 50),
            tag("waterfall", 1),
            tag("wide-dinner-garden", 3),
        ];
        // Prefix matches first (most used first), then substring matches, then letters-in-order.
        assert_eq!(
            names(&rank_suggestions("w", &tags, &[], 10)),
            vec![
                "wide-dinner-garden",
                "wedding",
                "waterfall",
                "bridal-wedding"
            ]
        );
        // "wide-dinner-garden" only matches loosely (w…e…d), so it comes last.
        assert_eq!(
            names(&rank_suggestions("wed", &tags, &[], 10)),
            vec!["wedding", "bridal-wedding", "wide-dinner-garden"]
        );
        assert_eq!(
            names(&rank_suggestions("pt", &tags, &[], 10)),
            vec!["Portrait"]
        );
    }

    #[test]
    fn suggestions_skip_excluded_and_honour_limit() {
        let tags = vec![tag("Family", 4), tag("fashion", 7), tag("food", 1)];
        let excluded = vec!["FASHION".to_string()];
        assert_eq!(
            names(&rank_suggestions("f", &tags, &excluded, 10)),
            vec!["Family", "food"]
        );
        assert_eq!(
            names(&rank_suggestions("f", &tags, &[], 1)),
            vec!["fashion"]
        );
    }

    #[test]
    fn empty_query_lists_most_used_tags() {
        let tags = vec![tag("a", 1), tag("b", 5), tag("c", 3)];
        assert_eq!(
            names(&rank_suggestions("  ", &tags, &[], 2)),
            vec!["b", "c"]
        );
    }

    #[test]
    fn mixed_tags_for_multi_selection() {
        let a = vec!["Wedding".to_string(), "smith".to_string()];
        let b = vec!["wedding".to_string(), "Portrait".to_string()];
        let c = vec!["Wedding".to_string()];
        let (common, mixed) = summarize_tags(&[&a, &b, &c]);
        assert_eq!(common, vec!["Wedding".to_string()]);
        assert_eq!(mixed, vec!["Portrait".to_string(), "smith".to_string()]);
    }

    #[test]
    fn mixed_tags_for_single_and_empty_selection() {
        let a = vec!["x".to_string()];
        assert_eq!(summarize_tags(&[&a]), (vec!["x".to_string()], vec![]));
        assert_eq!(summarize_tags(&[]), (vec![], vec![]));
    }

    #[test]
    fn local_change_matches_database_order() {
        let mut files = vec![
            (
                PathBuf::from("a"),
                Annotation {
                    tags: vec!["keep".into()],
                    ..Default::default()
                },
            ),
            (PathBuf::from("b"), Annotation::default()),
        ];
        // Adds land first, then removals (case-insensitive), so "New" never survives.
        apply_tag_change(&mut files, &["New".into(), "keep".into()], &["NEW".into()]);
        assert_eq!(files[0].1.tags, vec!["keep".to_string()]);
        assert_eq!(files[1].1.tags, vec!["keep".to_string()]);
        apply_tag_change(&mut files, &["zeta".into(), "alpha".into()], &[]);
        assert_eq!(
            files[0].1.tags,
            vec!["alpha".to_string(), "keep".to_string(), "zeta".to_string()]
        );
    }

    fn draft(text: &str, saved: &str, changed_at: Option<Instant>) -> NotesDraft {
        NotesDraft {
            path: PathBuf::from("a.jpg"),
            text: text.into(),
            saved: saved.into(),
            changed_at,
            saving: None,
            error: None,
            check_db: false,
            conflict: None,
        }
    }

    fn pending(text: &str) -> (PendingSave, crossbeam_channel::Sender<Result<(), String>>) {
        let (tx, result) = crossbeam_channel::bounded(1);
        (
            PendingSave {
                text: text.into(),
                result,
            },
            tx,
        )
    }

    /// Takes the relink answer, then stands in for the panel's read of the selection.
    fn reread(st: &mut TagsUi, library: &mut Library, ctx: &egui::Context, db: &Db) {
        st.sync_notes(library, ctx);
        let paths = st.cache.as_ref().unwrap().paths.clone();
        st.cache.as_mut().unwrap().files = Some(read_files(db, &paths).unwrap());
        st.sync_notes(library, ctx);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zxraw-tags-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_note_typed_then_renamed_at_once_lands_on_the_new_path() {
        let db = std::sync::Arc::new(forge_db::Db::open_in_memory().unwrap());
        let dir = temp_dir("rename-draft");
        let (old, new) = (dir.join("a.jpg"), dir.join("final.jpg"));
        std::fs::write(&old, b"photo").unwrap();
        let mut library = crate::library::test_library(db.clone());
        let ctx = egui::Context::default();
        let mut st = TagsUi::default();
        st.cache = Some(Selected {
            paths: vec![old.clone()],
            files: Some(vec![(old.clone(), Annotation::default())]),
        });
        // Typed, but the autosave pause has not passed yet.
        st.notes = Some(NotesDraft {
            path: old.clone(),
            ..draft("Café proof 🎉", "", Some(Instant::now()))
        });

        // Renamed in the app right away: the rename and the follow happen in the same frame.
        std::fs::rename(&old, &new).unwrap();
        let pairs = vec![(old.clone(), new.clone())];
        let relinked = library.follow_moves(pairs.clone());
        st.follow_moves(&pairs, relinked);
        assert_eq!(st.notes.as_ref().unwrap().path, new);

        // The selection now shows the renamed file: the draft is kept, then saved once it is
        // compared with the database after the relink.
        st.sync_selection(std::slice::from_ref(&new));
        st.sync_notes(&mut library, &ctx);
        assert_eq!(st.notes.as_ref().unwrap().text, "Café proof 🎉");
        assert!(library.flush(Duration::from_secs(5)));
        reread(&mut st, &mut library, &ctx, &db);
        st.save_notes(&mut library, &ctx);
        assert!(library.flush(Duration::from_secs(5)));
        st.poll_note_save(&ctx);
        assert_eq!(note_status(st.notes.as_ref().unwrap()), NoteStatus::Saved);

        let id = db.file_id(&new).unwrap().expect("a row at the new path");
        assert_eq!(db.annotation(id).unwrap().notes.as_deref(), Some("Café proof 🎉"));
        assert!(db.file_id(&old).unwrap().is_none());
        drop(library);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_save_after_switching_files_keeps_the_draft_for_recovery() {
        let mut st = TagsUi::default();
        let ctx = egui::Context::default();
        let (a, b) = (PathBuf::from("a.jpg"), PathBuf::from("b.jpg"));
        let (save, answer) = pending("client proof");
        st.notes = Some(NotesDraft {
            saving: Some(save),
            ..draft("client proof", "", None)
        });
        // The selection moves on while the save is in flight.
        let mut library = crate::library::test_library(std::sync::Arc::new(
            forge_db::Db::open_in_memory().unwrap(),
        ));
        st.cache = Some(Selected {
            paths: vec![b.clone()],
            files: Some(vec![(b.clone(), Annotation::default())]),
        });
        st.sync_notes(&mut library, &ctx);
        assert_eq!(st.notes.as_ref().unwrap().path, b);
        assert!(st.left_behind.contains_key(&a));

        answer.send(Err("disk full".into())).unwrap();
        st.poll_note_save(&ctx);
        assert_eq!(st.failed_left_behind(), vec![(a.as_path(), "disk full")]);
        // It goes into the recovery file at exit, alongside the open draft.
        st.notes.as_mut().unwrap().text = "second".into();
        let kept = st.unconfirmed_notes();
        assert_eq!(
            kept,
            vec![
                crate::library::PendingNote {
                    path: a.clone(),
                    note: Some("client proof".into()),
                    base: Some(String::new()),
                },
                crate::library::PendingNote {
                    path: b.clone(),
                    note: Some("second".into()),
                    base: Some(String::new()),
                },
            ]
        );
        // Selecting the file again brings the unsaved text back, marked as not saved.
        st.notes.as_mut().unwrap().text.clear();
        st.cache = Some(Selected {
            paths: vec![a.clone()],
            files: Some(vec![(a.clone(), Annotation::default())]),
        });
        st.sync_notes(&mut library, &ctx);
        let d = st.notes.as_ref().unwrap();
        assert_eq!(d.text, "client proof");
        assert_eq!(note_status(d), NoteStatus::Failed("disk full".into()));
        assert!(st.left_behind.is_empty());
    }

    #[test]
    fn a_brought_back_draft_never_overwrites_a_note_that_changed_meanwhile() {
        let mut st = TagsUi::default();
        let ctx = egui::Context::default();
        let mut library = crate::library::test_library(std::sync::Arc::new(
            forge_db::Db::open_in_memory().unwrap(),
        ));
        let (a, b) = (PathBuf::from("a.jpg"), PathBuf::from("b.jpg"));
        let failed = || NotesDraft {
            path: a.clone(),
            error: Some("disk full".into()),
            ..draft("my edit", "old", None)
        };
        let mut select = |st: &mut TagsUi, path: &PathBuf, note: Option<&str>| {
            st.sync_selection(std::slice::from_ref(path));
            st.sync_notes(&mut library, &ctx);
            // Until the read of the newly selected file is in, a brought-back draft waits.
            if let Some(d) = st.notes.as_ref().filter(|d| d.path == *path && d.check_db) {
                assert_eq!(note_status(d), NoteStatus::Checking);
                assert!(!note_dirty(d));
            }
            let annotation = Annotation { notes: note.map(str::to_string), ..Default::default() };
            st.cache.as_mut().unwrap().files = Some(vec![(path.clone(), annotation)]);
            st.sync_notes(&mut library, &ctx);
        };

        // The note was merged by a move while the edit waited: a conflict, nothing is saved.
        st.left_behind.insert(a.clone(), failed());
        select(&mut st, &a, Some("merged"));
        let d = st.notes.as_mut().unwrap();
        assert_eq!(note_status(d), NoteStatus::Conflict);
        assert_eq!(d.text, "my edit");
        assert!(!note_dirty(d), "a conflict is never saved on its own");
        assert_eq!(
            unconfirmed_note(d).unwrap().note.as_deref(),
            Some("my edit\n\n--- merged note ---\n\nmerged"),
            "kept at exit with both texts"
        );
        let now = Instant::now();
        d.resolve(ConflictChoice::KeepSaved, now);
        assert_eq!((d.text.as_str(), note_status(d)), ("merged", NoteStatus::Saved));

        let mut d = failed();
        d.conflict = Some("merged".into());
        d.resolve(ConflictChoice::UseDraft, now);
        assert_eq!(d.text, "my edit");
        assert!(note_dirty(&d) && d.changed_at == Some(now), "saved after the pause");

        let mut d = failed();
        d.conflict = Some("merged".into());
        d.resolve(ConflictChoice::Merge, now);
        assert_eq!(d.text, "my edit\n\n--- merged note ---\n\nmerged");
        assert_eq!(d.saved, "merged");
        assert!(note_dirty(&d));

        // Unchanged in the database: the draft is offered for saving again.
        st.notes = None;
        select(&mut st, &b, None);
        st.left_behind.insert(a.clone(), failed());
        select(&mut st, &a, Some("old"));
        let d = st.notes.as_ref().unwrap();
        assert_eq!(note_status(d), NoteStatus::Failed("disk full".into()));
        assert!(note_dirty(d));
    }

    #[test]
    fn a_failed_draft_moved_onto_an_annotated_file_is_checked_before_it_is_saved() {
        let db = std::sync::Arc::new(forge_db::Db::open_in_memory().unwrap());
        let dir = temp_dir("move-onto-note");
        let (old, dest) = (dir.join("a.jpg"), dir.join("dest").join("a.jpg"));
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&old, b"photo").unwrap();
        std::fs::write(&dest, b"other").unwrap();
        let entry = forge_core::listing::entry_for_path(&dest).unwrap();
        db.upsert_entries(&[entry]).unwrap();
        let dest_id = db.file_id(&dest).unwrap().unwrap();
        db.set_notes(&[dest_id], Some("theirs")).unwrap();
        let mut library = crate::library::test_library(db.clone());
        let ctx = egui::Context::default();
        let mut st = TagsUi::default();
        st.cache = Some(Selected {
            paths: vec![old.clone()],
            files: Some(vec![(old.clone(), Annotation::default())]),
        });
        // The save failed; the file is then moved onto an annotated one.
        st.notes = Some(NotesDraft {
            path: old.clone(),
            error: Some("disk full".into()),
            ..draft("my edit", "", None)
        });
        std::fs::remove_file(&dest).unwrap();
        std::fs::rename(&old, &dest).unwrap();
        let pairs = vec![(old.clone(), dest.clone())];
        let relinked = library.follow_moves(pairs.clone());
        st.follow_moves(&pairs, relinked);

        // A retry before the database is read again saves nothing.
        st.save_notes(&mut library, &ctx);
        assert!(st.notes.as_ref().unwrap().saving.is_none());
        assert!(library.flush(Duration::from_secs(5)));
        reread(&mut st, &mut library, &ctx, &db);
        let d = st.notes.as_ref().unwrap();
        assert_eq!(note_status(d), NoteStatus::Conflict);
        assert_eq!(d.conflict.as_deref(), Some("theirs"));
        st.save_notes(&mut library, &ctx);
        assert!(library.flush(Duration::from_secs(5)));
        assert_eq!(db.annotation(dest_id).unwrap().notes.as_deref(), Some("theirs"));
        drop(library);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_recheck_replaces_a_stale_conflict_with_the_latest_note() {
        let mut d = draft("mine", "old", None);
        d.check_against("first".into());
        assert_eq!(d.conflict.as_deref(), Some("first"));
        // Changed again before the user chose: the latest note is what a merge must keep.
        d.check_db = true;
        d.check_against("second".into());
        assert_eq!(d.conflict.as_deref(), Some("second"));
        assert_eq!(
            unconfirmed_note(&d).unwrap(),
            crate::library::PendingNote {
                path: d.path.clone(),
                note: Some("mine\n\n--- merged note ---\n\nsecond".into()),
                base: Some("second".into()),
            }
        );
        // Back to the note the edit was based on: still a choice, against that note.
        d.check_against("old".into());
        assert_eq!(d.conflict.as_deref(), Some("old"));
        // The edit itself is in the database now: nothing left to choose.
        d.check_against("mine".into());
        assert_eq!(d.conflict, None);
        assert_eq!(note_status(&d), NoteStatus::Saved);
    }

    #[test]
    fn a_draft_still_being_checked_is_kept_with_the_note_it_was_based_on() {
        let mut d = draft("my edit", "old", None);
        d.check_db = true;
        assert_eq!(note_status(&d), NoteStatus::Checking);
        assert_eq!(
            unconfirmed_note(&d).unwrap().base.as_deref(),
            Some("old"),
            "the next start merges when the database has moved on from it"
        );
    }

    #[test]
    fn a_left_behind_draft_is_dropped_once_confirmed_and_follows_moves() {
        let mut st = TagsUi::default();
        let ctx = egui::Context::default();
        let (save, answer) = pending("x");
        st.left_behind.insert(
            PathBuf::from("a.jpg"),
            NotesDraft {
                saving: Some(save),
                ..draft("x", "", None)
            },
        );
        st.follow_moves(
            &[(PathBuf::from("a.jpg"), PathBuf::from("b.jpg"))],
            crossbeam_channel::bounded(1).1,
        );
        assert_eq!(
            st.left_behind[&PathBuf::from("b.jpg")].path,
            PathBuf::from("b.jpg")
        );
        st.poll_note_save(&ctx);
        assert_eq!(st.left_behind.len(), 1, "still in flight");
        answer.send(Ok(())).unwrap();
        st.poll_note_save(&ctx);
        assert!(st.left_behind.is_empty());
    }

    #[test]
    fn an_unconfirmed_note_is_kept_for_the_next_start() {
        let (save, _answer) = pending("late note");
        let d = NotesDraft {
            saving: Some(save),
            ..draft("late note", "", None)
        };
        assert_eq!(
            unconfirmed_note(&d),
            Some(crate::library::PendingNote {
                path: PathBuf::from("a.jpg"),
                note: Some("late note".into()),
                base: Some(String::new()),
            })
        );
        assert_eq!(unconfirmed_note(&draft("same", "same", None)), None);
        // A cleared note is kept as a clear.
        assert_eq!(unconfirmed_note(&draft("  ", "old", None)).unwrap().note, None);
    }

    #[test]
    fn saved_is_shown_only_after_the_database_confirms() {
        let mut st = TagsUi::default();
        let ctx = egui::Context::default();
        let (save, answer) = pending("client proof");
        st.notes = Some(NotesDraft {
            saving: Some(save),
            ..draft("client proof", "", None)
        });
        assert_eq!(note_status(st.notes.as_ref().unwrap()), NoteStatus::Saving);
        st.poll_note_save(&ctx);
        assert_eq!(note_status(st.notes.as_ref().unwrap()), NoteStatus::Saving);
        answer.send(Ok(())).unwrap();
        st.poll_note_save(&ctx);
        let d = st.notes.as_ref().unwrap();
        assert_eq!(d.saved, "client proof");
        assert_eq!(note_status(d), NoteStatus::Saved);
    }

    #[test]
    fn a_failed_save_shows_the_error_and_waits_for_the_next_edit() {
        let mut st = TagsUi::default();
        let ctx = egui::Context::default();
        let t0 = Instant::now();
        let (save, answer) = pending("x");
        st.notes = Some(NotesDraft {
            saving: Some(save),
            ..draft("x", "", Some(t0))
        });
        answer.send(Err("disk full".into())).unwrap();
        st.poll_note_save(&ctx);
        let d = st.notes.as_mut().unwrap();
        assert_eq!(d.saved, "");
        assert_eq!(note_status(d), NoteStatus::Failed("disk full".into()));
        assert!(!autosave_due(d, t0 + Duration::from_secs(5)));
        // An edit clears the error and autosave resumes after the pause.
        d.error = None;
        d.changed_at = Some(t0);
        assert!(autosave_due(d, t0 + NOTE_AUTOSAVE_AFTER));
    }

    #[test]
    fn a_dropped_save_reports_the_library_unavailable() {
        let mut st = TagsUi::default();
        let (save, answer) = pending("x");
        drop(answer);
        st.notes = Some(NotesDraft {
            saving: Some(save),
            ..draft("x", "", None)
        });
        st.poll_note_save(&egui::Context::default());
        assert!(matches!(
            note_status(st.notes.as_ref().unwrap()),
            NoteStatus::Failed(_)
        ));
    }

    #[test]
    fn autosave_skips_text_already_being_saved() {
        let t0 = Instant::now();
        let (save, _answer) = pending("abc");
        let mut d = NotesDraft {
            saving: Some(save),
            ..draft("abc", "", Some(t0))
        };
        assert!(!autosave_due(&d, t0 + Duration::from_secs(5)));
        // Typing during the save makes the newer text due on its own.
        d.text.push('d');
        assert!(autosave_due(&d, t0 + Duration::from_secs(5)));
    }

    #[test]
    fn note_status_tracks_unsaved_saved_and_empty() {
        assert_eq!(note_status(&draft("x", "", None)), NoteStatus::Unsaved);
        assert_eq!(note_status(&draft("", "x", None)), NoteStatus::Unsaved);
        assert_eq!(note_status(&draft("x", "x", None)), NoteStatus::Saved);
        assert_eq!(note_status(&draft("", "", None)), NoteStatus::Empty);
        // Whitespace-only text is in sync with a saved blank note, so it shows nothing.
        assert_eq!(note_status(&draft("  ", "  ", None)), NoteStatus::Empty);
    }

    #[test]
    fn autosave_waits_for_the_pause_then_fires() {
        let t0 = Instant::now();
        let d = draft("hello", "", Some(t0));
        assert!(!autosave_due(&d, t0));
        assert!(!autosave_due(&d, t0 + Duration::from_millis(799)));
        assert!(autosave_due(&d, t0 + NOTE_AUTOSAVE_AFTER));
        assert!(autosave_due(&d, t0 + Duration::from_secs(5)));
    }

    #[test]
    fn autosave_is_not_due_when_text_matches_saved() {
        let t0 = Instant::now();
        let synced = draft("hello", "hello", Some(t0));
        assert!(!autosave_due(&synced, t0 + Duration::from_secs(5)));
        let no_edit_yet = draft("hello", "", None);
        assert!(!autosave_due(&no_edit_yet, t0 + Duration::from_secs(5)));
    }

    #[test]
    fn autosave_is_due_again_after_a_further_edit() {
        let t0 = Instant::now();
        // First edit, saved after the pause.
        let mut d = draft("a", "", Some(t0));
        assert!(autosave_due(&d, t0 + NOTE_AUTOSAVE_AFTER));
        d.saved = d.text.clone();
        d.changed_at = None;
        assert!(!autosave_due(&d, t0 + Duration::from_secs(10)));
        // A later edit restarts the pause.
        let t1 = t0 + Duration::from_secs(10);
        d.text.push('b');
        d.changed_at = Some(t1);
        assert!(!autosave_due(&d, t1 + Duration::from_millis(100)));
        assert!(autosave_due(&d, t1 + NOTE_AUTOSAVE_AFTER));
    }

    #[test]
    fn typed_tags_split_on_commas_and_trim() {
        assert_eq!(parse_tags(" a , ,b,c "), vec!["a", "b", "c"]);
        assert!(parse_tags("  ,  ").is_empty());
    }
}
