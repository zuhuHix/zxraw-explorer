//! The Windows half of [`super`]: `NtCreateFile` relative opens, `FILE_OPEN_REPARSE_POINT`,
//! volume serial numbers and final paths by handle.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::Mutex;

use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_NO_RECALL,
    FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, FileFullDirectoryInformation,
    NTCREATEFILE_CREATE_OPTIONS, NtCreateFile, NtQueryDirectoryFile,
};
use windows::Win32::Foundation::{HANDLE, OBJ_CASE_INSENSITIVE, UNICODE_STRING};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ACCESS_RIGHTS, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES, FILE_FULL_DIR_INFO, FILE_ID_INFO,
    FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_READ_DATA,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FileFullDirectoryInfo,
    FileFullDirectoryRestartInfo, FileIdInfo, GETFINALPATHNAMEBYHANDLE_FLAGS, GetDriveTypeW,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    OPEN_EXISTING, QueryDosDeviceW, SYNCHRONIZE, VOLUME_NAME_DOS, VOLUME_NAME_GUID,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::core::PCWSTR;

#[cfg(test)]
use super::PATH_OPENS;
use super::{Dir, Entry, Error, File, Fs, Info, PLACEHOLDER, entry, plain_component};

/// Is `target`, what `QueryDosDeviceW` reports for a drive letter, a volume device itself
/// (`\Device\HarddiskVolume3`, `\Device\CdRom0`)? Not for a SUBST alias
/// (`\??\C:\some\folder`), whose target path could hold links, nor a network redirector.
pub fn is_volume_device(target: &str) -> bool {
    let Some(name) = target.strip_prefix(r"\Device\") else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    !name.is_empty()
        && !name.contains(['\\', '/', ';', ':'])
        && !lower.contains("redirector")
        && !lower.starts_with("mup")
        && !lower.starts_with("webdav")
}

/// The drive letter of `path` if it is on a local drive (fixed, removable, CD or RAM disk),
/// asked of the system without opening anything. `None` for shares, mapped network drives,
/// device paths and drives that do not exist.
pub fn local_drive(path: &Path) -> Option<char> {
    use std::path::{Component, Prefix};
    let letter = match path.components().next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => (d as char).to_ascii_uppercase(),
            _ => return None,
        },
        _ => return None,
    };
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
    // DRIVE_REMOVABLE, DRIVE_FIXED, DRIVE_CDROM, DRIVE_RAMDISK
    if !matches!(
        unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) },
        2 | 3 | 5 | 6
    ) {
        return None;
    }
    // A SUBST drive is reported as local, but is an alias for a folder path that could hold
    // links; only a drive letter naming a volume device itself is used.
    let device: Vec<u16> = format!("{letter}:").encode_utf16().chain([0]).collect();
    let mut target = vec![0u16; 1024];
    let n = unsafe { QueryDosDeviceW(PCWSTR(device.as_ptr()), Some(&mut target)) } as usize;
    let first = target[..n.min(target.len())].split(|&c| c == 0).next()?;
    is_volume_device(&String::from_utf16_lossy(first)).then_some(letter)
}

/// The root folder of local drive `letter`.
pub fn volume_root(letter: char) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{letter}:\\"))
}

/// The create options of every relative open: never follow a link, and for files never recall
/// a cloud placeholder. (NTFS refuses `FILE_OPEN_NO_RECALL` on folders; a placeholder folder is
/// a reparse point, refused as such, and listings skip online-only folders before any open.)
pub fn create_options(dir: bool) -> NTCREATEFILE_CREATE_OPTIONS {
    let kind = if dir {
        FILE_DIRECTORY_FILE
    } else {
        FILE_NON_DIRECTORY_FILE
    };
    if dir {
        kind | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT
    } else {
        kind | FILE_OPEN_REPARSE_POINT | FILE_OPEN_NO_RECALL | FILE_SYNCHRONOUS_IO_NONALERT
    }
}

impl Fs {
    /// Opens the root of local drive `letter` (see [`local_drive`]), the only open by path.
    /// The root of a volume is never a link. With `record`, every opened handle's final path
    /// is kept (see [`Fs::opened`]).
    pub fn open_volume(letter: char, record: bool) -> Result<(Fs, Dir), Error> {
        #[cfg(test)]
        PATH_OPENS.with(|n| n.set(n.get() + 1));
        let path: Vec<u16> = format!(r"\\?\{letter}:\")
            .encode_utf16()
            .chain([0])
            .collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                (FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }
        .map_err(|e| Error::Io(e.to_string()))?;
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0 as _) };
        let (info, _) = by_handle(&handle)?;
        if !info.is_dir() || info.is_link() {
            return Err(Error::WrongKind);
        }
        // What was opened must be the root of a volume: `\\?\Volume{...}\`, nothing more.
        if !is_volume_root(&final_path_as(&handle, VOLUME_NAME_GUID)?) {
            return Err(Error::Link);
        }
        let root_final = final_path(&handle)?;
        if !root_final.starts_with(r"\\?\") || root_final.to_lowercase().starts_with(r"\\?\unc\") {
            return Err(Error::Link);
        }
        let fs = Fs {
            volume: volume_id(&handle)?,
            root_final: root_final.trim_end_matches('\\').to_lowercase(),
            opened: record.then(|| Mutex::new(Vec::new())),
        };
        fs.note(&root_final);
        Ok((fs, Dir::new(handle, info)))
    }

    /// Makes `dir`, a folder opened through this `Fs`, the root nothing may leave from now on.
    pub fn narrow(&mut self, dir: &Dir) -> Result<(), Error> {
        self.root_final = final_path(&dir.handle)?
            .trim_end_matches('\\')
            .to_lowercase();
        Ok(())
    }

    /// Opens `entry`, listed in `parent`, without following links. Refused before opening:
    /// an entry from another folder's listing, a link, a placeholder, the wrong kind. Checked
    /// on the handle after: not a link, the expected kind, on the root's volume, inside the
    /// root, no other names (files), not a placeholder.
    pub(super) fn open(&self, parent: &Dir, entry: &Entry, dir: bool) -> Result<(OwnedHandle, Info), Error> {
        let name = entry.name.as_str();
        if entry.parent != parent.serial || !plain_component(name) {
            return Err(Error::BadName);
        }
        if entry.info.is_link() {
            return Err(Error::Link);
        }
        if entry.is_placeholder() {
            return Err(Error::OnlineOnly);
        }
        if entry.info.is_dir() != dir {
            return Err(Error::WrongKind);
        }
        let wide: Vec<u16> = name.encode_utf16().collect();
        let bytes = u16::try_from(wide.len() * 2).map_err(|_| Error::BadName)?;
        let name_us = UNICODE_STRING {
            Length: bytes,
            MaximumLength: bytes,
            Buffer: windows::core::PWSTR(wide.as_ptr() as *mut u16),
        };
        let attrs = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: HANDLE(parent.handle.as_raw_handle() as _),
            ObjectName: &name_us,
            Attributes: OBJ_CASE_INSENSITIVE,
            ..Default::default()
        };
        let access: FILE_ACCESS_RIGHTS = if dir {
            FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE
        } else {
            FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE
        };
        let mut raw = HANDLE::default();
        let mut iosb = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtCreateFile(
                &mut raw,
                access,
                &attrs,
                &mut iosb,
                None,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
                create_options(dir),
                None,
                0,
            )
        };
        if status.is_err() {
            return Err(match status.0 as u32 {
                // OBJECT_NAME_NOT_FOUND, OBJECT_PATH_NOT_FOUND, NO_SUCH_FILE, DELETE_PENDING
                0xC000_0034 | 0xC000_003A | 0xC000_000F | 0xC000_0056 => Error::NotFound,
                // NOT_A_DIRECTORY, FILE_IS_A_DIRECTORY
                0xC000_0103 | 0xC000_00BA => Error::WrongKind,
                // OBJECT_NAME_INVALID
                0xC000_0033 => Error::BadName,
                code => Error::Io(format!("NTSTATUS {code:#010x}")),
            });
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw.0 as _) };
        // A link was opened as itself, so this is the link's own path.
        let path = final_path(&handle)?;
        self.note(&path);
        let (info, links) = by_handle(&handle)?;
        if info.is_link() || volume_id(&handle)? != self.volume {
            return Err(Error::Link);
        }
        if info.is_dir() != dir {
            return Err(Error::WrongKind);
        }
        if !dir && links > 1 {
            return Err(Error::Link);
        }
        if info.attributes & PLACEHOLDER != 0
            || forge_core::cloud::is_online_only_attrs(info.attributes)
        {
            return Err(Error::OnlineOnly);
        }
        if !path
            .to_lowercase()
            .strip_prefix(&self.root_final)
            .is_some_and(|rest| rest.starts_with('\\'))
        {
            return Err(Error::Link);
        }
        Ok((handle, info))
    }

    /// The entry `name` in folder `parent`, from a listing of `parent` filtered to that one
    /// name; nothing by that name is opened. `name` must be one plain component without
    /// wildcards.
    pub fn lookup(&self, parent: &Dir, name: &str) -> Result<Entry, Error> {
        if !plain_component(name) || name.contains(['*', '?', '<', '>', '"']) {
            return Err(Error::BadName);
        }
        // The listing runs on a fresh handle to the same folder: a handle keeps the name
        // filter of its first listing.
        let fresh = reopen(&parent.handle)?;
        let wide: Vec<u16> = name.encode_utf16().collect();
        let bytes = u16::try_from(wide.len() * 2).map_err(|_| Error::BadName)?;
        let name_us = UNICODE_STRING {
            Length: bytes,
            MaximumLength: bytes,
            Buffer: windows::core::PWSTR(wide.as_ptr() as *mut u16),
        };
        let mut buf = vec![0u64; 160];
        let mut iosb = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtQueryDirectoryFile(
                HANDLE(fresh.as_raw_handle() as _),
                None,
                None,
                None,
                &mut iosb,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                FileFullDirectoryInformation,
                true,
                Some(&name_us),
                true,
            )
        };
        if status.is_err() || status.0 as u32 == 0x8000_0006 {
            return Err(match status.0 as u32 {
                // NO_SUCH_FILE, NO_MORE_FILES, OBJECT_NAME_NOT_FOUND
                0xC000_000F | 0x8000_0006 | 0xC000_0034 => Error::NotFound,
                code => Error::Io(format!("NTSTATUS {code:#010x}")),
            });
        }
        // SAFETY: the system wrote one whole record at the start of `buf`.
        let record = unsafe { &*buf.as_ptr().cast::<FILE_FULL_DIR_INFO>() };
        let len = (record.FileNameLength as usize / 2).min(buf.len() * 4 - 40);
        let name_ptr = std::ptr::addr_of!(record.FileName).cast::<u16>();
        let found = OsString::from_wide(unsafe { std::slice::from_raw_parts(name_ptr, len) })
            .to_string_lossy()
            .into_owned();
        if !found.eq_ignore_ascii_case(name) && found.to_lowercase() != name.to_lowercase() {
            return Err(Error::NotFound);
        }
        let info = Info {
            attributes: record.FileAttributes,
            size: record.EndOfFile.max(0) as u64,
            mtime: record.LastWriteTime,
        };
        Ok(entry(found, info, parent.serial))
    }
}

impl Dir {
    /// Lists the folder, calling `each` per entry (`.` and `..` skipped) until it returns
    /// false. The listing reports links as themselves; nothing is opened.
    pub fn list(&self, mut each: impl FnMut(Entry) -> bool) -> Result<(), Error> {
        // u64 for alignment of the records inside.
        let mut buf = vec![0u64; 8 * 1024];
        let mut class = FileFullDirectoryRestartInfo;
        loop {
            let got = unsafe {
                GetFileInformationByHandleEx(
                    HANDLE(self.handle.as_raw_handle() as _),
                    class,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                )
            };
            class = FileFullDirectoryInfo;
            if let Err(e) = got {
                // ERROR_NO_MORE_FILES
                if e.code() == windows::core::HRESULT::from_win32(18) {
                    return Ok(());
                }
                return Err(Error::Io(e.to_string()));
            }
            let base = buf.as_ptr().cast::<u8>();
            let mut offset = 0usize;
            loop {
                // SAFETY: the system wrote whole records within `buf`, each one starting at a
                // `NextEntryOffset` from the previous, aligned to 8 bytes.
                let record = unsafe { &*base.add(offset).cast::<FILE_FULL_DIR_INFO>() };
                let len = record.FileNameLength as usize / 2;
                let name_ptr = std::ptr::addr_of!(record.FileName).cast::<u16>();
                let name = unsafe { std::slice::from_raw_parts(name_ptr, len) };
                if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
                    let name = OsString::from_wide(name).to_string_lossy().into_owned();
                    let info = Info {
                        attributes: record.FileAttributes,
                        size: record.EndOfFile.max(0) as u64,
                        mtime: record.LastWriteTime,
                    };
                    if !each(entry(name, info, self.serial)) {
                        return Ok(());
                    }
                }
                if record.NextEntryOffset == 0 {
                    break;
                }
                offset += record.NextEntryOffset as usize;
            }
        }
    }
}

impl File {
    /// Up to `buf.len()` bytes at `offset`.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        std::os::windows::fs::FileExt::seek_read(&self.file, buf, offset)
    }
}

/// Attributes, size and write time of an open handle, and its number of names (hard links).
fn by_handle(handle: &OwnedHandle) -> Result<(Info, u32), Error> {
    let mut data = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(handle.as_raw_handle() as _), &mut data) }
        .map_err(|e| Error::Io(e.to_string()))?;
    let mtime = (u64::from(data.ftLastWriteTime.dwHighDateTime) << 32)
        | u64::from(data.ftLastWriteTime.dwLowDateTime);
    let info = Info {
        attributes: data.dwFileAttributes,
        size: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
        mtime: mtime as i64,
    };
    Ok((info, data.nNumberOfLinks))
}

/// The 64-bit serial number of the volume an open handle is on.
fn volume_id(handle: &OwnedHandle) -> Result<u64, Error> {
    let mut data = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(handle.as_raw_handle() as _),
            FileIdInfo,
            (&mut data as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(|e| Error::Io(e.to_string()))?;
    Ok(data.VolumeSerialNumber)
}

/// A second handle to the folder `handle` is open on, opened relative to it with an empty name.
fn reopen(handle: &OwnedHandle) -> Result<OwnedHandle, Error> {
    let empty = UNICODE_STRING::default();
    let attrs = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: HANDLE(handle.as_raw_handle() as _),
        ObjectName: &empty,
        Attributes: OBJ_CASE_INSENSITIVE,
        ..Default::default()
    };
    let mut raw = HANDLE::default();
    let mut iosb = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtCreateFile(
            &mut raw,
            FILE_LIST_DIRECTORY | SYNCHRONIZE,
            &attrs,
            &mut iosb,
            None,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            create_options(true),
            None,
            0,
        )
    };
    if status.is_err() {
        return Err(Error::Io(format!("NTSTATUS {:#010x}", status.0 as u32)));
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(raw.0 as _) })
}

/// Is `path` (a `VOLUME_NAME_GUID` final path) the root of a volume, `\\?\Volume{...}\`?
pub fn is_volume_root(path: &str) -> bool {
    path.strip_prefix(r"\\?\Volume{")
        .and_then(|rest| rest.strip_suffix("}\\"))
        .is_some_and(|guid| !guid.is_empty() && !guid.contains(['\\', '/', '{', '}']))
}

/// Normalized final path of an open handle (`\\?\C:\...` or `\\?\UNC\...`).
fn final_path(handle: &OwnedHandle) -> Result<String, Error> {
    final_path_as(handle, VOLUME_NAME_DOS)
}

/// Normalized final path of an open handle, with the volume named as `volume` asks.
fn final_path_as(
    handle: &OwnedHandle,
    volume: GETFINALPATHNAMEBYHANDLE_FLAGS,
) -> Result<String, Error> {
    let mut buf = vec![0u16; 1024];
    loop {
        let n = unsafe {
            GetFinalPathNameByHandleW(
                HANDLE(handle.as_raw_handle() as _),
                &mut buf,
                GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | volume.0),
            )
        } as usize;
        if n == 0 {
            return Err(Error::Io(std::io::Error::last_os_error().to_string()));
        }
        if n < buf.len() {
            return Ok(String::from_utf16_lossy(&buf[..n]));
        }
        if n > 64 * 1024 {
            return Err(Error::Io("path too long".into()));
        }
        buf.resize(n + 1, 0);
    }
}
