//! Synchronous decode helpers run on the viewer's worker pool.
//!
//! * JPEG/PNG/TIFF/WebP/GIF/BMP via the `image` crate with dimension + allocation limits.
//! * RAW via the embedded JPEG preview (`forge_media::raw_preview`).
//! * Video via a representative frame from ffmpeg (if installed).
//!
//! EXIF orientation is applied to the decoded pixels only (never written back).

use std::io::Read;
use std::path::Path;

use forge_core::model::{MediaKind, ext_of, media_kind_for_ext};
use forge_jobs::CancelToken;
use forge_media::image_meta::{ImageFormat, detect_format};
use forge_media::raw_preview::{PreviewPick, extract_preview};
use forge_media::{DecodeLimits, FfTools, MediaError};
use image::{DynamicImage, ImageDecoder, ImageReader, Limits, RgbaImage};

/// Decoded pixels plus the natural (oriented, full-resolution) size of the source.
pub struct Decoded {
    pub rgba: RgbaImage,
    pub natural: [u32; 2],
    pub video: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// A video, but ffmpeg is not available.
    NoFfmpeg,
    /// Recognised but not decodable here (HEIC, unknown formats, RAW without preview…).
    Unsupported(String),
    /// Corrupt/truncated/unreadable.
    Failed(String),
    Cancelled,
}

impl From<MediaError> for DecodeError {
    fn from(e: MediaError) -> Self {
        match e {
            MediaError::ToolMissing(_) => DecodeError::NoFfmpeg,
            MediaError::Cancelled => DecodeError::Cancelled,
            MediaError::Unsupported(s) => DecodeError::Unsupported(s),
            MediaError::Missing(what) => DecodeError::Unsupported(format!("no {what} found")),
            other => DecodeError::Failed(other.to_string()),
        }
    }
}

impl From<image::ImageError> for DecodeError {
    fn from(e: image::ImageError) -> Self {
        MediaError::from(e).into()
    }
}

pub fn is_video(path: &Path) -> bool {
    media_kind_for_ext(&ext_of(path)) == MediaKind::Video
}

pub fn is_jpeg(path: &Path) -> bool {
    matches!(ext_of(path).as_str(), "jpg" | "jpeg" | "jpe")
}

/// Whether the JPEG has a dimensionally suitable EXIF thumbnail for a Quick decode.
/// The thumbnail bytes themselves are decoded by forge_media's shared thumbnail pipeline.
pub fn has_jpeg_thumbnail(path: &Path, min_edge: u32) -> bool {
    if !is_jpeg(path) {
        return false;
    }
    let Ok(metadata) = forge_media::read_image_metadata(path) else {
        return false;
    };
    let (Some(main_w), Some(main_h)) = (metadata.width, metadata.height) else {
        return false;
    };
    let ifd1_value = |name: &str| {
        metadata
            .exif_tags
            .iter()
            .find(|tag| tag.name == name && tag.group.ends_with("/1"))
            .and_then(|tag| {
                let digits: String = tag
                    .value
                    .trim_start()
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                digits.parse::<u32>().ok()
            })
    };
    if ifd1_value("JPEGInterchangeFormat").is_none()
        || ifd1_value("JPEGInterchangeFormatLength").is_none()
    {
        return false;
    }
    let (Some(thumb_w), Some(thumb_h)) = (ifd1_value("ImageWidth"), ifd1_value("ImageLength"))
    else {
        return false;
    };
    if thumb_w == 0 || thumb_h == 0 || main_w == 0 || main_h == 0 {
        return false;
    }
    let thumb_ar = thumb_w as f64 / thumb_h as f64;
    let main_ar = main_w as f64 / main_h as f64;
    thumb_w.max(thumb_h) >= min_edge && (thumb_ar / main_ar - 1.0).abs() < 0.03
}

/// Estimated decoded RGBA bytes, read from the image header before starting a decode.
/// Unknown formats are charged as an exclusive decode by returning `u64::MAX`.
pub fn estimated_input_bytes(path: &Path, video_edge: u32) -> u64 {
    if is_video(path) {
        let edge = video_edge.clamp(64, 4096) as u64;
        return edge.saturating_mul(edge).saturating_mul(4);
    }
    let Ok(reader) = ImageReader::open(path) else {
        return u64::MAX;
    };
    let Ok(reader) = reader.with_guessed_format() else {
        return u64::MAX;
    };
    reader
        .into_dimensions()
        .map(|(w, h)| (w as u64).saturating_mul(h as u64).saturating_mul(4))
        .unwrap_or(u64::MAX)
}

/// Makes a Quick tier from an already decoded Screen tier without cloning the Screen pixels.
pub fn quick_from_screen(screen: &Decoded, max_edge: u32) -> Decoded {
    let (w, h) = screen.rgba.dimensions();
    let long = w.max(h);
    let rgba = if long <= max_edge {
        screen.rgba.clone()
    } else {
        let scale = max_edge as f64 / long as f64;
        let tw = ((w as f64 * scale).round() as u32).clamp(1, max_edge);
        let th = ((h as f64 * scale).round() as u32).clamp(1, max_edge);
        image::imageops::resize(&screen.rgba, tw, th, image::imageops::FilterType::Triangle)
    };
    Decoded {
        rgba,
        natural: screen.natural,
        video: screen.video,
    }
}

fn check(cancel: &CancelToken) -> Result<(), DecodeError> {
    if cancel.is_cancelled() {
        Err(DecodeError::Cancelled)
    } else {
        Ok(())
    }
}

fn limits_for(l: &DecodeLimits) -> Limits {
    let mut lim = Limits::default();
    lim.max_image_width = Some(65_535 * 4);
    lim.max_image_height = Some(65_535 * 4);
    lim.max_alloc = Some(l.max_alloc_bytes);
    lim
}

fn check_pixels(w: u32, h: u32, l: &DecodeLimits) -> Result<(), DecodeError> {
    if w == 0 || h == 0 {
        return Err(DecodeError::Failed("zero image dimension".into()));
    }
    if w as u64 * h as u64 > l.max_input_pixels {
        return Err(DecodeError::Failed(format!(
            "image too large ({w}×{h}) — exceeds the decode limit"
        )));
    }
    Ok(())
}

fn decode_reader<R: std::io::BufRead + std::io::Seek>(
    mut r: ImageReader<R>,
    l: &DecodeLimits,
) -> Result<(DynamicImage, u8), DecodeError> {
    r.limits(limits_for(l));
    let mut dec = r.into_decoder()?;
    let (w, h) = dec.dimensions();
    check_pixels(w, h, l)?;
    let o = dec.orientation().map(|o| o.to_exif()).unwrap_or(1);
    Ok((DynamicImage::from_decoder(dec)?, o))
}

fn orient(img: &mut DynamicImage, o: u8) {
    if o != 1
        && let Some(o) = image::metadata::Orientation::from_exif(o)
    {
        img.apply_orientation(o);
    }
}

/// Decodes `path`, downscaled so the long edge is ≤ `max_edge` (`None` = full size).
pub fn decode(
    path: &Path,
    max_edge: Option<u32>,
    limits: &DecodeLimits,
    ff: &FfTools,
    cancel: &CancelToken,
) -> Result<Decoded, DecodeError> {
    check(cancel)?;
    if is_video(path) {
        let edge = max_edge.unwrap_or(4096).clamp(64, 4096);
        let frame = ff.representative_frame(path, edge, 5, Some(cancel))?;
        let natural = [frame.width(), frame.height()];
        return Ok(Decoded {
            rgba: frame,
            natural,
            video: true,
        });
    }

    let mut head = [0u8; 128];
    let n = {
        let mut f = std::fs::File::open(path).map_err(|e| DecodeError::Failed(e.to_string()))?;
        let mut n = 0;
        while n < head.len() {
            match f.read(&mut head[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) => return Err(DecodeError::Failed(e.to_string())),
            }
        }
        n
    };
    if n == 0 {
        return Err(DecodeError::Failed("empty file".into()));
    }
    let fmt = detect_format(&head[..n], &ext_of(path));
    let (mut img, o) = match fmt {
        f if f.is_raw() => {
            let pick = match max_edge {
                Some(e) => PreviewPick::AtLeast(e),
                None => PreviewPick::Largest,
            };
            let p = extract_preview(path, pick)?;
            check(cancel)?;
            check_pixels(p.width, p.height, limits)?;
            let mut r = ImageReader::new(std::io::Cursor::new(p.jpeg));
            r.set_format(image::ImageFormat::Jpeg);
            let (img, _) = decode_reader(r, limits)?;
            (img, p.orientation.clamp(1, 8) as u8)
        }
        ImageFormat::Heif => {
            return Err(DecodeError::Unsupported(
                "HEIC/HEIF/AVIF preview is not available yet".into(),
            ));
        }
        _ => {
            let r = ImageReader::open(path)
                .map_err(|e| DecodeError::Failed(e.to_string()))?
                .with_guessed_format()
                .map_err(|e| DecodeError::Failed(e.to_string()))?;
            if r.format().is_none() {
                return Err(DecodeError::Unsupported("unrecognised image format".into()));
            }
            decode_reader(r, limits)?
        }
    };
    check(cancel)?;
    orient(&mut img, o);
    let natural = [img.width(), img.height()];
    let rgba = match max_edge {
        Some(e) if natural[0].max(natural[1]) > e => forge_media::thumbs::fit(img, e),
        _ => img.into_rgba8(),
    };
    Ok(Decoded {
        rgba,
        natural,
        video: false,
    })
}

/// Thumbnail-sized decode via the shared media pipeline (fast paths: EXIF thumbnails,
/// small RAW previews).
pub fn thumbnail(
    path: &Path,
    size: u32,
    limits: &DecodeLimits,
    ff: &FfTools,
    cancel: &CancelToken,
) -> Result<Decoded, DecodeError> {
    check(cancel)?;
    let t = forge_media::generate_thumbnail(path, size, limits, Some(ff), 3, Some(cancel))?;
    let natural = [t.width, t.height];
    let rgba = RgbaImage::from_raw(t.width, t.height, t.rgba)
        .ok_or_else(|| DecodeError::Failed("thumbnail buffer size mismatch".into()))?;
    Ok(Decoded {
        rgba,
        natural,
        video: is_video(path),
    })
}

/// Downscales to fit a pixel budget (used to bound full-size decodes).
pub fn bound_pixels(d: Decoded, max_pixels: u64) -> Decoded {
    let (w, h) = (d.rgba.width() as u64, d.rgba.height() as u64);
    if w * h <= max_pixels || max_pixels == 0 {
        return d;
    }
    let k = (max_pixels as f64 / (w * h) as f64).sqrt();
    let edge = ((w.max(h) as f64) * k).floor().max(1.0) as u32;
    Decoded {
        rgba: forge_media::thumbs::fit(DynamicImage::ImageRgba8(d.rgba), edge),
        ..d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_png_and_reports_corrupt() {
        let dir = std::env::temp_dir().join(format!("zx_viewer_dec_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("g.png");
        let img = RgbaImage::from_fn(300, 200, |x, y| image::Rgba([x as u8, y as u8, 0, 255]));
        img.save(&p).unwrap();
        let ff = FfTools::default();
        let c = CancelToken::new();
        let l = DecodeLimits::default();
        let d = decode(&p, Some(100), &l, &ff, &c).unwrap();
        assert_eq!(d.natural, [300, 200]);
        assert_eq!(d.rgba.dimensions(), (100, 67));
        let d = decode(&p, None, &l, &ff, &c).unwrap();
        assert_eq!(d.rgba.dimensions(), (300, 200));
        assert_eq!(estimated_input_bytes(&p, 2560), 300 * 200 * 4);

        let quick = quick_from_screen(
            &Decoded {
                rgba: RgbaImage::new(1200, 600),
                natural: [1200, 600],
                video: false,
            },
            512,
        );
        assert_eq!(quick.rgba.dimensions(), (512, 256));
        assert_eq!(quick.natural, [1200, 600]);

        let bad = dir.join("bad.jpg");
        std::fs::write(&bad, [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]).unwrap();
        assert!(matches!(
            decode(&bad, Some(100), &l, &ff, &c),
            Err(DecodeError::Failed(_))
        ));
        let empty = dir.join("empty.png");
        std::fs::write(&empty, []).unwrap();
        assert!(decode(&empty, Some(100), &l, &ff, &c).is_err());

        c.cancel();
        assert_eq!(
            decode(&p, None, &l, &ff, &c).err(),
            Some(DecodeError::Cancelled)
        );

        let big = bound_pixels(
            Decoded {
                rgba: RgbaImage::new(400, 200),
                natural: [400, 200],
                video: false,
            },
            20_000,
        );
        assert!(big.rgba.width() as u64 * big.rgba.height() as u64 <= 20_000);
        assert_eq!(big.natural, [400, 200]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
