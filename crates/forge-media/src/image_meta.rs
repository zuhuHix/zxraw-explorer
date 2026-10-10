//! Still-image metadata: EXIF (via `kamadak-exif`, with our own container parsing for
//! RAW formats it cannot open directly) and XMP (embedded packet or `.xmp` sidecar).
//!
//! All reads are capped; malformed data yields partial results or an error, never a panic.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use exif::{Exif, Field, In, Tag, Value};
use serde::{Deserialize, Serialize};

use crate::bin_util::{self, Src, Tiff, tag};
use crate::error::{MediaError, Result};
use crate::raw_preview;

/// Detected container format (by magic bytes, extension only as a tie-breaker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Gif,
    Bmp,
    WebP,
    Tiff,
    /// HEIC/HEIF/AVIF (EXIF only; pixels need WIC).
    Heif,
    /// TIFF-based RAW (CR2, NEF, ARW, DNG, ORF, RW2, PEF, …).
    TiffRaw,
    Cr3,
    Raf,
    Unknown,
}

impl ImageFormat {
    pub fn is_raw(self) -> bool {
        matches!(self, Self::TiffRaw | Self::Cr3 | Self::Raf)
    }
}

/// Sniffs a file's format from its first bytes (and extension for TIFF vs TIFF-RAW).
pub fn detect_format(head: &[u8], ext: &str) -> ImageFormat {
    let ext = ext.to_ascii_lowercase();
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        ImageFormat::Jpeg
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        ImageFormat::Png
    } else if head.starts_with(b"GIF8") {
        ImageFormat::Gif
    } else if head.starts_with(b"BM") {
        ImageFormat::Bmp
    } else if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        ImageFormat::WebP
    } else if bin_util::is_cr3(head) {
        ImageFormat::Cr3
    } else if bin_util::is_raf(head) {
        ImageFormat::Raf
    } else if head.get(4..8) == Some(b"ftyp") {
        match head.get(8..12) {
            Some(
                b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"mif1" | b"msf1" | b"avif"
                | b"avis",
            ) => ImageFormat::Heif,
            _ => ImageFormat::Unknown,
        }
    } else if let Some((le, _)) = bin_util::tiff_header(head) {
        let magic = bin_util::u16e(head, 2, le).unwrap_or(42);
        if magic != 42 || forge_core::model::RAW_EXTS.contains(&ext.as_str()) {
            ImageFormat::TiffRaw
        } else {
            ImageFormat::Tiff
        }
    } else {
        ImageFormat::Unknown
    }
}

/// One EXIF tag for the details panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExifTag {
    /// "Tiff" / "Exif" / "Gps" / "Interop" plus IFD ("primary"/"thumbnail").
    pub group: String,
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum XmpSource {
    #[default]
    Embedded,
    Sidecar,
}

/// The subset of XMP we surface (Lightroom / Bridge / Capture One compatible).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct XmpData {
    /// -1 = rejected, 0 = unrated, 1..=5 stars.
    pub rating: Option<i32>,
    pub label: Option<String>,
    pub keywords: Vec<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub creator: Option<String>,
    pub source: XmpSource,
}

impl XmpData {
    pub fn is_empty(&self) -> bool {
        self.rating.is_none()
            && self.label.is_none()
            && self.keywords.is_empty()
            && self.title.is_none()
            && self.description.is_none()
            && self.creator.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ImageMetadata {
    pub format: Option<ImageFormat>,
    /// Stored pixel dimensions (before orientation). For RAW: the largest of sensor/EXIF/
    /// embedded-preview dimensions.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub focal_length_mm: Option<f64>,
    pub focal_length_35mm: Option<u32>,
    /// f-number, e.g. 2.8.
    pub aperture: Option<f64>,
    /// Exposure time in seconds.
    pub exposure_time_s: Option<f64>,
    /// Human-readable shutter speed, e.g. "1/250" or "2s".
    pub shutter: Option<String>,
    pub iso: Option<u32>,
    /// EV, e.g. -0.7.
    pub exposure_compensation: Option<f64>,
    /// ISO-8601-like local capture time, e.g. "2024-05-01T12:34:56.12+02:00" (offset only if
    /// the file records one).
    pub date_taken: Option<String>,
    /// Seconds since the Unix epoch. UTC if an offset is recorded; otherwise the camera's
    /// local wall-clock time interpreted as UTC (fine for ordering/burst grouping).
    pub capture_timestamp: Option<i64>,
    /// Sub-second part of the capture time, if recorded (milliseconds).
    pub capture_millis: Option<u32>,
    pub gps_lat: Option<f64>,
    pub gps_lon: Option<f64>,
    pub gps_alt_m: Option<f64>,
    /// EXIF orientation 1..=8.
    pub orientation: Option<u16>,
    pub color_space: Option<String>,
    /// Bits per channel/sample.
    pub bit_depth: Option<u32>,
    pub xmp: Option<XmpData>,
    pub exif_tags: Vec<ExifTag>,
}

impl ImageMetadata {
    /// Dimensions after applying the EXIF orientation (swapped for 5..=8).
    pub fn display_dimensions(&self) -> Option<(u32, u32)> {
        let (w, h) = (self.width?, self.height?);
        Some(if matches!(self.orientation, Some(5..=8)) {
            (h, w)
        } else {
            (w, h)
        })
    }
}

const CONTAINER_CAP: u64 = 16 * 1024 * 1024;
/// Largest metadata chunk (EXIF/XMP) we read from PNG/WebP containers.
const MAX_BLOCK: u64 = 4 * 1024 * 1024;
const TIFF_FIRST_CAP: u64 = 1024 * 1024;
const TIFF_SECOND_CAP: u64 = 32 * 1024 * 1024;
const XMP_SCAN_CAP: u64 = 1024 * 1024;
const XMP_MAX_LEN: usize = 2 * 1024 * 1024;
const MAX_TAG_VALUE_LEN: usize = 256;

/// Reads image metadata. Missing EXIF is not an error (fields are `None`); unreadable files
/// are.
pub fn read_image_metadata(path: &Path) -> Result<ImageMetadata> {
    let mut src = Src::open(path)?;
    let head = src.read_upto(0, 128)?;
    let fmt = detect_format(&head, &forge_core::model::ext_of(path));
    let jpeg_prefix = if fmt == ImageFormat::Jpeg {
        bin_util::read_jpeg_header_prefix(path).ok()
    } else {
        None
    };
    let jpeg_info = jpeg_prefix.as_deref().and_then(bin_util::parse_jpeg_header);
    let exifs = match &jpeg_prefix {
        Some(b) => jpeg_exif_from_prefix(b).into_iter().collect(),
        None => read_exifs(path, &mut src, fmt),
    };

    let mut m = ImageMetadata {
        format: Some(fmt),
        ..Default::default()
    };
    fill_from_exif(&mut m, &exifs);

    // Dimensions
    match fmt {
        f if f.is_raw() => {
            let (w, h, bits) = raw_dimensions(&mut src, fmt);
            let ex = m.width.zip(m.height);
            let best = [ex, w.zip(h)]
                .into_iter()
                .flatten()
                .max_by_key(|(w, h)| *w as u64 * *h as u64);
            if let Some((w, h)) = best {
                m.width = Some(w);
                m.height = Some(h);
            }
            m.bit_depth = bits.or(m.bit_depth);
        }
        ImageFormat::Heif | ImageFormat::Unknown => {}
        ImageFormat::Jpeg if jpeg_info.is_some() => {
            if let Some(i) = jpeg_info {
                m.width = Some(i.width);
                m.height = Some(i.height);
                m.bit_depth = Some(i.precision as u32);
            }
        }
        _ => {
            if let Ok((w, h, bits)) = header_dimensions(path) {
                m.width = Some(w);
                m.height = Some(h);
                if bits.is_some() {
                    m.bit_depth = bits;
                }
            }
        }
    }

    // XMP: sidecar wins field-by-field over the embedded packet.
    let embedded = match &jpeg_prefix {
        Some(b) => {
            let end = jpeg_info.map_or(b.len(), |i| i.sos_offset.min(b.len()));
            extract_xmp_packet(&b[..end])
        }
        None => read_embedded_xmp(&mut src, fmt),
    }
    .map(|x| parse_xmp(&x, XmpSource::Embedded));
    let sidecar = read_xmp_sidecar(path).ok().flatten();
    m.xmp = merge_xmp(sidecar, embedded);
    Ok(m)
}

/// EXIF orientation only (cheap; used by the thumbnail pipeline).
pub fn read_orientation(path: &Path) -> Option<u16> {
    let mut src = Src::open(path).ok()?;
    let head = src.read_upto(0, 128).ok()?;
    let fmt = detect_format(&head, &forge_core::model::ext_of(path));
    let exifs = read_exifs(path, &mut src, fmt);
    orientation_of(&exifs)
}

fn orientation_of(exifs: &[Exif]) -> Option<u16> {
    find(exifs, Tag::Orientation)
        .and_then(|f| f.value.get_uint(0))
        .filter(|o| (1..=8).contains(o))
        .map(|o| o as u16)
}

pub(crate) fn orientation_from_jpeg_bytes(b: &[u8]) -> Option<u16> {
    let e = exif_from_container(b)?;
    orientation_of(std::slice::from_ref(&e))
}

/// EXIF from an already-read JPEG header prefix (orientation + embedded thumbnail).
pub(crate) fn jpeg_exif_from_prefix(prefix: &[u8]) -> Option<Exif> {
    exif_from_container(prefix)
}

/// The EXIF IFD1 thumbnail of a parsed JPEG EXIF block, if any.
pub(crate) fn exif_thumbnail(exif: &Exif) -> Option<&[u8]> {
    let off = exif
        .get_field(Tag::JPEGInterchangeFormat, In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let len = exif
        .get_field(Tag::JPEGInterchangeFormatLength, In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    exif.buf().get(off..off.checked_add(len)?)
}

fn exif_reader() -> exif::Reader {
    let mut r = exif::Reader::new();
    r.continue_on_error(true);
    r
}

fn distill(r: std::result::Result<Exif, exif::Error>) -> Option<Exif> {
    r.or_else(|e| e.distill_partial_result(|_| {})).ok()
}

fn exif_from_container(b: &[u8]) -> Option<Exif> {
    distill(exif_reader().read_from_container(&mut Cursor::new(b)))
}

fn exif_from_tiff(b: Vec<u8>) -> Option<Exif> {
    distill(exif_reader().read_raw(b))
}

/// Wraps a TIFF block whose IFD0 really is an EXIF/GPS IFD (CR3 CMT2/CMT4) into a TIFF
/// whose IFD0 points at it, so kamadak interprets tag numbers in the right context.
fn wrap_child_ifd(mut d: Vec<u8>, pointer_tag: u16) -> Option<Vec<u8>> {
    let (le, ifd) = bin_util::tiff_header(&d)?;
    if d.len() % 2 == 1 {
        d.push(0);
    }
    let new_off = u32::try_from(d.len()).ok()?;
    let w16 = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
    let w32 = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
    d.extend_from_slice(&w16(1));
    d.extend_from_slice(&w16(pointer_tag));
    d.extend_from_slice(&w16(4));
    d.extend_from_slice(&w32(1));
    d.extend_from_slice(&w32(ifd));
    d.extend_from_slice(&w32(0));
    d[2..4].copy_from_slice(&w16(42));
    d[4..8].copy_from_slice(&w32(new_off));
    Some(d)
}

fn read_exifs(path: &Path, src: &mut Src, fmt: ImageFormat) -> Vec<Exif> {
    let mut out = Vec::new();
    match fmt {
        ImageFormat::Jpeg => {
            if let Ok(b) = bin_util::read_jpeg_header_prefix(path) {
                out.extend(jpeg_exif_from_prefix(&b));
            }
        }
        ImageFormat::Png | ImageFormat::WebP => {
            let blocks = if fmt == ImageFormat::Png {
                png_blocks(src)
            } else {
                webp_blocks(src)
            };
            out.extend(blocks.exif.and_then(exif_from_tiff));
        }
        ImageFormat::Heif => {
            // kamadak's ISO-BMFF walker seeks to the Exif item; it does not read the file.
            if let Ok(f) = std::fs::File::open(path) {
                let mut r = std::io::BufReader::new(f);
                out.extend(distill(exif_reader().read_from_container(&mut r)));
            }
        }
        ImageFormat::Tiff | ImageFormat::TiffRaw => {
            let parse = |cap: u64| -> Option<Exif> {
                let mut b = bin_util::read_prefix(path, cap).ok()?;
                // ORF ("RO"/"SR") and RW2 (0x55) use non-standard magic numbers.
                let (le, _) = bin_util::tiff_header(&b)?;
                let ft = if le { [42, 0] } else { [0, 42] };
                b[2..4].copy_from_slice(&ft);
                exif_from_tiff(b)
            };
            let len = src.len();
            let mut e = parse(TIFF_FIRST_CAP);
            let incomplete = e.as_ref().is_none_or(|e| {
                e.get_field(Tag::ExposureTime, In::PRIMARY).is_none()
                    && e.get_field(Tag::DateTimeOriginal, In::PRIMARY).is_none()
            });
            if incomplete
                && len > TIFF_FIRST_CAP
                && let Some(e2) = parse(TIFF_SECOND_CAP)
            {
                e = Some(e2);
            }
            out.extend(e);
            // RW2 & friends keep the full EXIF inside the embedded JPEG preview.
            if fmt == ImageFormat::TiffRaw
                && out
                    .iter()
                    .all(|e| e.get_field(Tag::ExposureTime, In::PRIMARY).is_none())
                && let Some(x) = preview_exif(src)
            {
                out.push(x);
            }
        }
        ImageFormat::Cr3 => {
            let l = raw_preview::cr3_layout(src);
            for (i, b) in l.cmt.iter().enumerate() {
                let Some(b) = b else { continue };
                if i == 2 || b.data_len > CONTAINER_CAP {
                    continue; // CMT3 = Canon maker notes (not decoded).
                }
                let Ok(d) = src.read_at(b.data_off, b.data_len) else {
                    continue;
                };
                let d = match i {
                    1 => wrap_child_ifd(d, 0x8769),
                    3 => wrap_child_ifd(d, 0x8825),
                    _ => Some(d),
                };
                out.extend(d.and_then(exif_from_tiff));
            }
        }
        ImageFormat::Raf => {
            out.extend(preview_exif(src));
        }
        ImageFormat::Gif | ImageFormat::Bmp | ImageFormat::Unknown => {}
    }
    out
}

/// EXIF from the largest embedded preview JPEG (reads only its first 256 KB).
fn preview_exif(src: &mut Src) -> Option<Exif> {
    let (cands, _) = raw_preview::candidates(src).ok()?;
    let c = cands
        .iter()
        .max_by_key(|c| c.width as u64 * c.height as u64)?;
    let b = src
        .read_upto(c.off, c.len.unwrap_or(u64::MAX).min(256 * 1024))
        .ok()?;
    exif_from_container(&b)
}

/// (width, height, bits) of the raw image for RAW formats, from TIFF IFDs (full-resolution
/// IFDs only) and the largest embedded preview.
fn raw_dimensions(src: &mut Src, fmt: ImageFormat) -> (Option<u32>, Option<u32>, Option<u32>) {
    let mut best: Option<(u32, u32)> = None;
    let mut bits = None;
    let consider = |w: u32, h: u32, best: &mut Option<(u32, u32)>| {
        if w > 0 && h > 0 && w <= 100_000 && h <= 100_000 {
            let a = w as u64 * h as u64;
            if best.is_none_or(|(bw, bh)| a > bw as u64 * bh as u64) {
                *best = Some((w, h));
            }
        }
    };
    if fmt == ImageFormat::TiffRaw
        && let Ok(t) = Tiff::parse(src, 0, false)
    {
        let mut best_raw_area = 0u64;
        for ifd in &t.ifds {
            let sub = t.uint(src, ifd, tag::NEW_SUBFILE_TYPE).unwrap_or(0);
            if sub != 0 {
                continue;
            }
            if let (Some(w), Some(h)) = (
                t.uint(src, ifd, tag::IMAGE_WIDTH),
                t.uint(src, ifd, tag::IMAGE_LENGTH),
            ) {
                consider(w, h, &mut best);
                let a = w as u64 * h as u64;
                if a > best_raw_area {
                    best_raw_area = a;
                    bits = t
                        .uint(src, ifd, tag::BITS_PER_SAMPLE)
                        .filter(|b| (1..=32).contains(b));
                }
            }
        }
    }
    if let Ok((cands, _)) = raw_preview::candidates(src) {
        for c in cands {
            consider(c.width, c.height, &mut best);
        }
    }
    (best.map(|b| b.0), best.map(|b| b.1), bits)
}

/// Header-only dimensions and bit depth via the `image` crate (never decodes pixels).
fn header_dimensions(path: &Path) -> Result<(u32, u32, Option<u32>)> {
    use image::ImageDecoder;
    let reader = image::ImageReader::open(path)
        .map_err(|e| MediaError::io(path, e))?
        .with_guessed_format()
        .map_err(|e| MediaError::io(path, e))?;
    let dec = reader.into_decoder()?;
    let (w, h) = dec.dimensions();
    let ct = dec.color_type();
    let ch = ct.channel_count().max(1) as u32;
    let bits = ct.bits_per_pixel() as u32 / ch;
    Ok((w, h, Some(bits)))
}

// ---------------------------------------------------------------------------
// EXIF field extraction
// ---------------------------------------------------------------------------

fn find(exifs: &[Exif], t: Tag) -> Option<&Field> {
    exifs.iter().find_map(|e| e.get_field(t, In::PRIMARY))
}

fn ascii(f: &Field) -> Option<String> {
    match &f.value {
        Value::Ascii(v) => {
            let s = v.first()?;
            let s = String::from_utf8_lossy(s);
            let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
            (!s.is_empty()).then(|| s.to_string())
        }
        _ => None,
    }
}

fn num(f: &Field) -> Option<f64> {
    let v = match &f.value {
        Value::Rational(v) => v.first().filter(|r| r.denom != 0).map(|r| r.to_f64()),
        Value::SRational(v) => v.first().filter(|r| r.denom != 0).map(|r| r.to_f64()),
        Value::Short(v) => v.first().map(|&x| x as f64),
        Value::Long(v) => v.first().map(|&x| x as f64),
        Value::SShort(v) => v.first().map(|&x| x as f64),
        Value::SLong(v) => v.first().map(|&x| x as f64),
        Value::Float(v) => v.first().map(|&x| x as f64),
        Value::Double(v) => v.first().copied(),
        _ => None,
    }?;
    v.is_finite().then_some(v)
}

fn rationals(f: &Field) -> Option<Vec<f64>> {
    match &f.value {
        Value::Rational(v) => Some(
            v.iter()
                .map(|r| if r.denom == 0 { f64::NAN } else { r.to_f64() })
                .collect(),
        ),
        _ => None,
    }
}

/// "1/250", "1/2.5", "2s", "1.3s".
pub fn format_shutter(t: f64) -> Option<String> {
    if !(t.is_finite() && t > 0.0) {
        return None;
    }
    if t >= 1.0 {
        let s = format!("{t:.1}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        return Some(format!("{s}s"));
    }
    let n = 1.0 / t;
    if (n - n.round()).abs() < 0.05 * n.max(1.0) || n >= 10.0 {
        Some(format!("1/{}", n.round() as u64))
    } else {
        let s = format!("{n:.1}");
        Some(format!("1/{}", s.trim_end_matches(".0")))
    }
}

fn fill_from_exif(m: &mut ImageMetadata, exifs: &[Exif]) {
    if exifs.is_empty() {
        return;
    }
    m.make = find(exifs, Tag::Make).and_then(ascii);
    m.model = find(exifs, Tag::Model).and_then(ascii);
    m.lens = find(exifs, Tag::LensModel).and_then(ascii);
    m.focal_length_mm = find(exifs, Tag::FocalLength)
        .and_then(num)
        .filter(|v| *v > 0.0);
    m.focal_length_35mm = find(exifs, Tag::FocalLengthIn35mmFilm)
        .and_then(|f| f.value.get_uint(0))
        .filter(|v| *v > 0);
    m.aperture = find(exifs, Tag::FNumber)
        .and_then(num)
        .filter(|v| *v > 0.0)
        .or_else(|| {
            find(exifs, Tag::ApertureValue)
                .and_then(num)
                .map(|av| 2f64.powf(av / 2.0))
                .filter(|v| v.is_finite() && *v > 0.0)
        })
        .map(|v| (v * 10.0).round() / 10.0);
    m.exposure_time_s = find(exifs, Tag::ExposureTime)
        .and_then(num)
        .filter(|v| *v > 0.0);
    m.shutter = m.exposure_time_s.and_then(format_shutter);
    m.iso = find(exifs, Tag::PhotographicSensitivity)
        .and_then(|f| f.value.get_uint(0))
        .or_else(|| find(exifs, Tag::ISOSpeed).and_then(|f| f.value.get_uint(0)))
        .filter(|v| *v > 0);
    m.exposure_compensation = find(exifs, Tag::ExposureBiasValue)
        .and_then(num)
        .map(|v| (v * 100.0).round() / 100.0);
    m.orientation = orientation_of(exifs);
    m.width = find(exifs, Tag::PixelXDimension).and_then(|f| f.value.get_uint(0));
    m.height = find(exifs, Tag::PixelYDimension).and_then(|f| f.value.get_uint(0));
    if m.width.is_none() {
        m.width = find(exifs, Tag::ImageWidth).and_then(|f| f.value.get_uint(0));
        m.height = find(exifs, Tag::ImageLength).and_then(|f| f.value.get_uint(0));
    }
    if m.width.is_none() || m.height.is_none() || m.width == Some(0) || m.height == Some(0) {
        m.width = None;
        m.height = None;
    }
    m.bit_depth = find(exifs, Tag::BitsPerSample)
        .and_then(|f| f.value.get_uint(0))
        .filter(|b| (1..=32).contains(b));

    // Color space: 1 = sRGB; 0xFFFF = uncalibrated (Adobe RGB if interop index says R03).
    m.color_space = find(exifs, Tag::ColorSpace)
        .and_then(|f| f.value.get_uint(0))
        .map(|cs| match cs {
            1 => "sRGB".to_string(),
            2 => "Adobe RGB".to_string(),
            0xFFFF => {
                let interop = exifs
                    .iter()
                    .find_map(|e| e.get_field(Tag::InteroperabilityIndex, In::PRIMARY))
                    .and_then(ascii);
                if interop.as_deref() == Some("R03") {
                    "Adobe RGB".to_string()
                } else {
                    "Uncalibrated".to_string()
                }
            }
            other => format!("Unknown ({other})"),
        });

    // Date taken
    let dt_field = find(exifs, Tag::DateTimeOriginal)
        .map(|f| (f, Tag::SubSecTimeOriginal, Tag::OffsetTimeOriginal))
        .or_else(|| {
            find(exifs, Tag::DateTimeDigitized)
                .map(|f| (f, Tag::SubSecTimeDigitized, Tag::OffsetTimeDigitized))
        })
        .or_else(|| find(exifs, Tag::DateTime).map(|f| (f, Tag::SubSecTime, Tag::OffsetTime)));
    if let Some((f, sub_t, off_t)) = dt_field
        && let Some(s) = ascii(f)
    {
        let sub = find(exifs, sub_t).and_then(ascii);
        let off = find(exifs, off_t).and_then(ascii);
        if let Some(dt) = parse_exif_datetime(&s, sub.as_deref(), off.as_deref()) {
            m.date_taken = Some(dt.iso);
            m.capture_timestamp = Some(dt.timestamp);
            m.capture_millis = dt.millis;
        }
    }

    // GPS
    let coord = |val: Tag, refr: Tag, pos: &str| -> Option<f64> {
        let v = rationals(find(exifs, val)?)?;
        let mut d = *v.first()?;
        if let Some(mn) = v.get(1) {
            d += mn / 60.0;
        }
        if let Some(s) = v.get(2) {
            d += s / 3600.0;
        }
        if !d.is_finite() {
            return None;
        }
        let r = find(exifs, refr)
            .and_then(ascii)
            .unwrap_or_else(|| pos.into());
        Some(if r.eq_ignore_ascii_case(pos) { d } else { -d })
    };
    let lat = coord(Tag::GPSLatitude, Tag::GPSLatitudeRef, "N").filter(|v| v.abs() <= 90.0);
    let lon = coord(Tag::GPSLongitude, Tag::GPSLongitudeRef, "E").filter(|v| v.abs() <= 180.0);
    if lat.is_some() && lon.is_some() {
        m.gps_lat = lat;
        m.gps_lon = lon;
        m.gps_alt_m = find(exifs, Tag::GPSAltitude).and_then(num).map(|a| {
            let below = find(exifs, Tag::GPSAltitudeRef).and_then(|f| f.value.get_uint(0));
            if below == Some(1) { -a } else { a }
        });
    }

    // Raw tag list for the details panel.
    for e in exifs {
        for f in e.fields() {
            if m.exif_tags.len() >= 2000 {
                break;
            }
            let value = match (&f.value, f.tag) {
                (_, Tag::MakerNote) => format!("({} bytes, not decoded)", value_len(&f.value)),
                (Value::Undefined(b, _), _) if b.len() > 64 => {
                    format!("({} bytes)", b.len())
                }
                (Value::Byte(b), _) if b.len() > 64 => format!("({} bytes)", b.len()),
                _ => {
                    let mut s = f.display_value().with_unit(e).to_string();
                    if s.len() > MAX_TAG_VALUE_LEN {
                        let mut cut = MAX_TAG_VALUE_LEN;
                        while !s.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        s.truncate(cut);
                        s.push('…');
                    }
                    s
                }
            };
            m.exif_tags.push(ExifTag {
                group: format!("{:?}/{}", f.tag.context(), f.ifd_num),
                name: f.tag.to_string(),
                value,
            });
        }
    }
}

fn value_len(v: &Value) -> usize {
    match v {
        Value::Undefined(b, _) | Value::Byte(b) => b.len(),
        _ => 0,
    }
}

pub(crate) struct ParsedDateTime {
    pub iso: String,
    pub timestamp: i64,
    pub millis: Option<u32>,
}

/// Parses "YYYY:MM:DD HH:MM:SS" (+ optional sub-seconds and "+HH:MM" offset).
pub(crate) fn parse_exif_datetime(
    s: &str,
    subsec: Option<&str>,
    offset: Option<&str>,
) -> Option<ParsedDateTime> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(b.get(r)?).ok()?.trim().parse().ok()
    };
    let (y, mo, d, h, mi, se) = (
        n(0..4)?,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    );
    if y == 0 || !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let subsec = subsec
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        .map(|s| &s[..s.len().min(9)]);
    let millis = subsec.and_then(|s| {
        let padded = format!("{s:0<3}");
        padded[..3].parse::<u32>().ok()
    });
    let off_secs = offset.and_then(|o| {
        let o = o.trim();
        let ob = o.as_bytes();
        if ob.len() < 6 || !matches!(ob[0], b'+' | b'-') || ob[3] != b':' {
            return None;
        }
        let hh: i64 = o.get(1..3)?.parse().ok()?;
        let mm: i64 = o.get(4..6)?.parse().ok()?;
        if hh > 14 || mm > 59 {
            return None;
        }
        let v = hh * 3600 + mm * 60;
        Some(if ob[0] == b'-' { -v } else { v })
    });
    let mut iso = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}");
    if let Some(s) = subsec {
        iso.push('.');
        iso.push_str(s);
    }
    if let (Some(o), Some(_)) = (offset, off_secs) {
        iso.push_str(&o.trim()[..6]);
    }
    let days = days_from_civil(y, mo, d);
    let ts = days * 86_400 + h * 3600 + mi * 60 + se - off_secs.unwrap_or(0);
    Some(ParsedDateTime {
        iso,
        timestamp: ts,
        millis,
    })
}

/// Howard Hinnant's days-from-civil algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// XMP
// ---------------------------------------------------------------------------

/// Scans the first few MB of the file for an `<x:xmpmeta>` packet.
/// Locates the XMP packet structurally where the container tells us where it is (TIFF tag
/// 700, CR3 XMP `uuid` box, PNG `iTXt`, WebP `XMP ` chunk); otherwise scans the first MB.
fn read_embedded_xmp(src: &mut Src, fmt: ImageFormat) -> Option<String> {
    let located: Option<Vec<u8>> = match fmt {
        ImageFormat::Tiff | ImageFormat::TiffRaw => (|| {
            let t = Tiff::parse(src, 0, false).ok()?;
            let e = *t.ifds.first()?.get(tag::XMP)?;
            if !matches!(e.typ, 1 | 2 | 7) || e.count as usize > XMP_MAX_LEN {
                return None;
            }
            match t.value_offset(&e) {
                Some(off) => src.read_at(off, e.count as u64).ok(),
                None => Some(e.raw[..e.count as usize].to_vec()),
            }
        })(),
        ImageFormat::Cr3 => (|| {
            let len = src.len();
            let top = bin_util::list_boxes(src, 0, len);
            let b = bin_util::find_uuid_box(src, &top, &bin_util::XMP_UUID)?;
            src.read_at(b.data_off, b.data_len.min(XMP_MAX_LEN as u64))
                .ok()
        })(),
        ImageFormat::Png => png_blocks(src).xmp,
        ImageFormat::WebP => webp_blocks(src).xmp,
        _ => None,
    };
    if let Some(x) = located.as_deref().and_then(extract_xmp_packet) {
        return Some(x);
    }
    let buf = src.read_upto(0, XMP_SCAN_CAP).ok()?;
    extract_xmp_packet(&buf)
}

#[derive(Default)]
struct Blocks {
    exif: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
}

/// Walks PNG chunks (seeking over pixel data) for `eXIf` and the XMP `iTXt` chunk.
fn png_blocks(src: &mut Src) -> Blocks {
    let mut out = Blocks::default();
    let mut pos = 8u64;
    for _ in 0..100_000 {
        let Ok(h) = src.read_at(pos, 8) else { break };
        let len = bin_util::be32(&h, 0).unwrap_or(0) as u64;
        let typ = &h[4..8];
        let data_off = pos + 8;
        match typ {
            b"IEND" => break,
            b"eXIf" if out.exif.is_none() && len <= MAX_BLOCK => {
                out.exif = src.read_at(data_off, len).ok();
            }
            b"iTXt" if out.xmp.is_none() && len <= MAX_BLOCK => {
                if let Ok(d) = src.read_at(data_off, len)
                    && d.starts_with(b"XML:com.adobe.xmp\0")
                {
                    // keyword\0 compression-flag method lang\0 translated\0 text
                    let compressed = d.get(18).copied().unwrap_or(1) != 0;
                    if !compressed {
                        out.xmp = Some(d);
                    }
                }
            }
            _ => {}
        }
        if out.exif.is_some() && out.xmp.is_some() {
            break;
        }
        pos = match data_off.checked_add(len + 4) {
            Some(p) => p,
            None => break,
        };
    }
    out
}

/// Walks WebP RIFF chunks for `EXIF` and `XMP `.
fn webp_blocks(src: &mut Src) -> Blocks {
    let mut out = Blocks::default();
    let end = src.len();
    let mut pos = 12u64;
    for _ in 0..10_000 {
        if pos + 8 > end {
            break;
        }
        let Ok(h) = src.read_at(pos, 8) else { break };
        let len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as u64;
        let data_off = pos + 8;
        match &h[0..4] {
            b"EXIF" if len <= MAX_BLOCK => {
                out.exif = src.read_at(data_off, len).ok().map(|mut d| {
                    if d.starts_with(b"Exif\0\0") {
                        d.drain(..6);
                    }
                    d
                });
            }
            b"XMP " if len <= MAX_BLOCK => out.xmp = src.read_at(data_off, len).ok(),
            _ => {}
        }
        pos = data_off + len + (len & 1);
    }
    out
}

pub(crate) fn extract_xmp_packet(buf: &[u8]) -> Option<String> {
    let mut from = 0;
    let start = loop {
        let at = from + find_bytes(&buf[from..], b"<x:x")?;
        let tail = &buf[at + 4..];
        if tail.starts_with(b"mpmeta") || tail.starts_with(b"apmeta") {
            break at;
        }
        from = at + 1;
    };
    let rest = &buf[start..];
    let end = find_bytes(rest, b"</x:xmpmeta>")
        .map(|e| e + 12)
        .or_else(|| find_bytes(rest, b"</x:xapmeta>").map(|e| e + 12))
        .unwrap_or(rest.len().min(XMP_MAX_LEN));
    let end = end.min(XMP_MAX_LEN);
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

fn find_bytes(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    let mut i = 0;
    while i + n.len() <= h.len() {
        let at = i + h[i..=h.len() - n.len()].iter().position(|&b| b == n[0])?;
        if h[at..].starts_with(n) {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

/// Candidate sidecar paths: `name.xmp` (Lightroom/C1) and `name.ext.xmp` (darktable).
pub fn sidecar_paths(path: &Path) -> Vec<PathBuf> {
    let mut v = vec![path.with_extension("xmp")];
    let mut s = path.as_os_str().to_owned();
    s.push(".xmp");
    v.push(PathBuf::from(s));
    v
}

/// Reads and parses an XMP sidecar next to `path`, if one exists.
pub fn read_xmp_sidecar(path: &Path) -> Result<Option<XmpData>> {
    if forge_core::model::ext_of(path) == "xmp" {
        return Ok(None);
    }
    for p in sidecar_paths(path) {
        if p.is_file() {
            let b = bin_util::read_prefix(&p, XMP_MAX_LEN as u64)?;
            let s = String::from_utf8_lossy(&b);
            return Ok(Some(parse_xmp(&s, XmpSource::Sidecar)));
        }
    }
    Ok(None)
}

fn merge_xmp(primary: Option<XmpData>, secondary: Option<XmpData>) -> Option<XmpData> {
    match (primary, secondary) {
        (None, None) => None,
        (Some(p), None) => Some(p),
        (None, Some(s)) => (!s.is_empty()).then_some(s),
        (Some(mut p), Some(s)) => {
            p.rating = p.rating.or(s.rating);
            p.label = p.label.or(s.label);
            if p.keywords.is_empty() {
                p.keywords = s.keywords;
            }
            p.title = p.title.or(s.title);
            p.description = p.description.or(s.description);
            p.creator = p.creator.or(s.creator);
            Some(p)
        }
    }
}

/// Tolerant XMP parser for the handful of properties we need. Handles both attribute
/// (`xmp:Rating="3"`) and element (`<xmp:Rating>3</xmp:Rating>`) forms and RDF
/// Bag/Seq/Alt containers. Assumes the conventional prefixes (`xmp`, `dc`, `rdf`), which is
/// what every mainstream writer emits.
pub fn parse_xmp(xml: &str, source: XmpSource) -> XmpData {
    let rating = prop(xml, "xmp:Rating")
        .or_else(|| prop(xml, "xap:Rating"))
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|r| r.is_finite())
        .map(|r| (r.round() as i32).clamp(-1, 5));
    let label = prop(xml, "xmp:Label")
        .or_else(|| prop(xml, "xap:Label"))
        .filter(|s| !s.trim().is_empty());
    let mut keywords: Vec<String> = element_body(xml, "dc:subject")
        .map(li_items)
        .unwrap_or_default();
    if keywords.is_empty()
        && let Some(s) = attr(xml, "dc:subject")
    {
        keywords = s.split(',').map(|k| k.trim().to_string()).collect();
    }
    keywords.retain(|k| !k.is_empty());
    keywords.dedup();
    let alt = |name: &str| -> Option<String> {
        if let Some(body) = element_body(xml, name) {
            let items = li_items_lang(body);
            items
                .iter()
                .find(|(l, _)| l.as_deref() == Some("x-default"))
                .or(items.first())
                .map(|(_, v)| v.clone())
                .or_else(|| {
                    let t = decode_entities(body.trim());
                    (!t.is_empty() && !t.contains('<')).then_some(t)
                })
        } else {
            attr(xml, name)
        }
        .filter(|s| !s.trim().is_empty())
    };
    XmpData {
        rating,
        label,
        keywords,
        title: alt("dc:title"),
        description: alt("dc:description"),
        creator: element_body(xml, "dc:creator")
            .and_then(|b| li_items(b).into_iter().next())
            .or_else(|| attr(xml, "dc:creator")),
        source,
    }
}

/// Property value in attribute or simple element form.
fn prop(xml: &str, name: &str) -> Option<String> {
    attr(xml, name).or_else(|| {
        let b = element_body(xml, name)?;
        let t = decode_entities(b.trim());
        (!t.contains('<')).then_some(t)
    })
}

fn attr(xml: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = xml.get(from..)?.find(name) {
        let at = from + i;
        from = at + name.len();
        let before_ok = at == 0
            || xml[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace());
        let rest = xml[from..].trim_start();
        if before_ok && let Some(r) = rest.strip_prefix('=') {
            let r = r.trim_start();
            let q = r.chars().next()?;
            if q == '"' || q == '\'' {
                let body = &r[1..];
                let end = body.find(q)?;
                return Some(decode_entities(&body[..end]));
            }
        }
    }
    None
}

/// Inner text of the first `<name ...>...</name>` element (not self-closing).
fn element_body<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut from = 0;
    while let Some(i) = xml.get(from..)?.find(&open) {
        let at = from + i;
        let after = at + open.len();
        from = after;
        let next = xml[after..].chars().next()?;
        if !(next == '>' || next.is_whitespace() || next == '/') {
            continue; // e.g. <dc:subjectFoo
        }
        let gt = after + xml[after..].find('>')?;
        if xml[..gt].ends_with('/') {
            continue; // self-closing
        }
        let end = gt + 1 + xml[gt + 1..].find(&close)?;
        return Some(&xml[gt + 1..end]);
    }
    None
}

fn li_items(body: &str) -> Vec<String> {
    li_items_lang(body).into_iter().map(|(_, v)| v).collect()
}

fn li_items_lang(body: &str) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("<rdf:li") {
        if out.len() >= 1000 {
            break;
        }
        let after = &rest[i + 7..];
        let Some(gt) = after.find('>') else { break };
        let tag = &after[..gt];
        if tag.ends_with('/') {
            rest = &after[gt + 1..];
            continue;
        }
        let lang = attr(tag, "xml:lang");
        let content = &after[gt + 1..];
        let Some(end) = content.find("</rdf:li>") else {
            break;
        };
        let v = decode_entities(content[..end].trim());
        out.push((lang, v));
        rest = &content[end + 9..];
    }
    out
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let semi = after.find(';').filter(|&p| p <= 10);
        let decoded = semi.and_then(|p| {
            let ent = &after[..p];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            }?;
            Some((c, p))
        });
        match decoded {
            Some((c, p)) => {
                out.push(c);
                rest = &after[p + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutter_formatting() {
        assert_eq!(format_shutter(1.0 / 250.0).unwrap(), "1/250");
        assert_eq!(format_shutter(0.4).unwrap(), "1/2.5");
        assert_eq!(format_shutter(0.5).unwrap(), "1/2");
        assert_eq!(format_shutter(2.0).unwrap(), "2s");
        assert_eq!(format_shutter(1.3).unwrap(), "1.3s");
        assert_eq!(format_shutter(1.0 / 8000.0).unwrap(), "1/8000");
        assert!(format_shutter(0.0).is_none());
        assert!(format_shutter(f64::NAN).is_none());
    }

    #[test]
    fn datetime_parsing() {
        let d = parse_exif_datetime("2024:05:01 12:34:56", Some("12"), Some("+02:00")).unwrap();
        assert_eq!(d.iso, "2024-05-01T12:34:56.12+02:00");
        assert_eq!(d.timestamp, 1_714_559_696);
        assert_eq!(d.millis, Some(120));
        let d = parse_exif_datetime("1970:01:01 00:00:00", None, None).unwrap();
        assert_eq!(d.timestamp, 0);
        assert!(parse_exif_datetime("0000:00:00 00:00:00", None, None).is_none());
        assert!(parse_exif_datetime("garbage", None, None).is_none());
        assert!(parse_exif_datetime("2024:05:01 1€:34:56", None, None).is_none());
    }

    #[test]
    fn xmp_parsing_attribute_and_element_forms() {
        let x = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF><rdf:Description
            xmp:Rating="4" xmp:Label="Red &amp; Hot">
            <dc:subject><rdf:Bag><rdf:li>beach</rdf:li><rdf:li>sun &lt;3</rdf:li></rdf:Bag></dc:subject>
            <dc:title><rdf:Alt><rdf:li xml:lang="de">Titel</rdf:li><rdf:li xml:lang="x-default">Title</rdf:li></rdf:Alt></dc:title>
            <dc:description><rdf:Alt><rdf:li xml:lang="x-default">Desc</rdf:li></rdf:Alt></dc:description>
            </rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let d = parse_xmp(x, XmpSource::Embedded);
        assert_eq!(d.rating, Some(4));
        assert_eq!(d.label.as_deref(), Some("Red & Hot"));
        assert_eq!(d.keywords, vec!["beach", "sun <3"]);
        assert_eq!(d.title.as_deref(), Some("Title"));
        assert_eq!(d.description.as_deref(), Some("Desc"));

        let x2 = "<x:xmpmeta><xmp:Rating>-1</xmp:Rating><xmp:Label>Blue</xmp:Label></x:xmpmeta>";
        let d = parse_xmp(x2, XmpSource::Sidecar);
        assert_eq!(d.rating, Some(-1));
        assert_eq!(d.label.as_deref(), Some("Blue"));
        // Garbage never panics.
        for s in [
            "",
            "<",
            "<dc:subject>",
            "<rdf:li",
            "xmp:Rating=",
            "xmp:Rating=\"",
            "&#xFFFFFFFF;",
            "&#99999999999;",
        ] {
            let _ = parse_xmp(s, XmpSource::Embedded);
        }
    }

    #[test]
    fn xmp_packet_extraction() {
        let mut buf = vec![0u8; 100];
        buf.extend_from_slice(b"<x:xmpmeta a='1'><xmp:Rating>2</xmp:Rating></x:xmpmeta>trailing");
        let p = extract_xmp_packet(&buf).unwrap();
        assert!(p.ends_with("</x:xmpmeta>"));
        assert!(extract_xmp_packet(b"nothing here").is_none());
    }

    #[test]
    fn wrap_child_ifd_roundtrip() {
        // Minimal TIFF whose IFD0 contains ExposureTime (EXIF-context tag 0x829A).
        let mut d = b"II*\0\x08\0\0\0".to_vec();
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&0x829Au16.to_le_bytes());
        d.extend_from_slice(&5u16.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&26u32.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&250u32.to_le_bytes());
        let w = wrap_child_ifd(d, 0x8769).unwrap();
        let e = exif_from_tiff(w).unwrap();
        let f = e.get_field(Tag::ExposureTime, In::PRIMARY).unwrap();
        assert_eq!(num(f), Some(1.0 / 250.0));
    }
}
