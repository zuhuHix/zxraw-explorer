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
fn find_data(path: &Path) -> Option<(u32, u32)> {
    find_info(path)
        .ok()
        .map(|info| (info.attributes, info.reparse_tag))
}

/// The find data ZXraw uses: attributes, reparse tag (0 unless a reparse point) and the
/// logical size (a placeholder reports its full size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FindInfo {
    attributes: u32,
    reparse_tag: u32,
    size: u64,
}

/// Why [`find_info`] failed: the path does not exist, or anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FindError {
    NotFound,
    Other,
}

#[cfg(windows)]
fn find_info(path: &Path) -> Result<FindInfo, FindError> {
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
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
            return Err(FindError::Other);
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
        .map_err(|error| {
            let code = error.code();
            if code == ERROR_FILE_NOT_FOUND.to_hresult()
                || code == ERROR_PATH_NOT_FOUND.to_hresult()
            {
                FindError::NotFound
            } else {
                FindError::Other
            }
        })?;
        // SAFETY: `handle` came from a successful FindFirstFileExW and is closed once.
        let _ = unsafe { FindClose(handle) };
        let tag = if data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            data.dwReserved0
        } else {
            0
        };
        Ok(FindInfo {
            attributes: data.dwFileAttributes,
            reparse_tag: tag,
            size: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
        })
    }
}

#[cfg(not(windows))]
fn find_info(path: &Path) -> Result<FindInfo, FindError> {
    // No cloud placeholders outside Windows.
    let md = fs::symlink_metadata(path).map_err(|_| FindError::NotFound)?;
    Ok(FindInfo {
        attributes: 0,
        reparse_tag: 0,
        size: md.len(),
    })
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

/// Sync roots the cloud clients registered with Windows (OneDrive, Dropbox, Google Drive and
/// other Cloud Files providers that use the shell's sync root manager), plus the OneDrive
/// folders named by the environment. `None` when the registrations cannot be read: callers
/// must then assume a sync root could be anywhere.
///
/// A provider that calls `CfRegisterSyncRoot` without the shell registration is not listed
/// (measured: such a root does not appear under `SyncRootManager`); its root folder still
/// carries the Cloud Files reparse tag, which [`in_cloud_sync_root`] sees from inside it.
pub fn known_sync_roots() -> Option<Vec<PathBuf>> {
    let mut roots = registered_sync_roots()?;
    roots.extend(
        ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"]
            .iter()
            .filter_map(std::env::var_os)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    );
    Some(roots)
}

/// The `UserSyncRoots` values under `HKLM\...\Explorer\SyncRootManager\<provider>`.
#[cfg(windows)]
fn registered_sync_roots() -> Option<Vec<PathBuf>> {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegEnumKeyExW,
        RegEnumValueW, RegOpenKeyExW,
    };
    use windows::core::{HSTRING, PWSTR};

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the key was opened by RegOpenKeyExW and is closed once.
            let _ = unsafe { RegCloseKey(self.0) };
        }
    }
    /// `Ok(None)` when the key does not exist; `Err` for any other failure.
    fn open(parent: HKEY, sub: &str) -> Result<Option<Key>, ()> {
        let mut key = HKEY::default();
        // SAFETY: `sub` is a valid NUL-terminated string and `key` a valid out pointer.
        let status =
            unsafe { RegOpenKeyExW(parent, &HSTRING::from(sub), None, KEY_READ, &mut key) };
        if status == ERROR_SUCCESS {
            Ok(Some(Key(key)))
        } else if status == ERROR_FILE_NOT_FOUND {
            Ok(None)
        } else {
            Err(())
        }
    }

    let base = match open(
        HKEY_LOCAL_MACHINE,
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager",
    ) {
        Ok(Some(key)) => key,
        Ok(None) => return Some(Vec::new()),
        Err(()) => return None,
    };
    let mut roots = Vec::new();
    for index in 0.. {
        let mut name = [0u16; 512];
        let mut name_len = name.len() as u32;
        // SAFETY: the buffer and its length describe valid memory owned here.
        let status = unsafe {
            RegEnumKeyExW(
                base.0,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                None,
                None,
                None,
                None,
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        if status != ERROR_SUCCESS {
            return None;
        }
        let provider = String::from_utf16_lossy(&name[..name_len as usize]);
        let users = match open(base.0, &format!(r"{provider}\UserSyncRoots")) {
            Ok(Some(key)) => key,
            Ok(None) => continue,
            Err(()) => return None,
        };
        for value in 0.. {
            let mut value_name = [0u16; 512];
            let mut value_name_len = value_name.len() as u32;
            let mut data = vec![0u8; 65_536];
            let mut data_len = data.len() as u32;
            let mut kind = 0u32;
            // SAFETY: every buffer and length describes valid memory owned here.
            let status = unsafe {
                RegEnumValueW(
                    users.0,
                    value,
                    Some(PWSTR(value_name.as_mut_ptr())),
                    &mut value_name_len,
                    None,
                    Some(&mut kind),
                    Some(data.as_mut_ptr()),
                    Some(&mut data_len),
                )
            };
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            if status != ERROR_SUCCESS {
                return None;
            }
            if kind != REG_SZ.0 && kind != REG_EXPAND_SZ.0 {
                continue;
            }
            let wide: Vec<u16> = data[..data_len as usize]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .take_while(|&unit| unit != 0)
                .collect();
            if !wide.is_empty() {
                let text = String::from_utf16_lossy(&wide);
                let text = if kind == REG_EXPAND_SZ.0 {
                    expand_environment(&text)
                } else {
                    text
                };
                roots.push(PathBuf::from(text));
            }
        }
    }
    Some(roots)
}

#[cfg(not(windows))]
fn registered_sync_roots() -> Option<Vec<PathBuf>> {
    Some(Vec::new())
}

/// Could a transfer of the folder `dir` read cloud files? True when `dir` is inside a sync
/// root ([`in_cloud_sync_root`]: the folder or an ancestor is a Cloud Files placeholder or
/// online-only, or its attributes cannot be read), when a known sync root lies inside `dir`
/// or holds it, when `roots` is `None` (the registrations could not be read), or when the
/// first page of its listing shows an online-only entry or cannot be read. The sample
/// catches offline files outside any Cloud Files sync root (a tiered file share such as
/// Azure File Sync), as long as some sit at the top of the folder. False only for a folder
/// positively outside every cloud location.
pub fn folder_may_hold_cloud_files(dir: &Path, roots: Option<&[PathBuf]>) -> bool {
    folder_may_hold_cloud_files_with(dir, roots, &find_data, &|dir| {
        first_page_has_online_only(dir, LISTING_SAMPLE)
    })
}

/// Entries of a folder's listing sampled by [`folder_may_hold_cloud_files`].
const LISTING_SAMPLE: usize = 256;

fn folder_may_hold_cloud_files_with(
    dir: &Path,
    roots: Option<&[PathBuf]>,
    find: &dyn Fn(&Path) -> Option<(u32, u32)>,
    sample: &dyn Fn(&Path) -> bool,
) -> bool {
    let Some(roots) = roots else {
        return true;
    };
    roots
        .iter()
        .any(|root| crate::paths::is_within(dir, root) || crate::paths::is_within(root, dir))
        || in_cloud_sync_root_with(dir, find)
        || sample(dir)
}

/// Does one of the first `limit` entries of `dir`'s listing (files or folders) carry an
/// online-only attribute? Listing data only, nothing is opened. True when the folder or an
/// entry cannot be read (fail closed).
fn first_page_has_online_only(dir: &Path, limit: usize) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return true;
    };
    for entry in entries.take(limit) {
        let Ok(entry) = entry else {
            return true;
        };
        let path = entry.path();
        match entry.metadata() {
            Ok(md) if is_online_only_metadata(&path, &md) => return true,
            Ok(_) => {}
            Err(_) => return true,
        }
    }
    false
}

/// Expands `%NAME%` references in a `REG_EXPAND_SZ` value from the environment. An unknown
/// name is left as written (the path then matches nothing, as Windows would leave it).
fn expand_environment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(value) => out.push_str(&value),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
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

/// What a background job may do with a file, from its find data alone (never opened).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadCheck {
    /// Nothing at that path: report it as missing, not as a cloud file.
    Missing,
    /// Data on this PC (or not a regular file); reading downloads nothing.
    Local,
    /// Online-only, or the attributes could not be read (fail closed). `size` is the logical
    /// size reading would download (0 when unknown).
    OnlineOnly { size: u64 },
}

/// [`background_read_allowed`] that also tells a missing file apart and gives the size, for
/// jobs that count what they would download. Never opens the file.
pub fn read_check(path: &Path) -> ReadCheck {
    read_check_from(path, find_info(path))
}

fn read_check_from(path: &Path, info: Result<FindInfo, FindError>) -> ReadCheck {
    const DIRECTORY: u32 = 0x10;
    match info {
        Err(FindError::NotFound) => ReadCheck::Missing,
        Err(FindError::Other) => ReadCheck::OnlineOnly { size: 0 },
        Ok(info) if info.attributes & DIRECTORY != 0 => ReadCheck::Local,
        Ok(info) if is_online_only_attrs(info.attributes) || is_simulated(path) => {
            ReadCheck::OnlineOnly { size: info.size }
        }
        Ok(_) => ReadCheck::Local,
    }
}

/// Why [`open_for_background_read`] refused or failed.
#[derive(Debug)]
pub enum BackgroundOpenError {
    /// Nothing at that path.
    Missing,
    /// The data is not on this PC (or its attributes could not be read).
    OnlineOnly,
    /// The open itself failed.
    Io(std::io::Error),
}

/// Opens `path` for a background read only if its data is on this PC, with no window for a
/// download between the check and the read:
///
/// 1. The find data is checked first, so a placeholder is never opened at all (opening a
///    `RECALL_ON_OPEN` placeholder downloads it).
/// 2. The file is opened (an open does not hydrate a `RECALL_ON_DATA_ACCESS` placeholder,
///    the kind OneDrive and other Cloud Files providers use; only reading does).
/// 3. The attributes are read again from the open handle; a placeholder found there is
///    closed unread.
///
/// While the returned handle is open the sync client cannot dehydrate the file
/// (`CfDehydratePlaceholder` fails with `ERROR_CLOUD_FILE_IN_USE`), so reads through it, or
/// by another program (ffmpeg) while it is held, never download. Measured with a test Cloud
/// Files provider: see `docs/LIMITATIONS.md`. `FILE_FLAG_OPEN_NO_RECALL` is not used: the
/// Cloud Files filter ignores it and hydrates on the first read.
pub fn open_for_background_read(path: &Path) -> Result<fs::File, BackgroundOpenError> {
    open_for_background_read_with(path, find_info, handle_attributes)
}

fn handle_attributes(file: &fs::File) -> std::io::Result<u32> {
    file.metadata().map(|md| metadata_attributes(&md))
}

/// [`open_for_background_read`] with injectable find data and handle attributes.
fn open_for_background_read_with(
    path: &Path,
    find: impl Fn(&Path) -> Result<FindInfo, FindError>,
    on_handle: impl Fn(&fs::File) -> std::io::Result<u32>,
) -> Result<fs::File, BackgroundOpenError> {
    match read_check_from(path, find(path)) {
        ReadCheck::Missing => return Err(BackgroundOpenError::Missing),
        ReadCheck::OnlineOnly { .. } => return Err(BackgroundOpenError::OnlineOnly),
        ReadCheck::Local => {}
    }
    let file = fs::File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => BackgroundOpenError::Missing,
        _ => BackgroundOpenError::Io(error),
    })?;
    match on_handle(&file) {
        Ok(attributes) if !is_online_only_attrs(attributes) && !is_simulated(path) => Ok(file),
        // Became a placeholder between the two checks, or unknown: close it unread.
        _ => Err(BackgroundOpenError::OnlineOnly),
    }
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
    #[test]
    fn read_check_tells_missing_local_and_online_only_apart() {
        let local = FindInfo {
            attributes: 0x20,
            reparse_tag: 0,
            size: 7,
        };
        let p = Path::new(r"C:\x\clip.mov");
        assert_eq!(read_check_from(p, Ok(local)), ReadCheck::Local);
        let online = FindInfo {
            attributes: 0x0040_0000,
            ..local
        };
        assert_eq!(
            read_check_from(p, Ok(online)),
            ReadCheck::OnlineOnly { size: 7 }
        );
        let dir = FindInfo {
            attributes: 0x10 | 0x0040_0000,
            ..local
        };
        assert_eq!(read_check_from(p, Ok(dir)), ReadCheck::Local);
        assert_eq!(
            read_check_from(p, Err(FindError::NotFound)),
            ReadCheck::Missing
        );
        // Unreadable attributes fail closed.
        assert_eq!(
            read_check_from(p, Err(FindError::Other)),
            ReadCheck::OnlineOnly { size: 0 }
        );
    }

    #[test]
    fn only_folders_positively_outside_cloud_locations_skip_the_walk() {
        let local = |_: &Path| Some((0x10, 0));
        let roots = [PathBuf::from(r"C:\Users\a\OneDrive")];
        let shoot = Path::new(r"D:\Shoots\2026");
        assert!(!folder_may_hold_cloud_files_with(shoot, Some(&roots), &local, &no_sample));
        // A sync root inside the selected folder (copying the whole profile folder).
        assert!(folder_may_hold_cloud_files_with(
            Path::new(r"c:\users\A"),
            Some(&roots),
            &local,
            &no_sample
        ));
        // Inside the sync root.
        assert!(folder_may_hold_cloud_files_with(
            Path::new(r"C:\Users\a\OneDrive\Pictures"),
            Some(&roots),
            &local,
            &no_sample
        ));
        // Registrations unreadable: assume anywhere.
        assert!(folder_may_hold_cloud_files_with(shoot, None, &local, &no_sample));
        // Marked by the Cloud Files reparse tag on an ancestor (a root not registered).
        let tagged = |p: &Path| {
            if p == Path::new(r"D:\Shoots") {
                Some((0x410, 0x9000_301A))
            } else {
                Some((0x10, 0))
            }
        };
        assert!(folder_may_hold_cloud_files_with(shoot, Some(&[]), &tagged, &no_sample));
        // Attributes unreadable: fail closed.
        assert!(folder_may_hold_cloud_files_with(shoot, Some(&[]), &|_| None, &no_sample));
    }

    fn no_sample(_: &Path) -> bool {
        false
    }

    #[test]
    fn a_folder_whose_listing_shows_offline_files_is_walked() {
        let local = |_: &Path| Some((0x10, 0));
        let shoot = Path::new(r"\\fileserver\tiered\2026");
        assert!(!folder_may_hold_cloud_files_with(shoot, Some(&[]), &local, &no_sample));
        assert!(folder_may_hold_cloud_files_with(shoot, Some(&[]), &local, &|_| true));

        let d = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(d.path().join(format!("{i}.bin")), b"x").unwrap();
        }
        assert!(!first_page_has_online_only(d.path(), LISTING_SAMPLE));
        let offline = d.path().join("3.bin");
        simulate_online_only(&offline);
        assert!(first_page_has_online_only(d.path(), LISTING_SAMPLE));
        clear_simulated_online_only(&offline);
        // An unreadable folder fails closed.
        assert!(first_page_has_online_only(&d.path().join("missing"), LISTING_SAMPLE));
    }

    #[test]
    fn expandable_registry_paths_are_expanded() {
        // SAFETY (test): no other test reads this variable.
        unsafe { std::env::set_var("ZXRAW_TEST_PROFILE", r"C:\Users\a") };
        assert_eq!(
            expand_environment(r"%ZXRAW_TEST_PROFILE%\OneDrive"),
            r"C:\Users\a\OneDrive"
        );
        assert_eq!(
            expand_environment(r"%ZXRAW_NO_SUCH_VAR%\x"),
            r"%ZXRAW_NO_SUCH_VAR%\x"
        );
        assert_eq!(expand_environment("100% sure"), "100% sure");
        assert_eq!(expand_environment(r"D:\plain"), r"D:\plain");
    }

    #[test]
    fn background_open_rechecks_on_the_handle() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.bin");
        std::fs::write(&f, b"12345").unwrap();
        let local = |_: &Path| {
            Ok(FindInfo {
                attributes: 0x20,
                reparse_tag: 0,
                size: 5,
            })
        };
        assert!(open_for_background_read_with(&f, local, |_| Ok(0x20)).is_ok());
        // Local in the find data, a placeholder by the time it was opened: closed unread.
        assert!(matches!(
            open_for_background_read_with(&f, local, |_| Ok(0x0040_0000)),
            Err(BackgroundOpenError::OnlineOnly)
        ));
        // Attributes unreadable on the handle: fail closed.
        assert!(matches!(
            open_for_background_read_with(&f, local, |_| Err(std::io::Error::other("x"))),
            Err(BackgroundOpenError::OnlineOnly)
        ));
        // A placeholder in the find data is never opened.
        let online = |_: &Path| {
            Ok(FindInfo {
                attributes: 0x0040_0000,
                reparse_tag: 0,
                size: 5,
            })
        };
        let opened = std::cell::Cell::new(false);
        assert!(matches!(
            open_for_background_read_with(&f, online, |_| {
                opened.set(true);
                Ok(0)
            }),
            Err(BackgroundOpenError::OnlineOnly)
        ));
        assert!(!opened.get());
        // Real files: local opens, missing is missing, simulated is refused.
        assert!(open_for_background_read(&f).is_ok());
        assert!(matches!(
            open_for_background_read(&d.path().join("gone")),
            Err(BackgroundOpenError::Missing)
        ));
        simulate_online_only(&f);
        assert!(matches!(
            open_for_background_read(&f),
            Err(BackgroundOpenError::OnlineOnly)
        ));
        clear_simulated_online_only(&f);
    }

    #[test]
    fn read_check_on_real_files() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.bin");
        std::fs::write(&f, b"12345").unwrap();
        assert_eq!(read_check(&f), ReadCheck::Local);
        assert_eq!(read_check(&d.path().join("gone.bin")), ReadCheck::Missing);
        assert_eq!(
            read_check(&d.path().join("no-dir").join("gone.bin")),
            ReadCheck::Missing
        );
        simulate_online_only(&f);
        assert_eq!(read_check(&f), ReadCheck::OnlineOnly { size: 5 });
        clear_simulated_online_only(&f);
    }

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
