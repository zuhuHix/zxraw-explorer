//! Text or image watermarks stamped onto exported images.
//!
//! The mark's width is `size_pct` percent of the image width (shrunk further if it would be
//! taller than the image), it sits in one of nine anchor positions inset by `margin_pct`
//! percent of the image's short edge, and it is blended with `opacity`.

use std::path::PathBuf;
use std::sync::OnceLock;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use image::{DynamicImage, Rgba, RgbaImage, imageops::FilterType};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Anchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    #[default]
    BottomRight,
}

impl Anchor {
    /// Row-major order, so a 3 x 3 grid of buttons can be laid out from it.
    pub const ALL: [Self; 9] = [
        Self::TopLeft,
        Self::Top,
        Self::TopRight,
        Self::Left,
        Self::Center,
        Self::Right,
        Self::BottomLeft,
        Self::Bottom,
        Self::BottomRight,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::TopLeft => "Top left",
            Self::Top => "Top",
            Self::TopRight => "Top right",
            Self::Left => "Left",
            Self::Center => "Center",
            Self::Right => "Right",
            Self::BottomLeft => "Bottom left",
            Self::Bottom => "Bottom",
            Self::BottomRight => "Bottom right",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum WatermarkSource {
    Text { text: String },
    /// A PNG (or any decodable image); its alpha channel is respected.
    Image { path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Watermark {
    pub source: WatermarkSource,
    pub anchor: Anchor,
    /// Mark width as a percentage of the image width, 1–100.
    pub size_pct: f32,
    /// 0.0 (invisible) – 1.0 (opaque).
    pub opacity: f32,
    /// Inset from the edges as a percentage of the image's short edge, 0–25.
    pub margin_pct: f32,
}

impl Default for Watermark {
    fn default() -> Self {
        Self {
            source: WatermarkSource::Text {
                text: "© Your Name".into(),
            },
            anchor: Anchor::BottomRight,
            size_pct: 20.0,
            opacity: 0.6,
            margin_pct: 3.0,
        }
    }
}

/// Size of the mark on an `img_w` x `img_h` image for a mark whose natural size is
/// `mark_w` x `mark_h`. Keeps the mark's aspect ratio; never larger than the image; at
/// least 1 x 1.
pub fn mark_size(img_w: u32, img_h: u32, mark_w: u32, mark_h: u32, size_pct: f32) -> (u32, u32) {
    if img_w == 0 || img_h == 0 || mark_w == 0 || mark_h == 0 {
        return (1, 1);
    }
    let pct = f64::from(size_pct.clamp(1.0, 100.0)) / 100.0;
    let mut w = f64::from(img_w) * pct;
    let mut h = w * f64::from(mark_h) / f64::from(mark_w);
    if h > f64::from(img_h) {
        let scale = f64::from(img_h) / h;
        w *= scale;
        h = f64::from(img_h);
    }
    let w = (w.round() as u32).clamp(1, img_w);
    let h = (h.round() as u32).clamp(1, img_h);
    (w, h)
}

/// Top-left corner of a `mark_w` x `mark_h` mark on an `img_w` x `img_h` image.
pub fn placement(
    img_w: u32,
    img_h: u32,
    mark_w: u32,
    mark_h: u32,
    anchor: Anchor,
    margin_pct: f32,
) -> (i64, i64) {
    let short_edge = f64::from(img_w.min(img_h));
    let margin = (short_edge * f64::from(margin_pct.clamp(0.0, 25.0)) / 100.0).round() as i64;
    let (iw, ih) = (i64::from(img_w), i64::from(img_h));
    let (mw, mh) = (i64::from(mark_w), i64::from(mark_h));

    let x = match anchor {
        Anchor::TopLeft | Anchor::Left | Anchor::BottomLeft => margin,
        Anchor::Top | Anchor::Center | Anchor::Bottom => (iw - mw) / 2,
        Anchor::TopRight | Anchor::Right | Anchor::BottomRight => iw - mw - margin,
    };
    let y = match anchor {
        Anchor::TopLeft | Anchor::Top | Anchor::TopRight => margin,
        Anchor::Left | Anchor::Center | Anchor::Right => (ih - mh) / 2,
        Anchor::BottomLeft | Anchor::Bottom | Anchor::BottomRight => ih - mh - margin,
    };
    (x.max(0), y.max(0))
}

/// Font size, in pixels, used to measure a text mark before it is sized for an image.
const REFERENCE_PX: f32 = 100.0;
/// Windows fonts to try, in order, from `%WINDIR%\Fonts`.
const FONT_FILES: [&str; 4] = ["segoeuib.ttf", "segoeui.ttf", "arialbd.ttf", "arial.ttf"];
/// Font file bytes, read once for the whole process.
static FONT_BYTES: OnceLock<Option<Vec<u8>>> = OnceLock::new();

fn font_bytes() -> Option<&'static [u8]> {
    FONT_BYTES
        .get_or_init(|| {
            let windir = std::env::var_os("WINDIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
            let fonts = windir.join("Fonts");
            FONT_FILES
                .iter()
                .find_map(|name| std::fs::read(fonts.join(name)).ok())
        })
        .as_deref()
}

/// A watermark ready to apply: fonts or the logo are loaded once, then reused per image.
pub struct PreparedWatermark {
    mark: Mark,
    anchor: Anchor,
    size_pct: f32,
    opacity: f32,
    margin_pct: f32,
}

enum Mark {
    Text {
        font: FontVec,
        text: String,
        /// Ink size of the text at `REFERENCE_PX`.
        ref_w: u32,
        ref_h: u32,
    },
    Logo(RgbaImage),
}

/// Coverage (0.0–1.0) of text ink, one value per pixel, row-major.
struct Coverage {
    w: u32,
    h: u32,
    data: Vec<f32>,
}

impl PreparedWatermark {
    /// Loads the font (text) or decodes the logo (image).
    pub fn prepare(watermark: &Watermark) -> Result<Self, String> {
        let mark = match &watermark.source {
            WatermarkSource::Text { text } => {
                let text = text.trim();
                if text.is_empty() {
                    return Err("Watermark text is empty.".into());
                }
                let bytes = font_bytes().ok_or("No font found for the watermark text.")?;
                let font = FontVec::try_from_vec(bytes.to_vec())
                    .map_err(|_| "Could not parse the watermark font.".to_string())?;
                let reference = rasterize(&font, text, REFERENCE_PX);
                if reference.w == 0 || reference.h == 0 {
                    return Err("Watermark text has no visible glyphs.".into());
                }
                Mark::Text {
                    font,
                    text: text.to_string(),
                    ref_w: reference.w,
                    ref_h: reference.h,
                }
            }
            WatermarkSource::Image { path } => {
                let logo = image::open(path)
                    .map_err(|e| {
                        format!("Could not open watermark image {}: {e}", path.display())
                    })?
                    .to_rgba8();
                if logo.width() == 0 || logo.height() == 0 {
                    return Err(format!("Watermark image {} is empty.", path.display()));
                }
                Mark::Logo(logo)
            }
        };
        Ok(Self {
            mark,
            anchor: watermark.anchor,
            size_pct: watermark.size_pct,
            opacity: watermark.opacity,
            margin_pct: watermark.margin_pct,
        })
    }

    /// Renders the mark at its final pixel size for an `img_w` x `img_h` target. Fails
    /// instead of aborting when the mark's buffers cannot be allocated.
    fn render(&self, img_w: u32, img_h: u32) -> Result<RgbaImage, String> {
        match &self.mark {
            Mark::Text {
                font,
                text,
                ref_w,
                ref_h,
            } => {
                let (target_w, target_h) = mark_size(img_w, img_h, *ref_w, *ref_h, self.size_pct);
                let px = REFERENCE_PX * target_w as f32 / *ref_w as f32;
                // Coverage (f32) plus the RGBA mark, with room for glyph overhang and shadow.
                let pad = (px / 10.0).ceil() as u64 + 2;
                ensure_alloc((u64::from(target_w) + pad) * (u64::from(target_h) + pad) * 8)?;
                Ok(text_mark(&rasterize(font, text, px), px))
            }
            Mark::Logo(logo) => {
                let (w, h) = mark_size(img_w, img_h, logo.width(), logo.height(), self.size_pct);
                if (w, h) == logo.dimensions() {
                    ensure_alloc(u64::from(w) * u64::from(h) * 4)?;
                    Ok(logo.clone())
                } else {
                    // Lanczos keeps a float copy of the intermediate pass as well.
                    ensure_alloc(u64::from(w) * u64::from(logo.height().max(h)) * 16)?;
                    Ok(image::imageops::resize(logo, w, h, FilterType::Lanczos3))
                }
            }
        }
    }

    /// Stamps the mark onto `image` in place. A mark that cannot be rendered is skipped.
    pub fn apply_rgba(&self, image: &mut RgbaImage) {
        let (img_w, img_h) = image.dimensions();
        let _ = self.blend(&mut **image, img_w, img_h, 4);
    }

    /// Stamps the mark onto `image`, blending into its own pixel buffer: only the mark's
    /// rectangle is touched and no full-size copy is made. The pixel format is unchanged
    /// except for exotic formats, which become 8-bit RGB(A). Fails when memory runs out.
    pub fn apply(&self, mut image: DynamicImage) -> Result<DynamicImage, String> {
        let (img_w, img_h) = (image.width(), image.height());
        match &mut image {
            DynamicImage::ImageLuma8(buf) => self.blend(&mut **buf, img_w, img_h, 1)?,
            DynamicImage::ImageLumaA8(buf) => self.blend(&mut **buf, img_w, img_h, 2)?,
            DynamicImage::ImageRgb8(buf) => self.blend(&mut **buf, img_w, img_h, 3)?,
            DynamicImage::ImageRgba8(buf) => self.blend(&mut **buf, img_w, img_h, 4)?,
            DynamicImage::ImageLuma16(buf) => self.blend(&mut **buf, img_w, img_h, 1)?,
            DynamicImage::ImageLumaA16(buf) => self.blend(&mut **buf, img_w, img_h, 2)?,
            DynamicImage::ImageRgb16(buf) => self.blend(&mut **buf, img_w, img_h, 3)?,
            DynamicImage::ImageRgba16(buf) => self.blend(&mut **buf, img_w, img_h, 4)?,
            DynamicImage::ImageRgb32F(buf) => self.blend(&mut **buf, img_w, img_h, 3)?,
            DynamicImage::ImageRgba32F(buf) => self.blend(&mut **buf, img_w, img_h, 4)?,
            _ => {
                let channels = if image.color().has_alpha() { 4 } else { 3 };
                ensure_alloc(u64::from(img_w) * u64::from(img_h) * channels)?;
                let mut converted = if channels == 4 {
                    DynamicImage::ImageRgba8(image.to_rgba8())
                } else {
                    DynamicImage::ImageRgb8(image.to_rgb8())
                };
                drop(image);
                return match &mut converted {
                    DynamicImage::ImageRgba8(buf) => self
                        .blend(&mut **buf, img_w, img_h, 4)
                        .map(|()| converted),
                    DynamicImage::ImageRgb8(buf) => self
                        .blend(&mut **buf, img_w, img_h, 3)
                        .map(|()| converted),
                    _ => Ok(converted),
                };
            }
        }
        Ok(image)
    }

    /// Source-over blend of the mark into a row-major buffer of `channels` channels per
    /// pixel (1 = grey, 2 = grey + alpha, 3 = RGB, 4 = RGBA).
    fn blend<T: Channel>(
        &self,
        data: &mut [T],
        img_w: u32,
        img_h: u32,
        channels: usize,
    ) -> Result<(), String> {
        if img_w == 0 || img_h == 0 || self.opacity.is_nan() {
            return Ok(());
        }
        let opacity = self.opacity.clamp(0.0, 1.0);
        if opacity <= 0.0 {
            return Ok(());
        }
        let mark = self.render(img_w, img_h)?;
        let (mark_w, mark_h) = mark.dimensions();
        let (x0, y0) = placement(img_w, img_h, mark_w, mark_h, self.anchor, self.margin_pct);
        let has_alpha = channels == 2 || channels == 4;
        let colour_channels = if channels >= 3 { 3 } else { 1 };
        for my in 0..mark_h {
            let y = y0 + i64::from(my);
            if y < 0 || y >= i64::from(img_h) {
                continue;
            }
            for mx in 0..mark_w {
                let x = x0 + i64::from(mx);
                if x < 0 || x >= i64::from(img_w) {
                    continue;
                }
                let src = mark.get_pixel(mx, my);
                let alpha = f32::from(src[3]) / 255.0 * opacity;
                if alpha <= 0.0 {
                    continue;
                }
                let start = (y as usize * img_w as usize + x as usize) * channels;
                let Some(pixel) = data.get_mut(start..start + channels) else {
                    continue;
                };
                let colour = [src[0], src[1], src[2]].map(|c| f32::from(c) / 255.0);
                let source: [f32; 3] = if colour_channels == 1 {
                    let grey = 0.2126 * colour[0] + 0.7152 * colour[1] + 0.0722 * colour[2];
                    [grey; 3]
                } else {
                    colour
                };
                let dst_alpha = if has_alpha {
                    pixel[channels - 1].to_unit()
                } else {
                    1.0
                };
                let out_alpha = alpha + dst_alpha * (1.0 - alpha);
                if out_alpha <= 0.0 {
                    continue;
                }
                for (channel, s) in pixel[..colour_channels].iter_mut().zip(source) {
                    let d = channel.to_unit();
                    *channel = T::from_unit((s * alpha + d * dst_alpha * (1.0 - alpha)) / out_alpha);
                }
                if has_alpha {
                    pixel[channels - 1] = T::from_unit(out_alpha);
                }
            }
        }
        Ok(())
    }
}

/// A pixel channel the mark can be blended into, as a 0.0–1.0 value.
trait Channel: Copy {
    fn to_unit(self) -> f32;
    fn from_unit(value: f32) -> Self;
}

impl Channel for u8 {
    fn to_unit(self) -> f32 {
        f32::from(self) / 255.0
    }
    fn from_unit(value: f32) -> Self {
        (value * 255.0).round().clamp(0.0, 255.0) as u8
    }
}

impl Channel for u16 {
    fn to_unit(self) -> f32 {
        f32::from(self) / 65535.0
    }
    fn from_unit(value: f32) -> Self {
        (value * 65535.0).round().clamp(0.0, 65535.0) as u16
    }
}

impl Channel for f32 {
    fn to_unit(self) -> f32 {
        self
    }
    fn from_unit(value: f32) -> Self {
        value
    }
}

/// Checks that `bytes` can be allocated right now, so a huge mark fails for one file
/// instead of aborting the process.
fn ensure_alloc(bytes: u64) -> Result<(), String> {
    let mut probe: Vec<u8> = Vec::new();
    usize::try_from(bytes)
        .ok()
        .and_then(|bytes| probe.try_reserve_exact(bytes).ok())
        .ok_or_else(|| "not enough memory for the watermark".to_string())
}

/// Source-over blend of a straight-alpha colour onto a straight-alpha pixel.
fn over(dst: &mut Rgba<u8>, src: [u8; 3], alpha: f32) {
    if alpha.is_nan() || alpha <= 0.0 {
        return;
    }
    let alpha = alpha.min(1.0);
    let dst_alpha = f32::from(dst[3]) / 255.0;
    let out_alpha = alpha + dst_alpha * (1.0 - alpha);
    if out_alpha <= 0.0 {
        return;
    }
    for (channel, &s) in dst.0[..3].iter_mut().zip(src.iter()) {
        let d = f32::from(*channel);
        let c = (f32::from(s) * alpha + d * dst_alpha * (1.0 - alpha)) / out_alpha;
        *channel = c.round().clamp(0.0, 255.0) as u8;
    }
    dst[3] = (out_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
}

/// Lays out `text` on one line at `px` pixels and rasterizes its ink.
fn rasterize(font: &FontVec, text: &str, px: f32) -> Coverage {
    let scaled = font.as_scaled(PxScale::from(px));
    let mut caret_x = 0.0_f32;
    let baseline = scaled.ascent();
    let mut previous = None;
    let mut outlined = Vec::new();
    for c in text.chars() {
        let id = font.glyph_id(c);
        if let Some(prev) = previous {
            caret_x += scaled.kern(prev, id);
        }
        let glyph = id.with_scale_and_position(px, point(caret_x, baseline));
        caret_x += scaled.h_advance(id);
        previous = Some(id);
        if let Some(og) = font.outline_glyph(glyph) {
            outlined.push(og);
        }
    }

    let mut bounds: Option<(f32, f32, f32, f32)> = None;
    for og in &outlined {
        let r = og.px_bounds();
        bounds = Some(match bounds {
            None => (r.min.x, r.min.y, r.max.x, r.max.y),
            Some((x0, y0, x1, y1)) => (
                x0.min(r.min.x),
                y0.min(r.min.y),
                x1.max(r.max.x),
                y1.max(r.max.y),
            ),
        });
    }
    let Some((x0, y0, x1, y1)) = bounds else {
        return Coverage {
            w: 0,
            h: 0,
            data: Vec::new(),
        };
    };
    let (x0, y0) = (x0.floor(), y0.floor());
    let w = ((x1 - x0).ceil().max(1.0)) as u32;
    let h = ((y1 - y0).ceil().max(1.0)) as u32;
    let mut data = vec![0.0_f32; w as usize * h as usize];
    for og in &outlined {
        let r = og.px_bounds();
        let ox = (r.min.x.floor() - x0) as i64;
        let oy = (r.min.y.floor() - y0) as i64;
        og.draw(|gx, gy, coverage| {
            let x = ox + i64::from(gx);
            let y = oy + i64::from(gy);
            if x >= 0 && y >= 0 && (x as u32) < w && (y as u32) < h {
                let cell = &mut data[(y as u32 * w + x as u32) as usize];
                *cell = (*cell + coverage).min(1.0);
            }
        });
    }
    Coverage { w, h, data }
}

/// White text with a soft dark shadow, drawn into a transparent image.
fn text_mark(coverage: &Coverage, px: f32) -> RgbaImage {
    if coverage.w == 0 || coverage.h == 0 {
        return RgbaImage::new(1, 1);
    }
    let offset = (px / 20.0).round().max(1.0) as u32;
    let mut mark = RgbaImage::new(coverage.w + offset, coverage.h + offset);
    // Shadow first, so the white text is never darkened by it.
    for y in 0..coverage.h {
        for x in 0..coverage.w {
            let c = coverage.data[(y * coverage.w + x) as usize];
            if c > 0.0 {
                over(
                    mark.get_pixel_mut(x + offset, y + offset),
                    [0, 0, 0],
                    0.5 * c,
                );
            }
        }
    }
    for y in 0..coverage.h {
        for x in 0..coverage.w {
            let c = coverage.data[(y * coverage.w + x) as usize];
            if c > 0.0 {
                over(mark.get_pixel_mut(x, y), [255, 255, 255], c);
            }
        }
    }
    mark
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_rgb(w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb([0, 0, 0])))
    }

    /// 40x40 PNG: left half opaque red, right half fully transparent.
    fn write_logo(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("logo.png");
        let mut logo = RgbaImage::new(40, 40);
        for (x, _, px) in logo.enumerate_pixels_mut() {
            *px = if x < 20 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 0, 0])
            };
        }
        logo.save(&path).expect("write logo");
        path
    }

    fn logo_watermark(path: PathBuf, anchor: Anchor, opacity: f32) -> Watermark {
        Watermark {
            source: WatermarkSource::Image { path },
            anchor,
            size_pct: 20.0,
            opacity,
            margin_pct: 3.0,
        }
    }

    #[test]
    fn placement_covers_all_nine_anchors() {
        // 1000x500 image, 100x50 mark, 2% margin = 10px.
        let expected = [
            (Anchor::TopLeft, (10, 10)),
            (Anchor::Top, (450, 10)),
            (Anchor::TopRight, (890, 10)),
            (Anchor::Left, (10, 225)),
            (Anchor::Center, (450, 225)),
            (Anchor::Right, (890, 225)),
            (Anchor::BottomLeft, (10, 440)),
            (Anchor::Bottom, (450, 440)),
            (Anchor::BottomRight, (890, 440)),
        ];
        for (anchor, want) in expected {
            assert_eq!(placement(1000, 500, 100, 50, anchor, 2.0), want, "{anchor:?}");
        }
    }

    #[test]
    fn placement_never_goes_negative() {
        // Mark larger than the image: clamped to the origin instead of underflowing.
        assert_eq!(placement(100, 100, 300, 300, Anchor::BottomRight, 5.0), (0, 0));
    }

    #[test]
    fn mark_size_width_percent_and_aspect() {
        assert_eq!(mark_size(1000, 500, 100, 50, 20.0), (200, 100));
        assert_eq!(mark_size(1000, 500, 300, 100, 20.0), (200, 67));
        assert_eq!(mark_size(1000, 500, 200, 100, 50.0), (500, 250));
    }

    #[test]
    fn mark_size_clamps_to_image_height() {
        // 100x1000 mark at 100% width would be 1000 tall; scaled down to the 500px height.
        assert_eq!(mark_size(1000, 500, 100, 1000, 100.0), (50, 500));
    }

    #[test]
    fn mark_size_never_zero_and_clamps_percent() {
        assert_eq!(mark_size(1000, 500, 10000, 1, 1.0), (10, 1));
        assert_eq!(mark_size(0, 0, 0, 0, 50.0), (1, 1));
        // 0% is treated as 1%, 250% as 100%.
        assert_eq!(mark_size(1000, 500, 100, 50, 0.0), (10, 5));
        assert_eq!(mark_size(1000, 500, 100, 50, 250.0), (1000, 500));
    }

    #[test]
    fn image_watermark_blends_only_opaque_pixels() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_logo(dir.path());
        let prepared = PreparedWatermark::prepare(&logo_watermark(path, Anchor::BottomRight, 1.0))
            .expect("prepare");

        // 20% of 200 = 40px logo at BottomRight with a 3px margin: rect x 157..197, y 57..97.
        let out = prepared.apply(solid_rgb(200, 100)).unwrap();
        let out = match out {
            DynamicImage::ImageRgb8(img) => img,
            other => panic!("expected RGB output, got {:?}", other.color()),
        };
        assert_eq!(out.get_pixel(160, 60).0, [255, 0, 0], "opaque part is red");
        assert_eq!(out.get_pixel(190, 60).0, [0, 0, 0], "transparent part is untouched");
        assert_eq!(out.get_pixel(0, 0).0, [0, 0, 0], "outside the mark");
        assert_eq!(out.get_pixel(156, 60).0, [0, 0, 0], "just left of the mark");
        assert_eq!(out.get_pixel(170, 97).0, [0, 0, 0], "just below the mark");
    }

    #[test]
    fn deep_grey_and_alpha_images_keep_their_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_logo(dir.path());
        let prepared = PreparedWatermark::prepare(&logo_watermark(path, Anchor::BottomRight, 1.0))
            .expect("prepare");

        let rgb16 = image::ImageBuffer::from_pixel(200, 100, image::Rgb([0u16, 0, 0]));
        match prepared.apply(DynamicImage::ImageRgb16(rgb16)).unwrap() {
            DynamicImage::ImageRgb16(img) => {
                assert_eq!(img.get_pixel(160, 60).0, [65535, 0, 0]);
                assert_eq!(img.get_pixel(0, 0).0, [0, 0, 0]);
            }
            other => panic!("expected RGB16, got {:?}", other.color()),
        }

        let grey = image::GrayImage::from_pixel(200, 100, image::Luma([0]));
        match prepared.apply(DynamicImage::ImageLuma8(grey)).unwrap() {
            // Pure red as grey (Rec. 709 luma).
            DynamicImage::ImageLuma8(img) => {
                assert_eq!(img.get_pixel(160, 60).0, [54]);
                assert_eq!(img.get_pixel(190, 60).0, [0]);
            }
            other => panic!("expected Luma8, got {:?}", other.color()),
        }

        // A fully transparent pixel under the opaque mark becomes the mark colour, opaque.
        let clear = RgbaImage::from_pixel(200, 100, Rgba([0, 0, 0, 0]));
        match prepared.apply(DynamicImage::ImageRgba8(clear)).unwrap() {
            DynamicImage::ImageRgba8(img) => {
                assert_eq!(img.get_pixel(160, 60).0, [255, 0, 0, 255]);
                assert_eq!(img.get_pixel(0, 0).0, [0, 0, 0, 0]);
            }
            other => panic!("expected RGBA8, got {:?}", other.color()),
        }
    }

    #[test]
    fn half_opacity_gives_half_intensity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_logo(dir.path());
        let prepared = PreparedWatermark::prepare(&logo_watermark(path, Anchor::BottomRight, 0.5))
            .expect("prepare");
        let mut img = RgbaImage::from_pixel(200, 100, Rgba([0, 0, 0, 255]));
        prepared.apply_rgba(&mut img);
        let red = img.get_pixel(160, 60).0[0];
        assert!((126..=130).contains(&red), "red channel was {red}");
        assert_eq!(img.get_pixel(160, 60).0[1], 0);
    }

    #[test]
    fn zero_opacity_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_logo(dir.path());
        let prepared = PreparedWatermark::prepare(&logo_watermark(path, Anchor::TopLeft, 0.0))
            .expect("prepare");
        let mut img = RgbaImage::from_pixel(200, 100, Rgba([10, 20, 30, 255]));
        prepared.apply_rgba(&mut img);
        assert!(img.pixels().all(|p| p.0 == [10, 20, 30, 255]));
    }

    #[test]
    fn text_watermark_stays_in_top_left() {
        let watermark = Watermark {
            source: WatermarkSource::Text {
                text: "Test".into(),
            },
            anchor: Anchor::TopLeft,
            ..Watermark::default()
        };
        let prepared = match PreparedWatermark::prepare(&watermark) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping text watermark test: {e}");
                return;
            }
        };
        let out = prepared.apply(solid_rgb(400, 200)).unwrap().into_rgb8();
        let mut changed = 0;
        for (x, y, px) in out.enumerate_pixels() {
            if px.0 != [0, 0, 0] {
                changed += 1;
                assert!(x < 200 && y < 100, "pixel ({x},{y}) changed outside top-left");
            }
        }
        assert!(changed > 0, "text watermark changed no pixels");
        assert_eq!(out.get_pixel(399, 199).0, [0, 0, 0]);
    }
}
