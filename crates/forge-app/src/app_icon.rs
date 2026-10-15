//! The app icon: the folder-camera mark in the colour chosen in Settings › Appearance
//! (Graphite by default, independent of the UI accent). The choice sets the window and
//! taskbar icon and is mirrored onto the user's shortcuts to the app (Start menu, desktop,
//! pinned taskbar).
//! Artwork: `assets/icons/zxraw-<colour>.png` (256 px) and `.ico` (every size, for the exe's
//! icon resources, see `build.rs`), both written by `packaging/generate_icon.py` from the
//! masters that `packaging/draw_logo.py` draws.

use std::sync::Arc;

use crate::settings::AppIcon;

/// Every colour, in `assets/icons/icons.txt` order (Graphite is the exe's own icon).
pub const ALL: [(AppIcon, &str); 10] = [
    (AppIcon::Graphite, "Graphite"),
    (AppIcon::Blue, "Blue"),
    (AppIcon::Steel, "Steel"),
    (AppIcon::Sky, "Sky"),
    (AppIcon::Teal, "Teal"),
    (AppIcon::Green, "Green"),
    (AppIcon::Purple, "Purple"),
    (AppIcon::Pink, "Pink"),
    (AppIcon::Red, "Red"),
    (AppIcon::Orange, "Orange"),
];

/// The 256 px mark of `icon`, with a transparent background.
fn png(icon: AppIcon) -> &'static [u8] {
    match icon {
        AppIcon::Graphite => include_bytes!("../../../assets/icons/zxraw-graphite.png"),
        AppIcon::Blue => include_bytes!("../../../assets/icons/zxraw-blue.png"),
        AppIcon::Steel => include_bytes!("../../../assets/icons/zxraw-steel.png"),
        AppIcon::Sky => include_bytes!("../../../assets/icons/zxraw-sky.png"),
        AppIcon::Teal => include_bytes!("../../../assets/icons/zxraw-teal.png"),
        AppIcon::Green => include_bytes!("../../../assets/icons/zxraw-green.png"),
        AppIcon::Purple => include_bytes!("../../../assets/icons/zxraw-purple.png"),
        AppIcon::Pink => include_bytes!("../../../assets/icons/zxraw-pink.png"),
        AppIcon::Red => include_bytes!("../../../assets/icons/zxraw-red.png"),
        AppIcon::Orange => include_bytes!("../../../assets/icons/zxraw-orange.png"),
    }
}

/// The decoded 256 px mark of `icon`.
fn decode(icon: AppIcon) -> image::RgbaImage {
    image::load_from_memory(png(icon))
        .expect("embedded application icon PNG must be valid")
        .to_rgba8()
}

/// The window and taskbar icon.
pub fn icon_data(icon: AppIcon) -> egui::IconData {
    let image = decode(icon);
    egui::IconData {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    }
}

/// Index of the matching icon in `zxraw.exe`: the order of `ALL` (`assets/icons/icons.txt`,
/// see `build.rs`).
fn resource_index(icon: AppIcon) -> i32 {
    ALL.iter().position(|(i, _)| *i == icon).unwrap_or(0) as i32
}

/// Switches the window icon to `icon`.
pub fn apply_to_window(ctx: &egui::Context, icon: AppIcon) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(Arc::new(icon_data(icon)))));
}

/// Points the user's shortcuts to this exe at `icon`, on a background thread. Shortcuts to
/// other programs are never touched; failures are only logged.
pub fn apply_to_shortcuts(icon: AppIcon) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            tracing::warn!("app icon: cannot locate the executable: {e}");
            return;
        }
    };
    std::thread::spawn(move || match forge_win::set_shortcut_icon(&exe, resource_index(icon)) {
        Ok(0) => {}
        Ok(n) => tracing::info!("app icon: updated {n} shortcut(s) to the {icon:?} icon"),
        Err(e) => tracing::warn!("app icon: could not update shortcuts: {e}"),
    });
}

/// A preview of `icon` for the settings page and the welcome notice, made once per run.
pub fn preview(ctx: &egui::Context, icon: AppIcon) -> egui::TextureHandle {
    let id = egui::Id::new(("app-icon-preview", icon as u8));
    if let Some(texture) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return texture;
    }
    let image = decode(icon);
    let texture = ctx.load_texture(
        format!("app-icon-{icon:?}"),
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

    /// The 256 px frame of an .ico (PNG-compressed, as the generator writes it).
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
    fn every_colour_decodes_and_graphite_is_the_default() {
        for (icon, _) in ALL {
            let data = icon_data(icon);
            assert_eq!((data.width, data.height), (256, 256), "{icon:?}");
            assert_eq!(data.rgba.len(), 256 * 256 * 4);
        }
        let old: crate::settings::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(*old.app_icon, AppIcon::Graphite);
        assert_eq!(resource_index(AppIcon::Graphite), 0, "the exe's own icon");
    }

    #[test]
    fn exe_icon_resources_match_the_embedded_pngs() {
        let names: Vec<&str> = MANIFEST.lines().collect();
        assert_eq!(names.len(), ALL.len());
        for (icon, label) in ALL {
            let name = names[resource_index(icon) as usize];
            assert_eq!(name, format!("zxraw-{}.ico", label.to_lowercase()));
            // The shortcut icon must be the same picture as the window icon.
            let frame = ico_256(&ico_bytes(name));
            assert!(frame == decode(icon), "{name} differs from its embedded PNG");
        }
    }
}
