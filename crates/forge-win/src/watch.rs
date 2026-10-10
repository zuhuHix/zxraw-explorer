//! Drive plug/unplug detection by polling [`list_drives`](crate::list_drives).
//!
//! Polling is deliberately simple: no window messages, no WMI. A drive that appears and
//! disappears between two polls is not reported.

#[cfg(windows)]
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
    if interval.is_zero() {
        return Err(WinError::Invalid("poll interval must be non-zero".into()));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("forge-drive-watch".into())
        .spawn(move || {
            let mut known: BTreeSet<PathBuf> = roots();
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
                let now = roots();
                for added in now.difference(&known) {
                    if tx.send(DriveEvent::Added(added.clone())).is_err() {
                        return;
                    }
                }
                for removed in known.difference(&now) {
                    if tx.send(DriveEvent::Removed(removed.clone())).is_err() {
                        return;
                    }
                }
                known = now;
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

#[cfg(not(windows))]
pub fn drive_change_watcher(_interval: Duration) -> Result<DriveWatcher> {
    Err(WinError::Unsupported)
}

#[cfg(windows)]
fn roots() -> BTreeSet<PathBuf> {
    crate::drives::logical_drive_roots().into_iter().collect()
}
