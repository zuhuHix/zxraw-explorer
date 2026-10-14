//! Cloud placeholder guard for the find-and-replace and backup-check engines.
//!
//! Online-only files (OneDrive files-on-demand and other Cloud Files providers) download
//! their data the moment it is read, so tools that read many files must skip them. The
//! attributes come from `GetFileAttributesW`, which never opens the file.
//!
//! The function names match `forge_core::cloud` from the cloud-placeholder change, so the
//! callers can switch to that module without other edits.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;

/// The data is not available locally (legacy HSM; also set by some sync clients).
pub const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
/// Opening the file or folder fetches it.
pub const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
/// Reading the data downloads it. Set on online-only Cloud Files placeholders.
pub const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

const ONLINE_ONLY_MASK: u32 =
    FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;

/// Do these Win32 file attributes describe a file whose data is not on this PC?
pub fn is_online_only_attrs(attrs: u32) -> bool {
    attrs & ONLINE_ONLY_MASK != 0
}

#[cfg(windows)]
fn attributes(path: &Path) -> Option<u32> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
    use windows::core::PCWSTR;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
    let a = unsafe { GetFileAttributesW(PCWSTR(wide.as_ptr())) };
    (a != INVALID_FILE_ATTRIBUTES).then_some(a)
}

#[cfg(not(windows))]
fn attributes(_path: &Path) -> Option<u32> {
    None
}

/// May background work read the data of `path`? False for online-only placeholders.
/// Unreadable attributes count as allowed: the reader then fails on its own.
pub fn background_read_allowed(path: &Path) -> bool {
    if is_simulated(path) {
        return false;
    }
    !attributes(path).is_some_and(is_online_only_attrs)
}

// Real placeholders cannot be created without a sync provider, so tests and the e2e hook
// mark paths as online-only here. Paths are matched exactly.

static ANY_SIMULATED: AtomicBool = AtomicBool::new(false);

fn simulated() -> &'static RwLock<HashSet<PathBuf>> {
    static SET: OnceLock<RwLock<HashSet<PathBuf>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

fn is_simulated(path: &Path) -> bool {
    ANY_SIMULATED.load(Ordering::Relaxed) && simulated().read().contains(path)
}

/// Treat `path` as an online-only placeholder in this process. For tests and automation.
#[doc(hidden)]
pub fn simulate_online_only(path: impl Into<PathBuf>) {
    simulated().write().insert(path.into());
    ANY_SIMULATED.store(true, Ordering::Relaxed);
}

/// Undo [`simulate_online_only`] for `path`.
#[doc(hidden)]
pub fn clear_simulated_online_only(path: &Path) {
    simulated().write().remove(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_only_attributes_are_recognised() {
        assert!(is_online_only_attrs(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | 0x20));
        assert!(is_online_only_attrs(FILE_ATTRIBUTE_OFFLINE));
        assert!(!is_online_only_attrs(0x20 | 0x01));
    }

    #[test]
    fn simulated_paths_are_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cloud.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(background_read_allowed(&file));
        simulate_online_only(file.clone());
        assert!(!background_read_allowed(&file));
        clear_simulated_online_only(&file);
        assert!(background_read_allowed(&file));
    }
}
