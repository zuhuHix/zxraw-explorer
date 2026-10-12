//! The app icon: the square logo (the default, and the exe's own file icon) or the round
//! one, chosen in Settings › Appearance. The choice sets the window and taskbar icon and
//! is mirrored onto the user's shortcuts to the app (Start menu, desktop, pinned taskbar).
//! Artwork: `assets/logo/`, sized by `packaging/generate_icon.py`.

use std::sync::Arc;

use crate::settings::AppIcon;

pub const ALL: [(AppIcon, &str); 2] = [(AppIcon::Square, "Square"), (AppIcon::Round, "Round")];

fn png(choice: AppIcon) -> &'static [u8] {
    match choice {
        AppIcon::Square => include_bytes!("../../../assets/zxraw.png"),
        AppIcon::Round => include_bytes!("../../../assets/zxraw-round.png"),
    }
}

/// The window and taskbar icon.
pub fn icon_data(choice: AppIcon) -> egui::IconData {
    let image = image::load_from_memory(png(choice))
        .expect("embedded application icon PNG must be valid")
        .to_rgba8();
    egui::IconData {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    }
}

/// Index of the matching icon in `zxraw.exe` (icon resources 1 and 2, see `build.rs`).
fn resource_index(choice: AppIcon) -> i32 {
    match choice {
        AppIcon::Square => 0,
        AppIcon::Round => 1,
    }
}

/// Switches the window icon to `choice`.
pub fn apply_to_window(ctx: &egui::Context, choice: AppIcon) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(Arc::new(icon_data(
        choice,
    )))));
}

/// Points the user's shortcuts to this exe at `choice`, on a background thread. Shortcuts
/// to other programs are never touched; failures are only logged.
pub fn apply_to_shortcuts(choice: AppIcon) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            tracing::warn!("app icon: cannot locate the executable: {e}");
            return;
        }
    };
    std::thread::spawn(
        move || match forge_win::set_shortcut_icon(&exe, resource_index(choice)) {
            Ok(0) => {}
            Ok(n) => tracing::info!("app icon: updated {n} shortcut(s) to the {choice:?} icon"),
            Err(e) => tracing::warn!("app icon: could not update shortcuts: {e}"),
        },
    );
}

/// A small preview of `choice` for the settings page, decoded once per run.
pub fn preview(ctx: &egui::Context, choice: AppIcon) -> egui::TextureHandle {
    let id = egui::Id::new(("app-icon-preview", choice as u8));
    if let Some(texture) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return texture;
    }
    let image = image::load_from_memory(png(choice))
        .expect("embedded application icon PNG must be valid")
        .resize(96, 96, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let texture = ctx.load_texture(
        format!("app-icon-{choice:?}"),
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

    #[test]
    fn both_icons_decode_and_square_is_the_default() {
        for (choice, _) in ALL {
            let icon = icon_data(choice);
            assert_eq!((icon.width, icon.height), (256, 256), "{choice:?}");
            assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        }
        let old: crate::settings::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(old.app_icon, AppIcon::Square);
        assert_eq!(resource_index(AppIcon::Square), 0, "the exe's own icon");
    }
}
