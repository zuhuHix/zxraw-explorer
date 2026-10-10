//! Live refresh: watches the folders currently shown (non-recursively) and reports which
//! ones changed, debounced so bursts of writes (a copy, an export) cause one refresh.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Quiet period after the last event before a folder is refreshed.
const DEBOUNCE: Duration = Duration::from_millis(400);
/// Never refresh the same folder more often than this (big folders get a longer gap).
const MIN_GAP: Duration = Duration::from_millis(1000);
const MIN_GAP_LARGE: Duration = Duration::from_secs(4);

pub struct FolderWatcher {
    watcher: Option<RecommendedWatcher>,
    rx: Receiver<PathBuf>,
    watched: HashSet<PathBuf>,
    /// dir -> time of the most recent event
    pending: HashMap<PathBuf, Instant>,
    last_refresh: HashMap<PathBuf, Instant>,
    ctx: egui::Context,
}

impl FolderWatcher {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = unbounded();
        let repaint = ctx.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                // Ignore pure access events; everything else may change the listing.
                if matches!(ev.kind, notify::EventKind::Access(_)) {
                    return;
                }
                for p in ev.paths {
                    if let Some(parent) = p.parent() {
                        let _ = tx.send(parent.to_path_buf());
                    }
                }
                repaint.request_repaint_after(DEBOUNCE);
            }
        })
        .map_err(|e| tracing::warn!("folder watcher unavailable: {e}"))
        .ok();
        Self {
            watcher,
            rx,
            watched: HashSet::new(),
            pending: HashMap::new(),
            last_refresh: HashMap::new(),
            ctx,
        }
    }

    /// Keep the watch set equal to `dirs` (the folders on screen).
    pub fn sync(&mut self, dirs: &[PathBuf]) {
        let Some(w) = self.watcher.as_mut() else { return };
        let want: HashSet<PathBuf> = dirs.iter().cloned().collect();
        for gone in self.watched.difference(&want).cloned().collect::<Vec<_>>() {
            let _ = w.unwatch(&gone);
            self.watched.remove(&gone);
            self.pending.remove(&gone);
        }
        for new in want.difference(&self.watched).cloned().collect::<Vec<_>>() {
            // Unwatchable folders (some network shares, removed drives) just don't
            // auto-refresh; F5 still works.
            if w.watch(&new, RecursiveMode::NonRecursive).is_ok() {
                self.watched.insert(new);
            }
        }
    }

    /// Folders that should be refreshed now. `size_of` gives the entry count for a dir so
    /// huge folders are throttled harder.
    pub fn due(&mut self, size_of: impl Fn(&Path) -> usize) -> Vec<PathBuf> {
        let now = Instant::now();
        while let Ok(dir) = self.rx.try_recv() {
            if self.watched.contains(&dir) {
                self.pending.insert(dir, now);
            }
        }
        let mut out = Vec::new();
        let mut next_wake: Option<Duration> = None;
        self.pending.retain(|dir, last_event| {
            let gap = if size_of(dir) > 20_000 { MIN_GAP_LARGE } else { MIN_GAP };
            let quiet = now.duration_since(*last_event) >= DEBOUNCE;
            let spaced = self.last_refresh.get(dir).is_none_or(|t| now.duration_since(*t) >= gap);
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
}
