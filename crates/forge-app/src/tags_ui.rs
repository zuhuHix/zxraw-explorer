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

/// Notes for the single selected file. Saved on Enter or when the field loses focus.
struct NotesDraft {
    path: PathBuf,
    text: String,
    saved: String,
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
            return;
        }
        self.save_notes(library, ctx);
        self.notes = None;
        let (Some(path), Some(cache)) = (want, self.cache.as_ref()) else {
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
        self.notes = Some(NotesDraft {
            path,
            saved: text.clone(),
            text,
        });
    }

    fn save_notes(&mut self, library: &mut Library, ctx: &egui::Context) {
        let Some(draft) = self.notes.as_mut() else {
            return;
        };
        if draft.text == draft.saved {
            return;
        }
        let path = draft.path.clone();
        let trimmed = draft.text.trim().to_string();
        draft.saved = draft.text.clone();
        let notes = (!trimmed.is_empty()).then_some(trimmed);
        if let Some((_, annotation)) = self
            .cache
            .as_mut()
            .and_then(|c| c.files.as_mut())
            .and_then(|files| files.iter_mut().find(|(p, _)| *p == path))
        {
            annotation.notes = notes.clone();
        }
        library.annotate(
            vec![path],
            AnnotationChange {
                notes: Some(notes),
                ..Default::default()
            },
        );
        self.edited(ctx);
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
pub fn panel(app: &mut ZxrawApp, ui: &mut Ui, entry: &FileEntry) {
    let ctx = ui.ctx().clone();
    let selection = selection_paths(app.pane(), &entry.path);
    let db = app.ops.library.db.clone();
    let library = &mut app.ops.library;
    let st = &mut app.ops.tags_ui;
    st.poll();

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
    ui.label(RichText::new("Notes").weak());
    if n == 1 {
        let lost_focus = match st.notes.as_mut() {
            Some(draft) => ui
                .add(
                    egui::TextEdit::singleline(&mut draft.text)
                        .hint_text("Saved on Enter or when you leave the field")
                        .desired_width(f32::INFINITY),
                )
                .lost_focus(),
            None => {
                ui.add_enabled(
                    false,
                    egui::TextEdit::singleline(&mut String::new()).hint_text("Loading…"),
                );
                false
            }
        };
        if lost_focus {
            st.save_notes(library, &ctx);
        }
    } else {
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

    #[test]
    fn typed_tags_split_on_commas_and_trim() {
        assert_eq!(parse_tags(" a , ,b,c "), vec!["a", "b", "c"]);
        assert!(parse_tags("  ,  ").is_empty());
    }
}
