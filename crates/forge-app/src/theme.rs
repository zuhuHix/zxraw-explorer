//! Visual design: restrained neutral palette, one accent, subtle borders.

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Visuals};

pub const ACCENT: Color32 = Color32::from_rgb(77, 144, 254);

/// Color-label swatches (Red, Yellow, Green, Blue, Purple) — used for culling labels.
pub const LABEL_COLORS: [Color32; 5] = [
    Color32::from_rgb(224, 82, 82),
    Color32::from_rgb(230, 190, 60),
    Color32::from_rgb(90, 180, 100),
    Color32::from_rgb(80, 140, 230),
    Color32::from_rgb(160, 100, 210),
];

pub fn apply(ctx: &egui::Context, dark: bool, scale: f32) {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
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
    v.selection.bg_fill = ACCENT.linear_multiply(if dark { 0.45 } else { 0.30 });
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
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
            (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(17.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
        ]
        .into();
    });
    ctx.set_zoom_factor(scale.clamp(0.5, 3.0));
}

pub fn muted(ui: &egui::Ui) -> Color32 {
    ui.visuals().weak_text_color()
}
