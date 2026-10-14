//! A browsing pane: current folder, history, streaming listing, filter, sort, selection.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossbeam_channel::{Receiver, unbounded};
use forge_core::FileEntry;
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

pub enum LoadState {
    Loading { since: Instant },
    Ready { errors: usize },
    Failed(String),
}

pub struct Pane {
    pub path: PathBuf,
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    /// All entries of the folder, sorted by `sort`.
    pub entries: Vec<FileEntry>,
    /// Indices into `entries` that pass the filter/hidden rules, in display order.
    pub visible: Vec<usize>,
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
    pending_select_keep: Option<String>,
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
        let mut p = Self {
            path: PathBuf::new(),
            back: Vec::new(),
            forward: Vec::new(),
            entries: Vec::new(),
            visible: Vec::new(),
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
            .map(|e| e.name.clone())
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
        self.path = path.clone();
        self.entries.clear();
        self.visible.clear();
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
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
        let Some(rx) = &self.rx else { return false };
        let mut changed = false;
        let mut finished = None;
        // Bound the work per frame so a giant folder can't stall a frame.
        for _ in 0..64 {
            match rx.try_recv() {
                Ok(ListMsg::Batch(b)) => {
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
        if let Some(state) = finished {
            self.load = state;
            self.rx = None;
            self.dirty_sort = true;
            changed = true;
            // `refresh` keeps the old selection; forget paths that are no longer listed.
            if !self.selected.is_empty() {
                let listed: HashSet<&Path> = self.entries.iter().map(|e| e.path.as_path()).collect();
                self.selected.retain(|p| listed.contains(p.as_path()));
            }
        }
        if self.dirty_sort {
            // While streaming, re-sort at most every ~150ms; always sort when done.
            let due = !self.is_loading() || self.last_sort.elapsed().as_millis() >= 150;
            if due {
                self.resort();
            }
        }
        if changed && !self.is_loading() {
            if let Some(name) = self.pending_select.take() {
                self.select_name(&name);
            }
            if let Some(name) = self.pending_select_keep.take() {
                self.cursor = self
                    .visible
                    .iter()
                    .position(|&i| self.entries[i].name.eq_ignore_ascii_case(&name));
            }
        }
        changed
    }

    pub fn resort(&mut self) {
        let cursor_path = self.cursor_entry().map(|e| e.path.clone());
        listing::sort_entries(&mut self.entries, self.sort);
        self.listing_changed();
        self.dirty_sort = false;
        self.last_sort = Instant::now();
        self.rebuild_visible();
        if let Some(p) = cursor_path {
            self.cursor = self.visible.iter().position(|&i| self.entries[i].path == p);
        }
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
    /// change when the list narrows or widens); a cursor whose item is filtered out moves to
    /// the first listed item. The selection is a set of paths, so it never goes stale.
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
        self.cursor = row_of(self, cursor).or(if self.visible.is_empty() { None } else { Some(0) });
        self.anchor = row_of(self, anchor);
    }

    pub fn rebuild_visible(&mut self) {
        self.listing_changed();
        let plain = self.filter_text.trim().to_lowercase();
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| (self.show_hidden || !e.hidden) && (self.show_system || !e.system))
            .filter(|(_, e)| match (&self.filter_query, &self.filter_error) {
                (Some(q), _) => q.matches_entry(e),
                // Unparseable query: fall back to plain substring so typing never "breaks".
                (None, Some(_)) => e.name.to_lowercase().contains(&plain),
                (None, None) => true,
            })
            .map(|(i, _)| i)
            .collect();
        if let Some(c) = self.cursor {
            if c >= self.visible.len() {
                self.cursor = self.visible.len().checked_sub(1);
            }
        }
    }

    pub fn entry_at(&self, vis: usize) -> Option<&FileEntry> {
        self.visible.get(vis).map(|&i| &self.entries[i])
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

    /// Selected paths, or the cursor item if nothing is selected.
    pub fn action_targets(&self) -> Vec<PathBuf> {
        let sel: Vec<PathBuf> = self
            .selected_entries()
            .iter()
            .map(|e| e.path.clone())
            .collect();
        if sel.is_empty() {
            self.cursor_entry()
                .map(|e| vec![e.path.clone()])
                .unwrap_or_default()
        } else {
            sel
        }
    }

    pub fn select_name(&mut self, name: &str) {
        if let Some(v) = self
            .visible
            .iter()
            .position(|&i| self.entries[i].name.eq_ignore_ascii_case(name))
        {
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
    /// listed. The cursor lands on the first match when it was not on a listed item.
    pub fn type_filter(&mut self, text: &str) {
        // A leading space would only make the filter look empty; spaces inside names count.
        let text = if self.filter_text.is_empty() { text.trim_start() } else { text };
        if text.is_empty() {
            return;
        }
        let mut filter = self.filter_text.clone();
        filter.push_str(text);
        // `set_filter` keeps the cursor on its item, or moves it to the first match.
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
    }
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
        assert_eq!(pane.cursor, Some(0), "the cursor lands on the first match");
        assert!(pane.selected.is_empty(), "filtering never selects");

        pane.cursor = Some(1);
        pane.type_filter("2");
        assert_eq!(visible_names(&pane), ["beach 02.jpg"]);
        assert_eq!(pane.cursor, Some(0), "the cursor stays on its item");
        pane.cursor = Some(0);
        pane.filter_backspace();
        pane.cursor = Some(1);
        pane.type_filter("1");
        assert_eq!(pane.cursor, Some(0), "a filtered-out cursor moves to the first match");
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
    fn clearing_a_filter_with_no_matches_puts_the_cursor_on_the_first_item() {
        let (_folder, mut pane) = listed(&["apple.jpg", "berry.jpg"]);
        pane.cursor = Some(1);
        pane.type_filter("qqqq");
        assert!(pane.visible.is_empty());
        assert_eq!(pane.cursor, None);
        pane.escape();
        assert_eq!(visible_names(&pane).len(), 2);
        assert_eq!(pane.cursor, Some(0), "the cursor is set again, on the first item");
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
}
