//! Cloud placeholders (OneDrive, Dropbox, Google Drive "files on demand").
//!
//! A placeholder is listed like an ordinary file, but reading its data makes Windows
//! download the whole file. Background work (thumbnails, metadata, hashing, scanners) must
//! never do that, so every background reader asks [`background_read_allowed`] (or checks
//! [`FileEntry::online_only`]) first. Explicit user actions (open, copy, export) are free to
//! read the file: that download is the user's choice.
//!
//! Detection uses only the attributes Windows already has: the directory listing's find data
//! or `FindFirstFileExW` on the exact path. Neither opens the file, so neither triggers a
//! download. The path check fails closed: unreadable attributes count as online-only.
//!
//! Recall-marked *folders* are still listed by walkers (indexer, ops, scans): listing a
//! placeholder folder fetches only names and sizes, never file contents.
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

/// Reads the find data of `path` with `FindFirstFileExW` (`FindExInfoBasic`): the same
/// attributes a directory listing reports, including `RECALL_ON_OPEN`, which
/// `GetFileAttributesW` does not report. Never opens the file.
pub struct FsAttributes;

impl AttributeSource for FsAttributes {
    fn attributes(&self, path: &Path) -> Option<u32> {
        find_data(path).map(|(attributes, _)| attributes)
    }
}

/// `(attributes, reparse tag)` from the find data of `path`; the tag is 0 unless the entry is
/// a reparse point. Never opens the file.
#[cfg(windows)]
fn find_data(path: &Path) -> Option<(u32, u32)> {
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Storage::FileSystem::{
            FIND_FIRST_EX_FLAGS, FindClose, FindExInfoBasic, FindExSearchNameMatch,
            FindFirstFileExW, WIN32_FIND_DATAW,
        };
        use windows::core::PCWSTR;
        let path = crate::paths::to_extended_length(path);
        let text = path.as_os_str().to_string_lossy();
        let name_part = text.strip_prefix(r"\\?\").unwrap_or(&text);
        // Wildcards would match other names (Windows file names cannot contain them).
        if name_part.is_empty() || name_part.contains(['*', '?']) {
            return None;
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut data = WIN32_FIND_DATAW::default();
        // SAFETY: `wide` is NUL-terminated and `data` is a valid WIN32_FIND_DATAW; both
        // outlive the call.
        let handle = unsafe {
            FindFirstFileExW(
                PCWSTR(wide.as_ptr()),
                FindExInfoBasic,
                (&mut data as *mut WIN32_FIND_DATAW).cast(),
                FindExSearchNameMatch,
                None,
                FIND_FIRST_EX_FLAGS(0),
            )
        }
        .ok()?;
        // SAFETY: `handle` came from a successful FindFirstFileExW and is closed once.
        let _ = unsafe { FindClose(handle) };
        let tag = if data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            data.dwReserved0
        } else {
            0
        };
        Some((data.dwFileAttributes, tag))
    }
}

#[cfg(not(windows))]
fn find_data(_path: &Path) -> Option<(u32, u32)> {
    // No cloud placeholders outside Windows.
    Some((0, 0))
}

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Is `tag` one of the Cloud Files reparse tags (`IO_REPARSE_TAG_CLOUD` to `_CLOUD_F`)?
pub fn is_cloud_reparse_tag(tag: u32) -> bool {
    tag & 0xFFFF_0FFF == 0x9000_001A
}

/// Is `dir` inside a cloud sync root (OneDrive, Dropbox, Google Drive "files on demand")?
///
/// True when `dir` or any ancestor is a Cloud Files placeholder (cloud reparse tag) or
/// online-only, and true when such an attribute cannot be read (fail closed). Drive and share
/// roots are not checked. For background work that would read many files below `dir` without
/// asking about each one, such as a repository scan.
pub fn in_cloud_sync_root(dir: &Path) -> bool {
    in_cloud_sync_root_with(dir, &find_data)
}

/// [`in_cloud_sync_root`] with an injectable `(attributes, reparse tag)` source.
pub fn in_cloud_sync_root_with(dir: &Path, find: &dyn Fn(&Path) -> Option<(u32, u32)>) -> bool {
    dir.ancestors().filter(|a| a.parent().is_some()).any(|a| {
        is_simulated(a)
            || find(a).is_none_or(|(attributes, tag)| {
                is_online_only_attrs(attributes) || is_cloud_reparse_tag(tag)
            })
    })
}

/// May background work (thumbnails, metadata, hashing, scans) read the data of `path`?
///
/// False for cloud placeholders whose data is not on this PC, and false when the attributes
/// cannot be read (fail closed). Call it at the worker boundary, just before opening the
/// file: a listing flag can be stale by then.
pub fn background_read_allowed(path: &Path) -> bool {
    background_read_allowed_with(path, &FsAttributes)
}

/// [`background_read_allowed`] with an injectable attribute source.
pub fn background_read_allowed_with(path: &Path, source: &dyn AttributeSource) -> bool {
    if is_simulated(path) {
        return false;
    }
    source
        .attributes(path)
        .is_some_and(|a| !is_online_only_attrs(a))
}

/// Pre-filter for an entry from a listing, without a syscall. Used for the badge and to avoid
/// queueing work; the worker still calls [`background_read_allowed`] before reading.
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
        // Unknown attributes fail closed.
        assert!(!background_read_allowed_with(&missing, &fake));
        let on_open = PathBuf::from(r"C:\photos\open.cr3");
        let fake = Fake(HashMap::from([(
            on_open.clone(),
            0x20 | FILE_ATTRIBUTE_RECALL_ON_OPEN,
        )]));
        assert!(!background_read_allowed_with(&on_open, &fake));
    }

    #[test]
    fn real_local_files_are_readable_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.jpg");
        std::fs::write(&f, b"x").unwrap();
        assert!(background_read_allowed(&f));
        // Missing files and wildcard patterns fail closed.
        assert!(!background_read_allowed(&dir.path().join("missing.jpg")));
        assert!(!background_read_allowed(&dir.path().join("a*.jpg")));
        let long = dir.path().join("d".repeat(120)).join("e".repeat(120));
        std::fs::create_dir_all(&long).unwrap();
        let long_file = long.join("long-path-photo.jpg");
        std::fs::write(&long_file, b"x").unwrap();
        assert!(background_read_allowed(&long_file));
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

    #[test]
    fn sync_root_check_looks_at_every_ancestor() {
        let cloud = |p: &Path| {
            let s = p.to_string_lossy().to_lowercase();
            Some(if s == r"c:\users\me\onedrive" {
                (0x10 | 0x400, 0x9000_701A)
            } else {
                (0x10, 0)
            })
        };
        assert!(in_cloud_sync_root_with(
            Path::new(r"C:\Users\me\OneDrive\code\repo"),
            &cloud
        ));
        assert!(in_cloud_sync_root_with(
            Path::new(r"C:\Users\me\OneDrive"),
            &cloud
        ));
        assert!(!in_cloud_sync_root_with(
            Path::new(r"C:\Users\me\code\repo"),
            &cloud
        ));
        // A junction is a reparse point too, but not a cloud one.
        let junction = |_: &Path| Some((0x10 | 0x400, 0xA000_0003));
        assert!(!in_cloud_sync_root_with(Path::new(r"C:\j\repo"), &junction));
        let recall = |p: &Path| {
            Some((
                if p.ends_with("repo") {
                    0x10 | 0x0040_0000
                } else {
                    0x10
                },
                0,
            ))
        };
        assert!(in_cloud_sync_root_with(Path::new(r"C:\x\repo"), &recall));
        assert!(
            in_cloud_sync_root_with(Path::new(r"C:\x\repo"), &|_| None),
            "fails closed"
        );
        assert!(is_cloud_reparse_tag(0x9000_001A) && is_cloud_reparse_tag(0x9000_F01A));
        assert!(!is_cloud_reparse_tag(0xA000_000C));
    }
}
