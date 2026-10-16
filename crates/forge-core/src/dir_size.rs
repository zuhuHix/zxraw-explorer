//! Recursive folder totals for the Size column: bytes, files and folders below a folder.
//!
//! Unlike [`crate::disk_usage`] (a full report with top files and extensions), this only
//! adds up, and is built to be fast enough to run for every folder on screen:
//!
//! * One `FindFirstFileExW` pass per folder (`FindExInfoBasic`, `FIND_FIRST_EX_LARGE_FETCH`)
//!   gives attributes, size and reparse tag of every entry; no file is opened, except the
//!   rare 0-byte file that may still be being written (see [`live_file_size`]).
//! * Folders are walked by several threads from one shared queue, so a single huge
//!   sub-folder is split up as well (not just the first level).
//! * A [`SizeCache`] lets the caller skip sub-trees it already knows and keep the totals of
//!   every sub-folder the walk completes, so opening a sub-folder later is instant.
//!
//! Sizes are logical sizes (what Explorer shows as "Size"). Symlinks and junctions are
//! neither followed nor counted, so nothing is counted twice and walks cannot loop. Cloud
//! folders whose listing is not on this PC are not entered (that would download it) and make
//! the result [`DirTotals::partial`]; online-only files count with their listed size.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use forge_jobs::CancelToken;
use parking_lot::{Condvar, Mutex};

use crate::cloud;
use crate::error::{CoreError, Result};
use crate::listing::live_file_size;

/// Totals of everything below one folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DirTotals {
    /// Logical file sizes (what Explorer's "Size" shows).
    pub bytes: u64,
    pub files: u64,
    /// Sub-folders below (not counting the folder itself).
    pub dirs: u64,
    /// Something below could not be read or was deliberately not entered (an unreadable
    /// folder, a cloud folder whose listing is not on this PC). `bytes` is then a lower bound.
    pub partial: bool,
}

impl DirTotals {
    fn add(&mut self, other: &DirTotals) {
        self.bytes += other.bytes;
        self.files += other.files;
        self.dirs += other.dirs;
        self.partial |= other.partial;
    }
}

/// Lets a caller supply totals it already knows (skipping that subtree) and receive the
/// totals of every folder the walk completes (so navigating into a sub-folder is instant).
///
/// Both are called from the walk's worker threads, once per sub-folder, so they should be a
/// quick map lookup / insert.
pub trait SizeCache: Sync {
    fn lookup(&self, dir: &Path) -> Option<DirTotals>;
    fn store(&self, dir: &Path, totals: &DirTotals);
}

/// A [`SizeCache`] that knows nothing and keeps nothing.
pub struct NoCache;

impl SizeCache for NoCache {
    fn lookup(&self, _dir: &Path) -> Option<DirTotals> {
        None
    }

    fn store(&self, _dir: &Path, _totals: &DirTotals) {}
}

/// Recursive totals of `dir`, walked fresh (the cache is not asked about `dir` itself).
///
/// `threads` (at least 1, the calling thread included) walk this one folder. `cache.lookup`
/// is asked about every sub-folder before it is entered, and `cache.store` receives every
/// sub-folder whose walk completed, but not `dir` (the caller stores the result).
/// Cancellation is checked at least once per folder and returns `Err(CoreError::Cancelled)`;
/// sub-folders completed before that have already been stored.
///
/// Errors on `dir` itself (missing, not a folder, access denied) are `Err`; errors below make
/// the result `partial`.
pub fn dir_totals(
    dir: &Path,
    cache: &dyn SizeCache,
    threads: usize,
    cancel: &CancelToken,
) -> Result<DirTotals> {
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    // Follows a link given as the root on purpose: the caller asked for that folder.
    let md = std::fs::metadata(dir).map_err(|e| CoreError::io(dir, e))?;
    if !md.is_dir() {
        return Err(CoreError::Invalid(format!(
            "{}: not a folder",
            dir.display()
        )));
    }
    let walk = Walk {
        cache,
        cancel,
        network: crate::netpath::is_network_path(dir),
        queue: Mutex::new(Queue::default()),
        ready: Condvar::new(),
        result: Mutex::new(None),
    };

    // The root is read on the calling thread so that a failure to open it is an error, not
    // a partial result.
    let mut scan = Scan::default();
    match walk.scan(dir, &mut scan) {
        Ok(()) => {}
        Err(ScanError::Cancelled) => return Err(CoreError::Cancelled),
        Err(ScanError::Open(e)) => return Err(CoreError::io(dir, e)),
    }
    if scan.subdirs.is_empty() {
        return Ok(scan.totals);
    }
    let root = Arc::new(Node {
        path: dir.to_path_buf(),
        parent: None,
        pending: AtomicUsize::new(0),
        totals: Mutex::new(DirTotals::default()),
    });
    walk.publish(root, scan);

    let threads = threads.max(1);
    std::thread::scope(|s| {
        for _ in 1..threads {
            s.spawn(|| walk.work());
        }
        walk.work();
    });
    walk.result.lock().take().ok_or(CoreError::Cancelled)
}

/// A folder of the walk. Post-order aggregation: `totals` starts as the folder's own files
/// and the sub-trees answered by the cache; every finished child adds its totals. `pending`
/// counts the children still running plus one for the folder's own scan, so it reaches 0
/// exactly once, when `totals` is final.
struct Node {
    path: PathBuf,
    parent: Option<Arc<Node>>,
    pending: AtomicUsize,
    totals: Mutex<DirTotals>,
}

#[derive(Default)]
struct Queue {
    items: VecDeque<Arc<Node>>,
    /// The root finished, or the walk was cancelled: workers leave.
    stop: bool,
}

struct Walk<'a> {
    cache: &'a dyn SizeCache,
    cancel: &'a CancelToken,
    network: bool,
    queue: Mutex<Queue>,
    ready: Condvar,
    result: Mutex<Option<DirTotals>>,
}

/// What one folder's listing contributed.
#[derive(Default)]
struct Scan {
    totals: DirTotals,
    subdirs: Vec<PathBuf>,
}

enum ScanError {
    /// The folder could not be listed at all.
    Open(io::Error),
    Cancelled,
}

/// One entry of a folder listing, as the platform enumerator reports it.
struct RawEntry {
    name: OsString,
    attributes: u32,
    size: u64,
    is_dir: bool,
    /// A name surrogate (symlink, junction, mount point): never followed or counted.
    is_link: bool,
}

/// How often a long listing re-checks cancellation, in entries.
const CANCEL_CHECK_EVERY: u64 = 4096;

/// Entering a folder with these attributes would download its listing from the cloud.
const NOT_ENTERED: u32 = cloud::FILE_ATTRIBUTE_RECALL_ON_OPEN | cloud::FILE_ATTRIBUTE_OFFLINE;

impl Walk<'_> {
    fn work(&self) {
        loop {
            let node = {
                let mut queue = self.queue.lock();
                loop {
                    if queue.stop {
                        return;
                    }
                    // Newest first (depth-first): keeps the queue short on wide trees.
                    if let Some(node) = queue.items.pop_back() {
                        break node;
                    }
                    self.ready.wait(&mut queue);
                }
            };
            if self.cancel.is_cancelled() {
                self.stop();
                return;
            }
            let mut scan = Scan::default();
            match self.scan(&node.path, &mut scan) {
                Ok(()) => {}
                Err(ScanError::Open(_)) => scan.totals.partial = true,
                Err(ScanError::Cancelled) => {
                    self.stop();
                    return;
                }
            }
            self.publish(node, scan);
        }
    }

    /// Records `node`'s own scan, queues its sub-folders and completes its own share.
    fn publish(&self, node: Arc<Node>, scan: Scan) {
        *node.totals.lock() = scan.totals;
        // Set before any child can finish and decrement it.
        node.pending
            .store(scan.subdirs.len() + 1, Ordering::Release);
        if !scan.subdirs.is_empty() {
            let mut queue = self.queue.lock();
            for path in scan.subdirs {
                queue.items.push_back(Arc::new(Node {
                    path,
                    parent: Some(Arc::clone(&node)),
                    pending: AtomicUsize::new(0),
                    totals: Mutex::new(DirTotals::default()),
                }));
            }
            drop(queue);
            self.ready.notify_all();
        }
        self.complete_one(node);
    }

    /// One piece of `node` finished; when it was the last, fold its totals into the parent,
    /// and so on up the tree.
    fn complete_one(&self, node: Arc<Node>) {
        let mut node = node;
        loop {
            if node.pending.fetch_sub(1, Ordering::AcqRel) != 1 {
                return;
            }
            let totals = *node.totals.lock();
            match &node.parent {
                Some(parent) => {
                    self.cache.store(&node.path, &totals);
                    parent.totals.lock().add(&totals);
                    let parent = Arc::clone(parent);
                    node = parent;
                }
                None => {
                    *self.result.lock() = Some(totals);
                    self.stop();
                    return;
                }
            }
        }
    }

    fn stop(&self) {
        self.queue.lock().stop = true;
        self.ready.notify_all();
    }

    /// Lists `dir` once: adds up its files, answers sub-folders from the cache where it can
    /// and collects the rest in `scan.subdirs`. A listing that fails part-way keeps what it
    /// read and is marked partial.
    fn scan(&self, dir: &Path, scan: &mut Scan) -> std::result::Result<(), ScanError> {
        let mut seen = 0u64;
        let mut cancelled = false;
        let complete = enumerate(dir, &mut |entry| {
            seen += 1;
            if seen.is_multiple_of(CANCEL_CHECK_EVERY) && self.cancel.is_cancelled() {
                cancelled = true;
                return false;
            }
            self.add_entry(dir, entry, scan);
            true
        })
        .map_err(ScanError::Open)?;
        if cancelled {
            return Err(ScanError::Cancelled);
        }
        if !complete {
            scan.totals.partial = true;
        }
        Ok(())
    }

    fn add_entry(&self, dir: &Path, entry: RawEntry, scan: &mut Scan) {
        if entry.is_link {
            return;
        }
        let totals = &mut scan.totals;
        if entry.is_dir {
            totals.dirs += 1;
            let path = dir.join(&entry.name);
            if entry.attributes & NOT_ENTERED != 0 || cloud::is_simulated(&path) {
                totals.partial = true;
            } else if let Some(known) = self.cache.lookup(&path) {
                totals.add(&known);
            } else {
                scan.subdirs.push(path);
            }
            return;
        }
        totals.files += 1;
        // A non-zero listed size is always right (live_file_size would return it as is), so
        // only a 0-byte entry needs its path, and online-only placeholders keep their listed
        // size without being opened.
        totals.bytes += if entry.size == 0 {
            let path = dir.join(&entry.name);
            let online_only =
                cloud::is_online_only_attrs(entry.attributes) || cloud::is_simulated(&path);
            live_file_size(&path, 0, online_only, self.network)
        } else {
            entry.size
        };
    }
}

/// Calls `f` for every entry of `dir` except "." and "..", until `f` returns false.
/// `Err` when `dir` cannot be listed at all; `Ok(false)` when the listing failed part-way.
#[cfg(windows)]
fn enumerate(dir: &Path, f: &mut dyn FnMut(RawEntry) -> bool) -> io::Result<bool> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES};
    use windows::Win32::Storage::FileSystem::{
        FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic, FindExSearchNameMatch,
        FindFirstFileExW, FindNextFileW, WIN32_FIND_DATAW,
    };
    use windows::core::PCWSTR;

    const DIRECTORY: u32 = 0x10;
    const REPARSE_POINT: u32 = 0x400;
    const NAME_SURROGATE: u32 = 0x2000_0000;

    /// The Win32 error inside an HRESULT, so `io::Error::kind` (access denied, not found)
    /// works for [`CoreError::io`].
    fn win32_error(error: &windows::core::Error) -> io::Error {
        let code = error.code().0 as u32;
        if code & 0xFFFF_0000 == 0x8007_0000 {
            io::Error::from_raw_os_error((code & 0xFFFF) as i32)
        } else {
            io::Error::from_raw_os_error(code as i32)
        }
    }

    let mut pattern: Vec<u16> = crate::paths::to_extended_length(dir)
        .as_os_str()
        .encode_wide()
        .collect();
    if !matches!(pattern.last(), Some(&c) if c == u16::from(b'\\') || c == u16::from(b'/')) {
        pattern.push(u16::from(b'\\'));
    }
    pattern.extend([u16::from(b'*'), 0]);

    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` is NUL-terminated and `data` is a valid WIN32_FIND_DATAW; both
    // outlive the call.
    let handle = match unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            (&mut data as *mut WIN32_FIND_DATAW).cast(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    } {
        Ok(handle) => handle,
        // An empty drive root has no "." entry, so nothing at all matches.
        Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(true),
        Err(e) => return Err(win32_error(&e)),
    };
    let complete = loop {
        let len = data
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(data.cFileName.len());
        let name = &data.cFileName[..len];
        let dot = u16::from(b'.');
        if name != [dot] && name != [dot, dot] {
            let attributes = data.dwFileAttributes;
            let entry = RawEntry {
                name: OsString::from_wide(name),
                attributes,
                size: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
                is_dir: attributes & DIRECTORY != 0,
                is_link: attributes & REPARSE_POINT != 0 && data.dwReserved0 & NAME_SURROGATE != 0,
            };
            if !f(entry) {
                break true;
            }
        }
        // SAFETY: `handle` is an open find handle and `data` a valid WIN32_FIND_DATAW.
        if let Err(e) = unsafe { FindNextFileW(handle, &mut data) } {
            break e.code() == ERROR_NO_MORE_FILES.to_hresult();
        }
    };
    // SAFETY: `handle` came from a successful FindFirstFileExW and is closed once.
    let _ = unsafe { FindClose(handle) };
    Ok(complete)
}

#[cfg(not(windows))]
fn enumerate(dir: &Path, f: &mut dyn FnMut(RawEntry) -> bool) -> io::Result<bool> {
    enumerate_std(dir, f)
}

/// The portable enumerator: `read_dir`. On Windows only the tests use it, to check the
/// `FindFirstFileExW` path against it.
#[cfg_attr(windows, allow(dead_code))]
fn enumerate_std(dir: &Path, f: &mut dyn FnMut(RawEntry) -> bool) -> io::Result<bool> {
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else {
            return Ok(false);
        };
        let (Ok(file_type), Ok(md)) = (entry.file_type(), entry.metadata()) else {
            return Ok(false);
        };
        let raw = RawEntry {
            name: entry.file_name(),
            attributes: cloud::metadata_attributes(&md),
            size: md.len(),
            is_dir: file_type.is_dir(),
            is_link: file_type.is_symlink(),
        };
        if !f(raw) {
            break;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    fn write(path: &Path, len: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, vec![1u8; len]).unwrap();
    }

    fn totals(dir: &Path, threads: usize) -> DirTotals {
        dir_totals(dir, &NoCache, threads, &CancelToken::new()).unwrap()
    }

    /// Records every store; answers lookups from `known`.
    #[derive(Default)]
    struct MapCache {
        known: HashMap<PathBuf, DirTotals>,
        looked_up: Mutex<Vec<PathBuf>>,
        stored: Mutex<HashMap<PathBuf, DirTotals>>,
    }

    impl SizeCache for MapCache {
        fn lookup(&self, dir: &Path) -> Option<DirTotals> {
            self.looked_up.lock().push(dir.to_path_buf());
            self.known.get(dir).copied()
        }

        fn store(&self, dir: &Path, totals: &DirTotals) {
            let previous = self.stored.lock().insert(dir.to_path_buf(), *totals);
            assert!(previous.is_none(), "{} stored twice", dir.display());
        }
    }

    /// root/a.bin (10), root/x/b.bin (20), root/x/y/c.bin (30), root/x/y/d.bin (40),
    /// root/z/ (empty).
    fn sample_tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        write(&root.join("a.bin"), 10);
        write(&root.join("x").join("b.bin"), 20);
        write(&root.join("x").join("y").join("c.bin"), 30);
        write(&root.join("x").join("y").join("d.bin"), 40);
        fs::create_dir(root.join("z")).unwrap();
        d
    }

    #[test]
    fn nested_totals_count_bytes_files_and_folders() {
        let d = sample_tree();
        for threads in [1, 2, 4, 16] {
            let t = totals(d.path(), threads);
            assert_eq!(
                t,
                DirTotals {
                    bytes: 100,
                    files: 4,
                    dirs: 3,
                    partial: false
                },
                "threads={threads}"
            );
        }
        let x = totals(&d.path().join("x"), 2);
        assert_eq!((x.bytes, x.files, x.dirs), (90, 3, 1));
    }

    #[test]
    fn empty_folder_and_root_errors() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(totals(d.path(), 4), DirTotals::default());
        let cancel = CancelToken::new();
        let missing = dir_totals(&d.path().join("missing"), &NoCache, 2, &cancel);
        assert!(
            matches!(missing, Err(CoreError::NotFound(_))),
            "{missing:?}"
        );
        write(&d.path().join("f.txt"), 3);
        let file = dir_totals(&d.path().join("f.txt"), &NoCache, 2, &cancel);
        assert!(matches!(file, Err(CoreError::Invalid(_))), "{file:?}");
    }

    #[test]
    fn deep_and_wide_trees_add_up_with_many_threads() {
        let d = tempfile::tempdir().unwrap();
        // A chain 40 levels deep next to a wide level of 50 folders.
        let mut deep = d.path().join("deep");
        for level in 0..40 {
            deep = deep.join(format!("l{level}"));
            write(&deep.join("f"), level + 1);
        }
        for i in 0..50 {
            write(&d.path().join("wide").join(format!("w{i}")).join("f"), 2);
        }
        let expected = DirTotals {
            bytes: (1..=40).sum::<usize>() as u64 + 100,
            files: 90,
            dirs: 1 + 40 + 1 + 50,
            partial: false,
        };
        for threads in [1, 3, 8] {
            assert_eq!(totals(d.path(), threads), expected, "threads={threads}");
        }
    }

    #[test]
    fn junctions_are_not_followed_or_counted() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("target");
        write(&target.join("big.bin"), 1000);
        let walked = d.path().join("walked");
        write(&walked.join("small.bin"), 5);
        let junction = walked.join("junction");
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return; // mklink unavailable in this environment
        }
        let t = totals(&walked, 2);
        assert_eq!(
            t,
            DirTotals {
                bytes: 5,
                files: 1,
                dirs: 0,
                partial: false
            }
        );
    }

    #[test]
    fn cancellation_returns_cancelled() {
        let d = sample_tree();
        let cancel = CancelToken::new();
        cancel.cancel();
        let r = dir_totals(d.path(), &NoCache, 2, &cancel);
        assert!(matches!(r, Err(CoreError::Cancelled)), "{r:?}");

        // Cancelled mid-walk, from the cache callback: no result, and the walk stops.
        struct CancelOnLookup<'a>(&'a CancelToken);
        impl SizeCache for CancelOnLookup<'_> {
            fn lookup(&self, _dir: &Path) -> Option<DirTotals> {
                self.0.cancel();
                None
            }
            fn store(&self, _dir: &Path, _totals: &DirTotals) {}
        }
        let cancel = CancelToken::new();
        let r = dir_totals(d.path(), &CancelOnLookup(&cancel), 3, &cancel);
        assert!(matches!(r, Err(CoreError::Cancelled)), "{r:?}");
    }

    #[test]
    fn cache_lookup_replaces_a_subtree() {
        let d = sample_tree();
        let x = d.path().join("x");
        let fake = DirTotals {
            bytes: 1_000_000,
            files: 7,
            dirs: 2,
            partial: true,
        };
        let cache = MapCache {
            known: HashMap::from([(x.clone(), fake)]),
            ..Default::default()
        };
        let t = dir_totals(d.path(), &cache, 2, &CancelToken::new()).unwrap();
        // a.bin + the fake x (itself a folder) + empty z.
        assert_eq!(
            t,
            DirTotals {
                bytes: 10 + 1_000_000,
                files: 1 + 7,
                dirs: 2 + 2,
                partial: true
            }
        );
        let looked_up = cache.looked_up.lock();
        assert!(looked_up.contains(&x));
        assert!(!looked_up.contains(&x.join("y")), "x was entered");
        assert!(!looked_up.iter().any(|p| p == d.path()), "root looked up");
    }

    #[test]
    fn store_receives_every_completed_sub_folder() {
        let d = sample_tree();
        let cache = MapCache::default();
        let t = dir_totals(d.path(), &cache, 3, &CancelToken::new()).unwrap();
        let stored = cache.stored.lock();
        let x = d.path().join("x");
        assert_eq!(stored.len(), 3, "{stored:?}");
        assert!(!stored.contains_key(d.path()), "root stored");
        assert_eq!(
            stored[&x],
            DirTotals {
                bytes: 90,
                files: 3,
                dirs: 1,
                partial: false
            }
        );
        assert_eq!(stored[&x.join("y")].bytes, 70);
        assert_eq!(stored[&d.path().join("z")], DirTotals::default());
        assert_eq!(t.bytes, 100);
        // Every sub-folder was asked about before being entered; the root was not.
        assert_eq!(cache.looked_up.lock().len(), 3);
    }

    #[test]
    fn simulated_online_only_file_counts_its_listed_size() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("cloud.bin");
        write(&f, 1234);
        cloud::simulate_online_only(&f);
        let t = totals(d.path(), 1);
        cloud::clear_simulated_online_only(&f);
        assert_eq!((t.bytes, t.files, t.partial), (1234, 1, false));
    }

    #[test]
    fn cloud_folder_not_on_this_pc_is_not_entered() {
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join("local.bin"), 3);
        let cloud_dir = d.path().join("cloud");
        write(&cloud_dir.join("inside.bin"), 500);
        cloud::simulate_online_only(&cloud_dir);
        let cache = MapCache::default();
        let t = dir_totals(d.path(), &cache, 2, &CancelToken::new());
        cloud::clear_simulated_online_only(&cloud_dir);
        let t = t.unwrap();
        assert_eq!(
            t,
            DirTotals {
                bytes: 3,
                files: 1,
                dirs: 1,
                partial: true
            }
        );
        assert!(cache.looked_up.lock().is_empty());
    }

    #[test]
    fn a_file_still_being_written_counts_its_live_size() {
        use std::io::Write;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("sub").join("rec.mp4");
        fs::create_dir(path.parent().unwrap()).unwrap();
        // The handle stays open, as a recorder's would: the listing may still say 0 bytes.
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&vec![7u8; 300_000]).unwrap();
        file.flush().unwrap();
        assert_eq!(totals(d.path(), 2).bytes, 300_000);
        drop(file);
    }

    #[cfg(windows)]
    #[test]
    fn unreadable_sub_folder_makes_the_result_partial() {
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join("ok.bin"), 8);
        let locked = d.path().join("locked");
        write(&locked.join("hidden.bin"), 100);
        let icacls = |args: &[&str]| {
            std::process::Command::new("icacls")
                .arg(&locked)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        // Deny "list folder" to Everyone.
        if !icacls(&["/deny", "*S-1-1-0:(RD)"]) {
            return; // icacls unavailable in this environment
        }
        let listable = fs::read_dir(&locked).is_ok();
        let r = dir_totals(d.path(), &NoCache, 2, &CancelToken::new());
        let root_locked = dir_totals(&locked, &NoCache, 1, &CancelToken::new());
        icacls(&["/remove:d", "*S-1-1-0"]);
        if listable {
            return; // listing still allowed (a privileged account): nothing to check
        }
        let t = r.unwrap();
        assert_eq!((t.bytes, t.files, t.dirs, t.partial), (8, 1, 1, true));
        assert!(
            matches!(root_locked, Err(CoreError::AccessDenied(_))),
            "{root_locked:?}"
        );
    }

    #[test]
    fn platform_enumerator_matches_read_dir() {
        let d = sample_tree();
        let collect = |f: fn(&Path, &mut dyn FnMut(RawEntry) -> bool) -> io::Result<bool>| {
            let mut out = Vec::new();
            let complete = f(d.path(), &mut |e| {
                out.push((e.name, e.size, e.is_dir, e.is_link));
                true
            })
            .unwrap();
            assert!(complete);
            out.sort();
            out
        };
        let native = collect(enumerate);
        assert_eq!(native, collect(enumerate_std));
        assert_eq!(native.len(), 3);
    }

    #[cfg(windows)]
    #[test]
    fn long_paths_are_walked() {
        let d = tempfile::tempdir().unwrap();
        let mut deep = d.path().to_path_buf();
        while deep.as_os_str().len() < 400 {
            deep = deep.join("a_rather_long_folder_name_to_pass_max_path");
        }
        let ext = crate::paths::to_extended_length(&deep);
        fs::create_dir_all(&ext).unwrap();
        fs::write(ext.join("f.bin"), [0u8; 77]).unwrap();
        let t = totals(d.path(), 2);
        assert_eq!((t.bytes, t.files, t.partial), (77, 1, false));
        assert_eq!(totals(&deep, 1).bytes, 77);
    }

    /// Rough throughput on a generated ~20k-file tree with one dominant sub-folder:
    /// `cargo test -p forge-core --release --lib dir_size -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn perf_twenty_thousand_files() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let mut expected = 0u64;
        let mut make = |dir: &Path, files: usize| {
            fs::create_dir_all(dir).unwrap();
            for i in 0..files {
                fs::write(dir.join(format!("f{i}.dat")), b"x").unwrap();
                expected += 1;
            }
        };
        // Dominant: big/{0..9}/{0..9}/ with 160 files each (16k files).
        for a in 0..10 {
            for b in 0..10 {
                make(
                    &root.join("big").join(a.to_string()).join(b.to_string()),
                    160,
                );
            }
        }
        // The rest: 40 small folders of 100 files (4k files).
        for i in 0..40 {
            make(&root.join(format!("small{i}")).join("inner"), 50);
            make(&root.join(format!("small{i}")), 50);
        }
        let cancel = CancelToken::new();
        for threads in [1, 4, 1, 4] {
            let start = std::time::Instant::now();
            let t = dir_totals(root, &NoCache, threads, &cancel).unwrap();
            let elapsed = start.elapsed();
            assert_eq!(t.files, expected);
            println!(
                "threads={threads}: {} files, {} dirs in {:.1} ms ({:.0} files/s)",
                t.files,
                t.dirs,
                elapsed.as_secs_f64() * 1000.0,
                t.files as f64 / elapsed.as_secs_f64()
            );
        }
    }
}
