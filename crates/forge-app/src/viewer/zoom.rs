//! Pure zoom / pan math for the viewer canvas.
//!
//! Coordinates:
//! * `img` — the *natural* image size in image pixels (after EXIF orientation). Whatever
//!   texture level is currently loaded (thumbnail, screen preview, full decode) is
//!   stretched over the same natural rectangle, so switching levels never moves the view.
//! * scale — screen points per natural image pixel. "100 %" (actual pixels) is
//!   `1 / pixels_per_point`, passed around as `one`.
//! * `center` — the image point shown at the centre of the view, normalised to `0..=1`
//!   per axis, so it survives resolution changes and can be shared between compare panels.

use egui::{Pos2, Rect, Vec2, vec2};

/// Largest magnification relative to actual pixels.
pub const MAX_ZOOM_OVER_ACTUAL: f32 = 16.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Zoom {
    /// Fit the whole image into the view (never upscaled beyond actual pixels).
    Fit,
    /// Fixed scale in screen points per natural image pixel.
    Scale(f32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewState {
    pub zoom: Zoom,
    pub center: Vec2,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            zoom: Zoom::Fit,
            center: vec2(0.5, 0.5),
        }
    }
}

/// Scale that fits `img` into `view`, capped at `max_scale` (actual pixels).
pub fn fit_scale(img: Vec2, view: Vec2, max_scale: f32) -> f32 {
    if img.x <= 0.0 || img.y <= 0.0 || view.x <= 0.0 || view.y <= 0.0 {
        return max_scale.max(f32::MIN_POSITIVE);
    }
    (view.x / img.x).min(view.y / img.y).min(max_scale)
}

/// Clamps a normalised centre so the image never leaves a gap at an edge it could cover;
/// axes where the scaled image is smaller than the view are centred.
pub fn clamp_center(center: Vec2, scaled: Vec2, view: Vec2) -> Vec2 {
    let axis = |c: f32, size: f32, v: f32| {
        if size <= v || size <= 0.0 {
            0.5
        } else {
            let half = v / (2.0 * size);
            if c.is_finite() { c.clamp(half, 1.0 - half) } else { 0.5 }
        }
    };
    vec2(axis(center.x, scaled.x, view.x), axis(center.y, scaled.y, view.y))
}

/// Allowed scale range for an image in a view.
pub fn scale_bounds(img: Vec2, view: Vec2, one: f32) -> (f32, f32) {
    let fit = fit_scale(img, view, one);
    (fit.min(one) * 0.5, one * MAX_ZOOM_OVER_ACTUAL)
}

impl ViewState {
    pub fn scale(&self, img: Vec2, view: Rect, one: f32) -> f32 {
        match self.zoom {
            Zoom::Fit => fit_scale(img, view.size(), one),
            Zoom::Scale(s) => s,
        }
    }

    /// Screen rectangle covered by the whole (natural) image.
    pub fn image_rect(&self, img: Vec2, view: Rect, one: f32) -> Rect {
        let s = self.scale(img, view, one);
        let size = img * s;
        let c = match self.zoom {
            Zoom::Fit => vec2(0.5, 0.5),
            Zoom::Scale(_) => clamp_center(self.center, size, view.size()),
        };
        let min = view.center() - vec2(c.x * size.x, c.y * size.y);
        Rect::from_min_size(min, size)
    }

    /// Image pixel (natural coordinates) under a screen position.
    pub fn screen_to_image(&self, img: Vec2, view: Rect, one: f32, p: Pos2) -> Vec2 {
        let r = self.image_rect(img, view, one);
        let s = r.width() / img.x.max(f32::MIN_POSITIVE);
        (p - r.min) / s
    }

    /// Sets the scale to `new_scale` keeping the image pixel under `anchor` fixed on screen
    /// (subject to pan clamping at the image edges).
    pub fn zoom_about(&mut self, img: Vec2, view: Rect, one: f32, anchor: Pos2, new_scale: f32) {
        if img.x <= 0.0 || img.y <= 0.0 {
            return;
        }
        let (lo, hi) = scale_bounds(img, view.size(), one);
        let new_scale = new_scale.clamp(lo, hi);
        let p = self.screen_to_image(img, view, one, anchor);
        let new_size = img * new_scale;
        let new_min = anchor - p * new_scale;
        let c = view.center() - new_min;
        self.center = clamp_center(
            vec2(c.x / new_size.x, c.y / new_size.y),
            new_size,
            view.size(),
        );
        self.zoom = Zoom::Scale(new_scale);
    }

    /// Multiplies the current scale by `factor` around `anchor`.
    pub fn zoom_by(&mut self, img: Vec2, view: Rect, one: f32, anchor: Pos2, factor: f32) {
        let s = self.scale(img, view, one);
        self.zoom_about(img, view, one, anchor, s * factor);
    }

    /// Pans by a screen-space drag delta.
    pub fn pan(&mut self, img: Vec2, view: Rect, _one: f32, delta: Vec2) {
        let Zoom::Scale(s) = self.zoom else {
            return;
        };
        let size = img * s;
        if size.x <= 0.0 || size.y <= 0.0 {
            return;
        }
        let c = clamp_center(self.center, size, view.size());
        self.center = clamp_center(
            c - vec2(delta.x / size.x, delta.y / size.y),
            size,
            view.size(),
        );
    }

    /// True when showing actual pixels.
    pub fn is_actual(&self, one: f32) -> bool {
        matches!(self.zoom, Zoom::Scale(s) if (s - one).abs() <= one * 1e-3)
    }

    /// Fit ↔ 100 % toggle; zooming in keeps the pixel under `anchor` fixed.
    pub fn toggle_actual(&mut self, img: Vec2, view: Rect, one: f32, anchor: Pos2) {
        if self.is_actual(one) {
            *self = Self::default();
        } else {
            self.zoom_about(img, view, one, anchor, one);
        }
    }

    /// Zoom as a percentage of actual pixels.
    pub fn percent(&self, img: Vec2, view: Rect, one: f32) -> f32 {
        self.scale(img, view, one) / one * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    fn view() -> Rect {
        Rect::from_min_size(pos2(100.0, 50.0), vec2(800.0, 600.0))
    }

    #[test]
    fn fit_computation() {
        // Landscape wider than view: limited by width.
        assert!((fit_scale(vec2(4000.0, 2000.0), vec2(800.0, 600.0), 1.0) - 0.2).abs() < 1e-6);
        // Portrait: limited by height.
        assert!((fit_scale(vec2(3000.0, 6000.0), vec2(800.0, 600.0), 1.0) - 0.1).abs() < 1e-6);
        // Small image is not upscaled beyond actual pixels.
        assert_eq!(fit_scale(vec2(100.0, 50.0), vec2(800.0, 600.0), 1.0), 1.0);
        // HiDPI: actual pixels is 0.5 points per pixel.
        assert_eq!(fit_scale(vec2(100.0, 50.0), vec2(800.0, 600.0), 0.5), 0.5);
        // Degenerate input does not divide by zero.
        assert!(fit_scale(vec2(0.0, 10.0), vec2(800.0, 600.0), 1.0).is_finite());

        let v = ViewState::default();
        let r = v.image_rect(vec2(4000.0, 2000.0), view(), 1.0);
        assert!((r.width() - 800.0).abs() < 1e-3);
        assert!((r.height() - 400.0).abs() < 1e-3);
        assert!((r.center() - view().center()).length() < 1e-3);
    }

    #[test]
    fn zoom_around_cursor_keeps_pixel_fixed() {
        let img = vec2(6000.0, 4000.0);
        let mut v = ViewState::default();
        let anchor = pos2(300.0, 200.0);
        let before = v.screen_to_image(img, view(), 1.0, anchor);
        v.zoom_about(img, view(), 1.0, anchor, 0.5);
        let after = v.screen_to_image(img, view(), 1.0, anchor);
        assert!((before - after).length() < 0.05, "{before:?} vs {after:?}");
        // Repeated wheel steps keep it fixed too.
        for _ in 0..5 {
            v.zoom_by(img, view(), 1.0, anchor, 1.25);
            let p = v.screen_to_image(img, view(), 1.0, anchor);
            assert!((before - p).length() < 0.1, "{before:?} vs {p:?}");
        }
        // Zoom out back toward fit stays well-defined.
        v.zoom_by(img, view(), 1.0, anchor, 0.01);
        let r = v.image_rect(img, view(), 1.0);
        assert!(r.width() > 0.0 && r.width().is_finite());
    }

    #[test]
    fn pan_clamping() {
        let img = vec2(2000.0, 1000.0);
        let mut v = ViewState {
            zoom: Zoom::Scale(1.0),
            center: vec2(0.5, 0.5),
        };
        // Drag far to the right: left edge of the image must not leave the view's left edge.
        v.pan(img, view(), 1.0, vec2(10_000.0, 10_000.0));
        let r = v.image_rect(img, view(), 1.0);
        assert!((r.left() - view().left()).abs() < 1e-3);
        assert!((r.top() - view().top()).abs() < 1e-3);
        // Drag far the other way: right/bottom edges pinned.
        v.pan(img, view(), 1.0, vec2(-1e6, -1e6));
        let r = v.image_rect(img, view(), 1.0);
        assert!((r.right() - view().right()).abs() < 1e-2);
        assert!((r.bottom() - view().bottom()).abs() < 1e-2);
        // Axis smaller than the view is centred.
        let c = clamp_center(vec2(0.0, 0.9), vec2(400.0, 2000.0), vec2(800.0, 600.0));
        assert_eq!(c.x, 0.5);
        assert!((c.y - 0.85).abs() < 1e-6);
    }

    #[test]
    fn toggle_fit_actual() {
        let img = vec2(6000.0, 4000.0);
        let mut v = ViewState::default();
        let anchor = view().center();
        v.toggle_actual(img, view(), 1.0, anchor);
        assert!(v.is_actual(1.0));
        assert!((v.percent(img, view(), 1.0) - 100.0).abs() < 1e-3);
        v.toggle_actual(img, view(), 1.0, anchor);
        assert_eq!(v.zoom, Zoom::Fit);
    }
}
