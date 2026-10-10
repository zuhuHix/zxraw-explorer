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
    fn volume_serial_is_stable_for_siblings() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        let b = d.path().join("b.txt");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        assert_eq!(volume_serial(&a).unwrap(), volume_serial(&b).unwrap());
    }
}
