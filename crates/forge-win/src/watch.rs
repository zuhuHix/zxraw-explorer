//! Drive plug/unplug detection by polling [`list_drives`](crate::list_drives).
//!
//! Polling is deliberately simple: no window messages, no WMI. A drive that appears and
//! disappears between two polls is not reported.

#[cfg(any(windows, target_os = "linux"))]
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::{Result, WinError};

/// A change in the set of logical drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveEvent {
    /// A drive root (e.g. `E:\`) appeared.
    Added(PathBuf),
    /// A drive root disappeared.
    Removed(PathBuf),
}

/// Background poller. Dropping it stops the thread (within one polling step).
pub struct DriveWatcher {
    events: Receiver<DriveEvent>,
    stop: Arc<AtomicBool>,
    _thread: JoinHandle<()>,
}

impl DriveWatcher {
    /// Receiver for [`DriveEvent`]s. Use `try_iter()` from the UI thread.
    pub fn events(&self) -> &Receiver<DriveEvent> {
        &self.events
    }
}

impl Drop for DriveWatcher {
    fn drop(&mut self) {
        // Only signal the thread. Joining could block the caller for as long as a poll
        // waits on a dead network drive, so the thread is left to exit on its own.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Starts polling the drive list every `interval`. The first poll only records the current
/// state; events are produced for changes after that.
#[cfg(windows)]
pub fn drive_change_watcher(interval: Duration) -> Result<DriveWatcher> {
    start_poller(interval, || Some(roots()))
}

/// Linux: the set of visible mount points is polled from `/proc/self/mountinfo`.
#[cfg(target_os = "linux")]
pub fn drive_change_watcher(interval: Duration) -> Result<DriveWatcher> {
    start_poller(interval, crate::drives::linux::mount_roots)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn drive_change_watcher(_interval: Duration) -> Result<DriveWatcher> {
    Err(WinError::Unsupported)
}

/// The poll loop shared by the platforms. `read` returns the current set of roots, or `None`
/// when they could not be read: that poll is skipped, so a failed read is not seen as every
/// drive being removed.
#[cfg(any(windows, target_os = "linux"))]
fn start_poller(
    interval: Duration,
    read: impl Fn() -> Option<BTreeSet<PathBuf>> + Send + 'static,
) -> Result<DriveWatcher> {
    if interval.is_zero() {
        return Err(WinError::Invalid("poll interval must be non-zero".into()));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("forge-drive-watch".into())
        .spawn(move || {
            let mut known: Option<BTreeSet<PathBuf>> = read();
            // Sleep in short steps so Drop stops us promptly.
            let step = interval.min(Duration::from_millis(100));
            while !stop_thread.load(Ordering::Relaxed) {
                let mut waited = Duration::ZERO;
                while waited < interval && !stop_thread.load(Ordering::Relaxed) {
                    std::thread::sleep(step);
                    waited += step;
                }
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let Some(now) = read() else {
                    continue;
                };
                if let Some(before) = &known {
                    for added in now.difference(before) {
                        if tx.send(DriveEvent::Added(added.clone())).is_err() {
                            return;
                        }
                    }
                    for removed in before.difference(&now) {
                        if tx.send(DriveEvent::Removed(removed.clone())).is_err() {
                            return;
                        }
                    }
                }
                known = Some(now);
            }
        })
        .map_err(|source| WinError::Io {
            op: "spawn drive watcher",
            path: PathBuf::new(),
            source,
        })?;
    Ok(DriveWatcher {
        events: rx,
        stop,
        _thread: handle,
    })
}

#[cfg(windows)]
fn roots() -> BTreeSet<PathBuf> {
    crate::drives::logical_drive_roots().into_iter().collect()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn watcher_reports_added_and_removed_roots_then_stops() {
        let state = Arc::new(Mutex::new(BTreeSet::from([PathBuf::from("/")])));
        let calls = Arc::new(AtomicUsize::new(0));
        let (st, c) = (Arc::clone(&state), Arc::clone(&calls));
        let watcher = start_poller(Duration::from_millis(20), move || {
            c.fetch_add(1, Ordering::Relaxed);
            Some(st.lock().unwrap().clone())
        })
        .unwrap();

        // The first poll records the baseline; change the set only after it has run.
        std::thread::sleep(Duration::from_millis(100));
        state.lock().unwrap().insert(PathBuf::from("/mnt/x"));
        let ev = watcher
            .events()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(ev, DriveEvent::Added(PathBuf::from("/mnt/x")));

        state.lock().unwrap().remove(&PathBuf::from("/"));
        let ev = watcher
            .events()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(ev, DriveEvent::Removed(PathBuf::from("/")));

        drop(watcher);
        std::thread::sleep(Duration::from_millis(300));
        let after_drop = calls.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            after_drop,
            "the poller stopped with the watcher"
        );
    }

    #[test]
    fn unreadable_polls_are_skipped_not_reported_as_removals() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = Arc::clone(&fail);
        let watcher = start_poller(Duration::from_millis(20), move || {
            if f.load(Ordering::Relaxed) {
                None
            } else {
                Some(BTreeSet::from([PathBuf::from("/")]))
            }
        })
        .unwrap();
        fail.store(true, Ordering::Relaxed);
        assert!(
            watcher
                .events()
                .recv_timeout(Duration::from_millis(200))
                .is_err()
        );
    }

    #[test]
    fn real_watcher_starts_and_drops_cleanly() {
        let watcher = drive_change_watcher(Duration::from_millis(50)).unwrap();
        std::thread::sleep(Duration::from_millis(120));
        drop(watcher);
        assert!(drive_change_watcher(Duration::ZERO).is_err());
    }
}
