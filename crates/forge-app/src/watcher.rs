//! Live refresh: watches the folders currently shown (non-recursively) and reports which
//! ones changed, debounced so bursts of writes (a copy, an export) cause one refresh.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Quiet period after the last event before a folder is refreshed.
const DEBOUNCE: Duration = Duration::from_millis(400);
/// Never refresh the same folder more often than this (big folders get a longer gap).
const MIN_GAP: Duration = Duration::from_millis(1000);
const MIN_GAP_LARGE: Duration = Duration::from_secs(4);

pub struct FolderWatcher {
    /// The folders to register, sent to the thread that owns the notify watcher.
    /// Registering opens each folder (notify checks it and opens a handle), which blocks for
    /// as long as an offline share takes to fail, so it never happens on the UI thread.
    register: Option<Sender<HashSet<PathBuf>>>,
    /// Sets sent to, and sets applied by, the registration thread (tests wait on these).
    #[cfg_attr(not(test), allow(dead_code))]
    sent: u64,
    applied: Arc<AtomicU64>,
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

/// Owns the notify watcher and keeps its registrations equal to the newest wanted set. A
/// folder that is slow to open holds up only this thread; sets sent meanwhile collapse
/// into the newest one.
fn register_loop(
    mut watcher: RecommendedWatcher,
    wanted: Receiver<HashSet<PathBuf>>,
    applied: Arc<AtomicU64>,
) {
    let mut registered: HashSet<PathBuf> = HashSet::new();
    while let Ok(mut want) = wanted.recv() {
        let mut taken = 1;
        while let Ok(newer) = wanted.try_recv() {
            want = newer;
            taken += 1;
        }
        for gone in registered.difference(&want).cloned().collect::<Vec<_>>() {
            let _ = watcher.unwatch(&gone);
            registered.remove(&gone);
        }
        for new in want.difference(&registered).cloned().collect::<Vec<_>>() {
            // Unwatchable folders (some network shares, removed drives) just don't
            // auto-refresh; F5 still works.
            if watcher.watch(&new, RecursiveMode::NonRecursive).is_ok() {
                registered.insert(new);
            }
        }
        applied.fetch_add(taken, Ordering::Release);
    }
}

impl FolderWatcher {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = unbounded();
        let repaint = ctx.clone();
        let shown = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
        let shown_by_notify = Arc::clone(&shown);
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                // Ignore pure access events; everything else may change the listing.
                if matches!(ev.kind, notify::EventKind::Access(_)) {
                    return;
                }
                for p in ev.paths {
                    let _ = tx.send(p.clone());
                    if let Some(parent) = p.parent() {
                        let _ = tx.send(parent.to_path_buf());
                        // Windows may report a shown folder's deletion only as an event in
                        // its parent. Probe the parent's shown children here, on notify's
                        // thread, and report the ones that are gone.
                        let gone: Vec<PathBuf> = shown_by_notify
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
                            let _ = tx.send(dir);
                        }
                    }
                }
                repaint.request_repaint_after(DEBOUNCE);
            }
        })
        .map_err(|e| tracing::warn!("folder watcher unavailable: {e}"))
        .ok();
        let applied = Arc::new(AtomicU64::new(0));
        let register = watcher.and_then(|watcher| {
            let (register, wanted) = unbounded();
            let applied = Arc::clone(&applied);
            std::thread::Builder::new()
                .name("folder-watch-register".into())
                .spawn(move || register_loop(watcher, wanted, applied))
                .map_err(|e| tracing::warn!("folder watcher unavailable: {e}"))
                .ok()
                .map(|_| register)
        });
        Self {
            register,
            sent: 0,
            applied,
            rx,
            watched: HashSet::new(),
            shown,
            pending: HashMap::new(),
            last_refresh: HashMap::new(),
            ctx,
        }
    }

    /// Keep the watch set equal to `dirs` (the folders on screen). Touches no folder: the
    /// registration thread does that.
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
        if register.send(folders).is_ok() {
            self.sent += 1;
        }
        for gone in self.watched.difference(&want).cloned().collect::<Vec<_>>() {
            self.pending.remove(&gone);
        }
        if let Ok(mut shown) = self.shown.lock() {
            shown.clone_from(&want);
        }
        self.watched = want;
    }

    /// Waits until the registration thread has applied every set sent so far.
    #[cfg(test)]
    fn wait_registered(&self) {
        let started = Instant::now();
        while self.applied.load(Ordering::Acquire) < self.sent
            && started.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
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

    #[test]
    fn detects_new_file_after_debounce() {
        let d = tempfile::tempdir().unwrap();
        let mut w = FolderWatcher::new(egui::Context::default());
        let dir = d.path().to_path_buf();
        w.sync(std::slice::from_ref(&dir));
        w.wait_registered();
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
        watcher.wait_registered();
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
        // TEST-NET-1 (RFC 5737) is unroutable: registering it blocks until the network gives
        // up, which must happen on the registration thread only.
        let mut watcher = FolderWatcher::new(egui::Context::default());
        let share = PathBuf::from(r"\\192.0.2.1\x\shoot");
        let started = Instant::now();
        watcher.sync(std::slice::from_ref(&share));
        watcher.sync(&[share.clone(), share.join("day1")]);
        watcher.sync(std::slice::from_ref(&share));
        assert!(started.elapsed() < Duration::from_millis(250));
    }
}
