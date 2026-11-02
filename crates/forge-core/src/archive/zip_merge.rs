//! Rebuild a ZIP by copying entry records byte for byte.
//!
//! `zip`'s `raw_copy_file` re-creates each entry's headers from parsed options and drops
//! general-purpose flags on the way (bit 0, *encrypted*, and bit 3, *data descriptor*), so a
//! ZipCrypto entry copied with it is no longer readable. Here an entry's local header, data
//! and data descriptor are copied as they are, and its central-directory record too, with only
//! the local-header offset changed (in the zip64 field when there is or must be one). Nothing
//! is decoded or decrypted: kept entries keep their bytes, metadata, method and password.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use zip::ZipArchive;

use crate::error::{CoreError, Result};
use crate::ops::JobCtx;

const CENTRAL_SIG: &[u8; 4] = b"PK\x01\x02";
const LOCAL_SIG: &[u8; 4] = b"PK\x03\x04";
const ZIP64_EXTRA: u16 = 0x0001;

/// Why a split archive is not edited (copying its records would make a broken one-disk zip).
pub(crate) const SPLIT_ARCHIVE: &str =
    "Split (multi-part) zip archives can't be edited; extract them instead";

/// One entry as stored: where its local record lies in the archive, and its central record.
#[derive(Debug, Clone)]
pub(crate) struct RawRecord {
    /// Local header, data and data descriptor: `start..end` of the archive file.
    pub start: u64,
    pub end: u64,
    /// The central-directory record (fixed part, name, extra fields, comment).
    pub central: Vec<u8>,
}

fn bad(archive: &Path, why: impl std::fmt::Display) -> CoreError {
    CoreError::Invalid(format!("{}: {why}", archive.display()))
}

fn io_err(archive: &Path) -> impl Fn(io::Error) -> CoreError + '_ {
    move |e| CoreError::io(archive, e)
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The records of every entry of `za` (read from `file`, the same archive), in
/// central-directory order. A local record ends where the next one starts (or the central
/// directory does); an entry whose data would run past that overlaps another and is refused.
pub(crate) fn raw_records(
    archive: &Path,
    file: &mut File,
    za: &mut ZipArchive<BufReader<File>>,
) -> Result<Vec<RawRecord>> {
    let io = io_err(archive);
    check_single_disk(archive, file)?;
    let mut found = Vec::with_capacity(za.len());
    for i in 0..za.len() {
        let f = za
            .by_index_raw(i)
            .map_err(|e| bad(archive, format!("entry {i}: {e}")))?;
        let data_end = f
            .data_start()
            .ok_or_else(|| bad(archive, "entry data not located"))?
            .saturating_add(f.compressed_size());
        found.push((f.header_start(), f.central_header_start(), data_end));
    }
    // Where the central directory starts: right after the last local record.
    let mut central_start = za.central_directory_start();
    if let Some(&(_, first_central, _)) = found.iter().min_by_key(|r| r.1) {
        central_start = central_start.min(first_central);
    }
    let mut starts: Vec<u64> = found.iter().map(|r| r.0).collect();
    starts.sort_unstable();
    starts.dedup();
    let overlap = || CoreError::UnsafePath(format!("{}: entries overlap", archive.display()));
    if starts.len() != found.len() {
        return Err(overlap()); // two entries share one local header
    }
    let mut records = Vec::with_capacity(found.len());
    for (start, central_at, data_end) in found {
        let next = starts.partition_point(|&s| s <= start);
        let end = starts.get(next).copied().unwrap_or(central_start);
        if data_end > end {
            return Err(overlap());
        }
        file.seek(SeekFrom::Start(start)).map_err(&io)?;
        let mut sig = [0u8; 4];
        file.read_exact(&mut sig).map_err(&io)?;
        if &sig != LOCAL_SIG {
            return Err(bad(
                archive,
                "local header not found where the directory says",
            ));
        }
        file.seek(SeekFrom::Start(central_at)).map_err(&io)?;
        let mut central = vec![0u8; 46];
        file.read_exact(&mut central).map_err(&io)?;
        if &central[..4] != CENTRAL_SIG {
            return Err(bad(archive, "central record not found"));
        }
        let rest = u16_at(&central, 28) as usize
            + u16_at(&central, 30) as usize
            + u16_at(&central, 32) as usize;
        central.resize(46 + rest, 0);
        file.read_exact(&mut central[46..]).map_err(&io)?;
        if disk_start(&central) != Some(0) {
            return Err(CoreError::Unsupported(SPLIT_ARCHIVE.into()));
        }
        records.push(RawRecord {
            start,
            end,
            central,
        });
    }
    Ok(records)
}

/// Refuse archives that span several disks (split or spanned zips): the end-of-directory
/// record's disk numbers, the zip64 locator's and zip64 record's, must all say one disk.
pub(crate) fn check_single_disk(archive: &Path, file: &mut File) -> Result<()> {
    let io = io_err(archive);
    let split = || CoreError::Unsupported(SPLIT_ARCHIVE.into());
    let len = file.seek(SeekFrom::End(0)).map_err(&io)?;
    // The end record (22 bytes plus a comment of up to 65,535) and the zip64 locator before it.
    let tail_len = len.min(22 + 0xFFFF + 20);
    file.seek(SeekFrom::Start(len - tail_len)).map_err(&io)?;
    let mut tail = vec![0u8; tail_len as usize];
    file.read_exact(&mut tail).map_err(&io)?;
    // The last end record whose comment reaches exactly to the end of the file.
    let eocd = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| {
            &tail[i..i + 4] == b"PK\x05\x06"
                && i + 22 + u16_at(&tail, i + 20) as usize == tail.len()
        })
        .ok_or_else(|| bad(archive, "end of central directory not found"))?;
    let (this_disk, cd_disk) = (u16_at(&tail, eocd + 4), u16_at(&tail, eocd + 6));
    let locator = eocd
        .checked_sub(20)
        .filter(|&l| &tail[l..l + 4] == b"PK\x06\x07");
    let Some(l) = locator else {
        return if this_disk == 0 && cd_disk == 0 {
            Ok(())
        } else {
            Err(split())
        };
    };
    // Zip64: the end record may hold 0xFFFF placeholders; the zip64 records hold the numbers.
    if !matches!(this_disk, 0 | 0xFFFF) || !matches!(cd_disk, 0 | 0xFFFF) {
        return Err(split());
    }
    let zip64_disk = u32_at(&tail, l + 4);
    let total_disks = u32_at(&tail, l + 16);
    if zip64_disk != 0 || total_disks > 1 {
        return Err(split());
    }
    let record_at = u64::from_le_bytes(tail[l + 8..l + 16].try_into().unwrap());
    let mut record = [0u8; 56];
    file.seek(SeekFrom::Start(record_at)).map_err(&io)?;
    file.read_exact(&mut record).map_err(&io)?;
    // A self-extracting stub may shift offsets; the record is then not where it says, and
    // its disk numbers can't be checked here: refuse rather than guess.
    if &record[..4] != b"PK\x06\x06" {
        return Err(bad(archive, "zip64 end of central directory not found"));
    }
    if u32_at(&record, 16) != 0 || u32_at(&record, 20) != 0 {
        return Err(split());
    }
    Ok(())
}

/// The disk a central record's entry starts on: the 16-bit field, or the zip64 field when
/// that holds 0xFFFF. `None` when the zip64 field is missing.
fn disk_start(central: &[u8]) -> Option<u32> {
    let disk16 = u16_at(central, 34);
    if disk16 != 0xFFFF {
        return Some(u32::from(disk16));
    }
    // In the zip64 field after the sizes and offset the record left to it.
    let skip = 8 * usize::from(u32_at(central, 24) == u32::MAX)
        + 8 * usize::from(u32_at(central, 20) == u32::MAX)
        + 8 * usize::from(u32_at(central, 42) == u32::MAX);
    let extra_at = 46 + u16_at(central, 28) as usize;
    let extra_end = extra_at + u16_at(central, 30) as usize;
    let mut at = extra_at;
    while at + 4 <= extra_end {
        let (id, len) = (u16_at(central, at), u16_at(central, at + 2) as usize);
        if id == ZIP64_EXTRA {
            return (skip + 4 <= len).then(|| u32_at(central, at + 4 + skip));
        }
        at += 4 + len;
    }
    None
}

/// The central record `central` with its local-header offset set to `offset`.
pub(crate) fn with_offset(mut central: Vec<u8>, offset: u64) -> Result<Vec<u8>> {
    let usize32 = u32_at(&central, 24);
    let csize32 = u32_at(&central, 20);
    let offset32 = u32_at(&central, 42);
    let name_len = u16_at(&central, 28) as usize;
    let extra_len = u16_at(&central, 30) as usize;
    let extra_at = 46 + name_len;
    // Where the offset sits inside a zip64 field: after the sizes the record left to it.
    let skip = 8 * usize::from(usize32 == u32::MAX) + 8 * usize::from(csize32 == u32::MAX);
    let mut zip64 = None;
    let mut at = extra_at;
    while at + 4 <= extra_at + extra_len {
        let (id, len) = (u16_at(&central, at), u16_at(&central, at + 2) as usize);
        if id == ZIP64_EXTRA {
            zip64 = Some((at, len));
            break;
        }
        at += 4 + len;
    }
    if offset32 == u32::MAX {
        // The offset is in the zip64 field already: replace it there.
        let (field, len) = zip64.ok_or_else(|| {
            CoreError::Invalid("zip64 offset marker without a zip64 field".into())
        })?;
        if skip + 8 > len {
            return Err(CoreError::Invalid("zip64 field too short".into()));
        }
        central[field + 4 + skip..field + 4 + skip + 8].copy_from_slice(&offset.to_le_bytes());
        return Ok(central);
    }
    if offset < u64::from(u32::MAX) {
        central[42..46].copy_from_slice(&(offset as u32).to_le_bytes());
        return Ok(central);
    }
    // Past 4 GiB with no zip64 offset yet: add it to the zip64 field (or add the field).
    let insert_at = match zip64 {
        Some((field, len)) => {
            let new_len = u16::try_from(len + 8)
                .map_err(|_| CoreError::Invalid("zip64 field too long".into()))?;
            central[field + 2..field + 4].copy_from_slice(&new_len.to_le_bytes());
            field + 4 + skip
        }
        None => {
            let field = extra_at + extra_len;
            central.splice(field..field, [0x01, 0x00, 0x00, 0x00]);
            central[field + 2..field + 4].copy_from_slice(&8u16.to_le_bytes());
            field + 4
        }
    };
    central.splice(insert_at..insert_at, offset.to_le_bytes());
    let added = if zip64.is_some() { 8 } else { 12 };
    let new_extra = u16::try_from(extra_len + added)
        .map_err(|_| CoreError::Invalid("extra fields too long".into()))?;
    central[30..32].copy_from_slice(&new_extra.to_le_bytes());
    central[42..46].copy_from_slice(&u32::MAX.to_le_bytes());
    // Zip64 needs version 4.5 to extract.
    let needed = u16_at(&central, 6).max(45);
    central[6..8].copy_from_slice(&needed.to_le_bytes());
    Ok(central)
}

/// Writes an archive out of copied records, then its central directory.
pub(crate) struct MergeWriter<W: Write> {
    out: W,
    pos: u64,
    central: Vec<Vec<u8>>,
}

impl<W: Write> MergeWriter<W> {
    pub(crate) fn new(out: W) -> Self {
        Self {
            out,
            pos: 0,
            central: Vec::new(),
        }
    }

    /// Copy `record` from `src` (the archive it was read from) as it is.
    pub(crate) fn copy(
        &mut self,
        src: &mut File,
        record: &RawRecord,
        ctx: &JobCtx<'_>,
        buf: &mut [u8],
    ) -> Result<()> {
        let fail = |e: io::Error| CoreError::Invalid(format!("copying an entry: {e}"));
        let central = with_offset(record.central.clone(), self.pos)?;
        src.seek(SeekFrom::Start(record.start)).map_err(fail)?;
        let mut left = record.end - record.start;
        while left > 0 {
            ctx.cancel.check()?;
            let n = (left as usize).min(buf.len());
            src.read_exact(&mut buf[..n]).map_err(fail)?;
            self.out.write_all(&buf[..n]).map_err(fail)?;
            left -= n as u64;
            self.pos += n as u64;
        }
        self.central.push(central);
        Ok(())
    }

    /// Write the central directory and its end records (zip64 when needed), with the
    /// archive `comment`, and return the writer.
    pub(crate) fn finish(mut self, comment: &[u8]) -> io::Result<W> {
        let cd_start = self.pos;
        for c in &self.central {
            self.out.write_all(c)?;
            self.pos += c.len() as u64;
        }
        let cd_size = self.pos - cd_start;
        let count = self.central.len() as u64;
        let zip64 =
            count >= 0xFFFF || cd_start >= u64::from(u32::MAX) || cd_size >= u64::from(u32::MAX);
        let w = &mut self.out;
        if zip64 {
            let record_at = self.pos;
            w.write_all(b"PK\x06\x06")?;
            w.write_all(&44u64.to_le_bytes())?;
            w.write_all(&45u16.to_le_bytes())?; // made by
            w.write_all(&45u16.to_le_bytes())?; // needed
            w.write_all(&0u32.to_le_bytes())?; // this disk
            w.write_all(&0u32.to_le_bytes())?; // disk of the directory
            w.write_all(&count.to_le_bytes())?;
            w.write_all(&count.to_le_bytes())?;
            w.write_all(&cd_size.to_le_bytes())?;
            w.write_all(&cd_start.to_le_bytes())?;
            w.write_all(b"PK\x06\x07")?;
            w.write_all(&0u32.to_le_bytes())?;
            w.write_all(&record_at.to_le_bytes())?;
            w.write_all(&1u32.to_le_bytes())?;
        }
        let comment = &comment[..comment.len().min(0xFFFF)];
        w.write_all(b"PK\x05\x06")?;
        w.write_all(&0u16.to_le_bytes())?;
        w.write_all(&0u16.to_le_bytes())?;
        let n16 = count.min(0xFFFF) as u16;
        w.write_all(&n16.to_le_bytes())?;
        w.write_all(&n16.to_le_bytes())?;
        w.write_all(&(cd_size.min(u64::from(u32::MAX)) as u32).to_le_bytes())?;
        w.write_all(&(cd_start.min(u64::from(u32::MAX)) as u32).to_le_bytes())?;
        w.write_all(&(comment.len() as u16).to_le_bytes())?;
        w.write_all(comment)?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn central(usize32: u32, csize32: u32, offset32: u32, extra: &[u8]) -> Vec<u8> {
        let mut c = vec![0u8; 46];
        c[..4].copy_from_slice(CENTRAL_SIG);
        c[6..8].copy_from_slice(&20u16.to_le_bytes());
        c[20..24].copy_from_slice(&csize32.to_le_bytes());
        c[24..28].copy_from_slice(&usize32.to_le_bytes());
        c[28..30].copy_from_slice(&1u16.to_le_bytes());
        c[30..32].copy_from_slice(&(extra.len() as u16).to_le_bytes());
        c[42..46].copy_from_slice(&offset32.to_le_bytes());
        c.push(b'x');
        c.extend_from_slice(extra);
        c
    }

    #[test]
    fn disk_starts_are_read_from_the_record_or_its_zip64_field() {
        assert_eq!(disk_start(&central(10, 10, 7, &[])), Some(0));
        let mut c = central(10, 10, 7, &[]);
        c[34] = 1;
        assert_eq!(disk_start(&c), Some(1));
        // 0xFFFF: the zip64 field holds it, after the offset the record left to it.
        let mut extra = vec![0x01, 0x00, 12, 0x00];
        extra.extend(9u64.to_le_bytes());
        extra.extend(2u32.to_le_bytes());
        let mut c = central(10, 10, u32::MAX, &extra);
        c[34..36].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert_eq!(disk_start(&c), Some(2));
        // 0xFFFF without the zip64 field: unknown, so refused by the caller.
        let mut c = central(10, 10, 7, &[]);
        c[34..36].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert_eq!(disk_start(&c), None);
    }

    #[test]
    fn offsets_are_patched_in_place_or_in_zip64() {
        // Small offset in a plain record: only bytes 42..46 change.
        let c = central(10, 10, 7, &[]);
        let p = with_offset(c.clone(), 1234).unwrap();
        assert_eq!(u32_at(&p, 42), 1234);
        assert_eq!(p[..42], c[..42]);
        assert_eq!(p[46..], c[46..]);

        // Offset already in a zip64 field after both sizes.
        let mut extra = vec![0x01, 0x00, 24, 0x00];
        extra.extend(5u64.to_le_bytes());
        extra.extend(6u64.to_le_bytes());
        extra.extend(7u64.to_le_bytes());
        let c = central(u32::MAX, u32::MAX, u32::MAX, &extra);
        let p = with_offset(c, 99).unwrap();
        assert_eq!(
            u64::from_le_bytes(p[47 + 4 + 16..47 + 4 + 24].try_into().unwrap()),
            99
        );

        // Past 4 GiB without a zip64 field: one is added holding the offset.
        let big = 5 << 30;
        let p = with_offset(central(10, 10, 7, &[]), big).unwrap();
        assert_eq!(u32_at(&p, 42), u32::MAX);
        assert_eq!(u16_at(&p, 30), 12);
        assert_eq!(&p[47..51], &[0x01, 0x00, 8, 0]);
        assert_eq!(u64::from_le_bytes(p[51..59].try_into().unwrap()), big);
        assert!(u16_at(&p, 6) >= 45);

        // Past 4 GiB with a zip64 field holding the uncompressed size only.
        let mut extra = vec![0x01, 0x00, 8, 0x00];
        extra.extend(5u64.to_le_bytes());
        let p = with_offset(central(u32::MAX, 10, 7, &extra), big).unwrap();
        assert_eq!(u16_at(&p, 30), 20);
        assert_eq!(u16_at(&p, 49), 16);
        assert_eq!(u64::from_le_bytes(p[51..59].try_into().unwrap()), 5);
        assert_eq!(u64::from_le_bytes(p[59..67].try_into().unwrap()), big);
    }
}
