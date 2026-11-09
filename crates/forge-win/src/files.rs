//! Per-file Win32 queries and attribute edits.

use std::path::Path;

use crate::{Result, WinError};

#[cfg(windows)]
mod consts {
    pub const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
    pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
    pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    pub const FILE_ATTRIBUTE_NOT_CONTENT_INDEXED: u32 = 0x2000;
    /// Read attributes only; no data access is needed for the queries here.
    pub const FILE_READ_ATTRIBUTES: u32 = 0x80;
    pub const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    /// Do not follow a symlink or junction at the end of the path.
    pub const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    pub const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
}

/// Opens `path` for attribute queries and returns the handle information.
#[cfg(windows)]
fn handle_info(
    path: &Path,
) -> Result<windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let file = std::fs::OpenOptions::new()
        .access_mode(consts::FILE_READ_ATTRIBUTES)
        .share_mode(consts::FILE_SHARE_ALL)
        .custom_flags(consts::FILE_FLAG_BACKUP_SEMANTICS | consts::FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|source| WinError::Io {
            op: "open for attribute query",
            path: path.to_path_buf(),
            source,
        })?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(|e| crate::api_err("GetFileInformationByHandle", e))?;
    Ok(info)
}

/// Who an open file is: what [`handle_identity`] reads from a handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandleIdentity {
    /// The 64-bit volume serial number (`FILE_ID_INFO`).
    pub volume: u64,
    /// The 128-bit file id (`FILE_ID_INFO`): a file replaced by another of the same name
    /// gets a different one.
    pub file_id: u128,
    /// Hard links to it: 0 once its last name was removed (it was deleted or replaced while
    /// the handle stayed open).
    pub links: u32,
    pub size: u64,
    /// Last write time, FILETIME ticks.
    pub last_write: u64,
}

/// The identity of the file `file` has open (see [`HandleIdentity`]).
#[cfg(windows)]
pub fn handle_identity(file: &std::fs::File) -> Result<HandleIdentity> {
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx,
    };

    let handle = HANDLE(file.as_raw_handle());
    let mut id = FILE_ID_INFO::default();
    // SAFETY: `id` is a FILE_ID_INFO of the size passed; the handle is open.
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&mut id as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(|e| crate::api_err("GetFileInformationByHandleEx(FileIdInfo)", e))?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: as above.
    unsafe { GetFileInformationByHandle(handle, &mut info) }
        .map_err(|e| crate::api_err("GetFileInformationByHandle", e))?;
    Ok(HandleIdentity {
        volume: id.VolumeSerialNumber,
        file_id: u128::from_le_bytes(id.FileId.Identifier),
        links: info.nNumberOfLinks,
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        last_write: (u64::from(info.ftLastWriteTime.dwHighDateTime) << 32)
            | u64::from(info.ftLastWriteTime.dwLowDateTime),
    })
}

/// Identity fields from metadata (`fstat` for a handle, `lstat` for a path). The file id is
/// the inode number; the volume is the raw `st_dev`.
#[cfg(unix)]
fn identity_of(md: &std::fs::Metadata) -> HandleIdentity {
    use std::os::unix::fs::MetadataExt;

    HandleIdentity {
        volume: md.dev(),
        file_id: u128::from(md.ino()),
        links: u32::try_from(md.nlink()).unwrap_or(u32::MAX),
        size: md.size(),
        last_write: filetime_ticks(md.mtime(), md.mtime_nsec()),
    }
}

/// FILETIME ticks (100 ns since 1601-01-01 UTC) of a Unix time.
#[cfg(unix)]
fn filetime_ticks(secs: i64, nsec: i64) -> u64 {
    const UNIX_EPOCH_IN_FILETIME_SECS: i64 = 11_644_473_600;
    let secs = (secs + UNIX_EPOCH_IN_FILETIME_SECS).max(0) as u64;
    secs * 10_000_000 + nsec.max(0) as u64 / 100
}

/// `st_dev` folded to the 32-bit volume serial the Windows API reports.
#[cfg(unix)]
pub(crate) fn fold_dev(dev: u64) -> u32 {
    (dev ^ (dev >> 32)) as u32
}

/// The identity of the file `file` has open (see [`HandleIdentity`]), read with `fstat`.
#[cfg(unix)]
pub fn handle_identity(file: &std::fs::File) -> Result<HandleIdentity> {
    let md = file.metadata().map_err(|source| WinError::Io {
        op: "read file identity",
        path: std::path::PathBuf::new(),
        source,
    })?;
    Ok(identity_of(&md))
}

#[cfg(not(any(windows, unix)))]
pub fn handle_identity(_file: &std::fs::File) -> Result<HandleIdentity> {
    Err(WinError::Unsupported)
}

/// The identity of the file at `path` (not following a link at the end), read through an
/// attribute-only handle that shares everything.
#[cfg(windows)]
pub fn path_identity(path: &Path) -> Result<HandleIdentity> {
    use std::os::windows::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .access_mode(consts::FILE_READ_ATTRIBUTES)
        .share_mode(consts::FILE_SHARE_ALL)
        .custom_flags(consts::FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|source| WinError::Io {
            op: "open for an identity query",
            path: path.to_path_buf(),
            source,
        })?;
    handle_identity(&file)
}

/// The identity of the file at `path`, not following a link at the end (`lstat`).
#[cfg(unix)]
pub fn path_identity(path: &Path) -> Result<HandleIdentity> {
    let md = std::fs::symlink_metadata(path).map_err(|source| WinError::Io {
        op: "read an identity",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(identity_of(&md))
}

#[cfg(not(any(windows, unix)))]
pub fn path_identity(_path: &Path) -> Result<HandleIdentity> {
    Err(WinError::Unsupported)
}

/// Renames the file `file` has open (with `DELETE` access) to `target`, replacing what is
/// there, with POSIX semantics (`SetFileInformationByHandle(FileRenameInfoEx)`): the file
/// moved is the one held, whatever its old path names meanwhile. A file at `target` that is
/// open elsewhere is replaced only if those handles share deleting.
#[cfg(windows)]
pub fn rename_open_file_over(file: &std::fs::File, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FILE_RENAME_INFO, FileRenameInfoEx, SetFileInformationByHandle,
    };

    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
    const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;
    let name: Vec<u16> = target.as_os_str().encode_wide().collect();
    let header = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let bytes = header + (name.len() + 1) * 2;
    // u64 words: aligned for FILE_RENAME_INFO.
    let mut buf = vec![0u64; bytes.div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buf` holds `bytes` bytes, aligned; the name and its NUL fit after the header.
    unsafe {
        (*info).Anonymous.Flags = FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*info).FileNameLength = (name.len() * 2) as u32;
        let dst = std::ptr::addr_of_mut!((*info).FileName).cast::<u16>();
        std::ptr::copy_nonoverlapping(name.as_ptr(), dst, name.len());
        *dst.add(name.len()) = 0;
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileRenameInfoEx,
            info.cast(),
            bytes as u32,
        )
    }
    .map_err(|e| crate::api_err("SetFileInformationByHandle(FileRenameInfoEx)", e))
}

/// Renames the file `file` has open to `target`, replacing what is there.
///
/// Approximation: Linux cannot rename through a descriptor. This takes the name the kernel
/// has for the open file (`/proc/self/fd/N`) and renames that. It refuses when the file has
/// no name left or when the name no longer refers to the open file. A rename of another
/// file between those checks and the rename is not excluded.
#[cfg(target_os = "linux")]
pub fn rename_open_file_over(file: &std::fs::File, target: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::AsRawFd;

    const OP: &str = "rename open file";
    let held = file.metadata().map_err(|source| WinError::Io {
        op: OP,
        path: target.to_path_buf(),
        source,
    })?;
    if held.nlink() == 0 {
        return Err(WinError::Io {
            op: OP,
            path: target.to_path_buf(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        });
    }
    let link = format!("/proc/self/fd/{}", file.as_raw_fd());
    let current = std::fs::read_link(&link).map_err(|source| WinError::Io {
        op: OP,
        path: Path::new(&link).to_path_buf(),
        source,
    })?;
    let named = std::fs::symlink_metadata(&current).map_err(|source| WinError::Io {
        op: OP,
        path: current.clone(),
        source,
    })?;
    if named.dev() != held.dev() || named.ino() != held.ino() {
        return Err(WinError::Io {
            op: OP,
            path: current,
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the name no longer refers to the open file",
            ),
        });
    }
    std::fs::rename(&current, target).map_err(|source| WinError::Io {
        op: OP,
        path: current.clone(),
        source,
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn rename_open_file_over(_file: &std::fs::File, _target: &Path) -> Result<()> {
    Err(WinError::Unsupported)
}

/// Number of hard links to the file (1 for a normal file). Directories report their
/// link count too.
#[cfg(windows)]
pub fn hard_link_count(path: &Path) -> Result<u32> {
    Ok(handle_info(path)?.nNumberOfLinks)
}

#[cfg(unix)]
pub fn hard_link_count(path: &Path) -> Result<u32> {
    Ok(path_identity(path)?.links)
}

#[cfg(not(any(windows, unix)))]
pub fn hard_link_count(_path: &Path) -> Result<u32> {
    Err(WinError::Unsupported)
}

/// Serial number of the volume that holds `path`.
#[cfg(windows)]
pub fn volume_serial(path: &Path) -> Result<u32> {
    Ok(handle_info(path)?.dwVolumeSerialNumber)
}

#[cfg(unix)]
pub fn volume_serial(path: &Path) -> Result<u32> {
    Ok(fold_dev(path_identity(path)?.volume))
}

#[cfg(not(any(windows, unix)))]
pub fn volume_serial(_path: &Path) -> Result<u32> {
    Err(WinError::Unsupported)
}

/// Identity of the file or folder at `path` (not following a link at the end): the
/// volume serial and the file index. A file replaced by another of the same name gets a
/// different identity; renaming within the volume keeps it.
#[cfg(windows)]
pub fn file_id(path: &Path) -> Result<(u32, u64)> {
    let info = handle_info(path)?;
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((info.dwVolumeSerialNumber, index))
}

/// On Unix the volume part is the folded `st_dev` and the index part the inode number.
#[cfg(unix)]
pub fn file_id(path: &Path) -> Result<(u32, u64)> {
    let id = path_identity(path)?;
    Ok((fold_dev(id.volume), id.file_id as u64))
}

#[cfg(not(any(windows, unix)))]
pub fn file_id(_path: &Path) -> Result<(u32, u64)> {
    Err(WinError::Unsupported)
}

/// The lowercased `\\server\share` or `x:` a network path starts with, without touching the
/// network: UNC paths (also `\\?\UNC\…`) and drive letters that Windows reports as remote
/// (a mapped drive; `GetDriveTypeW` answers from the drive table). `None` for local paths.
#[cfg(windows)]
pub fn network_root(path: &Path) -> Option<String> {
    use std::path::{Component, Prefix};
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::core::PCWSTR;
    const DRIVE_REMOTE: u32 = 4;

    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return None;
    };
    match prefix.kind() {
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => Some(
            format!(
                r"\\{}\{}",
                server.to_string_lossy(),
                share.to_string_lossy()
            )
            .to_lowercase(),
        ),
        Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
            let root = crate::to_wide(format!(r"{}:\", d as char));
            let kind = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
            (kind == DRIVE_REMOTE).then(|| format!("{}:", d as char).to_lowercase())
        }
        _ => None,
    }
}

/// Linux: the `//host/share` of a CIFS/SMB mount that holds `path`, from the mount table.
/// Other network file systems (NFS, sshfs) have no share name here and give `None`.
#[cfg(target_os = "linux")]
pub fn network_root(path: &Path) -> Option<String> {
    crate::drives::linux::network_share_of(path).ok().flatten()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn network_root(_path: &Path) -> Option<String> {
    None
}

/// Identity of the volume that holds `path`, following folder mount points (a disk mounted
/// at `C:\Mount` is not `C:`): the volume GUID name for local volumes. Network paths are
/// identified by their share or mapped drive letter (`network_root`) without a network
/// call, so a slow or offline share cannot stall the caller.
#[cfg(windows)]
pub fn volume_identity(path: &Path) -> Result<String> {
    use windows::Win32::Storage::FileSystem::{
        GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    };
    use windows::core::PCWSTR;

    if let Some(root) = network_root(path) {
        return Ok(root);
    }
    // A verbatim path's root would come back verbatim: ask with the plain form instead
    // (`\\?\C:` as `C:`).
    let text = path.to_string_lossy();
    let plain = text.strip_prefix(r"\\?\").unwrap_or(&text).to_string();
    let wide = crate::to_wide(&plain);
    let mut root = vec![0u16; wide.len().max(261) + 1];
    unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut root) }
        .map_err(|e| crate::api_err("GetVolumePathNameW", e))?;
    let len = root.iter().position(|&c| c == 0).unwrap_or(root.len());
    let root_text = String::from_utf16_lossy(&root[..len]);
    if root_text.starts_with(r"\\") && !root_text.starts_with(r"\\?\") {
        return Ok(root_text.to_lowercase());
    }
    let mut name = [0u16; 64];
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(root.as_ptr()), &mut name) }
        .map_err(|e| crate::api_err("GetVolumeNameForVolumeMountPointW", e))?;
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Ok(String::from_utf16_lossy(&name[..len]).to_lowercase())
}

/// Linux: `uuid:<UUID>` of the file system that holds `path` (from `/dev/disk/by-uuid`), or
/// `dev:<major>:<minor>` when the device has no UUID. Symlinks in `path` are not followed.
#[cfg(target_os = "linux")]
pub fn volume_identity(path: &Path) -> Result<String> {
    crate::drives::linux::volume_identity(path).map_err(|source| WinError::Io {
        op: "volume identity",
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn volume_identity(_path: &Path) -> Result<String> {
    Err(WinError::Unsupported)
}

/// True if the volume holding `path` is removable (USB stick, SD card). UNC paths and
/// anything without a drive letter return `false`.
#[cfg(windows)]
pub fn is_removable(path: &Path) -> Result<bool> {
    use std::path::{Component, Prefix};

    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::core::PCWSTR;

    const DRIVE_REMOVABLE: u32 = 2;
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return Ok(false);
    };
    let letter = match prefix.kind() {
        Prefix::Disk(b) | Prefix::VerbatimDisk(b) => b as char,
        _ => return Ok(false),
    };
    let root = crate::to_wide(format!("{letter}:\\"));
    let kind = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
    Ok(kind == DRIVE_REMOVABLE)
}

/// Linux: true if the mount that holds `path` is a removable or USB device (see
/// `drives::linux`). Uses the mount table and sysfs only.
#[cfg(target_os = "linux")]
pub fn is_removable(path: &Path) -> Result<bool> {
    crate::drives::linux::path_is_removable(path).map_err(|source| WinError::Io {
        op: "removable check",
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn is_removable(_path: &Path) -> Result<bool> {
    Err(WinError::Unsupported)
}

/// Physical disk numbers backing the volume that holds `path` (several for spanned or
/// mirrored volumes). `None` when unknown: UNC/network paths, no drive letter, or the
/// query fails. Callers must treat `None` as "may share a disk with anything".
#[cfg(windows)]
pub fn physical_disks(path: &Path) -> Option<Vec<u32>> {
    use std::path::{Component, Prefix};

    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{DISK_EXTENT, VOLUME_DISK_EXTENTS};
    use windows::core::PCWSTR;

    /// Room for this many extents; a volume spanning more disks reports `None`.
    const MAX_EXTENTS: usize = 16;

    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return None;
    };
    let letter = match prefix.kind() {
        Prefix::Disk(b) | Prefix::VerbatimDisk(b) => b as char,
        _ => return None,
    };
    let device = crate::to_wide(format!("\\\\.\\{letter}:"));

    // Zero access rights: no data access is needed, so no administrator rights either.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(device.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()?;

    // u64 words keep the buffer 8-byte aligned for the DISK_EXTENT fields.
    let bytes = std::mem::size_of::<VOLUME_DISK_EXTENTS>()
        + MAX_EXTENTS * std::mem::size_of::<DISK_EXTENT>();
    let mut buf = vec![0u64; bytes.div_ceil(8)];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            None,
            0,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 8) as u32,
            Some(&mut returned),
            None,
        )
    };
    unsafe {
        let _ = CloseHandle(handle);
    }
    ok.ok()?;

    let mut disks: Vec<u32> = unsafe {
        let extents = &*buf.as_ptr().cast::<VOLUME_DISK_EXTENTS>();
        let count = extents.NumberOfDiskExtents as usize;
        if count == 0 || count > MAX_EXTENTS {
            return None;
        }
        std::slice::from_raw_parts(extents.Extents.as_ptr(), count)
            .iter()
            .map(|e| e.DiskNumber)
            .collect()
    };
    disks.sort_unstable();
    disks.dedup();
    Some(disks)
}

#[cfg(not(windows))]
pub fn physical_disks(_path: &Path) -> Option<Vec<u32>> {
    None
}

/// Sets or clears the read-only and hidden attributes. Other attributes that the user can
/// set (archive, system, not-content-indexed) are preserved; directory and reparse bits
/// are never passed back to the system.
#[cfg(windows)]
pub fn set_attributes(path: &Path, readonly: bool, hidden: bool) -> Result<()> {
    use std::os::windows::fs::MetadataExt;

    use windows::Win32::Storage::FileSystem::{FILE_FLAGS_AND_ATTRIBUTES, SetFileAttributesW};
    use windows::core::PCWSTR;

    let md = std::fs::symlink_metadata(path).map_err(|source| WinError::Io {
        op: "read attributes",
        path: path.to_path_buf(),
        source,
    })?;
    let keep = md.file_attributes()
        & (consts::FILE_ATTRIBUTE_ARCHIVE
            | consts::FILE_ATTRIBUTE_SYSTEM
            | consts::FILE_ATTRIBUTE_NOT_CONTENT_INDEXED);
    let mut attrs = keep;
    if readonly {
        attrs |= consts::FILE_ATTRIBUTE_READONLY;
    }
    if hidden {
        attrs |= consts::FILE_ATTRIBUTE_HIDDEN;
    }
    if attrs == 0 {
        attrs = consts::FILE_ATTRIBUTE_NORMAL;
    }
    let wide = crate::to_wide(path);
    unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(attrs)) }
        .map_err(|e| crate::api_err("SetFileAttributesW", e))
}

/// Unix: `readonly` removes every write bit (`true`) or adds the owner's (`false`). Linux
/// and macOS have no hidden attribute (a hidden file is one whose name starts with a dot),
/// and renaming a file to hide it is not done, so `hidden` is accepted and ignored. A symbolic
/// link is left alone: its permission bits are not used.
#[cfg(unix)]
pub fn set_attributes(path: &Path, readonly: bool, _hidden: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let md = std::fs::symlink_metadata(path).map_err(|source| WinError::Io {
        op: "read attributes",
        path: path.to_path_buf(),
        source,
    })?;
    if md.file_type().is_symlink() {
        return Ok(());
    }
    let mode = md.permissions().mode() & 0o7777;
    let wanted = if readonly {
        mode & !0o222
    } else {
        mode | 0o200
    };
    if wanted == mode {
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(wanted)).map_err(|source| {
        WinError::Io {
            op: "set attributes",
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(any(windows, unix)))]
pub fn set_attributes(_path: &Path, _readonly: bool, _hidden: bool) -> Result<()> {
    Err(WinError::Unsupported)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::fs::MetadataExt;

    #[test]
    fn volume_identity_is_shared_within_a_volume() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let a = volume_identity(d.path()).unwrap();
        assert!(a.starts_with(r"\\?\volume{"), "{a}");
        assert_eq!(volume_identity(&sub).unwrap(), a);
        let verbatim = std::path::PathBuf::from(format!(r"\\?\{}", sub.display()));
        assert_eq!(volume_identity(&verbatim).unwrap(), a);
        assert_eq!(network_root(d.path()), None);
    }

    #[test]
    fn network_paths_are_identified_without_the_network() {
        // An unreachable server answers at once: no lookup is made.
        let started = std::time::Instant::now();
        let plain = std::path::Path::new(r"\\no-such-host.invalid\Share\a\b.txt");
        let verbatim = std::path::Path::new(r"\\?\UNC\NO-SUCH-HOST.invalid\share\c");
        assert_eq!(
            volume_identity(plain).unwrap(),
            r"\\no-such-host.invalid\share"
        );
        assert_eq!(
            volume_identity(verbatim).unwrap(),
            r"\\no-such-host.invalid\share"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn hard_link_count_of_new_file_is_one() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("single.txt");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(hard_link_count(&f).unwrap(), 1);
    }

    #[test]
    fn hard_link_count_sees_extra_link() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"x").unwrap();
        std::fs::hard_link(&f, d.path().join("b.txt")).unwrap();
        assert_eq!(hard_link_count(&f).unwrap(), 2);
    }

    #[test]
    fn set_attributes_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("attrs.txt");
        std::fs::write(&f, b"x").unwrap();

        set_attributes(&f, true, true).unwrap();
        let md = std::fs::metadata(&f).unwrap();
        assert!(md.permissions().readonly());
        assert_ne!(md.file_attributes() & consts::FILE_ATTRIBUTE_HIDDEN, 0);

        set_attributes(&f, false, false).unwrap();
        let md = std::fs::metadata(&f).unwrap();
        assert!(!md.permissions().readonly());
        assert_eq!(md.file_attributes() & consts::FILE_ATTRIBUTE_HIDDEN, 0);
        // Leave the file deletable for the temp directory cleanup.
        std::fs::remove_file(&f).unwrap();
    }

    #[test]
    fn physical_disks_of_temp_dir_is_known() {
        let disks = physical_disks(&std::env::temp_dir()).expect("temp dir volume");
        assert!(!disks.is_empty());
        assert!(disks.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
    }

    #[test]
    fn physical_disks_of_siblings_match() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        let b = d.path().join("b.txt");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        assert_eq!(physical_disks(&a), physical_disks(&b));
    }

    #[test]
    fn physical_disks_of_unc_path_is_none() {
        assert_eq!(physical_disks(Path::new(r"\\server\share\x")), None);
    }

    #[test]
    fn volume_serial_is_stable_for_siblings() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        let b = d.path().join("b.txt");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        assert_eq!(volume_serial(&a).unwrap(), volume_serial(&b).unwrap());
    }

    #[test]
    fn file_id_survives_a_rename_but_not_a_replacement() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        let b = d.path().join("b.txt");
        std::fs::write(&a, b"x").unwrap();
        let first = file_id(&a).unwrap();
        assert_eq!(file_id(&a).unwrap(), first);
        std::fs::rename(&a, &b).unwrap();
        assert_eq!(file_id(&b).unwrap(), first, "a rename keeps the identity");
        // Keep `b` alive so the index cannot be reused by the new `a`.
        std::fs::write(&a, b"x").unwrap();
        assert_ne!(file_id(&a).unwrap(), first, "a new file has a new identity");
        assert!(file_id(d.path()).is_ok(), "folders have an identity too");
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode()
    }

    #[test]
    fn file_id_differs_between_files_and_is_stable() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        let b = d.path().join("b.txt");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        let first = file_id(&a).unwrap();
        assert_eq!(file_id(&a).unwrap(), first);
        assert_ne!(file_id(&b).unwrap(), first);
    }

    #[test]
    fn hard_links_share_a_file_id_and_count_two() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        let b = d.path().join("b.txt");
        std::fs::write(&a, b"x").unwrap();
        assert_eq!(hard_link_count(&a).unwrap(), 1);
        std::fs::hard_link(&a, &b).unwrap();
        assert_eq!(file_id(&a).unwrap(), file_id(&b).unwrap());
        assert_eq!(hard_link_count(&a).unwrap(), 2);
        assert_eq!(hard_link_count(&b).unwrap(), 2);
    }

    #[test]
    fn volume_serial_is_shared_in_one_directory() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        let b = d.path().join("b.txt");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        assert_eq!(volume_serial(&a).unwrap(), volume_serial(&b).unwrap());
        assert_eq!(volume_serial(&a).unwrap(), file_id(&a).unwrap().0);
    }

    #[test]
    fn handle_and_path_identity_agree() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        std::fs::write(&a, b"hello").unwrap();
        let f = std::fs::File::open(&a).unwrap();
        let by_handle = handle_identity(&f).unwrap();
        assert_eq!(by_handle, path_identity(&a).unwrap());
        assert_eq!(by_handle.size, 5);
        assert_eq!(by_handle.links, 1);
        assert!(by_handle.last_write > 0);
        assert_eq!(by_handle.file_id, u128::from(file_id(&a).unwrap().1));
    }

    #[test]
    fn set_attributes_readonly_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("ro.txt");
        std::fs::write(&f, b"x").unwrap();
        set_attributes(&f, true, false).unwrap();
        assert!(std::fs::metadata(&f).unwrap().permissions().readonly());
        assert_eq!(mode_of(&f) & 0o222, 0);
        set_attributes(&f, false, false).unwrap();
        assert!(!std::fs::metadata(&f).unwrap().permissions().readonly());
        assert_ne!(mode_of(&f) & 0o200, 0);
    }

    #[test]
    fn hidden_flag_is_ignored() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("visible.txt");
        std::fs::write(&f, b"x").unwrap();
        let before = mode_of(&f);
        set_attributes(&f, false, true).unwrap();
        assert!(f.exists(), "no rename happens");
        assert_eq!(mode_of(&f), before);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    #[test]
    fn rename_open_file_over_replaces_the_target() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("edited.tmp");
        let target = d.path().join("original.txt");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&target, b"old").unwrap();
        let held = std::fs::File::open(&target).unwrap();
        let edited = std::fs::File::open(&src).unwrap();
        rename_open_file_over(&edited, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!src.exists());
        assert_eq!(
            handle_identity(&held).unwrap().links,
            0,
            "the replaced file lost its name"
        );
    }

    #[test]
    fn volume_identity_is_shared_by_siblings() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        let b = d.path().join("b.txt");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        let v = volume_identity(&a).unwrap();
        assert!(v.starts_with("uuid:") || v.starts_with("dev:"), "{v}");
        assert_eq!(volume_identity(&b).unwrap(), v);
        assert!(is_removable(&a).is_ok());
    }
}
