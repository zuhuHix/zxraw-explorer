//! Visual design: restrained neutral palette, one accent, subtle borders.

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Visuals};

pub const ACCENT: Color32 = Color32::from_rgb(77, 144, 254);
const LIGHT_LINK: Color32 = Color32::from_rgb(0, 85, 160);

/// Color-label swatches (Red, Yellow, Green, Blue, Purple) — used for culling labels.
pub const LABEL_COLORS: [Color32; 5] = [
    Color32::from_rgb(224, 82, 82),
    Color32::from_rgb(230, 190, 60),
    Color32::from_rgb(90, 180, 100),
    Color32::from_rgb(80, 140, 230),
    Color32::from_rgb(160, 100, 210),
];

pub fn apply(ctx: &egui::Context, dark: bool, scale: f32) {
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    let (bg, panel, faint, border) = if dark {
        (
            Color32::from_rgb(24, 25, 28),
            Color32::from_rgb(30, 31, 35),
            Color32::from_rgb(36, 37, 42),
            Color32::from_rgb(50, 52, 58),
        )
    } else {
        (
            Color32::from_rgb(250, 250, 251),
            Color32::from_rgb(243, 244, 246),
            Color32::from_rgb(236, 237, 240),
            Color32::from_rgb(218, 220, 225),
        )
    };
    v.panel_fill = panel;
    v.window_fill = panel;
    v.extreme_bg_color = bg;
    v.faint_bg_color = faint;
    v.window_stroke = Stroke::new(1.0, border);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border);
    // Keep selected list rows opaque, with text colors that retain WCAG AA contrast.
    v.selection.bg_fill = selection_fill(dark);
    v.selection.stroke = Stroke::new(1.0, selection_text(dark));
    v.hyperlink_color = link_color(dark);
    let r = CornerRadius::same(4);
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.inactive.corner_radius = r;
    v.widgets.hovered.corner_radius = r;
    v.widgets.active.corner_radius = r;
    v.widgets.open.corner_radius = r;
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    ctx.set_visuals(v);

    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 4.0);
        s.spacing.button_padding = egui::vec2(8.0, 3.0);
        s.spacing.interact_size.y = 22.0;
        s.text_styles = [
            (
                TextStyle::Small,
                FontId::new(11.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(13.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Heading,
                FontId::new(17.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.5, FontFamily::Monospace),
            ),
        ]
        .into();
    });
    ctx.set_zoom_factor(scale.clamp(0.5, 3.0));
}

pub fn muted(ui: &egui::Ui) -> Color32 {
    ui.visuals().weak_text_color()
}

fn link_color(dark: bool) -> Color32 {
    if dark { ACCENT } else { LIGHT_LINK }
}

/// Fill behind selected rows and tiles.
pub fn selection_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(39, 85, 139)
    } else {
        Color32::from_rgb(216, 231, 255)
    }
}

/// Text color on selected rows and tiles (WCAG AA against `selection_fill`).
pub fn selection_text(dark: bool) -> Color32 {
    if dark {
        Color32::WHITE
    } else {
        Color32::from_rgb(17, 36, 58)
    }
}

/// Keyboard focus ring drawn around the cursor row or tile.
pub fn focus_ring(dark: bool) -> Color32 {
    if dark {
        Color32::WHITE
    } else {
        Color32::from_rgb(20, 66, 125)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Panel background the list views draw on, per theme (see `apply`).
    const PANEL_DARK: Color32 = Color32::from_rgb(30, 31, 35);
    const PANEL_LIGHT: Color32 = Color32::from_rgb(243, 244, 246);

    /// WCAG 2.x contrast ratio between two opaque colors (1.0 ..= 21.0).
    fn contrast_ratio(a: Color32, b: Color32) -> f64 {
        fn luminance(c: Color32) -> f64 {
            let linear = |channel: u8| {
                let s = f64::from(channel) / 255.0;
                if s <= 0.03928 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(c.r()) + 0.7152 * linear(c.g()) + 0.0722 * linear(c.b())
        }
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn selected_row_text_meets_wcag_aa() {
        for dark in [true, false] {
            let ratio = contrast_ratio(selection_text(dark), selection_fill(dark));
            assert!(
                ratio >= 4.5,
                "selection text ratio {ratio:.2} (dark={dark})"
            );
        }
    }

    #[test]
    fn hyperlink_color_meets_wcag_aa_on_its_theme_panel() {
        for (dark, panel) in [(true, PANEL_DARK), (false, PANEL_LIGHT)] {
            let ratio = contrast_ratio(link_color(dark), panel);
            assert!(
                ratio >= 4.5,
                "hyperlink ratio {ratio:.2} on {panel:?} (dark={dark})"
            );
        }
    }

    #[test]
    fn focus_ring_meets_non_text_contrast_on_selection_and_panel() {
        for (dark, panel) in [(true, PANEL_DARK), (false, PANEL_LIGHT)] {
            for background in [selection_fill(dark), panel] {
                let ratio = contrast_ratio(focus_ring(dark), background);
                assert!(
                    ratio >= 3.0,
                    "focus ring ratio {ratio:.2} on {background:?} (dark={dark})"
                );
            }
        }
    }
}
