//! The app icon: the square logo (the default, and the exe's own file icon) or the round
//! one, chosen in Settings › Appearance, on a tile in the accent colour. The choice sets
//! the window and taskbar icon and is mirrored onto the user's shortcuts to the app (Start
//! menu, desktop, pinned taskbar).
//! Artwork: `assets/logo/`, cleaned and sized by `packaging/generate_icon.py`, which also
//! writes one .ico per shape and accent for the exe's icon resources (see `build.rs`).

use std::sync::Arc;

use crate::settings::{Accent, AppIcon};
use crate::theme;

pub const ALL: [(AppIcon, &str); 2] = [(AppIcon::Square, "Square"), (AppIcon::Round, "Round")];

/// The transparent artwork (white X, grey aperture and outline), 256 px.
fn art(shape: AppIcon) -> &'static [u8] {
    match shape {
        AppIcon::Square => include_bytes!("../../../assets/icons/square-art.png"),
        AppIcon::Round => include_bytes!("../../../assets/icons/round-art.png"),
    }
}

/// Grey mask of the inside of the artwork's outline: where the accent tile shows.
fn tile_mask(shape: AppIcon) -> &'static [u8] {
    match shape {
        AppIcon::Square => include_bytes!("../../../assets/icons/square-tile.png"),
        AppIcon::Round => include_bytes!("../../../assets/icons/round-tile.png"),
    }
}

/// Top and bottom of the tile's vertical gradient: the accent deepened so the white X keeps
/// its contrast (`tile_colours` in the generator).
fn tile_colours(accent: Accent) -> ([f32; 3], [f32; 3]) {
    let base = theme::accent_base(accent);
    let scaled = |k: f32| [base.r(), base.g(), base.b()].map(|c| (f32::from(c) * k).round());
    (scaled(0.86), scaled(0.56))
}

/// The artwork composited over its accent tile, 256 px.
fn compose(shape: AppIcon, accent: Accent) -> image::RgbaImage {
    let decode = |bytes| {
        image::load_from_memory(bytes)
            .expect("embedded application icon PNG must be valid")
            .to_rgba8()
    };
    let art = decode(art(shape));
    let mask = image::load_from_memory(tile_mask(shape))
        .expect("embedded icon tile mask must be valid")
        .to_luma8();
    let (top, bottom) = tile_colours(accent);
    let height = art.height() as f32;
    let mut out = art.clone();
    for (x, y, px) in out.enumerate_pixels_mut() {
        let t = (y as f32 + 0.5) / height;
        let fill = [0, 1, 2].map(|i| (top[i] * (1.0 - t) + bottom[i] * t).round());
        let tile_a = f32::from(mask.get_pixel(x, y)[0]) / 255.0;
        let [r, g, b, a] = art.get_pixel(x, y).0;
        let art_a = f32::from(a) / 255.0;
        // Straight-alpha "over": the artwork on top of the tile.
        let out_a = art_a + tile_a * (1.0 - art_a);
        let channel = |c: u8, i: usize| {
            if out_a <= 0.0 {
                0
            } else {
                ((f32::from(c) * art_a + fill[i] * tile_a * (1.0 - art_a)) / out_a).round() as u8
            }
        };
        *px = image::Rgba([
            channel(r, 0),
            channel(g, 1),
            channel(b, 2),
            (out_a * 255.0).round() as u8,
        ]);
    }
    out
}

/// The window and taskbar icon.
pub fn icon_data(shape: AppIcon, accent: Accent) -> egui::IconData {
    let image = compose(shape, accent);
    egui::IconData {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    }
}

/// Index of the matching icon in `zxraw.exe`: all accents of the square logo, then all of
/// the round one, in `theme::ACCENTS` order (`assets/icons/icons.txt`, see `build.rs`).
fn resource_index(shape: AppIcon, accent: Accent) -> i32 {
    let shape_index = ALL.iter().position(|(s, _)| *s == shape).unwrap_or(0);
    let accent_index = theme::ACCENTS
        .iter()
        .position(|(a, _)| *a == accent)
        .unwrap_or(0);
    (shape_index * theme::ACCENTS.len() + accent_index) as i32
}

/// Switches the window icon to `shape` on `accent`.
pub fn apply_to_window(ctx: &egui::Context, shape: AppIcon, accent: Accent) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(Arc::new(icon_data(
        shape, accent,
    )))));
}

/// Points the user's shortcuts to this exe at `shape` on `accent`, on a background thread.
/// Shortcuts to other programs are never touched; failures are only logged.
pub fn apply_to_shortcuts(shape: AppIcon, accent: Accent) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            tracing::warn!("app icon: cannot locate the executable: {e}");
            return;
        }
    };
    std::thread::spawn(move || {
        match forge_win::set_shortcut_icon(&exe, resource_index(shape, accent)) {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                "app icon: updated {n} shortcut(s) to the {shape:?} icon on {accent:?}"
            ),
            Err(e) => tracing::warn!("app icon: could not update shortcuts: {e}"),
        }
    });
}

/// A small preview of `shape` on `accent` for the settings page, made once per run.
pub fn preview(ctx: &egui::Context, shape: AppIcon, accent: Accent) -> egui::TextureHandle {
    let id = egui::Id::new(("app-icon-preview", shape as u8, accent));
    if let Some(texture) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return texture;
    }
    let image = image::DynamicImage::ImageRgba8(compose(shape, accent))
        .resize(96, 96, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let texture = ctx.load_texture(
        format!("app-icon-{shape:?}-{accent:?}"),
        egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            image.as_raw(),
        ),
        egui::TextureOptions::LINEAR,
    );
    ctx.data_mut(|d| d.insert_temp(id, texture.clone()));
    texture
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = include_str!("../../../assets/icons/icons.txt");

    fn ico_bytes(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/icons")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// The 256 px frame of an .ico (stored as PNG by the generator).
    fn ico_256(bytes: &[u8]) -> image::RgbaImage {
        let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]) as usize;
        let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
        let entry = (0..u16_at(4))
            .map(|n| 6 + n * 16)
            .find(|&e| bytes[e] == 0 && bytes[e + 1] == 0)
            .expect("a 256 px frame");
        let (size, offset) = (u32_at(entry + 8), u32_at(entry + 12));
        image::load_from_memory(&bytes[offset..offset + size])
            .expect("PNG frame")
            .to_rgba8()
    }

    #[test]
    fn every_icon_decodes_and_square_blue_is_the_default() {
        for (shape, _) in ALL {
            for (accent, _) in theme::ACCENTS {
                let icon = icon_data(shape, accent);
                assert_eq!(
                    (icon.width, icon.height),
                    (256, 256),
                    "{shape:?} {accent:?}"
                );
                assert_eq!(icon.rgba.len(), 256 * 256 * 4);
            }
        }
        let old: crate::settings::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!((old.app_icon, old.accent), (AppIcon::Square, Accent::Blue));
        assert_eq!(
            resource_index(AppIcon::Square, Accent::Blue),
            0,
            "the exe's own icon"
        );
    }

    #[test]
    fn exe_icon_resources_match_the_window_icons() {
        let names: Vec<&str> = MANIFEST.lines().collect();
        assert_eq!(names.len(), ALL.len() * theme::ACCENTS.len());
        for (shape, shape_name) in ALL {
            for (accent, accent_name) in theme::ACCENTS {
                let name = names[resource_index(shape, accent) as usize];
                assert_eq!(
                    name,
                    format!(
                        "zxraw-{}-{}.ico",
                        shape_name.to_lowercase(),
                        accent_name.to_lowercase()
                    )
                );
                // The generator and `compose` must agree, or the shortcut icon would not
                // match the window icon.
                let ico = ico_256(&ico_bytes(name));
                let ours = compose(shape, accent);
                let worst = ico
                    .pixels()
                    .zip(ours.pixels())
                    .filter(|(a, b)| a[3] > 8 || b[3] > 8)
                    .flat_map(|(a, b)| (0..4).map(move |i| a[i].abs_diff(b[i])))
                    .max()
                    .unwrap_or(0);
                assert!(worst <= 3, "{name}: differs by up to {worst}");
            }
        }
    }
}
