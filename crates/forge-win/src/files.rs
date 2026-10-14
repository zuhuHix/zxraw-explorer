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

/// Number of hard links to the file (1 for a normal file). Directories report their
/// link count too.
#[cfg(windows)]
pub fn hard_link_count(path: &Path) -> Result<u32> {
    Ok(handle_info(path)?.nNumberOfLinks)
}

#[cfg(not(windows))]
pub fn hard_link_count(_path: &Path) -> Result<u32> {
    Err(WinError::Unsupported)
}

/// Serial number of the volume that holds `path`.
#[cfg(windows)]
pub fn volume_serial(path: &Path) -> Result<u32> {
    Ok(handle_info(path)?.dwVolumeSerialNumber)
}

#[cfg(not(windows))]
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

#[cfg(not(windows))]
pub fn file_id(_path: &Path) -> Result<(u32, u64)> {
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
pub fn set_attributes(_path: &Path, _readonly: bool, _hidden: bool) -> Result<()> {
    Err(WinError::Unsupported)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::fs::MetadataExt;

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
