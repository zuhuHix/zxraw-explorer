//! Per-pane burst and visual near-duplicate grouping for the browser.
//!
//! Metadata and thumbnail-sized perceptual hashes are computed on a shared bounded worker
//! pool. Results arrive in small batches so the browser can keep drawing ungrouped entries
//! while analysis continues. A global bounded cache reuses capture times and hashes for
//! unchanged path/size/mtime tuples.
//!
//! Burst members may be at most two seconds apart and at most ten seconds from the group's
//! first capture. Near duplicates compare against the group's first dHash and only the
//! preceding 32 capture-ordered neighbors; later members cannot extend a group by matching
//! only its most recent member.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, bounded};
use forge_core::{
    FileEntry,
    model::{EntryKind, MediaKind},
};
use forge_jobs::{CancelToken, Priority, WorkerPool};

use crate::app::ZxrawApp;

const BURST_GAP_SECS: f64 = 2.0;
/// Stops a chain of adjacent shots from turning a long event into one enormous burst.
const BURST_MAX_SPAN_SECS: f64 = 10.0;
const HASH_DISTANCE: u32 = 6;
/// Compare only the preceding 32 images in capture-time order.
const HASH_NEIGHBORS: usize = 32;
const THUMB_EDGE: u32 = 96;
const CACHE_LIMIT: usize = 100_000;
const MESSAGE_CAPACITY: usize = 128;
const RESULTS_PER_BATCH: usize = 32;

/// `Cmd::GroupSimilar`: toggles grouping for the focused pane.
pub fn toggle(app: &mut ZxrawApp) {
    let enabled = app.pane_mut().similar.toggle();
    if enabled {
        app.toast("Grouping bursts and similar photos");
    } else {
        app.toast("Photo grouping off");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    CaptureTimes,
    Hashes,
}

#[derive(Debug, Clone)]
enum GroupDelta {
    Started {
        key: PathBuf,
        first: PathBuf,
        second: PathBuf,
    },
    Added {
        key: PathBuf,
        member: PathBuf,
    },
}

enum WorkerMessage {
    Progress {
        phase: Phase,
        done: usize,
        total: usize,
    },
    Groups(Vec<GroupDelta>),
    Finished,
}

/// State owned by one browser pane. Navigation cancels the old generation but keeps the
/// toggle enabled, so the next folder is grouped after its listing completes.
pub(crate) struct Grouping {
    enabled: bool,
    needs_run: bool,
    running: bool,
    complete: bool,
    phase: Phase,
    done: usize,
    total: usize,
    generation: u64,
    cancel: CancelToken,
    rx: Option<Receiver<WorkerMessage>>,
    groups: HashMap<PathBuf, Vec<PathBuf>>,
    expanded: HashSet<PathBuf>,
    /// Bumped whenever `enabled`, `groups` or `expanded` change, so the grid can cache its
    /// layout instead of rebuilding it every frame.
    revision: u64,
}

impl Default for Grouping {
    fn default() -> Self {
        Self {
            enabled: false,
            needs_run: false,
            running: false,
            complete: false,
            phase: Phase::CaptureTimes,
            done: 0,
            total: 0,
            generation: 0,
            cancel: CancelToken::new(),
            rx: None,
            groups: HashMap::new(),
            expanded: HashSet::new(),
            revision: 0,
        }
    }
}

impl Grouping {
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn running(&self) -> bool {
        self.running
    }

    pub(crate) fn complete(&self) -> bool {
        self.complete
    }

    pub(crate) fn progress(&self) -> (Phase, usize, usize) {
        (self.phase, self.done, self.total)
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn groups(&self) -> &HashMap<PathBuf, Vec<PathBuf>> {
        &self.groups
    }

    pub(crate) fn is_expanded(&self, key: &PathBuf) -> bool {
        self.expanded.contains(key)
    }

    pub(crate) fn set_expanded(&mut self, key: PathBuf, expanded: bool) -> bool {
        if !self.groups.contains_key(&key) {
            return false;
        }
        if expanded {
            self.expanded.insert(key);
        } else {
            self.expanded.remove(&key);
        }
        self.revision += 1;
        true
    }

    pub(crate) fn toggle_expanded(&mut self, key: &PathBuf) -> bool {
        if !self.groups.contains_key(key) {
            return false;
        }
        if !self.expanded.remove(key) {
            self.expanded.insert(key.clone());
        }
        self.revision += 1;
        true
    }

    pub(crate) fn toggle(&mut self) -> bool {
        self.enabled = !self.enabled;
        self.cancel.cancel();
        self.rx = None;
        self.running = false;
        self.complete = false;
        self.done = 0;
        self.total = 0;
        self.groups.clear();
        self.expanded.clear();
        self.generation = self.generation.wrapping_add(1);
        self.revision += 1;
        self.needs_run = self.enabled;
        self.enabled
    }

    pub(crate) fn on_navigation(&mut self) {
        self.cancel.cancel();
        self.rx = None;
        self.running = false;
        self.complete = false;
        self.done = 0;
        self.total = 0;
        self.groups.clear();
        self.expanded.clear();
        self.generation = self.generation.wrapping_add(1);
        self.revision += 1;
        self.needs_run = self.enabled;
    }

    /// Start after a folder listing finishes, then apply only a bounded number of worker
    /// messages per frame. Both work and memory use stay bounded for large folders.
    pub(crate) fn tick(
        &mut self,
        entries: &[FileEntry],
        listing_finished: bool,
        ctx: &egui::Context,
    ) {
        self.poll(ctx);
        if self.enabled && self.needs_run && listing_finished {
            self.start(entries, ctx);
        }
        if self.enabled && !listing_finished {
            ctx.request_repaint_after(Duration::from_millis(80));
        } else if self.running || self.needs_run {
            ctx.request_repaint_after(Duration::from_millis(30));
        }
    }

    fn start(&mut self, entries: &[FileEntry], ctx: &egui::Context) {
        let sources: Vec<Source> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.kind == EntryKind::File
                    && matches!(entry.media_kind(), MediaKind::Image | MediaKind::Raw)
            })
            .map(|(index, entry)| Source {
                index,
                path: entry.path.clone(),
                size: entry.size,
                modified: entry.modified,
            })
            .collect();
        self.needs_run = false;
        self.groups.clear();
        self.expanded.clear();
        self.revision += 1;
        self.done = 0;
        self.total = sources.len();
        self.phase = Phase::CaptureTimes;
        if sources.is_empty() {
            self.running = false;
            self.complete = true;
            return;
        }

        let (tx, rx) = bounded(MESSAGE_CAPACITY);
        self.cancel = CancelToken::new();
        let cancel = self.cancel.clone();
        let worker_ctx = ctx.clone();
        let generation = self.generation;
        self.rx = Some(rx);
        self.running = true;
        self.complete = false;
        let submitted = analysis_pool().submit(Priority::Low, move || {
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                analyze_sources(sources, generation, &cancel, &tx, &worker_ctx);
            }));
            if run.is_err() {
                tracing::error!("similar-photo analysis worker panicked");
                send_message(&tx, WorkerMessage::Finished, &cancel, &worker_ctx);
            }
        });
        if submitted.is_err() {
            self.rx = None;
            self.running = false;
            self.needs_run = true;
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.rx else { return };
        let mut finished = false;
        for _ in 0..16 {
            let Ok(message) = rx.try_recv() else { break };
            match message {
                WorkerMessage::Progress { phase, done, total } => {
                    self.phase = phase;
                    self.done = done;
                    self.total = total;
                }
                WorkerMessage::Groups(deltas) => {
                    self.revision += 1;
                    for delta in deltas {
                        match delta {
                            GroupDelta::Started { key, first, second } => {
                                self.groups.insert(key, vec![first, second]);
                            }
                            GroupDelta::Added { key, member } => {
                                self.groups.entry(key).or_default().push(member);
                            }
                        }
                    }
                }
                WorkerMessage::Finished => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.running = false;
            self.complete = true;
            self.rx = None;
        } else if self.running {
            ctx.request_repaint_after(Duration::from_millis(30));
        }
    }
}

#[derive(Clone)]
struct Source {
    index: usize,
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone)]
struct WorkItem {
    source: Source,
    key: CacheKey,
    capture_time: Option<f64>,
}

fn analyze_sources(
    sources: Vec<Source>,
    generation: u64,
    cancel: &CancelToken,
    tx: &Sender<WorkerMessage>,
    ctx: &egui::Context,
) {
    let total = sources.len();
    let mut work = Vec::with_capacity(total);
    for (position, source) in sources.into_iter().enumerate() {
        if cancel.check().is_err() {
            return;
        }
        let key = CacheKey {
            path: source.path.clone(),
            size: source.size,
            modified: source.modified,
        };
        let mut cached = cache_get(&key).unwrap_or_default();
        if !cached.capture_done {
            let exif_time = forge_media::read_image_metadata(&source.path)
                .ok()
                .and_then(|metadata| {
                    metadata.capture_timestamp.map(|secs| {
                        secs as f64 + metadata.capture_millis.unwrap_or(0) as f64 / 1000.0
                    })
                });
            if cancel.check().is_err() {
                return;
            }
            cached.capture_time = exif_time.or_else(|| source.modified.map(system_time_secs));
            cached.capture_done = true;
            cache_put(key.clone(), cached.clone());
        }
        work.push(WorkItem {
            source,
            key,
            capture_time: cached.capture_time,
        });
        if ((position + 1) % RESULTS_PER_BATCH == 0 || position + 1 == total)
            && !send_message(
                tx,
                WorkerMessage::Progress {
                    phase: Phase::CaptureTimes,
                    done: position + 1,
                    total,
                },
                cancel,
                ctx,
            )
        {
            return;
        }
    }

    work.sort_by(compare_capture_time);
    let mut grouper = IncrementalGrouper::default();
    let mut deltas = Vec::with_capacity(RESULTS_PER_BATCH);
    for (position, item) in work.iter().enumerate() {
        if cancel.check().is_err() {
            return;
        }
        let mut cached = cache_get(&item.key).unwrap_or_default();
        if !cached.hash_done {
            let dhash = hash_thumbnail(&item.source.path, cancel);
            if cancel.check().is_err() {
                return;
            }
            cached.dhash = dhash;
            cached.hash_done = true;
            cache_put(item.key.clone(), cached.clone());
        }
        if let Some(delta) = grouper.push(AnalyzedItem {
            index: item.source.index,
            path: item.source.path.clone(),
            capture_time: item.capture_time,
            dhash: cached.dhash,
        }) {
            deltas.push(delta);
        }
        if deltas.len() >= RESULTS_PER_BATCH || position + 1 == total {
            if !deltas.is_empty()
                && !send_message(
                    tx,
                    WorkerMessage::Groups(std::mem::take(&mut deltas)),
                    cancel,
                    ctx,
                )
            {
                return;
            }
            if !send_message(
                tx,
                WorkerMessage::Progress {
                    phase: Phase::Hashes,
                    done: position + 1,
                    total,
                },
                cancel,
                ctx,
            ) {
                return;
            }
        }
    }
    send_message(tx, WorkerMessage::Finished, cancel, ctx);
    let _ = generation;
}

fn send_message(
    tx: &Sender<WorkerMessage>,
    mut message: WorkerMessage,
    cancel: &CancelToken,
    ctx: &egui::Context,
) -> bool {
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        match tx.send_timeout(message, Duration::from_millis(50)) {
            Ok(()) => {
                ctx.request_repaint();
                return true;
            }
            Err(crossbeam_channel::SendTimeoutError::Timeout(returned)) => message = returned,
            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

fn compare_capture_time(a: &WorkItem, b: &WorkItem) -> std::cmp::Ordering {
    match (a.capture_time, b.capture_time) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
    .then(a.source.index.cmp(&b.source.index))
}

fn system_time_secs(time: SystemTime) -> f64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

fn hash_thumbnail(path: &std::path::Path, cancel: &CancelToken) -> Option<u64> {
    if cancel.check().is_err() {
        return None;
    }
    let image = forge_media::generate_thumbnail(
        path,
        THUMB_EDGE,
        &forge_media::DecodeLimits::default(),
        None,
        1,
        Some(cancel),
    )
    .ok()?;
    let rgba = image::RgbaImage::from_raw(image.width, image.height, image.rgba)?;
    Some(forge_media::analysis::dhash(
        &image::DynamicImage::ImageRgba8(rgba),
    ))
}

fn analysis_pool() -> &'static WorkerPool {
    static POOL: OnceLock<WorkerPool> = OnceLock::new();
    POOL.get_or_init(|| WorkerPool::new("similar-analysis", 2, 8))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Clone, Default)]
struct CachedAnalysis {
    capture_done: bool,
    capture_time: Option<f64>,
    hash_done: bool,
    dhash: Option<u64>,
}

#[derive(Default)]
struct AnalysisCache {
    entries: HashMap<CacheKey, CachedAnalysis>,
    order: VecDeque<CacheKey>,
}

fn cache() -> &'static Mutex<AnalysisCache> {
    static CACHE: OnceLock<Mutex<AnalysisCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(AnalysisCache::default()))
}

fn cache_get(key: &CacheKey) -> Option<CachedAnalysis> {
    cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entries
        .get(key)
        .cloned()
}

fn cache_put(key: CacheKey, value: CachedAnalysis) {
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !cache.entries.contains_key(&key) {
        cache.order.push_back(key.clone());
    }
    cache.entries.insert(key, value);
    while cache.entries.len() > CACHE_LIMIT {
        if let Some(oldest) = cache.order.pop_front() {
            cache.entries.remove(&oldest);
        } else {
            break;
        }
    }
}

#[derive(Debug, Clone)]
struct AnalyzedItem {
    index: usize,
    path: PathBuf,
    capture_time: Option<f64>,
    dhash: Option<u64>,
}

#[derive(Default)]
struct IncrementalGrouper {
    recent: VecDeque<AnalyzedItem>,
    membership: HashMap<usize, usize>,
    groups: HashMap<usize, GroupAnchor>,
}

struct GroupAnchor {
    path: PathBuf,
    capture_time: Option<f64>,
    dhash: Option<u64>,
}

impl IncrementalGrouper {
    fn push(&mut self, item: AnalyzedItem) -> Option<GroupDelta> {
        let prior: Vec<&AnalyzedItem> = self.recent.iter().rev().take(HASH_NEIGHBORS).collect();
        let burst = prior.first().and_then(|last| {
            let (Some(now), Some(before)) = (item.capture_time, last.capture_time) else {
                return None;
            };
            let gap = now - before;
            if !(0.0..=BURST_GAP_SECS).contains(&gap) {
                return None;
            }
            let root = self
                .membership
                .get(&last.index)
                .copied()
                .unwrap_or(last.index);
            let start = match self.groups.get(&root) {
                Some(group) => group.capture_time,
                None => Some(before),
            };
            start
                .filter(|start| now - start <= BURST_MAX_SPAN_SECS)
                .map(|_| root)
        });
        let similar = item.dhash.and_then(|hash| {
            prior.iter().find_map(|neighbor| {
                let root = self
                    .membership
                    .get(&neighbor.index)
                    .copied()
                    .unwrap_or(neighbor.index);
                let anchor_hash = self
                    .groups
                    .get(&root)
                    .and_then(|group| group.dhash)
                    .or(neighbor.dhash)?;
                (forge_media::analysis::hamming(hash, anchor_hash) <= HASH_DISTANCE).then_some(root)
            })
        });

        // A true burst adjacency wins over a visual match to a second nearby group. This
        // keeps each image in one group and makes group membership deterministic.
        let root = burst.or(similar);
        let delta = match root {
            Some(root) if self.groups.contains_key(&root) => {
                self.membership.insert(item.index, root);
                self.groups.get(&root).map(|group| GroupDelta::Added {
                    key: group.path.clone(),
                    member: item.path.clone(),
                })
            }
            Some(root) => {
                let first = self
                    .recent
                    .iter()
                    .find(|prior| prior.index == root)
                    .cloned();
                first.map(|first| {
                    self.membership.insert(first.index, root);
                    self.membership.insert(item.index, root);
                    self.groups.insert(
                        root,
                        GroupAnchor {
                            path: first.path.clone(),
                            capture_time: first.capture_time,
                            dhash: first.dhash,
                        },
                    );
                    GroupDelta::Started {
                        key: first.path.clone(),
                        first: first.path,
                        second: item.path.clone(),
                    }
                })
            }
            None => None,
        };
        self.recent.push_back(item);
        while self.recent.len() > HASH_NEIGHBORS {
            self.recent.pop_front();
        }
        delta
    }
}

/// Pure grouping helper used by the algorithm's boundary and chaining tests.
#[cfg(test)]
fn group_analyzed(items: &[AnalyzedItem]) -> Vec<Vec<usize>> {
    let mut sorted = items.to_vec();
    sorted.sort_by(|a, b| {
        match (a.capture_time, b.capture_time) {
            (Some(a), Some(b)) => a.total_cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then(a.index.cmp(&b.index))
    });
    let mut grouper = IncrementalGrouper::default();
    for item in sorted {
        grouper.push(item);
    }
    let mut groups = HashMap::<usize, Vec<usize>>::new();
    for (member, root) in grouper.membership {
        groups.entry(root).or_default().push(member);
    }
    groups
        .into_values()
        .filter(|members| members.len() > 1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(index: usize, time: Option<f64>, hash: u64) -> AnalyzedItem {
        AnalyzedItem {
            index,
            path: PathBuf::from(format!("{index}.jpg")),
            capture_time: time,
            dhash: Some(hash),
        }
    }

    fn has_group(groups: &[Vec<usize>], members: &[usize]) -> bool {
        let mut expected = members.to_vec();
        expected.sort_unstable();
        groups.iter().any(|group| {
            let mut group = group.clone();
            group.sort_unstable();
            group == expected
        })
    }

    #[test]
    fn burst_gap_includes_two_seconds_and_splits_after_it() {
        let groups = group_analyzed(&[
            item(0, Some(0.0), 0),
            item(1, Some(2.0), u64::MAX),
            item(2, Some(4.01), 0xaaaa_aaaa_aaaa_aaaa),
        ]);
        assert!(has_group(&groups, &[0, 1]));
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn similar_chaining_compares_to_the_first_hash() {
        let groups = group_analyzed(&[
            item(0, Some(0.0), 0),
            item(1, Some(60.0), 0x3f),
            item(2, Some(120.0), 0xfff),
        ]);
        assert!(has_group(&groups, &[0, 1]));
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn empty_single_identical_and_missing_timestamps_are_safe() {
        assert!(group_analyzed(&[]).is_empty());
        assert!(group_analyzed(&[item(0, Some(1.0), 0)]).is_empty());
        let identical = group_analyzed(&[item(0, Some(1.0), 0), item(1, Some(1.0), u64::MAX)]);
        assert!(has_group(&identical, &[0, 1]));

        let missing =
            group_analyzed(&[item(0, Some(1.0), 0), item(1, None, 0xaaaa_aaaa_aaaa_aaaa)]);
        assert!(missing.is_empty());
    }
}
