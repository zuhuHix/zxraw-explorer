//! Format-independent defences used by every archive backend: entry name rules, collision
//! keys, link-free folder creation in the destination and decoder memory estimates.

#[cfg(test)]
use std::cell::RefCell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};
use crate::ops::FileIdentity;
use crate::paths;

#[cfg(test)]
pub(crate) type BeforeParentCreateHook = Box<dyn Fn(&Path)>;

#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_PARENT_CREATE: RefCell<Option<BeforeParentCreateHook>> =
        const { RefCell::new(None) };
    /// Tests: runs with the destination's final path as soon as an extraction pinned it,
    /// before the archive is listed and planned.
    pub(crate) static AFTER_DEST_PINNED: RefCell<Option<BeforeParentCreateHook>> =
        const { RefCell::new(None) };
    /// Tests: runs with the folder's final path before an extraction that made its folder
    /// takes back what it wrote (cancelled, or nothing extracted).
    pub(crate) static BEFORE_CREATED_CLEANUP: RefCell<Option<BeforeParentCreateHook>> =
        const { RefCell::new(None) };
    /// Tests: runs with the destination's final path once it is resolved, before the folders
    /// of that path are pinned.
    pub(crate) static BEFORE_BASE_PIN: RefCell<Option<BeforeParentCreateHook>> =
        const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn run_hook(
    hook: &'static std::thread::LocalKey<RefCell<Option<BeforeParentCreateHook>>>,
    path: &Path,
) {
    hook.with(|h| {
        if let Some(f) = &*h.borrow() {
            f(path);
        }
    });
}

/// Normalise an entry name as stored: `\` becomes `/`. Directory names keep their trailing `/`.
pub fn normalize_entry_name(name: &str) -> String {
    name.replace('\\', "/")
}

/// Validate an (untrusted) entry name and return the relative path it extracts to.
///
/// Rejects rooted names (`/x`, `\x`), drive and UNC prefixes (`C:`, `\\server`), device
/// paths (`\\?\`, `\\.\`), `..`, NUL and other control characters, `:` (alternate data
/// streams), names ending in a dot or space and reserved device names (`CON`, `nul.txt`,
/// `COM1 .log`, ...). See [`paths::validate_relative`].
pub fn validate_entry_name(name: &str) -> Result<PathBuf> {
    let trimmed = name.trim_end_matches(['/', '\\']);
    paths::validate_relative(trimmed)
}

/// Key under which two entry names collide on Windows (case-insensitive, `/`-separated,
/// without a trailing `/`).
pub fn collision_key(rel: &Path) -> String {
    paths::to_slash(rel).to_lowercase()
}

/// Folders held open so they can't change under a write: each one was opened without
/// following reparse points (relative to the folder pinned before it) and checked by handle
/// to be a real folder, and the handles deny `FILE_SHARE_DELETE`, so while they live none of
/// them can be renamed, deleted or replaced by a junction, and a path through them resolves
/// to the folders that were checked.
pub(crate) struct PinnedDirs {
    handles: Vec<fs::File>,
}

impl PinnedDirs {
    /// The deepest folder pinned, if any.
    pub(crate) fn last(&self) -> Option<&fs::File> {
        self.handles.last()
    }
}

/// An extraction destination pinned for a whole job: resolved once to its final path
/// (through any junction, subst or mapped drive the user's path legitimately goes through),
/// then every folder of that real path, from the volume root down, pinned (see
/// [`PinnedDirs`]), each opened relative to the one above it, and the last checked to be the
/// folder the path resolved to. Entries are written beneath [`PinnedBase::root`]; per entry
/// only the folders below it are pinned ([`pin_dirs`]).
pub(crate) struct PinnedBase {
    root: PathBuf,
    /// Its last handle is the destination's own.
    chain: PinnedDirs,
    identity: Option<FileIdentity>,
}

impl PinnedBase {
    pub(crate) fn open(dest: &Path) -> Result<Self> {
        // What the user's path resolves to now, and that folder's identity, from one handle:
        // the chain pinned below must end at this very folder.
        let (root, want) = resolve(dest)?;
        #[cfg(test)]
        run_hook(&BEFORE_BASE_PIN, &root);
        let mut handles: Vec<fs::File> = Vec::new();
        let mut cur = PathBuf::new();
        for c in root.components() {
            cur.push(c);
            match c {
                // The prefix alone (`\\?\C:`) is not a folder; the root after it is.
                std::path::Component::Prefix(_) => {}
                std::path::Component::RootDir => handles.push(open_pinned(&cur)?),
                std::path::Component::Normal(name) => {
                    let parent = handles.last().ok_or_else(|| {
                        CoreError::Invalid(format!("{} has no volume root", root.display()))
                    })?;
                    let (handle, _) = open_child(parent, name, &cur, Create::No)?;
                    handles.push(handle);
                }
                _ => {
                    return Err(CoreError::Invalid(format!(
                        "{} is not a final path",
                        root.display()
                    )));
                }
            }
        }
        let last = handles
            .last()
            .ok_or_else(|| CoreError::Invalid(format!("{} is not a folder", dest.display())))?;
        let identity = FileIdentity::of_dir(last);
        if want.is_some() && identity != want {
            return Err(CoreError::UnsafePath(format!(
                "{} was replaced while it was being opened; refusing to extract into it",
                dest.display()
            )));
        }
        Ok(Self {
            root,
            chain: PinnedDirs { handles },
            identity,
        })
    }

    /// The destination's final path, which entries are written beneath.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The destination's own pinned handle.
    pub(crate) fn handle(&self) -> &fs::File {
        self.chain.last().expect("a pinned base holds its folder")
    }

    /// The destination folder's identity as pinned.
    pub(crate) fn identity(&self) -> Option<FileIdentity> {
        self.identity
    }

    /// The folder `name` directly inside this one, pinned (refused when it is a link or
    /// not a folder), opened relative to this one's handle. Only that folder is pinned by
    /// the result: keep `self` alive with it for its ancestors to stay pinned.
    pub(crate) fn child(&self, name: &std::ffi::OsStr) -> Result<PinnedBase> {
        let mut parts = Path::new(name).components();
        if !matches!(
            (parts.next(), parts.next()),
            (Some(std::path::Component::Normal(_)), None)
        ) {
            return Err(CoreError::Invalid(format!(
                "{} is not a folder name",
                Path::new(name).display()
            )));
        }
        let root = self.root.join(name);
        let (handle, _) = open_child(self.handle(), name, &root, Create::No)?;
        let identity = FileIdentity::of_dir(&handle);
        Ok(Self {
            root,
            chain: PinnedDirs {
                handles: vec![handle],
            },
            identity,
        })
    }
}

/// The final path `dest` resolves to and that folder's identity, read through one handle.
#[cfg(windows)]
fn resolve(dest: &Path) -> Result<(PathBuf, Option<FileIdentity>)> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW};
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_SHARE_ALL: u32 = 0x7;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dest)
        .map_err(|e| CoreError::io(dest, e))?;
    let identity = FileIdentity::of_dir(&file);
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: the handle is open; the buffer's length is passed with it.
        let n = unsafe {
            GetFinalPathNameByHandleW(HANDLE(file.as_raw_handle()), &mut buf, FILE_NAME_NORMALIZED)
        } as usize;
        if n == 0 {
            return Err(CoreError::io(dest, io::Error::last_os_error()));
        }
        if n < buf.len() {
            buf.truncate(n);
            break;
        }
        buf.resize(n + 1, 0);
    }
    Ok((PathBuf::from(std::ffi::OsString::from_wide(&buf)), identity))
}

#[cfg(not(windows))]
fn resolve(dest: &Path) -> Result<(PathBuf, Option<FileIdentity>)> {
    let root = fs::canonicalize(dest).map_err(|e| CoreError::io(dest, e))?;
    let identity = fs::File::open(&root)
        .ok()
        .and_then(|f| FileIdentity::of_dir(&f));
    Ok((root, identity))
}

/// Pin the existing folders of `rel` below `base` without creating any, refusing links
/// (see [`pin_dirs`]): what is then removed beneath them is what the job wrote there.
pub(crate) fn pin_existing(base: &PinnedBase, rel: &Path) -> Result<PinnedDirs> {
    pin_chain(base, rel, Create::No, &mut |_, _| {})
}

/// Create `base/rel` one component at a time, each opened relative to the folder above it,
/// refusing to pass through links, junctions or files, and return the folders below `base`
/// pinned (see [`PinnedDirs`]); `base` itself is pinned by its [`PinnedBase`]. Keep it alive
/// while anything is written beneath.
pub(crate) fn pin_dirs(base: &PinnedBase, rel: &Path) -> Result<PinnedDirs> {
    pin_dirs_with(base, rel, &mut |_, _| {})
}

/// [`pin_dirs`], telling `created` about each folder it made (its path relative to `base`
/// and its identity).
pub(crate) fn pin_dirs_with(
    base: &PinnedBase,
    rel: &Path,
    created: &mut dyn FnMut(&Path, Option<FileIdentity>),
) -> Result<PinnedDirs> {
    pin_chain(base, rel, Create::Yes, created)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Create {
    No,
    Yes,
}

fn pin_chain(
    base: &PinnedBase,
    rel: &Path,
    create: Create,
    created: &mut dyn FnMut(&Path, Option<FileIdentity>),
) -> Result<PinnedDirs> {
    let mut handles: Vec<fs::File> = Vec::new();
    let mut cur = base.root().to_path_buf();
    let mut cur_rel = PathBuf::new();
    for c in rel.components() {
        let std::path::Component::Normal(name) = c else {
            return Err(CoreError::Invalid(format!(
                "{} is not a relative folder path",
                rel.display()
            )));
        };
        cur.push(name);
        cur_rel.push(name);
        let parent = handles.last().unwrap_or_else(|| base.handle());
        let (handle, made) = match open_child(parent, name, &cur, Create::No) {
            Err(CoreError::NotFound(_)) if create == Create::Yes => {
                #[cfg(test)]
                run_hook(&BEFORE_PARENT_CREATE, &cur);
                // Whatever is there now (a link or junction may have won a race) is checked
                // through the handle that then pins it.
                open_child(parent, name, &cur, Create::Yes)?
            }
            other => other?,
        };
        if made {
            created(&cur_rel, FileIdentity::of_dir(&handle));
        }
        handles.push(handle);
    }
    Ok(PinnedDirs { handles })
}

/// Open a folder itself (not what a reparse point leads to), deny delete sharing, and
/// check through the handle that it is a real folder. By path: used for the volume root.
#[cfg(windows)]
fn open_pinned(path: &Path) -> Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // List access, not just attributes: opens asking only for attributes are left out of
    // the sharing check, so their share mode would not keep anyone from renaming the folder.
    const FILE_LIST_DIRECTORY: u32 = 0x1;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = fs::OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|e| CoreError::io(path, e))?;
    check_real_dir(&file, path)?;
    Ok(file)
}

#[cfg(not(windows))]
fn open_pinned(path: &Path) -> Result<fs::File> {
    verify_real_directory(path)?;
    fs::File::open(path).map_err(|e| CoreError::io(path, e))
}

/// Through the handle: the object opened is a folder and not a reparse point.
#[cfg(windows)]
fn check_real_dir(file: &fs::File, path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    // GetFileInformationByHandle: the attributes of the opened object itself.
    let attrs = file
        .metadata()
        .map_err(|e| CoreError::io(path, e))?
        .file_attributes();
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(CoreError::UnsafePath(format!(
            "{} is a link; refusing to extract through it",
            path.display()
        )));
    }
    if attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(CoreError::Invalid(format!(
            "{} exists and is not a folder",
            path.display()
        )));
    }
    Ok(())
}

/// The folder `name` in the folder open as `parent` (`path` is where that is, for messages),
/// opened relative to that handle without following a reparse point, pinned (delete sharing
/// denied) and checked to be a real folder. With [`Create::Yes`] it is made if missing;
/// returns whether this call made it.
#[cfg(windows)]
fn open_child(
    parent: &fs::File,
    name: &std::ffi::OsStr,
    path: &Path,
    create: Create,
) -> Result<(fs::File, bool)> {
    const FILE_LIST_DIRECTORY: u32 = 0x1;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    let (file, made) = nt::open_relative(
        parent,
        name,
        nt::Kind::Dir,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        create == Create::Yes,
    )
    .map_err(|e| {
        if e.kind() == io::ErrorKind::NotADirectory {
            CoreError::Invalid(format!("{} exists and is not a folder", path.display()))
        } else {
            CoreError::io(path, e)
        }
    })?;
    check_real_dir(&file, path)?;
    Ok((file, made))
}

#[cfg(not(windows))]
fn open_child(
    _parent: &fs::File,
    _name: &std::ffi::OsStr,
    path: &Path,
    create: Create,
) -> Result<(fs::File, bool)> {
    let mut made = false;
    if create == Create::Yes {
        match fs::create_dir(path) {
            Ok(()) => made = true,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(CoreError::io(path, e)),
        }
    }
    Ok((open_pinned(path)?, made))
}

/// What became of something [`remove_if_same`] was asked to remove.
#[derive(Debug)]
pub(crate) enum Removal {
    Removed,
    /// Nothing has that name any more.
    Gone,
    /// What has that name now is not what was recorded (or could not be identified).
    Changed,
    /// It matches, but only by the weak identity (no file id), which can't prove it is the
    /// same item: never removed.
    Unconfirmed,
    /// A folder still holding something.
    NotEmpty,
    Failed(io::Error),
}

/// Remove the file (or, with `dir`, the empty folder) `name` in the folder open as `parent`,
/// but only when it is still the one recorded as `want`: it is opened relative to `parent`
/// without following a reparse point, its identity checked through that handle, and deleted
/// through the same handle. Only a file id proves that: with the weak identity nothing is
/// removed.
#[cfg(windows)]
pub(crate) fn remove_if_same(
    parent: &fs::File,
    name: &std::ffi::OsStr,
    dir: bool,
    want: Option<FileIdentity>,
) -> Removal {
    const DELETE: u32 = 0x0001_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_SHARE_ALL: u32 = 0x7;
    const ERROR_DIR_NOT_EMPTY: i32 = 145;
    let kind = if dir { nt::Kind::Dir } else { nt::Kind::File };
    let file = match nt::open_relative(
        parent,
        name,
        kind,
        DELETE | FILE_READ_ATTRIBUTES,
        FILE_SHARE_ALL,
        false,
    ) {
        Ok((file, _)) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Removal::Gone,
        // A folder where a file was, or the other way round.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotADirectory | io::ErrorKind::IsADirectory
            ) =>
        {
            return Removal::Changed;
        }
        Err(e) => return Removal::Failed(e),
    };
    let now = if dir {
        FileIdentity::of_dir(&file)
    } else {
        FileIdentity::of(&file)
    };
    let Some(want) = want.filter(|w| now == Some(*w)) else {
        return Removal::Changed;
    };
    if !want.is_strong() {
        return Removal::Unconfirmed;
    }
    match crate::ops::delete_open(&file, Path::new(name)) {
        Ok(()) => Removal::Removed,
        Err(e) if e.raw_os_error() == Some(ERROR_DIR_NOT_EMPTY) => Removal::NotEmpty,
        Err(e) => Removal::Failed(e),
    }
}

#[cfg(not(windows))]
pub(crate) fn remove_if_same(
    _parent: &fs::File,
    _name: &std::ffi::OsStr,
    _dir: bool,
    _want: Option<FileIdentity>,
) -> Removal {
    Removal::Failed(io::Error::new(
        io::ErrorKind::Unsupported,
        "removal by handle is not supported here",
    ))
}

/// `NtCreateFile` relative to a folder handle.
#[cfg(windows)]
mod nt {
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows::Wdk::Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT,
        FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    };
    use windows::Win32::Foundation::{
        HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
    };
    use windows::Win32::Storage::FileSystem::{
        FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE,
    };
    use windows::Win32::System::IO::IO_STATUS_BLOCK;
    use windows::core::PWSTR;

    #[derive(Clone, Copy)]
    pub(super) enum Kind {
        Dir,
        File,
    }

    const SYNCHRONIZE: u32 = 0x0010_0000;
    /// `IO_STATUS_BLOCK.Information` when the object was created.
    const FILE_CREATED: usize = 2;

    /// Open `name` (one plain name) in the folder open as `parent`, the object itself and not
    /// what a reparse point leads to; with `create` (folders only) make it when missing.
    /// Returns the handle and whether it was made.
    pub(super) fn open_relative(
        parent: &File,
        name: &std::ffi::OsStr,
        kind: Kind,
        access: u32,
        share: u32,
        create: bool,
    ) -> io::Result<(File, bool)> {
        let wide: Vec<u16> = name.encode_wide().collect();
        if wide.is_empty()
            || wide
                .iter()
                .any(|&c| c == u16::from(b'\\') || c == u16::from(b'/'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a plain name",
            ));
        }
        let bytes = u16::try_from(wide.len() * 2)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name too long"))?;
        let unicode = UNICODE_STRING {
            Length: bytes,
            MaximumLength: bytes,
            Buffer: PWSTR(wide.as_ptr().cast_mut()),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: HANDLE(parent.as_raw_handle()),
            ObjectName: &unicode,
            Attributes: OBJ_CASE_INSENSITIVE,
            ..Default::default()
        };
        let mut options = FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT;
        options |= match kind {
            Kind::Dir => FILE_DIRECTORY_FILE | FILE_OPEN_FOR_BACKUP_INTENT,
            Kind::File => FILE_NON_DIRECTORY_FILE,
        };
        let disposition = if create && matches!(kind, Kind::Dir) {
            FILE_OPEN_IF
        } else {
            FILE_OPEN
        };
        let mut handle = HANDLE::default();
        // SAFETY: an all-zero IO_STATUS_BLOCK is valid.
        let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        // SAFETY: every pointer refers to a live local; `wide` outlives the call.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                FILE_ACCESS_RIGHTS(access | SYNCHRONIZE),
                &attributes,
                &mut status_block,
                None,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                FILE_SHARE_MODE(share),
                disposition,
                options,
                None,
                0,
            )
        };
        if status.is_err() {
            // SAFETY: a plain conversion of a status code.
            let code = unsafe { RtlNtStatusToDosError(status) };
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        // SAFETY: NtCreateFile succeeded, so `handle` is a new handle we now own.
        let file = unsafe { File::from_raw_handle(handle.0) };
        Ok((file, status_block.Information == FILE_CREATED))
    }
}

#[cfg(not(windows))]
fn verify_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| CoreError::io(path, error))?;
    if metadata.is_dir() && !is_link_or_reparse(&metadata) {
        Ok(())
    } else if is_link_or_reparse(&metadata) {
        Err(CoreError::UnsafePath(format!(
            "{} is a link; refusing to extract through it",
            path.display()
        )))
    } else {
        Err(CoreError::Invalid(format!(
            "{} exists and is not a folder",
            path.display()
        )))
    }
}

pub(crate) fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

// ---------------------------------------------------------------------------
// Decoder memory
// ---------------------------------------------------------------------------

/// Dictionary size declared by an LZMA stream as stored in a ZIP entry (method 14): a 2-byte
/// version, a 2-byte properties size, then the 5-byte LZMA properties.
pub(crate) fn zip_lzma_dictionary(head: &[u8]) -> Option<u64> {
    let props_len = u16::from_le_bytes([*head.get(2)?, *head.get(3)?]) as usize;
    if props_len < 5 {
        return None;
    }
    let d = head.get(5..9)?;
    Some(u32::from_le_bytes([d[0], d[1], d[2], d[3]]) as u64)
}

/// Largest LZMA2 dictionary declared by the first block of an `.xz` stream.
pub(crate) fn xz_dictionary(head: &[u8]) -> Option<u64> {
    const MAGIC: &[u8] = &[0xFD, b'7', b'z', b'X', b'Z', 0];
    if head.get(..6)? != MAGIC {
        return None;
    }
    let block = head.get(12..)?;
    let header_len = (*block.first()? as usize + 1) * 4;
    let block = block.get(..header_len.min(block.len()))?;
    let flags = *block.get(1)?;
    let filters = (flags & 0x03) as usize + 1;
    let mut pos = 2;
    let varint = |pos: &mut usize| -> Option<u64> {
        let mut value = 0u64;
        for shift in 0..9 {
            let byte = *block.get(*pos)?;
            *pos += 1;
            value |= u64::from(byte & 0x7F) << (shift * 7);
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    };
    if flags & 0x40 != 0 {
        varint(&mut pos)?;
    }
    if flags & 0x80 != 0 {
        varint(&mut pos)?;
    }
    let mut largest = None;
    for _ in 0..filters {
        let id = varint(&mut pos)?;
        let size = varint(&mut pos)? as usize;
        let props = block.get(pos..pos + size)?;
        pos += size;
        if id == 0x21 {
            let bits = u32::from(*props.first()? & 0x3F);
            let dict = if bits >= 40 {
                u64::from(u32::MAX)
            } else {
                u64::from(2 | (bits & 1)) << (bits / 2 + 11)
            };
            largest = Some(largest.map_or(dict, |l: u64| l.max(dict)));
        }
    }
    largest
}

/// Model memory declared by a PPMd (method 98) ZIP entry: the 2-byte header holds the
/// order, the memory size in MiB minus one, and the restore method.
pub(crate) fn zip_ppmd_memory(head: &[u8]) -> Option<u64> {
    let v = u16::from_le_bytes([*head.first()?, *head.get(1)?]);
    Some((u64::from((v >> 4) & 0xFF) + 1) << 20)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malicious_names_are_rejected() {
        for evil in [
            "../evil.txt",
            "ok/../../evil.txt",
            "..\\evil.txt",
            "a\\..\\..\\evil.txt",
            "/abs.txt",
            "\\abs.txt",
            "C:/Windows/evil.txt",
            "C:evil.txt",
            "c:\\evil.txt",
            "\\\\server\\share\\evil.txt",
            "//server/share/evil.txt",
            "\\\\?\\C:\\evil.txt",
            "\\\\.\\PhysicalDrive0",
            "file.txt:ads",
            "dir/file.txt:Zone.Identifier",
            "nul\0byte.txt",
            "tab\there.txt",
            "CON",
            "con",
            "PRN.txt",
            "aux.tar.gz",
            "NUL",
            "dir/COM1",
            "COM9.log",
            "LPT1",
            "lpt9.txt",
            "COM1 .txt",
            "trailing.",
            "trailing ",
            "dir./x.txt",
            "dir /x.txt",
            "",
            "/",
            "..",
            "./..",
        ] {
            assert!(
                validate_entry_name(evil).is_err(),
                "{evil:?} must be rejected"
            );
        }
    }

    #[test]
    fn ordinary_names_are_accepted() {
        for (ok, expect) in [
            ("a.txt", "a.txt"),
            ("dir/", "dir"),
            ("dir\\sub\\x.txt", "dir/sub/x.txt"),
            ("./a/./b.txt", "a/b.txt"),
            ("Проект 日本/файл.txt", "Проект 日本/файл.txt"),
            ("COM10.txt", "COM10.txt"),
            ("console.log", "console.log"),
            (".hidden", ".hidden"),
        ] {
            let rel = validate_entry_name(ok).unwrap();
            assert_eq!(paths::to_slash(&rel), expect);
        }
    }

    #[test]
    fn collisions_ignore_case_and_separators() {
        let a = validate_entry_name("Dir/File.TXT").unwrap();
        let b = validate_entry_name("dir\\file.txt").unwrap();
        assert_eq!(collision_key(&a), collision_key(&b));
        assert_eq!(normalize_entry_name("a\\b\\"), "a/b/");
    }

    #[cfg(windows)]
    #[test]
    fn pinned_folders_cannot_be_swapped_while_held() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("dest");
        fs::create_dir(&base).unwrap();
        let pinned_base = PinnedBase::open(&base).unwrap();
        let pinned = pin_dirs(&pinned_base, Path::new("a/b")).unwrap();
        // Neither a folder of the chain nor the base can be renamed or removed, so none can
        // be replaced by a junction while an entry is written beneath them.
        assert!(fs::rename(base.join("a"), d.path().join("moved")).is_err());
        assert!(fs::rename(base.join("a/b"), base.join("a/c")).is_err());
        assert!(fs::remove_dir(base.join("a/b")).is_err());
        assert!(fs::rename(&base, d.path().join("dest2")).is_err());
        // Nor can any folder above the destination, up to the volume root.
        assert!(fs::rename(d.path(), d.path().with_extension("moved")).is_err());
        // Writing beneath them still works.
        fs::write(base.join("a/b/x.txt"), b"x").unwrap();
        fs::rename(base.join("a/b/x.txt"), base.join("a/b/y.txt")).unwrap();
        drop(pinned);
        drop(pinned_base);
        fs::remove_file(base.join("a/b/y.txt")).unwrap();
        fs::rename(base.join("a"), d.path().join("moved")).unwrap();

        // A junction planted in the destination is refused, and nothing is created through it.
        let outside = d.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(base.join("j"))
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
        if !made {
            eprintln!("mklink unavailable; skipping the junction assertion");
            return;
        }
        let pinned_base = PinnedBase::open(&base).unwrap();
        let err = pin_dirs(&pinned_base, Path::new("j/sub"))
            .err()
            .expect("refused");
        assert!(matches!(err, CoreError::UnsafePath(_)), "{err:?}");
        assert!(!outside.join("sub").exists());
    }

    #[test]
    fn decoder_memory_headers_parse() {
        // version 5.2, props size 5, lc/lp/pb byte, 64 MiB dictionary.
        let lzma = [5, 2, 5, 0, 0x5D, 0, 0, 0, 4];
        assert_eq!(zip_lzma_dictionary(&lzma), Some(64 << 20));
        // 8 MiB PPMd model, order 6.
        assert_eq!(
            zip_ppmd_memory(&((7u16 << 4) | 5).to_le_bytes()),
            Some(8 << 20)
        );
        let mut xz = vec![0xFD, b'7', b'z', b'X', b'Z', 0, 0, 1, 0, 0, 0, 0];
        // Block header: size byte 2 (12 bytes), one filter, LZMA2 (0x21) with 1 prop byte.
        xz.extend([2, 0, 0x21, 1, 22, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(xz_dictionary(&xz), Some(8 << 20));
        assert_eq!(xz_dictionary(b"not xz"), None);
    }
}
