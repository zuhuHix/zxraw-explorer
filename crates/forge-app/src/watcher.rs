//! Live refresh: watches the folders currently shown (non-recursively) and reports which
//! ones changed, debounced so bursts of writes (a copy, an export) cause one refresh.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, select, unbounded};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Quiet period after the last event before a folder is refreshed.
const DEBOUNCE: Duration = Duration::from_millis(400);
/// Never refresh the same folder more often than this (big folders get a longer gap).
const MIN_GAP: Duration = Duration::from_millis(1000);
const MIN_GAP_LARGE: Duration = Duration::from_secs(4);
/// Waits before trying again to watch a folder that could not be watched (an offline share,
/// a removed drive). The last wait repeats for as long as the folder is shown.
const RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
];
/// Network folders being registered at once. Each attempt runs on its own thread with its
/// own watcher, so a share that stalls holds up neither local folders nor other shares.
const NETWORK_SLOTS: usize = 4;
/// A network registration still running after this long frees its slot: its thread is left
/// to finish on its own (it may be stuck in an open call on an offline share), but no longer
/// keeps other shares from being registered.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(30);
/// Timed-out registrations allowed to linger at once. While this many are stuck, no new
/// network registration starts, so stuck threads cannot pile up.
const HUNG_CAP: usize = 8;
/// Threads that drop released network watchers. Closing a handle on an offline share can
/// block, so it happens here, never on the registrar, and never on a thread per release.
const CLEANUP_THREADS: usize = 2;

pub struct FolderWatcher {
    /// The folders to watch, sent to the registrar thread. Registering opens each folder
    /// (notify checks it and opens a handle), which blocks for as long as an offline share
    /// takes to fail, so it never happens on the UI thread.
    register: Option<Sender<HashSet<PathBuf>>>,
    /// What the registrar has done with each wanted folder (tests read it).
    #[cfg_attr(not(test), allow(dead_code))]
    registrations: Arc<Mutex<HashMap<PathBuf, Registration>>>,
    rx: Receiver<PathBuf>,
    /// Directories shown by panes.
    watched: HashSet<PathBuf>,
    /// Copy of `watched` for the notify thread, which probes the filesystem so the UI
    /// thread never has to (an offline network path can block for seconds).
    shown: Arc<Mutex<HashSet<PathBuf>>>,
    /// dir -> time of the most recent event
    pending: HashMap<PathBuf, Instant>,
    last_refresh: HashMap<PathBuf, Instant>,
    ctx: egui::Context,
}

/// Where notify events go. Every watcher (the shared local one and one per network folder)
/// gets a handler built from a clone.
#[derive(Clone)]
struct EventSink {
    tx: Sender<PathBuf>,
    shown: Arc<Mutex<HashSet<PathBuf>>>,
    repaint: egui::Context,
}

impl EventSink {
    fn handler(self) -> impl FnMut(notify::Result<notify::Event>) + Send + 'static {
        move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else {
                return;
            };
            // Ignore pure access events; everything else may change the listing.
            if matches!(ev.kind, notify::EventKind::Access(_)) {
                return;
            }
            for p in ev.paths {
                let _ = self.tx.send(p.clone());
                if let Some(parent) = p.parent() {
                    let _ = self.tx.send(parent.to_path_buf());
                    // Windows may report a shown folder's deletion only as an event in
                    // its parent. Probe the parent's shown children here, on notify's
                    // thread, and report the ones that are gone.
                    let gone: Vec<PathBuf> = self
                        .shown
                        .lock()
                        .map(|shown| {
                            shown
                                .iter()
                                .filter(|dir| dir.parent() == Some(parent) || dir.parent() == Some(p.as_path()))
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    for dir in gone.into_iter().filter(|dir| !dir.is_dir()) {
                        let _ = self.tx.send(dir);
                    }
                }
            }
            self.repaint.request_repaint_after(DEBOUNCE);
        }
    }
}

/// The registrar's state for one wanted folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Registration {
    Watched,
    /// An attempt is running.
    Trying,
    /// The last `attempts` attempts failed; the next one starts at `retry_at`.
    Failed { attempts: usize, retry_at: Instant },
}

/// A finished attempt to watch `path`. A network attempt brings its own watcher.
struct Attempt {
    path: PathBuf,
    network: bool,
    result: Result<Option<RecommendedWatcher>, ()>,
}

enum LocalCommand {
    Watch(PathBuf),
    Unwatch(PathBuf),
}

/// Keeps the watched folders equal to the newest wanted set without ever waiting on one
/// folder itself. Local folders are registered in order on one worker that owns a shared
/// watcher; each network folder is registered on its own thread (at most `NETWORK_SLOTS`
/// at once) with a watcher of its own, so a stalled share delays nothing else. A folder that
/// cannot be watched is tried again after `backoff`, until it is no longer wanted.
struct Registrar {
    sink: EventSink,
    backoff: Vec<Duration>,
    local: Sender<LocalCommand>,
    results: Sender<Attempt>,
    network_watchers: HashMap<PathBuf, RecommendedWatcher>,
    /// Network attempts holding a slot (running and not timed out).
    network_trying: usize,
    /// Network attempts that timed out and are still running.
    hung: usize,
    register_timeout: Duration,
    /// Released network watchers go here to be dropped by the cleanup pool.
    cleanup: Sender<RecommendedWatcher>,
    want: HashSet<PathBuf>,
    /// Attempts still running, wanted or not.
    trying: HashMap<PathBuf, Running>,
    state: Arc<Mutex<HashMap<PathBuf, Registration>>>,
}

/// An attempt that has not finished yet.
struct Running {
    network: bool,
    started: Instant,
    /// It ran past the registration timeout and gave up its slot.
    timed_out: bool,
}

/// Starts `threads` threads that drop whatever is sent to them, in order. Dropping can block
/// (closing a watch on an offline share); a few fixed threads with a queue keep that off the
/// sender without a thread per item.
fn cleanup_pool<T: Send + 'static>(threads: usize) -> Sender<T> {
    let (tx, rx) = unbounded::<T>();
    for i in 0..threads {
        let rx = rx.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("folder-watch-cleanup-{i}"))
            .spawn(move || {
                for item in rx {
                    drop(item);
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("folder watch cleanup thread unavailable: {e}");
        }
    }
    tx
}

impl Registrar {
    fn run(mut self, wanted: Receiver<HashSet<PathBuf>>, results: Receiver<Attempt>) {
        loop {
            let wake = match (self.next_retry(), self.next_timeout()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let wait = wake.map_or(Duration::from_secs(3600), |at| {
                at.saturating_duration_since(Instant::now())
            });
            select! {
                recv(wanted) -> set => match set {
                    Ok(mut set) => {
                        while let Ok(newer) = wanted.try_recv() {
                            set = newer;
                        }
                        self.set_want(set);
                    }
                    Err(_) => return,
                },
                recv(results) -> attempt => {
                    if let Ok(attempt) = attempt {
                        self.finished(attempt);
                    }
                }
                default(wait) => {}
            }
            self.start_due();
        }
    }

    fn set_want(&mut self, want: HashSet<PathBuf>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let gone: Vec<PathBuf> = state.keys().filter(|p| !want.contains(*p)).cloned().collect();
        // A running attempt for a folder no longer wanted is cleaned up when it finishes
        // (see `finished`).
        let watched: Vec<PathBuf> = gone
            .into_iter()
            .filter(|path| state.remove(path) == Some(Registration::Watched))
            .collect();
        drop(state);
        for path in watched {
            self.release(&path);
        }
        self.want = want;
    }

    /// Stops watching `path`, which was watched.
    fn release(&mut self, path: &Path) {
        match self.network_watchers.remove(path) {
            // Closing a handle on an offline share can block too: not on this thread.
            Some(watcher) => {
                let _ = self.cleanup.send(watcher);
            }
            None => {
                let _ = self.local.send(LocalCommand::Unwatch(path.to_path_buf()));
            }
        }
    }

    fn finished(&mut self, attempt: Attempt) {
        let Attempt {
            path,
            network,
            result,
        } = attempt;
        match self.trying.remove(&path) {
            Some(Running {
                timed_out: true, ..
            }) => {
                self.hung = self.hung.saturating_sub(1);
                tracing::info!("a timed-out watch registration of {} finished", path.display());
            }
            _ if network => self.network_trying = self.network_trying.saturating_sub(1),
            _ => {}
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.want.contains(&path) {
            drop(state);
            if let Ok(watcher) = result {
                match watcher {
                    Some(watcher) => {
                        let _ = self.cleanup.send(watcher);
                    }
                    None => {
                        let _ = self.local.send(LocalCommand::Unwatch(path));
                    }
                }
            }
            return;
        }
        let attempts = match state.get(&path) {
            Some(Registration::Failed { attempts, .. }) => *attempts,
            _ => 0,
        };
        match result {
            Ok(watcher) => {
                if let Some(watcher) = watcher {
                    self.network_watchers.insert(path.clone(), watcher);
                }
                state.insert(path, Registration::Watched);
            }
            Err(()) => {
                let wait = self.backoff[attempts.min(self.backoff.len() - 1)];
                state.insert(
                    path,
                    Registration::Failed {
                        attempts: attempts + 1,
                        retry_at: Instant::now() + wait,
                    },
                );
            }
        }
    }

    fn next_retry(&self) -> Option<Instant> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .iter()
            .filter(|(path, _)| !self.trying.contains_key(*path))
            .filter_map(|(_, r)| match r {
                Registration::Failed { retry_at, .. } => Some(*retry_at),
                _ => None,
            })
            .min()
    }

    /// When the oldest network attempt still holding a slot times out.
    fn next_timeout(&self) -> Option<Instant> {
        self.trying
            .values()
            .filter(|r| r.network && !r.timed_out)
            .map(|r| r.started + self.register_timeout)
            .min()
    }

    /// Frees the slots of network attempts that ran past the timeout. Their threads keep
    /// running (they are stuck in a call that cannot be cancelled) and count as hung.
    fn expire_timeouts(&mut self, now: Instant) {
        for (path, running) in &mut self.trying {
            if running.network
                && !running.timed_out
                && now.saturating_duration_since(running.started) >= self.register_timeout
            {
                running.timed_out = true;
                self.network_trying = self.network_trying.saturating_sub(1);
                self.hung += 1;
                tracing::warn!(
                    "watching {} has not answered for {:?}; registering other shares meanwhile",
                    path.display(),
                    self.register_timeout
                );
            }
        }
    }

    /// Starts an attempt for every wanted folder that is new or due for a retry.
    fn start_due(&mut self) {
        let now = Instant::now();
        self.expire_timeouts(now);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut due: Vec<PathBuf> = self
            .want
            .iter()
            .filter(|p| !self.trying.contains_key(*p))
            .filter(|p| match state.get(*p) {
                None => true,
                Some(Registration::Failed { retry_at, .. }) => *retry_at <= now,
                Some(_) => false,
            })
            .cloned()
            .collect();
        due.sort();
        for path in due {
            let network = forge_core::netpath::is_network_path(&path);
            if network {
                if self.network_trying >= NETWORK_SLOTS || self.hung >= HUNG_CAP {
                    continue; // Started when a slot frees up (or a stuck attempt ends).
                }
                self.network_trying += 1;
                let (sink, results, target) =
                    (self.sink.clone(), self.results.clone(), path.clone());
                let spawned = std::thread::Builder::new()
                    .name("folder-watch-network".into())
                    .spawn(move || {
                        let result = notify::recommended_watcher(sink.handler())
                            .and_then(|mut watcher| {
                                watcher.watch(&target, RecursiveMode::NonRecursive)?;
                                Ok(watcher)
                            })
                            .map(Some)
                            .map_err(|_| ());
                        let _ = results.send(Attempt {
                            path: target,
                            network: true,
                            result,
                        });
                    });
                if spawned.is_err() {
                    self.network_trying -= 1;
                    continue;
                }
            } else if self.local.send(LocalCommand::Watch(path.clone())).is_err() {
                continue;
            }
            // Keep the attempt count of a retry; `finished` reads it.
            if !matches!(state.get(&path), Some(Registration::Failed { .. })) {
                state.insert(path.clone(), Registration::Trying);
            }
            self.trying.insert(
                path,
                Running {
                    network,
                    started: now,
                    timed_out: false,
                },
            );
        }
    }
}

/// Owns the watcher shared by local folders and registers them one at a time, in order.
fn local_loop(mut watcher: RecommendedWatcher, commands: Receiver<LocalCommand>, results: Sender<Attempt>) {
    for command in commands {
        match command {
            LocalCommand::Watch(path) => {
                let result = watcher
                    .watch(&path, RecursiveMode::NonRecursive)
                    .map(|()| None)
                    .map_err(|_| ());
                let _ = results.send(Attempt {
                    path,
                    network: false,
                    result,
                });
            }
            LocalCommand::Unwatch(path) => {
                let _ = watcher.unwatch(&path);
            }
        }
    }
}

impl FolderWatcher {
    pub fn new(ctx: egui::Context) -> Self {
        Self::with_timing(ctx, RETRY_BACKOFF.to_vec(), REGISTER_TIMEOUT)
    }

    #[cfg(test)]
    fn with_backoff(ctx: egui::Context, backoff: Vec<Duration>) -> Self {
        Self::with_timing(ctx, backoff, REGISTER_TIMEOUT)
    }

    fn with_timing(ctx: egui::Context, backoff: Vec<Duration>, register_timeout: Duration) -> Self {
        let (tx, rx) = unbounded();
        let shown = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
        let sink = EventSink {
            tx,
            shown: Arc::clone(&shown),
            repaint: ctx.clone(),
        };
        let registrations = Arc::new(Mutex::new(HashMap::new()));
        let register =
            Self::start_registrar(sink, backoff, register_timeout, Arc::clone(&registrations));
        Self {
            register,
            registrations,
            rx,
            watched: HashSet::new(),
            shown,
            pending: HashMap::new(),
            last_refresh: HashMap::new(),
            ctx,
        }
    }

    fn start_registrar(
        sink: EventSink,
        backoff: Vec<Duration>,
        register_timeout: Duration,
        state: Arc<Mutex<HashMap<PathBuf, Registration>>>,
    ) -> Option<Sender<HashSet<PathBuf>>> {
        let watcher = notify::recommended_watcher(sink.clone().handler())
            .map_err(|e| tracing::warn!("folder watcher unavailable: {e}"))
            .ok()?;
        let (results_tx, results_rx) = unbounded();
        let (local_tx, local_rx) = unbounded();
        let local_results = results_tx.clone();
        std::thread::Builder::new()
            .name("folder-watch-local".into())
            .spawn(move || local_loop(watcher, local_rx, local_results))
            .map_err(|e| tracing::warn!("folder watcher unavailable: {e}"))
            .ok()?;
        let (register, wanted) = unbounded();
        let registrar = Registrar {
            sink,
            backoff: if backoff.is_empty() {
                RETRY_BACKOFF.to_vec()
            } else {
                backoff
            },
            local: local_tx,
            results: results_tx,
            network_watchers: HashMap::new(),
            network_trying: 0,
            hung: 0,
            register_timeout,
            cleanup: cleanup_pool(CLEANUP_THREADS),
            want: HashSet::new(),
            trying: HashMap::new(),
            state,
        };
        std::thread::Builder::new()
            .name("folder-watch-register".into())
            .spawn(move || registrar.run(wanted, results_rx))
            .map_err(|e| tracing::warn!("folder watcher unavailable: {e}"))
            .ok()?;
        Some(register)
    }

    /// Keep the watch set equal to `dirs` (the folders on screen). Touches no folder: the
    /// registrar thread does that.
    pub fn sync(&mut self, dirs: &[PathBuf]) {
        let Some(register) = &self.register else {
            return;
        };
        let want: HashSet<PathBuf> = dirs.iter().cloned().collect();
        if want == self.watched {
            return;
        }
        // Parents too, so a shown folder's external deletion can be noticed.
        let mut folders = want.clone();
        folders.extend(want.iter().filter_map(|dir| dir.parent().map(Path::to_path_buf)));
        let _ = register.send(folders);
        for gone in self.watched.difference(&want).cloned().collect::<Vec<_>>() {
            self.pending.remove(&gone);
        }
        if let Ok(mut shown) = self.shown.lock() {
            shown.clone_from(&want);
        }
        self.watched = want;
    }

    /// The registrar's state for `dir`, `None` if it has not looked at it (yet).
    #[cfg(test)]
    fn registration(&self, dir: &Path) -> Option<Registration> {
        self.registrations.lock().unwrap().get(dir).copied()
    }

    /// Waits up to `limit` for `dir` to be watched.
    #[cfg(test)]
    fn wait_watched(&self, dir: &Path, limit: Duration) -> bool {
        let started = Instant::now();
        while started.elapsed() < limit {
            if self.registration(dir) == Some(Registration::Watched) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// Folders that should be refreshed now. `size_of` gives the entry count for a dir so
    /// huge folders are throttled harder.
    pub fn due(&mut self, size_of: impl Fn(&Path) -> usize) -> Vec<PathBuf> {
        let now = Instant::now();
        while let Ok(dir) = self.rx.try_recv() {
            // A deleted shown folder arrives as its own path (see the notify callback),
            // so only shown folders need handling; no filesystem probe on this thread.
            if self.watched.contains(&dir) {
                self.pending.insert(dir, now);
            }
        }
        let mut out = Vec::new();
        let mut next_wake: Option<Duration> = None;
        self.pending.retain(|dir, last_event| {
            let gap = if size_of(dir) > 20_000 {
                MIN_GAP_LARGE
            } else {
                MIN_GAP
            };
            let quiet = now.duration_since(*last_event) >= DEBOUNCE;
            let spaced = self
                .last_refresh
                .get(dir)
                .is_none_or(|t| now.duration_since(*t) >= gap);
            if quiet && spaced {
                out.push(dir.clone());
                false
            } else {
                // Not eligible yet: make sure the UI wakes up when it will be, otherwise a
                // throttled change would sit here until some unrelated repaint.
                let until_quiet = DEBOUNCE.saturating_sub(now.duration_since(*last_event));
                let until_spaced = self
                    .last_refresh
                    .get(dir)
                    .map(|t| gap.saturating_sub(now.duration_since(*t)))
                    .unwrap_or_default();
                let wait = until_quiet.max(until_spaced).max(Duration::from_millis(10));
                next_wake = Some(next_wake.map_or(wait, |w| w.min(wait)));
                true
            }
        });
        if let Some(w) = next_wake {
            self.ctx.request_repaint_after(w);
        }
        for d in &out {
            self.last_refresh.insert(d.clone(), now);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TEST-NET-1 (RFC 5737) share: unroutable, so registering it stalls until the network
    /// gives up. Windows remembers an unreachable host and then fails fast, so each run picks
    /// a host the other tests (which use .1) are unlikely to have used just before.
    fn stalled_share() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        PathBuf::from(format!(r"\\192.0.2.{}\x\shoot", 2 + nanos % 250))
    }

    #[test]
    fn detects_new_file_after_debounce() {
        let d = tempfile::tempdir().unwrap();
        let mut w = FolderWatcher::new(egui::Context::default());
        let dir = d.path().to_path_buf();
        w.sync(std::slice::from_ref(&dir));
        assert!(w.wait_watched(&dir, Duration::from_secs(5)));
        std::fs::write(dir.join("new.txt"), b"x").unwrap();
        let t0 = Instant::now();
        let mut got = Vec::new();
        while t0.elapsed() < Duration::from_secs(5) && got.is_empty() {
            std::thread::sleep(Duration::from_millis(100));
            got = w.due(|_| 0);
        }
        assert_eq!(got.len(), 1, "expected one refresh for the watched folder");
        assert!(forge_core::paths::paths_equal(&got[0], &dir));
    }

    #[test]
    fn detects_external_deletion_of_a_shown_folder() {
        let parent = tempfile::tempdir().unwrap();
        let shown = parent.path().join("shown");
        std::fs::create_dir(&shown).unwrap();
        let mut watcher = FolderWatcher::new(egui::Context::default());
        watcher.sync(std::slice::from_ref(&shown));
        assert!(watcher.wait_watched(&shown, Duration::from_secs(5)));
        assert!(watcher.wait_watched(parent.path(), Duration::from_secs(5)));
        std::fs::remove_dir(&shown).unwrap();

        let started = Instant::now();
        let mut due = Vec::new();
        while started.elapsed() < Duration::from_secs(5) && due.is_empty() {
            std::thread::sleep(Duration::from_millis(100));
            due = watcher.due(|_| 0);
        }
        assert_eq!(due, vec![shown]);
    }

    #[test]
    fn showing_an_offline_share_never_waits_on_the_caller() {
        let mut watcher = FolderWatcher::new(egui::Context::default());
        let share = stalled_share();
        let started = Instant::now();
        watcher.sync(std::slice::from_ref(&share));
        watcher.sync(&[share.clone(), share.join("day1")]);
        watcher.sync(std::slice::from_ref(&share));
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn a_stalled_share_does_not_hold_up_a_local_folder() {
        let d = tempfile::tempdir().unwrap();
        let local = d.path().to_path_buf();
        let mut watcher = FolderWatcher::new(egui::Context::default());
        // The share first, so a single registration queue would be stuck on it.
        let share = stalled_share();
        watcher.sync(std::slice::from_ref(&share));
        watcher.sync(&[share.clone(), local.clone()]);
        assert!(
            watcher.wait_watched(&local, Duration::from_secs(3)),
            "the local folder waited behind the share: {:?}",
            watcher.registration(&local)
        );
        std::fs::write(local.join("new.txt"), b"x").unwrap();
        let started = Instant::now();
        let mut due = Vec::new();
        while started.elapsed() < Duration::from_secs(5) && due.is_empty() {
            std::thread::sleep(Duration::from_millis(100));
            due = watcher.due(|_| 0);
        }
        assert_eq!(due, vec![local]);
    }

    #[test]
    fn a_folder_that_could_not_be_watched_is_tried_again() {
        let d = tempfile::tempdir().unwrap();
        let later = d.path().join("later");
        let mut watcher = FolderWatcher::with_backoff(
            egui::Context::default(),
            vec![Duration::from_millis(100)],
        );
        watcher.sync(std::slice::from_ref(&later));
        // It does not exist yet, so the first attempt fails...
        let started = Instant::now();
        while !matches!(watcher.registration(&later), Some(Registration::Failed { .. }))
            && started.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(watcher.registration(&later), Some(Registration::Failed { .. })));
        // ...and once it appears, a retry watches it without the set changing.
        std::fs::create_dir(&later).unwrap();
        assert!(watcher.wait_watched(&later, Duration::from_secs(5)));
    }

    #[test]
    fn a_hung_registration_frees_its_slot_after_the_timeout() {
        // More stalled shares than slots: without the timeout the last one would wait until
        // a stuck attempt gave up (about 20 s); with it, it starts right after the timeout.
        let base = stalled_share();
        let host = |i: usize| {
            let text = base.to_string_lossy().into_owned();
            let rest = text.trim_start_matches(r"\\192.0.2.");
            let first: usize = rest.split('\\').next().unwrap().parse().unwrap();
            PathBuf::from(format!(r"\\192.0.2.{}\x", 2 + (first + i * 7) % 250))
        };
        let shares: Vec<PathBuf> = (0..=NETWORK_SLOTS).map(host).collect();
        let mut watcher = FolderWatcher::with_timing(
            egui::Context::default(),
            RETRY_BACKOFF.to_vec(),
            Duration::from_millis(200),
        );
        watcher.sync(&shares);
        let last = shares.last().unwrap();
        let started = Instant::now();
        while watcher.registration(last).is_none() && started.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            watcher.registration(last).is_some(),
            "the last share never got a slot"
        );
    }

    #[test]
    fn the_cleanup_pool_drops_on_a_fixed_number_of_threads() {
        struct SlowDrop(Sender<std::thread::ThreadId>);
        impl Drop for SlowDrop {
            fn drop(&mut self) {
                std::thread::sleep(Duration::from_millis(20));
                let _ = self.0.send(std::thread::current().id());
            }
        }
        let pool = cleanup_pool::<SlowDrop>(2);
        let (tx, rx) = unbounded();
        let started = Instant::now();
        for _ in 0..12 {
            pool.send(SlowDrop(tx.clone())).unwrap();
        }
        // Sending never waits for a drop.
        assert!(started.elapsed() < Duration::from_millis(100));
        let threads: HashSet<std::thread::ThreadId> = (0..12)
            .map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect();
        assert!(threads.len() <= 2, "{} threads dropped items", threads.len());
        assert!(!threads.contains(&std::thread::current().id()));
    }

    #[test]
    fn retries_stop_when_the_folder_is_no_longer_shown() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("missing");
        let mut watcher = FolderWatcher::with_backoff(
            egui::Context::default(),
            vec![Duration::from_millis(50)],
        );
        watcher.sync(std::slice::from_ref(&missing));
        std::thread::sleep(Duration::from_millis(200));
        watcher.sync(&[]);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(watcher.registration(&missing), None);
    }
}
