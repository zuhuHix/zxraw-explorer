//! Cloud placeholders (OneDrive, Dropbox, Google Drive "files on demand").
//!
//! A placeholder is listed like an ordinary file, but reading its data makes Windows
//! download the whole file. Background work (thumbnails, metadata, hashing, scanners) must
//! never do that, so every background reader asks [`background_read_allowed`] (or checks
//! [`FileEntry::online_only`]) first. Explicit user actions (open, copy, export) are free to
//! read the file: that download is the user's choice.
//!
//! Detection uses only the attributes Windows already has: the directory listing's find data
//! or `GetFileAttributesW`. Neither opens the file, so neither triggers a download.
//!
//! The check lives in one place so other policies (for example slow network shares) can be
//! added to [`background_read_allowed`] later without touching the callers.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;

use crate::model::FileEntry;

/// The data is not available locally (legacy HSM; also set by some sync clients).
pub const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
/// Opening the file or folder fetches it (used for folders whose listing is not local yet).
pub const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
/// Reading the data downloads it. Set on online-only Cloud Files placeholders.
pub const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

const ONLINE_ONLY_MASK: u32 =
    FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;

/// Do these Win32 file attributes describe a file whose data is not on this PC?
pub fn is_online_only_attrs(attrs: u32) -> bool {
    attrs & ONLINE_ONLY_MASK != 0
}

/// Win32 attributes carried by `md` (0 off Windows). For a `DirEntry::metadata()` this is
/// the find data from the listing, so no file is opened.
pub fn metadata_attributes(md: &fs::Metadata) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes()
    }
    #[cfg(not(windows))]
    {
        let _ = md;
        0
    }
}

/// Is the entry described by `path` and its listing metadata `md` online-only?
pub fn is_online_only_metadata(path: &Path, md: &fs::Metadata) -> bool {
    is_online_only_attrs(metadata_attributes(md)) || is_simulated(path)
}

/// Where attributes come from. Production uses [`FsAttributes`]; tests inject their own.
pub trait AttributeSource {
    /// Win32 attributes of `path`, or `None` when they cannot be read.
    fn attributes(&self, path: &Path) -> Option<u32>;
}

/// Reads attributes with `GetFileAttributesW`, which never opens the file.
pub struct FsAttributes;

impl AttributeSource for FsAttributes {
    #[cfg(windows)]
    fn attributes(&self, path: &Path) -> Option<u32> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
        use windows::core::PCWSTR;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
        let a = unsafe { GetFileAttributesW(PCWSTR(wide.as_ptr())) };
        (a != INVALID_FILE_ATTRIBUTES).then_some(a)
    }
    #[cfg(not(windows))]
    fn attributes(&self, _path: &Path) -> Option<u32> {
        None
    }
}

/// May background work (thumbnails, metadata, hashing, scans) read the data of `path`?
///
/// False for cloud placeholders whose data is not on this PC. Unreadable attributes count
/// as allowed: the reader then fails on its own, exactly as before.
pub fn background_read_allowed(path: &Path) -> bool {
    background_read_allowed_with(path, &FsAttributes)
}

/// [`background_read_allowed`] with an injectable attribute source.
pub fn background_read_allowed_with(path: &Path, source: &dyn AttributeSource) -> bool {
    if is_simulated(path) {
        return false;
    }
    !source.attributes(path).is_some_and(is_online_only_attrs)
}

/// [`background_read_allowed`] for an entry from a listing, without another syscall.
pub fn background_read_allowed_entry(entry: &FileEntry) -> bool {
    !entry.online_only && !is_simulated(&entry.path)
}

// --- Simulated placeholders -------------------------------------------------------------
//
// Real placeholders cannot be created without a sync provider, so tests (and the e2e
// automation hook) mark paths as online-only here. Paths are matched exactly, so tests
// running in parallel on their own temp folders do not interfere.

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
    use std::collections::HashMap;

    struct Fake(HashMap<PathBuf, u32>);
    impl AttributeSource for Fake {
        fn attributes(&self, path: &Path) -> Option<u32> {
            self.0.get(path).copied()
        }
    }

    #[test]
    fn online_only_attributes_are_detected() {
        assert!(!is_online_only_attrs(0));
        assert!(!is_online_only_attrs(0x20)); // archive
        assert!(!is_online_only_attrs(0x400)); // reparse point alone: a pinned/local file
        assert!(!is_online_only_attrs(0x0008_0000)); // pinned
        assert!(is_online_only_attrs(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS));
        assert!(is_online_only_attrs(FILE_ATTRIBUTE_RECALL_ON_OPEN));
        assert!(is_online_only_attrs(FILE_ATTRIBUTE_OFFLINE));
        // A typical OneDrive online-only file: archive | reparse | unpinned | recall on access.
        assert!(is_online_only_attrs(
            0x20 | 0x400 | 0x0010_0000 | 0x0040_0000
        ));
    }

    #[test]
    fn injected_attributes_decide_background_reads() {
        let local = PathBuf::from(r"C:\photos\local.cr3");
        let cloud = PathBuf::from(r"C:\photos\cloud.cr3");
        let missing = PathBuf::from(r"C:\photos\missing.cr3");
        let fake = Fake(HashMap::from([
            (local.clone(), 0x20),
            (
                cloud.clone(),
                0x20 | 0x400 | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
            ),
        ]));
        assert!(background_read_allowed_with(&local, &fake));
        assert!(!background_read_allowed_with(&cloud, &fake));
        assert!(background_read_allowed_with(&missing, &fake));
    }

    #[test]
    fn real_local_files_are_readable_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.jpg");
        std::fs::write(&f, b"x").unwrap();
        assert!(background_read_allowed(&f));
        assert!(background_read_allowed(
            &dir.path().join("does-not-exist.jpg")
        ));
    }

    #[test]
    fn simulated_paths_are_online_only() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sim.nef");
        std::fs::write(&f, b"x").unwrap();
        simulate_online_only(&f);
        assert!(!background_read_allowed(&f));
        let entry = crate::listing::entry_for_path(&f).unwrap();
        assert!(entry.online_only);
        assert!(!background_read_allowed_entry(&entry));
        clear_simulated_online_only(&f);
        assert!(background_read_allowed(&f));
    }
}
