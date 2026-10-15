//! Link-safe file access for the Git scan.
//!
//! The folder being scanned is untrusted: any name in it can be a symlink, junction or other
//! reparse point leading elsewhere, including to a network share (merely opening such a path
//! makes Windows connect, which leaks the user's NTLM hash). So the scan never opens anything
//! by path. The root is opened once; every other file or folder is opened one name at a time,
//! relative to a handle already validated, with `FILE_OPEN_REPARSE_POINT` so that a link is
//! opened as itself and never followed. Each handle is then checked before use:
//!
//! * it must not be a reparse point (the open did not follow it, so it is refused instead);
//! * it must be on the root's volume, and its final path must be inside the root's.
//!
//! Since a file is read through the handle that was checked, swapping a name for a link
//! between the check and the read changes nothing. Reads take a byte cap.

use std::ffi::OsString;
use std::io::Read;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::Mutex;

use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NTCREATEFILE_CREATE_OPTIONS, NtCreateFile,
};
use windows::Win32::Foundation::{HANDLE, OBJ_CASE_INSENSITIVE, UNICODE_STRING};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_FULL_DIR_INFO, FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
    FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
    FileFullDirectoryInfo, FileFullDirectoryRestartInfo, GETFINALPATHNAMEBYHANDLE_FLAGS,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    OPEN_EXISTING, SYNCHRONIZE, VOLUME_NAME_DOS,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::core::PCWSTR;

/// Why something could not be opened or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Nothing by that name.
    NotFound,
    /// A folder where a file was expected, or the other way round.
    WrongKind,
    /// A link (any reparse point), or a handle that ended up off the root's volume or outside
    /// its folder. Never followed.
    Link,
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
    /// Last write time, as a Windows `FILETIME` (100 ns units since 1601).
    pub mtime: i64,
}

impl Info {
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0
    }
    pub fn is_link(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    }
}

/// One entry of a folder listing.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub info: Info,
}

/// The scan's view of the file system: the root it may not leave, and (in tests) a record of
/// every handle it opened.
pub struct Fs {
    volume: u32,
    /// Final path of the root, lowercase, without a trailing `\`.
    root_final: String,
    opened: Option<Mutex<Vec<String>>>,
}

/// An open, checked folder.
pub struct Dir {
    handle: OwnedHandle,
}

/// An open, checked file.
pub struct File {
    file: std::fs::File,
    pub info: Info,
}

impl Fs {
    /// Opens the folder at `root`. Links in `root` itself are followed: that is the folder the
    /// user chose to browse. Nothing below it ever is. With `record`, every opened handle's
    /// final path is kept (see [`Fs::opened`]).
    pub fn open_root(root: &Path, record: bool) -> Result<(Fs, Dir), Error> {
        let wide: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                (FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .map_err(|e| Error::Io(e.to_string()))?;
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0 as _) };
        let (info, volume) = by_handle(&handle)?;
        if !info.is_dir() {
            return Err(Error::WrongKind);
        }
        let root_final = final_path(&handle)?;
        let fs = Fs {
            volume,
            root_final: root_final.trim_end_matches('\\').to_lowercase(),
            opened: record.then(|| Mutex::new(Vec::new())),
        };
        fs.note(&root_final);
        Ok((fs, Dir { handle }))
    }

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

    /// Opens `name` (one component) below `parent` without following links, and checks the
    /// handle: not a reparse point, the expected kind, on the root's volume, inside the root.
    fn open(&self, parent: &Dir, name: &str, dir: bool) -> Result<(OwnedHandle, Info), Error> {
        if !plain_component(name) {
            return Err(Error::BadName);
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
        let (access, kind): (FILE_ACCESS_RIGHTS, NTCREATEFILE_CREATE_OPTIONS) = if dir {
            (
                FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_DIRECTORY_FILE,
            )
        } else {
            (
                FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_NON_DIRECTORY_FILE,
            )
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
                kind | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
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
        let (info, volume) = by_handle(&handle)?;
        if info.is_link() || volume != self.volume {
            return Err(Error::Link);
        }
        if info.is_dir() != dir {
            return Err(Error::WrongKind);
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

    /// Opens folder `name` below `parent`.
    pub fn dir(&self, parent: &Dir, name: &str) -> Result<Dir, Error> {
        let (handle, _) = self.open(parent, name, true)?;
        Ok(Dir { handle })
    }

    /// Opens file `name` below `parent`.
    pub fn file(&self, parent: &Dir, name: &str) -> Result<File, Error> {
        let (handle, info) = self.open(parent, name, false)?;
        Ok(File {
            file: std::fs::File::from(handle),
            info,
        })
    }

    /// Opens the file at `/`-separated `rel` below `base`, one component at a time.
    pub fn file_at(&self, base: &Dir, rel: &str) -> Result<File, Error> {
        let mut parts: Vec<&str> = rel.split('/').collect();
        let last = parts.pop().ok_or(Error::BadName)?;
        let mut owned: Option<Dir> = None;
        for part in parts {
            let next = self.dir(owned.as_ref().unwrap_or(base), part)?;
            owned = Some(next);
        }
        self.file(owned.as_ref().unwrap_or(base), last)
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
                    if !each(Entry { name, info }) {
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

    /// Up to `buf.len()` bytes at `offset`.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        std::os::windows::fs::FileExt::seek_read(&self.file, buf, offset)
    }
}

/// Attributes, size, write time and volume serial number of an open handle.
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
    Ok((info, data.dwVolumeSerialNumber))
}

/// Normalized final path of an open handle (`\\?\C:\...` or `\\?\UNC\...`).
fn final_path(handle: &OwnedHandle) -> Result<String, Error> {
    let mut buf = vec![0u16; 1024];
    loop {
        let n = unsafe {
            GetFinalPathNameByHandleW(
                HANDLE(handle.as_raw_handle() as _),
                &mut buf,
                GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0),
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

/// A single name the file system takes as written: not empty, not `.` or `..`, no separator,
/// no `:` (streams), no NUL. Device names mean nothing to a relative `NtCreateFile`.
pub fn plain_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 255 * 3
        && !name.contains(['\\', '/', ':', '\0'])
}
