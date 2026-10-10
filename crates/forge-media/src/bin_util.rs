//! Minimal, bounds-checked binary parsing helpers shared by the metadata and RAW preview
//! code: random-access sources, JPEG marker walking, a TIFF IFD walker and an ISO-BMFF box
//! iterator. All input is untrusted: every offset/length is checked, every loop is bounded.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::{MediaError, Result};

/// Largest single read we will ever perform from an untrusted offset/length pair.
pub(crate) const MAX_READ: u64 = 128 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Random-access source
// ---------------------------------------------------------------------------

/// A random-access byte source: either an open file or an in-memory buffer.
pub(crate) enum Src<'a> {
    File { file: File, len: u64, path: PathBuf },
    Mem(&'a [u8]),
}

impl<'a> Src<'a> {
    pub fn open(path: &Path) -> Result<Src<'static>> {
        let file = File::open(path).map_err(|e| MediaError::io(path, e))?;
        let len = file.metadata().map_err(|e| MediaError::io(path, e))?.len();
        Ok(Src::File {
            file,
            len,
            path: path.to_path_buf(),
        })
    }

    pub fn mem(data: &'a [u8]) -> Self {
        Src::Mem(data)
    }

    pub fn len(&self) -> u64 {
        match self {
            Src::File { len, .. } => *len,
            Src::Mem(d) => d.len() as u64,
        }
    }

    /// Reads exactly `len` bytes at `off`. Fails (never panics) if out of range.
    pub fn read_at(&mut self, off: u64, len: u64) -> Result<Vec<u8>> {
        if len > MAX_READ {
            return Err(MediaError::corrupt(format!("read of {len} bytes refused")));
        }
        let end = off
            .checked_add(len)
            .ok_or_else(|| MediaError::corrupt("offset overflow"))?;
        if end > self.len() {
            return Err(MediaError::corrupt(format!(
                "range {off}..{end} beyond end of data ({})",
                self.len()
            )));
        }
        match self {
            Src::Mem(d) => Ok(d[off as usize..end as usize].to_vec()),
            Src::File { file, path, .. } => {
                let mut buf = vec![0u8; len as usize];
                file.seek(SeekFrom::Start(off))
                    .map_err(|e| MediaError::io(&*path, e))?;
                file.read_exact(&mut buf)
                    .map_err(|e| MediaError::io(&*path, e))?;
                Ok(buf)
            }
        }
    }

    /// Reads up to `len` bytes at `off` (clamped to the end of data).
    pub fn read_upto(&mut self, off: u64, len: u64) -> Result<Vec<u8>> {
        if off >= self.len() {
            return Ok(Vec::new());
        }
        let len = len.min(self.len() - off);
        self.read_at(off, len)
    }
}

/// Reads a JPEG's header segments (everything before SOS): 256 KB first, up to 2 MB if the
/// first SOS is further in (large APP segments such as ICC profiles or extended XMP).
pub(crate) fn read_jpeg_header_prefix(path: &Path) -> Result<Vec<u8>> {
    let small = read_prefix(path, 256 * 1024)?;
    if parse_jpeg_header(&small).is_some() || (small.len() as u64) < 256 * 1024 {
        return Ok(small);
    }
    read_prefix(path, 2 * 1024 * 1024)
}

/// Reads at most `cap` bytes from the start of a file.
pub(crate) fn read_prefix(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let f = File::open(path).map_err(|e| MediaError::io(path, e))?;
    let mut buf = Vec::new();
    f.take(cap)
        .read_to_end(&mut buf)
        .map_err(|e| MediaError::io(path, e))?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Endian helpers
// ---------------------------------------------------------------------------

pub(crate) fn be16(d: &[u8], o: usize) -> Option<u16> {
    d.get(o..o.checked_add(2)?)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}
pub(crate) fn be32(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o.checked_add(4)?)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}
pub(crate) fn be64(d: &[u8], o: usize) -> Option<u64> {
    d.get(o..o.checked_add(8)?).map(|b| {
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        u64::from_be_bytes(a)
    })
}
pub(crate) fn u16e(d: &[u8], o: usize, le: bool) -> Option<u16> {
    d.get(o..o.checked_add(2)?).map(|b| {
        if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    })
}
pub(crate) fn u32e(d: &[u8], o: usize, le: bool) -> Option<u32> {
    d.get(o..o.checked_add(4)?).map(|b| {
        let a = [b[0], b[1], b[2], b[3]];
        if le {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    })
}

// ---------------------------------------------------------------------------
// JPEG
// ---------------------------------------------------------------------------

/// What we learn from walking a JPEG's header segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JpegInfo {
    pub width: u32,
    pub height: u32,
    /// SOF marker type (0xC0..=0xCF). Baseline/extended/progressive = C0/C1/C2.
    pub sof: u8,
    /// Byte offset of the first SOS marker.
    pub sos_offset: usize,
    /// Sample precision in bits (8 for baseline, 12 for extended).
    pub precision: u8,
}

impl JpegInfo {
    /// True for DCT JPEGs a normal decoder handles (excludes lossless SOF3 used for RAW data).
    pub fn is_displayable(&self) -> bool {
        matches!(self.sof, 0xC0..=0xC2) && self.width > 0 && self.height > 0
    }
}

/// Walks JPEG marker segments up to SOS. `data` may be a prefix of the file.
pub(crate) fn parse_jpeg_header(data: &[u8]) -> Option<JpegInfo> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    let mut sof: Option<(u8, u32, u32, u8)> = None;
    for _ in 0..1024 {
        // Skip to marker, tolerating fill bytes.
        if *data.get(pos)? != 0xFF {
            return None;
        }
        while *data.get(pos)? == 0xFF {
            pos += 1;
        }
        let marker = *data.get(pos)?;
        pos += 1;
        match marker {
            0xD8 | 0x01 | 0xD0..=0xD7 => continue,
            0xD9 => return None,
            0xDA => {
                let (sof, width, height, precision) = sof?;
                return Some(JpegInfo {
                    width,
                    height,
                    sof,
                    sos_offset: pos - 2,
                    precision,
                });
            }
            _ => {}
        }
        let len = be16(data, pos)? as usize;
        if len < 2 {
            return None;
        }
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = be16(data, pos + 3)? as u32;
            let w = be16(data, pos + 5)? as u32;
            let prec = *data.get(pos + 2)?;
            sof = Some((marker, w, h, prec));
        }
        pos = pos.checked_add(len)?;
    }
    None
}

/// Given a complete JPEG starting at `data[0]`, returns its length up to and including EOI
/// (or `None` if no EOI is found).
pub(crate) fn jpeg_end(data: &[u8]) -> Option<usize> {
    let info = parse_jpeg_header(data)?;
    let mut i = info.sos_offset + 2;
    // 0xFF 0xD9 cannot occur inside entropy-coded data (0xFF is byte-stuffed), so the first
    // occurrence after SOS is the EOI (progressive files have further SOS markers before it).
    while i + 1 < data.len() {
        if data[i] == 0xFF && data[i + 1] == 0xD9 {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// TIFF
// ---------------------------------------------------------------------------

pub(crate) mod tag {
    pub const NEW_SUBFILE_TYPE: u16 = 0x00FE;
    pub const IMAGE_WIDTH: u16 = 0x0100;
    pub const IMAGE_LENGTH: u16 = 0x0101;
    pub const BITS_PER_SAMPLE: u16 = 0x0102;
    pub const COMPRESSION: u16 = 0x0103;
    pub const STRIP_OFFSETS: u16 = 0x0111;
    pub const ORIENTATION: u16 = 0x0112;
    pub const STRIP_BYTE_COUNTS: u16 = 0x0117;
    pub const SUB_IFDS: u16 = 0x014A;
    pub const JPEG_IF_OFFSET: u16 = 0x0201;
    pub const JPEG_IF_LENGTH: u16 = 0x0202;
    pub const EXIF_IFD: u16 = 0x8769;
    pub const XMP: u16 = 0x02BC;
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TiffEntry {
    pub tag: u16,
    pub typ: u16,
    pub count: u32,
    /// Raw 4-byte value/offset field.
    pub raw: [u8; 4],
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Ifd {
    pub entries: Vec<TiffEntry>,
}

impl Ifd {
    pub fn get(&self, tag: u16) -> Option<&TiffEntry> {
        self.entries.iter().find(|e| e.tag == tag)
    }
}

fn type_size(typ: u16) -> u64 {
    match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => 0,
    }
}

/// Parsed TIFF structure: all IFDs reachable from the header (chain + SubIFDs, optionally
/// the EXIF IFD), with offsets resolved relative to `base` in the source.
pub(crate) struct Tiff {
    pub le: bool,
    pub base: u64,
    pub ifds: Vec<Ifd>,
}

/// TIFF variants: standard (42), Olympus ORF ("RO"/"SR"), Panasonic RW2 (0x55).
pub(crate) fn tiff_header(d: &[u8]) -> Option<(bool, u32)> {
    let le = match d.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let magic = u16e(d, 2, le)?;
    if !matches!(magic, 42 | 0x4F52 | 0x5352 | 0x0055) {
        return None;
    }
    Some((le, u32e(d, 4, le)?))
}

const MAX_IFDS: usize = 64;
const MAX_ENTRIES: usize = 2000;

impl Tiff {
    /// Parses the TIFF whose header is at `base`. `follow_exif` also descends into the
    /// EXIF IFD (some formats keep previews referenced there).
    pub fn parse(src: &mut Src, base: u64, follow_exif: bool) -> Result<Tiff> {
        let hdr = src.read_at(base, 8)?;
        let (le, first) =
            tiff_header(&hdr).ok_or_else(|| MediaError::corrupt("not a TIFF header"))?;
        let mut t = Tiff {
            le,
            base,
            ifds: Vec::new(),
        };
        let mut visited = HashSet::new();
        let mut next = first as u64;
        // Main chain
        let mut chain = 0;
        while next != 0 && chain < 16 {
            chain += 1;
            match t.walk(src, next, 0, follow_exif, &mut visited) {
                Some(n) => next = n,
                None => break,
            }
        }
        if t.ifds.is_empty() {
            return Err(MediaError::corrupt("TIFF has no readable IFD"));
        }
        Ok(t)
    }

    /// Reads one IFD (and its children). Returns the next-IFD offset.
    fn walk(
        &mut self,
        src: &mut Src,
        off: u64,
        depth: u32,
        follow_exif: bool,
        visited: &mut HashSet<u64>,
    ) -> Option<u64> {
        if depth > 3 || self.ifds.len() >= MAX_IFDS || !visited.insert(off) {
            return None;
        }
        let abs = self.base.checked_add(off)?;
        let cnt_b = src.read_at(abs, 2).ok()?;
        let count = u16e(&cnt_b, 0, self.le)? as usize;
        if count == 0 || count > MAX_ENTRIES {
            return None;
        }
        let body = src
            .read_at(abs + 2, count as u64 * 12 + 4)
            .ok()
            .or_else(|| {
                // Tolerate a missing next-offset at EOF.
                src.read_at(abs + 2, count as u64 * 12).ok()
            })?;
        let mut ifd = Ifd::default();
        for i in 0..count {
            let o = i * 12;
            let raw = body.get(o + 8..o + 12)?;
            ifd.entries.push(TiffEntry {
                tag: u16e(&body, o, self.le)?,
                typ: u16e(&body, o + 2, self.le)?,
                count: u32e(&body, o + 4, self.le)?,
                raw: [raw[0], raw[1], raw[2], raw[3]],
            });
        }
        let next = u32e(&body, count * 12, self.le).unwrap_or(0) as u64;
        let mut children = Vec::new();
        if let Some(e) = ifd.get(tag::SUB_IFDS) {
            children.extend(self.uints(src, e, 16));
        }
        if follow_exif && let Some(e) = ifd.get(tag::EXIF_IFD) {
            children.extend(self.uints(src, e, 1));
        }
        self.ifds.push(ifd);
        for c in children {
            if c != 0 {
                self.walk(src, c as u64, depth + 1, follow_exif, visited);
            }
        }
        Some(next)
    }

    /// Absolute offset of an entry's value data.
    pub fn value_offset(&self, e: &TiffEntry) -> Option<u64> {
        let size = type_size(e.typ).checked_mul(e.count as u64)?;
        if size <= 4 {
            None
        } else {
            let o = if self.le {
                u32::from_le_bytes(e.raw)
            } else {
                u32::from_be_bytes(e.raw)
            };
            self.base.checked_add(o as u64)
        }
    }

    /// Unsigned integer values of a SHORT/LONG/IFD entry (at most `max`).
    pub fn uints(&self, src: &mut Src, e: &TiffEntry, max: usize) -> Vec<u32> {
        let n = (e.count as usize).min(max);
        let sz = type_size(e.typ);
        if !matches!(e.typ, 3 | 4 | 13) || n == 0 {
            return Vec::new();
        }
        let bytes: Vec<u8> = match self.value_offset(e) {
            None => e.raw.to_vec(),
            Some(off) => match src.read_at(off, n as u64 * sz) {
                Ok(b) => b,
                Err(_) => return Vec::new(),
            },
        };
        (0..n)
            .filter_map(|i| {
                if sz == 2 {
                    u16e(&bytes, i * 2, self.le).map(u32::from)
                } else {
                    u32e(&bytes, i * 4, self.le)
                }
            })
            .collect()
    }

    pub fn uint(&self, src: &mut Src, ifd: &Ifd, t: u16) -> Option<u32> {
        let e = ifd.get(t)?;
        self.uints(src, e, 1).first().copied()
    }
}

// ---------------------------------------------------------------------------
// ISO-BMFF (CR3)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub(crate) struct BoxHdr {
    pub typ: [u8; 4],
    /// Absolute offset of the payload.
    pub data_off: u64,
    /// Payload length.
    pub data_len: u64,
}

/// Lists the child boxes inside `[start, end)`. Bounded to 512 boxes.
pub(crate) fn list_boxes(src: &mut Src, start: u64, end: u64) -> Vec<BoxHdr> {
    let end = end.min(src.len());
    let mut out = Vec::new();
    let mut pos = start;
    while pos.saturating_add(8) <= end && out.len() < 512 {
        let Ok(h) = src.read_at(pos, 8) else { break };
        let size32 = be32(&h, 0).unwrap_or(0) as u64;
        let typ = [h[4], h[5], h[6], h[7]];
        let (hdr_len, size) = match size32 {
            0 => (8, end - pos),
            1 => {
                let Ok(x) = src.read_at(pos + 8, 8) else {
                    break;
                };
                (16, be64(&x, 0).unwrap_or(0))
            }
            s => (8, s),
        };
        if size < hdr_len || pos.saturating_add(size) > end {
            break;
        }
        out.push(BoxHdr {
            typ,
            data_off: pos + hdr_len,
            data_len: size - hdr_len,
        });
        pos += size;
    }
    out
}

pub(crate) fn find_box<'b>(boxes: &'b [BoxHdr], typ: &[u8; 4]) -> Option<&'b BoxHdr> {
    boxes.iter().find(|b| &b.typ == typ)
}

/// Finds a top-level/child `uuid` box with the given 16-byte UUID.
pub(crate) fn find_uuid_box(src: &mut Src, boxes: &[BoxHdr], uuid: &[u8; 16]) -> Option<BoxHdr> {
    for b in boxes.iter().filter(|b| &b.typ == b"uuid") {
        if b.data_len >= 16
            && let Ok(u) = src.read_at(b.data_off, 16)
            && u.as_slice() == uuid
        {
            return Some(BoxHdr {
                typ: *b"uuid",
                data_off: b.data_off + 16,
                data_len: b.data_len - 16,
            });
        }
    }
    None
}

pub(crate) const CR3_CANON_UUID: [u8; 16] = [
    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48,
];
pub(crate) const CR3_PRVW_UUID: [u8; 16] = [
    0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16,
];

/// True if the first bytes look like a Canon CR3 (ISO-BMFF with brand "crx ").
pub(crate) fn is_cr3(head: &[u8]) -> bool {
    head.get(4..8) == Some(b"ftyp") && head.get(8..12) == Some(b"crx ")
}

/// Canon CR3 / generic ISO-BMFF XMP box UUID (be7acfcb-97a9-42e8-9c71-999491e3afac).
pub(crate) const XMP_UUID: [u8; 16] = [
    0xbe, 0x7a, 0xcf, 0xcb, 0x97, 0xa9, 0x42, 0xe8, 0x9c, 0x71, 0x99, 0x94, 0x91, 0xe3, 0xaf, 0xac,
];

pub(crate) const RAF_MAGIC: &[u8] = b"FUJIFILMCCD-RAW ";

pub(crate) fn is_raf(head: &[u8]) -> bool {
    head.starts_with(RAF_MAGIC)
}

/// RAF header: (JPEG offset, JPEG length), big-endian at 0x54 / 0x58.
pub(crate) fn raf_jpeg_range(head: &[u8]) -> Option<(u64, u64)> {
    if !is_raf(head) {
        return None;
    }
    let off = be32(head, 0x54)? as u64;
    let len = be32(head, 0x58)? as u64;
    (off > 0 && len > 0).then_some((off, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_header_rejects_garbage() {
        assert!(parse_jpeg_header(&[]).is_none());
        assert!(parse_jpeg_header(&[0xFF, 0xD8, 0xFF]).is_none());
        assert!(parse_jpeg_header(&[0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x01]).is_none());
        let mut v = vec![0xFF, 0xD8];
        v.extend(std::iter::repeat_n(0xFF, 10_000));
        assert!(parse_jpeg_header(&v).is_none());
    }

    #[test]
    fn tiff_rejects_loops_and_truncation() {
        // IFD at 8 whose next pointer points back to itself.
        let mut d = b"II*\0\x08\0\0\0".to_vec();
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&0x0100u16.to_le_bytes());
        d.extend_from_slice(&3u16.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&[10, 0, 0, 0]);
        d.extend_from_slice(&8u32.to_le_bytes());
        let mut src = Src::mem(&d);
        let t = Tiff::parse(&mut src, 0, true).unwrap();
        assert_eq!(t.ifds.len(), 1);
        // Truncated
        let mut src = Src::mem(&d[..12]);
        assert!(Tiff::parse(&mut src, 0, true).is_err());
    }

    #[test]
    fn boxes_bounded() {
        // size field larger than data
        let d = [0u8, 0, 0xFF, 0xFF, b'f', b't', b'y', b'p'];
        let mut src = Src::mem(&d);
        assert!(list_boxes(&mut src, 0, 8).is_empty());
    }
}
