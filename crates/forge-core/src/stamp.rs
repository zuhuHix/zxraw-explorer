//! File identity stamps: size, last-write time, volume and file index, and link count.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// What a concurrent writer or a swapped file changes: size, last-write time and file identity
/// (volume serial and file index), plus the hard-link count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub(crate) len: u64,
    pub(crate) modified: u64,
    pub(crate) volume: u32,
    pub(crate) index: u64,
    pub(crate) links: u32,
}

/// The [`Stamp`] of an open file, from `GetFileInformationByHandle`.
#[cfg(windows)]
pub(crate) fn handle_stamp(file: &File) -> io::Result<Stamp> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is owned by `file`, which outlives the call; `info` is a valid out
    // pointer.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(io::Error::other)?;
    let join = |hi: u32, lo: u32| (u64::from(hi) << 32) | u64::from(lo);
    Ok(Stamp {
        len: join(info.nFileSizeHigh, info.nFileSizeLow),
        modified: join(
            info.ftLastWriteTime.dwHighDateTime,
            info.ftLastWriteTime.dwLowDateTime,
        ),
        volume: info.dwVolumeSerialNumber,
        index: join(info.nFileIndexHigh, info.nFileIndexLow),
        links: info.nNumberOfLinks,
    })
}

#[cfg(not(windows))]
pub(crate) fn handle_stamp(file: &File) -> io::Result<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let md = file.metadata()?;
    Ok(Stamp {
        len: md.len(),
        modified: (md.mtime() as u64) << 32 ^ md.mtime_nsec() as u64,
        volume: md.dev() as u32,
        index: md.ino(),
        links: md.nlink() as u32,
    })
}

/// The [`Stamp`] of `path`, opened for attributes only and sharing everything, so no other
/// program is disturbed. A link at `path` is not followed.
pub(crate) fn path_stamp(path: &Path) -> io::Result<Stamp> {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(path)?
    };
    #[cfg(not(windows))]
    let file = File::open(path)?;
    handle_stamp(&file)
}
