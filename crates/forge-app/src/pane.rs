//! A browsing pane: current folder, history, streaming listing, filter, sort, selection.
//!
//! In details view, folders can be opened in place: an expanded folder's children are
//! listed (streamed, like the folder itself) into the same `entries`, and `visible` holds
//! the flattened tree, each level sorted, with `depth` giving the indent of every row.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use forge_core::{EntryKind, FileEntry};
use forge_core::listing::{self, SortKey, SortSpec};
use forge_core::query::Query;
use forge_jobs::CancelToken;

use crate::settings::ViewMode;

/// Messages from the background listing thread.
enum ListMsg {
    Batch(Vec<FileEntry>),
    Done { errors: usize },
    Failed(String),
}

/// Listing of a folder opened in place. Its messages carry `id`, so a superseded listing
/// (collapsed, refreshed or reopened since) is ignored.
struct ChildLoad {
    id: u64,
    cancel: CancelToken,
    loading: bool,
    /// A re-listing of an already listed folder collects here and replaces the old rows
    /// when complete, so a change inside an open folder does not make its rows flicker.
    staged: Option<Vec<FileEntry>>,
    /// Why the folder could not be listed (shown on its row).
    error: Option<String>,
}

/// Folders opened in place are listed by a small shared pool of workers, so opening many
/// folders never starts one thread each.
const SUBDIR_WORKERS: usize = 4;
/// Listing messages a pane may have waiting; a worker waits when its pane is this far behind.
const SUBDIR_BACKLOG: usize = 64;

type ChildMsg = (u64, PathBuf, ListMsg);

struct ListJob {
    id: u64,
    dir: PathBuf,
    cancel: CancelToken,
    tx: Sender<ChildMsg>,
}

/// Queue of the shared listing workers (started on first use).
fn subdir_queue() -> &'static Sender<ListJob> {
    static QUEUE: std::sync::OnceLock<Sender<ListJob>> = std::sync::OnceLock::new();
    QUEUE.get_or_init(|| {
        let (tx, rx) = unbounded::<ListJob>();
        for n in 0..SUBDIR_WORKERS {
            let rx = rx.clone();
            let _ = std::thread::Builder::new()
                .name(format!("list-subdir-{n}"))
                .spawn(move || {
                    for job in rx {
                        // A folder closed (or relisted) while its job waited is skipped.
                        if job.cancel.is_cancelled() {
                            continue;
                        }
                        let r = listing::list_dir_streaming(&job.dir, 2048, &job.cancel, |b| {
                            let _ = job.tx.send((job.id, job.dir.clone(), ListMsg::Batch(b)));
                        });
                        if job.cancel.is_cancelled() {
                            continue;
                        }
                        let msg = match r {
                            Ok(errors) => ListMsg::Done { errors },
                            Err(e) => ListMsg::Failed(e.to_string()),
                        };
                        let _ = job.tx.send((job.id, job.dir, msg));
                    }
                });
        }
        tx
    })
}

/// `paths` without the ones inside another of them: a selected folder already covers its
/// contents, so an operation must not get both (a copy would duplicate the child, a move or
/// delete would fail on it). Compared per component, ignoring case, as Windows does, so
/// `C:\a\b` covers `C:\a\b\c` but not `C:\a\bc`.
pub fn without_covered(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    fn key(path: &Path) -> Vec<String> {
        path.components()
            .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
            .collect()
    }
    let all: HashSet<Vec<String>> = paths.iter().map(|p| key(p)).collect();
    paths
        .into_iter()
        .filter(|p| {
            let k = key(p);
            !(1..k.len()).any(|n| all.contains(&k[..n]))
        })
        .collect()
}

pub enum LoadState {
    Loading { since: Instant },
    Ready { errors: usize },
    Failed(String),
}

pub struct Pane {
    pub path: PathBuf,
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    /// All entries of the folder, sorted by `sort`, together with the entries of the folders
    /// opened in place (details view). Use `visible` for display order.
    pub entries: Vec<FileEntry>,
    /// Indices into `entries` that pass the filter/hidden rules, in display order.
    pub visible: Vec<usize>,
    /// Tree depth of each `visible` row; empty when no folder is open (all rows at depth 0).
    pub depth: Vec<u16>,
    /// Folders opened in place, by path. Remembered for the tab across navigation; only
    /// the ones listed in the current folder (and inside open folders) are shown open.
    pub expanded: HashSet<PathBuf>,
    /// Listings of the open folders whose children are in `entries`.
    children: HashMap<PathBuf, ChildLoad>,
    child_tx: Sender<ChildMsg>,
    child_rx: Receiver<ChildMsg>,
    next_child_id: u64,
    /// While a refresh lists the folder again, its entries collect here (see `load_path`).
    staged: Option<Vec<FileEntry>>,
    /// The view `poll` last saw: leaving details view closes the tree, returning reopens it.
    tree_view: ViewMode,
    pub sort: SortSpec,
    pub view: ViewMode,
    pub filter_text: String,
    filter_query: Option<Query>,
    pub filter_error: Option<String>,
    pub selected: HashSet<PathBuf>,
    /// Index into `visible` of the keyboard cursor / range-selection anchor.
    pub cursor: Option<usize>,
    pub anchor: Option<usize>,
    pub load: LoadState,
    rx: Option<Receiver<ListMsg>>,
    cancel: CancelToken,
    dirty_sort: bool,
    last_sort: Instant,
    /// Set when the pane should scroll the cursor into view next frame.
    pub scroll_to_cursor: bool,
    /// Name to select once loading finishes (e.g. the folder we navigated up from).
    pending_select: Option<String>,
    /// Like `pending_select`, but only moves the cursor (refresh keeps the selection).
    pending_select_keep: Option<PathBuf>,
    pub editing_path: Option<String>,
    pub show_hidden: bool,
    pub show_system: bool,
    /// Optional burst / visual near-duplicate grouping for this folder.
    pub similar: crate::similar::Grouping,
    /// Changes whenever `entries` or `visible` change (see [`Pane::listing_rev`]).
    listing_rev: u64,
    /// Sticky selection: a plain click toggles an item in or out of the selection, as
    /// Ctrl+click does (`Cmd::ToggleStickySelection`; Esc turns it off).
    pub sticky: bool,
}

/// Whether typing in the file list filters it (Settings › Browsing). The app publishes the
/// setting each frame so the list views, which only see a pane, can read it.
pub fn publish_typing_filters(ctx: &egui::Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new("zxraw-typing-filters"), on));
}

pub fn typing_filters(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(egui::Id::new("zxraw-typing-filters")))
        .unwrap_or(false)
}

/// Source of listing revisions, shared by all panes so two panes never report the same one.
static LISTING_REV: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_listing_rev() -> u64 {
    LISTING_REV.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Pane {
    pub fn new(path: PathBuf, sort: SortSpec, view: ViewMode) -> Self {
        let (child_tx, child_rx) = bounded(SUBDIR_BACKLOG);
        let mut p = Self {
            path: PathBuf::new(),
            back: Vec::new(),
            forward: Vec::new(),
            entries: Vec::new(),
            visible: Vec::new(),
            depth: Vec::new(),
            expanded: HashSet::new(),
            children: HashMap::new(),
            child_tx,
            child_rx,
            next_child_id: 1,
            tree_view: view,
            staged: None,
            sort,
            view,
            filter_text: String::new(),
            filter_query: None,
            filter_error: None,
            selected: HashSet::new(),
            cursor: None,
            anchor: None,
            load: LoadState::Ready { errors: 0 },
            rx: None,
            cancel: CancelToken::new(),
            dirty_sort: false,
            last_sort: Instant::now(),
            scroll_to_cursor: false,
            pending_select: None,
            pending_select_keep: None,
            editing_path: None,
            show_hidden: false,
            show_system: false,
            similar: crate::similar::Grouping::default(),
            listing_rev: 0,
            sticky: false,
        };
        p.load_path(path);
        p
    }

    pub fn title(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.load, LoadState::Loading { .. })
    }

    /// Navigate to `path`, pushing the current folder onto the back stack.
    pub fn navigate(&mut self, path: PathBuf) {
        if path == self.path {
            return self.refresh();
        }
        if !self.path.as_os_str().is_empty() {
            self.back.push(self.path.clone());
            if self.back.len() > 200 {
                self.back.remove(0);
            }
        }
        self.forward.clear();
        self.load_path(path);
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }
    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }
    pub fn back(&mut self) {
        if let Some(p) = self.back.pop() {
            let came_from = self
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            self.forward.push(self.path.clone());
            self.load_path(p);
            self.pending_select = came_from;
        }
    }
    pub fn forward(&mut self) {
        if let Some(p) = self.forward.pop() {
            self.back.push(self.path.clone());
            self.load_path(p);
        }
    }
    pub fn up(&mut self) {
        if let Some(parent) = self.path.parent().map(Path::to_path_buf) {
            let came_from = self
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            self.navigate(parent);
            self.pending_select = came_from;
        }
    }

    /// Re-list the folder, keeping the selection and the cursor item (if it still exists).
    pub fn refresh(&mut self) {
        let keep: Vec<PathBuf> = self.selected.iter().cloned().collect();
        let cursor = self
            .cursor_entry()
            .map(|e| e.path.clone())
            .or_else(|| self.pending_select_keep.clone());
        let p = self.path.clone();
        self.load_path(p);
        self.selected.extend(keep);
        self.pending_select_keep = cursor;
    }

    /// Identifies the current listing: it changes whenever entries are added, removed or
    /// reordered, or the visible subset changes. Per-frame code caches derived data (for
    /// example over 50,000 entries) against it instead of walking the listing every frame.
    pub fn listing_rev(&self) -> u64 {
        self.listing_rev
    }

    fn listing_changed(&mut self) {
        self.listing_rev = next_listing_rev();
    }

    fn load_path(&mut self, path: PathBuf) {
        self.listing_changed();
        self.cancel.cancel();
        self.cancel = CancelToken::new();
        if path == self.path {
            self.similar.on_refresh();
        } else {
            self.similar.on_navigation();
        }
        if path == self.path && !path.as_os_str().is_empty() {
            // Refresh: the rows, selection and cursor stay while the folder (and every
            // folder open in place) is listed again; complete listings replace the rows.
            self.staged = Some(Vec::new());
            let open: Vec<PathBuf> = self.children.keys().cloned().collect();
            for dir in open {
                self.start_listing(dir, true);
            }
        } else {
            self.staged = None;
            for load in self.children.values() {
                load.cancel.cancel();
            }
            self.children.clear();
            self.entries.clear();
            self.visible.clear();
            self.depth.clear();
            self.selected.clear();
            self.cursor = None;
            self.anchor = None;
        }
        self.path = path.clone();
        self.pending_select = None;
        self.pending_select_keep = None;
        self.load = LoadState::Loading {
            since: Instant::now(),
        };
        let (tx, rx) = unbounded();
        self.rx = Some(rx);
        let cancel = self.cancel.clone();
        let spawned = std::thread::Builder::new()
            .name("list-dir".into())
            .spawn(move || {
                let r = listing::list_dir_streaming(&path, 2048, &cancel, |b| {
                    let _ = tx.send(ListMsg::Batch(b));
                });
                let _ = tx.send(match r {
                    Ok(errors) => ListMsg::Done { errors },
                    Err(e) => ListMsg::Failed(e.to_string()),
                });
            });
        if let Err(e) = spawned {
            self.load = LoadState::Failed(e.to_string());
        }
    }

    /// Drain listing messages. Returns true if anything changed (caller repaints).
    pub fn poll(&mut self) -> bool {
        let mut changed = self.sync_tree_view();
        changed |= self.poll_children();
        let Some(rx) = &self.rx else {
            if self.dirty_sort
                && (!self.children_loading() || self.last_sort.elapsed().as_millis() >= 150)
            {
                self.resort();
            }
            return changed;
        };
        let mut finished = None;
        let mut open = Vec::new();
        // Bound the work per frame so a giant folder can't stall a frame.
        for _ in 0..64 {
            match rx.try_recv() {
                Ok(ListMsg::Batch(b)) if self.staged.is_some() => {
                    self.staged.as_mut().unwrap().extend(b);
                }
                Ok(ListMsg::Batch(b)) => {
                    if self.view == ViewMode::Details && !self.expanded.is_empty() {
                        open.extend(
                            b.iter()
                                .filter(|e| {
                                    e.kind == EntryKind::Dir
                                        && self.expanded.contains(&e.path)
                                        && !self.children.contains_key(&e.path)
                                })
                                .map(|e| e.path.clone()),
                        );
                    }
                    self.entries.extend(b);
                    // Field-level update: `rx` is still borrowed from `self`.
                    self.listing_rev = next_listing_rev();
                    self.dirty_sort = true;
                    changed = true;
                }
                Ok(ListMsg::Done { errors }) => {
                    finished = Some(LoadState::Ready { errors });
                    break;
                }
                Ok(ListMsg::Failed(e)) => {
                    finished = Some(LoadState::Failed(e));
                    break;
                }
                Err(_) => break,
            }
        }
        for dir in open {
            self.start_child_load(dir);
        }
        if let Some(state) = finished {
            // Only a listing where every item was read shows which selected items are gone.
            let complete = matches!(state, LoadState::Ready { errors: 0 });
            if let Some(fresh) = self.staged.take() {
                // A refresh finished (a failed one leaves nothing listed, as before).
                let fresh = if matches!(state, LoadState::Failed(_)) { Vec::new() } else { fresh };
                let dir = self.path.clone();
                self.reshape(false, |pane| pane.swap_children(&dir, fresh, complete));
            }
            self.load = state;
            self.rx = None;
            self.dirty_sort = true;
            changed = true;
            // `refresh` keeps the old selection; forget paths that are no longer listed.
            // Items inside open folders may still be on their way, and items inside a
            // closed folder stay selected while the folder is listed (see `set_expanded`).
            if complete && !self.selected.is_empty() {
                let listed: HashSet<&Path> = self.entries.iter().map(|e| e.path.as_path()).collect();
                let expanded = &self.expanded;
                let root = self.path.as_path();
                self.selected.retain(|p| {
                    listed.contains(p.as_path())
                        || p.parent().is_some_and(|d| expanded.contains(d))
                        || p.ancestors()
                            .skip(1)
                            .take_while(|a| *a != root)
                            .any(|a| listed.contains(a))
                });
            }
        }
        if self.dirty_sort {
            // While streaming, re-sort at most every ~150ms; always sort when done.
            let due = (!self.is_loading() && !self.children_loading())
                || self.last_sort.elapsed().as_millis() >= 150;
            if due {
                self.resort();
            }
        }
        if changed && !self.is_loading() {
            if let Some(name) = self.pending_select.take() {
                self.select_name(&name);
            }
            // By path: names repeat across open folders. An item that is gone leaves no cursor.
            if let Some(path) = self.pending_select_keep.take() {
                self.cursor = self.row_of(&path);
            }
        }
        changed
    }

    pub fn resort(&mut self) {
        let cursor_path = self.cursor_entry().map(|e| e.path.clone());
        let anchor_path = self.anchor.and_then(|a| self.entry_at(a)).map(|e| e.path.clone());
        listing::sort_entries(&mut self.entries, self.sort);
        // Row indices are stale until `visible` is rebuilt; nothing may read them.
        self.visible.clear();
        self.listing_changed();
        self.dirty_sort = false;
        self.last_sort = Instant::now();
        self.rebuild_visible();
        if let Some(p) = cursor_path {
            self.cursor = self.row_of(&p);
        }
        self.anchor = anchor_path.and_then(|p| self.row_of(&p));
    }

    fn row_of(&self, path: &Path) -> Option<usize> {
        self.visible.iter().position(|&i| self.entries[i].path == path)
    }

    pub fn set_sort(&mut self, key: SortKey) {
        if self.sort.key == key {
            self.sort.descending = !self.sort.descending;
        } else {
            self.sort.key = key;
            self.sort.descending =
                matches!(key, SortKey::Size | SortKey::Modified | SortKey::Created);
        }
        self.resort();
    }

    /// Change the filter. The cursor and the range anchor stay on their items (row indices
    /// change when the list narrows or widens); when their item is filtered out they are
    /// cleared, never moved to another file (an action would then hit a file the user did not
    /// choose). The next arrow key starts from the first row. The selection is a set of paths,
    /// so it never goes stale.
    pub fn set_filter(&mut self, text: String) {
        let path_at = |pane: &Self, vis: Option<usize>| {
            vis.and_then(|v| pane.entry_at(v)).map(|e| e.path.clone())
        };
        let cursor = path_at(self, self.cursor);
        let anchor = path_at(self, self.anchor);
        self.filter_text = text;
        self.filter_error = None;
        self.filter_query = None;
        let t = self.filter_text.trim();
        if !t.is_empty() {
            match Query::parse(t) {
                Ok(q) => self.filter_query = Some(q),
                Err(e) => self.filter_error = Some(e.to_string()),
            }
        }
        self.rebuild_visible();
        let row_of = |pane: &Self, path: Option<PathBuf>| {
            path.and_then(|p| pane.visible.iter().position(|&i| pane.entries[i].path == p))
        };
        self.cursor = row_of(self, cursor);
        self.anchor = row_of(self, anchor);
    }

    /// Recompute the rows. The cursor and the Shift-select anchor stay on their items (by
    /// path) while those are listed, and are cleared when their item is no longer shown.
    /// Callers that changed `entries` must clear `visible` first.
    pub fn rebuild_visible(&mut self) {
        let cursor_path = self.cursor_entry().map(|e| e.path.clone());
        let anchor_path = self.anchor.and_then(|a| self.entry_at(a)).map(|e| e.path.clone());
        self.rebuild_rows();
        // An item that is no longer shown (hidden, filtered out) takes the cursor with it:
        // left on its old row, the cursor would point at another file.
        if let Some(p) = cursor_path {
            self.cursor = self.row_of(&p);
        }
        self.anchor = anchor_path.and_then(|p| self.row_of(&p));
    }

    fn rebuild_rows(&mut self) {
        self.listing_changed();
        let plain = self.filter_text.trim().to_lowercase();
        let shown = |e: &FileEntry| (self.show_hidden || !e.hidden) && (self.show_system || !e.system);
        let matches = |e: &FileEntry| match (&self.filter_query, &self.filter_error) {
            (Some(q), _) => q.matches_entry(e),
            // Unparseable query: fall back to plain substring so typing never "breaks".
            (None, Some(_)) => e.name.to_lowercase().contains(&plain),
            (None, None) => true,
        };
        if self.children.is_empty() {
            self.visible = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| shown(e) && matches(e))
                .map(|(i, _)| i)
                .collect();
            self.depth = Vec::new();
        } else {
            let (visible, depth) =
                flatten_tree(&self.entries, &self.path, &self.expanded, shown, matches);
            self.visible = visible;
            self.depth = depth;
        }
        if let Some(c) = self.cursor {
            if c >= self.visible.len() {
                self.cursor = self.visible.len().checked_sub(1);
            }
        }
    }

    pub fn entry_at(&self, vis: usize) -> Option<&FileEntry> {
        self.visible.get(vis).and_then(|&i| self.entries.get(i))
    }

    pub fn cursor_entry(&self) -> Option<&FileEntry> {
        self.cursor.and_then(|c| self.entry_at(c))
    }

    pub fn is_selected(&self, e: &FileEntry) -> bool {
        self.selected.contains(&e.path)
    }

    pub fn selected_entries(&self) -> Vec<&FileEntry> {
        self.visible
            .iter()
            .map(|&i| &self.entries[i])
            .filter(|e| self.selected.contains(&e.path))
            .collect()
    }

    /// Selected paths in display order, without the items inside a selected folder.
    pub fn selected_roots(&self) -> Vec<PathBuf> {
        without_covered(self.selected_entries().iter().map(|e| e.path.clone()).collect())
    }

    /// Selected paths, or the cursor item if nothing is selected.
    /// What an operation acts on: the selected paths (a folder covers the selected items
    /// inside it, see [`without_covered`]), or the cursor item if nothing is selected.
    ///
    /// A selection that is entirely hidden (hidden files turned off, filtered out, inside a
    /// closed folder) targets nothing: the cursor is not used in its place.
    pub fn action_targets(&self) -> Vec<PathBuf> {
        let sel = self.selected_roots();
        if sel.is_empty() && !self.selected.is_empty() {
            Vec::new()
        } else if sel.is_empty() {
            self.cursor_entry()
                .map(|e| vec![e.path.clone()])
                .unwrap_or_default()
        } else {
            sel
        }
    }

    /// What F2 renames (and what Properties and Open with show): the cursor item when it is
    /// selected (a right-clicked row is, even inside a selected folder, which leaves it out
    /// of [`Pane::action_targets`]) or is the target, else the first of the targets. A
    /// selection that is entirely hidden renames nothing, even when the cursor sits on
    /// another, visible file.
    pub fn rename_target(&self) -> Option<&FileEntry> {
        let targets = self.action_targets();
        self.cursor_entry()
            .filter(|e| self.is_selected(e) || targets.contains(&e.path))
            .or_else(|| {
                let first = targets.first()?;
                self.entries.iter().find(|e| &e.path == first)
            })
    }

    pub fn select_name(&mut self, name: &str) {
        // Names repeat across open folders: the item under the cursor wins.
        let at_cursor = self
            .cursor
            .filter(|_| self.cursor_entry().is_some_and(|e| e.name.eq_ignore_ascii_case(name)));
        if let Some(v) = at_cursor.or_else(|| {
            self.visible
                .iter()
                .position(|&i| self.entries[i].name.eq_ignore_ascii_case(name))
        }) {
            self.click(v, false, false);
            self.scroll_to_cursor = true;
        }
    }

    /// Select `name` once the current load completes (or now, if already loaded).
    pub fn set_pending_select(&mut self, name: String) {
        if self.is_loading() {
            self.pending_select = Some(name);
            // Supersedes a refresh's "keep the cursor where it was".
            self.pending_select_keep = None;
        } else {
            self.select_name(&name);
        }
    }

    pub fn select_all(&mut self) {
        self.selected = self
            .visible
            .iter()
            .map(|&i| self.entries[i].path.clone())
            .collect();
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    pub fn invert_selection(&mut self) {
        let all: HashSet<PathBuf> = self
            .visible
            .iter()
            .map(|&i| self.entries[i].path.clone())
            .collect();
        self.selected = all.difference(&self.selected).cloned().collect();
    }

    /// A right-click on a row: the menu acts on the selection when the row is part of it
    /// (the cursor moves to the row, so Open, Rename and Properties take it), else on the
    /// row alone.
    pub fn context_click(&mut self, vis: usize) {
        match self.entry_at(vis) {
            Some(e) if self.is_selected(e) => self.cursor = Some(vis),
            Some(_) => self.click(vis, false, false),
            None => {}
        }
    }

    /// Mouse/keyboard selection semantics matching Explorer.
    pub fn click(&mut self, vis: usize, ctrl: bool, shift: bool) {
        let Some(path) = self.entry_at(vis).map(|e| e.path.clone()) else {
            return;
        };
        if shift {
            let a = self.anchor.unwrap_or(vis);
            let (lo, hi) = if a <= vis { (a, vis) } else { (vis, a) };
            if !ctrl {
                self.selected.clear();
            }
            for v in lo..=hi {
                if let Some(e) = self.entry_at(v) {
                    self.selected.insert(e.path.clone());
                }
            }
        } else if ctrl {
            if !self.selected.remove(&path) {
                self.selected.insert(path);
            }
            self.anchor = Some(vis);
        } else {
            self.selected.clear();
            self.selected.insert(path);
            self.anchor = Some(vis);
        }
        self.cursor = Some(vis);
    }

    /// The members of a stack that the listing shows, in listing order. Hidden and filtered
    /// files are left out, so a stack never selects, drags or counts a file the user cannot see.
    pub fn visible_group_paths(&self, members: &[PathBuf]) -> Vec<PathBuf> {
        let listed: HashSet<&Path> = self
            .visible
            .iter()
            .map(|&i| self.entries[i].path.as_path())
            .collect();
        members
            .iter()
            .filter(|path| listed.contains(path.as_path()))
            .cloned()
            .collect()
    }

    /// Select every member represented by a collapsed stack while preserving Explorer's
    /// Ctrl and Shift selection behavior around the stack's first visible position.
    pub fn click_group(&mut self, vis: usize, members: &[PathBuf], ctrl: bool, shift: bool) {
        if shift {
            self.click(vis, ctrl, true);
            self.selected.extend(members.iter().cloned());
            return;
        }
        if ctrl {
            let all_selected = members.iter().all(|path| self.selected.contains(path));
            for path in members {
                if all_selected {
                    self.selected.remove(path);
                } else {
                    self.selected.insert(path.clone());
                }
            }
        } else {
            self.selected.clear();
            self.selected.extend(members.iter().cloned());
        }
        self.anchor = Some(vis);
        self.cursor = Some(vis);
    }

    /// Move the cursor by `delta` rows (keyboard navigation).
    pub fn move_cursor(&mut self, delta: isize, shift: bool, ctrl: bool) {
        if self.visible.is_empty() {
            return;
        }
        let cur = self.cursor.map(|c| c as isize).unwrap_or(-1);
        let next = (cur + delta).clamp(0, self.visible.len() as isize - 1) as usize;
        if ctrl {
            self.cursor = Some(next);
        } else {
            self.click(next, false, shift);
        }
        self.scroll_to_cursor = true;
    }

    /// Type-to-filter: typed characters extend the filter, so only matching names stay
    /// listed. A cursor whose item is filtered out is cleared (see [`Pane::set_filter`]).
    pub fn type_filter(&mut self, text: &str) {
        // A leading space would only make the filter look empty; spaces inside names count.
        let text = if self.filter_text.is_empty() { text.trim_start() } else { text };
        if text.is_empty() {
            return;
        }
        let mut filter = self.filter_text.clone();
        filter.push_str(text);
        // `set_filter` keeps the cursor on its item, or clears it.
        self.set_filter(filter);
        self.scroll_to_cursor = true;
    }

    /// Backspace in type-to-filter mode: removes the filter's last character. Returns false
    /// when there is no filter, so Backspace keeps its usual meaning (up one folder).
    pub fn filter_backspace(&mut self) -> bool {
        if self.filter_text.is_empty() {
            return false;
        }
        let mut filter = self.filter_text.clone();
        filter.pop();
        self.set_filter(filter);
        self.scroll_to_cursor = true;
        true
    }

    /// Esc in the file list undoes one thing at a time: the filter, then sticky selection,
    /// then the selection.
    pub fn escape(&mut self) {
        if !self.filter_text.is_empty() {
            self.set_filter(String::new());
            self.scroll_to_cursor = true;
        } else if self.sticky {
            self.sticky = false;
        } else {
            self.clear_selection();
        }
    }

    /// A mouse click on a row or tile: sticky selection turns a plain click into Ctrl+click.
    pub fn pointer_click(&mut self, vis: usize, ctrl: bool, shift: bool) {
        self.click(vis, ctrl || self.sticky, shift);
    }

    // -----------------------------------------------------------------------
    // Folders opened in place
    // -----------------------------------------------------------------------

    /// Tree depth of a visible row (0 for the folder's own entries).
    pub fn depth_at(&self, vis: usize) -> usize {
        self.depth.get(vis).copied().unwrap_or(0) as usize
    }

    /// Whether `entry` can be opened in place: a real folder, in details view. Links are
    /// left out so a junction loop cannot open forever.
    pub fn can_expand(&self, entry: &FileEntry) -> bool {
        self.view == ViewMode::Details && entry.kind == EntryKind::Dir
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        self.view == ViewMode::Details && self.expanded.contains(path)
    }

    /// The open folder's listing is still streaming in.
    pub fn is_child_loading(&self, path: &Path) -> bool {
        self.children.get(path).is_some_and(|c| c.loading && c.staged.is_none())
    }

    fn children_loading(&self) -> bool {
        self.children.values().any(|c| c.loading)
    }

    /// Listing the folder or a folder opened in place (the app keeps repainting).
    pub fn is_busy(&self) -> bool {
        self.is_loading() || self.children_loading()
    }

    /// Why an open folder could not be listed.
    pub fn child_error(&self, path: &Path) -> Option<&str> {
        self.children.get(path).and_then(|c| c.error.as_deref())
    }

    /// Folders whose items are listed (the open folders), for the folder watcher.
    pub fn open_folders(&self) -> impl Iterator<Item = &Path> {
        self.children.keys().map(PathBuf::as_path)
    }

    /// Open or close a listed folder in place. Closing forgets the items listed below it;
    /// folders open inside it stay remembered. Selected items inside stay selected but
    /// hidden, so they target nothing (see [`Pane::action_targets`]) and show again when
    /// the folder is reopened: dropping them would leave the cursor, now on the folder, as
    /// the target, and a Delete would take the whole folder.
    pub fn set_expanded(&mut self, path: &Path, open: bool) {
        if open {
            let listed = self
                .entries
                .iter()
                .any(|e| e.path == path && self.can_expand(e));
            if !listed || self.children.contains_key(path) {
                return;
            }
            self.expanded.insert(path.to_path_buf());
            self.reshape(false, |pane| pane.start_child_load(path.to_path_buf()));
        } else if self.expanded.remove(path) {
            // A cursor inside the folder moves to the folder.
            self.reshape(true, |pane| pane.drop_children(path, false));
            self.scroll_to_cursor = true;
        }
    }

    pub fn toggle_expanded_at(&mut self, vis: usize) {
        if let Some(e) = self.entry_at(vis).filter(|e| self.can_expand(e)) {
            let path = e.path.clone();
            let open = !self.expanded.contains(&path);
            self.set_expanded(&path, open);
        }
    }

    /// Right arrow in details view: open the folder under the cursor, or step into it when
    /// it is already open. Returns false when the key has nothing to do.
    pub fn tree_right(&mut self) -> bool {
        let Some(cur) = self.cursor else { return false };
        let Some(e) = self.entry_at(cur).filter(|e| self.can_expand(e)) else {
            return false;
        };
        let path = e.path.clone();
        if !self.children.contains_key(&path) {
            self.set_expanded(&path, true);
        } else if cur + 1 < self.visible.len() && self.depth_at(cur + 1) > self.depth_at(cur) {
            self.click(cur + 1, false, false);
            self.scroll_to_cursor = true;
        }
        true
    }

    /// Left arrow in details view: close the open folder under the cursor, or go to the
    /// folder that contains the item.
    pub fn tree_left(&mut self) -> bool {
        let Some(cur) = self.cursor else { return false };
        let Some(e) = self.entry_at(cur) else { return false };
        let path = e.path.clone();
        if self.can_expand(e) && self.children.contains_key(&path) {
            self.set_expanded(&path, false);
            return true;
        }
        if self.depth_at(cur) == 0 {
            return false;
        }
        let parent = path.parent().map(Path::to_path_buf);
        if let Some(v) = self
            .visible
            .iter()
            .position(|&i| Some(&self.entries[i].path) == parent.as_ref())
        {
            self.click(v, false, false);
            self.scroll_to_cursor = true;
        }
        true
    }

    /// Whether `dir` is an open folder whose items this pane lists.
    pub fn shows_children_of(&self, dir: &Path) -> bool {
        self.children.contains_key(dir)
    }

    /// Re-list an open folder (its contents changed). Selected items inside are kept when
    /// they are listed again.
    pub fn reload_children(&mut self, dir: &Path) {
        if !self.children.contains_key(dir) {
            return;
        }
        self.start_listing(dir.to_path_buf(), true);
    }

    fn start_child_load(&mut self, dir: PathBuf) {
        self.start_listing(dir, false);
    }

    /// Replace the rows listed below `dir` with a new listing of it. Rows of subfolders that
    /// are gone go too; open subfolders that remain keep theirs. Selected items the listing
    /// leaves out are forgotten only when it is `complete` (no item failed to read): an item
    /// a partial listing missed may still be there.
    fn swap_children(&mut self, dir: &Path, fresh: Vec<FileEntry>, complete: bool) {
        let listed: HashSet<&Path> = fresh.iter().map(|e| e.path.as_path()).collect();
        let gone: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| e.path.parent() == Some(dir) && !listed.contains(e.path.as_path()))
            .map(|e| e.path.clone())
            .collect();
        drop(listed);
        for path in &gone {
            self.drop_children(path, complete);
            if complete {
                self.selected.remove(path);
            }
        }
        let open: Vec<PathBuf> = fresh
            .iter()
            .filter(|e| {
                e.kind == EntryKind::Dir
                    && self.expanded.contains(&e.path)
                    && !self.children.contains_key(&e.path)
            })
            .map(|e| e.path.clone())
            .collect();
        self.entries.retain(|e| e.path.parent() != Some(dir));
        self.entries.extend(fresh);
        self.dirty_sort = true;
        if complete {
            self.prune_hidden_selection(dir);
        }
        for path in open {
            self.start_child_load(path);
        }
    }

    fn start_listing(&mut self, dir: PathBuf, replace: bool) {
        if let Some(old) = self.children.remove(&dir) {
            old.cancel.cancel();
        }
        let id = self.next_child_id;
        self.next_child_id += 1;
        let cancel = CancelToken::new();
        let job = ListJob {
            id,
            dir: dir.clone(),
            cancel: cancel.clone(),
            tx: self.child_tx.clone(),
        };
        let loading = subdir_queue().send(job).is_ok();
        let staged = replace.then(Vec::new);
        let error = (!loading).then(|| "the folder could not be listed".to_string());
        self.children.insert(dir, ChildLoad { id, cancel, loading, staged, error });
    }

    /// Remove everything listed below `dir` and cancel those listings.
    fn drop_children(&mut self, dir: &Path, deselect: bool) {
        let inside = |p: &Path| p != dir && p.starts_with(dir);
        self.children.retain(|path, load| {
            let keep = !path.starts_with(dir);
            if !keep {
                load.cancel.cancel();
            }
            keep
        });
        self.entries.retain(|e| !inside(&e.path));
        if deselect {
            self.selected.retain(|p| !inside(p));
        }
    }

    /// Drain the open folders' listings (bounded per frame, like the main listing).
    fn poll_children(&mut self) -> bool {
        let mut changed = false;
        let mut open = Vec::new();
        let mut swaps = Vec::new();
        // Folders whose first listing just completed without read errors.
        let mut listed = Vec::new();
        for _ in 0..64 {
            let Ok((id, dir, msg)) = self.child_rx.try_recv() else { break };
            let Some(load) = self.children.get_mut(&dir).filter(|l| l.id == id) else {
                continue;
            };
            match msg {
                ListMsg::Batch(b) if load.staged.is_some() => {
                    load.staged.as_mut().unwrap().extend(b);
                    continue;
                }
                ListMsg::Done { errors } if load.staged.is_some() => {
                    load.loading = false;
                    swaps.push((dir, load.staged.take().unwrap(), errors == 0));
                    continue;
                }
                ListMsg::Failed(e) if load.staged.is_some() => {
                    // Keep the rows that were listed; the folder may be back next time.
                    load.loading = false;
                    load.staged = None;
                    load.error = Some(e);
                    changed = true;
                    continue;
                }
                ListMsg::Batch(b) => {
                    if self.view == ViewMode::Details {
                        open.extend(
                            b.iter()
                                .filter(|e| e.kind == EntryKind::Dir && self.expanded.contains(&e.path))
                                .map(|e| e.path.clone()),
                        );
                    }
                    self.entries.extend(b);
                }
                ListMsg::Done { errors } => {
                    load.loading = false;
                    load.error = None;
                    if errors == 0 {
                        listed.push(dir.clone());
                    }
                }
                ListMsg::Failed(e) => {
                    load.loading = false;
                    load.error = Some(e);
                }
            }
            self.dirty_sort = true;
            changed = true;
        }
        for dir in open {
            self.start_child_load(dir);
        }
        for dir in listed {
            self.prune_hidden_selection(&dir);
        }
        if !swaps.is_empty() {
            self.reshape(false, |pane| {
                for (dir, fresh, complete) in swaps {
                    pane.swap_children(&dir, fresh, complete);
                }
            });
            changed = true;
        }
        changed
    }

    /// `dir` was just listed completely: forget selected paths below it that the listing
    /// shows are gone. A selected item inside a closed folder stays selected (hidden) while
    /// the folder is listed, so it may have been deleted or renamed meanwhile; without this
    /// it would stay selected, and a file that later appears at its path would be picked.
    /// Items deeper down stay while the folder holding them is listed here, until that
    /// folder's own listing completes.
    fn prune_hidden_selection(&mut self, dir: &Path) {
        if self.selected.is_empty() {
            return;
        }
        let listed: HashSet<&Path> = self
            .entries
            .iter()
            .filter(|e| e.path.parent() == Some(dir))
            .map(|e| e.path.as_path())
            .collect();
        self.selected.retain(|p| {
            // The item directly in `dir` that `p` is, or is inside of.
            match p.ancestors().find(|a| a.parent() == Some(dir)) {
                Some(top) => listed.contains(top),
                None => true,
            }
        });
    }

    /// Leaving details view closes the tree (the open folders stay remembered); coming back
    /// lists them again.
    fn sync_tree_view(&mut self) -> bool {
        if self.view == self.tree_view {
            return false;
        }
        self.tree_view = self.view;
        // Leaving details view closes every open folder: like closing one.
        self.reshape(true, |pane| {
            if pane.view != ViewMode::Details {
                let open: Vec<PathBuf> = pane.children.keys().cloned().collect();
                for dir in open {
                    // Kept selected but hidden, as when one folder is closed.
                    pane.drop_children(&dir, false);
                }
            } else {
                let open: Vec<PathBuf> = pane
                    .entries
                    .iter()
                    .filter(|e| e.kind == EntryKind::Dir && pane.expanded.contains(&e.path))
                    .map(|e| e.path.clone())
                    .collect();
                for dir in open {
                    pane.start_child_load(dir);
                }
            }
        });
        true
    }

    /// Change the tree's shape, then rebuild the rows. `change` may remove entries, which
    /// invalidates row indices, so the cursor and anchor are carried over by path. When its
    /// item is gone the anchor is cleared, and so is the cursor, unless the user closed the
    /// folder it was in (`closing`): then it moves to that folder. A cursor left on some
    /// other row would make Delete act on an item the user never chose.
    fn reshape(&mut self, closing: bool, change: impl FnOnce(&mut Self)) {
        let path_at = |pane: &Self, vis: Option<usize>| {
            vis.and_then(|v| pane.entry_at(v)).map(|e| e.path.clone())
        };
        let cursor = path_at(self, self.cursor);
        let anchor = path_at(self, self.anchor);
        change(self);
        self.cursor = None;
        self.anchor = None;
        self.visible.clear();
        if self.dirty_sort {
            self.resort();
        } else {
            self.rebuild_visible();
        }
        self.cursor = cursor.and_then(|p| {
            if closing { self.row_of_nearest(&p) } else { self.row_of(&p) }
        });
        // Only the cursor falls back to a parent folder: an anchor moved there would make a
        // later Shift-click select the folder (and a Delete take all of it).
        self.anchor = anchor.and_then(|p| self.row_of(&p));
    }

    /// Row of `path`, or of its nearest listed parent folder.
    fn row_of_nearest(&self, path: &Path) -> Option<usize> {
        path.ancestors()
            .take_while(|p| *p != self.path)
            .find_map(|p| self.visible.iter().position(|&i| self.entries[i].path == p))
    }


    /// Jump to the next entry whose name starts with the typed prefix (type-ahead find).
    pub fn type_ahead(&mut self, prefix: &str) {
        let p = prefix.to_lowercase();
        let start = self.cursor.map(|c| c + 1).unwrap_or(0);
        let n = self.visible.len();
        for k in 0..n {
            let v = (start + k) % n;
            if self.entries[self.visible[v]]
                .name
                .to_lowercase()
                .starts_with(&p)
            {
                self.click(v, false, false);
                self.scroll_to_cursor = true;
                return;
            }
        }
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.cancel.cancel();
        for load in self.children.values() {
            load.cancel.cancel();
        }
    }
}

/// Display order of a folder with some subfolders opened in place: each level in `entries`
/// order (already sorted), every expanded folder followed by its children, one level deeper.
/// `shown` applies the hidden/system rules (a hidden folder hides its children too);
/// `matches` is the filter. A folder that does not match stays listed while one of its
/// descendants does, so a match is never shown without the folders that lead to it.
/// Returns the indices into `entries` and the depth of each.
pub(crate) fn flatten_tree(
    entries: &[FileEntry],
    root: &Path,
    expanded: &HashSet<PathBuf>,
    shown: impl Fn(&FileEntry) -> bool,
    matches: impl Fn(&FileEntry) -> bool,
) -> (Vec<usize>, Vec<u16>) {
    let mut by_parent: HashMap<&Path, Vec<usize>> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if let Some(parent) = e.path.parent() {
            by_parent.entry(parent).or_default().push(i);
        }
    }
    let mut out = (Vec::new(), Vec::new());
    // Explicit stack instead of recursion: (parent, depth, next child position, row of the
    // folder whose children these are, or None for the root).
    struct Level<'a> {
        children: &'a [usize],
        next: usize,
        depth: u16,
        /// Row of the folder that owns this level and whether it matched the filter.
        owner: Option<(usize, bool)>,
    }
    let empty: Vec<usize> = Vec::new();
    let mut stack = vec![Level {
        children: by_parent.get(root).unwrap_or(&empty),
        next: 0,
        depth: 0,
        owner: None,
    }];
    while let Some(level) = stack.last_mut() {
        let Some(&i) = level.children.get(level.next) else {
            let done = stack.pop().unwrap();
            if let Some((row, matched)) = done.owner {
                // A folder that does not match is kept only when something below it is.
                if !matched && out.0.len() == row + 1 {
                    out.0.pop();
                    out.1.pop();
                }
            }
            continue;
        };
        level.next += 1;
        let depth = level.depth;
        let e = &entries[i];
        if !shown(e) {
            continue;
        }
        let matched = matches(e);
        let opens = e.kind == EntryKind::Dir && expanded.contains(&e.path);
        if opens {
            let row = out.0.len();
            out.0.push(i);
            out.1.push(depth);
            stack.push(Level {
                children: by_parent.get(e.path.as_path()).unwrap_or(&empty),
                next: 0,
                depth: depth + 1,
                owner: Some((row, matched)),
            });
        } else if matched {
            out.0.push(i);
            out.1.push(depth);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::model::EntryKind;

    #[test]
    fn refresh_of_the_same_folder_keeps_groups_and_navigation_drops_them() {
        let folder = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let mut pane = Pane::new(
            folder.path().to_path_buf(),
            SortSpec::default(),
            ViewMode::Grid,
        );
        pane.similar.toggle();
        let key = folder.path().join("burst-1.jpg");
        let second = folder.path().join("burst-2.jpg");
        pane.similar
            .insert_group_for_test(key.clone(), vec![key, second]);

        pane.refresh();
        assert_eq!(pane.similar.groups().len(), 1);

        pane.navigate(other.path().to_path_buf());
        assert!(pane.similar.groups().is_empty());
    }

    #[test]
    fn repeated_refresh_keeps_the_pending_cursor_and_selection() {
        let folder = tempfile::tempdir().unwrap();
        let selected = folder.path().join("selected.jpg");
        std::fs::write(&selected, b"image").unwrap();
        let mut pane = Pane::new(
            folder.path().to_path_buf(),
            SortSpec::default(),
            ViewMode::Details,
        );

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while pane.is_loading() {
            pane.poll();
            assert!(Instant::now() < deadline, "initial listing timed out");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        pane.poll();
        pane.select_name("selected.jpg");
        assert_eq!(
            pane.cursor_entry().map(|entry| entry.name.as_str()),
            Some("selected.jpg")
        );

        pane.refresh();
        pane.refresh();
        while pane.is_loading() {
            pane.poll();
            assert!(Instant::now() < deadline, "refreshed listing timed out");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        pane.poll();

        assert_eq!(
            pane.cursor_entry().map(|entry| entry.name.as_str()),
            Some("selected.jpg")
        );
        assert!(pane.selected.contains(&selected));
    }

    #[test]
    fn visible_group_paths_leave_out_entries_the_listing_hides() {
        let folder = tempfile::tempdir().unwrap();
        let mut pane = Pane::new(
            folder.path().to_path_buf(),
            SortSpec::default(),
            ViewMode::Grid,
        );
        let shown = folder.path().join("shown.jpg");
        let hidden = folder.path().join("hidden.jpg");
        pane.entries = vec![
            FileEntry {
                path: shown.clone(),
                name: "shown.jpg".into(),
                kind: EntryKind::File,
                size: 1,
                modified: None,
                created: None,
                hidden: false,
                readonly: false,
                system: false,
                online_only: false,
                ext: "jpg".into(),
            },
            FileEntry {
                path: hidden.clone(),
                name: "hidden.jpg".into(),
                kind: EntryKind::File,
                size: 1,
                modified: None,
                created: None,
                hidden: true,
                readonly: false,
                system: false,
                online_only: false,
                ext: "jpg".into(),
            },
        ];
        pane.rebuild_visible();

        let members = vec![hidden.clone(), shown.clone()];
        assert_eq!(pane.visible_group_paths(&members), vec![shown]);
    }
    fn listed(names: &[&str]) -> (tempfile::TempDir, Pane) {
        let folder = tempfile::tempdir().unwrap();
        let mut pane = Pane::new(
            folder.path().to_path_buf(),
            SortSpec::default(),
            ViewMode::Details,
        );
        pane.entries = names
            .iter()
            .map(|name| FileEntry {
                path: folder.path().join(name),
                name: (*name).into(),
                kind: EntryKind::File,
                size: 1,
                modified: None,
                created: None,
                hidden: false,
                readonly: false,
                system: false,
                online_only: false,
                ext: "jpg".into(),
            })
            .collect();
        pane.rebuild_visible();
        (folder, pane)
    }

    fn visible_names(pane: &Pane) -> Vec<&str> {
        pane.visible.iter().map(|&i| pane.entries[i].name.as_str()).collect()
    }

    #[test]
    fn typing_extends_the_filter_and_backspace_and_escape_undo_it() {
        let (_folder, mut pane) = listed(&["beach 01.jpg", "beach 02.jpg", "city.jpg"]);
        pane.type_filter(" ");
        assert!(pane.filter_text.is_empty(), "a leading space is not a filter");
        pane.type_filter("bea");
        pane.type_filter("ch 0");
        assert_eq!(pane.filter_text, "beach 0");
        assert_eq!(visible_names(&pane), ["beach 01.jpg", "beach 02.jpg"]);
        assert_eq!(pane.cursor, None, "filtering never places the cursor");
        assert!(pane.selected.is_empty(), "filtering never selects");

        pane.cursor = Some(1);
        pane.type_filter("2");
        assert_eq!(visible_names(&pane), ["beach 02.jpg"]);
        assert_eq!(pane.cursor, Some(0), "the cursor stays on its item");
        pane.cursor = Some(0);
        pane.filter_backspace();
        pane.cursor = Some(1);
        pane.type_filter("1");
        assert_eq!(pane.cursor, None, "a filtered-out cursor is cleared, not moved");
        pane.move_cursor(1, false, false);
        assert_eq!(pane.cursor, Some(0), "the next arrow starts from the first row");
        pane.filter_backspace();
        pane.type_filter("2");
        assert!(pane.filter_backspace());
        assert_eq!(pane.filter_text, "beach 0");
        assert_eq!(visible_names(&pane).len(), 2);

        pane.escape();
        assert!(pane.filter_text.is_empty());
        assert_eq!(visible_names(&pane).len(), 3);
        assert!(!pane.filter_backspace(), "no filter: Backspace goes up a folder instead");
    }

    #[test]
    fn widening_the_filter_keeps_the_cursor_and_anchor_on_their_items() {
        let (_folder, mut pane) = listed(&["apple.jpg", "berry.jpg", "cherry.jpg"]);
        pane.click(0, false, false); // anchor on apple.jpg
        pane.cursor = Some(2); // cursor on cherry.jpg
        pane.type_filter("cherr");
        assert_eq!(visible_names(&pane), ["cherry.jpg"]);
        assert_eq!(pane.cursor, Some(0));
        assert_eq!(pane.anchor, None, "the anchor's item is filtered out");

        assert!(pane.filter_backspace());
        assert_eq!(
            pane.cursor_entry().map(|e| e.name.as_str()),
            Some("cherry.jpg"),
            "Backspace keeps the cursor on cherry.jpg, not on whatever is now at its row"
        );

        pane.escape();
        pane.click(2, false, false); // anchor and cursor on cherry.jpg
        pane.type_filter("cherr");
        pane.escape();
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("cherry.jpg"));
        assert_eq!(pane.entry_at(pane.anchor.unwrap()).map(|e| e.name.as_str()), Some("cherry.jpg"));
        assert!(pane.selected.contains(&pane.entries[2].path), "the selection is by path");
    }

    #[test]
    fn a_cursor_filtered_away_stays_cleared_when_the_filter_is_cleared() {
        let (_folder, mut pane) = listed(&["apple.jpg", "berry.jpg"]);
        pane.cursor = Some(1);
        pane.type_filter("qqqq");
        assert!(pane.visible.is_empty());
        assert_eq!(pane.cursor, None);
        pane.escape();
        assert_eq!(visible_names(&pane).len(), 2);
        assert_eq!(pane.cursor, None, "no file becomes the cursor by itself");
        assert!(pane.action_targets().is_empty());
    }

    #[test]
    fn showing_hidden_files_keeps_the_cursor_and_anchor_on_their_items() {
        let (_folder, mut pane) = listed(&["a-hidden.jpg", "b.jpg", "c.jpg", "d.jpg"]);
        pane.entries[0].hidden = true;
        pane.rebuild_visible();
        assert_eq!(visible_names(&pane), ["b.jpg", "c.jpg", "d.jpg"]);
        pane.click(1, false, false); // anchor on c.jpg
        pane.cursor = Some(2); // cursor on d.jpg

        // The hidden file sorts before both, so every row index shifts by one.
        pane.show_hidden = true;
        pane.rebuild_visible();
        assert_eq!(visible_names(&pane), ["a-hidden.jpg", "b.jpg", "c.jpg", "d.jpg"]);
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("d.jpg"));
        assert_eq!(pane.entry_at(pane.anchor.unwrap()).map(|e| e.name.as_str()), Some("c.jpg"));

        pane.show_hidden = false;
        pane.rebuild_visible();
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("d.jpg"));
        assert_eq!(pane.entry_at(pane.anchor.unwrap()).map(|e| e.name.as_str()), Some("c.jpg"));

        // Sorting the other way moves the rows too.
        pane.set_sort(SortKey::Name);
        assert_eq!(visible_names(&pane), ["d.jpg", "c.jpg", "b.jpg"]);
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("d.jpg"));
        assert_eq!(pane.entry_at(pane.anchor.unwrap()).map(|e| e.name.as_str()), Some("c.jpg"));
    }

    #[test]
    fn hiding_the_selected_cursor_item_leaves_nothing_to_act_on() {
        let (_folder, mut pane) = listed(&["a-secret.jpg", "b.jpg", "c.jpg"]);
        pane.entries[0].hidden = true;
        pane.show_hidden = true;
        pane.rebuild_visible();
        pane.click(0, false, false);
        assert_eq!(pane.action_targets(), vec![pane.entries[0].path.clone()]);

        pane.show_hidden = false;
        pane.rebuild_visible();
        assert_eq!(pane.cursor, None, "the cursor does not move to b.jpg");
        assert!(pane.action_targets().is_empty(), "Delete must not hit another file");
        assert!(pane.selected.contains(&pane.entries[0].path), "the hidden pick is kept");

        // Shown again, the selection is the target again.
        pane.show_hidden = true;
        pane.rebuild_visible();
        assert_eq!(pane.action_targets(), vec![pane.entries[0].path.clone()]);
    }

    #[test]
    fn filtering_away_the_selection_leaves_nothing_to_act_on() {
        let (_folder, mut pane) = listed(&["apple.jpg", "berry.jpg", "cherry.jpg"]);
        pane.click(2, false, false);
        pane.type_filter("berr");
        assert_eq!(visible_names(&pane), ["berry.jpg"]);
        assert!(pane.action_targets().is_empty(), "the selection is filtered out");
        pane.escape();
        assert_eq!(pane.action_targets(), vec![pane.entries[2].path.clone()]);
    }

    #[test]
    fn rename_never_targets_a_file_the_user_did_not_pick() {
        let (_folder, mut pane) = listed(&["apple.jpg", "berry.jpg", "cherry.jpg"]);
        pane.click(2, false, false);
        pane.type_filter("berr");
        assert_eq!(pane.rename_target(), None, "the cursor was cleared with its item");
        pane.cursor = Some(0); // berry, as Ctrl+arrow would leave it, unselected
        assert_eq!(pane.rename_target(), None, "cherry is selected but hidden: rename nothing");

        pane.escape();
        assert_eq!(pane.rename_target().map(|e| e.name.as_str()), Some("cherry.jpg"));
        pane.click(0, true, false); // apple joins; the cursor is on apple
        assert_eq!(pane.rename_target().map(|e| e.name.as_str()), Some("apple.jpg"));
        pane.clear_selection();
        pane.cursor = Some(1);
        assert_eq!(pane.rename_target().map(|e| e.name.as_str()), Some("berry.jpg"));
    }

    #[test]
    fn targets_are_the_visible_selection_or_the_cursor() {
        let (_folder, mut pane) = listed(&["apple.jpg", "berry.jpg", "cherry.jpg"]);
        pane.click(0, false, false);
        pane.click(2, true, false);
        assert_eq!(
            pane.action_targets(),
            vec![pane.entries[0].path.clone(), pane.entries[2].path.clone()]
        );
        pane.clear_selection();
        pane.cursor = Some(1);
        assert_eq!(pane.action_targets(), vec![pane.entries[1].path.clone()]);
    }

    #[test]
    fn typed_query_syntax_uses_the_same_filter_machinery() {
        let (_folder, mut pane) = listed(&["a.jpg", "b.cr3", "c.cr3"]);
        pane.entries[1].ext = "cr3".into();
        pane.entries[2].ext = "cr3".into();
        pane.type_filter("ext:cr3");
        assert_eq!(visible_names(&pane), ["b.cr3", "c.cr3"]);
    }

    #[test]
    fn sticky_selection_makes_a_plain_click_toggle() {
        let (_folder, mut pane) = listed(&["1.jpg", "2.jpg", "3.jpg"]);
        pane.pointer_click(0, false, false);
        pane.pointer_click(1, false, false);
        assert_eq!(pane.selected.len(), 1, "without sticky a click replaces the selection");

        pane.sticky = true;
        pane.pointer_click(0, false, false);
        pane.pointer_click(2, false, false);
        assert_eq!(pane.selected.len(), 3);
        pane.pointer_click(1, false, false);
        assert_eq!(pane.selected.len(), 2, "a second click takes the item out again");
        assert!(!pane.selected.contains(&pane.entries[1].path));

        // Keyboard movement still moves a single selection: sticky is for the pointer.
        pane.move_cursor(1, false, false);
        assert_eq!(pane.selected.len(), 1);
    }

    #[test]
    fn escape_clears_filter_then_sticky_then_selection() {
        let (_folder, mut pane) = listed(&["1.jpg", "2.jpg"]);
        pane.sticky = true;
        pane.pointer_click(0, false, false);
        pane.pointer_click(1, false, false);
        pane.type_filter("1");

        pane.escape();
        assert!(pane.filter_text.is_empty());
        assert!(pane.sticky);
        pane.escape();
        assert!(!pane.sticky, "the second Esc ends sticky selection");
        assert_eq!(pane.selected.len(), 2, "and keeps what was picked");
        pane.escape();
        assert!(pane.selected.is_empty());
    }

    fn fake_entry(root: &Path, rel: &str, kind: EntryKind) -> FileEntry {
        let path = root.join(rel);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        FileEntry {
            path,
            name,
            kind,
            size: 1,
            modified: None,
            created: None,
            hidden: false,
            readonly: false,
            system: false,
            online_only: false,
            ext: "".into(),
        }
    }

    fn open_set(paths: &[PathBuf]) -> HashSet<PathBuf> {
        paths.iter().cloned().collect()
    }

    #[test]
    fn flatten_tree_lists_open_folders_depth_first() {
        let root = Path::new(r"C:\root");
        let entries = vec![
            fake_entry(root, "a", EntryKind::Dir),
            fake_entry(root, "a/x.txt", EntryKind::File),
            fake_entry(root, "a/sub", EntryKind::Dir),
            fake_entry(root, "a/sub/deep.txt", EntryKind::File),
            fake_entry(root, "b.txt", EntryKind::File),
            fake_entry(root, "c", EntryKind::Dir),
            fake_entry(root, "c/k.txt", EntryKind::File),
        ];
        // `sub` is not open, so `deep.txt` stays out.
        let expanded = open_set(&[root.join("a"), root.join("c")]);
        let (rows, depth) = flatten_tree(
            &entries,
            root,
            &expanded,
            |_: &FileEntry| true,
            |_: &FileEntry| true,
        );
        assert_eq!(rows, vec![0, 1, 2, 4, 5, 6]);
        assert_eq!(depth, vec![0, 1, 1, 0, 0, 1]);

        let closed = open_set(&[]);
        let (rows, depth) = flatten_tree(
            &entries,
            root,
            &closed,
            |_: &FileEntry| true,
            |_: &FileEntry| true,
        );
        assert_eq!(rows, vec![0, 4, 5]);
        assert_eq!(depth, vec![0, 0, 0]);
    }

    #[test]
    fn flatten_tree_hides_the_children_of_a_hidden_folder() {
        let root = Path::new(r"C:\root");
        let entries = vec![
            fake_entry(root, "a", EntryKind::Dir),
            fake_entry(root, "a/x.txt", EntryKind::File),
            fake_entry(root, "b.txt", EntryKind::File),
        ];
        let expanded = open_set(&[root.join("a")]);
        let (rows, depth) = flatten_tree(
            &entries,
            root,
            &expanded,
            |_: &FileEntry| true,
            |_: &FileEntry| true,
        );
        assert_eq!(rows, vec![0, 1, 2]);
        assert_eq!(depth, vec![0, 1, 0]);

        let (rows, _) = flatten_tree(
            &entries,
            root,
            &expanded,
            |e: &FileEntry| e.name != "a",
            |_: &FileEntry| true,
        );
        assert_eq!(rows, vec![2], "the open folder is hidden, so its children are too");
    }

    #[test]
    fn flatten_tree_filter_keeps_folders_that_lead_to_a_match() {
        let root = Path::new(r"C:\root");
        let entries = vec![
            fake_entry(root, "a", EntryKind::Dir),
            fake_entry(root, "a/x.txt", EntryKind::File),
            fake_entry(root, "a/sub", EntryKind::Dir),
            fake_entry(root, "a/sub/deep.txt", EntryKind::File),
            fake_entry(root, "b.txt", EntryKind::File),
            fake_entry(root, "c", EntryKind::Dir),
            fake_entry(root, "c/y.txt", EntryKind::File),
        ];
        let expanded = open_set(&[root.join("a"), root.join("a").join("sub"), root.join("c")]);

        // `a` and `sub` do not match, but `deep.txt` below them does. `c` has no match at all.
        let (rows, depth) = flatten_tree(
            &entries,
            root,
            &expanded,
            |_: &FileEntry| true,
            |e: &FileEntry| e.name == "deep.txt",
        );
        assert_eq!(rows, vec![0, 2, 3]);
        assert_eq!(depth, vec![0, 1, 2]);

        // A matching child under a matching folder stays, with the sibling that does not match dropped.
        let (rows, depth) = flatten_tree(
            &entries,
            root,
            &expanded,
            |_: &FileEntry| true,
            |e: &FileEntry| e.name == "a" || e.name == "x.txt",
        );
        assert_eq!(rows, vec![0, 1]);
        assert_eq!(depth, vec![0, 1]);

        // A matching folder stays even when nothing below it matches.
        let (rows, _) = flatten_tree(
            &entries,
            root,
            &expanded,
            |_: &FileEntry| true,
            |e: &FileEntry| e.name == "a",
        );
        assert_eq!(rows, vec![0]);
    }

    /// Waits until the pane's main listing and every open folder's listing have finished,
    /// then takes the final poll so the rows are rebuilt.
    fn settle(pane: &mut Pane) {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            pane.poll();
            if !pane.is_busy() {
                break;
            }
            assert!(Instant::now() < deadline, "folder listing timed out");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        pane.poll();
    }

    /// `root` holds `a/` (with `a1.txt` and `sub/deep.txt`) and `b.txt`.
    fn tree_fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("a").join("sub")).unwrap();
        std::fs::write(root.join("a").join("a1.txt"), b"a1").unwrap();
        std::fs::write(root.join("a").join("sub").join("deep.txt"), b"deep").unwrap();
        std::fs::write(root.join("b.txt"), b"b").unwrap();
        (dir, root)
    }

    fn open_tree_pane(root: &Path) -> Pane {
        let mut pane = Pane::new(root.to_path_buf(), SortSpec::default(), ViewMode::Details);
        settle(&mut pane);
        pane
    }

    fn row_of(pane: &Pane, name: &str) -> usize {
        pane.visible
            .iter()
            .position(|&i| pane.entries[i].name == name)
            .unwrap_or_else(|| panic!("{name} is not listed"))
    }

    #[test]
    fn expanded_folders_list_their_children_in_place() {
        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let sub = a.join("sub");
        let mut pane = open_tree_pane(&root);

        pane.set_expanded(&a, true);
        settle(&mut pane);
        assert_eq!(visible_names(&pane), ["a", "sub", "a1.txt", "b.txt"]);
        assert_eq!(pane.depth, vec![0u16, 1, 1, 0]);

        pane.set_expanded(&sub, true);
        settle(&mut pane);
        assert_eq!(visible_names(&pane), ["a", "sub", "deep.txt", "a1.txt", "b.txt"]);
        assert_eq!(pane.depth, vec![0u16, 1, 2, 1, 0]);

        pane.set_expanded(&a, false);
        settle(&mut pane);
        assert_eq!(visible_names(&pane), ["a", "b.txt"]);
        assert!(pane.expanded.contains(&sub), "the nested folder stays remembered");
        assert!(!pane.expanded.contains(&a));
    }

    #[test]
    fn arrow_keys_open_step_into_and_close_folders() {
        let (_dir, root) = tree_fixture();
        let mut pane = open_tree_pane(&root);

        pane.select_name("a");
        assert!(pane.tree_right(), "right arrow opens the folder under the cursor");
        settle(&mut pane);
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("a"));

        assert!(pane.tree_right(), "right arrow on an open folder steps into it");
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("sub"));

        assert!(pane.tree_left(), "left arrow on a closed nested item goes to its folder");
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("a"));

        assert!(pane.tree_left(), "left arrow on an open folder closes it");
        assert!(!pane.expanded.contains(&root.join("a")));
        assert_eq!(visible_names(&pane), ["a", "b.txt"]);
    }

    #[test]
    fn selection_reaches_nested_items_and_collapsing_hides_them() {
        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&a, true);
        settle(&mut pane);

        let nested = row_of(&pane, "a1.txt");
        pane.click(nested, false, false);
        let other = row_of(&pane, "b.txt");
        pane.click(other, true, false);
        let picked: Vec<&str> = pane
            .selected_entries()
            .into_iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(picked, ["a1.txt", "b.txt"]);

        pane.set_expanded(&a, false);
        assert_eq!(pane.selected.len(), 2, "the collapsed folder's item stays selected");
        let shown: Vec<&str> = pane.selected_entries().into_iter().map(|e| e.name.as_str()).collect();
        assert_eq!(shown, ["b.txt"], "but hidden");
        assert_eq!(pane.action_targets(), vec![root.join("b.txt")]);
    }

    #[test]
    fn collapsing_a_folder_never_widens_the_target_to_it() {
        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&a, true);
        settle(&mut pane);
        pane.set_expanded(&root.join("a").join("sub"), true);
        settle(&mut pane);

        let first = row_of(&pane, "a1.txt");
        pane.click(first, false, false);
        let last = row_of(&pane, "deep.txt");
        pane.click(last, true, false);
        assert_eq!(pane.action_targets().len(), 2);

        // Left arrow on the nested item goes to its folder; closing it leaves the cursor
        // on `a` with the selection hidden inside.
        pane.set_expanded(&a, false);
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("a"));
        assert!(pane.action_targets().is_empty(), "Delete must not take the closed folder");
        assert_eq!(pane.rename_target(), None);

        // A refresh keeps the hidden selection (dropping it would target the folder too).
        pane.refresh();
        settle(&mut pane);
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("a"));
        assert!(pane.action_targets().is_empty(), "still nothing after a refresh");

        // Reopening shows the selection again.
        pane.set_expanded(&a, true);
        settle(&mut pane);
        let mut shown: Vec<&str> = pane.selected_entries().into_iter().map(|e| e.name.as_str()).collect();
        shown.sort();
        assert_eq!(shown, ["a1.txt", "deep.txt"]);

        // Leaving details view closes every folder the same way.
        pane.view = ViewMode::Grid;
        pane.poll();
        assert!(pane.action_targets().is_empty(), "the grid view hides the selection");

        // Esc is the way out: the cursor item is the target again.
        pane.escape();
        assert_eq!(pane.action_targets(), vec![a]);
    }

    #[test]
    fn right_click_on_a_selected_row_moves_the_cursor_and_keeps_the_selection() {
        let (_dir, root) = tree_fixture();
        let mut pane = open_tree_pane(&root);
        let a = row_of(&pane, "a");
        let b = row_of(&pane, "b.txt");
        pane.click(a, false, false);
        pane.click(b, true, false);
        // Ctrl+Up leaves the cursor on `a`; the right-click is on `b.txt`.
        pane.move_cursor(-1, false, true);
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("a"));

        pane.context_click(b);
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("b.txt"), "Open takes it");
        assert_eq!(pane.rename_target().map(|e| e.name.as_str()), Some("b.txt"));
        assert_eq!(pane.action_targets().len(), 2, "the selection stays for Delete and Copy");

        // A row outside the selection replaces it.
        pane.clear_selection();
        pane.click(a, false, false);
        pane.context_click(b);
        assert_eq!(pane.action_targets(), vec![root.join("b.txt")]);
    }

    #[test]
    fn filter_applies_to_nested_items() {
        let (_dir, root) = tree_fixture();
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&root.join("a"), true);
        settle(&mut pane);

        pane.set_filter("a1".into());
        assert_eq!(visible_names(&pane), ["a", "a1.txt"]);
    }

    #[test]
    fn leaving_details_view_closes_the_tree_and_returning_reopens_it() {
        let (_dir, root) = tree_fixture();
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&root.join("a"), true);
        settle(&mut pane);

        pane.view = ViewMode::Grid;
        pane.poll();
        assert_eq!(visible_names(&pane), ["a", "b.txt"], "only the top level is shown");
        assert!(pane.depth.is_empty());

        pane.view = ViewMode::Details;
        settle(&mut pane);
        assert_eq!(visible_names(&pane), ["a", "sub", "a1.txt", "b.txt"]);
    }

    #[test]
    fn reloading_an_open_folder_keeps_its_rows_until_the_new_listing_is_in() {
        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&a, true);
        settle(&mut pane);
        let a1 = row_of(&pane, "a1.txt");
        pane.click(a1, false, false);

        std::fs::write(a.join("a2.txt"), b"a2").unwrap();
        std::fs::remove_dir_all(a.join("sub")).unwrap();
        pane.reload_children(&a);
        assert_eq!(
            visible_names(&pane),
            ["a", "sub", "a1.txt", "b.txt"],
            "the old rows stay while the folder is listed again"
        );
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while visible_names(&pane) != ["a", "a1.txt", "a2.txt", "b.txt"] {
            pane.poll();
            assert!(Instant::now() < deadline, "reload timed out: {:?}", visible_names(&pane));
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(pane.depth, vec![0u16, 1, 1, 0]);
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("a1.txt"));
        assert!(pane.selected.contains(&a.join("a1.txt")), "the selection survives");
    }

    #[test]
    fn refresh_keeps_nested_rows_and_forgets_an_open_folder_that_is_gone() {
        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&a, true);
        settle(&mut pane);

        pane.refresh();
        assert!(
            pane.entries.iter().any(|e| e.name == "a1.txt"),
            "nested rows are not thrown away by a refresh"
        );
        settle(&mut pane);
        assert_eq!(visible_names(&pane), ["a", "sub", "a1.txt", "b.txt"]);

        std::fs::remove_dir_all(&a).unwrap();
        pane.refresh();
        settle(&mut pane);
        assert_eq!(visible_names(&pane), ["b.txt"]);
        assert!(!pane.entries.iter().any(|e| e.name == "a1.txt"));
        assert!(!pane.shows_children_of(&a));
    }

    #[test]
    fn a_selected_folder_covers_the_selected_items_inside_it() {
        let paths = |list: &[&str]| list.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(
            without_covered(paths(&[
                r"C:\a\b",
                r"C:\A\B\c.txt",
                r"C:\a\bc",
                r"C:\a\b\sub\deep.txt",
                r"C:\x.txt",
            ])),
            paths(&[r"C:\a\b", r"C:\a\bc", r"C:\x.txt"]),
            "per component and ignoring case; order kept"
        );

        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&a, true);
        settle(&mut pane);
        pane.click(row_of(&pane, "a"), false, false);
        pane.click(row_of(&pane, "a1.txt"), true, false);
        pane.click(row_of(&pane, "b.txt"), true, false);
        assert_eq!(pane.selected.len(), 3);
        assert_eq!(pane.action_targets(), vec![a, root.join("b.txt")]);
    }

    #[test]
    fn right_clicking_a_selected_item_inside_a_selected_folder_targets_the_item() {
        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&a, true);
        settle(&mut pane);
        pane.click(row_of(&pane, "a"), false, false);
        pane.click(row_of(&pane, "a1.txt"), true, false);

        pane.context_click(row_of(&pane, "a1.txt"));
        // Rename, Properties and Open with take the clicked row, not the folder covering it.
        assert_eq!(pane.rename_target().map(|e| e.name.as_str()), Some("a1.txt"));
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("a1.txt"));
        // Delete, Copy and Move still act on the folder, which covers the item.
        assert_eq!(pane.action_targets(), vec![a]);
    }

    #[test]
    fn reopening_a_folder_forgets_hidden_picks_that_are_gone() {
        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let sub = a.join("sub");
        let (a1, deep) = (a.join("a1.txt"), sub.join("deep.txt"));
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&a, true);
        settle(&mut pane);
        pane.set_expanded(&sub, true);
        settle(&mut pane);
        pane.click(row_of(&pane, "a1.txt"), false, false);
        pane.click(row_of(&pane, "deep.txt"), true, false);
        pane.set_expanded(&a, false);
        settle(&mut pane);

        // Gone while their folder is closed: a refresh cannot tell, the folder is listed.
        std::fs::remove_file(&a1).unwrap();
        std::fs::rename(&sub, a.join("moved")).unwrap();
        pane.refresh();
        settle(&mut pane);
        assert_eq!(pane.selected.len(), 2, "still hidden in the closed folder");

        // Reopening lists the folder: the picks that are gone are forgotten.
        pane.set_expanded(&a, true);
        settle(&mut pane);
        assert!(pane.selected.is_empty(), "{:?}", pane.selected);

        // A file that appears at the old path later is not picked.
        std::fs::write(&a1, b"new").unwrap();
        pane.refresh();
        settle(&mut pane);
        assert!(!pane.selected.contains(&a1));
        assert!(!pane.selected.contains(&deep));
    }

    #[test]
    fn a_listing_with_read_errors_never_forgets_a_pick() {
        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let a1 = a.join("a1.txt");
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&a, true);
        settle(&mut pane);
        pane.click(row_of(&pane, "a1.txt"), false, false);
        let sub = pane.entries.iter().find(|e| e.name == "sub").unwrap().clone();

        // A re-listing of `a` that could not read `a1.txt`: it lists only `sub`.
        let relist = |pane: &mut Pane, id: u64, errors: usize| {
            pane.children.insert(
                a.clone(),
                ChildLoad {
                    id,
                    cancel: CancelToken::new(),
                    loading: true,
                    staged: Some(vec![sub.clone()]),
                    error: None,
                },
            );
            pane.child_tx.send((id, a.clone(), ListMsg::Done { errors })).unwrap();
            pane.poll();
        };
        relist(&mut pane, 1_000_001, 1);
        assert!(pane.selected.contains(&a1), "a partial listing keeps the pick");
        relist(&mut pane, 1_000_002, 0);
        assert!(!pane.selected.contains(&a1), "a complete one shows it is gone");
    }

    #[test]
    fn a_hidden_pick_in_a_nested_closed_folder_stays_until_that_folder_is_listed() {
        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let sub = a.join("sub");
        let deep = sub.join("deep.txt");
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&a, true);
        settle(&mut pane);
        pane.set_expanded(&sub, true);
        settle(&mut pane);
        pane.click(row_of(&pane, "deep.txt"), false, false);
        pane.set_expanded(&sub, false);
        settle(&mut pane);

        // `a` is listed again, `sub` is still there but closed: the pick stays, hidden.
        pane.reload_children(&a);
        settle(&mut pane);
        assert!(pane.selected.contains(&deep));
        assert!(pane.action_targets().is_empty(), "hidden picks target nothing");
    }

    #[test]
    fn the_shift_select_anchor_stays_on_its_item_when_rows_move() {
        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let mut pane = open_tree_pane(&root);
        pane.click(row_of(&pane, "b.txt"), false, false);

        // Rows streamed in above the anchor.
        pane.set_expanded(&a, true);
        settle(&mut pane);
        assert_eq!(pane.anchor, Some(row_of(&pane, "b.txt")));
        pane.click(row_of(&pane, "a1.txt"), false, true);
        let picked: Vec<&str> = pane.selected_entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(picked, ["a1.txt", "b.txt"], "the range runs from the anchor's item");

        // A re-sort moves the rows.
        pane.click(row_of(&pane, "b.txt"), false, false);
        pane.set_sort(SortKey::Name);
        assert_eq!(pane.anchor, Some(row_of(&pane, "b.txt")));
        // Its item filtered out: the anchor is dropped instead of pointing at another row.
        pane.set_filter("a1".into());
        assert_eq!(pane.anchor, None);
    }

    #[test]
    fn collapsing_a_folder_clears_an_anchor_inside_it_but_moves_the_cursor_up() {
        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&a, true);
        settle(&mut pane);
        pane.click(row_of(&pane, "a1.txt"), false, false);

        pane.set_expanded(&a, false);
        assert_eq!(pane.cursor_entry().map(|e| e.name.as_str()), Some("a"));
        assert_eq!(pane.anchor, None, "the anchor is not moved to the folder");

        // A Shift-click now selects from the clicked row only, never the folder.
        pane.click(row_of(&pane, "b.txt"), false, true);
        let picked: Vec<&str> = pane.selected_entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(picked, ["b.txt"]);
    }

    #[test]
    fn an_open_folder_that_cannot_be_listed_reports_why() {
        let (_dir, root) = tree_fixture();
        let a = root.join("a");
        let mut pane = open_tree_pane(&root);
        std::fs::remove_dir_all(&a).unwrap();
        pane.set_expanded(&a, true);
        settle(&mut pane);
        assert!(pane.child_error(&a).is_some(), "the folder row shows the error");
        assert!(!pane.is_busy());
    }

    #[test]
    fn many_open_folders_share_the_listing_workers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        for n in 0..12 {
            let sub = root.join(format!("f{n:02}"));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("x.txt"), b"x").unwrap();
        }
        let mut pane = open_tree_pane(&root);
        for n in 0..12 {
            pane.set_expanded(&root.join(format!("f{n:02}")), true);
        }
        settle(&mut pane);
        assert_eq!(pane.visible.len(), 24, "every folder and its file");
        assert!(pane.depth.iter().filter(|&&d| d == 1).count() == 12);
        // Closing a folder before its job ran drops the job.
        pane.set_expanded(&root.join("f00"), false);
        assert!(!pane.shows_children_of(&root.join("f00")));
    }

    #[test]
    fn refresh_keeps_the_open_folders() {
        let (_dir, root) = tree_fixture();
        let mut pane = open_tree_pane(&root);
        pane.set_expanded(&root.join("a"), true);
        settle(&mut pane);

        pane.refresh();
        settle(&mut pane);
        assert_eq!(visible_names(&pane), ["a", "sub", "a1.txt", "b.txt"]);
    }
}
