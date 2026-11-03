//! Self-extracting archives: a program stub ("MZ", a PE image) with an archive after it.
//!
//! [`detect`] looks at the PE header only as far as it must: the end of the image (the
//! highest raw end of its sections) is where the payload normally starts. From there it
//! searches a bounded window for a 7z signature and checks the zip end record at the end of the
//! file. Every candidate is validated before it counts: a 7z start header by its CRC and a
//! next header inside the file; a zip end record by a central directory inside the file.
//!
//! When both validate, the one after the image end wins (the payload, not the stub); between
//! two of those, the earlier one (the outer archive).
//!
//! 7z payloads are detected but not opened yet: the 7z backend reads files by path from offset
//! 0, and a start offset has to be threaded through it first (see [`SfxKind::SevenZip`]).
//! Zip payloads open through the zip backend, which finds its records wherever they are.
//!
//! Opening a `.exe` as an archive is explicit: [`mark_open_as_archive`] lets the path split
//! into an archive and inner path (see `split_archive_path`), while `is_archive_name` stays
//! false, so double-clicking the program still runs it.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;

use parking_lot::Mutex;

use super::{zip_backend, ArchiveBackend};
use crate::error::{CoreError, Result};
use crate::paths;

/// Shown when a 7z payload is found: it can't be opened until the 7z backend takes an offset.
pub const SEVEN_ZIP_SFX_UNSUPPORTED: &str = "a 7z self-extracting archive can't be opened yet \
     (its 7z data sits after the program stub); extract it with 7-Zip instead";

/// Bytes searched for a 7z signature after the end of a valid PE image.
const SEARCH_WINDOW: u64 = 16 << 20;
/// Bytes searched from the start of a file whose PE header doesn't parse.
const MALFORMED_WINDOW: u64 = 1 << 20;
/// Bytes read per step of the signature search (with overlap for a signature across two).
const CHUNK: u64 = 1 << 20;
/// PE sections the loader accepts at most.
const MAX_SECTIONS: u16 = 96;
/// Signature candidates validated at most (a stub full of fake signatures stays bounded).
const MAX_CANDIDATES: usize = 64;
/// Entries of the "opened as archive" set kept (the most recent ones).
const MAX_OPENED: usize = 32;

const SEVEN_SIG: [u8; 6] = [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C];
const ZIP_EOCD: &[u8; 4] = b"PK\x05\x06";
const ZIP_CD: &[u8; 4] = b"PK\x01\x02";

/// What a self-extracting file carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SfxKind {
    /// A zip whose first record is at `start`; the stub is before it.
    Zip { start: u64 },
    /// A 7z archive whose signature is at `start`. Not openable yet (see the module docs).
    SevenZip { start: u64 },
}

/// The archive a file carries after its program stub, if it has one. `Ok(None)` for a file
/// that isn't an SFX: not "MZ", no valid PE header and no valid payload, or a truncated file.
/// Reads are bounded (the PE header, the zip tail and [`SEARCH_WINDOW`] at most).
pub fn detect(path: &Path) -> Result<Option<SfxKind>> {
    let mut file = File::open(path).map_err(|e| CoreError::io(path, e))?;
    detect_in(&mut file).map_err(|e| CoreError::io(path, e))
}

/// The zip backend for a self-extracting zip; `None` when the file isn't one. A 7z payload is
/// an error ([`SEVEN_ZIP_SFX_UNSUPPORTED`]), not a backend.
pub fn sfx_backend(path: &Path) -> Result<Option<&'static dyn ArchiveBackend>> {
    match detect(path)? {
        None => Ok(None),
        Some(SfxKind::Zip { .. }) => Ok(Some(&zip_backend::ZIP)),
        Some(SfxKind::SevenZip { .. }) => Err(CoreError::Unsupported(format!(
            "{}: {SEVEN_ZIP_SFX_UNSUPPORTED}",
            paths::display_path(path)
        ))),
    }
}

/// Refuse an edit of a self-extracting 7z (the zip case is refused by `check_no_prefix`).
pub fn refuse_edit_if_sfx(path: &Path) -> Result<()> {
    if let Some(SfxKind::SevenZip { .. }) = detect(path)? {
        return Err(CoreError::Unsupported(zip_backend::SFX_NOT_EDITABLE.into()));
    }
    Ok(())
}

fn detect_in(file: &mut File) -> io::Result<Option<SfxKind>> {
    let len = file.seek(SeekFrom::End(0))?;
    if len < 64 {
        return Ok(None);
    }
    match bytes_at(file, len, 0, 2)? {
        Some(mz) if mz == b"MZ" => {}
        _ => return Ok(None),
    }
    let image_end = pe_image_end(file, len)?;
    let (from, to) = match image_end {
        Some(end) => (end, end.saturating_add(SEARCH_WINDOW)),
        None => (0, MALFORMED_WINDOW),
    };
    // (start, kind) of every valid payload.
    let mut found: Vec<(u64, SfxKind)> = Vec::new();
    if let Some(start) = zip_start(file, len)? {
        found.push((start, SfxKind::Zip { start }));
    }
    for start in sevenz_starts(file, len, from, to)? {
        found.push((start, SfxKind::SevenZip { start }));
    }
    let end = image_end.unwrap_or(0);
    // After the image first (the payload, not the stub), then the outermost archive.
    Ok(found
        .into_iter()
        .min_by_key(|(start, _)| (*start < end, *start))
        .map(|(_, kind)| kind))
}

/// `len` is the file length; a read past it is `Ok(None)`, never an error.
fn bytes_at(file: &mut File, len: u64, pos: u64, n: usize) -> io::Result<Option<Vec<u8>>> {
    match pos.checked_add(n as u64) {
        Some(end) if end <= len => {}
        _ => return Ok(None),
    }
    file.seek(SeekFrom::Start(pos))?;
    let mut buf = vec![0u8; n];
    file.read_exact(&mut buf)?;
    Ok(Some(buf))
}

fn u16_le(b: &[u8]) -> u64 {
    u16::from_le_bytes([b[0], b[1]]) as u64
}

fn u32_le(b: &[u8]) -> u64 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64
}

fn u64_le(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

/// The end of a PE image: the highest raw end of its sections. `None` when the PE header
/// doesn't parse or its sections reach past the file.
fn pe_image_end(file: &mut File, len: u64) -> io::Result<Option<u64>> {
    let Some(lfanew) = bytes_at(file, len, 0x3C, 4)? else {
        return Ok(None);
    };
    let lfanew = u32_le(&lfanew);
    let Some(pe) = bytes_at(file, len, lfanew, 24)? else {
        return Ok(None);
    };
    if &pe[..4] != b"PE\0\0" {
        return Ok(None);
    }
    let sections = u16_le(&pe[6..8]);
    let optional = u16_le(&pe[20..22]);
    if sections == 0 || sections > MAX_SECTIONS as u64 {
        return Ok(None);
    }
    let table = lfanew + 24 + optional;
    let Some(table) = bytes_at(file, len, table, sections as usize * 40)? else {
        return Ok(None);
    };
    let mut end = 0u64;
    for s in table.chunks_exact(40) {
        let raw_size = u32_le(&s[16..20]);
        let raw_ptr = u32_le(&s[20..24]);
        end = end.max(raw_ptr + raw_size);
    }
    if end == 0 || end > len {
        return Ok(None);
    }
    Ok(Some(end))
}

/// The start of the zip whose end record is in the file's last 22 + 65,535 bytes, if one
/// validates: its central directory lies inside the file, with its header where it should be.
/// Zip64 records are not looked for here.
fn zip_start(file: &mut File, len: u64) -> io::Result<Option<u64>> {
    let tail = len.min(22 + 65_535);
    let base = len - tail;
    let Some(buf) = bytes_at(file, len, base, tail as usize)? else {
        return Ok(None);
    };
    if buf.len() < 22 {
        return Ok(None);
    }
    let mut tried = 0;
    for i in (0..=buf.len() - 22).rev() {
        if &buf[i..i + 4] != ZIP_EOCD {
            continue;
        }
        tried += 1;
        if tried > MAX_CANDIDATES {
            break;
        }
        let at = base + i as u64;
        let comment = u16_le(&buf[i + 20..i + 22]);
        if at + 22 + comment > len {
            continue;
        }
        let cd_size = u32_le(&buf[i + 12..i + 16]);
        let cd_offset = u32_le(&buf[i + 16..i + 20]);
        if cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF {
            continue;
        }
        let Some(cd_start) = at.checked_sub(cd_size) else {
            continue;
        };
        let Some(start) = cd_start.checked_sub(cd_offset) else {
            continue;
        };
        if cd_size == 0 {
            // An empty archive: its end record is the whole archive.
            return Ok(Some(start));
        }
        if matches!(bytes_at(file, len, cd_start, 4)?, Some(sig) if sig == ZIP_CD) {
            return Ok(Some(start));
        }
    }
    Ok(None)
}

/// Every position in `[from, to)` (clamped to the file) where a 7z start header validates.
fn sevenz_starts(file: &mut File, len: u64, from: u64, to: u64) -> io::Result<Vec<u64>> {
    let to = to.min(len);
    let mut out = Vec::new();
    let mut pos = from;
    while pos < to && out.len() < MAX_CANDIDATES {
        let n = CHUNK.min(to - pos);
        let Some(buf) = bytes_at(file, len, pos, n as usize)? else {
            break;
        };
        for (i, w) in buf.windows(SEVEN_SIG.len()).enumerate() {
            if w == SEVEN_SIG && valid_start_header(file, len, pos + i as u64)? {
                out.push(pos + i as u64);
                if out.len() >= MAX_CANDIDATES {
                    break;
                }
            }
        }
        if pos + n >= to {
            break;
        }
        // The next chunk starts so a signature cut by this one's end is seen whole.
        pos += n - (SEVEN_SIG.len() as u64 - 1);
    }
    Ok(out)
}

/// Whether a 7z start header at `at` is genuine: its CRC matches, its major version is 0 and
/// its next header lies inside the file and starts like a header (`0x01`) or an encoded one
/// (`0x17`). An empty archive has no next header.
fn valid_start_header(file: &mut File, len: u64, at: u64) -> io::Result<bool> {
    let Some(h) = bytes_at(file, len, at, 32)? else {
        return Ok(false);
    };
    if h[6] != 0 {
        return Ok(false);
    }
    if u32_le(&h[8..12]) != u64::from(crc32fast::hash(&h[12..32])) {
        return Ok(false);
    }
    let offset = u64_le(&h[12..20]);
    let size = u64_le(&h[20..28]);
    if size == 0 {
        return Ok(offset == 0);
    }
    let Some(header) = at.checked_add(32).and_then(|p| p.checked_add(offset)) else {
        return Ok(false);
    };
    if header.checked_add(size).is_none_or(|end| end > len) {
        return Ok(false);
    }
    Ok(matches!(bytes_at(file, len, header, 1)?, Some(b) if b[0] == 0x01 || b[0] == 0x17))
}

// ---------------------------------------------------------------------------
// Files opened as archives
// ---------------------------------------------------------------------------

/// Paths the user chose to open as archives (lowercase, `\` separators), most recent last.
fn opened() -> &'static Mutex<Vec<String>> {
    static OPENED: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    OPENED.get_or_init(Default::default)
}

fn key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}

/// Let `path` (a file that isn't an archive by name, such as a `.exe`) split into an archive
/// and an inner path from now on. Nothing is checked here: a file that isn't an archive fails
/// when its listing is read.
pub fn mark_open_as_archive(path: &Path) {
    let mut set = opened().lock();
    let k = key(path);
    set.retain(|p| *p != k);
    set.push(k);
    let excess = set.len().saturating_sub(MAX_OPENED);
    set.drain(..excess);
}

/// Whether `path` was marked by [`mark_open_as_archive`].
pub fn is_open_as_archive(path: &Path) -> bool {
    let k = key(path);
    opened().lock().iter().any(|p| *p == k)
}
