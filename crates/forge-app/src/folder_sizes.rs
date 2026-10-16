//! Recursive folder totals for the Size column, measured in the background.
//!
//! The panes say which folders are on screen ([`FolderSizes::want`]) and read totals with
//! [`FolderSizes::get`], which is one lock and a hash lookup, so it can run for every row
//! every frame. A small pool of worker threads measures wanted folders in on-screen order
//! with [`forge_core::dir_size::dir_totals`], and every sub-folder a walk completes is kept,
//! so opening a sub-folder later shows its sizes at once.
//!
//! A recursive change watch on each local folder a pane shows decides which kept totals can
//! be trusted: while it covers a folder, any change below it arrives as an event, so a total
//! measured under the watch stays right until an event says otherwise. Later walks skip such
//! trusted sub-trees and only re-read the branch that changed. Network folders are never
//! watched (a recursive watch on a share is expensive and unreliable), so their totals are
//! never reused by walks and are re-measured when they are older than a few minutes.
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
/// Network totals are never trusted (no watch); a wanted one older than this is measured
/// again.
const NETWORK_MAX_AGE: Duration = Duration::from_secs(5 * 60);
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

#[derive(Debug, Clone, Copy)]
struct Timing {
    debounce: Duration,
    min_gap: Duration,
    network_max_age: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            debounce: DEBOUNCE,
            min_gap: MIN_GAP,
            network_max_age: NETWORK_MAX_AGE,
        }
    }
}

/// A kept total.
#[derive(Debug, Clone, Copy)]
struct Cached {
    totals: DirTotals,
    at: Instant,
    /// `get` shows it as `Known`. Cleared by a change below the folder or when the watch
    /// that covered it went away; the total is then shown as stale until measured again.
    fresh: bool,
    /// Measured while a recursive watch covered the folder, and nothing changed since: walks
    /// may use it instead of entering the folder. Implies `fresh`.
    trusted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootState {
    /// Handed to the registrar thread, not watched yet.
    Registering,
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

/// A wanted folder with changes below it, waiting for the debounce.
struct Dirty {
    first: Instant,
    last: Instant,
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
    /// The local folders the panes show, each with a recursive watch.
    roots: HashMap<PathBuf, RootState>,
    /// Drive letter -> mapped to a network share. Asked once per letter.
    drives: HashMap<u8, bool>,
    shutdown: bool,
}

struct Shared {
    state: Mutex<State>,
    /// Workers wait here for jobs (and for the next debounced folder to be due).
    wake: Condvar,
    revision: AtomicU64,
    ctx: egui::Context,
    walk: Box<WalkFn>,
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
    /// `last_*` have been applied under the current settings.
    applied: bool,
}

impl FolderSizes {
    /// Threads start lazily, on the first folder wanted.
    pub fn new(ctx: egui::Context) -> Self {
        Self::with_walk(ctx, dir_size::dir_totals, Timing::default())
    }

    fn with_walk(
        ctx: egui::Context,
        walk: impl Fn(&Path, &dyn SizeCache, usize, &CancelToken) -> forge_core::Result<DirTotals>
        + Send
        + Sync
        + 'static,
        timing: Timing,
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
            roots: HashMap::new(),
            drives: HashMap::new(),
            shutdown: false,
        };
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(state),
                wake: Condvar::new(),
                revision: AtomicU64::new(0),
                ctx,
                walk: Box::new(walk),
                timing,
                local_threads,
            }),
            workers,
            started: false,
            register: None,
            last_folders: Vec::new(),
            last_roots: Vec::new(),
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
    /// folders the panes show (`roots`, for the recursive change watch). Cheap when both are
    /// unchanged, so it may be called every frame. Queues measurements for wanted folders
    /// without a fresh result; cancels running/queued work for folders no longer wanted.
    pub fn want(&mut self, folders: &[PathBuf], roots: &[PathBuf]) {
        if self.applied
            && folders == self.last_folders.as_slice()
            && roots == self.last_roots.as_slice()
        {
            return;
        }
        self.last_folders = folders.to_vec();
        self.last_roots = roots.to_vec();
        self.applied = true;
        self.apply();
    }

    fn apply(&mut self) {
        let now = Instant::now();
        let timing = self.shared.timing;
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

        let mut local_roots: Vec<PathBuf> = Vec::new();
        if st.enabled {
            for root in &self.last_roots {
                if !network_of(&mut st.drives, root) && !local_roots.contains(root) {
                    local_roots.push(root.clone());
                }
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
                st.roots.insert(root.clone(), RootState::Registering);
                roots_changed = true;
            }
        }
        // Totals under a watch that goes away can no longer be trusted. The registrar keeps
        // an old watch until the new ones are set up, so a folder still covered by a new
        // root (going into a sub-folder) stays trusted.
        changed |= st.demote_uncovered(&dropped, true);

        for (path, w) in &wanted {
            if st.dirty.contains_key(path) || st.running.contains_key(path) {
                continue; // Measured when the debounce ends / re-checked when the walk ends.
            }
            let needed = match st.cache.get_mut(path) {
                None => true,
                Some(c) => {
                    if w.network && c.fresh && now.duration_since(c.at) >= timing.network_max_age {
                        c.fresh = false;
                        changed = true;
                    }
                    !c.fresh
                }
            };
            if needed {
                st.queue.insert(path.clone());
            }
        }
        st.wanted = wanted;
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
            Some(c) if c.fresh => FolderSize::Known(c.totals),
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

    /// Wanted folders still waiting to be measured, being measured or waiting for changes
    /// to settle (for a "measuring…" hint).
    pub fn pending_count(&self) -> usize {
        let st = self.shared.state.lock();
        st.wanted
            .keys()
            .filter(|p| {
                st.queue.contains(*p) || st.running.contains_key(*p) || st.dirty.contains_key(*p)
            })
            .count()
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
            true,
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
    /// Entries in or below any of `gone` that no remaining root covers lose their trust
    /// (and, with `stale`, show as stale until measured again).
    fn demote_uncovered(&mut self, gone: &[PathBuf], stale: bool) -> bool {
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
            if stale && c.fresh {
                c.fresh = false;
                changed = true;
            }
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

    /// `path` changed: its own entry, its folder and every ancestor become stale, running
    /// walks of those folders are told, and wanted ones wait for the debounce. A folder that
    /// appears (`structural`: created, renamed, moved in) at a path with a kept total also
    /// loses the totals kept below that path: they belong to whatever was there before, and
    /// moving a tree in reports only its top folder.
    fn invalidate_path(&mut self, path: &Path, structural: bool, now: Instant) -> bool {
        let mut changed = false;
        if structural && self.cache.contains_key(path) {
            let before = self.cache.len();
            self.cache.retain(|k, _| k == path || !is_within(k, path));
            changed |= self.cache.len() != before;
        }
        let mut cur = Some(path);
        while let Some(p) = cur {
            if let Some(c) = self.cache.get_mut(p) {
                changed |= c.fresh;
                c.fresh = false;
                c.trusted = false;
            }
            if let Some(job) = self.running.get_mut(p) {
                job.touch(path);
            }
            if self.wanted.contains_key(p) {
                self.dirty
                    .entry(p.to_path_buf())
                    .and_modify(|d| d.last = now)
                    .or_insert(Dirty {
                        first: now,
                        last: now,
                    });
            }
            cur = p.parent();
        }
        changed
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
            }
        }
        for (path, job) in &mut self.running {
            if is_within(path, dir) || is_within(dir, path) {
                job.touched_all = true;
            }
        }
        changed
    }

    /// Moves wanted folders whose changes have settled into the queue. Returns when the next
    /// waiting one will be due.
    fn promote_due(&mut self, now: Instant, timing: &Timing) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut due = Vec::new();
        for (path, d) in &self.dirty {
            // Quiet for the debounce, or changing for a while already (a long copy)...
            let settled = (d.last + timing.debounce).min(d.first + timing.min_gap);
            // ...and not measured within the last gap.
            let spaced = self
                .last_start
                .get(path)
                .map_or(now, |t| *t + timing.min_gap);
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
        next
    }

    /// The queued folder to measure next: the first on screen whose previous walk has ended,
    /// skipping network folders while the network slots are taken.
    fn pick(&mut self) -> Option<(PathBuf, bool)> {
        let (path, network) = self
            .queue
            .iter()
            .filter(|p| !self.running.contains_key(*p))
            .filter_map(|p| self.wanted.get(p).map(|w| (w, p)))
            .filter(|(w, _)| !w.network || self.network_running < NETWORK_JOBS)
            .min_by_key(|(w, _)| w.order)
            .map(|(w, p)| (p.clone(), w.network))?;
        self.queue.remove(&path);
        Some((path, network))
    }

    fn insert(&mut self, path: PathBuf, cached: Cached) {
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
    fn finish(&mut self, path: PathBuf, result: forge_core::Result<DirTotals>) {
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
            let trusted = job.trusted && !tainted && self.watched_cover(&path);
            self.insert(
                path.clone(),
                Cached {
                    totals,
                    at: now,
                    fresh: !tainted,
                    trusted,
                },
            );
        }
        // Cancelled, or changed while it ran, and still wanted: measure again.
        let fresh = self.cache.get(&path).is_some_and(|c| c.fresh);
        if self.wanted.contains_key(&path) && !fresh && !self.dirty.contains_key(&path) {
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
        let trusted = job.trusted && !tainted && st.watched_cover(dir);
        let wanted = st.wanted.contains_key(dir);
        st.insert(
            dir.to_path_buf(),
            Cached {
                totals: *totals,
                at: Instant::now(),
                fresh: !tainted,
                trusted,
            },
        );
        drop(st);
        if wanted {
            self.shared.bump();
        }
    }
}

fn worker(shared: Arc<Shared>) {
    let mut st = shared.state.lock();
    loop {
        if st.shutdown {
            return;
        }
        let now = Instant::now();
        let next_due = st.promote_due(now, &shared.timing);
        let Some((path, network)) = st.pick() else {
            match next_due {
                Some(at) => {
                    shared.wake.wait_until(&mut st, at);
                }
                None => shared.wake.wait(&mut st),
            }
            continue;
        };
        let cancel = CancelToken::new();
        let trusted = !network && st.watched_cover(&path);
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
        st.finish(path, result);
        if !cancelled {
            shared.bump();
            shared.ctx.request_repaint();
        }
        // Another job may have become startable (a network slot freed, a folder whose walk
        // was running is queued again).
        shared.wake.notify_all();
    }
}

/// Marks `paths` changed (see [`State::invalidate_path`]) and wakes the workers so the
/// debounce timer covers them.
fn invalidate_paths(shared: &Shared, paths: &[PathBuf], structural: bool) {
    let now = Instant::now();
    let mut st = shared.state.lock();
    let mut changed = false;
    for path in paths {
        changed |= st.invalidate_path(path, structural, now);
    }
    drop(st);
    if changed {
        shared.bump();
        shared.ctx.request_repaint();
    }
    shared.wake.notify_all();
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
            st.dirty
                .entry(w)
                .and_modify(|d| d.last = now)
                .or_insert(Dirty {
                    first: now,
                    last: now,
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
            let structural = matches!(
                ev.kind,
                EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
            );
            invalidate_paths(shared, &ev.paths, structural);
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
            if watched.contains(root) {
                continue;
            }
            let ok = watcher
                .as_mut()
                .is_some_and(|w| w.watch(root, RecursiveMode::Recursive).is_ok());
            if ok {
                watched.insert(root.clone());
            }
            let mut st = shared.state.lock();
            if let Some(state) = st.roots.get_mut(root) {
                if ok {
                    *state = RootState::Watched;
                } else {
                    *state = RootState::Failed;
                    // Totals measured under an old watch were right until now; they just
                    // cannot be relied on from here on.
                    st.demote_uncovered(std::slice::from_ref(root), false);
                }
            }
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
            network_max_age: NETWORK_MAX_AGE,
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
                })
            },
            quick(),
        );
        let dir = PathBuf::from(r"C:\fake\folder");
        let rev = fs.revision();
        fs.want(std::slice::from_ref(&dir), &[]);
        assert_eq!(fs.get(&dir), FolderSize::Pending { stale: None });
        assert_eq!(fs.pending_count(), 1);
        gate_tx.send(()).unwrap();
        assert!(wait_for(LIMIT, || known(&fs, &dir).is_some()));
        assert_eq!(known(&fs, &dir).unwrap().bytes, 42);
        assert!(fs.revision() > rev);
        assert!(fs.wait_idle(LIMIT));
        assert_eq!(fs.pending_count(), 0);
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
            network_max_age: NETWORK_MAX_AGE,
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
        // Watch first, so the first walk is already trusted.
        fs.want(&[], std::slice::from_ref(&r));
        assert!(fs.wait_watched(&r, LIMIT));
        fs.want(std::slice::from_ref(&r), std::slice::from_ref(&r));
        assert!(wait_known_bytes(&fs, &r, 350));
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
        fs.want(&[], std::slice::from_ref(&r));
        assert!(fs.wait_watched(&r, LIMIT));
        fs.want(std::slice::from_ref(&a), std::slice::from_ref(&r));
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
        };
        for p in [r"C:\r\x", r"C:\r\x\y", r"C:\r\xy", r"C:\r"] {
            st.insert(PathBuf::from(p), c);
        }
        st.invalidate_path(Path::new(r"C:\r\x"), true, Instant::now());
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
}
