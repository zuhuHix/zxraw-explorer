//! Which archive file a move out of it saw, so that its removal step touches exactly that
//! file, unchanged (see `archive_out`).
//!
//! An [`ArchiveIdentity`] is the file's volume serial and 128-bit file id, its size, its
//! last write time, and a hash of its directory region: a zip's end records and central
//! directory, a 7z's start header and header, or else the first and last 64 KiB. Size and
//! time alone miss a rewrite that keeps both; the file id misses nothing that replaces the
//! file, and the directory hash catches an in-place rewrite of the names and checksums.
//!
//! [`hold_unchanged`] opens the archive once, sharing only reading and deleting: while the
//! handle is open no other program can open it for writing (the open fails, or waits here
//! for one already writing). The identity is checked on that handle. Deleting is shared so
//! the edit can still rename its rebuilt archive over the file; [`Held::still_at`] then
//! checks, right before the edit opens the path, that the path is still this file, and
//! [`Held::replaced`] after it that the file the edit replaced was this one.

use std::fs::File;
use std::hash::Hasher;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

/// See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveIdentity {
    volume: u64,
    file_id: u128,
    size: u64,
    last_write: u64,
    directory: u64,
}

/// The largest directory region hashed whole; beyond it, its first and last 64 KiB.
const MAX_DIRECTORY: u64 = 256 << 20;
const EDGE: u64 = 64 << 10;

/// The identity of the archive at `path` now (reads its directory region: call it off the
/// UI thread).
pub fn read(path: &Path) -> Result<ArchiveIdentity, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    of_file(&mut file)
}

fn of_file(file: &mut File) -> Result<ArchiveIdentity, String> {
    let id = forge_win::files::handle_identity(file).map_err(|e| e.to_string())?;
    let directory = directory_hash(file, id.size).map_err(|e| e.to_string())?;
    Ok(ArchiveIdentity {
        volume: id.volume,
        file_id: id.file_id,
        size: id.size,
        last_write: id.last_write,
        directory,
    })
}

fn read_at(file: &mut File, at: u64, len: u64) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(at))?;
    let mut buf = Vec::new();
    file.take(len).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Hashes `len` bytes at `at` (whole up to [`MAX_DIRECTORY`], else their two edges).
fn hash_range(h: &mut impl Hasher, file: &mut File, at: u64, len: u64) -> std::io::Result<()> {
    h.write_u64(at);
    h.write_u64(len);
    if len <= MAX_DIRECTORY {
        h.write(&read_at(file, at, len)?);
    } else {
        h.write(&read_at(file, at, EDGE)?);
        h.write(&read_at(file, at + len - EDGE, EDGE)?);
    }
    Ok(())
}

fn u32_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?)))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn directory_hash(file: &mut File, size: u64) -> std::io::Result<u64> {
    // A fixed-key hasher: the same bytes always give the same value.
    #[allow(deprecated)]
    let mut h = std::hash::SipHasher::new();
    let head = read_at(file, 0, 32)?;
    if head.starts_with(b"7z\xBC\xAF\x27\x1C") && head.len() == 32 {
        // Start header, then the header it points to.
        h.write(&head);
        if let (Some(offset), Some(len)) = (u64_at(&head, 12), u64_at(&head, 20)) {
            let at = 32u64.saturating_add(offset);
            if at.saturating_add(len) <= size {
                hash_range(&mut h, file, at, len)?;
                return Ok(h.finish());
            }
        }
    } else if let Some((eocd_at, tail, cd_at, cd_len)) = zip_directory(file, size)? {
        h.write_u64(eocd_at);
        h.write(&tail);
        hash_range(&mut h, file, cd_at, cd_len)?;
        return Ok(h.finish());
    }
    // Another format: its edges.
    hash_range(&mut h, file, 0, size.min(EDGE))?;
    hash_range(&mut h, file, size.saturating_sub(EDGE), size.min(EDGE))?;
    Ok(h.finish())
}

/// Where a zip's end records start, their bytes to the end of the file, and the central
/// directory's offset and length.
type ZipDirectory = (u64, Vec<u8>, u64, u64);

/// A zip's end of central directory record (with any zip64 records before it).
fn zip_directory(file: &mut File, size: u64) -> std::io::Result<Option<ZipDirectory>> {
    // The record is 22 bytes plus a comment of at most 65535, after a 20-byte zip64 locator.
    let span = size.min(22 + 65_535 + 20);
    let start = size - span;
    let tail = read_at(file, start, span)?;
    let Some(pos) = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| tail[i..].starts_with(b"PK\x05\x06"))
    else {
        return Ok(None);
    };
    let (Some(mut cd_len), Some(mut cd_at)) = (u32_at(&tail, pos + 12), u32_at(&tail, pos + 16))
    else {
        return Ok(None);
    };
    let mut from = pos;
    if (cd_len == 0xFFFF_FFFF || cd_at == 0xFFFF_FFFF)
        && pos >= 20
        && tail[pos - 20..].starts_with(b"PK\x06\x07")
        && let Some(zip64_at) = u64_at(&tail, pos - 20 + 8)
    {
        let record = read_at(file, zip64_at, 56)?;
        if record.starts_with(b"PK\x06\x06")
            && let (Some(len), Some(at)) = (u64_at(&record, 40), u64_at(&record, 48))
        {
            cd_len = len;
            cd_at = at;
            from = pos - 20;
        }
    }
    if cd_at.saturating_add(cd_len) > size {
        return Ok(None);
    }
    Ok(Some((start + from as u64, tail[from..].to_vec(), cd_at, cd_len)))
}

/// The archive, held open against writers while it is edited (see the module docs).
pub struct Held {
    file: File,
    identity: ArchiveIdentity,
}

/// `ERROR_SHARING_VIOLATION`: another program has the file open in a way ours conflicts with.
const SHARING_VIOLATION: i32 = 32;

/// Opens `path` denying writers and checks it is still `expected`. Waits up to `wait` for a
/// program that has it open for writing.
pub fn hold_unchanged(
    path: &Path,
    expected: &ArchiveIdentity,
    wait: Duration,
) -> Result<Held, String> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_DELETE: u32 = 0x4;
    let until = Instant::now() + wait;
    let mut file = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(path)
        {
            Ok(file) => break file,
            Err(e) if e.raw_os_error() == Some(SHARING_VIOLATION) && Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) if e.raw_os_error() == Some(SHARING_VIOLATION) => {
                return Err("another program is writing to it".into());
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let identity = of_file(&mut file)?;
    if identity != *expected {
        return Err("it changed since the items were moved out".into());
    }
    Ok(Held { file, identity })
}

impl Held {
    /// Whether `path` still names the held file (nothing was renamed over it or it away).
    pub fn still_at(&self, path: &Path) -> Result<(), String> {
        match forge_win::files::path_identity(path) {
            Ok(id) if id.volume == self.identity.volume && id.file_id == self.identity.file_id => {
                Ok(())
            }
            Ok(_) => Err("another file was put in its place".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// After the edit: whether the held file is the one it replaced (it has no name left).
    pub fn replaced(&self) -> bool {
        forge_win::files::handle_identity(&self.file).is_ok_and(|id| id.links == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip_of(path: &Path, name: &str) {
        use std::io::Write;
        let mut zw = zip::ZipWriter::new(File::create(path).unwrap());
        zw.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(b"same data").unwrap();
        zw.finish().unwrap();
    }

    #[test]
    fn a_rewrite_keeping_size_and_time_is_a_different_archive() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, "one.txt");
        let before = read(&zip).unwrap();
        assert_eq!(read(&zip).unwrap(), before);
        let modified = std::fs::metadata(&zip).unwrap().modified().unwrap();
        // Same length name, written in place, the time put back.
        let other = dir.path().join("b.zip");
        zip_of(&other, "two.txt");
        std::fs::write(&zip, std::fs::read(&other).unwrap()).unwrap();
        File::options()
            .write(true)
            .open(&zip)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(std::fs::metadata(&zip).unwrap().len(), before.size);
        let after = read(&zip).unwrap();
        assert_eq!((after.file_id, after.size, after.last_write), (before.file_id, before.size, before.last_write));
        assert_ne!(after, before);
        let error = hold_unchanged(&zip, &before, Duration::ZERO).err().unwrap();
        assert!(error.contains("changed"), "{error}");
    }

    #[test]
    fn a_copy_put_in_its_place_is_a_different_archive() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, "one.txt");
        let before = read(&zip).unwrap();
        let copy = dir.path().join("copy.zip");
        std::fs::copy(&zip, &copy).unwrap();
        std::fs::rename(&zip, dir.path().join("away.zip")).unwrap();
        std::fs::rename(&copy, &zip).unwrap();
        assert!(hold_unchanged(&zip, &before, Duration::ZERO).is_err());
    }

    #[test]
    fn a_writer_keeps_the_hold_waiting_and_the_hold_keeps_writers_out() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, "one.txt");
        let expected = read(&zip).unwrap();
        let writer = File::options().write(true).open(&zip).unwrap();
        let error = hold_unchanged(&zip, &expected, Duration::from_millis(300))
            .err()
            .unwrap();
        assert!(error.contains("writing"), "{error}");
        // The writer leaves while the hold waits: it goes ahead.
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(writer);
        });
        let held = hold_unchanged(&zip, &expected, Duration::from_secs(5)).unwrap();
        releaser.join().unwrap();
        // While held, nobody can open it for writing, but it can still be read, and the
        // path still names it.
        assert!(File::options().write(true).open(&zip).is_err());
        assert!(File::open(&zip).is_ok());
        held.still_at(&zip).unwrap();
        assert!(!held.replaced());
        // A rename over it (what an edit does at its end) still works, and is seen.
        let rebuilt = dir.path().join("rebuilt.zip");
        zip_of(&rebuilt, "one.txt");
        std::fs::rename(&rebuilt, &zip).unwrap();
        assert!(held.replaced());
        assert!(held.still_at(&zip).is_err());
    }
}
