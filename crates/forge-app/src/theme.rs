//! Visual design: layered neutral surfaces faintly tinted towards one accent colour (chosen
//! in Settings › Appearance), soft shadows under everything that floats, gentle rounding.
//!
//! Surfaces, darkest to lightest in the dark theme: `sunken` (text fields), `chrome` (tab
//! strip, toolbar, status bar), `sidebar`, `content` (file list, preview) and `raised`
//! (windows, menus, popups). Code that paints with the accent calls [`accent`].

use egui::{
    Color32, CornerRadius, FontFamily, FontId, Frame, Margin, Shadow, Stroke, TextStyle, Visuals,
};

use crate::settings::Accent;

/// The default accent (Blue). Prefer [`accent`], which follows the user's choice and keeps
/// its contrast on the light theme.
pub const ACCENT: Color32 = Color32::from_rgb(72, 118, 176);

/// Accent presets offered in Settings › Appearance, in swatch order.
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
    /// Text fields in dialogs.
    pub sunken: Color32,
    /// The window itself: tab strip, toolbar, sidebar, status bar and the gutters between
    /// the content cards.
    pub chrome: Color32,
    /// Sidebar background; the sidebar sits directly on the chrome.
    pub sidebar: Color32,
    /// The raised cards that hold the file listing and the preview.
    pub content: Color32,
    /// Windows, menus and popups.
    pub raised: Color32,
    /// Alternate list rows.
    pub stripe: Color32,
    /// The slim strip behind the details view's column headers.
    pub header: Color32,
    /// Hovered rows and tiles on a card.
    pub hover: Color32,
    /// Hairline around cards and the active tab.
    pub card_border: Color32,
    /// Fill of the filter and address pills on the toolbar.
    pub field: Color32,
    /// Body text (labels); readable on every surface.
    pub text: Color32,
    pub border: Color32,
    /// Fill behind selected rows.
    pub selection: Color32,
    /// Text on `selection` (WCAG AA).
    pub selection_text: Color32,
    /// Quiet accent-tinted fill: selected tiles, the active view-mode pill.
    pub accent_soft: Color32,
    /// Accent-coloured text on `accent_soft` (WCAG AA).
    pub accent_soft_text: Color32,
    /// Keyboard focus ring around the cursor row or tile.
    pub focus: Color32,
    /// Background of the selected sidebar entry (with an accent bar beside it).
    pub nav_selected: Color32,
}

impl Palette {
    pub fn new(dark: bool, accent: Accent) -> Self {
        let base = accent_base(accent);
        // Surfaces are OKLab lightness levels with a little of the accent's hue, enough
        // that Green reads green-ish and Purple purple-ish across the whole window.
        let tint = |lightness, chroma| surface(lightness, chroma, base);
        if dark {
            let chroma = 0.018;
            let content = tint(0.292, chroma);
            let raised = tint(0.322, chroma);
            let chrome = tint(0.205, chroma);
            let selection = darken_until(mix(content, base, 0.6), Color32::WHITE, 4.5);
            let text = tint(0.79, 0.008);
            // Selected tiles show body text on this tint, so it never gets too bright.
            let accent_soft = darken_until(mix(content, base, 0.3), text, 4.5);
            Self {
                dark,
                accent: readable(base, raised, 4.5),
                sunken: tint(0.18, chroma),
                chrome,
                sidebar: chrome,
                content,
                raised,
                stripe: tint(0.302, chroma),
                header: tint(0.262, chroma),
                hover: tint(0.338, chroma),
                card_border: tint(0.36, chroma),
                field: tint(0.255, chroma),
                // egui's own dark body text (grey 140) is too dim on the raised surfaces.
                text,
                border: tint(0.37, chroma),
                selection,
                selection_text: Color32::WHITE,
                accent_soft,
                accent_soft_text: readable(mix(base, Color32::WHITE, 0.35), accent_soft, 4.5),
                focus: Color32::WHITE,
                nav_selected: tint(0.285, 0.03),
            }
        } else {
            let chroma = 0.012;
            let chrome = tint(0.935, chroma);
            let content = tint(0.997, 0.003);
            let selection = mix(content, base, 0.2);
            let selection_text = readable(mix(base, Color32::BLACK, 0.7), selection, 4.5);
            let accent_soft = mix(content, base, 0.14);
            let nav_selected = tint(0.895, 0.03);
            Self {
                dark,
                accent: readable(base, chrome, 4.5),
                sunken: Color32::WHITE,
                chrome,
                sidebar: chrome,
                content,
                raised: Color32::WHITE,
                stripe: tint(0.978, 0.005),
                header: tint(0.962, 0.008),
                hover: tint(0.952, 0.014),
                card_border: tint(0.885, chroma),
                field: Color32::WHITE,
                text: tint(0.36, 0.01),
                border: tint(0.87, chroma),
                selection,
                selection_text,
                accent_soft,
                accent_soft_text: readable(mix(base, Color32::BLACK, 0.45), accent_soft, 4.5),
                focus: readable(readable(base, selection, 3.0), nav_selected, 3.0),
                nav_selected,
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

/// Gap between the window edge, the sidebar and the content cards.
const GUTTER: i8 = 8;

/// Frame for the tab strip, toolbar and status bar: plain chrome.
pub fn chrome_frame(ctx: &egui::Context, margin: Margin) -> Frame {
    Frame::new().fill(palette(ctx).chrome).inner_margin(margin)
}

/// Frame for the left sidebar panel; the sidebar sits on the chrome.
pub fn sidebar_frame(ctx: &egui::Context) -> Frame {
    Frame::new()
        .fill(palette(ctx).sidebar)
        .inner_margin(Margin {
            left: GUTTER,
            right: 2,
            top: 4,
            bottom: 4,
        })
}

/// Frame for the central panel: chrome around the card(s) drawn with [`card`].
pub fn content_frame(ctx: &egui::Context) -> Frame {
    chrome_frame(
        ctx,
        Margin {
            left: GUTTER,
            right: GUTTER,
            top: 2,
            bottom: GUTTER,
        },
    )
}

/// Frame for side panels that hold a card (the preview).
pub fn content_side_frame(ctx: &egui::Context) -> Frame {
    chrome_frame(
        ctx,
        Margin {
            left: 0,
            right: GUTTER,
            top: 2,
            bottom: GUTTER,
        },
    )
}

/// The raised, rounded card that holds a file listing or the preview. `highlight` gives it
/// a faint accent edge and glow (the focused pane in dual-pane mode).
pub fn card_frame(ctx: &egui::Context, highlight: bool) -> Frame {
    let p = palette(ctx);
    let (stroke, shadow) = if highlight {
        (
            Stroke::new(1.0, p.accent.gamma_multiply(0.7)),
            Shadow {
                offset: [0, 0],
                blur: 14,
                spread: 0,
                color: p.accent.gamma_multiply(if p.dark { 0.3 } else { 0.25 }),
            },
        )
    } else {
        (
            Stroke::new(1.0, p.card_border),
            Shadow {
                offset: [0, 2],
                blur: 12,
                spread: 0,
                color: Color32::from_black_alpha(if p.dark { 110 } else { 22 }),
            },
        )
    };
    Frame::new()
        .fill(p.content)
        .stroke(stroke)
        .shadow(shadow)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(6))
}

/// Shows `add_contents` in a [`card_frame`] that fills the available space.
pub fn card<R>(
    ui: &mut egui::Ui,
    highlight: bool,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    card_frame(ui.ctx(), highlight)
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            add_contents(ui)
        })
        .inner
}

/// Lays out `add_contents` in exactly the available width. Anything wider is clipped
/// instead of widening `ui`: in a side panel that would grow the panel past its size and
/// slide it under the listing.
pub fn clip_to_width<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let rect = ui.available_rect_before_wrap();
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(*ui.layout()));
    child.shrink_clip_rect(rect);
    let r = add_contents(&mut child);
    let bottom = child.min_rect().bottom().min(rect.bottom());
    ui.advance_cursor_after_rect(egui::Rect::from_min_max(rect.min, egui::pos2(rect.right(), bottom)));
    r
}

/// A toolbar toggle: a ghost icon button that becomes an accent-tinted pill when `selected`.
pub fn pill_toggle(ui: &mut egui::Ui, selected: bool, icon: &str) -> egui::Response {
    let p = palette(ui.ctx());
    let button = if selected {
        egui::Button::new(egui::RichText::new(icon).color(p.accent_soft_text))
            .fill(p.accent_soft)
            .stroke(Stroke::new(1.0, p.accent.gamma_multiply(0.45)))
    } else {
        egui::Button::new(icon).frame_when_inactive(false)
    };
    ui.add(
        button
            .selected(selected)
            .corner_radius(CornerRadius::same(12))
            .min_size(egui::vec2(32.0, 24.0)),
    )
}

/// Styles text fields in `ui` as the toolbar's rounded pills: a hairline at rest and an
/// accent outline while focused.
pub fn pill_field_style(ui: &mut egui::Ui) {
    let p = palette(ui.ctx());
    let v = ui.visuals_mut();
    for state in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
    ] {
        state.corner_radius = CornerRadius::same(13);
    }
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.card_border);
    v.selection.stroke = Stroke::new(1.5, p.accent);
}

/// A toolbar icon button with no fill until hovered.
pub fn ghost_button(icon: &str) -> egui::Button<'_> {
    egui::Button::new(icon)
        .frame_when_inactive(false)
        .corner_radius(CornerRadius::same(7))
        .min_size(egui::vec2(28.0, 24.0))
}

/// Paints a soft glow of `color` around `rect` (the bar under the active tab).
pub fn paint_glow(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let glow = Shadow {
        offset: [0, 0],
        blur: 10,
        spread: 1,
        color: color.gamma_multiply(0.55),
    };
    painter.add(glow.as_shape(rect, CornerRadius::same(2)));
}

/// Paints `rect` with a left-to-right gradient from `from` to `to`.
pub fn paint_hgradient(painter: &egui::Painter, rect: egui::Rect, from: Color32, to: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), from);
    mesh.colored_vertex(rect.right_top(), to);
    mesh.colored_vertex(rect.left_bottom(), from);
    mesh.colored_vertex(rect.right_bottom(), to);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
    painter.add(mesh);
}

/// The far end of the accent gradient (drive usage bar): the accent, a little lighter and
/// turned ~35° in hue.
pub fn accent_gradient_end(ctx: &egui::Context) -> Color32 {
    let [l, a, b] = to_oklab(accent(ctx));
    let (sin, cos) = 0.6_f32.sin_cos();
    from_oklab([l + 0.04, a * cos - b * sin, a * sin + b * cos])
}

/// Icons of plain files and documents: a grey leaning towards the accent.
pub fn neutral_icon(ctx: &egui::Context) -> Color32 {
    let p = palette(ctx);
    mix(mix(p.text, p.chrome, 0.25), p.accent, 0.3)
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
    let tint = |lightness| surface(lightness, 0.018, accent_base(accent));
    v.panel_fill = p.chrome;
    v.window_fill = p.raised;
    v.extreme_bg_color = p.sunken;
    v.faint_bg_color = p.stripe;
    v.code_bg_color = p.stripe;
    v.window_stroke = Stroke::new(1.0, if dark { tint(0.4) } else { tint(0.86) });
    v.hyperlink_color = p.accent;
    v.widgets.noninteractive.fg_stroke.color = p.text;
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);
    // Keep selected list rows opaque, with text colors that retain WCAG AA contrast.
    v.selection.bg_fill = p.selection;
    v.selection.stroke = Stroke::new(1.0, p.selection_text);

    // Widgets: a quiet raised fill at rest, a soft lift on hover, a dip when pressed; no
    // outline flash or growth on hover.
    let (rest, hover, press, rest_edge, hover_edge, rail) = if dark {
        (
            tint(0.36),
            tint(0.4),
            tint(0.335),
            tint(0.4),
            tint(0.46),
            tint(0.37),
        )
    } else {
        (
            Color32::WHITE,
            tint(0.952),
            tint(0.925),
            tint(0.86),
            tint(0.8),
            tint(0.9),
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
    if !vertical {
        paint_glow(ui.painter(), bar, accent);
    }
    ui.painter().rect_filled(bar, CornerRadius::same(2), accent);
}

/// A neutral surface: OKLab `lightness` with `chroma` in the hue of `accent`.
fn surface(lightness: f32, chroma: f32, accent: Color32) -> Color32 {
    let [_, a, b] = to_oklab(accent);
    let len = a.hypot(b).max(1e-6);
    from_oklab([lightness, a / len * chroma, b / len * chroma])
}

fn to_linear(c: u8) -> f32 {
    let s = f32::from(c) / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

fn from_linear(l: f32) -> u8 {
    let s = if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round().clamp(0.0, 255.0) as u8
}

/// sRGB to OKLab (Björn Ottosson's matrices).
fn to_oklab(c: Color32) -> [f32; 3] {
    let (r, g, b) = (to_linear(c.r()), to_linear(c.g()), to_linear(c.b()));
    let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_99 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

fn from_oklab([l, a, b]: [f32; 3]) -> Color32 {
    let l_ = (l + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m_ = (l - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
    let s_ = (l - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    Color32::from_rgb(
        from_linear(4.076_741_7 * l_ - 3.307_711_6 * m_ + 0.230_969_94 * s_),
        from_linear(-1.268_438 * l_ + 2.609_757_4 * m_ - 0.341_319_38 * s_),
        from_linear(-0.004_196_086_3 * l_ - 0.703_418_6 * m_ + 1.707_614_7 * s_),
    )
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
        // CIE L* (0 ..= 100) of a colour.
        let lightness = |c: Color32| {
            let y = luminance(c);
            if y > 0.008_856 {
                116.0 * y.cbrt() - 16.0
            } else {
                903.3 * y
            }
        };
        for (accent, p) in palettes() {
            // The content card must stand clearly off the chrome around it.
            let step = lightness(p.content) - lightness(p.chrome);
            let wanted = if p.dark { 8.0..=14.0 } else { 3.0..=10.0 };
            assert!(
                wanted.contains(&step),
                "card vs chrome L* step {step:.1} ({accent:?}, dark={})",
                p.dark
            );
            for surface in [
                p.chrome,
                p.sidebar,
                p.content,
                p.raised,
                p.nav_selected,
                p.stripe,
                p.header,
                p.hover,
                p.field,
            ] {
                let ratio = contrast_ratio(p.text, surface);
                assert!(
                    ratio >= 4.5,
                    "text ratio {ratio:.2} on {surface:?} ({accent:?})"
                );
            }
        }
    }

    #[test]
    fn accent_pill_and_selected_tile_text_meet_wcag_aa() {
        for (accent, p) in palettes() {
            for (text, what) in [(p.accent_soft_text, "pill icon"), (p.text, "tile label")] {
                let ratio = contrast_ratio(text, p.accent_soft);
                assert!(
                    ratio >= 4.5,
                    "{what} ratio {ratio:.2} on {:?} ({accent:?}, dark={})",
                    p.accent_soft,
                    p.dark
                );
            }
        }
    }

    #[test]
    fn neutrals_carry_a_visible_tint_of_the_accent() {
        for (accent, p) in palettes() {
            let [_, a, b] = to_oklab(p.chrome);
            let chroma = a.hypot(b);
            assert!(
                (0.008..=0.03).contains(&chroma),
                "chrome chroma {chroma:.3} ({accent:?}, dark={})",
                p.dark
            );
        }
    }

    #[test]
    fn oklab_round_trips() {
        for c in [
            ACCENT,
            Color32::from_gray(37),
            Color32::from_rgb(232, 72, 85),
        ] {
            assert_eq!(from_oklab(to_oklab(c)), c);
        }
    }

    #[test]
    fn old_settings_files_keep_the_blue_accent() {
        let old: crate::settings::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(*old.accent, Accent::Blue);
        assert_eq!(accent_base(*old.accent), ACCENT);
        let accents: Vec<Accent> = ACCENTS.iter().map(|(a, _)| *a).collect();
        assert_eq!(accents[0], Accent::default());
    }
}
