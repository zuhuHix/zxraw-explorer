//! Finds photo exports from configurable filename rules.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

use egui::{RichText, Ui};
use forge_core::model::{EntryKind, FileEntry, MediaKind, ext_of, media_kind_for_ext};

use crate::app::{Side, Tab, ZxrawApp};
use crate::pane::Pane;

const MAX_QUERY_CACHE: usize = 96;
/// Folder scans waiting for the worker. The newest requests run first, and the oldest are dropped.
const MAX_QUEUED_SCANS: usize = 8;
const MAX_NEARBY_DIRS: usize = 32;
const MAX_DIRECTORY_ENTRIES: usize = 4096;
const MAX_DIRECTORY_DISCOVERY_ENTRIES: usize = 512;
pub const MAX_EXPORT_RULES: usize = 64;

pub const DEFAULT_EXPORT_RULES: [&str; 6] = [
    "{base}-Edit",
    "{base}-Edit-{n}",
    "{base}_edited",
    "{base}-HDR",
    "{base}-Pano",
    "{base}-{n}",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Associations {
    pub originals: Vec<PathBuf>,
    pub exports: Vec<PathBuf>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryState {
    Pending,
    Complete(Associations),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct QueryKey {
    path: PathBuf,
    rules: Vec<String>,
}

#[derive(Default)]
struct QueryCache {
    generation: u64,
    entries: HashMap<QueryKey, QueryState>,
}

impl QueryCache {
    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.entries.clear();
    }

    fn publish_if_current(&mut self, generation: u64, key: QueryKey, state: QueryState) -> bool {
        if self.generation != generation {
            return false;
        }
        self.entries.insert(key, state);
        true
    }
}

static QUERY_CACHE: OnceLock<Mutex<QueryCache>> = OnceLock::new();

fn query_cache() -> &'static Mutex<QueryCache> {
    QUERY_CACHE.get_or_init(|| Mutex::new(QueryCache::default()))
}

pub fn default_rules() -> Vec<String> {
    DEFAULT_EXPORT_RULES
        .iter()
        .map(|rule| (*rule).to_string())
        .collect()
}

pub fn active_rules(rules: &[String]) -> Vec<String> {
    rules.iter().take(MAX_EXPORT_RULES).cloned().collect()
}

/// Validates a filename template. `{base}` is required; `{n}` is an optional numeric token.
pub fn validate_rule(rule: &str) -> Result<(), String> {
    if rule.trim().is_empty() {
        return Err("Rule cannot be empty".into());
    }
    if rule.len() > 160 {
        return Err("Rule is longer than 160 bytes".into());
    }
    if rule.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '/' | '\\' | ':' | '"' | '<' | '>' | '|' | '?' | '*'
            )
    }) {
        return Err("Rules must be filename patterns without path separators".into());
    }
    if rule.match_indices("{base}").count() != 1 {
        return Err("Rule must contain `{base}` exactly once".into());
    }
    if rule.match_indices("{n}").count() > 1 {
        return Err("Rule can contain `{n}` at most once".into());
    }
    let residue = rule.replace("{base}", "").replace("{n}", "");
    if residue.contains('{') || residue.contains('}') {
        return Err("Only `{base}` and `{n}` placeholders are supported".into());
    }
    Ok(())
}

fn all_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// A validated rule, split at `{n}`. `{base}` stays in the text and is filled in per stem.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedRule {
    /// Text before `{n}`, or the whole rule when it has no `{n}`.
    head: String,
    /// Text after `{n}`, when the rule has one.
    tail: Option<String>,
}

impl PreparedRule {
    fn new(rule: &str) -> Option<Self> {
        validate_rule(rule).ok()?;
        // Split before substituting `{base}`, so a literal `{n}` in a filename cannot act as the number token.
        Some(match rule.split_once("{n}") {
            Some((head, tail)) => Self {
                head: head.to_string(),
                tail: Some(tail.to_string()),
            },
            None => Self {
                head: rule.to_string(),
                tail: None,
            },
        })
    }

    fn matches(&self, original_stem: &str, export_stem: &str) -> bool {
        let head = self.head.replace("{base}", original_stem);
        let Some(tail) = &self.tail else {
            return head.eq_ignore_ascii_case(export_stem);
        };
        let tail = tail.replace("{base}", original_stem);
        if export_stem.len() < head.len() + tail.len() {
            return false;
        }
        let Some(found_head) = export_stem.get(..head.len()) else {
            return false;
        };
        let Some(found_tail) = export_stem.get(export_stem.len() - tail.len()..) else {
            return false;
        };
        if !found_head.eq_ignore_ascii_case(&head) || !found_tail.eq_ignore_ascii_case(&tail) {
            return false;
        }
        export_stem
            .get(head.len()..export_stem.len() - tail.len())
            .is_some_and(all_digits)
    }

    /// Lower-cased original stems this rule could have produced `export_stem` from. Every stem
    /// that `matches` accepts is in the list; `matches` still confirms each pair.
    fn original_keys(&self, export_stem: &str) -> Vec<String> {
        let export = export_stem.to_ascii_lowercase();
        let head = self.head.to_ascii_lowercase();
        let mut keys = Vec::new();
        match &self.tail {
            None => {
                // `before{base}after`: the original is whatever lies between the literal parts.
                if let Some((before, after)) = head.split_once("{base}")
                    && let Some(stem) = export
                        .strip_prefix(before)
                        .and_then(|rest| rest.strip_suffix(after))
                {
                    keys.push(stem.to_string());
                }
            }
            Some(tail) => {
                let tail = tail.to_ascii_lowercase();
                if let Some((before, between)) = head.split_once("{base}") {
                    // `before{base}between{n}tail`: the middle reads `stem between digits`.
                    let Some(middle) = export
                        .strip_prefix(before)
                        .and_then(|rest| rest.strip_suffix(tail.as_str()))
                    else {
                        return keys;
                    };
                    for position in 0..=middle.len() {
                        let (Some(stem), Some(digits)) = (
                            middle.get(..position),
                            middle.get(position + between.len()..),
                        ) else {
                            continue;
                        };
                        if middle[position..].starts_with(between) && all_digits(digits) {
                            keys.push(stem.to_string());
                        }
                    }
                } else if let Some((before, after)) = tail.split_once("{base}") {
                    // `head{n}before{base}after`: the middle reads `digits before stem`.
                    let Some(middle) = export
                        .strip_prefix(head.as_str())
                        .and_then(|rest| rest.strip_suffix(after))
                    else {
                        return keys;
                    };
                    for position in 0..=middle.len() {
                        let (Some(digits), Some(stem)) = (
                            middle.get(..position),
                            middle.get(position + before.len()..),
                        ) else {
                            continue;
                        };
                        if middle[position..].starts_with(before) && all_digits(digits) {
                            keys.push(stem.to_string());
                        }
                    }
                }
            }
        }
        keys
    }
}

/// Validates and splits each active rule once, so a command or scan does not repeat that work for every file.
fn prepare_rules(rules: &[String]) -> Vec<PreparedRule> {
    rules
        .iter()
        .take(MAX_EXPORT_RULES)
        .map(String::as_str)
        .filter_map(PreparedRule::new)
        .collect()
}

#[cfg(windows)]
pub(crate) fn has_reparse_attribute(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(crate) fn has_reparse_attribute(_metadata: &fs::Metadata) -> bool {
    false
}

/// `FileType::is_symlink` does not report Windows junctions, so directories also check the reparse attribute.
pub(crate) fn is_link_or_junction(entry: &fs::DirEntry, file_type: &fs::FileType) -> bool {
    file_type.is_symlink()
        || (file_type.is_dir()
            && entry
                .metadata()
                .is_ok_and(|metadata| has_reparse_attribute(&metadata)))
}

fn stem_of(path: &Path) -> Option<String> {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
}

fn is_export_of(original: &Path, export: &Path, rules: &[PreparedRule]) -> bool {
    let (Some(original_stem), Some(export_stem)) = (stem_of(original), stem_of(export)) else {
        return false;
    };
    rules
        .iter()
        .any(|rule| rule.matches(&original_stem, &export_stem))
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}

fn is_media_file(path: &Path) -> bool {
    let ext = ext_of(path);
    matches!(
        media_kind_for_ext(&ext),
        MediaKind::Image | MediaKind::Raw | MediaKind::Video
    )
}

fn nearby_directories(path: &Path) -> Vec<PathBuf> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let mut directories = vec![parent.to_path_buf()];

    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.take(MAX_DIRECTORY_DISCOVERY_ENTRIES).flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir()
                && !is_link_or_junction(&entry, &file_type)
                && matches!(
                    entry
                        .file_name()
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .as_str(),
                    "export" | "exports"
                )
            {
                directories.push(entry.path());
            }
        }
    }

    // A file stored in an Export(s) folder is commonly paired with originals one level up.
    if parent.file_name().is_some_and(|name| {
        matches!(
            name.to_string_lossy().to_ascii_lowercase().as_str(),
            "export" | "exports"
        )
    }) && let Some(grandparent) = parent.parent()
    {
        directories.push(grandparent.to_path_buf());
    }

    // Export sets also commonly use a sibling folder such as RAW and JPEG.
    if let Some(grandparent) = parent.parent()
        && let Ok(entries) = fs::read_dir(grandparent)
    {
        for entry in entries.take(MAX_DIRECTORY_DISCOVERY_ENTRIES).flatten() {
            if entry.path() == parent {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() && !is_link_or_junction(&entry, &file_type) {
                directories.push(entry.path());
                if directories.len() >= MAX_NEARBY_DIRS {
                    break;
                }
            }
        }
    }

    let mut seen = HashSet::new();
    directories.retain(|directory| seen.insert(normalized_path(directory)));
    directories.truncate(MAX_NEARBY_DIRS);
    directories
}

fn scan_for(path: &Path, rules: &[String]) -> Associations {
    let rules = prepare_rules(rules);
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut entries_seen = 0usize;
    let mut truncated = false;

    for directory in nearby_directories(path) {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries {
            if entries_seen >= MAX_DIRECTORY_ENTRIES {
                truncated = true;
                break;
            }
            entries_seen += 1;
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() || is_link_or_junction(&entry, &file_type) {
                continue;
            }
            let candidate = entry.path();
            if !is_media_file(&candidate) || !seen.insert(normalized_path(&candidate)) {
                continue;
            }
            candidates.push(candidate);
        }
        if truncated {
            break;
        }
    }

    let mut associations = Associations {
        truncated,
        ..Associations::default()
    };
    let mut originals = HashSet::new();
    let mut exports = HashSet::new();
    for candidate in candidates {
        if normalized_path(&candidate) == normalized_path(path) {
            continue;
        }
        if is_export_of(path, &candidate, &rules) {
            if exports.insert(normalized_path(&candidate)) {
                associations.exports.push(candidate);
            }
        } else if is_export_of(&candidate, path, &rules)
            && originals.insert(normalized_path(&candidate))
        {
            associations.originals.push(candidate);
        }
    }
    associations.originals.sort();
    associations.exports.sort();
    associations
}

/// Forgets association scans (a folder changed, so exports may have appeared or disappeared).
/// Results from scans already in flight are discarded by their cache generation.
pub fn invalidate_cache() {
    query_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .invalidate();
}

struct ScanJob {
    key: QueryKey,
    generation: u64,
    repaint: egui::Context,
}

/// Queued scans. The worker takes the newest job first, so a folder the user is looking at is
/// not stuck behind scans for files they already passed.
static SCAN_QUEUE: Mutex<VecDeque<ScanJob>> = Mutex::new(VecDeque::new());
static SCAN_READY: Condvar = Condvar::new();
static SCAN_WORKER: OnceLock<Result<(), String>> = OnceLock::new();

/// Pushes `item` and returns the oldest items pushed out to keep the queue within `limit`.
fn push_bounded<T>(queue: &mut VecDeque<T>, item: T, limit: usize) -> Vec<T> {
    queue.push_back(item);
    let mut dropped = Vec::new();
    while queue.len() > limit {
        dropped.extend(queue.pop_front());
    }
    dropped
}

fn enqueue_scan(job: ScanJob) -> Result<(), String> {
    SCAN_WORKER
        .get_or_init(|| {
            std::thread::Builder::new()
                .name("export-association-scan".into())
                .spawn(scan_worker)
                .map(|_handle| ())
                .map_err(|error| error.to_string())
        })
        .clone()?;
    let dropped = push_bounded(
        &mut SCAN_QUEUE.lock().unwrap_or_else(|error| error.into_inner()),
        job,
        MAX_QUEUED_SCANS,
    );
    SCAN_READY.notify_one();
    if !dropped.is_empty() {
        // Dropped requests were never run, so forget them; a later frame can request them again.
        let mut cache = query_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for job in dropped {
            if cache.generation == job.generation {
                cache.entries.remove(&job.key);
            }
        }
    }
    Ok(())
}

fn scan_worker() {
    loop {
        let job = {
            let mut queue = SCAN_QUEUE.lock().unwrap_or_else(|error| error.into_inner());
            loop {
                if let Some(job) = queue.pop_back() {
                    break job;
                }
                queue = SCAN_READY
                    .wait(queue)
                    .unwrap_or_else(|error| error.into_inner());
            }
        };
        let state =
            match panic::catch_unwind(AssertUnwindSafe(|| scan_for(&job.key.path, &job.key.rules)))
            {
                Ok(associations) => QueryState::Complete(associations),
                Err(_) => QueryState::Failed("the scan stopped unexpectedly".into()),
            };
        let mut cache = query_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cache.publish_if_current(job.generation, job.key, state);
        job.repaint.request_repaint();
    }
}

pub fn request_for(path: &Path, rules: &[String], ctx: &egui::Context) {
    let key = QueryKey {
        path: path.to_path_buf(),
        rules: active_rules(rules),
    };
    let generation = {
        let mut cache = query_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if cache.entries.contains_key(&key) {
            return;
        }
        if cache.entries.len() >= MAX_QUERY_CACHE
            && let Some(oldest) = cache
                .entries
                .iter()
                .find_map(|(key, state)| (*state != QueryState::Pending).then_some(key.clone()))
        {
            cache.entries.remove(&oldest);
        }
        let generation = cache.generation;
        cache.entries.insert(key.clone(), QueryState::Pending);
        generation
    };

    let job = ScanJob {
        key: key.clone(),
        generation,
        repaint: ctx.clone(),
    };
    if let Err(error) = enqueue_scan(job) {
        let mut cache = query_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cache.publish_if_current(generation, key, QueryState::Failed(error));
    }
}

pub fn cached_for(path: &Path, rules: &[String]) -> Option<QueryState> {
    query_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .entries
        .get(&QueryKey {
            path: path.to_path_buf(),
            rules: active_rules(rules),
        })
        .cloned()
}

/// Related originals/exports in the preview panel.
pub fn panel(app: &mut ZxrawApp, ui: &mut Ui, entry: &FileEntry) {
    if entry.kind != EntryKind::File || !is_media_file(&entry.path) {
        return;
    }
    let rules = active_rules(&app.settings.export_rules);
    request_for(&entry.path, &rules, ui.ctx());
    let state = cached_for(&entry.path, &rules);
    let associations = match state {
        Some(QueryState::Complete(associations)) => associations,
        Some(QueryState::Pending) | None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Checking related originals and exports…");
            });
            return;
        }
        Some(QueryState::Failed(error)) => {
            ui.label(RichText::new(format!("Related files unavailable: {error}")).weak());
            return;
        }
    };

    let mut action = None;
    if !associations.originals.is_empty() {
        ui.separator();
        ui.label(RichText::new("Original(s)").strong());
        for path in &associations.originals {
            if let Some(row_action) = related_row(ui, &entry.path, path) {
                action = Some(row_action);
            }
        }
    }
    if !associations.exports.is_empty() {
        ui.separator();
        ui.label(RichText::new("Exports").strong());
        for path in &associations.exports {
            if let Some(row_action) = related_row(ui, &entry.path, path) {
                action = Some(row_action);
            }
        }
    }
    if associations.truncated {
        ui.label(
            RichText::new("Nearby-folder scan reached its file limit.")
                .small()
                .weak(),
        );
    }

    if let Some((kind, path)) = action {
        match kind {
            RelatedActionKind::Open => crate::actions::open_file(app, &path),
            RelatedActionKind::Reveal => app.ops.spawn_shell("Reveal related file", move || {
                forge_win::reveal_in_explorer(&path)
            }),
            RelatedActionKind::ShowInFolder => app.navigate(path),
        }
    }
}

#[derive(Clone, Copy)]
enum RelatedActionKind {
    Open,
    Reveal,
    ShowInFolder,
}

fn related_row(ui: &mut Ui, current: &Path, path: &Path) -> Option<(RelatedActionKind, PathBuf)> {
    let mut action = None;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    ui.horizontal_wrapped(|ui| {
        ui.label(name);
        if ui.small_button("Open").clicked() {
            action = Some(RelatedActionKind::Open);
        }
        if ui.small_button("Reveal").clicked() {
            action = Some(RelatedActionKind::Reveal);
        }
        let label = if path.parent() == current.parent() {
            "Select in pane"
        } else {
            "Show in folder"
        };
        if ui.small_button(label).clicked() {
            action = Some(RelatedActionKind::ShowInFolder);
        }
    });
    action.map(|kind| (kind, path.to_path_buf()))
}

/// `Cmd::SelectOriginals` selects matches visible in the current pane.
pub fn select_originals(app: &mut ZxrawApp) {
    select_counterparts(app, false);
}

/// `Cmd::SelectExports` selects matches visible in the current pane.
pub fn select_exports(app: &mut ZxrawApp) {
    select_counterparts(app, true);
}

/// The latest select command. Matching runs on its own thread, and `poll` applies the result.
struct PendingSelection {
    id: u64,
    origin: SelectionOrigin,
    want_exports: bool,
    /// `None` while matching runs.
    matches: Option<Vec<PathBuf>>,
}

#[derive(Clone)]
struct SelectionOrigin {
    tab_index: usize,
    side: Side,
    folder: PathBuf,
    listing_rev: u64,
}

static PENDING_SELECTION: Mutex<Option<PendingSelection>> = Mutex::new(None);
static NEXT_SELECTION_ID: AtomicU64 = AtomicU64::new(1);

fn pending_selection() -> MutexGuard<'static, Option<PendingSelection>> {
    PENDING_SELECTION
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Whether a select command is still matching, or has a result that `poll` has not applied yet.
pub fn selection_pending() -> bool {
    pending_selection().is_some()
}

fn select_counterparts(app: &mut ZxrawApp, want_exports: bool) {
    let rules = active_rules(&app.settings.export_rules);
    let tab_index = app.active;
    let tab = app.tab();
    let side = if tab.right.is_some() {
        tab.focus
    } else {
        Side::Left
    };
    let pane = pane_on_side(tab, side).expect("the focused pane exists");
    let origin = SelectionOrigin {
        tab_index,
        side,
        folder: pane.path.clone(),
        listing_rev: pane.listing_rev(),
    };
    let targets = pane
        .action_targets()
        .into_iter()
        .filter(|target| is_media_file(target))
        .collect::<Vec<_>>();
    let candidates = pane
        .visible
        .iter()
        .filter_map(|&index| pane.entries.get(index))
        .filter(|entry| entry.kind == EntryKind::File && is_media_file(&entry.path))
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    // A new command replaces an unfinished one; the older thread then finds its id is gone.
    let id = NEXT_SELECTION_ID.fetch_add(1, Ordering::Relaxed);
    *pending_selection() = Some(PendingSelection {
        id,
        origin,
        want_exports,
        matches: None,
    });
    let repaint = app.ops.ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("export-association-select".into())
        .spawn(move || {
            // A panic must still finish the command, or it would stay pending forever.
            let matches = std::panic::catch_unwind(|| {
                let rules = prepare_rules(&rules);
                match_counterparts(&targets, &candidates, &rules, want_exports)
            })
            .unwrap_or_else(|_| {
                tracing::error!("export-association matching panicked");
                Vec::new()
            });
            finish_selection(id, matches);
            repaint.request_repaint();
        });
    if let Err(error) = spawned {
        *pending_selection() = None;
        app.toast_err(format!("Could not start the selection: {error}"));
    }
}

fn finish_selection(id: u64, matches: Vec<PathBuf>) {
    let mut pending = pending_selection();
    if let Some(pending) = pending.as_mut().filter(|pending| pending.id == id) {
        pending.matches = Some(matches);
    }
}

/// Applies a finished select command, on the first frame after it finishes. Returns whether
/// the pane changed.
pub fn poll(app: &mut ZxrawApp) -> bool {
    let ready = {
        let mut pending = pending_selection();
        if pending
            .as_ref()
            .is_some_and(|pending| pending.matches.is_some())
        {
            pending.take()
        } else {
            None
        }
    };
    let Some(PendingSelection {
        origin,
        want_exports,
        matches: Some(matches),
        ..
    }) = ready
    else {
        return false;
    };
    let count = match apply_selection_to_origin(&mut app.tabs, &origin, &matches) {
        Ok(count) => count,
        Err(SelectionApplyError::PaneGone) => {
            app.toast("Selection cancelled: its originating tab or pane is no longer available");
            return true;
        }
        Err(SelectionApplyError::PaneChanged) => {
            app.toast("Selection cancelled: its originating pane changed while matching");
            return true;
        }
    };
    if count == 0 {
        let what = if want_exports { "exports" } else { "originals" };
        app.show_preview = true;
        app.toast(format!(
            "No matching {what} are in this folder. Nearby matches appear in the preview with Show in folder."
        ));
        return true;
    }
    app.toast(format!("Selected {count} counterpart(s)"));
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionApplyError {
    PaneGone,
    PaneChanged,
}

fn pane_on_side(tab: &Tab, side: Side) -> Option<&Pane> {
    match side {
        Side::Left => Some(&tab.left),
        Side::Right => tab.right.as_ref(),
    }
}

fn pane_on_side_mut(tab: &mut Tab, side: Side) -> Option<&mut Pane> {
    match side {
        Side::Left => Some(&mut tab.left),
        Side::Right => tab.right.as_mut(),
    }
}

fn apply_selection_to_origin(
    tabs: &mut [Tab],
    origin: &SelectionOrigin,
    matches: &[PathBuf],
) -> Result<usize, SelectionApplyError> {
    let pane = tabs
        .get(origin.tab_index)
        .and_then(|tab| pane_on_side(tab, origin.side))
        .ok_or(SelectionApplyError::PaneGone)?;
    if pane.path != origin.folder || pane.listing_rev() != origin.listing_rev {
        return Err(SelectionApplyError::PaneChanged);
    }

    // Only entries still listed are selected, in case the folder changed while matching ran.
    let matched = matches
        .iter()
        .map(|path| normalized_path(path))
        .collect::<HashSet<_>>();
    let mut found = Vec::new();
    let mut cursor = None;
    for (position, &index) in pane.visible.iter().enumerate() {
        let Some(entry) = pane.entries.get(index) else {
            continue;
        };
        if matched.contains(&normalized_path(&entry.path)) {
            found.push(entry.path.clone());
            cursor.get_or_insert(position);
        }
    }
    if found.is_empty() {
        return Ok(0);
    }
    let count = found.len();
    let pane = tabs
        .get_mut(origin.tab_index)
        .and_then(|tab| pane_on_side_mut(tab, origin.side))
        .ok_or(SelectionApplyError::PaneGone)?;
    pane.selected.clear();
    pane.selected.extend(found);
    pane.cursor = cursor;
    pane.scroll_to_cursor = true;
    Ok(count)
}

/// Pairs `targets` with `candidates` through an index of original stems. Each export-side file
/// is indexed once by the originals its rules could come from, and each original-side file
/// looks up its own stem, so the work does not grow with the number of pairs. Every pair the
/// index proposes is still confirmed by `PreparedRule::matches`.
///
/// Returns the candidates that are counterparts of a target, in listing order.
fn match_counterparts(
    targets: &[PathBuf],
    candidates: &[PathBuf],
    rules: &[PreparedRule],
    want_exports: bool,
) -> Vec<PathBuf> {
    // Targets are the originals when selecting exports, and candidates are the originals otherwise.
    let (originals, exports) = if want_exports {
        (targets, candidates)
    } else {
        (candidates, targets)
    };
    let export_stems = exports.iter().map(|path| stem_of(path)).collect::<Vec<_>>();
    let mut index: HashMap<String, Vec<usize>> = HashMap::new();
    for (position, export_stem) in export_stems.iter().enumerate() {
        let Some(export_stem) = export_stem else {
            continue;
        };
        let mut keys = rules
            .iter()
            .flat_map(|rule| rule.original_keys(export_stem))
            .collect::<Vec<_>>();
        keys.sort_unstable();
        keys.dedup();
        for key in keys {
            index.entry(key).or_default().push(position);
        }
    }

    let mut matched = HashSet::new();
    for original in originals {
        let Some(original_stem) = stem_of(original) else {
            continue;
        };
        let Some(positions) = index.get(&original_stem.to_ascii_lowercase()) else {
            continue;
        };
        for &position in positions {
            let export = &exports[position];
            if normalized_path(original) == normalized_path(export) {
                continue;
            }
            let Some(export_stem) = &export_stems[position] else {
                continue;
            };
            if rules
                .iter()
                .any(|rule| rule.matches(&original_stem, export_stem))
            {
                matched.insert(normalized_path(if want_exports {
                    export
                } else {
                    original
                }));
            }
        }
    }
    candidates
        .iter()
        .filter(|candidate| matched.contains(&normalized_path(candidate)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"fixture").unwrap();
    }

    fn prepared(rules: &[String]) -> Vec<PreparedRule> {
        prepare_rules(rules)
    }

    /// The full pairwise comparison that `match_counterparts` replaces. Its results are the reference.
    fn full_comparison(
        targets: &[PathBuf],
        candidates: &[PathBuf],
        rules: &[PreparedRule],
        want_exports: bool,
    ) -> Vec<PathBuf> {
        candidates
            .iter()
            .filter(|candidate| {
                targets.iter().any(|target| {
                    if normalized_path(candidate) == normalized_path(target) {
                        return false;
                    }
                    if want_exports {
                        is_export_of(target, candidate, rules)
                    } else {
                        is_export_of(candidate, target, rules)
                    }
                })
            })
            .cloned()
            .collect()
    }

    #[test]
    fn default_rules_match_all_supported_export_names() {
        let original = Path::new("IMG_1234.CR3");
        let rules = prepared(&default_rules());
        for export in [
            "IMG_1234-Edit.jpg",
            "IMG_1234-Edit-2.jpg",
            "IMG_1234_edited.tif",
            "IMG_1234-HDR.jpg",
            "IMG_1234-Pano.tif",
            "IMG_1234-3.jpg",
        ] {
            assert!(
                is_export_of(original, Path::new(export), &rules),
                "{export}"
            );
        }
    }

    #[test]
    fn matching_is_exact_and_numeric_tokens_do_not_accept_text() {
        let rules = prepared(&default_rules());
        assert!(!is_export_of(
            Path::new("IMG_12345.CR3"),
            Path::new("IMG_1234-Edit.jpg"),
            &rules
        ));
        assert!(!is_export_of(
            Path::new("IMG_1234.CR3"),
            Path::new("IMG_1234-Edit-copy.jpg"),
            &rules
        ));
        assert!(!is_export_of(
            Path::new("IMG_1234.CR3"),
            Path::new("IMG_12345.jpg"),
            &rules
        ));
    }

    #[test]
    fn numeric_token_needs_digits_and_literal_braces_in_names_are_plain_text() {
        let rules = prepared(&default_rules());
        for export in ["IMG_1234--1.jpg", "IMG_1234-+1.jpg", "IMG_1234-.jpg"] {
            assert!(
                !is_export_of(Path::new("IMG_1234.CR3"), Path::new(export), &rules),
                "{export}"
            );
        }
        // A literal `{n}` in the original name must not be read as the number token.
        assert!(is_export_of(
            Path::new("IMG{n}.CR3"),
            Path::new("IMG{n}-7.jpg"),
            &rules
        ));
        assert!(!is_export_of(
            Path::new("IMG{n}.CR3"),
            Path::new("IMG-7.jpg"),
            &rules
        ));
    }

    #[test]
    fn invalid_rules_are_rejected_without_interpreting_filename_text() {
        for rule in [
            "",
            "Edit",
            "{base}-{unknown}",
            "{base}/{n}",
            "{base}-{n}-{n}",
            "{base}{base}",
        ] {
            assert!(validate_rule(rule).is_err(), "{rule}");
        }
        let rules = prepared(&["{base}.*".to_string()]);
        assert!(rules.is_empty());
        assert!(!is_export_of(
            Path::new("photo.jpg"),
            Path::new("photo.anything.jpg"),
            &rules
        ));
    }

    #[test]
    fn index_proposes_every_pair_a_full_comparison_accepts() {
        let temp = TempDir::new().unwrap();
        let folder = temp.path();
        let names = [
            "IMG_1234.CR3",
            "IMG_1234-Edit.jpg",
            "IMG_1234-Edit-2.jpg",
            "IMG_1234-Edit-x.jpg",
            "IMG_1234-9.jpg",
            "IMG_1234_edited.TIF",
            "IMG_1234-HDR.jpg",
            "IMG_12-3.jpg",
            "IMG_12.jpg",
            "IMG{n}.CR3",
            "IMG{n}-7.jpg",
            "IMG-7.jpg",
            "7-IMG_1234.jpg",
            "IMG_1234-12.jpg",
            "IMG_1234-12-Edit.jpg",
            "x-IMG_1234y5z.jpg",
            "xIMG_1234yz.jpg",
            "IMG_1234y5zHDR.jpg",
            "IMG_1234-1-Edit.jpg",
            "IMG_1234-1-1.jpg",
        ];
        let files = names
            .iter()
            .map(|name| folder.join(name))
            .collect::<Vec<_>>();
        // Extra rules put `{n}` before and after `{base}`, adjoin them, and include one that is invalid.
        let mut rule_text = default_rules();
        rule_text.extend(
            [
                "{n}-{base}",
                "{base}{n}",
                "x{base}y{n}z",
                "{n}{base}-HDR",
                "{base}-{n}-{n}",
            ]
            .map(String::from),
        );
        let rules = prepared(&rule_text);
        for want_exports in [true, false] {
            for target in &files {
                let targets = std::slice::from_ref(target);
                assert_eq!(
                    match_counterparts(targets, &files, &rules, want_exports),
                    full_comparison(targets, &files, &rules, want_exports),
                    "target {} (exports: {want_exports})",
                    target.display()
                );
            }
            assert_eq!(
                match_counterparts(&files, &files, &rules, want_exports),
                full_comparison(&files, &files, &rules, want_exports),
            );
        }
    }

    #[test]
    fn large_selection_finishes_quickly_with_the_same_counterparts() {
        // 2,048 originals and their exports: 4,096 candidates, selected against 256 targets.
        let originals = (0..2048)
            .map(|index| PathBuf::from(format!("C:\\shoot\\IMG_{index:04}.CR3")))
            .collect::<Vec<_>>();
        let exports = (0..2048)
            .map(|index| PathBuf::from(format!("C:\\shoot\\IMG_{index:04}-Edit.jpg")))
            .collect::<Vec<_>>();
        let candidates = originals
            .iter()
            .chain(&exports)
            .cloned()
            .collect::<Vec<_>>();
        let mut rule_text = default_rules();
        rule_text.extend((0..MAX_EXPORT_RULES - 6).map(|index| format!("{{base}}-r{index}-{{n}}")));
        let rules = prepared(&rule_text);
        assert_eq!(rules.len(), MAX_EXPORT_RULES);

        let started = std::time::Instant::now();
        let mut found = match_counterparts(&originals[..256], &candidates, &rules, true);
        let mut from_exports = match_counterparts(&exports[..256], &candidates, &rules, false);
        let elapsed = started.elapsed();

        found.sort();
        from_exports.sort();
        assert_eq!(found, exports[..256].to_vec());
        assert_eq!(from_exports, originals[..256].to_vec());
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "matching took {elapsed:?}"
        );
    }

    #[test]
    fn push_bounded_drops_the_oldest_requests_and_keeps_the_newest() {
        let mut queue = VecDeque::new();
        let mut dropped = Vec::new();
        for item in 0..20 {
            dropped.extend(push_bounded(&mut queue, item, MAX_QUEUED_SCANS));
        }
        assert_eq!(dropped, (0..12).collect::<Vec<_>>());
        assert_eq!(queue.len(), MAX_QUEUED_SCANS);
        // The worker takes from the back, so the newest request runs first.
        assert_eq!(queue.pop_back(), Some(19));
    }

    #[test]
    fn invalidated_pending_scan_cannot_publish_and_a_new_generation_can() {
        let mut cache = QueryCache::default();
        let key = QueryKey {
            path: PathBuf::from(r"C:\shoot\IMG_1234.jpg"),
            rules: default_rules(),
        };
        let started_generation = cache.generation;
        cache.entries.insert(key.clone(), QueryState::Pending);

        cache.invalidate();

        assert!(cache.entries.is_empty());
        assert!(!cache.publish_if_current(
            started_generation,
            key.clone(),
            QueryState::Complete(Associations::default())
        ));
        assert!(!cache.entries.contains_key(&key));

        let next_generation = cache.generation;
        cache.entries.insert(key.clone(), QueryState::Pending);
        assert!(cache.publish_if_current(
            next_generation,
            key.clone(),
            QueryState::Complete(Associations::default())
        ));
        assert_eq!(
            cache.entries.get(&key),
            Some(&QueryState::Complete(Associations::default()))
        );
    }

    #[test]
    fn selection_result_updates_its_originating_pane_only() {
        fn file(path: PathBuf) -> FileEntry {
            FileEntry {
                name: path.file_name().unwrap().to_string_lossy().into_owned(),
                ext: ext_of(&path),
                path,
                kind: EntryKind::File,
                size: 1,
                modified: None,
                created: None,
                hidden: false,
                readonly: false,
                system: false,
            }
        }

        fn pane(folder: &Path, entries: &[FileEntry]) -> Pane {
            let mut pane = Pane::new(
                folder.to_path_buf(),
                forge_core::listing::SortSpec::default(),
                crate::settings::ViewMode::Details,
            );
            pane.entries = entries.to_vec();
            pane.rebuild_visible();
            pane
        }

        let temp = TempDir::new().unwrap();
        let original = temp.path().join("IMG_1234.jpg");
        let export = temp.path().join("IMG_1234-Edit.jpg");
        let existing_right_selection = temp.path().join("IMG_9999.jpg");
        let entries = vec![
            file(original.clone()),
            file(export.clone()),
            file(existing_right_selection.clone()),
        ];
        let mut left = pane(temp.path(), &entries);
        left.selected.insert(original);
        let mut right = pane(temp.path(), &entries);
        right.selected.insert(existing_right_selection.clone());
        let listing_rev = left.listing_rev();
        let mut tabs = vec![Tab {
            left,
            right: Some(right),
            focus: Side::Right,
        }];
        let origin = SelectionOrigin {
            tab_index: 0,
            side: Side::Left,
            folder: temp.path().to_path_buf(),
            listing_rev,
        };

        let count = apply_selection_to_origin(&mut tabs, &origin, std::slice::from_ref(&export))
            .expect("the originating pane still matches");

        assert_eq!(count, 1);
        assert_eq!(tabs[0].left.selected, HashSet::from([export]));
        assert_eq!(
            tabs[0].right.as_ref().unwrap().selected,
            HashSet::from([existing_right_selection])
        );
    }

    #[test]
    fn association_scan_finds_exports_in_export_and_sibling_folders() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let original = root.join("RAW").join("IMG_1234.CR3");
        let export_subfolder = root.join("RAW").join("Exports").join("IMG_1234-Edit.jpg");
        let export_sibling = root.join("JPEG").join("IMG_1234-HDR.jpg");
        touch(&original);
        touch(&export_subfolder);
        touch(&export_sibling);

        let result = scan_for(&original, &default_rules());
        assert!(result.exports.contains(&export_subfolder));
        assert!(result.exports.contains(&export_sibling));
    }

    #[test]
    fn export_path_resolves_its_original_in_a_sibling_folder() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let original = root.join("RAW").join("IMG_1234.CR3");
        let export = root.join("JPEG").join("IMG_1234-Edit.jpg");
        touch(&original);
        touch(&export);

        let result = scan_for(&export, &default_rules());
        assert_eq!(result.originals, vec![original]);
    }
}
