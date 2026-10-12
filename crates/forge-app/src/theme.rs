//! Visual design: layered neutral surfaces faintly tinted towards one accent colour (chosen
//! in Settings › Appearance), soft shadows under everything that floats, gentle rounding.
//!
//! Surfaces, darkest to lightest in the dark theme: `sunken` (text fields), `chrome` (tab
//! strip, toolbar, status bar), `sidebar`, `content` (file list, preview) and `raised`
//! (windows, menus, popups). Code that paints with the accent calls [`accent`].

use egui::{Color32, CornerRadius, FontFamily, FontId, Frame, Shadow, Stroke, TextStyle, Visuals};

use crate::settings::Accent;

/// The default accent (Blue). Prefer [`accent`], which follows the user's choice and keeps
/// its contrast on the light theme.
pub const ACCENT: Color32 = Color32::from_rgb(77, 144, 254);

/// Accent presets offered in Settings › Appearance, in swatch order. Keep in step with
/// `ACCENTS` in `packaging/generate_icon.py`, which tints the app icon tiles.
pub const ACCENTS: [(Accent, &str); 7] = [
    (Accent::Blue, "Blue"),
    (Accent::Purple, "Purple"),
    (Accent::Pink, "Pink"),
    (Accent::Red, "Red"),
    (Accent::Orange, "Orange"),
    (Accent::Green, "Green"),
    (Accent::Teal, "Teal"),
];

/// The preset's own colour: swatches, the icon tile, and the base every theme colour is
/// derived from.
pub fn accent_base(accent: Accent) -> Color32 {
    match accent {
        Accent::Blue => ACCENT,
        Accent::Purple => Color32::from_rgb(151, 117, 250),
        Accent::Pink => Color32::from_rgb(232, 98, 170),
        Accent::Red => Color32::from_rgb(232, 72, 85),
        Accent::Orange => Color32::from_rgb(240, 135, 50),
        Accent::Green => Color32::from_rgb(52, 180, 110),
        Accent::Teal => Color32::from_rgb(32, 178, 190),
    }
}

/// Color-label swatches (Red, Yellow, Green, Blue, Purple) — used for culling labels.
pub const LABEL_COLORS: [Color32; 5] = [
    Color32::from_rgb(224, 82, 82),
    Color32::from_rgb(230, 190, 60),
    Color32::from_rgb(90, 180, 100),
    Color32::from_rgb(80, 140, 230),
    Color32::from_rgb(160, 100, 210),
];

/// Every colour of one theme and accent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Accent for indicators, progress, links and accent text; legible on every surface.
    pub accent: Color32,
    pub sunken: Color32,
    pub chrome: Color32,
    pub sidebar: Color32,
    pub content: Color32,
    pub raised: Color32,
    /// Alternate list rows.
    pub stripe: Color32,
    /// Body text (labels); readable on every surface.
    pub text: Color32,
    pub border: Color32,
    /// Fill behind selected rows and tiles.
    pub selection: Color32,
    /// Text on `selection` (WCAG AA).
    pub selection_text: Color32,
    /// Keyboard focus ring around the cursor row or tile.
    pub focus: Color32,
    /// Background of the selected sidebar entry (with an accent bar beside it).
    pub nav_selected: Color32,
}

impl Palette {
    pub fn new(dark: bool, accent: Accent) -> Self {
        let base = accent_base(accent);
        let tint = |level| tinted(level, base, tint_strength(dark));
        if dark {
            let (sunken, chrome, sidebar, content, raised) =
                (tint(18), tint(23), tint(27), tint(31), tint(40));
            let selection = darken_until(mix(content, base, 0.45), Color32::WHITE, 4.5);
            Self {
                dark,
                accent: readable(base, raised, 4.5),
                sunken,
                chrome,
                sidebar,
                content,
                raised,
                stripe: tint(38),
                // egui's own dark body text (grey 140) is too dim on the raised surfaces.
                text: Color32::from_gray(170),
                border: tint(47),
                selection,
                selection_text: Color32::WHITE,
                focus: Color32::WHITE,
                nav_selected: tint(41),
            }
        } else {
            let (chrome, sidebar, content) = (tint(235), tint(242), tint(251));
            let selection = mix(content, base, 0.2);
            let selection_text = readable(mix(base, Color32::BLACK, 0.7), selection, 4.5);
            Self {
                dark,
                accent: readable(base, chrome, 4.5),
                sunken: Color32::WHITE,
                chrome,
                sidebar,
                content,
                raised: Color32::WHITE,
                stripe: tint(245),
                text: Color32::from_gray(60),
                border: tint(214),
                selection,
                selection_text,
                focus: readable(readable(base, selection, 3.0), content, 3.0),
                nav_selected: tint(229),
            }
        }
    }
}

/// The palette last applied with [`apply`].
pub fn palette(ctx: &egui::Context) -> Palette {
    ctx.data(|d| d.get_temp::<Palette>(palette_id()))
        .unwrap_or_else(|| Palette::new(ctx.global_style().visuals.dark_mode, Accent::default()))
}

/// The user's accent colour, adjusted for the current theme. Use it for anything that
/// should follow Settings › Appearance › Accent colour.
pub fn accent(ctx: &egui::Context) -> Color32 {
    palette(ctx).accent
}

/// Keyboard focus ring drawn around the cursor row or tile.
pub fn focus_ring(ctx: &egui::Context) -> Color32 {
    palette(ctx).focus
}

/// Frame for the left sidebar panel.
pub fn sidebar_frame(ctx: &egui::Context) -> Frame {
    Frame::side_top_panel(&ctx.global_style()).fill(palette(ctx).sidebar)
}

/// Frame for the central file panes.
pub fn content_frame(ctx: &egui::Context) -> Frame {
    Frame::central_panel(&ctx.global_style()).fill(palette(ctx).content)
}

/// Frame for side panels that show content (the preview).
pub fn content_side_frame(ctx: &egui::Context) -> Frame {
    Frame::side_top_panel(&ctx.global_style()).fill(palette(ctx).content)
}

fn palette_id() -> egui::Id {
    egui::Id::new("zxraw-theme-palette")
}

pub fn apply(ctx: &egui::Context, dark: bool, accent: Accent, scale: f32) {
    let p = Palette::new(dark, accent);
    ctx.data_mut(|d| d.insert_temp(palette_id(), p));
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    let base = accent_base(accent);
    let tint = |level| tinted(level, base, tint_strength(dark));
    v.panel_fill = p.chrome;
    v.window_fill = p.raised;
    v.extreme_bg_color = p.sunken;
    v.faint_bg_color = p.stripe;
    v.code_bg_color = p.stripe;
    v.window_stroke = Stroke::new(1.0, if dark { tint(54) } else { tint(208) });
    v.hyperlink_color = p.accent;
    v.widgets.noninteractive.fg_stroke.color = p.text;
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);
    // Keep selected list rows opaque, with text colors that retain WCAG AA contrast.
    v.selection.bg_fill = p.selection;
    v.selection.stroke = Stroke::new(1.0, p.selection_text);

    // Widgets: a quiet raised fill at rest, a soft lift on hover, a dip when pressed; no
    // outline flash or growth on hover.
    let (rest, hover, press, rest_edge, hover_edge, rail) = if dark {
        (tint(40), tint(48), tint(36), tint(52), tint(62), tint(54))
    } else {
        (
            Color32::WHITE,
            tint(238),
            tint(229),
            tint(210),
            tint(196),
            tint(222),
        )
    };
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.bg_fill = p.raised;
    w.noninteractive.weak_bg_fill = p.raised;
    w.inactive.weak_bg_fill = rest;
    w.inactive.bg_fill = rail;
    w.inactive.bg_stroke = Stroke::new(1.0, rest_edge);
    w.hovered.weak_bg_fill = hover;
    w.hovered.bg_fill = hover;
    w.hovered.bg_stroke = Stroke::new(1.0, hover_edge);
    w.active.weak_bg_fill = press;
    w.active.bg_fill = press;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.open.weak_bg_fill = hover;
    w.open.bg_fill = hover;
    w.open.bg_stroke = Stroke::new(1.0, hover_edge);
    let r = CornerRadius::same(5);
    for state in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        state.corner_radius = r;
        state.expansion = 0.0;
    }

    // Depth: soft, offset shadows under windows and popups; deeper in the dark theme where
    // a lighter surface alone reads less clearly.
    let shade = if dark { 140 } else { 64 };
    v.window_shadow = Shadow {
        offset: [0, 10],
        blur: 32,
        spread: 0,
        color: Color32::from_black_alpha(shade),
    };
    v.popup_shadow = Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(shade / 4 * 3),
    };
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.slider_trailing_fill = true;
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

/// Paints the accent bar marking the selected sidebar entry or tab: a short rounded pill
/// along the left edge (`vertical`) or the bottom edge of `rect`.
pub fn paint_indicator(ui: &egui::Ui, rect: egui::Rect, vertical: bool) {
    let accent = accent(ui.ctx());
    let bar = if vertical {
        let h = (rect.height() * 0.55).clamp(10.0, 18.0);
        egui::Rect::from_center_size(
            egui::pos2(rect.left() + 1.5, rect.center().y),
            egui::vec2(3.0, h),
        )
    } else {
        let w = (rect.width() * 0.4).clamp(12.0, 28.0);
        egui::Rect::from_center_size(
            egui::pos2(rect.center().x, rect.bottom() - 1.5),
            egui::vec2(w, 3.0),
        )
    };
    ui.painter().rect_filled(bar, CornerRadius::same(2), accent);
}

/// How far neutral surfaces lean towards the accent's hue: enough to feel cool with Blue or
/// warm with Orange, never enough to read as coloured.
fn tint_strength(dark: bool) -> f32 {
    if dark { 0.045 } else { 0.04 }
}

/// A grey of the given `level`, nudged towards `accent`'s hue without changing brightness.
fn tinted(level: u8, accent: Color32, amount: f32) -> Color32 {
    let [r, g, b, _] = accent.to_array().map(f32::from);
    let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let channel = |c: f32| {
        (f32::from(level) + (c - luma) * amount)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(channel(r), channel(g), channel(b))
}

/// `a` blended towards `b` by `t` (0 ..= 1), in gamma space.
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let channel = |x: u8, y: u8| {
        (f32::from(x) + (f32::from(y) - f32::from(x)) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(
        channel(a.r(), b.r()),
        channel(a.g(), b.g()),
        channel(a.b(), b.b()),
    )
}

/// `color`, lightened or darkened (away from `background`) just enough to reach `ratio`.
fn readable(color: Color32, background: Color32, ratio: f64) -> Color32 {
    let towards = if luminance(background) < 0.18 {
        Color32::WHITE
    } else {
        Color32::BLACK
    };
    (0..=20)
        .map(|step| mix(color, towards, step as f32 / 20.0))
        .find(|&c| contrast_ratio(c, background) >= ratio)
        .unwrap_or(towards)
}

/// `fill`, darkened just enough that `text` on it reaches `ratio`.
fn darken_until(fill: Color32, text: Color32, ratio: f64) -> Color32 {
    (0..=20)
        .map(|step| mix(fill, Color32::BLACK, step as f32 / 20.0))
        .find(|&c| contrast_ratio(text, c) >= ratio)
        .unwrap_or(Color32::BLACK)
}

/// WCAG 2.x relative luminance of an opaque color.
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

/// WCAG 2.x contrast ratio between two opaque colors (1.0 ..= 21.0).
fn contrast_ratio(a: Color32, b: Color32) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palettes() -> impl Iterator<Item = (Accent, Palette)> {
        ACCENTS.into_iter().flat_map(|(accent, _)| {
            [true, false]
                .into_iter()
                .map(move |dark| (accent, Palette::new(dark, accent)))
        })
    }

    #[test]
    fn selected_row_text_meets_wcag_aa() {
        for (accent, p) in palettes() {
            let ratio = contrast_ratio(p.selection_text, p.selection);
            assert!(
                ratio >= 4.5,
                "selection text ratio {ratio:.2} ({accent:?}, dark={})",
                p.dark
            );
        }
    }

    #[test]
    fn selected_tab_and_sidebar_text_meets_wcag_aa() {
        // Selected buttons draw `selection_text` on their own fill (tab, sidebar entry).
        for (accent, p) in palettes() {
            for fill in [p.content, p.nav_selected] {
                let ratio = contrast_ratio(p.selection_text, fill);
                assert!(
                    ratio >= 4.5,
                    "selected text ratio {ratio:.2} on {fill:?} ({accent:?}, dark={})",
                    p.dark
                );
            }
        }
    }

    #[test]
    fn accent_text_meets_wcag_aa_on_every_surface() {
        for (accent, p) in palettes() {
            for surface in [p.sunken, p.chrome, p.sidebar, p.content, p.raised] {
                let ratio = contrast_ratio(p.accent, surface);
                assert!(
                    ratio >= 4.5,
                    "accent ratio {ratio:.2} on {surface:?} ({accent:?}, dark={})",
                    p.dark
                );
            }
        }
    }

    #[test]
    fn focus_ring_meets_non_text_contrast_on_selection_and_panel() {
        for (accent, p) in palettes() {
            for background in [p.selection, p.content, p.nav_selected] {
                let ratio = contrast_ratio(p.focus, background);
                assert!(
                    ratio >= 3.0,
                    "focus ring ratio {ratio:.2} on {background:?} ({accent:?}, dark={})",
                    p.dark
                );
            }
        }
    }

    #[test]
    fn surfaces_are_layered_and_body_text_stays_readable() {
        for (accent, p) in palettes() {
            let layers = [p.chrome, p.sidebar, p.content];
            let levels: Vec<f64> = layers.iter().map(|&c| luminance(c)).collect();
            assert!(
                levels.windows(2).all(|w| w[0] < w[1]),
                "surfaces not ordered: {layers:?} ({accent:?}, dark={})",
                p.dark
            );
            for surface in [p.chrome, p.sidebar, p.content, p.raised, p.nav_selected] {
                let ratio = contrast_ratio(p.text, surface);
                assert!(
                    ratio >= 4.5,
                    "text ratio {ratio:.2} on {surface:?} ({accent:?})"
                );
            }
        }
    }

    #[test]
    fn old_settings_files_keep_the_blue_accent() {
        let old: crate::settings::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(old.accent, Accent::Blue);
        assert_eq!(accent_base(old.accent), ACCENT);
        let accents: Vec<Accent> = ACCENTS.iter().map(|(a, _)| *a).collect();
        assert_eq!(accents[0], Accent::default());
    }
}
