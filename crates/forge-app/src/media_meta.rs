//! Media metadata for the preview panel, loaded off the UI thread.

use std::path::PathBuf;
use std::sync::Arc;

use egui::{RichText, Ui};
use forge_core::FileEntry;
use forge_core::model::MediaKind;
use parking_lot::Mutex;

#[derive(Clone)]
pub enum MediaMeta {
    Image(Box<forge_media::ImageMetadata>),
    Video(Box<forge_media::VideoMetadata>),
}

type MetaSlot = Arc<Mutex<Option<(PathBuf, Result<MediaMeta, String>)>>>;

/// Holds the metadata of the most recently requested file.
#[derive(Default)]
pub struct MetaLoader {
    slot: MetaSlot,
    requested: Option<PathBuf>,
}

impl MetaLoader {
    pub fn get(&mut self, e: &FileEntry, ctx: &egui::Context) -> Option<Result<MediaMeta, String>> {
        if let Some((p, r)) = &*self.slot.lock() {
            if *p == e.path {
                return Some(r.clone());
            }
        }
        if self.requested.as_deref() != Some(e.path.as_path()) {
            self.requested = Some(e.path.clone());
            let slot = self.slot.clone();
            let path = e.path.clone();
            let kind = e.media_kind();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let r = match kind {
                    MediaKind::Video => forge_media::FfTools::global()
                        .probe(&path)
                        .map(|m| MediaMeta::Video(Box::new(m)))
                        .map_err(|e| e.to_string()),
                    _ => forge_media::read_image_metadata(&path)
                        .map(|m| MediaMeta::Image(Box::new(m)))
                        .map_err(|e| e.to_string()),
                };
                *slot.lock() = Some((path, r));
                ctx.request_repaint();
            });
        }
        None
    }
}

fn kv(ui: &mut Ui, k: &str, v: Option<String>) {
    if let Some(v) = v.filter(|v| !v.is_empty()) {
        ui.label(RichText::new(k).color(ui.visuals().weak_text_color()));
        ui.label(v);
        ui.end_row();
    }
}

pub fn image_grid(ui: &mut Ui, m: &forge_media::ImageMetadata) {
    egui::Grid::new("exif")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            kv(
                ui,
                "Dimensions",
                m.display_dimensions()
                    .map(|(w, h)| format!("{w} × {h}  ({:.1} MP)", (w as f64 * h as f64) / 1e6)),
            );
            let camera = match (&m.make, &m.model) {
                (Some(mk), Some(md)) if md.starts_with(mk.as_str()) => Some(md.clone()),
                (Some(mk), Some(md)) => Some(format!("{mk} {md}")),
                (a, b) => a.clone().or(b.clone()),
            };
            kv(ui, "Camera", camera);
            kv(ui, "Lens", m.lens.clone());
            kv(
                ui,
                "Focal length",
                m.focal_length_mm.map(|f| match m.focal_length_35mm {
                    Some(e) if e as f64 != f.round() => format!("{f:.0} mm  ({e} mm equiv.)"),
                    _ => format!("{f:.0} mm"),
                }),
            );
            kv(ui, "Aperture", m.aperture.map(|a| format!("f/{a:.1}")));
            kv(ui, "Shutter", m.shutter.clone().map(|s| format!("{s} s")));
            kv(ui, "ISO", m.iso.map(|i| i.to_string()));
            kv(
                ui,
                "Exposure comp.",
                m.exposure_compensation
                    .filter(|e| *e != 0.0)
                    .map(|e| format!("{e:+.1} EV")),
            );
            kv(ui, "Taken", m.date_taken.clone());
            kv(ui, "Color space", m.color_space.clone());
            kv(ui, "Bit depth", m.bit_depth.map(|b| format!("{b}-bit")));
            kv(
                ui,
                "GPS",
                m.gps_lat
                    .zip(m.gps_lon)
                    .map(|(a, o)| format!("{a:.5}, {o:.5}")),
            );
            if let Some(x) = &m.xmp {
                kv(
                    ui,
                    "XMP rating",
                    x.rating.map(|r| "★".repeat(r.clamp(0, 5) as usize)),
                );
                kv(ui, "XMP label", x.label.clone());
                kv(
                    ui,
                    "Keywords",
                    (!x.keywords.is_empty()).then(|| x.keywords.join(", ")),
                );
                kv(ui, "Title", x.title.clone());
            }
        });
    if !m.exif_tags.is_empty() {
        ui.collapsing(format!("All EXIF tags ({})", m.exif_tags.len()), |ui| {
            egui::Grid::new("alltags")
                .num_columns(2)
                .striped(true)
                .show(ui, |ui| {
                    for t in &m.exif_tags {
                        ui.label(
                            RichText::new(&t.name)
                                .small()
                                .color(ui.visuals().weak_text_color()),
                        );
                        ui.label(RichText::new(&t.value).small());
                        ui.end_row();
                    }
                });
        });
    }
}

pub fn video_grid(ui: &mut Ui, m: &forge_media::VideoMetadata) {
    egui::Grid::new("vmeta")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            kv(ui, "Duration", m.duration_s.map(format_duration));
            kv(
                ui,
                "Resolution",
                m.width.zip(m.height).map(|(w, h)| {
                    if m.rotation != 0 {
                        format!("{w} × {h}  (rotated {}°)", m.rotation)
                    } else {
                        format!("{w} × {h}")
                    }
                }),
            );
            kv(
                ui,
                "Frame rate",
                m.fps
                    .map(|f| format!("{f:.3} fps{}", if m.vfr { "  (variable)" } else { "" })),
            );
            kv(
                ui,
                "Codec",
                m.codec.clone().map(|c| match &m.profile {
                    Some(p) => format!("{c} ({p})"),
                    None => c,
                }),
            );
            kv(ui, "Bit depth", m.bit_depth.map(|b| format!("{b}-bit")));
            kv(ui, "Chroma", m.chroma_subsampling.clone());
            kv(
                ui,
                "Bitrate",
                m.bitrate.map(|b| format!("{:.1} Mb/s", b as f64 / 1e6)),
            );
            kv(
                ui,
                "HDR",
                m.hdr.map(|h| {
                    format!(
                        "{h:?}{}",
                        if m.dolby_vision {
                            " + Dolby Vision"
                        } else {
                            ""
                        }
                    )
                }),
            );
            kv(
                ui,
                "Color",
                m.color_primaries
                    .clone()
                    .map(|p| format!("{p} / {}", m.color_transfer.clone().unwrap_or_default())),
            );
            kv(ui, "Timecode", m.timecode.clone());
            kv(ui, "Recorded", m.creation_time.clone());
            kv(
                ui,
                "Camera",
                m.make
                    .clone()
                    .map(|mk| format!("{mk} {}", m.model.clone().unwrap_or_default())),
            );
            kv(ui, "Container", m.container.clone());
            for (i, a) in m.audio.iter().enumerate() {
                kv(
                    ui,
                    &format!("Audio {}", i + 1),
                    Some(format!(
                        "{} {}ch {} Hz",
                        a.codec.clone().unwrap_or_default(),
                        a.channels.unwrap_or(0),
                        a.sample_rate.unwrap_or(0)
                    )),
                );
            }
        });
}

pub fn format_duration(s: f64) -> String {
    let t = s.max(0.0);
    let h = (t / 3600.0) as u64;
    let m = ((t % 3600.0) / 60.0) as u64;
    let sec = t % 60.0;
    if h > 0 {
        format!("{h}:{m:02}:{sec:05.2}")
    } else {
        format!("{m}:{sec:05.2}")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn durations() {
        assert_eq!(super::format_duration(65.5), "1:05.50");
        assert_eq!(super::format_duration(3725.0), "1:02:05.00");
    }
}
