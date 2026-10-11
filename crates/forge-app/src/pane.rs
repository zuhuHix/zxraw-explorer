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
        let cursor = self.cursor_entry().map(|e| e.name.clone());
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

    pub fn set_filter(&mut self, text: String) {
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
                ext: "jpg".into(),
            },
        ];
        pane.rebuild_visible();

        let members = vec![hidden.clone(), shown.clone()];
        assert_eq!(pane.visible_group_paths(&members), vec![shown]);
    }
}
