//! A stamp that changes when an archive file is replaced by another file.
//!
//! Size and modified time alone are not enough: copy tools keep the modified time, so a
//! different archive of the same size would look unchanged. The stamp adds the file's
//! identity (on Windows, the volume serial and file id, which a file written anew gets
//! anew) and hashes both ends of the file, where the zip central directory, the 7z end
//! header and the tar end are.
//!
//! The stamp is deterministic (xxh3 with a fixed seed, no per-process random state), so it
//! can be compared across runs. Its I/O is bounded: one open, and at most 4 KiB from the
//! start and 64 KiB from the end.
//!
//! A stamp is *strong* only when the file id was read. Where the file system or share does
//! not report one, a same-size, same-time file that differs only between the two hashed ends
//! has the same stamp, so a weak stamp is never taken to match a cached copy: callers read
//! the archive again instead.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::UNIX_EPOCH;

use xxhash_rust::xxh3::Xxh3;

use crate::error::{CoreError, Result};

/// Bytes hashed from the start of the file.
const HEAD: u64 = 4 << 10;
/// Bytes hashed from the end of the file.
const TAIL: u64 = 64 << 10;

/// The stamp of an archive file (see [`archive_stamp`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveStamp {
    /// The hash of the length, modified time, identity (when `strong`) and both ends.
    pub value: u64,
    /// Whether the file id was read: a file written anew then gets a different value. When
    /// not, equal values only say that length, time and both ends are equal.
    pub strong: bool,
}

#[cfg(test)]
thread_local! {
    /// Tests: when set, the file id counts as unavailable (the weak path), on this thread.
    pub(crate) static HIDE_FILE_ID: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The stamp of the archive file at `path` now: its length, modified time, identity and
/// the bytes at both ends. Equal strong stamps mean the same file as far as this can tell.
pub fn archive_stamp(path: &Path) -> Result<ArchiveStamp> {
    let io = |e| CoreError::io(path, e);
    let mut file = File::open(path).map_err(io)?;
    // The metadata of the open handle, so the length, time and identity are of this file.
    let md = file.metadata().map_err(io)?;
    let len = md.len();
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let mut h = Xxh3::new();
    h.update(&len.to_le_bytes());
    h.update(&mtime.to_le_bytes());
    let strong = identity_into(&file, &mut h);
    let head = len.min(HEAD);
    hash_range(&mut h, &mut file, 0, head).map_err(io)?;
    // The tail starts after the head, so a small file's bytes are hashed once.
    let tail_at = len.saturating_sub(TAIL).max(head);
    hash_range(&mut h, &mut file, tail_at, len - tail_at).map_err(io)?;
    Ok(ArchiveStamp {
        value: h.digest(),
        strong,
    })
}

/// Hashes the volume serial and the 128-bit file id (`FILE_ID_INFO`) of the open file: a file
/// written anew, as a copy over an archive does, gets a new id. Nothing is hashed when the id
/// can't be read. Returns whether it was hashed.
#[cfg(windows)]
fn identity_into(file: &File, h: &mut Xxh3) -> bool {
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    if hide_file_id() {
        return false;
    }
    let mut id = FILE_ID_INFO::default();
    // SAFETY: `id` is a FILE_ID_INFO of the size passed; the handle is open.
    let read = unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            (&mut id as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if read.is_ok() {
        h.update(&id.VolumeSerialNumber.to_le_bytes());
        h.update(&id.FileId.Identifier);
    }
    read.is_ok()
}

/// No file id on this platform: the stamp is weak.
#[cfg(not(windows))]
fn identity_into(_: &File, _: &mut Xxh3) -> bool {
    false
}

/// Tests can make the file id unavailable (see `HIDE_FILE_ID`); always false otherwise.
#[cfg(all(windows, test))]
fn hide_file_id() -> bool {
    HIDE_FILE_ID.with(std::cell::Cell::get)
}

#[cfg(all(windows, not(test)))]
fn hide_file_id() -> bool {
    false
}

/// Hashes `len` bytes of `file` from `at`, in pieces (no buffer larger than [`TAIL`]).
fn hash_range(h: &mut Xxh3, file: &mut File, at: u64, len: u64) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(at))?;
    let mut buf = vec![0u8; len.min(TAIL) as usize];
    let mut left = len;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        file.read_exact(&mut buf[..n])?;
        h.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_bytes_same_stamp_and_empty_files_work() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.bin");
        let empty = dir.path().join("e.bin");
        std::fs::write(&a, b"hello archive").unwrap();
        std::fs::write(&b, b"hello archive").unwrap();
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(archive_stamp(&a).unwrap(), archive_stamp(&a).unwrap());
        assert_ne!(archive_stamp(&a).unwrap(), archive_stamp(&b).unwrap());
        assert_ne!(archive_stamp(&a).unwrap(), archive_stamp(&empty).unwrap());
        assert!(archive_stamp(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn bytes_far_from_both_ends_are_not_read_but_both_ends_are() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.bin");
        let mut data = vec![7u8; 1 << 20];
        std::fs::write(&p, &data).unwrap();
        let base = archive_stamp(&p).unwrap();
        // The middle is outside the hashed ranges (same length, same mtime kept below).
        data[512 << 10] = 1;
        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::fs::write(&p, &data).unwrap();
        File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(archive_stamp(&p).unwrap(), base);
        // The last byte is inside the tail.
        let last = data.len() - 1;
        data[last] = 1;
        std::fs::write(&p, &data).unwrap();
        File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_ne!(archive_stamp(&p).unwrap(), base);
    }

    #[test]
    fn a_stamp_without_a_file_id_is_weak_and_differs_from_the_strong_one() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.bin");
        std::fs::write(&p, b"weak or strong").unwrap();
        let strong = archive_stamp(&p).unwrap();
        HIDE_FILE_ID.with(|h| h.set(true));
        let weak = archive_stamp(&p);
        HIDE_FILE_ID.with(|h| h.set(false));
        let weak = weak.unwrap();
        assert!(!weak.strong);
        #[cfg(windows)]
        assert!(strong.strong, "NTFS and local volumes report a file id");
        #[cfg(windows)]
        assert_ne!(weak, strong, "the weak stamp hashes no file id");
    }
}
