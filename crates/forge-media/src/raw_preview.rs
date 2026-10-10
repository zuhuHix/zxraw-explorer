//! Embedded JPEG preview extraction from camera RAW files.
//!
//! We never develop RAW sensor data (explicit non-goal). Every mainstream RAW embeds one or
//! more JPEG previews; we locate them by parsing the container:
//!
//! * **TIFF-based** (CR2, NEF, NRW, ARW, SR2, DNG, PEF, ORF, RW2, 3FR, ERF, …): walk the IFD
//!   chain, SubIFDs and the EXIF IFD; candidates are `JPEGInterchangeFormat`/`Length` pairs,
//!   single-strip JPEG-compressed IFDs (compression 6/7) and large UNDEFINED blobs that start
//!   with a JPEG SOI (Panasonic `JpgFromRaw`, tag 0x2E).
//! * **CR3** (ISO-BMFF): `THMB` box in the Canon `moov/uuid`, the `PRVW` box in the preview
//!   `uuid`, and the full-size JPEG that is the first sample of the first `trak` in `mdat`.
//! * **RAF**: JPEG offset/length from the fixed header at 0x54/0x58.
//! * **Fallback**: if a TIFF-based file yields nothing usable (ORF/PEF keep big previews in
//!   maker notes), scan the file for JPEG SOI markers and validate each header.
//!
//! Every candidate is validated by walking its JPEG header: lossless JPEG (SOF3, used for
//! raw sensor data in CR2/DNG) is rejected, and its real dimensions come from the SOF.

use std::path::Path;

use crate::bin_util::{
    self, BoxHdr, CR3_CANON_UUID, CR3_PRVW_UUID, Src, Tiff, be32, find_box, find_uuid_box,
    list_boxes, parse_jpeg_header, tag,
};
use crate::error::{MediaError, Result};

/// Which embedded preview to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewPick {
    /// The largest preview by pixel count.
    Largest,
    /// The smallest preview whose long edge is ≥ the given size (cheapest to decode for a
    /// thumbnail); falls back to the largest if none is big enough.
    AtLeast(u32),
}

/// Where in the container the preview came from (diagnostics / tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewSource {
    TiffJpegInterchange,
    TiffStrip,
    TiffBlob,
    Cr3Thumb,
    Cr3Prvw,
    Cr3Full,
    RafHeader,
    Scan,
}

#[derive(Debug, Clone)]
pub struct RawPreview {
    /// Complete JPEG bytes.
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// EXIF orientation (1..=8) to apply to the preview; 1 if unknown.
    pub orientation: u16,
    pub source: PreviewSource,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Candidate {
    pub off: u64,
    /// Length if known from the container; `None` for scan hits (computed later).
    pub len: Option<u64>,
    pub width: u32,
    pub height: u32,
    pub source: PreviewSource,
}

impl Candidate {
    fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

const MIN_JPEG_LEN: u64 = 512;
/// Largest embedded JPEG we will read (full-size previews of 100 MP bodies are ~30 MB).
const MAX_JPEG_LEN: u64 = 96 * 1024 * 1024;
const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;

/// Extracts an embedded JPEG preview from a RAW file.
pub fn extract_preview(path: &Path, pick: PreviewPick) -> Result<RawPreview> {
    let mut src = Src::open(path)?;
    extract(&mut src, pick)
}

/// Same as [`extract_preview`] for an in-memory file.
pub fn extract_preview_from_bytes(data: &[u8], pick: PreviewPick) -> Result<RawPreview> {
    let mut src = Src::mem(data);
    extract(&mut src, pick)
}

fn extract(src: &mut Src, pick: PreviewPick) -> Result<RawPreview> {
    let (cands, orientation) = candidates(src)?;
    let best = choose(&cands, pick).ok_or(MediaError::Missing("embedded JPEG preview"))?;
    let jpeg = read_candidate(src, &best)?;
    Ok(RawPreview {
        jpeg,
        width: best.width,
        height: best.height,
        orientation,
        source: best.source,
    })
}

fn choose(cands: &[Candidate], pick: PreviewPick) -> Option<Candidate> {
    let largest = cands.iter().max_by_key(|c| c.area()).copied();
    match pick {
        PreviewPick::Largest => largest,
        PreviewPick::AtLeast(n) => cands
            .iter()
            .filter(|c| c.width.max(c.height) >= n)
            .min_by_key(|c| c.area())
            .copied()
            .or(largest),
    }
}

fn read_candidate(src: &mut Src, c: &Candidate) -> Result<Vec<u8>> {
    match c.len {
        Some(len) => {
            let data = src.read_at(c.off, len)?;
            // Trim trailing padding after EOI if we can find it cheaply.
            Ok(data)
        }
        None => {
            let data = src.read_upto(c.off, MAX_JPEG_LEN)?;
            let end = bin_util::jpeg_end(&data).unwrap_or(data.len());
            let mut data = data;
            data.truncate(end);
            Ok(data)
        }
    }
}

/// All valid preview candidates plus the orientation that applies to them.
pub(crate) fn candidates(src: &mut Src) -> Result<(Vec<Candidate>, u16)> {
    let head = src.read_upto(0, 128)?;
    if bin_util::is_cr3(&head) {
        return Ok(cr3_candidates(src));
    }
    if let Some((off, len)) = bin_util::raf_jpeg_range(&head) {
        let mut v = Vec::new();
        if let Some(c) = validate(src, off, len, PreviewSource::RafHeader) {
            v.push(c);
        }
        let orientation = src
            .read_upto(off, len.min(256 * 1024))
            .ok()
            .and_then(|b| crate::image_meta::orientation_from_jpeg_bytes(&b))
            .unwrap_or(1);
        return Ok((v, orientation));
    }
    if bin_util::tiff_header(&head).is_some() {
        let tiff = Tiff::parse(src, 0, true)?;
        let mut v = tiff_candidates(src, &tiff);
        let orientation = tiff
            .ifds
            .first()
            .and_then(|i| tiff.uint(src, i, tag::ORIENTATION))
            .filter(|o| (1..=8).contains(o))
            .unwrap_or(1) as u16;
        if v.iter().all(|c| c.width.max(c.height) < 640) {
            v.extend(scan_candidates(src));
        }
        return Ok((v, orientation));
    }
    Err(MediaError::Unsupported(
        "not a recognised RAW container (TIFF/CR3/RAF)".into(),
    ))
}

fn validate(src: &mut Src, off: u64, len: u64, source: PreviewSource) -> Option<Candidate> {
    if !(MIN_JPEG_LEN..=MAX_JPEG_LEN).contains(&len) || off.checked_add(len)? > src.len() {
        return None;
    }
    let head = src.read_at(off, len.min(512 * 1024)).ok()?;
    let info = parse_jpeg_header(&head)?;
    info.is_displayable().then_some(Candidate {
        off,
        len: Some(len),
        width: info.width,
        height: info.height,
        source,
    })
}

fn tiff_candidates(src: &mut Src, tiff: &Tiff) -> Vec<Candidate> {
    let mut out = Vec::new();
    for ifd in &tiff.ifds {
        // JPEGInterchangeFormat / Length
        if let (Some(o), Some(l)) = (
            tiff.uint(src, ifd, tag::JPEG_IF_OFFSET),
            tiff.uint(src, ifd, tag::JPEG_IF_LENGTH),
        ) && let Some(c) = validate(
            src,
            tiff.base + o as u64,
            l as u64,
            PreviewSource::TiffJpegInterchange,
        ) {
            out.push(c);
        }
        // Single-strip JPEG-compressed image.
        let comp = tiff.uint(src, ifd, tag::COMPRESSION);
        if matches!(comp, Some(6 | 7))
            && let (Some(so), Some(sc)) =
                (ifd.get(tag::STRIP_OFFSETS), ifd.get(tag::STRIP_BYTE_COUNTS))
            && so.count == 1
            && sc.count == 1
        {
            let o = tiff.uints(src, so, 1);
            let l = tiff.uints(src, sc, 1);
            if let (Some(&o), Some(&l)) = (o.first(), l.first())
                && let Some(c) = validate(
                    src,
                    tiff.base + o as u64,
                    l as u64,
                    PreviewSource::TiffStrip,
                )
            {
                out.push(c);
            }
        }
        // Large UNDEFINED/BYTE blobs that are JPEGs (e.g. RW2 JpgFromRaw 0x2E).
        for e in &ifd.entries {
            if matches!(e.typ, 1 | 7)
                && e.count as u64 >= 4096
                && e.tag != 0x927C
                && let Some(off) = tiff.value_offset(e)
                && let Ok(sig) = src.read_at(off, 3)
                && sig == [0xFF, 0xD8, 0xFF]
                && let Some(c) = validate(src, off, e.count as u64, PreviewSource::TiffBlob)
            {
                out.push(c);
            }
        }
    }
    dedupe(out)
}

fn dedupe(mut v: Vec<Candidate>) -> Vec<Candidate> {
    v.sort_by_key(|c| c.off);
    v.dedup_by_key(|c| c.off);
    v
}

/// Brute-force scan for JPEG SOI markers; each hit is validated by its header.
fn scan_candidates(src: &mut Src) -> Vec<Candidate> {
    const CHUNK: u64 = 4 * 1024 * 1024;
    let mut out = Vec::new();
    let end = src.len().min(MAX_SCAN_BYTES);
    let mut pos = 0u64;
    while pos < end && out.len() < 64 {
        let Ok(buf) = src.read_upto(pos, CHUNK + 2) else {
            break;
        };
        if buf.len() < 3 {
            break;
        }
        let limit = buf.len().saturating_sub(2).min(CHUNK as usize);
        let mut i = 0;
        while i < limit {
            if buf[i] == 0xFF && buf[i + 1] == 0xD8 && buf[i + 2] == 0xFF {
                let off = pos + i as u64;
                // Parse from the chunk already in memory; only re-read near the chunk end.
                let head = if buf.len() - i >= 256 * 1024 {
                    None
                } else {
                    src.read_upto(off, 512 * 1024).ok()
                };
                let head_ref = head.as_deref().unwrap_or(&buf[i..]);
                if let Some(info) = parse_jpeg_header(head_ref)
                    && info.is_displayable()
                    && info.width as u64 * info.height as u64 >= 160 * 120
                {
                    out.push(Candidate {
                        off,
                        len: None,
                        width: info.width,
                        height: info.height,
                        source: PreviewSource::Scan,
                    });
                }
            }
            i += 1;
        }
        pos += CHUNK;
    }
    out
}

// ---------------------------------------------------------------------------
// CR3
// ---------------------------------------------------------------------------

/// Locates a JPEG (SOI) within the first 64 bytes of a box payload.
fn jpeg_in_box(src: &mut Src, b: &BoxHdr, source: PreviewSource) -> Option<Candidate> {
    let head = src.read_upto(b.data_off, 64.min(b.data_len)).ok()?;
    let pos = head.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF])? as u64;
    validate(src, b.data_off + pos, b.data_len - pos, source)
}

pub(crate) struct Cr3Layout {
    pub cmt: [Option<BoxHdr>; 4],
    pub thmb: Option<BoxHdr>,
    pub prvw: Option<BoxHdr>,
    /// (offset, size) of the first sample of the first track (full-size JPEG).
    pub full: Option<(u64, u64)>,
}

pub(crate) fn cr3_layout(src: &mut Src) -> Cr3Layout {
    let mut l = Cr3Layout {
        cmt: [None; 4],
        thmb: None,
        prvw: None,
        full: None,
    };
    let len = src.len();
    let top = list_boxes(src, 0, len);
    if let Some(moov) = find_box(&top, b"moov").copied() {
        let kids = list_boxes(src, moov.data_off, moov.data_off + moov.data_len);
        if let Some(canon) = find_uuid_box(src, &kids, &CR3_CANON_UUID) {
            let ck = list_boxes(src, canon.data_off, canon.data_off + canon.data_len);
            for (i, name) in [b"CMT1", b"CMT2", b"CMT3", b"CMT4"].iter().enumerate() {
                l.cmt[i] = find_box(&ck, name).copied();
            }
            l.thmb = find_box(&ck, b"THMB").copied();
        }
        if let Some(trak) = find_box(&kids, b"trak").copied() {
            l.full = first_sample(src, &trak);
        }
    }
    if let Some(p) = find_uuid_box(src, &top, &CR3_PRVW_UUID) {
        // Payload: 8 bytes of unknown header, then a PRVW box.
        let head = src
            .read_upto(p.data_off, 64.min(p.data_len))
            .unwrap_or_default();
        if let Some(i) = head.windows(4).position(|w| w == b"PRVW") {
            let start = (p.data_off + i as u64).saturating_sub(4);
            let inner = list_boxes(src, start, p.data_off + p.data_len);
            l.prvw = find_box(&inner, b"PRVW").copied();
        }
    }
    l
}

fn first_sample(src: &mut Src, trak: &BoxHdr) -> Option<(u64, u64)> {
    let mut cur = *trak;
    for name in [b"mdia", b"minf", b"stbl"] {
        let kids = list_boxes(src, cur.data_off, cur.data_off + cur.data_len);
        cur = *find_box(&kids, name)?;
    }
    let kids = list_boxes(src, cur.data_off, cur.data_off + cur.data_len);
    let off = if let Some(co) = find_box(&kids, b"co64") {
        let d = src.read_upto(co.data_off, 16).ok()?;
        (be32(&d, 4)? >= 1).then_some(())?;
        bin_util::be64(&d, 8)?
    } else {
        let co = find_box(&kids, b"stco")?;
        let d = src.read_upto(co.data_off, 12).ok()?;
        (be32(&d, 4)? >= 1).then_some(())?;
        be32(&d, 8)? as u64
    };
    let sz = find_box(&kids, b"stsz")?;
    let d = src.read_upto(sz.data_off, 16).ok()?;
    let fixed = be32(&d, 4)?;
    let count = be32(&d, 8)?;
    if count == 0 {
        return None;
    }
    let size = if fixed != 0 { fixed } else { be32(&d, 12)? };
    Some((off, size as u64))
}

fn cr3_candidates(src: &mut Src) -> (Vec<Candidate>, u16) {
    let l = cr3_layout(src);
    let mut v = Vec::new();
    if let Some(b) = l.thmb
        && let Some(c) = jpeg_in_box(src, &b, PreviewSource::Cr3Thumb)
    {
        v.push(c);
    }
    if let Some(b) = l.prvw
        && let Some(c) = jpeg_in_box(src, &b, PreviewSource::Cr3Prvw)
    {
        v.push(c);
    }
    if let Some((off, len)) = l.full
        && let Some(c) = validate(src, off, len, PreviewSource::Cr3Full)
    {
        v.push(c);
    }
    let orientation = l.cmt[0]
        .and_then(|b| {
            let t = Tiff::parse(src, b.data_off, false).ok()?;
            let ifd0 = t.ifds.first()?;
            t.uint(src, ifd0, tag::ORIENTATION)
        })
        .filter(|o| (1..=8).contains(o))
        .unwrap_or(1) as u16;
    (v, orientation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_raw() {
        assert!(extract_preview_from_bytes(b"hello world", PreviewPick::Largest).is_err());
        assert!(extract_preview_from_bytes(&[], PreviewPick::Largest).is_err());
    }

    #[test]
    fn truncated_raf_is_error() {
        let mut d = bin_util::RAF_MAGIC.to_vec();
        d.resize(0x60, 0);
        d[0x54..0x58].copy_from_slice(&1000u32.to_be_bytes());
        d[0x58..0x5C].copy_from_slice(&5000u32.to_be_bytes());
        assert!(extract_preview_from_bytes(&d, PreviewPick::Largest).is_err());
    }
}
