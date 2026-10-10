//! Viewer palette and small painting helpers (kept local so the module is self-contained;
//! values match `theme.rs`).

use egui::{Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Stroke, vec2};

use super::cull::Annotation;

pub const ACCENT: Color32 = Color32::from_rgb(77, 144, 254);
pub const LABEL_COLORS: [Color32; 5] = [
    Color32::from_rgb(224, 82, 82),
    Color32::from_rgb(230, 190, 60),
    Color32::from_rgb(90, 180, 100),
    Color32::from_rgb(80, 140, 230),
    Color32::from_rgb(160, 100, 210),
];
pub const CANVAS_BG: Color32 = Color32::from_rgb(17, 17, 19);
pub const BAR_BG: Color32 = Color32::from_rgb(27, 28, 32);
pub const BAR_BORDER: Color32 = Color32::from_rgb(44, 46, 52);
pub const STRIP_BG: Color32 = Color32::from_rgb(22, 23, 26);
pub const TEXT: Color32 = Color32::from_rgb(222, 224, 228);
pub const MUTED: Color32 = Color32::from_rgb(138, 142, 150);
pub const FAINT: Color32 = Color32::from_rgb(84, 87, 94);
pub const STAR: Color32 = Color32::from_rgb(250, 200, 60);
pub const PICK: Color32 = Color32::from_rgb(90, 200, 110);
pub const REJECT: Color32 = Color32::from_rgb(230, 80, 80);
pub const OVERLAY_BG: Color32 = Color32::from_rgba_premultiplied(14, 14, 16, 215);

pub fn pick_color(pick: i8) -> Color32 {
    match pick {
        1 => PICK,
        -1 => REJECT,
        _ => MUTED,
    }
}

/// Rounded "pill" with text; returns its rect.
pub fn pill(
    p: &Painter,
    anchor: Pos2,
    align: Align2,
    text: &str,
    fg: Color32,
    bg: Color32,
    size: f32,
) -> Rect {
    let galley = p.layout_no_wrap(text.to_string(), FontId::proportional(size), fg);
    let pad = vec2(7.0, 3.0);
    let r = align.anchor_size(anchor, galley.size() + pad * 2.0);
    p.rect_filled(
        r,
        CornerRadius::same(((r.height() / 2.0) as u8).min(10)),
        bg,
    );
    p.galley(r.min + pad, galley, fg);
    r
}

/// Annotation badges on a thumbnail/panel rect (stars bottom-left, label dot top-right,
/// pick flag / reject dimming top-left).
pub fn draw_annotation(p: &Painter, r: Rect, a: Annotation, scale: f32) {
    if a.pick < 0 {
        p.rect_filled(r, 2.0, Color32::from_black_alpha(140));
    }
    if a.rating > 0 {
        let stars: String = "★".repeat(a.rating.min(5) as usize);
        let g = p.layout_no_wrap(stars, FontId::proportional(11.0 * scale), STAR);
        let pos = r.left_bottom() + vec2(4.0 * scale, -3.0 * scale - g.size().y);
        let bg = Rect::from_min_size(pos - vec2(2.0, 0.0), g.size() + vec2(4.0, 0.0));
        p.rect_filled(bg, 3.0, Color32::from_black_alpha(140));
        p.galley(pos, g, STAR);
    }
    if let Some(c) = a.label.and_then(|l| LABEL_COLORS.get(l as usize)) {
        let c0 = r.right_top() + vec2(-8.0, 8.0) * scale;
        p.circle(
            c0,
            4.5 * scale,
            *c,
            Stroke::new(1.0, Color32::from_black_alpha(160)),
        );
    }
    match a.pick {
        1 => {
            p.text(
                r.left_top() + vec2(4.0, 2.0) * scale,
                Align2::LEFT_TOP,
                "⚑",
                FontId::proportional(13.0 * scale),
                PICK,
            );
        }
        -1 => {
            p.text(
                r.left_top() + vec2(4.0, 2.0) * scale,
                Align2::LEFT_TOP,
                "✖",
                FontId::proportional(12.0 * scale),
                REJECT,
            );
        }
        _ => {}
    }
}

pub fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}
