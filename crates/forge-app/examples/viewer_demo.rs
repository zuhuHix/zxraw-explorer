//! Standalone demo of the ZXraw Explorer viewer (crates/forge-app/src/viewer).
//!
//! ```text
//! cargo run -p forge-app --example viewer_demo -- [FILES or FOLDERS...]
//! ```
//!
//! Without arguments it generates a set of test images (gradients with EXIF, a burst, an
//! EXIF-rotated portrait, PNG/TIFF/WebP, a corrupt JPEG and a fake video) under
//! `%TEMP%\zxraw_viewer_demo` and opens those. Annotations live in an in-memory store.
//!
//! Environment (for automation / screenshots):
//! * `ZXRAW_SCREENSHOT=out.png` — capture the rendered frame via egui's screenshot path
//!   after ~90 frames (`ZXRAW_SCREENSHOT_FRAMES`), save it and exit.
//! * `ZXRAW_VIEWER_MODE=single|cull|compare` (default `cull`), `ZXRAW_VIEWER_START=N`.
//! * `ZXRAW_VIEWER_KEYS=I,G,Shift+6,Z` — keys injected one per frame from frame 30.

#[path = "../src/viewer/mod.rs"]
mod viewer;

use std::path::{Path, PathBuf};

use viewer::{AnnotationChange, AnnotationStore, MemoryStore, Viewer, ViewerMode, ViewerOutcome};

const MEDIA_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "tif", "tiff", "webp", "gif", "bmp", "cr2", "cr3", "nef", "arw",
    "dng", "raf", "orf", "rw2", "pef", "srw", "mp4", "mov", "mkv", "avi", "mts", "m4v",
];

fn main() -> eframe::Result {
    let mut items: Vec<PathBuf> = Vec::new();
    for a in std::env::args_os().skip(1) {
        let p = PathBuf::from(a);
        if p.is_dir() {
            let mut v: Vec<PathBuf> = std::fs::read_dir(&p)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && is_media(p))
                .collect();
            v.sort();
            items.extend(v);
        } else {
            items.push(p);
        }
    }
    let generated = items.is_empty();
    if generated {
        match generate_samples() {
            Ok(v) => items = v,
            Err(e) => eprintln!("could not generate sample images: {e}"),
        }
    }
    let mode = match std::env::var("ZXRAW_VIEWER_MODE").unwrap_or_default().to_ascii_lowercase().as_str() {
        "single" => ViewerMode::Single,
        "compare" => ViewerMode::Compare,
        _ => ViewerMode::Cull,
    };
    let start: usize = std::env::var("ZXRAW_VIEWER_START").ok().and_then(|s| s.parse().ok()).unwrap_or(0);

    let mut store = MemoryStore::default();
    if generated {
        let seed: &[(usize, AnnotationChange)] = &[
            (0, AnnotationChange { rating: Some(3), pick: Some(1), ..Default::default() }),
            (1, AnnotationChange { rating: Some(5), label: Some(Some(2)), ..Default::default() }),
            (2, AnnotationChange { rating: Some(2), label: Some(Some(0)), ..Default::default() }),
            (3, AnnotationChange { pick: Some(-1), ..Default::default() }),
            (5, AnnotationChange { rating: Some(4), label: Some(Some(3)), pick: Some(1) }),
        ];
        for (i, c) in seed {
            if let Some(p) = items.get(*i) {
                store.set(std::slice::from_ref(p), *c);
            }
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ZXraw Explorer — Viewer demo")
            .with_inner_size([1360.0, 840.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "zxraw-viewer-demo",
        options,
        Box::new(move |cc| {
            install_fonts(&cc.egui_ctx);
            apply_dark(&cc.egui_ctx);
            let viewer = Viewer::new(items, start, mode, &cc.egui_ctx);
            Ok(Box::new(Demo {
                viewer,
                store,
                shot: std::env::var_os("ZXRAW_SCREENSHOT").map(PathBuf::from),
                keys: parse_keys(&std::env::var("ZXRAW_VIEWER_KEYS").unwrap_or_default()),
                frames: 0,
                shot_after: std::env::var("ZXRAW_SCREENSHOT_FRAMES").ok().and_then(|s| s.parse().ok()).unwrap_or(90),
                requested: false,
            }))
        }),
    )
}

struct Demo {
    viewer: Viewer,
    store: MemoryStore,
    shot: Option<PathBuf>,
    keys: Vec<(egui::Key, egui::Modifiers)>,
    frames: u32,
    shot_after: u32,
    requested: bool,
}

impl eframe::App for Demo {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if self.frames >= 30 && !self.keys.is_empty() {
            let (key, modifiers) = self.keys.remove(0);
            for pressed in [true, false] {
                raw.events.push(egui::Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed,
                    repeat: false,
                    modifiers,
                });
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let out = egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.viewer.show(ui, &mut self.store))
            .inner;
        if let ViewerOutcome::Close { last_index } = out {
            println!("viewer closed at item {last_index}");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if let Some(path) = self.shot.clone() {
            self.frames += 1;
            if self.frames > self.shot_after && self.keys.is_empty() && !self.requested {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                self.requested = true;
            }
            let shot = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(img) = shot {
                let [w, h] = img.size;
                let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                match image::RgbaImage::from_raw(w as u32, h as u32, bytes) {
                    Some(buf) => match buf.save(&path) {
                        Ok(()) => println!("screenshot saved to {}", path.display()),
                        Err(e) => eprintln!("screenshot save failed: {e}"),
                    },
                    None => eprintln!("screenshot buffer size mismatch"),
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint();
        } else if !self.keys.is_empty() {
            self.frames += 1;
            ctx.request_repaint();
        }
    }
}

fn parse_keys(s: &str) -> Vec<(egui::Key, egui::Modifiers)> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|tok| {
            let mut m = egui::Modifiers::NONE;
            let mut name = tok;
            loop {
                if let Some(r) = name.strip_prefix("Shift+") {
                    m.shift = true;
                    name = r;
                } else if let Some(r) = name.strip_prefix("Ctrl+") {
                    m.ctrl = true;
                    m.command = true;
                    name = r;
                } else {
                    break;
                }
            }
            let k = egui::Key::from_name(name)
                .or_else(|| name.strip_prefix("Num").and_then(egui::Key::from_name));
            if k.is_none() {
                eprintln!("unknown key name: {name}");
            }
            k.map(|k| (k, m))
        })
        .collect()
}

fn is_media(p: &Path) -> bool {
    p.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| MEDIA_EXTS.contains(&e.as_str()))
}

// ---------------------------------------------------------------------------
// Look & feel (same palette as the app's theme.rs)
// ---------------------------------------------------------------------------

fn apply_dark(ctx: &egui::Context) {
    use egui::{Color32, CornerRadius, Stroke};
    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::from_rgb(30, 31, 35);
    v.window_fill = Color32::from_rgb(30, 31, 35);
    v.extreme_bg_color = Color32::from_rgb(24, 25, 28);
    v.faint_bg_color = Color32::from_rgb(36, 37, 42);
    v.window_stroke = Stroke::new(1.0, Color32::from_rgb(50, 52, 58));
    let accent = Color32::from_rgb(77, 144, 254);
    v.selection.bg_fill = accent.linear_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0, accent);
    let r = CornerRadius::same(4);
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.inactive.corner_radius = r;
    v.widgets.hovered.corner_radius = r;
    v.widgets.active.corner_radius = r;
    v.window_corner_radius = CornerRadius::same(8);
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 4.0);
        s.spacing.button_padding = egui::vec2(8.0, 3.0);
        s.spacing.interact_size.y = 22.0;
    });
}

fn install_fonts(ctx: &egui::Context) {
    use std::sync::Arc;
    let dir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts");
    let mut defs = egui::FontDefinitions::default();
    let prop = egui::FontFamily::Proportional;
    if let Ok(b) = std::fs::read(dir.join("segoeui.ttf")) {
        defs.font_data.insert("segoeui".into(), Arc::new(egui::FontData::from_owned(b)));
        defs.families.entry(prop.clone()).or_default().insert(0, "segoeui".into());
    }
    if let Ok(b) = std::fs::read(dir.join("seguisym.ttf")) {
        defs.font_data.insert("seguisym".into(), Arc::new(egui::FontData::from_owned(b)));
        defs.families.entry(prop).or_default().push("seguisym".into());
        defs.families.entry(egui::FontFamily::Monospace).or_default().push("seguisym".into());
    }
    ctx.set_fonts(defs);
}

// ---------------------------------------------------------------------------
// Sample generation
// ---------------------------------------------------------------------------

struct Exif<'a> {
    make: &'a str,
    model: &'a str,
    orientation: u16,
    date: &'a str,
    fnum: (u32, u32),
    exposure: (u32, u32),
    iso: u16,
    focal: (u32, u32),
}

fn generate_samples() -> std::io::Result<Vec<PathBuf>> {
    let dir = std::env::temp_dir().join("zxraw_viewer_demo");
    std::fs::create_dir_all(&dir)?;
    let mut out = Vec::new();
    let mut jpeg = |name: &str, w: u32, h: u32, hue: f32, exif: Exif| -> std::io::Result<()> {
        let p = dir.join(name);
        if !p.exists() {
            let img = pattern(w, h, hue);
            let mut buf = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 90)
                .encode_image(&img)
                .map_err(std::io::Error::other)?;
            let app1 = exif_app1(&exif);
            let mut file = Vec::with_capacity(buf.len() + app1.len());
            file.extend_from_slice(&buf[..2]);
            file.extend_from_slice(&app1);
            file.extend_from_slice(&buf[2..]);
            std::fs::write(&p, file)?;
        }
        out.push(p);
        Ok(())
    };
    let base = |date| Exif {
        make: "Canon",
        model: "Canon EOS R5",
        orientation: 1,
        date,
        fnum: (28, 10),
        exposure: (1, 250),
        iso: 400,
        focal: (85, 1),
    };
    jpeg("01_landscape.jpg", 4000, 2667, 0.58, base("2026:10:01 09:12:30"))?;
    jpeg("02_burst_a.jpg", 3000, 2000, 0.08, Exif { iso: 800, exposure: (1, 2000), fnum: (40, 10), focal: (200, 1), ..base("2026:10:01 10:05:00") })?;
    jpeg("03_burst_b.jpg", 3000, 2000, 0.10, Exif { iso: 800, exposure: (1, 2000), fnum: (40, 10), focal: (200, 1), ..base("2026:10:01 10:05:00") })?;
    jpeg("04_burst_c.jpg", 3000, 2000, 0.12, Exif { iso: 800, exposure: (1, 2000), fnum: (40, 10), focal: (200, 1), ..base("2026:10:01 10:05:01") })?;
    jpeg("05_portrait_exif_rot.jpg", 3000, 2000, 0.33, Exif { orientation: 6, make: "SONY", model: "ILCE-7M4", fnum: (18, 10), focal: (50, 1), exposure: (1, 125), iso: 100, ..base("2026:10:01 11:40:02") })?;
    let mut simple = |name: &str, w: u32, h: u32, hue: f32| -> std::io::Result<()> {
        let p = dir.join(name);
        if !p.exists() {
            let img = pattern(w, h, hue);
            if name.ends_with(".webp") {
                image::DynamicImage::ImageRgb8(img).to_rgba8().save(&p).map_err(std::io::Error::other)?;
            } else {
                img.save(&p).map_err(std::io::Error::other)?;
            }
        }
        out.push(p);
        Ok(())
    };
    simple("06_graphic.png", 1600, 1066, 0.75)?;
    simple("07_scan.tif", 2400, 1600, 0.45)?;
    simple("08_web.webp", 1200, 800, 0.95)?;
    let corrupt = dir.join("09_corrupt.jpg");
    std::fs::write(&corrupt, [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1])?;
    out.push(corrupt);
    let video = dir.join("10_clip.mp4");
    if !video.exists() {
        std::fs::write(&video, b"\0\0\0\x18ftypmp42not-a-real-video")?;
    }
    out.push(video);
    Ok(out)
}

/// Colourful test card: gradient, rings, a fine checker patch (to judge 100 %) and bars.
fn pattern(w: u32, h: u32, hue: f32) -> image::RgbImage {
    image::RgbImage::from_fn(w, h, |x, y| {
        let fx = x as f32 / w as f32;
        let fy = y as f32 / h as f32;
        let (cx, cy) = (fx - 0.5, (fy - 0.5) * h as f32 / w as f32);
        let d = (cx * cx + cy * cy).sqrt();
        let ring = ((d * 60.0).sin() * 0.5 + 0.5) * (1.0 - (d * 2.0).min(1.0));
        let hh = (hue + fx * 0.15 + ring * 0.05).fract();
        let l = 0.25 + 0.45 * (1.0 - fy) + 0.2 * ring;
        let (mut r, mut g, mut b) = hsl(hh, 0.55, l.min(0.92));
        // Fine checker in the lower-right patch: visible only at 100 %.
        if fx > 0.70 && fx < 0.90 && fy > 0.62 && fy < 0.86 {
            let c = if (x / 2 + y / 2) % 2 == 0 { 235.0 } else { 25.0 };
            r = c;
            g = c;
            b = c;
        }
        // Grey ramp at the bottom.
        if fy > 0.93 {
            let v = (fx * 255.0).round();
            r = v;
            g = v;
            b = v;
        }
        image::Rgb([r as u8, g as u8, b as u8])
    })
}

fn hsl(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h * 6.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    ((r + m) * 255.0, (g + m) * 255.0, (b + m) * 255.0)
}

/// Minimal little-endian EXIF APP1 segment (IFD0 + Exif sub-IFD).
fn exif_app1(e: &Exif) -> Vec<u8> {
    fn ascii(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }
    fn rational((n, d): (u32, u32)) -> Vec<u8> {
        let mut v = n.to_le_bytes().to_vec();
        v.extend_from_slice(&d.to_le_bytes());
        v
    }
    // (tag, type, count, data)
    type Entry = (u16, u16, u32, Vec<u8>);
    fn ifd_size(entries: &[Entry]) -> u32 {
        let data: u32 = entries
            .iter()
            .map(|(_, _, _, d)| if d.len() > 4 { (d.len() as u32 + 1) & !1 } else { 0 })
            .sum();
        2 + 12 * entries.len() as u32 + 4 + data
    }
    fn write_ifd(entries: &[Entry], at: u32) -> Vec<u8> {
        let mut table = Vec::new();
        let mut data = Vec::new();
        let data_start = at + 2 + 12 * entries.len() as u32 + 4;
        table.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (tag, ty, count, d) in entries {
            table.extend_from_slice(&tag.to_le_bytes());
            table.extend_from_slice(&ty.to_le_bytes());
            table.extend_from_slice(&count.to_le_bytes());
            if d.len() <= 4 {
                let mut v = d.clone();
                v.resize(4, 0);
                table.extend_from_slice(&v);
            } else {
                table.extend_from_slice(&(data_start + data.len() as u32).to_le_bytes());
                data.extend_from_slice(d);
                if data.len() % 2 == 1 {
                    data.push(0);
                }
            }
        }
        table.extend_from_slice(&0u32.to_le_bytes());
        table.extend_from_slice(&data);
        table
    }
    let exif_entries: Vec<Entry> = vec![
        (0x829A, 5, 1, rational(e.exposure)),
        (0x829D, 5, 1, rational(e.fnum)),
        (0x8827, 3, 1, e.iso.to_le_bytes().to_vec()),
        (0x9003, 2, 20, ascii(e.date)),
        (0x920A, 5, 1, rational(e.focal)),
    ];
    let make = ascii(e.make);
    let model = ascii(e.model);
    let mut ifd0: Vec<Entry> = vec![
        (0x010F, 2, make.len() as u32, make),
        (0x0110, 2, model.len() as u32, model),
        (0x0112, 3, 1, e.orientation.to_le_bytes().to_vec()),
        (0x8769, 4, 1, vec![0; 4]),
    ];
    let exif_at = 8 + ifd_size(&ifd0);
    ifd0[3].3 = exif_at.to_le_bytes().to_vec();
    let mut tiff = b"II*\0".to_vec();
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&write_ifd(&ifd0, 8));
    tiff.extend_from_slice(&write_ifd(&exif_entries, exif_at));
    let mut seg = vec![0xFF, 0xE1];
    seg.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
    seg.extend_from_slice(b"Exif\0\0");
    seg.extend_from_slice(&tiff);
    seg
}
