//! Link-safe file access for the Git scan.
//!
//! The folder being scanned is untrusted: any name in it can be a symlink, junction or other
//! reparse point leading elsewhere, including to a network share (merely opening such a path
//! makes Windows connect, which leaks the user's NTLM hash). So the scan never opens anything
//! by path except the root of a local volume (`C:\`, checked to be a local drive first). Every
//! other folder or file, the browsed folder's ancestors included, is opened one name at a time,
//! relative to a handle already validated, with `FILE_OPEN_REPARSE_POINT` (a link is opened as
//! itself and never followed) and `FILE_OPEN_NO_RECALL` (a cloud placeholder is not
//! downloaded). Each handle is then checked before use:
//!
//! * it must not be a reparse point (the open did not follow it, so it is refused instead);
//! * it must be on the root's volume (64-bit volume serial number), and its final path must be
//!   inside the root's (or, while walking down to the root, be the next folder on the way);
//! * a file must have no other names (hard links), which could be outside the folder;
//! * it must not be an online-only placeholder.
//!
//! Nothing is opened before its parent folder's listing has been seen: a folder or file can
//! only be opened from an [`Entry`] that the listing of that same, held parent produced
//! ([`Dir::list`] or [`Fs::lookup`]), and an entry that is a reparse point or a cloud
//! placeholder (recall on open or on data access, offline) is refused without being opened.
//! (`FILE_OPEN_NO_RECALL` alone does not stop a read from downloading a placeholder.)
//!
//! Since a file is read through the handle that was checked, swapping a name for a link
//! between the check and the read changes nothing. Reads take a byte cap.
//!
//! On Unix ([`unix`]) the same rules hold with the tools Unix has: the root `/` is the only
//! open by path; every other name is opened relative to a held folder with `openat` and
//! `O_NOFOLLOW` (a symlink is refused, never followed) after `fstatat(AT_SYMLINK_NOFOLLOW)`
//! saw it in that folder. A mount point counts as a link once the scan is confined to its root
//! (a different `st_dev`, like a mounted folder's reparse point on Windows); a folder on a
//! network or FUSE file system is refused; a file must have one name (`st_nlink`), be a regular
//! file (no FIFO or device is ever opened) and lie under the root's `/proc/self/fd` path.

use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
#[cfg(windows)]
use std::sync::atomic::Ordering;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod win;
#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use win::*;

/// An open file or folder.
#[cfg(windows)]
type Handle = std::os::windows::io::OwnedHandle;
/// An open file or folder.
#[cfg(unix)]
type Handle = std::os::fd::OwnedFd;

/// `FILE_ATTRIBUTE_DIRECTORY`; on Unix, set for a folder.
pub const ATTR_DIRECTORY: u32 = 0x10;
/// `FILE_ATTRIBUTE_REPARSE_POINT`; on Unix, set for a symlink and (once the scan is confined to
/// its root) a mount point.
pub const ATTR_LINK: u32 = 0x400;

/// Why something could not be opened or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Nothing by that name.
    NotFound,
    /// A folder where a file was expected, or the other way round.
    WrongKind,
    /// A link (any reparse point), a file with other names (hard links), or a handle that
    /// ended up off the root's volume or outside its folder. Never followed.
    Link,
    /// An online-only placeholder; never read, which would download it.
    OnlineOnly,
    /// A name that cannot be opened as a single plain component.
    BadName,
    /// Larger than the caller's cap.
    TooBig,
    /// Anything else.
    Io(String),
}

/// What a listing or an open handle says about an entry.
#[derive(Debug, Clone, Copy)]
pub struct Info {
    pub attributes: u32,
    pub size: u64,
    /// Last write time: on Windows a `FILETIME` (100 ns units since 1601), on Unix nanoseconds
    /// since 1970 (see `git_status::unix_time`).
    pub mtime: i64,
}

impl Info {
    pub fn is_dir(&self) -> bool {
        self.attributes & ATTR_DIRECTORY != 0
    }
    pub fn is_link(&self) -> bool {
        self.attributes & ATTR_LINK != 0
    }
}

/// Attributes of a cloud placeholder whose data (or the file itself) is not on this PC:
/// `OFFLINE`, `RECALL_ON_OPEN`, `RECALL_ON_DATA_ACCESS`.
pub const PLACEHOLDER: u32 = 0x1000 | 0x4_0000 | 0x40_0000;

/// One entry of a folder listing.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub info: Info,
    /// The [`Dir::serial`] of the folder whose listing produced it.
    parent: u64,
}

impl Entry {
    /// Is this a placeholder that reading (or opening) could download?
    pub fn is_placeholder(&self) -> bool {
        self.info.attributes & PLACEHOLDER != 0
            || forge_core::cloud::is_online_only_attrs(self.info.attributes)
    }
}

/// Serial numbers of open folders, so an [`Entry`] can only be opened through its own parent.
static DIR_SERIAL: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
thread_local! {
    /// Names that listings on this thread report as online-only placeholders, for tests.
    pub static FAKE_PLACEHOLDERS: std::cell::RefCell<Option<Box<dyn Fn(&str) -> bool>>> =
        const { std::cell::RefCell::new(None) };
}

/// The entry `name`/`info` listed in folder `parent`.
fn entry(name: String, mut info: Info, parent: u64) -> Entry {
    #[cfg(test)]
    FAKE_PLACEHOLDERS.with(|fake| {
        if fake.borrow().as_ref().is_some_and(|f| f(&name)) {
            info.attributes |= 0x40_0000;
        }
    });
    #[cfg(not(test))]
    let _ = &mut info;
    Entry { name, info, parent }
}

/// The scan's view of the file system: the root it may not leave, and (in tests) a record of
/// every handle it opened.
pub struct Fs {
    /// The root's volume serial number (Windows) or `st_dev` (Unix).
    volume: u64,
    /// Unix: has [`Fs::narrow`] confined the scan yet? Until then (walking down from `/`)
    /// crossing a mount point is allowed.
    #[cfg(unix)]
    narrowed: bool,
    /// Unix: the root's mount id once narrowed (`statx` `stx_mnt_id`), where known.
    #[cfg(unix)]
    mount: Option<u64>,
    /// Final path of the root (lowercase on Windows), without a trailing separator.
    root_final: String,
    /// Final paths of every handle opened, in tests.
    opened: Option<Mutex<Vec<String>>>,
}

/// An open, checked folder.
pub struct Dir {
    handle: Handle,
    pub info: Info,
    serial: u64,
    /// The folder's `st_dev`: an entry on another device is a mount point.
    #[cfg(unix)]
    dev: u64,
    /// The folder's mount id (`statx` `stx_mnt_id`), where known: an entry on another mount
    /// (a bind mount of the same device) is a mount point too.
    #[cfg(unix)]
    mount: Option<u64>,
}

impl Dir {
    #[cfg(windows)]
    fn new(handle: Handle, info: Info) -> Dir {
        Dir {
            handle,
            info,
            serial: DIR_SERIAL.fetch_add(1, Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Opens by path on this thread, for tests.
    pub static PATH_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// An open, checked file.
pub struct File {
    file: std::fs::File,
    pub info: Info,
}

impl Fs {
    /// Final paths of every handle opened so far, if recording.
    #[cfg(test)]
    pub fn opened(&self) -> Vec<String> {
        self.opened
            .as_ref()
            .map(|m| m.lock().unwrap().clone())
            .unwrap_or_default()
    }

    /// Final path of the root, as recorded.
    #[cfg(test)]
    pub fn root_final(&self) -> &str {
        &self.root_final
    }

    fn note(&self, path: &str) {
        if let Some(opened) = &self.opened {
            opened.lock().unwrap().push(path.to_string());
        }
    }

    /// Opens folder `entry`, listed in `parent`.
    #[cfg(windows)]
    pub fn dir(&self, parent: &Dir, entry: &Entry) -> Result<Dir, Error> {
        let (handle, info) = self.open(parent, entry, true)?;
        Ok(Dir::new(handle, info))
    }

    /// Opens file `entry`, listed in `parent`.
    #[cfg(windows)]
    pub fn file(&self, parent: &Dir, entry: &Entry) -> Result<File, Error> {
        let (handle, info) = self.open(parent, entry, false)?;
        Ok(File {
            file: std::fs::File::from(handle),
            info,
        })
    }

    /// Looks folder `name` up in `parent`, then opens it.
    pub fn dir_named(&self, parent: &Dir, name: &str) -> Result<Dir, Error> {
        let entry = self.lookup(parent, name)?;
        self.dir(parent, &entry)
    }

    /// Looks file `name` up in `parent`, then opens it.
    pub fn file_named(&self, parent: &Dir, name: &str) -> Result<File, Error> {
        let entry = self.lookup(parent, name)?;
        self.file(parent, &entry)
    }

    /// Opens the file at `/`-separated `rel` below `base`, one component at a time.
    pub fn file_at(&self, base: &Dir, rel: &str) -> Result<File, Error> {
        let mut parts: Vec<&str> = rel.split('/').collect();
        let last = parts.pop().ok_or(Error::BadName)?;
        let mut owned: Option<Dir> = None;
        for part in parts {
            let next = self.dir_named(owned.as_ref().unwrap_or(base), part)?;
            owned = Some(next);
        }
        self.file_named(owned.as_ref().unwrap_or(base), last)
    }

    /// Reads file `rel` below `base` whole, if it exists and holds at most `cap` bytes.
    /// `Ok(None)` when there is no such file.
    pub fn read_opt(&self, base: &Dir, rel: &str, cap: u64) -> Result<Option<Vec<u8>>, Error> {
        match self.file_at(base, rel) {
            Ok(file) => file.read_capped(cap).map(Some),
            Err(Error::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl File {
    /// The whole file, refusing (without reading) one over `cap` bytes.
    pub fn read_capped(mut self, cap: u64) -> Result<Vec<u8>, Error> {
        if self.info.size > cap {
            return Err(Error::TooBig);
        }
        let mut out = Vec::with_capacity(self.info.size as usize);
        (&mut self.file)
            .take(cap + 1)
            .read_to_end(&mut out)
            .map_err(|e| Error::Io(e.to_string()))?;
        if out.len() as u64 > cap {
            return Err(Error::TooBig);
        }
        Ok(out)
    }
}

/// A single name the file system takes as written: not empty, not `.` or `..`, no separator,
/// no `:` (streams), no NUL. Device names mean nothing to a relative `NtCreateFile`. On Unix
/// `\\` and `:` are ordinary characters; only `/` and NUL are refused.
pub fn plain_component(name: &str) -> bool {
    let separators: &[char] = if cfg!(windows) {
        &['\\', '/', ':', '\0']
    } else {
        &['/', '\0']
    };
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 255 * 3
        && !name.contains(separators)
}
