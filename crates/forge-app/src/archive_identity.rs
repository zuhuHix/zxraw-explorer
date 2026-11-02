//! Which archive file a move out of it saw, so that its removal touches exactly that file,
//! unchanged (see `archive_out`).
//!
//! An [`ArchiveIdentity`] is the file's volume serial and 128-bit file id, its size, its
//! last write time, and a hash of its directory region: a zip's end records (its zip64 end
//! record with its extensible data, the zip64 locator and the end record) and its central
//! directory, a 7z's start header and header, or else the first and last 64 KiB. Size and
//! time alone miss a rewrite that keeps both; the file id misses nothing that replaces the
//! file; the directory hash catches an in-place rewrite of names, counts and checksums. The
//! hash key is random per process (the identity is only ever compared within one run).
//!
//! * Reading the items ([`hold_for_reading`]): the archive is opened once sharing only
//!   reading, the identity is read from that handle, and the handle is kept while the items
//!   are read. Meanwhile no other program can write, rename or delete the file, so every
//!   read through its path sees that same file.
//! * Removing them ([`hold_unchanged`], [`Held::copy_to_private`], [`PrivateCopy::replace`]):
//!   the original is opened sharing only reading, its identity checked on that handle, and
//!   it is copied through that handle to a private temp file next to it. The edit runs on
//!   the copy only. Then the original is opened again (sharing reading and deleting, so it
//!   can be replaced), checked again, and the edited copy is renamed over it through the
//!   copy's own handle. Entries are only ever removed from a verified copy of the original.

use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
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
/// How long a hold waits for a program that has the archive open for writing.
pub const WRITER_WAIT: Duration = Duration::from_secs(10);

const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_DELETE: u32 = 0x4;
/// `DELETE` access, needed to rename a file through its handle.
const DELETE: u32 = 0x0001_0000;
/// `ERROR_SHARING_VIOLATION`: another program has the file open in a way ours conflicts with.
const SHARING_VIOLATION: i32 = 32;

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
    Read::by_ref(file).take(len).read_to_end(&mut buf)?;
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

/// One random key for the process: the same bytes hash the same within a run.
fn hasher() -> impl Hasher {
    static KEY: OnceLock<std::collections::hash_map::RandomState> = OnceLock::new();
    KEY.get_or_init(std::collections::hash_map::RandomState::new)
        .build_hasher()
}

fn directory_hash(file: &mut File, size: u64) -> std::io::Result<u64> {
    let mut h = hasher();
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
    } else if let Some(zip) = zip_directory(file, size)? {
        h.write_u64(zip.end_at);
        h.write(&zip.end);
        if let Some((at, len)) = zip.zip64_end {
            hash_range(&mut h, file, at, len)?;
        }
        hash_range(&mut h, file, zip.directory_at, zip.directory_len)?;
        return Ok(h.finish());
    }
    // Another format: its edges.
    hash_range(&mut h, file, 0, size.min(EDGE))?;
    hash_range(&mut h, file, size.saturating_sub(EDGE), size.min(EDGE))?;
    Ok(h.finish())
}

/// A zip's directory region.
struct ZipDirectory {
    /// Where `end` starts: the zip64 locator if there is one, else the end record.
    end_at: u64,
    /// The locator (if any) and the end record with its comment, to the end of the file.
    end: Vec<u8>,
    /// The whole zip64 end record, extensible data included: offset and length.
    zip64_end: Option<(u64, u64)>,
    directory_at: u64,
    directory_len: u64,
}

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
    let (Some(mut directory_len), Some(mut directory_at)) =
        (u32_at(&tail, pos + 12), u32_at(&tail, pos + 16))
    else {
        return Ok(None);
    };
    let mut from = pos;
    let mut zip64_end = None;
    // A zip64 locator right before the end record: the zip64 end record it points to is
    // part of the directory region whether or not the end record needs its values.
    if pos >= 20
        && tail[pos - 20..].starts_with(b"PK\x06\x07")
        && let Some(record_at) = u64_at(&tail, pos - 20 + 8)
    {
        from = pos - 20;
        let record = read_at(file, record_at, 56)?;
        if record.starts_with(b"PK\x06\x06")
            && let Some(rest) = u64_at(&record, 4)
        {
            let len = rest.saturating_add(12);
            if record_at.saturating_add(len) <= size {
                zip64_end = Some((record_at, len));
            }
            if (directory_len == 0xFFFF_FFFF || directory_at == 0xFFFF_FFFF)
                && let (Some(len), Some(at)) = (u64_at(&record, 40), u64_at(&record, 48))
            {
                directory_len = len;
                directory_at = at;
            }
        }
    }
    if directory_at.saturating_add(directory_len) > size {
        return Ok(None);
    }
    Ok(Some(ZipDirectory {
        end_at: start + from as u64,
        end: tail[from..].to_vec(),
        zip64_end,
        directory_at,
        directory_len,
    }))
}

/// Opens `path` for reading with `share`, waiting up to `wait` for a program writing to it.
fn open_waiting(path: &Path, share: u32, access: u32, wait: Duration) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;

    let until = Instant::now() + wait;
    loop {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).share_mode(share);
        if access != 0 {
            options.access_mode(access);
        }
        match options.open(path) {
            Ok(file) => return Ok(file),
            Err(e) if e.raw_os_error() == Some(SHARING_VIOLATION) && Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) if e.raw_os_error() == Some(SHARING_VIOLATION) => {
                return Err("another program is writing to it".into());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// The archive held while its items are read: nobody can write, rename or delete it.
pub struct ReadHold {
    _file: File,
}

/// Opens `path` sharing only reading (see the module docs) and reads its identity from that
/// handle. Waits up to `wait` for a program writing to it.
pub fn hold_for_reading(path: &Path, wait: Duration) -> Result<(ReadHold, ArchiveIdentity), String> {
    let mut file = open_waiting(path, FILE_SHARE_READ, 0, wait)?;
    let identity = of_file(&mut file)?;
    Ok((ReadHold { _file: file }, identity))
}

/// The original archive, held sharing only reading and checked (see [`hold_unchanged`]).
pub struct Held {
    file: File,
    path: PathBuf,
}

/// Opens `path` sharing only reading, waiting up to `wait` for a writer, and checks it is
/// still `expected` on that handle.
pub fn hold_unchanged(
    path: &Path,
    expected: &ArchiveIdentity,
    wait: Duration,
) -> Result<Held, String> {
    let mut file = open_waiting(path, FILE_SHARE_READ, 0, wait)?;
    if of_file(&mut file)? != *expected {
        return Err("it changed since the items were moved out".into());
    }
    Ok(Held {
        file,
        path: path.to_path_buf(),
    })
}

impl Held {
    /// Copies the held original, through its handle, to a new temp file next to it, with
    /// its "downloaded from the internet" mark; closes the original once copied.
    pub fn copy_to_private(mut self) -> Result<PrivateCopy, String> {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = self.path.parent().ok_or("the archive has no folder")?;
        let copy = PrivateCopy {
            path: dir.join(forge_core::ops::partial_name()),
            placed: false,
        };
        // Nobody else may open it while it is written.
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(&copy.path)
            .map_err(|e| e.to_string())?;
        self.file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        std::io::copy(&mut self.file, &mut out).map_err(|e| e.to_string())?;
        out.sync_all().map_err(|e| e.to_string())?;
        drop(out);
        // The mark is a stream of its own, which the edit then keeps on its result.
        let zone = |p: &Path| PathBuf::from(format!("{}:Zone.Identifier", p.display()));
        if let Ok(mark) = std::fs::read(zone(&self.path)) {
            let _ = std::fs::write(zone(&copy.path), mark);
        }
        drop(self);
        Ok(copy)
    }
}

/// A private copy of the original archive, edited in place of it; removed when dropped
/// unless it replaced the original.
pub struct PrivateCopy {
    path: PathBuf,
    placed: bool,
}

impl Drop for PrivateCopy {
    fn drop(&mut self) {
        if !self.placed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Tests: runs right after the original was checked the second time, before the rename.
    static BEFORE_RENAME: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

/// What [`PrivateCopy::replace`] found after the rename.
#[derive(Debug, PartialEq, Eq)]
pub enum Replaced {
    /// The checked original was replaced by its edited copy.
    Original,
    /// The original was moved away between its last check and the rename: the edited copy
    /// replaced whatever was at its path; the original itself is unchanged where it went.
    MovedAway,
}

impl PrivateCopy {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Puts the edited copy in place of `original`, after checking again that `original` is
    /// still `expected`. The original is held from that check through the rename, sharing
    /// reading and deleting only: it can't be written meanwhile, and the rename (through the
    /// copy's own handle, so the file moved is this copy) may replace it.
    pub fn replace(
        mut self,
        original: &Path,
        expected: &ArchiveIdentity,
    ) -> Result<Replaced, String> {
        use std::os::windows::fs::OpenOptionsExt;

        let mut held = open_waiting(original, FILE_SHARE_READ | FILE_SHARE_DELETE, 0, WRITER_WAIT)?;
        if of_file(&mut held)? != *expected {
            return Err("it changed while the copy was edited".into());
        }
        #[cfg(test)]
        BEFORE_RENAME.with(|hook| {
            if let Some(hook) = hook.borrow().as_ref() {
                hook();
            }
        });
        let edited = std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(0)
            .open(&self.path)
            .map_err(|e| e.to_string())?;
        forge_win::files::rename_open_file_over(&edited, original).map_err(|e| e.to_string())?;
        self.placed = true;
        let replaced = forge_win::files::handle_identity(&held).is_ok_and(|id| id.links == 0);
        Ok(if replaced {
            Replaced::Original
        } else {
            Replaced::MovedAway
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn zip_of(path: &Path, names: &[&str]) {
        let mut zw = zip::ZipWriter::new(File::create(path).unwrap());
        for name in names {
            zw.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(b"same data").unwrap();
        }
        zw.finish().unwrap();
    }

    fn names_in(zip: &Path) -> Vec<String> {
        let mut names: Vec<String> = forge_core::archive::list(zip)
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.name)
            .collect();
        names.sort();
        names
    }

    fn partials_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".forge-partial-"))
            .collect()
    }

    /// Deletes `names` from the archive at `path` as a move's removal does.
    fn remove(path: &Path, expected: &ArchiveIdentity, names: &[&str]) -> Result<Replaced, String> {
        let copy = hold_unchanged(path, expected, Duration::ZERO)?.copy_to_private()?;
        let edit = forge_core::archive::ArchiveEdit {
            add: Vec::new(),
            delete: names.iter().map(|n| n.to_string()).collect(),
        };
        let (cancel, progress) = (forge_jobs::CancelToken::new(), forge_jobs::Progress::default());
        forge_core::archive::update(
            copy.path(),
            &edit,
            &forge_core::archive::CreateOptions::default(),
            &forge_core::ops::JobCtx::new(&cancel, &progress),
        )
        .map_err(|e| e.to_string())?;
        copy.replace(path, expected)
    }

    #[test]
    fn a_rewrite_keeping_size_and_time_is_a_different_archive() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, &["one.txt"]);
        let before = read(&zip).unwrap();
        assert_eq!(read(&zip).unwrap(), before);
        let modified = std::fs::metadata(&zip).unwrap().modified().unwrap();
        // Same length name, written in place, the time put back.
        let other = dir.path().join("b.zip");
        zip_of(&other, &["two.txt"]);
        std::fs::write(&zip, std::fs::read(&other).unwrap()).unwrap();
        File::options()
            .write(true)
            .open(&zip)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let after = read(&zip).unwrap();
        assert_eq!(
            (after.file_id, after.size, after.last_write),
            (before.file_id, before.size, before.last_write)
        );
        assert_ne!(after, before);
        let error = hold_unchanged(&zip, &before, Duration::ZERO).err().unwrap();
        assert!(error.contains("changed"), "{error}");
    }

    /// `bytes` of a zip with a zip64 end record and locator put before its end record,
    /// stating `count` entries.
    fn with_zip64_end(bytes: &[u8], count: u64) -> Vec<u8> {
        let eocd = bytes.windows(4).rposition(|w| w == b"PK\x05\x06").unwrap();
        let end = &bytes[eocd..];
        let cd_len = u64::from(u32::from_le_bytes(end[12..16].try_into().unwrap()));
        let cd_at = u64::from(u32::from_le_bytes(end[16..20].try_into().unwrap()));
        let mut out = bytes[..eocd].to_vec();
        let record_at = out.len() as u64;
        out.extend_from_slice(b"PK\x06\x06");
        out.extend_from_slice(&44u64.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&cd_len.to_le_bytes());
        out.extend_from_slice(&cd_at.to_le_bytes());
        out.extend_from_slice(b"PK\x06\x07");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&record_at.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(end);
        out
    }

    #[test]
    fn a_change_of_the_zip64_end_record_alone_is_a_different_archive() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, &["one.txt"]);
        let plain = std::fs::read(&zip).unwrap();
        std::fs::write(&zip, with_zip64_end(&plain, 1)).unwrap();
        let modified = std::fs::metadata(&zip).unwrap().modified().unwrap();
        let before = read(&zip).unwrap();
        // Only the zip64 entry count differs, written in place with the time put back.
        std::fs::write(&zip, with_zip64_end(&plain, 2)).unwrap();
        File::options()
            .write(true)
            .open(&zip)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let after = read(&zip).unwrap();
        assert_eq!(
            (after.file_id, after.size, after.last_write),
            (before.file_id, before.size, before.last_write)
        );
        assert_ne!(after, before);
    }

    #[test]
    fn a_copy_put_in_its_place_is_a_different_archive() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, &["one.txt"]);
        let before = read(&zip).unwrap();
        let copy = dir.path().join("copy.zip");
        std::fs::copy(&zip, &copy).unwrap();
        std::fs::rename(&zip, dir.path().join("away.zip")).unwrap();
        std::fs::rename(&copy, &zip).unwrap();
        assert!(hold_unchanged(&zip, &before, Duration::ZERO).is_err());
    }

    #[test]
    fn while_its_items_are_read_the_archive_cant_be_written_renamed_or_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, &["one.txt"]);
        let (hold, identity) = hold_for_reading(&zip, Duration::ZERO).unwrap();
        assert_eq!(identity, read(&zip).unwrap(), "read from the held file");
        assert!(File::options().write(true).open(&zip).is_err());
        assert!(std::fs::rename(&zip, dir.path().join("away.zip")).is_err());
        assert!(std::fs::remove_file(&zip).is_err());
        // Reading it, as the extraction does, works.
        assert_eq!(names_in(&zip), ["one.txt"]);
        drop(hold);
        std::fs::rename(&zip, dir.path().join("away.zip")).unwrap();
    }

    #[test]
    fn a_writer_keeps_the_hold_waiting_then_refused() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, &["one.txt"]);
        let expected = read(&zip).unwrap();
        let writer = File::options().write(true).open(&zip).unwrap();
        let error = hold_unchanged(&zip, &expected, Duration::from_millis(300))
            .err()
            .unwrap();
        assert!(error.contains("writing"), "{error}");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(writer);
        });
        let held = hold_unchanged(&zip, &expected, Duration::from_secs(5)).unwrap();
        releaser.join().unwrap();
        assert!(File::options().write(true).open(&zip).is_err());
        assert!(std::fs::rename(&zip, dir.path().join("away.zip")).is_err());
        drop(held);
    }

    #[test]
    fn the_removal_edits_a_verified_copy_and_puts_it_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, &["one.txt", "two.txt"]);
        let expected = read(&zip).unwrap();
        assert_eq!(remove(&zip, &expected, &["one.txt"]), Ok(Replaced::Original));
        assert_eq!(names_in(&zip), ["two.txt"]);
        assert!(partials_in(dir.path()).is_empty());
    }

    #[test]
    fn a_failed_edit_leaves_the_original_byte_identical_and_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, &["one.txt"]);
        let bytes = std::fs::read(&zip).unwrap();
        let expected = read(&zip).unwrap();
        // The copy is made, then the edit fails (cancelled) and the copy is dropped.
        let copy = hold_unchanged(&zip, &expected, Duration::ZERO)
            .unwrap()
            .copy_to_private()
            .unwrap();
        let cancel = forge_jobs::CancelToken::new();
        cancel.cancel();
        let progress = forge_jobs::Progress::default();
        let result = forge_core::archive::update(
            copy.path(),
            &forge_core::archive::ArchiveEdit {
                add: Vec::new(),
                delete: vec!["one.txt".into()],
            },
            &forge_core::archive::CreateOptions::default(),
            &forge_core::ops::JobCtx::new(&cancel, &progress),
        );
        assert!(result.is_err());
        drop(copy);
        assert_eq!(std::fs::read(&zip).unwrap(), bytes);
        assert!(partials_in(dir.path()).is_empty());
    }

    #[test]
    fn a_substitute_put_in_place_before_the_last_check_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, &["one.txt", "two.txt"]);
        let expected = read(&zip).unwrap();
        let copy = hold_unchanged(&zip, &expected, Duration::ZERO)
            .unwrap()
            .copy_to_private()
            .unwrap();
        // After the copy, another program swaps a file with the same entries into place.
        std::fs::rename(&zip, dir.path().join("away.zip")).unwrap();
        zip_of(&zip, &["one.txt", "two.txt"]);
        let substitute = std::fs::read(&zip).unwrap();
        let error = copy.replace(&zip, &expected).unwrap_err();
        assert!(error.contains("changed"), "{error}");
        assert_eq!(std::fs::read(&zip).unwrap(), substitute, "nothing removed from it");
        assert!(partials_in(dir.path()).is_empty());
    }

    #[test]
    fn a_substitute_put_in_place_after_the_last_check_is_replaced_by_the_edited_original() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, &["one.txt", "two.txt"]);
        let original = std::fs::read(&zip).unwrap();
        let expected = read(&zip).unwrap();
        let away = dir.path().join("away.zip");
        let (zip_hook, away_hook) = (zip.clone(), away.clone());
        BEFORE_RENAME.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                // Deleting is shared from the last check on: the original can be moved
                // away and a substitute put in its place.
                std::fs::rename(&zip_hook, &away_hook).unwrap();
                zip_of(&zip_hook, &["one.txt", "two.txt", "three.txt"]);
            }));
        });
        let result = remove(&zip, &expected, &["one.txt"]);
        BEFORE_RENAME.with(|hook| hook.borrow_mut().take());
        // The edited copy of the checked original replaced the substitute; the original is
        // byte-identical where it went; nothing was removed from the substitute.
        assert_eq!(result, Ok(Replaced::MovedAway));
        assert_eq!(names_in(&zip), ["two.txt"]);
        assert_eq!(std::fs::read(&away).unwrap(), original);
        assert!(partials_in(dir.path()).is_empty());
    }
}
