//! Recursive folder totals for the Size column, measured in the background.
//!
//! The panes say which folders are on screen ([`FolderSizes::want`]) and read totals with
//! [`FolderSizes::get`], which is one lock and a hash lookup, so it can run for every row
//! every frame. A small pool of worker threads measures wanted folders in on-screen order
//! with [`forge_core::dir_size::dir_totals`], and every sub-folder a walk completes is kept,
//! so opening a sub-folder later shows its sizes at once.
//!
//! A recursive change watch on each local folder a pane shows (while some of its folders are
//! wanted) decides which kept totals can be trusted: while it covers a folder, any change
//! below it arrives as an event, so a total measured by a walk that *started* under the
//! watch stays right until an event says otherwise. Later walks skip such trusted sub-trees
//! and only re-read the branch that changed. A walk that started before its watch was set up
//! may have missed a change, so its result is shown as stale and measured again once the
//! watch is up (walks wait briefly for a watch being set up, so this is rare). Where no
//! watch covers a folder (the watch failed, or a network folder: a recursive watch on a
//! share is expensive and unreliable), totals are never reused by walks and only count as
//! fresh for a while, after which a wanted folder is measured again.
//!
//! Busy trees (a drive root's AppData, caches, the pagefile) send a steady stream of change
//! events. An event only marks its folder and ancestors (O(depth)); a wanted folder is then
//! measured at most every few seconds, so a busy volume costs little while idle.
//!
//! Nothing here touches the filesystem on the UI thread: `want`, `get` and `invalidate*`
//! only update maps (the one exception, telling a mapped network drive from a local one, asks
//! `GetDriveTypeW` once per drive letter, which answers locally).

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use forge_core::CoreError;
use forge_core::dir_size::{self, DirTotals, SizeCache};
use forge_jobs::CancelToken;
use notify::{RecursiveMode, Watcher};
use parking_lot::{Condvar, Mutex, MutexGuard};

/// Quiet period after the last change event before an affected folder is measured again.
const DEBOUNCE: Duration = Duration::from_millis(500);
/// A folder is measured at most this often while changes keep arriving (a long copy): the
/// total then moves every couple of seconds instead of waiting for the copy to end.
const MIN_GAP: Duration = Duration::from_secs(2);
/// The gap for busy folders: those below a watched drive root, or with many changes in one
/// debounce window. Keeps a busy volume from turning into constant re-measuring.
const BUSY_GAP: Duration = Duration::from_secs(5);
/// Changes within one debounce window that make a folder busy.
const BUSY_EVENTS: u32 = 20;
/// Network totals are never trusted (no watch); a wanted one older than this is measured
/// again.
const NETWORK_MAX_AGE: Duration = Duration::from_secs(5 * 60);
/// Local totals no watch covers (the watch failed) count as fresh this long.
const UNWATCHED_MAX_AGE: Duration = Duration::from_secs(30);
/// Partial totals (a folder could not be read) count as fresh this long: a folder that becomes
/// readable (its permissions fixed) sends no event to the watch, so it is simply read again.
/// Only the folders on the way to the unreadable ones are listed again; complete branches
/// are reused. A folder that stays unreadable (`C:\System Volume Information`) gives the same
/// partial total each time: each repeat doubles this, up to [`PARTIAL_MAX_BACKOFF`], until a
/// change event below the folder or a refresh (F5) starts over.
const PARTIAL_MAX_AGE: Duration = Duration::from_secs(30);
/// The longest a repeated, identical partial total stays fresh.
const PARTIAL_MAX_BACKOFF: Duration = Duration::from_secs(10 * 60);
/// Walks below a root whose watch is being set up wait this long for it, so their result
/// can be trusted instead of having to be measured twice.
const REGISTER_GRACE: Duration = Duration::from_secs(1);
/// Kept totals; past this, untrusted and then the oldest entries are dropped.
const CACHE_CAP: usize = 200_000;
/// Network folders measured at once. Each walk lists folders one round trip at a time, so a
/// couple in flight hide latency without flooding the server (see `forge_core::netpolicy`).
const NETWORK_JOBS: usize = forge_core::netpolicy::NETWORK_SCAN_WORKERS;
/// Changes remembered per running walk. Past this the walk's whole result counts as stale.
const TOUCHED_CAP: usize = 256;

/// What the Size column shows for a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderSize {
    /// Sizes are off for this folder (setting off, network folder with network sizes off).
    Off,
    /// Being measured. `stale`: an older total to show meanwhile (weak colour).
    Pending {
        stale: Option<DirTotals>,
    },
    Known(DirTotals),
}

/// The walk, injectable so tests can use slow or counting fakes.
type WalkFn = dyn Fn(&Path, &dyn SizeCache, usize, &CancelToken) -> forge_core::Result<DirTotals>
    + Send
    + Sync;

/// Test hook run by the registrar before it watches a root: may block (a slow
/// registration) and may decide the outcome (`Some(ok)`) instead of the real watch (`None`).
type WatchHook = dyn Fn(&Path) -> Option<bool> + Send + Sync;

#[derive(Debug, Clone, Copy)]
struct Timing {
    debounce: Duration,
    min_gap: Duration,
    busy_gap: Duration,
    network_max_age: Duration,
    unwatched_max_age: Duration,
    partial_max_age: Duration,
    partial_max_backoff: Duration,
    register_grace: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            debounce: DEBOUNCE,
            min_gap: MIN_GAP,
            busy_gap: BUSY_GAP,
            network_max_age: NETWORK_MAX_AGE,
            unwatched_max_age: UNWATCHED_MAX_AGE,
            partial_max_age: PARTIAL_MAX_AGE,
            partial_max_backoff: PARTIAL_MAX_BACKOFF,
            register_grace: REGISTER_GRACE,
        }
    }
}

/// A kept total.
#[derive(Debug, Clone, Copy)]
struct Cached {
    totals: DirTotals,
    at: Instant,
    /// `get` shows it as `Known` (until `until`, if set). Cleared by a change below the
    /// folder, when the watch that covered it went away, or when the walk may have missed a
    /// change; the total is then shown as stale until measured again.
    fresh: bool,
    /// Measured by a walk that started while a recursive watch covered the folder, and
    /// nothing changed since: walks may use it instead of entering the folder. Implies
    /// `fresh` and no `until`.
    trusted: bool,
    /// No watch covers the folder: fresh only until then.
    until: Option<Instant>,
    /// A partial total: how long it was fresh for (see [`PARTIAL_MAX_AGE`]); zero otherwise,
    /// and reset to zero by a change below the folder or a refresh.
    partial_delay: Duration,
}

impl Cached {
    fn is_fresh(&self, now: Instant) -> bool {
        self.fresh && self.until.is_none_or(|u| now < u)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootState {
    /// Handed to the registrar thread, not watched yet.
    Registering {
        since: Instant,
    },
    Watched,
    /// The watch could not be set up; totals below are never trusted.
    Failed,
}

struct Wanted {
    /// Position in the caller's list: jobs start in on-screen order.
    order: usize,
    network: bool,
}

struct Job {
    cancel: CancelToken,
    network: bool,
    /// A watch covered the folder when the walk started.
    trusted: bool,
    /// Changes below the folder since the walk started: totals the walk read for folders
    /// containing one of them may be out of date.
    touched: HashSet<PathBuf>,
    touched_all: bool,
}

impl Job {
    fn touch(&mut self, path: &Path) {
        if self.touched_all {
            return;
        }
        if self.touched.len() >= TOUCHED_CAP {
            self.touched_all = true;
            self.touched.clear();
        } else {
            self.touched.insert(path.to_path_buf());
        }
    }

    /// Was something below `dir` changed while this walk ran?
    fn touched_within(&self, dir: &Path) -> bool {
        self.touched_all || self.touched.iter().any(|t| is_within(t, dir))
    }
}

/// What a change event says about its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// Created, renamed or moved in: a folder there is not the one measured before.
    Appeared,
    /// Modified in place: a file's data, or a folder's own timestamps and attributes.
    Modified,
    /// Removed, or anything else.
    Content,
}

/// A wanted folder with changes below it, waiting for the debounce.
struct Dirty {
    first: Instant,
    last: Instant,
    /// Change events since `first`; many make the folder busy (see `BUSY_GAP`).
    events: u32,
}

struct State {
    enabled: bool,
    network_sizes: bool,
    cache: HashMap<PathBuf, Cached>,
    cap: usize,
    wanted: HashMap<PathBuf, Wanted>,
    /// Wanted folders to measure as soon as a worker is free (and their previous walk, if
    /// still running, has ended).
    queue: HashSet<PathBuf>,
    running: HashMap<PathBuf, Job>,
    network_running: usize,
    dirty: HashMap<PathBuf, Dirty>,
    /// When each wanted folder's latest walk started, for `MIN_GAP`.
    last_start: HashMap<PathBuf, Instant>,
    /// Wanted folders whose total is fresh only until the given time (no watch covers them).
    expiring: HashMap<PathBuf, Instant>,
    /// Folders that appeared where a total was kept: the totals kept below them belong to
    /// whatever was there before and are dropped by the next worker (not per event).
    purge: HashSet<PathBuf>,
    /// The local folders the panes show that contain wanted folders, each with a recursive
    /// watch.
    roots: HashMap<PathBuf, RootState>,
    /// Drive letter -> mapped to a network share. Asked once per letter.
    drives: HashMap<u8, bool>,
    /// Bumped whenever a total is kept, a walk starts or the wanted set changes: what
    /// `last_event` saw may then be out of date.
    generation: u64,
    /// For `invalidate_path`'s fast path.
    last_event: Option<LastEvent>,
    shutdown: bool,
}

/// What the latest change event's slow path did: the next event in the same folder can skip
/// the walk up the ancestors.
struct LastEvent {
    parent: PathBuf,
    generation: u64,
    /// The wanted ancestors of the folder (now waiting for their debounce).
    wanted: Vec<PathBuf>,
}

struct Shared {
    state: Mutex<State>,
    /// Workers wait here for jobs (and for the next debounced or expiring folder to be due).
    wake: Condvar,
    revision: AtomicU64,
    ctx: egui::Context,
    walk: Box<WalkFn>,
    watch_hook: Option<Box<WatchHook>>,
    timing: Timing,
    /// Threads per local walk; network walks use one.
    local_threads: usize,
}

impl Shared {
    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Release);
    }
}

pub struct FolderSizes {
    shared: Arc<Shared>,
    workers: usize,
    started: bool,
    /// Local roots for the registrar thread; started with the first root.
    register: Option<Sender<Vec<PathBuf>>>,
    last_folders: Vec<PathBuf>,
    last_roots: Vec<PathBuf>,
    /// The generation given to the last [`FolderSizes::want_at`] (`None` after `want`).
    last_generation: Option<u64>,
    /// `last_*` have been applied under the current settings.
    applied: bool,
}

impl FolderSizes {
    /// Threads start lazily, on the first folder wanted.
    pub fn new(ctx: egui::Context) -> Self {
        Self::with_parts(ctx, dir_size::dir_totals, Timing::default(), None)
    }

    #[cfg(test)]
    fn with_walk(
        ctx: egui::Context,
        walk: impl Fn(&Path, &dyn SizeCache, usize, &CancelToken) -> forge_core::Result<DirTotals>
        + Send
        + Sync
        + 'static,
        timing: Timing,
    ) -> Self {
        Self::with_parts(ctx, walk, timing, None)
    }

    fn with_parts(
        ctx: egui::Context,
        walk: impl Fn(&Path, &dyn SizeCache, usize, &CancelToken) -> forge_core::Result<DirTotals>
        + Send
        + Sync
        + 'static,
        timing: Timing,
        watch_hook: Option<Box<WatchHook>>,
    ) -> Self {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let workers = (cores / 2).clamp(2, 4);
        let local_threads = (cores / workers).clamp(2, 4);
        let state = State {
            enabled: true,
            network_sizes: false,
            cache: HashMap::new(),
            cap: CACHE_CAP,
            wanted: HashMap::new(),
            queue: HashSet::new(),
            running: HashMap::new(),
            network_running: 0,
            dirty: HashMap::new(),
            last_start: HashMap::new(),
            expiring: HashMap::new(),
            purge: HashSet::new(),
            roots: HashMap::new(),
            drives: HashMap::new(),
            generation: 0,
            last_event: None,
            shutdown: false,
        };
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(state),
                wake: Condvar::new(),
                revision: AtomicU64::new(0),
                ctx,
                walk: Box::new(walk),
                watch_hook,
                timing,
                local_threads,
            }),
            workers,
            started: false,
            register: None,
            last_folders: Vec::new(),
            last_roots: Vec::new(),
            last_generation: None,
            applied: false,
        }
    }

    pub fn configure(&mut self, enabled: bool, network: bool) {
        {
            let mut st = self.shared.state.lock();
            if st.enabled == enabled && st.network_sizes == network {
                return;
            }
            st.enabled = enabled;
            st.network_sizes = network;
        }
        self.shared.bump();
        self.applied = false;
        self.apply();
    }

    /// The folders whose totals are on screen now (all panes, visible dir rows) and the
    /// folders the panes show (`roots`). A root gets a recursive change watch only while it
    /// contains a wanted folder, so a pane with nothing to measure watches nothing. Compares
    /// both lists to the last call's (tests; the app uses [`FolderSizes::want_at`]). Queues measurements
    /// for wanted folders without a fresh result; cancels running/queued work for folders no
    /// longer wanted. Totals that expire later are re-measured by the workers on their own.
    #[cfg(test)]
    pub fn want(&mut self, folders: &[PathBuf], roots: &[PathBuf]) {
        if self.applied
            && self.last_generation.is_none()
            && folders == self.last_folders.as_slice()
            && roots == self.last_roots.as_slice()
        {
            return;
        }
        self.last_generation = None;
        self.last_folders = folders.to_vec();
        self.last_roots = roots.to_vec();
        self.applied = true;
        self.apply();
    }

    /// [`FolderSizes::want`] for a caller that calls it every frame: `generation` changes
    /// whenever `folders` does, so an unchanged frame costs a comparison of `roots` (the few
    /// folders the panes show) instead of every wanted path.
    pub fn want_at(&mut self, generation: u64, folders: &[PathBuf], roots: &[PathBuf]) {
        if self.applied
            && self.last_generation == Some(generation)
            && roots == self.last_roots.as_slice()
        {
            return;
        }
        self.last_generation = Some(generation);
        self.last_folders = folders.to_vec();
        self.last_roots = roots.to_vec();
        self.applied = true;
        self.apply();
    }

    fn apply(&mut self) {
        let now = Instant::now();
        let mut guard = self.shared.state.lock();
        let st = &mut *guard;
        let mut changed = false;

        let mut wanted = HashMap::new();
        if st.enabled {
            for (order, folder) in self.last_folders.iter().enumerate() {
                let network = network_of(&mut st.drives, folder);
                if network && !st.network_sizes {
                    continue;
                }
                wanted
                    .entry(folder.clone())
                    .or_insert(Wanted { order, network });
            }
        }
        // Navigating away must stop disk work quickly. Sub-folders the walk completed stay.
        for (path, job) in &st.running {
            if !wanted.contains_key(path) {
                job.cancel.cancel();
            }
        }
        st.queue.retain(|p| wanted.contains_key(p));
        st.dirty.retain(|p, _| wanted.contains_key(p));
        st.last_start.retain(|p, _| wanted.contains_key(p));
        st.expiring.retain(|p, _| wanted.contains_key(p));

        // Only roots with something to measure below them are watched: a pane at a drive
        // root in a view without the Size column must not watch the whole volume.
        let mut local_roots: Vec<PathBuf> = Vec::new();
        for root in &self.last_roots {
            if local_roots.contains(root) || network_of(&mut st.drives, root) {
                continue;
            }
            if wanted.iter().any(|(p, w)| !w.network && is_within(p, root)) {
                local_roots.push(root.clone());
            }
        }
        let dropped: Vec<PathBuf> = st
            .roots
            .keys()
            .filter(|r| !local_roots.contains(r))
            .cloned()
            .collect();
        let mut roots_changed = !dropped.is_empty();
        for root in &dropped {
            st.roots.remove(root);
        }
        for root in &local_roots {
            if !st.roots.contains_key(root) {
                st.roots
                    .insert(root.clone(), RootState::Registering { since: now });
                roots_changed = true;
            }
        }
        // Totals under a watch that goes away can no longer be trusted. The registrar keeps
        // an old watch until the new ones are set up, so a folder still covered by a new
        // root (going into a sub-folder) stays trusted.
        changed |= st.demote_uncovered(&dropped);

        for path in wanted.keys() {
            if st.dirty.contains_key(path) || st.running.contains_key(path) {
                continue; // Measured when the debounce ends / re-checked when the walk ends.
            }
            match st.cache.get(path) {
                // Fresh; one that expires is measured again by a worker when it does.
                Some(c) if c.is_fresh(now) => {
                    if let Some(until) = c.until {
                        st.expiring.insert(path.clone(), until);
                    }
                }
                // Missing, stale, or expired while not wanted (`get` already says so).
                _ => {
                    st.queue.insert(path.clone());
                }
            }
        }
        st.wanted = wanted;
        st.generation += 1;
        let has_work = !st.wanted.is_empty();
        drop(guard);

        if changed {
            self.shared.bump();
        }
        if roots_changed {
            self.send_roots(local_roots);
        }
        if has_work {
            self.start_workers();
        }
        self.shared.wake.notify_all();
    }

    /// Totals of `dir` as the Size column should show them. One lock and a hash lookup.
    pub fn get(&self, dir: &Path) -> FolderSize {
        let mut guard = self.shared.state.lock();
        let st = &mut *guard;
        if !st.enabled || (!st.network_sizes && network_of(&mut st.drives, dir)) {
            return FolderSize::Off;
        }
        match st.cache.get(dir) {
            Some(c) if c.fresh && c.until.is_none_or(|u| Instant::now() < u) => {
                FolderSize::Known(c.totals)
            }
            Some(c) => FolderSize::Pending {
                stale: Some(c.totals),
            },
            None => FolderSize::Pending { stale: None },
        }
    }

    /// Bumps whenever a result arrives or an invalidation changes what `get` returns.
    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    /// The user refreshed `dir` (F5): totals of `dir`, everything below and its ancestors
    /// are no longer trusted, and the wanted ones among them are measured again now. The old
    /// totals stay visible as stale meanwhile, so the column never blanks.
    pub fn invalidate_tree(&mut self, dir: &Path) {
        let mut guard = self.shared.state.lock();
        let st = &mut *guard;
        let changed = st.mark_tree(dir);
        let affected: Vec<PathBuf> = st
            .wanted
            .keys()
            .filter(|w| is_within(w, dir) || is_within(dir, w))
            .cloned()
            .collect();
        for path in affected {
            st.dirty.remove(&path);
            // A walk already running may have read stale trusted totals: restart it (it is
            // queued again when it ends, see `finish`).
            if let Some(job) = st.running.get(&path) {
                job.cancel.cancel();
            }
            st.queue.insert(path);
        }
        drop(guard);
        if changed {
            self.shared.bump();
        }
        self.shared.wake.notify_all();
    }

    /// Something changed at `path`: its folder and every ancestor are stale, and the wanted
    /// ones among them are measured again once changes settle.
    pub fn invalidate(&mut self, path: &Path) {
        invalidate_paths(
            &self.shared,
            std::slice::from_ref(&path.to_path_buf()),
            Change::Appeared,
        );
    }

    fn send_roots(&mut self, roots: Vec<PathBuf>) {
        if self.register.is_none() {
            if roots.is_empty() {
                return;
            }
            let (tx, rx) = crossbeam_channel::unbounded();
            let shared = Arc::clone(&self.shared);
            let spawned = std::thread::Builder::new()
                .name("folder-sizes-watch".into())
                .spawn(move || registrar(shared, rx));
            if let Err(e) = spawned {
                tracing::warn!("folder size change watch unavailable: {e}");
                // Nothing will ever watch these: measure them as unwatched.
                let now = Instant::now();
                let timing = self.shared.timing;
                let mut st = self.shared.state.lock();
                for root in &roots {
                    st.set_root_state(root, false, now, &timing);
                }
                drop(st);
                self.shared.wake.notify_all();
                return;
            }
            self.register = Some(tx);
        }
        if let Some(tx) = &self.register {
            let _ = tx.send(roots);
        }
    }

    fn start_workers(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        for i in 0..self.workers {
            let shared = Arc::clone(&self.shared);
            let spawned = std::thread::Builder::new()
                .name(format!("folder-sizes-{i}"))
                .spawn(move || worker(shared));
            if let Err(e) = spawned {
                tracing::warn!("folder size thread unavailable: {e}");
            }
        }
    }

    /// Waits up to `limit` until nothing is queued, running or waiting for a debounce.
    #[cfg(test)]
    pub(crate) fn wait_idle(&self, limit: Duration) -> bool {
        wait_for(limit, || {
            let st = self.shared.state.lock();
            st.queue.is_empty() && st.running.is_empty() && st.dirty.is_empty()
        })
    }

    /// Waits up to `limit` for `root`'s recursive watch to be set up.
    #[cfg(test)]
    fn wait_watched(&self, root: &Path, limit: Duration) -> bool {
        wait_for(limit, || {
            self.shared.state.lock().roots.get(root) == Some(&RootState::Watched)
        })
    }
}

impl Drop for FolderSizes {
    fn drop(&mut self) {
        let mut st = self.shared.state.lock();
        st.shutdown = true;
        for job in st.running.values() {
            job.cancel.cancel();
        }
        drop(st);
        self.shared.wake.notify_all();
        // Dropping `register` ends the registrar thread, which drops the watcher.
    }
}

impl State {
    /// Entries in or below any of `gone` that no remaining root covers lose their trust and
    /// show as stale until measured again.
    fn demote_uncovered(&mut self, gone: &[PathBuf]) -> bool {
        if gone.is_empty() {
            return false;
        }
        let covering: Vec<&PathBuf> = self
            .roots
            .iter()
            .filter(|(_, s)| **s != RootState::Failed)
            .map(|(r, _)| r)
            .collect();
        let mut changed = false;
        for (path, c) in &mut self.cache {
            if !gone.iter().any(|g| is_within(path, g))
                || covering.iter().any(|r| is_within(path, r))
            {
                continue;
            }
            changed |= c.fresh;
            c.fresh = false;
            c.trusted = false;
        }
        changed
    }

    /// Is `dir` covered by a recursive watch that is set up?
    fn watched_cover(&self, dir: &Path) -> bool {
        self.roots
            .iter()
            .any(|(r, s)| *s == RootState::Watched && is_within(dir, r))
    }

    /// When the watch being set up for a root covering `dir` was asked for, if any.
    fn registering_since(&self, dir: &Path) -> Option<Instant> {
        self.roots
            .iter()
            .filter(|(r, _)| is_within(dir, r))
            .filter_map(|(_, s)| match s {
                RootState::Registering { since } => Some(*since),
                _ => None,
            })
            .min()
    }

    /// A running walk covers `path`. O(depth).
    fn walk_covers(&self, path: &Path) -> bool {
        !self.running.is_empty() && path.ancestors().any(|a| self.running.contains_key(a))
    }

    /// Below a watched drive root (`C:\`), where changes never stop.
    fn under_drive_root(&self, dir: &Path) -> bool {
        self.roots
            .keys()
            .any(|r| r.parent().is_none() && is_within(dir, r))
    }

    /// How `totals` of `dir` from a walk that `tainted` or not, started `trusted` (under a
    /// watch) or not, is kept. A partial total is never reused by later walks and expires
    /// (see [`PARTIAL_MAX_AGE`]); the same partial total as last time expires twice as late.
    #[allow(clippy::too_many_arguments)]
    fn classify(
        &self,
        dir: &Path,
        network: bool,
        trusted: bool,
        tainted: bool,
        totals: DirTotals,
        now: Instant,
        timing: &Timing,
    ) -> Cached {
        let (fresh, trusted, until) =
            self.classify_complete(dir, network, trusted, tainted, now, timing);
        let mut cached = Cached {
            totals,
            at: now,
            fresh,
            trusted,
            until,
            partial_delay: Duration::ZERO,
        };
        if totals.partial && fresh {
            // The same folders unreadable again, nothing else changed: back off.
            let repeat = self
                .cache
                .get(dir)
                .filter(|c| c.totals == totals && !c.partial_delay.is_zero())
                .map(|c| c.partial_delay);
            let base = timing.partial_max_age;
            cached.trusted = false;
            cached.until = Some(match repeat {
                Some(previous) => {
                    let delay = (previous * 2).min(timing.partial_max_backoff).max(base);
                    cached.partial_delay = delay;
                    now + delay
                }
                None => {
                    cached.partial_delay = base;
                    let limit = now + base;
                    until.map_or(limit, |u| u.min(limit))
                }
            });
        }
        cached
    }

    fn classify_complete(
        &self,
        dir: &Path,
        network: bool,
        trusted: bool,
        tainted: bool,
        now: Instant,
        timing: &Timing,
    ) -> (bool, bool, Option<Instant>) {
        if tainted {
            (false, false, None)
        } else if network {
            (true, false, Some(now + timing.network_max_age))
        } else if self.watched_cover(dir) {
            // A watch that came up while the walk ran may have missed a change made before:
            // shown as stale and measured again under the watch.
            (trusted, trusted, None)
        } else if self.registering_since(dir).is_some() {
            // Measured again when the watch is up (or has failed), see `set_root_state`.
            (false, false, None)
        } else {
            (true, false, Some(now + timing.unwatched_max_age))
        }
    }

    /// The registrar set up `root`'s watch (`ok`) or failed to. Wanted folders below it
    /// whose totals are not trusted are measured again: under the watch, or as unwatched
    /// totals that expire. Totals kept below a failed root lose their trust and expire.
    fn set_root_state(&mut self, root: &Path, ok: bool, now: Instant, timing: &Timing) {
        match self.roots.get_mut(root) {
            Some(s @ RootState::Registering { .. }) => {
                *s = if ok {
                    RootState::Watched
                } else {
                    RootState::Failed
                };
            }
            _ => return,
        }
        if !ok {
            let until = now + timing.unwatched_max_age;
            let covering: Vec<PathBuf> = self
                .roots
                .iter()
                .filter(|(_, s)| **s == RootState::Watched)
                .map(|(r, _)| r.clone())
                .collect();
            for (path, c) in &mut self.cache {
                if !is_within(path, root) || covering.iter().any(|r| is_within(path, r)) {
                    continue;
                }
                c.trusted = false;
                if c.fresh && c.until.is_none() {
                    c.until = Some(until);
                    if self.wanted.contains_key(path) {
                        self.expiring.insert(path.clone(), until);
                    }
                }
            }
        }
        let again: Vec<PathBuf> = self
            .wanted
            .iter()
            .filter(|(p, w)| !w.network && is_within(p, root))
            .filter(|(p, _)| !self.running.contains_key(*p) && !self.dirty.contains_key(*p))
            .filter(|(p, _)| {
                self.cache
                    .get(*p)
                    .is_none_or(|c| if ok { !c.trusted } else { !c.is_fresh(now) })
            })
            .map(|(p, _)| p.clone())
            .collect();
        self.queue.extend(again);
    }

    /// `path` changed: its own entry, its folder and every ancestor become stale, running
    /// walks of those folders are told, and wanted ones wait for the debounce. A folder that
    /// appears ([`Change::Appeared`]) at a path with a kept total also loses the totals kept
    /// below that path (see `purge`). O(depth): no cache scan.
    /// Returns (what `get` shows changed, a worker must look: a new debounce or purge).
    #[cfg(test)]
    fn invalidate_path(&mut self, path: &Path, change: Change, now: Instant) -> (bool, bool) {
        self.try_invalidate_path(path, change, true, now)
            .unwrap_or_default()
    }

    /// [`State::invalidate_path`], except that it returns `None` instead for a modified path
    /// that may be a folder a running walk has listed but not stored yet, unless `file`
    /// (the caller checked that it is not a folder).
    fn try_invalidate_path(
        &mut self,
        path: &Path,
        change: Change,
        file: bool,
        now: Instant,
    ) -> Option<(bool, bool)> {
        let kept = self.cache.contains_key(path);
        if change == Change::Modified {
            if kept {
                // A measured folder's own timestamps or attributes. NTFS reports them when
                // our walks list folders that changed lately: taken as a change, measuring
                // a folder would invalidate it again. A folder's own metadata says nothing
                // about the sizes inside, whose changes arrive on their own paths.
                return Some((false, false));
            }
            if !file && self.walk_covers(path) {
                return None;
            }
        }
        let mut changed = false;
        let mut wake = false;
        if change == Change::Appeared && kept {
            wake |= self.purge.insert(path.to_path_buf());
        }
        // Fast path: another change in (or of) the folder of the previous event, with nothing
        // measured, started or wanted since. The ancestors are stale and the running walks
        // told already (a walk records the folder: for the folders it reads, that is the
        // same as the file); only the path's own entry and the debounce need updating.
        let parent = path.parent();
        if let Some(last) = &self.last_event
            && last.generation == self.generation
            && (Some(last.parent.as_path()) == parent || last.parent == path)
            && (self.running.is_empty() || !self.running.contains_key(path))
            && !self.wanted.contains_key(path)
            && last.wanted.iter().all(|w| self.dirty.contains_key(w))
        {
            for w in &last.wanted {
                if let Some(d) = self.dirty.get_mut(w) {
                    d.last = now;
                    d.events = d.events.saturating_add(1);
                }
            }
            if let Some(c) = self.cache.get_mut(path) {
                changed |= c.fresh;
                c.fresh = false;
                c.trusted = false;
                c.partial_delay = Duration::ZERO;
            }
            return Some((changed, wake));
        }
        let mut wanted_above = Vec::new();
        let mut cur = Some(path);
        while let Some(p) = cur {
            if let Some(c) = self.cache.get_mut(p) {
                changed |= c.fresh;
                c.fresh = false;
                c.trusted = false;
                c.partial_delay = Duration::ZERO;
            }
            if let Some(job) = self.running.get_mut(p) {
                job.touch(path);
            }
            if self.wanted.contains_key(p) {
                if p != path {
                    wanted_above.push(p.to_path_buf());
                }
                match self.dirty.get_mut(p) {
                    Some(d) => {
                        d.last = now;
                        d.events = d.events.saturating_add(1);
                    }
                    None => {
                        self.dirty.insert(
                            p.to_path_buf(),
                            Dirty {
                                first: now,
                                last: now,
                                events: 1,
                            },
                        );
                        wake = true;
                    }
                }
            }
            cur = p.parent();
        }
        self.last_event = parent.map(|parent| LastEvent {
            parent: parent.to_path_buf(),
            generation: self.generation,
            wanted: wanted_above,
        });
        Some((changed, wake))
    }

    /// Drops the totals kept below folders that appeared anew (see `invalidate_path`). One
    /// pass over the cache, checking each entry's ancestors. Wanted folders that lose their
    /// total are measured again.
    fn run_purges(&mut self) -> bool {
        if self.purge.is_empty() {
            return false;
        }
        let purge = std::mem::take(&mut self.purge);
        let mut removed = Vec::new();
        self.cache.retain(|k, _| {
            let gone = k.ancestors().skip(1).any(|a| purge.contains(a));
            if gone {
                removed.push(k.clone());
            }
            !gone
        });
        for path in &removed {
            self.expiring.remove(path);
            if self.wanted.contains_key(path)
                && !self.running.contains_key(path)
                && !self.dirty.contains_key(path)
            {
                self.queue.insert(path.clone());
            }
        }
        !removed.is_empty()
    }

    /// `dir`, everything below and its ancestors become stale and untrusted; running walks
    /// of any of them count as touched.
    fn mark_tree(&mut self, dir: &Path) -> bool {
        let mut changed = false;
        for (path, c) in &mut self.cache {
            if is_within(path, dir) || is_within(dir, path) {
                changed |= c.fresh;
                c.fresh = false;
                c.trusted = false;
                c.partial_delay = Duration::ZERO;
            }
        }
        for (path, job) in &mut self.running {
            if is_within(path, dir) || is_within(dir, path) {
                job.touched_all = true;
            }
        }
        changed
    }

    /// Moves wanted folders whose changes have settled, and those whose unwatched total
    /// expired, into the queue. Returns when the next waiting one will be due, and whether a
    /// total expired (what `get` shows changed).
    fn promote_due(&mut self, now: Instant, timing: &Timing) -> (Option<Instant>, bool) {
        let mut next: Option<Instant> = None;
        let mut due = Vec::new();
        for (path, d) in &self.dirty {
            let gap = if d.events >= BUSY_EVENTS || self.under_drive_root(path) {
                timing.busy_gap
            } else {
                timing.min_gap
            };
            // Quiet for the debounce, or changing for a while already (a long copy)...
            let settled = (d.last + timing.debounce).min(d.first + gap);
            // ...and not measured within the last gap.
            let spaced = self.last_start.get(path).map_or(now, |t| *t + gap);
            let at = settled.max(spaced);
            if at <= now {
                due.push(path.clone());
            } else {
                next = Some(next.map_or(at, |n| n.min(at)));
            }
        }
        for path in due {
            self.dirty.remove(&path);
            self.queue.insert(path);
        }
        let mut expired = Vec::new();
        for (path, until) in &self.expiring {
            if *until <= now {
                expired.push(path.clone());
            } else {
                next = Some(next.map_or(*until, |n| n.min(*until)));
            }
        }
        let any_expired = !expired.is_empty();
        for path in expired {
            self.expiring.remove(&path);
            if !self.running.contains_key(&path) && !self.dirty.contains_key(&path) {
                self.queue.insert(path);
            }
        }
        (next, any_expired)
    }

    /// The queued folder to measure next: the first on screen whose previous walk has ended,
    /// skipping network folders while the network slots are taken, and local folders whose
    /// watch is still being set up (for up to the grace period; `hold` gets when that ends).
    fn pick(
        &mut self,
        now: Instant,
        timing: &Timing,
        hold: &mut Option<Instant>,
    ) -> Option<(PathBuf, bool)> {
        let mut best: Option<(usize, &PathBuf, bool)> = None;
        for p in &self.queue {
            if self.running.contains_key(p) {
                continue;
            }
            let Some(w) = self.wanted.get(p) else {
                continue;
            };
            if w.network && self.network_running >= NETWORK_JOBS {
                continue;
            }
            if best.is_some_and(|(order, _, _)| order <= w.order) {
                continue;
            }
            let held = if !w.network && !self.watched_cover(p) {
                self.registering_since(p)
                    .map(|since| since + timing.register_grace)
                    .filter(|ready| now < *ready)
            } else {
                None
            };
            if let Some(ready) = held {
                *hold = Some(hold.map_or(ready, |h| h.min(ready)));
                continue;
            }
            best = Some((w.order, p, w.network));
        }
        let (path, network) = best.map(|(_, p, n)| (p.clone(), n))?;
        self.queue.remove(&path);
        Some((path, network))
    }

    fn insert(&mut self, path: PathBuf, cached: Cached) {
        self.generation += 1;
        match cached.until {
            Some(until) if cached.fresh && self.wanted.contains_key(&path) => {
                self.expiring.insert(path.clone(), until);
            }
            _ => {
                self.expiring.remove(&path);
            }
        }
        self.cache.insert(path, cached);
        if self.cache.len() > self.cap {
            self.evict();
        }
    }

    /// Brings the cache down to 90% of its cap: untrusted entries first, then the oldest.
    /// Wanted and running folders are kept (they would otherwise show "…" with nothing
    /// queued to measure them).
    fn evict(&mut self) {
        let target = self.cache.len().saturating_sub(self.cap - self.cap / 10);
        let mut candidates: Vec<(bool, Instant, PathBuf)> = self
            .cache
            .iter()
            .filter(|(p, _)| !self.wanted.contains_key(*p) && !self.running.contains_key(*p))
            .map(|(p, c)| (c.trusted, c.at, p.clone()))
            .collect();
        let n = target.min(candidates.len());
        if n == 0 {
            return;
        }
        if n < candidates.len() {
            candidates.select_nth_unstable_by(n - 1, |a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        }
        for (_, _, path) in candidates.into_iter().take(n) {
            self.cache.remove(&path);
        }
    }

    /// A walk of `path` ended.
    fn finish(&mut self, path: PathBuf, result: forge_core::Result<DirTotals>, timing: &Timing) {
        let Some(job) = self.running.remove(&path) else {
            return;
        };
        if job.network {
            self.network_running = self.network_running.saturating_sub(1);
        }
        let tainted = job.touched_within(&path);
        let now = Instant::now();
        let totals = match result {
            Ok(totals) => Some(totals),
            Err(CoreError::Cancelled) => None,
            // An unreadable or vanished folder: show what is known (nothing) as partial
            // rather than "measuring" forever.
            Err(e) => {
                tracing::debug!("folder size of {}: {e}", path.display());
                Some(DirTotals {
                    partial: true,
                    ..DirTotals::default()
                })
            }
        };
        if let Some(totals) = totals {
            let cached = self.classify(
                &path,
                job.network,
                job.trusted,
                tainted,
                totals,
                now,
                timing,
            );
            self.insert(path.clone(), cached);
        }
        // Cancelled, changed while it ran, or started before its watch was up, and still
        // wanted: measure again. While the watch is still being set up the registrar queues
        // it once the watch is up (or failed), so it is not measured in a loop meanwhile.
        let fresh = self.cache.get(&path).is_some_and(|c| c.is_fresh(now));
        if self.wanted.contains_key(&path)
            && !fresh
            && !self.dirty.contains_key(&path)
            && (job.network || self.watched_cover(&path) || self.registering_since(&path).is_none())
        {
            self.queue.insert(path);
        }
    }
}

/// Hands walks the totals they may skip and keeps what they complete.
struct Adapter<'a> {
    shared: &'a Shared,
    job: &'a Path,
}

impl SizeCache for Adapter<'_> {
    fn lookup(&self, dir: &Path) -> Option<DirTotals> {
        let st = self.shared.state.lock();
        st.cache
            .get(dir)
            .filter(|c| c.trusted && c.fresh)
            .map(|c| c.totals)
    }

    fn store(&self, dir: &Path, totals: &DirTotals) {
        let mut st = self.shared.state.lock();
        let Some(job) = st.running.get(self.job) else {
            return;
        };
        let tainted = job.touched_within(dir);
        let now = Instant::now();
        let cached = st.classify(
            dir,
            job.network,
            job.trusted,
            tainted,
            *totals,
            now,
            &self.shared.timing,
        );
        let wanted = st.wanted.contains_key(dir);
        st.insert(dir.to_path_buf(), cached);
        drop(st);
        if wanted {
            self.shared.bump();
        }
    }
}

fn worker(shared: Arc<Shared>) {
    let timing = shared.timing;
    let mut st = shared.state.lock();
    loop {
        if st.shutdown {
            return;
        }
        let now = Instant::now();
        let purged = st.run_purges();
        let (next_due, expired) = st.promote_due(now, &timing);
        if purged || expired {
            shared.bump();
            shared.ctx.request_repaint();
        }
        let mut hold = None;
        let Some((path, network)) = st.pick(now, &timing, &mut hold) else {
            let wake_at = match (next_due, hold) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            match wake_at {
                Some(at) => {
                    shared.wake.wait_until(&mut st, at);
                }
                None => shared.wake.wait(&mut st),
            }
            continue;
        };
        let cancel = CancelToken::new();
        let trusted = !network && st.watched_cover(&path);
        st.generation += 1;
        st.running.insert(
            path.clone(),
            Job {
                cancel: cancel.clone(),
                network,
                trusted,
                touched: HashSet::new(),
                touched_all: false,
            },
        );
        if network {
            st.network_running += 1;
        }
        st.last_start.insert(path.clone(), now);
        let threads = if network { 1 } else { shared.local_threads };
        let result = MutexGuard::unlocked(&mut st, || {
            let adapter = Adapter {
                shared: &shared,
                job: &path,
            };
            (shared.walk)(&path, &adapter, threads, &cancel)
        });
        let cancelled = matches!(result, Err(CoreError::Cancelled));
        st.finish(path, result, &timing);
        if !cancelled {
            shared.bump();
            shared.ctx.request_repaint();
        }
        // Another job may have become startable (a network slot freed, a folder whose walk
        // was running is queued again).
        shared.wake.notify_all();
    }
}

/// Marks `paths` changed (see [`State::invalidate_path`]) and wakes the workers when a
/// debounce timer has to start. A steady stream of events on the same folders costs a few
/// hash lookups each: no wake-ups, no repaints.
fn invalidate_paths(shared: &Shared, paths: &[PathBuf], change: Change) {
    let now = Instant::now();
    let mut st = shared.state.lock();
    let mut changed = false;
    let mut wake = false;
    let mut unsure = Vec::new();
    for path in paths {
        match st.try_invalidate_path(path, change, false, now) {
            Some((c, w)) => {
                changed |= c;
                wake |= w;
            }
            None => unsure.push(path),
        }
    }
    drop(st);
    if !unsure.is_empty() {
        // Possibly a folder a running walk listed (see `try_invalidate_path`): one attribute
        // read each, outside the lock. Only while a walk runs below the path, so rare.
        unsure.retain(|p| !is_dir_entry(p));
        let mut st = shared.state.lock();
        for path in unsure {
            if let Some((c, w)) = st.try_invalidate_path(path, change, true, now) {
                changed |= c;
                wake |= w;
            }
        }
    }
    if changed {
        shared.bump();
        shared.ctx.request_repaint();
    }
    if wake {
        shared.wake.notify_all();
    }
}

/// Events were lost (overflow, rescan, watch error): nothing below the roots containing
/// `paths` (all roots when `paths` is empty) can be trusted.
fn invalidate_roots(shared: &Shared, paths: &[PathBuf]) {
    let now = Instant::now();
    let mut st = shared.state.lock();
    let roots: Vec<PathBuf> = st
        .roots
        .keys()
        .filter(|r| paths.is_empty() || paths.iter().any(|p| is_within(p, r) || is_within(r, p)))
        .cloned()
        .collect();
    let mut changed = false;
    for root in &roots {
        changed |= st.mark_tree(root);
        let affected: Vec<PathBuf> = st
            .wanted
            .keys()
            .filter(|w| is_within(w, root))
            .cloned()
            .collect();
        for w in affected {
            // Lost events mean a busy tree: use the busy gap.
            st.dirty
                .entry(w)
                .and_modify(|d| {
                    d.last = now;
                    d.events = d.events.max(BUSY_EVENTS);
                })
                .or_insert(Dirty {
                    first: now,
                    last: now,
                    events: BUSY_EVENTS,
                });
        }
    }
    drop(st);
    if changed {
        shared.bump();
        shared.ctx.request_repaint();
    }
    shared.wake.notify_all();
}

fn on_event(shared: &Shared, res: notify::Result<notify::Event>) {
    use notify::EventKind;
    use notify::event::ModifyKind;
    match res {
        Err(e) => invalidate_roots(shared, &e.paths),
        Ok(ev) if ev.need_rescan() => invalidate_roots(shared, &ev.paths),
        Ok(ev) if matches!(ev.kind, EventKind::Access(_)) => {}
        Ok(ev) => {
            // notify reports `<watched root>\<relative name>`, so the paths share the
            // roots' (and the listings') spelling; no normalising needed.
            let change = match ev.kind {
                EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_)) => Change::Appeared,
                EventKind::Modify(_) => Change::Modified,
                _ => Change::Content,
            };
            invalidate_paths(shared, &ev.paths, change);
        }
    }
}

/// Keeps one recursive watcher's roots equal to the newest wanted set. Registering opens
/// each folder, so it happens here, never on the UI thread. New roots are watched before
/// old ones are released, so a folder covered by both never goes unwatched.
fn registrar(shared: Arc<Shared>, rx: Receiver<Vec<PathBuf>>) {
    let events = Arc::clone(&shared);
    let mut watcher = notify::recommended_watcher(move |res| on_event(&events, res))
        .map_err(|e| tracing::warn!("folder size change watch unavailable: {e}"))
        .ok();
    let mut watched: HashSet<PathBuf> = HashSet::new();
    while let Ok(mut want) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            want = newer;
        }
        for root in &want {
            // Still watched from before (dropped and wanted again before this thread saw
            // it): the watch never stopped.
            let ok = watched.contains(root) || {
                let ok = match shared.watch_hook.as_ref().and_then(|hook| hook(root)) {
                    Some(ok) => ok,
                    None => watcher
                        .as_mut()
                        .is_some_and(|w| w.watch(root, RecursiveMode::Recursive).is_ok()),
                };
                if ok {
                    watched.insert(root.clone());
                }
                ok
            };
            let mut st = shared.state.lock();
            st.set_root_state(root, ok, Instant::now(), &shared.timing);
            drop(st);
            shared.wake.notify_all();
        }
        let gone: Vec<PathBuf> = watched
            .iter()
            .filter(|r| !want.contains(r))
            .cloned()
            .collect();
        for root in gone {
            if let Some(w) = watcher.as_mut() {
                let _ = w.unwatch(&root);
            }
            watched.remove(&root);
        }
    }
}

/// Is `path` a folder? Reads the attributes its directory entry holds
/// (`GetFileAttributesW`) without opening it: opening a file that was just written can make
/// the virus scanner check it first, which on a busy volume costs more than everything else.
fn is_dir_entry(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
    let attrs = unsafe { GetFileAttributesW(windows::core::PCWSTR(wide.as_ptr())) };
    attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0
}

/// Is `path` on a network location? UNC paths are lexical; a drive letter's answer is asked
/// once and kept.
fn network_of(drives: &mut HashMap<u8, bool>, path: &Path) -> bool {
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => *drives
                .entry(d.to_ascii_uppercase())
                .or_insert_with(|| forge_core::netpath::is_network_path(path)),
            _ => forge_core::netpath::is_network_path(path),
        },
        _ => forge_core::netpath::is_network_path(path),
    }
}

/// `path` is `base` or below it. Byte-wise (cheap enough to run over the whole cache);
/// paths come from the same listings, so their spelling matches.
fn is_within(path: &Path, base: &Path) -> bool {
    let p = path.as_os_str().as_encoded_bytes();
    let b = base.as_os_str().as_encoded_bytes();
    if !p.starts_with(b) {
        return false;
    }
    p.len() == b.len()
        || matches!(b.last(), Some(b'\\' | b'/'))
        || matches!(p[b.len()], b'\\' | b'/')
}

#[cfg(test)]
fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    done()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    const LIMIT: Duration = Duration::from_secs(4);

    fn quick() -> Timing {
        Timing {
            debounce: Duration::from_millis(50),
            min_gap: Duration::from_millis(200),
            busy_gap: Duration::from_millis(400),
            network_max_age: NETWORK_MAX_AGE,
            unwatched_max_age: UNWATCHED_MAX_AGE,
            partial_max_age: PARTIAL_MAX_AGE,
            partial_max_backoff: PARTIAL_MAX_BACKOFF,
            register_grace: REGISTER_GRACE,
        }
    }

    fn real(timing: Timing) -> FolderSizes {
        FolderSizes::with_walk(egui::Context::default(), dir_size::dir_totals, timing)
    }

    fn write(path: &Path, len: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![7u8; len]).unwrap();
    }

    fn known(fs: &FolderSizes, dir: &Path) -> Option<DirTotals> {
        match fs.get(dir) {
            FolderSize::Known(t) => Some(t),
            _ => None,
        }
    }

    fn wait_known_bytes(fs: &FolderSizes, dir: &Path, bytes: u64) -> bool {
        wait_for(LIMIT, || known(fs, dir).is_some_and(|t| t.bytes == bytes))
    }

    /// root/a/{x 100 B, b/{y 200 B}}, root/c/{z 50 B}
    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a").join("x"), 100);
        write(&dir.path().join("a").join("b").join("y"), 200);
        write(&dir.path().join("c").join("z"), 50);
        dir
    }

    #[test]
    fn pending_until_the_walk_answers_then_known() {
        let (gate_tx, gate_rx) = crossbeam_channel::bounded::<()>(0);
        let mut fs = FolderSizes::with_walk(
            egui::Context::default(),
            move |_, _, _, _| {
                let _ = gate_rx.recv_timeout(LIMIT);
                Ok(DirTotals {
                    bytes: 42,
                    files: 1,
                    dirs: 0,
                    partial: false,
                    ..DirTotals::default()
                })
            },
            quick(),
        );
        let dir = PathBuf::from(r"C:\fake\folder");
        let rev = fs.revision();
        fs.want(std::slice::from_ref(&dir), &[]);
        assert_eq!(fs.get(&dir), FolderSize::Pending { stale: None });
        gate_tx.send(()).unwrap();
        assert!(wait_for(LIMIT, || known(&fs, &dir).is_some()));
        assert_eq!(known(&fs, &dir).unwrap().bytes, 42);
        assert!(fs.revision() > rev);
        assert!(fs.wait_idle(LIMIT));
    }

    #[test]
    fn a_folder_no_longer_wanted_stops_its_walk() {
        let started = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let (s, t) = (Arc::clone(&started), Arc::clone(&stopped));
        let mut fs = FolderSizes::with_walk(
            egui::Context::default(),
            move |_, _, _, cancel| {
                s.store(true, Ordering::SeqCst);
                let begun = Instant::now();
                while begun.elapsed() < LIMIT {
                    if cancel.is_cancelled() {
                        t.store(true, Ordering::SeqCst);
                        return Err(CoreError::Cancelled);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(DirTotals::default())
            },
            quick(),
        );
        let dir = PathBuf::from(r"C:\fake\slow");
        fs.want(std::slice::from_ref(&dir), &[]);
        assert!(wait_for(LIMIT, || started.load(Ordering::SeqCst)));
        fs.want(&[], &[]);
        assert!(wait_for(Duration::from_secs(1), || stopped.load(Ordering::SeqCst)));
        assert!(fs.wait_idle(LIMIT));
        assert_eq!(fs.get(&dir), FolderSize::Pending { stale: None });
    }

    #[test]
    fn sub_folders_measured_by_a_walk_are_known_at_once() {
        let root = tree();
        let a = root.path().join("a");
        let mut fs = real(quick());
        fs.want(std::slice::from_ref(&a), &[root.path().to_path_buf()]);
        assert!(wait_known_bytes(&fs, &a, 300));
        let b = known(&fs, &a.join("b")).expect("stored by the walk of a");
        assert_eq!((b.bytes, b.files, b.dirs), (200, 1, 0));
        let a_totals = known(&fs, &a).unwrap();
        assert_eq!((a_totals.files, a_totals.dirs), (2, 1));
    }

    #[test]
    fn invalidate_marks_ancestors_stale_and_remeasures() {
        let root = tree();
        let a = root.path().join("a");
        let b = a.join("b");
        let mut fs = real(quick());
        fs.want(std::slice::from_ref(&a), &[]);
        assert!(wait_known_bytes(&fs, &a, 300));
        let old_a = known(&fs, &a).unwrap();
        let old_b = known(&fs, &b).unwrap();

        let new_file = b.join("new.bin");
        write(&new_file, 1000);
        fs.invalidate(&new_file);
        assert_eq!(fs.get(&a), FolderSize::Pending { stale: Some(old_a) });
        assert_eq!(fs.get(&b), FolderSize::Pending { stale: Some(old_b) });
        assert!(wait_known_bytes(&fs, &a, 1300));
        assert_eq!(known(&fs, &b).unwrap().bytes, 1200);
    }

    #[test]
    fn invalidate_tree_remeasures_without_waiting() {
        let root = tree();
        let a = root.path().join("a");
        let mut fs = real(Timing {
            debounce: Duration::from_secs(30),
            min_gap: Duration::from_secs(30),
            busy_gap: Duration::from_secs(30),
            ..Timing::default()
        });
        fs.want(std::slice::from_ref(&a), &[]);
        assert!(wait_known_bytes(&fs, &a, 300));
        write(&a.join("b").join("more"), 5);
        fs.invalidate_tree(&a);
        assert!(matches!(
            fs.get(&a),
            FolderSize::Pending { stale: Some(_) } | FolderSize::Known(_)
        ));
        assert!(wait_known_bytes(&fs, &a, 305));
    }

    #[test]
    fn network_folders_are_off_while_network_sizes_are_off() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let mut fs = FolderSizes::with_walk(
            egui::Context::default(),
            move |_, _, threads, _| {
                c.fetch_add(1, Ordering::SeqCst);
                assert_eq!(threads, 1, "network walks use one thread");
                Ok(DirTotals {
                    bytes: 9,
                    ..DirTotals::default()
                })
            },
            quick(),
        );
        let share = PathBuf::from(r"\\server\share\folder");
        fs.configure(true, false);
        fs.want(std::slice::from_ref(&share), std::slice::from_ref(&share));
        assert_eq!(fs.get(&share), FolderSize::Off);
        assert!(fs.wait_idle(Duration::from_millis(200)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // Never watched, even with network sizes on.
        fs.configure(true, true);
        assert!(wait_known_bytes(&fs, &share, 9));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(fs.shared.state.lock().roots.is_empty());
    }

    #[test]
    fn disabled_sizes_are_off_and_measure_nothing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let mut fs = FolderSizes::with_walk(
            egui::Context::default(),
            move |_, _, _, _| {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(DirTotals::default())
            },
            quick(),
        );
        fs.configure(false, true);
        let dir = PathBuf::from(r"C:\fake\local");
        fs.want(std::slice::from_ref(&dir), std::slice::from_ref(&dir));
        assert_eq!(fs.get(&dir), FolderSize::Off);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        fs.configure(true, false);
        assert!(wait_for(LIMIT, || known(&fs, &dir).is_some()));
    }

    #[test]
    fn a_change_below_a_watched_root_updates_its_total() {
        let root = tree();
        let r = root.path().to_path_buf();
        let a = r.join("a");
        let mut fs = real(quick());
        fs.want(&[r.clone(), a.clone()], std::slice::from_ref(&r));
        assert!(fs.wait_watched(&r, LIMIT), "watch registered");
        assert!(wait_known_bytes(&fs, &r, 350));
        write(&a.join("b").join("deep").join("new.bin"), 4000);
        assert!(
            wait_known_bytes(&fs, &r, 4350),
            "root total follows the change"
        );
        assert!(wait_known_bytes(&fs, &a, 4300));
    }

    #[test]
    fn remeasure_reuses_trusted_sub_totals() {
        let root = tree();
        let r = root.path().to_path_buf();
        let c = r.join("c");
        let skipped = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let seen = Arc::clone(&skipped);
        struct Spy<'a> {
            inner: &'a dyn SizeCache,
            seen: &'a Mutex<Vec<PathBuf>>,
        }
        impl SizeCache for Spy<'_> {
            fn lookup(&self, dir: &Path) -> Option<DirTotals> {
                let hit = self.inner.lookup(dir);
                if hit.is_some() {
                    self.seen.lock().push(dir.to_path_buf());
                }
                hit
            }
            fn store(&self, dir: &Path, totals: &DirTotals) {
                self.inner.store(dir, totals);
            }
        }
        let mut fs = FolderSizes::with_walk(
            egui::Context::default(),
            move |dir, cache, threads, cancel| {
                dir_size::dir_totals(
                    dir,
                    &Spy {
                        inner: cache,
                        seen: &seen,
                    },
                    threads,
                    cancel,
                )
            },
            quick(),
        );
        // The first walk waits for the watch, so it is already trusted.
        fs.want(std::slice::from_ref(&r), std::slice::from_ref(&r));
        assert!(fs.wait_watched(&r, LIMIT));
        assert!(wait_known_bytes(&fs, &r, 350));
        assert!(fs.shared.state.lock().cache[&r].trusted);
        assert!(skipped.lock().is_empty());

        write(&r.join("a").join("x"), 150);
        fs.invalidate(&r.join("a").join("x"));
        assert!(wait_known_bytes(&fs, &r, 400));
        let skipped = skipped.lock();
        assert!(
            skipped.contains(&c),
            "unchanged sibling reused: {skipped:?}"
        );
        assert!(!skipped.contains(&r.join("a")), "changed branch re-read");
    }

    #[test]
    fn dropping_a_watched_root_leaves_its_totals_stale() {
        let root = tree();
        let r = root.path().to_path_buf();
        let a = r.join("a");
        let mut fs = real(quick());
        fs.want(std::slice::from_ref(&a), std::slice::from_ref(&r));
        assert!(fs.wait_watched(&r, LIMIT));
        assert!(wait_known_bytes(&fs, &a, 300));
        // Into a sub-folder: still covered, so still known.
        fs.want(&[a.join("b")], std::slice::from_ref(&a));
        assert!(known(&fs, &a.join("b")).is_some());
        assert!(known(&fs, &a).is_some());
        // Elsewhere entirely: nothing watches these totals any more.
        fs.want(&[], &[]);
        assert!(matches!(fs.get(&a), FolderSize::Pending { stale: Some(t) } if t.bytes == 300));
    }

    #[test]
    fn eviction_drops_untrusted_and_old_entries_but_keeps_wanted_ones() {
        let fs = real(quick());
        let mut st = fs.shared.state.lock();
        st.cap = 10;
        let base = Instant::now();
        let entry = |secs: u64, trusted: bool| Cached {
            totals: DirTotals::default(),
            at: base + Duration::from_secs(secs),
            fresh: true,
            trusted,
            until: None,
            partial_delay: Duration::ZERO,
        };
        st.wanted.insert(
            PathBuf::from("w"),
            Wanted {
                order: 0,
                network: false,
            },
        );
        st.insert(PathBuf::from("w"), entry(0, false));
        for i in 0..5 {
            st.insert(PathBuf::from(format!("t{i}")), entry(i, true));
        }
        for i in 0..5 {
            st.insert(PathBuf::from(format!("u{i}")), entry(10 + i, false));
        }
        // The 11th entry brings the cache down to 9: two untrusted entries go.
        assert_eq!(st.cache.len(), 9);
        assert!(st.cache.contains_key(Path::new("w")));
        assert!((0..5).all(|i| st.cache.contains_key(&PathBuf::from(format!("t{i}")))));
        assert!(!st.cache.contains_key(Path::new("u0")));
        assert!(!st.cache.contains_key(Path::new("u1")));
    }

    #[test]
    fn a_folder_reappearing_drops_the_totals_kept_below_it() {
        let fs = real(quick());
        let mut st = fs.shared.state.lock();
        let c = Cached {
            totals: DirTotals {
                bytes: 1,
                ..DirTotals::default()
            },
            at: Instant::now(),
            fresh: true,
            trusted: true,
            until: None,
            partial_delay: Duration::ZERO,
        };
        for p in [r"C:\r\x", r"C:\r\x\y", r"C:\r\xy", r"C:\r"] {
            st.insert(PathBuf::from(p), c);
        }
        st.invalidate_path(Path::new(r"C:\r\x"), Change::Appeared, Instant::now());
        assert!(
            st.cache.contains_key(Path::new(r"C:\r\x\y")),
            "not per event"
        );
        assert!(st.run_purges());
        assert!(!st.cache.contains_key(Path::new(r"C:\r\x\y")));
        assert!(!st.cache[Path::new(r"C:\r\x")].fresh);
        assert!(!st.cache[Path::new(r"C:\r")].trusted);
        assert!(
            st.cache[Path::new(r"C:\r\xy")].trusted,
            "a sibling sharing the prefix stays"
        );
    }

    #[test]
    fn is_within_respects_component_boundaries() {
        assert!(is_within(Path::new(r"C:\a\b"), Path::new(r"C:\a")));
        assert!(is_within(Path::new(r"C:\a"), Path::new(r"C:\a")));
        assert!(is_within(Path::new(r"C:\a"), Path::new(r"C:\")));
        assert!(!is_within(Path::new(r"C:\ab"), Path::new(r"C:\a")));
        assert!(!is_within(Path::new(r"C:\a"), Path::new(r"C:\a\b")));
    }

    fn counting(
        calls: &Arc<AtomicUsize>,
    ) -> impl Fn(&Path, &dyn SizeCache, usize, &CancelToken) -> forge_core::Result<DirTotals>
    + Send
    + Sync
    + 'static {
        let c = Arc::clone(calls);
        move |dir, cache, threads, cancel| {
            c.fetch_add(1, Ordering::SeqCst);
            dir_size::dir_totals(dir, cache, threads, cancel)
        }
    }

    #[test]
    fn a_walk_done_before_its_watch_is_up_is_measured_again_under_it() {
        let root = tree();
        let r = root.path().to_path_buf();
        let (gate_tx, gate_rx) = crossbeam_channel::bounded::<()>(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut fs = FolderSizes::with_parts(
            egui::Context::default(),
            counting(&calls),
            Timing {
                register_grace: Duration::ZERO,
                ..quick()
            },
            // A slow registration: the walk starts and ends before the watch is up.
            Some(Box::new(move |_| {
                let _ = gate_rx.recv_timeout(LIMIT);
                None
            })),
        );
        fs.want(std::slice::from_ref(&r), std::slice::from_ref(&r));
        assert!(wait_for(LIMIT, || matches!(
            fs.get(&r),
            FolderSize::Pending { stale: Some(t) } if t.bytes == 350
        )));
        assert!(!fs.shared.state.lock().cache[&r].trusted);
        // Not measured in a loop while the watch is coming.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(fs.get(&r), FolderSize::Pending { .. }));
        // A change in the gap, which no event will ever report.
        write(&r.join("c").join("gap.bin"), 1000);
        gate_tx.send(()).unwrap();
        assert!(fs.wait_watched(&r, LIMIT));
        assert!(
            wait_known_bytes(&fs, &r, 1350),
            "measured again once watched"
        );
        assert!(fs.shared.state.lock().cache[&r].trusted);
    }

    #[test]
    fn the_first_walk_waits_briefly_for_its_watch() {
        let root = tree();
        let r = root.path().to_path_buf();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut fs = FolderSizes::with_parts(
            egui::Context::default(),
            counting(&calls),
            quick(),
            Some(Box::new(|_| {
                std::thread::sleep(Duration::from_millis(150));
                None
            })),
        );
        fs.want(std::slice::from_ref(&r), std::slice::from_ref(&r));
        assert!(wait_known_bytes(&fs, &r, 350));
        assert!(fs.wait_idle(LIMIT));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "measured once, under the watch"
        );
        assert!(fs.shared.state.lock().cache[&r].trusted);
    }

    #[test]
    fn a_root_whose_watch_failed_is_measured_again_after_a_while() {
        let root = tree();
        let r = root.path().to_path_buf();
        let mut fs = FolderSizes::with_parts(
            egui::Context::default(),
            dir_size::dir_totals,
            Timing {
                unwatched_max_age: Duration::from_millis(300),
                ..quick()
            },
            Some(Box::new(|_| Some(false))),
        );
        fs.want(std::slice::from_ref(&r), std::slice::from_ref(&r));
        assert!(wait_known_bytes(&fs, &r, 350));
        {
            let st = fs.shared.state.lock();
            assert_eq!(st.roots.get(&r), Some(&RootState::Failed));
            assert!(!st.cache[&r].trusted);
        }
        write(&r.join("c").join("later.bin"), 1000);
        // No event and no new `want`: the total expires and is measured again.
        assert!(wait_known_bytes(&fs, &r, 1350));
    }

    #[test]
    fn want_at_skips_an_unchanged_generation_without_looking_at_the_folders() {
        let mut fs = FolderSizes::with_walk(
            egui::Context::default(),
            |_, _, _, _| Ok(DirTotals::default()),
            quick(),
        );
        let a = PathBuf::from(r"C:\fake\a");
        let b = PathBuf::from(r"C:\fake\b");
        fs.want_at(1, std::slice::from_ref(&a), &[]);
        assert!(fs.shared.state.lock().wanted.contains_key(&a));
        // Same generation: the (deliberately different) list is not even read.
        fs.want_at(1, std::slice::from_ref(&b), &[]);
        assert!(!fs.shared.state.lock().wanted.contains_key(&b));
        fs.want_at(2, std::slice::from_ref(&b), &[]);
        let st = fs.shared.state.lock();
        assert!(st.wanted.contains_key(&b) && !st.wanted.contains_key(&a));
    }

    #[test]
    fn a_partial_total_is_measured_again_after_a_while() {
        // The folder is unreadable at first ("+"), then readable: no event says so.
        let walks = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&walks);
        let mut fs = FolderSizes::with_parts(
            egui::Context::default(),
            move |_, _, _, _| {
                let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
                Ok(DirTotals {
                    bytes: if first { 10 } else { 30 },
                    files: 1,
                    dirs: 0,
                    partial: first,
                    ..DirTotals::default()
                })
            },
            Timing {
                partial_max_age: Duration::from_millis(300),
                ..quick()
            },
            Some(Box::new(|_| Some(true))),
        );
        let root = PathBuf::from(r"C:\fake\locked");
        fs.want(std::slice::from_ref(&root), std::slice::from_ref(&root));
        assert!(wait_for(LIMIT, || known(&fs, &root).is_some_and(|t| t.partial)));
        assert!(
            !fs.shared.state.lock().cache[&root].trusted,
            "later walks must read it again"
        );
        // No event and no new `want`: the partial total expires and is measured again.
        assert!(wait_known_bytes(&fs, &root, 30));
        assert!(!known(&fs, &root).unwrap().partial);
        assert!(
            fs.shared.state.lock().cache[&root].trusted,
            "a complete total under a watch"
        );
    }

    #[test]
    fn the_same_partial_total_is_measured_again_less_and_less_often() {
        // A folder that stays unreadable (System Volume Information): the same "+" every time.
        const SAME: DirTotals = DirTotals {
            bytes: 10,
            files: 1,
            dirs: 1,
            partial: true,
            unreadable: 1,
            unreadable_hash: 0x5eed,
        };
        let starts = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let recorded = Arc::clone(&starts);
        let timing = Timing {
            partial_max_age: Duration::from_millis(100),
            partial_max_backoff: Duration::from_millis(800),
            ..quick()
        };
        let mut fs = FolderSizes::with_parts(
            egui::Context::default(),
            move |_, _, _, _| {
                recorded.lock().push(Instant::now());
                Ok(SAME)
            },
            timing,
            Some(Box::new(|_| Some(true))),
        );
        let root = PathBuf::from(r"C:\fake\volume");
        let walks = |n: usize| {
            assert!(
                wait_for(LIMIT, || starts.lock().len() >= n),
                "walk {n} never came"
            );
            starts.lock().clone()
        };
        let gap = |s: &[Instant], i: usize| s[i] - s[i - 1];
        fs.want(std::slice::from_ref(&root), std::slice::from_ref(&root));
        // Fresh for 100 ms, then 200 ms, then 400 ms (instead of 100 ms each time).
        let s = walks(4);
        assert!(gap(&s, 2) >= Duration::from_millis(180), "{:?}", gap(&s, 2));
        assert!(gap(&s, 3) >= Duration::from_millis(380), "{:?}", gap(&s, 3));
        // A refresh (F5) measures now and starts over at 100 ms (not 800 ms).
        fs.invalidate_tree(&root);
        let n = walks(5).len();
        let s = walks(n + 1);
        assert!(gap(&s, n) < Duration::from_millis(500), "{:?}", gap(&s, n));
        // So does a change below the folder: after the debounce, again 100 ms.
        let s = walks(n + 2);
        assert!(
            gap(&s, n + 1) >= Duration::from_millis(180),
            "backing off again"
        );
        fs.invalidate(&root.join("changed.bin"));
        let n = walks(s.len() + 1).len();
        let s = walks(n + 1);
        assert!(gap(&s, n) < Duration::from_millis(500), "{:?}", gap(&s, n));

        // Another set of unreadable folders (or other bytes) starts over too.
        let mut st = fs.shared.state.lock();
        let now = Instant::now();
        let kept = Cached {
            totals: SAME,
            at: now,
            fresh: true,
            trusted: false,
            until: Some(now),
            partial_delay: Duration::from_millis(400),
        };
        st.cache.insert(root.clone(), kept);
        let delay = |st: &State, totals| {
            st.classify(&root, false, true, false, totals, now, &timing)
                .partial_delay
        };
        assert_eq!(delay(&st, SAME), Duration::from_millis(800));
        let other = DirTotals {
            unreadable_hash: 0xbeef,
            ..SAME
        };
        assert_eq!(delay(&st, other), Duration::from_millis(100));
        assert_eq!(
            delay(&st, DirTotals { bytes: 11, ..SAME }),
            Duration::from_millis(100)
        );
        st.cache.get_mut(&root).unwrap().partial_delay = Duration::from_millis(800);
        assert_eq!(delay(&st, SAME), Duration::from_millis(800), "capped");
    }

    #[test]
    fn only_roots_with_wanted_folders_are_watched() {
        let mut fs = FolderSizes::with_walk(
            egui::Context::default(),
            |_, _, _, _| Ok(DirTotals::default()),
            quick(),
        );
        let pane = PathBuf::from(r"C:\fake\pane");
        let other = PathBuf::from(r"C:\fake\other");
        fs.want(&[], &[pane.clone(), other.clone()]);
        assert!(
            fs.shared.state.lock().roots.is_empty(),
            "nothing to measure, no watch"
        );
        fs.want(&[pane.join("sub")], &[pane.clone(), other.clone()]);
        let roots: Vec<PathBuf> = fs.shared.state.lock().roots.keys().cloned().collect();
        assert_eq!(roots, vec![pane]);
    }

    #[test]
    fn busy_folders_and_drive_roots_are_measured_less_often() {
        let fs = real(quick());
        let timing = Timing {
            debounce: Duration::from_millis(10),
            min_gap: Duration::from_millis(100),
            busy_gap: Duration::from_secs(1),
            ..Timing::default()
        };
        let mut st = fs.shared.state.lock();
        st.roots.insert(PathBuf::from(r"C:\"), RootState::Watched);
        let now = Instant::now();
        for (p, events) in [(r"D:\quiet", 1), (r"D:\busy", BUSY_EVENTS), (r"C:\x", 1)] {
            let p = PathBuf::from(p);
            st.dirty.insert(
                p.clone(),
                Dirty {
                    first: now,
                    last: now,
                    events,
                },
            );
            st.last_start.insert(p, now);
        }
        st.promote_due(now + Duration::from_millis(150), &timing);
        let queued = |st: &State, p: &str| st.queue.contains(Path::new(p));
        assert!(queued(&st, r"D:\quiet"));
        assert!(!queued(&st, r"D:\busy"));
        assert!(!queued(&st, r"C:\x"), "below a watched drive root");
        st.promote_due(now + Duration::from_millis(1050), &timing);
        assert!(queued(&st, r"D:\busy") && queued(&st, r"C:\x"));
    }

    #[test]
    fn repeated_events_on_a_folder_neither_wake_workers_nor_scan() {
        let fs = real(quick());
        let mut st = fs.shared.state.lock();
        st.wanted.insert(
            PathBuf::from(r"C:\r\w"),
            Wanted {
                order: 0,
                network: false,
            },
        );
        let now = Instant::now();
        let f = Path::new(r"C:\r\w\deep\file.tmp");
        assert_eq!(
            st.invalidate_path(f, Change::Appeared, now),
            (false, true),
            "new debounce"
        );
        assert_eq!(st.invalidate_path(f, Change::Content, now), (false, false));
        assert_eq!(st.dirty[Path::new(r"C:\r\w")].events, 2);
        // A measured folder's own timestamps (our walk listed it) say nothing about sizes.
        let deep = PathBuf::from(r"C:\r\w\deep");
        st.insert(
            deep.clone(),
            Cached {
                totals: DirTotals::default(),
                at: now,
                fresh: true,
                trusted: true,
                until: None,
                partial_delay: Duration::ZERO,
            },
        );
        assert_eq!(
            st.invalidate_path(&deep, Change::Modified, now),
            (false, false)
        );
        assert!(st.cache[&deep].trusted);
        assert_eq!(st.dirty[Path::new(r"C:\r\w")].events, 2);
        // A file's data changing does count.
        st.invalidate_path(&deep.join("file.bin"), Change::Modified, now);
        assert!(!st.cache[&deep].fresh);
        // The next event in that folder takes the fast path, unless something was measured
        // since: a fresh total of an ancestor must go stale again.
        let w = PathBuf::from(r"C:\r\w");
        st.insert(
            w.clone(),
            Cached {
                totals: DirTotals::default(),
                at: now,
                fresh: true,
                trusted: true,
                until: None,
                partial_delay: Duration::ZERO,
            },
        );
        assert_eq!(
            st.invalidate_path(&deep.join("other.bin"), Change::Content, now),
            (true, false)
        );
        assert!(!st.cache[&w].fresh);
        assert_eq!(st.dirty[&w].events, 4);
        assert!(
            st.purge.is_empty(),
            "no total kept at the path: nothing to purge"
        );
    }
}
