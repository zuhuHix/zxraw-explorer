//! Image canvas: drawing, zoom/pan interaction, status cards and overlays.

use std::time::Duration;

use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Painter, Pos2, Rect, Sense, Stroke, Ui,
    Vec2, pos2, vec2,
};

use super::cull::{self, AnnotationStore};
use super::decode::DecodeError;
use super::loader::Level;
use super::style::{self, ACCENT, BAR_BG, BAR_BORDER, MUTED, OVERLAY_BG, TEXT};
use super::zoom::ViewState;
use super::{Viewer, ViewerMode, ZoomKey};

const UV: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

impl Viewer {
    /// Natural (oriented, full-resolution) size for zoom math, if anything is known yet.
    fn natural_of(&self, item: usize) -> Option<Vec2> {
        if let Some([w, h]) = self.loader.natural(item) {
            return Some(vec2(w as f32, h as f32));
        }
        let (ph, video) = self.loader.placeholder_size(item)?;
        let ph = vec2(ph[0] as f32, ph[1] as f32);
        // Use EXIF dimensions when they agree with the placeholder's aspect ratio, so the
        // zoom level doesn't jump when the screen preview replaces the placeholder.
        if !video && let Some((w, h)) = self.meta.get(item).and_then(|m| m.dims) {
            let (w, h) = (w as f32, h as f32);
            if w > 0.0 && h > 0.0 && ((w / h) / (ph.x / ph.y) - 1.0).abs() < 0.02 {
                return Some(vec2(w, h));
            }
        }
        Some(ph)
    }

    /// Paints `item` into `rect` with `view` (no interaction). Returns false if there is
    /// nothing to paint yet.
    fn paint_item(&mut self, p: &Painter, rect: Rect, item: usize, view: &ViewState) -> bool {
        let ppp = p.ctx().pixels_per_point();
        let one = 1.0 / ppp;
        let Some(nat) = self.natural_of(item) else {
            return false;
        };
        let img_rect = view.image_rect(nat, rect, one);
        let need_px = img_rect.width() * ppp;
        let Some((level, tex)) = self.loader.pick(item, need_px) else {
            return false;
        };
        for (t, r) in &tex.tiles {
            let tr = Rect::from_min_max(
                img_rect.lerp_inside(r.min.to_vec2()),
                img_rect.lerp_inside(r.max.to_vec2()),
            );
            if tr.intersects(rect) {
                p.image(t.id(), tr, UV, Color32::WHITE);
            }
        }
        // Zoomed past what the loaded texture can show: ask for a full-resolution decode.
        let full_known = self.loader.natural(item).is_some();
        if level != Level::Full
            && full_known
            && need_px > tex.size[0] as f32 * 1.05
            && tex.size[0] < tex.natural[0]
            && !self.need_full.contains(&item)
        {
            self.need_full.push(item);
        }
        true
    }

    /// Interactive canvas for one item. `view` is updated by wheel/drag/double-click.
    fn draw_item(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        item: usize,
        view: &mut ViewState,
        id: Id,
    ) -> egui::Response {
        let resp = ui.interact(rect, id, Sense::click_and_drag());
        let one = 1.0 / ui.ctx().pixels_per_point();
        if let Some(nat) = self.natural_of(item) {
            if resp.hovered() {
                let (scroll, zd, hover) = ui.input(|i| {
                    (
                        i.smooth_scroll_delta.y,
                        i.zoom_delta(),
                        i.pointer.hover_pos(),
                    )
                });
                if let Some(h) = hover {
                    if (zd - 1.0).abs() > 1e-4 {
                        view.zoom_by(nat, rect, one, h, zd);
                    } else if scroll.abs() > 0.0 {
                        view.zoom_by(nat, rect, one, h, (scroll * 0.0018).exp());
                    }
                }
            }
            if resp.dragged() {
                view.pan(nat, rect, one, resp.drag_delta());
                ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            } else if resp.hovered() && matches!(view.zoom, super::Zoom::Scale(_)) {
                ui.ctx().set_cursor_icon(CursorIcon::Grab);
            }
            if resp.double_clicked()
                && let Some(h) = resp.interact_pointer_pos()
            {
                view.toggle_actual(nat, rect, one, h);
            }
        }
        let p = ui.painter_at(rect);
        if !self.paint_item(&p, rect, item, view) {
            self.status_card(&p, rect, item);
        } else {
            self.item_badges(&p, rect, item);
        }
        resp
    }

    /// Error / loading / "needs FFmpeg" card when there is no image to show.
    fn status_card(&self, p: &Painter, rect: Rect, item: usize) {
        let err = self
            .loader
            .error((item, Level::Screen))
            .or_else(|| self.loader.error((item, Level::Quick)));
        let name = self.file_name(item);
        let (icon, title, body, icon_col): (&str, String, String, Color32) = match err {
            None => {
                // Still loading.
                let c = rect.center();
                spinner(p, c - vec2(0.0, 14.0), 11.0);
                p.text(
                    c + vec2(0.0, 14.0),
                    Align2::CENTER_TOP,
                    &name,
                    FontId::proportional(13.0),
                    MUTED,
                );
                p.ctx().request_repaint_after(Duration::from_millis(30));
                return;
            }
            Some(DecodeError::NoFfmpeg) => {
                let mut body = String::from(
                    "Install FFmpeg and make sure ffmpeg.exe is on PATH (or set its folder in \
                     Settings). ZXraw Explorer then shows a representative frame here.",
                );
                if let Some(sz) = self.meta.get(item).and_then(|m| m.file_size) {
                    body.push_str(&format!("\n\nFile size: {}", style::human_size(sz)));
                }
                ("🎞", "Video preview needs FFmpeg".into(), body, ACCENT)
            }
            Some(DecodeError::Unsupported(m)) => (
                "🖼",
                "Preview not available for this format".into(),
                m.clone(),
                MUTED,
            ),
            Some(DecodeError::Failed(m)) => (
                "⚠",
                "This file couldn't be displayed".into(),
                format!("It may be corrupt, truncated or still being written.\n{m}"),
                style::REJECT,
            ),
            Some(DecodeError::Cancelled) => return,
        };
        let w = rect.width().clamp(200.0, 460.0);
        let body_g = p.layout(body, FontId::proportional(12.5), MUTED, w - 40.0);
        let h = 110.0 + body_g.size().y;
        let card = Rect::from_center_size(rect.center(), vec2(w, h)).intersect(rect.shrink(8.0));
        p.rect(
            card,
            CornerRadius::same(10),
            BAR_BG,
            Stroke::new(1.0, BAR_BORDER),
            egui::StrokeKind::Inside,
        );
        let mut y = card.top() + 18.0;
        p.text(
            pos2(card.center().x, y),
            Align2::CENTER_TOP,
            icon,
            FontId::proportional(26.0),
            icon_col,
        );
        y += 36.0;
        p.text(
            pos2(card.center().x, y),
            Align2::CENTER_TOP,
            title,
            FontId::proportional(15.0),
            TEXT,
        );
        y += 24.0;
        p.text(
            pos2(card.center().x, y),
            Align2::CENTER_TOP,
            &name,
            FontId::proportional(12.0),
            style::FAINT.gamma_multiply(1.5),
        );
        y += 20.0;
        p.galley(
            pos2(card.center().x - body_g.size().x / 2.0, y),
            body_g,
            MUTED,
        );
    }

    /// Small pills on the canvas: video, burst, loading, sharpness.
    fn item_badges(&self, p: &Painter, rect: Rect, item: usize) {
        let inner = rect.shrink(12.0);
        let mut tl = inner.left_top();
        let is_video = self.loader.placeholder_size(item).is_some_and(|(_, v)| v)
            || self.loader.natural(item).is_some()
                && self.meta.get(item).is_some_and(|m| m.is_video);
        if is_video {
            let r = style::pill(
                p,
                tl,
                Align2::LEFT_TOP,
                "▶ Video · representative frame",
                TEXT,
                OVERLAY_BG,
                12.0,
            );
            tl.y = r.bottom() + 6.0;
        }
        if let Some(e) = self.loader.error((item, Level::Screen))
            && self.loader.natural(item).is_none()
        {
            let msg = match e {
                DecodeError::Failed(m) | DecodeError::Unsupported(m) => {
                    format!("Full preview unavailable: {m}")
                }
                _ => "Full preview unavailable".into(),
            };
            style::pill(
                p,
                tl,
                Align2::LEFT_TOP,
                &msg,
                style::REJECT,
                OVERLAY_BG,
                12.0,
            );
        }
        let mut tr = inner.right_top();
        // Single/cull show the burst chip in the header; compare labels each panel.
        if self.mode == ViewerMode::Compare
            && let Some(Some((pos, len))) = self.bursts.get(item)
        {
            let r = style::pill(
                p,
                tr,
                Align2::RIGHT_TOP,
                &format!("Burst {pos}/{len}"),
                TEXT,
                ACCENT.gamma_multiply(0.55),
                12.0,
            );
            tr.y = r.bottom() + 6.0;
        }
        let loading = [Level::Screen, Level::Full, Level::Quick]
            .iter()
            .any(|&l| self.loader.loading((item, l)));
        if loading {
            let r = style::pill(
                p,
                tr,
                Align2::RIGHT_TOP,
                "      Loading",
                MUTED,
                OVERLAY_BG,
                12.0,
            );
            spinner(p, pos2(r.left() + 12.0, r.center().y), 5.0);
            p.ctx().request_repaint_after(Duration::from_millis(30));
        }
        if self.show_sharpness {
            let text = match self.loader.sharpness(item) {
                Some(s) => format!("Sharpness ≈ {:.2} · estimate", s.score),
                None if self.loader.natural(item).is_some()
                    && self.meta.get(item).is_some_and(|m| m.is_video) =>
                {
                    "Sharpness: n/a for video".into()
                }
                None => "Sharpness: estimating…".into(),
            };
            let col = self
                .loader
                .sharpness(item)
                .map_or(MUTED, |s| sharp_color(s.score));
            let r = style::pill(
                p,
                inner.right_bottom(),
                Align2::RIGHT_BOTTOM,
                &format!("   {text}"),
                TEXT,
                OVERLAY_BG,
                12.0,
            );
            p.circle_filled(pos2(r.left() + 9.0, r.center().y), 3.5, col);
        }
    }

    // -----------------------------------------------------------------------
    // Single / cull
    // -----------------------------------------------------------------------

    pub(super) fn single_canvas(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        store: &mut dyn AnnotationStore,
    ) {
        self.panel_rects = vec![rect];
        if self.items.is_empty() || self.visible.is_empty() {
            let p = ui.painter_at(rect);
            let msg = if self.items.is_empty() {
                "No photos to show"
            } else {
                "No photos match this filter"
            };
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                msg,
                FontId::proportional(15.0),
                MUTED,
            );
            return;
        }
        let item = self.cur;
        let mut view = self.view;
        self.draw_item(ui, rect, item, &mut view, Id::new("viewer_canvas"));
        self.view = view;
        let p = ui.painter_at(rect);
        if self.mode == ViewerMode::Cull {
            let a = store.get(self.path(item));
            cull_badge(&p, rect, a);
        }
        if self.show_info {
            self.info_overlay(&p, rect, item);
        }
    }

    fn info_overlay(&self, p: &Painter, rect: Rect, item: usize) {
        let mut rows: Vec<(&str, String)> = Vec::new();
        rows.push(("File", self.file_name(item)));
        let m = self.meta.get(item);
        let nat = self.loader.natural(item);
        if let Some([w, h]) = nat.or_else(|| m.and_then(|m| m.dims).map(|(w, h)| [w, h])) {
            rows.push((
                "Dimensions",
                format!("{w} × {h}  ({:.1} MP)", w as f64 * h as f64 / 1e6),
            ));
        }
        match m {
            None => rows.push(("", "Reading metadata…".into())),
            Some(m) => {
                if let Some(v) = &m.camera {
                    rows.push(("Camera", v.clone()));
                }
                if let Some(v) = &m.lens {
                    rows.push(("Lens", v.clone()));
                }
                if let Some(v) = m.focal_mm {
                    rows.push(("Focal length", format!("{} mm", trim_num(v))));
                }
                if let Some(v) = m.aperture {
                    rows.push(("Aperture", format!("ƒ/{}", trim_num(v))));
                }
                if let Some(v) = &m.shutter {
                    let v = if v.ends_with('s') {
                        v.clone()
                    } else {
                        format!("{v} s")
                    };
                    rows.push(("Shutter", v));
                }
                if let Some(v) = m.iso {
                    rows.push(("ISO", v.to_string()));
                }
                if let Some(v) = m.exposure_comp.filter(|v| v.abs() > 1e-3) {
                    rows.push(("Exposure comp.", format!("{v:+.1} EV")));
                }
                if let Some(v) = &m.date {
                    rows.push(("Date", v.clone()));
                }
                if let Some(v) = &m.video {
                    if let Some(d) = &v.duration {
                        rows.push(("Duration", d.clone()));
                    }
                    if let Some(c) = &v.codec {
                        rows.push(("Codec", c.clone()));
                    }
                    if let Some(f) = v.fps {
                        rows.push(("Frame rate", format!("{} fps", trim_num(f))));
                    }
                }
                if let Some(v) = m.file_size {
                    rows.push(("File size", style::human_size(v)));
                }
                if let Some(v) = &m.format {
                    rows.push(("Format", v.clone()));
                }
                if let Some(e) = &m.error {
                    rows.push(("Metadata", e.clone()));
                }
            }
        }
        if let Some(s) = self.loader.sharpness(item) {
            rows.push(("Sharpness", format!("≈ {:.2}  (estimate)", s.score)));
        }
        let kf = FontId::proportional(12.0);
        let vf = FontId::proportional(12.5);
        let kw = 96.0;
        let vw = 260.0;
        let mut galleys = Vec::new();
        let mut h = 0.0;
        for (k, v) in &rows {
            let kg = p.layout_no_wrap(k.to_string(), kf.clone(), MUTED);
            let vg = p.layout(v.clone(), vf.clone(), TEXT, vw);
            h += vg.size().y.max(kg.size().y) + 5.0;
            galleys.push((kg, vg));
        }
        let note = self.loader.sharpness(item).map(|_| {
            p.layout(
                "Sharpness is guidance only: an estimate from local contrast on a downscaled \
                 copy. Soft-by-design shots (portraits, skies, motion) score low."
                    .to_string(),
                FontId::proportional(11.0),
                style::FAINT.gamma_multiply(1.6),
                kw + vw,
            )
        });
        if let Some(n) = &note {
            h += n.size().y + 8.0;
        }
        let pad = 12.0;
        let r = Rect::from_min_size(
            rect.left_top() + vec2(12.0, 12.0),
            vec2(kw + vw + pad * 2.0, h + pad * 2.0 - 5.0),
        );
        p.rect(
            r,
            CornerRadius::same(8),
            OVERLAY_BG,
            Stroke::new(1.0, BAR_BORDER),
            egui::StrokeKind::Inside,
        );
        let mut y = r.top() + pad;
        for (kg, vg) in galleys {
            let rh = vg.size().y.max(kg.size().y);
            p.galley(pos2(r.left() + pad, y), kg, MUTED);
            p.galley(pos2(r.left() + pad + kw, y), vg, TEXT);
            y += rh + 5.0;
        }
        if let Some(n) = note {
            p.galley(pos2(r.left() + pad, y + 3.0), n, MUTED);
        }
    }

    // -----------------------------------------------------------------------
    // Compare
    // -----------------------------------------------------------------------

    pub(super) fn compare_canvas(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        store: &mut dyn AnnotationStore,
    ) {
        let n = self.compare.panels.len();
        if n == 0 {
            self.single_canvas(ui, rect, store);
            return;
        }
        if self.compare.split && n == 2 {
            self.split_canvas(ui, rect, store);
            return;
        }
        let (cols, rows) = match n {
            1 => (1, 1),
            2 => (2, 1),
            3 => (3, 1),
            _ => (2, 2),
        };
        let gap = 4.0;
        let cw = (rect.width() - gap * (cols as f32 - 1.0)) / cols as f32;
        let rh = (rect.height() - gap * (rows as f32 - 1.0)) / rows as f32;
        let mut rects = Vec::with_capacity(n);
        for k in 0..n {
            let (c, r) = (k % cols, k / cols);
            rects.push(Rect::from_min_size(
                rect.min + vec2(c as f32 * (cw + gap), r as f32 * (rh + gap)),
                vec2(cw, rh),
            ));
        }
        self.panel_rects = rects.clone();
        for (k, pr) in rects.into_iter().enumerate() {
            let item = self.compare.panels[k];
            let inner = pr.shrink(2.0);
            let mut view = if self.compare.sync {
                self.compare.shared
            } else {
                self.compare.views[k]
            };
            let resp = self.draw_item(ui, inner, item, &mut view, Id::new(("viewer_cmp", k)));
            if self.compare.sync {
                self.compare.shared = view;
            } else {
                self.compare.views[k] = view;
            }
            if resp.clicked() || resp.drag_started() || resp.double_clicked() {
                self.compare.active = k;
                self.cur = item;
                self.scroll_strip = true;
            }
            let p = ui.painter_at(pr);
            let active = k == self.compare.active;
            p.rect_stroke(
                pr.shrink(1.0),
                CornerRadius::same(3),
                if active {
                    Stroke::new(2.0, ACCENT)
                } else {
                    Stroke::new(1.0, BAR_BORDER)
                },
                egui::StrokeKind::Inside,
            );
            let pos = self.visible.binary_search(&item).map(|p| p + 1).ok();
            let title = match pos {
                Some(p) => format!("{}  ·  {}", p, self.file_name(item)),
                None => self.file_name(item),
            };
            style::pill(
                &p,
                inner.left_bottom() + vec2(10.0, -40.0),
                Align2::LEFT_BOTTOM,
                &title,
                if active { TEXT } else { MUTED },
                OVERLAY_BG,
                12.0,
            );
            cull_badge(&p, inner, store.get(self.path(item)));
        }
    }

    fn split_canvas(&mut self, ui: &mut Ui, rect: Rect, store: &mut dyn AnnotationStore) {
        let (a, b) = (self.compare.panels[0], self.compare.panels[1]);
        self.panel_rects = vec![rect, rect];
        let mut view = self.compare.shared;
        // A ("before") handles interaction and paints the whole canvas.
        let resp = self.draw_item(ui, rect, a, &mut view, Id::new("viewer_split"));
        self.compare.shared = view;
        let x = rect.left() + rect.width() * self.compare.split_x;
        // B ("after") painted right of the divider with the same view.
        let right = Rect::from_min_max(pos2(x, rect.top()), rect.max);
        let pb = ui.painter_at(right);
        pb.rect_filled(right, 0.0, style::CANVAS_BG);
        if !self.paint_item(&pb, rect, b, &view) {
            self.status_card(&pb, right, b);
        }
        if (resp.clicked() || resp.drag_started())
            && let Some(pp) = resp.interact_pointer_pos()
        {
            self.compare.active = usize::from(pp.x > x);
            self.cur = self.compare.panels[self.compare.active];
        }
        // Divider.
        let handle = Rect::from_center_size(pos2(x, rect.center().y), vec2(14.0, rect.height()));
        let hr = ui.interact(handle, Id::new("viewer_split_handle"), Sense::drag());
        if hr.hovered() || hr.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        if hr.dragged()
            && let Some(pp) = hr.interact_pointer_pos()
        {
            self.compare.split_x = ((pp.x - rect.left()) / rect.width()).clamp(0.02, 0.98);
        }
        let p = ui.painter_at(rect);
        let col = if hr.hovered() || hr.dragged() {
            ACCENT
        } else {
            Color32::from_white_alpha(200)
        };
        p.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(2.0, col),
        );
        let c = pos2(x, rect.center().y);
        p.circle(c, 13.0, OVERLAY_BG, Stroke::new(1.5, col));
        p.text(
            c,
            Align2::CENTER_CENTER,
            "◀▶",
            FontId::proportional(10.0),
            col,
        );
        let inner = rect.shrink(12.0);
        let ta = format!("Before · {}", self.file_name(a));
        let tb = format!("After · {}", self.file_name(b));
        let act = self.compare.active;
        style::pill(
            &p,
            inner.left_top(),
            Align2::LEFT_TOP,
            &ta,
            if act == 0 { TEXT } else { MUTED },
            OVERLAY_BG,
            12.0,
        );
        style::pill(
            &p,
            inner.right_top() + vec2(0.0, 28.0),
            Align2::RIGHT_TOP,
            &tb,
            if act == 1 { TEXT } else { MUTED },
            OVERLAY_BG,
            12.0,
        );
        let left_half = Rect::from_min_max(rect.min, pos2(x, rect.max.y));
        cull_badge(&ui.painter_at(left_half), rect, store.get(self.path(a)));
        let pr = ui.painter_at(right);
        cull_badge_right(&pr, rect, store.get(self.path(b)));
    }

    // -----------------------------------------------------------------------
    // Keyboard zoom
    // -----------------------------------------------------------------------

    pub(super) fn zoom_key(&mut self, key: ZoomKey, hover: Option<Pos2>) {
        let (item, rect) = match self.mode {
            ViewerMode::Compare => {
                let a = self.compare.active;
                match (self.compare.panels.get(a), self.panel_rects.get(a)) {
                    (Some(&i), Some(&r)) => (i, r.shrink(2.0)),
                    _ => return,
                }
            }
            _ if self.items.is_empty() => return,
            _ => (self.cur, self.last_canvas),
        };
        let Some(nat) = self.natural_of(item) else {
            return;
        };
        let one = 1.0 / self.loader_ppp();
        let anchor = hover.filter(|h| rect.contains(*h)).unwrap_or(rect.center());
        let view = match self.mode {
            ViewerMode::Compare if self.compare.sync || self.compare.split => {
                &mut self.compare.shared
            }
            ViewerMode::Compare => &mut self.compare.views[self.compare.active],
            _ => &mut self.view,
        };
        match key {
            ZoomKey::Toggle => view.toggle_actual(nat, rect, one, anchor),
            ZoomKey::Fit => *view = ViewState::default(),
            ZoomKey::In => view.zoom_by(nat, rect, one, anchor, 1.25),
            ZoomKey::Out => view.zoom_by(nat, rect, one, anchor, 0.8),
        }
    }

    pub(super) fn current_zoom_percent(&self) -> Option<(f32, bool)> {
        let (item, rect, view) = match self.mode {
            ViewerMode::Compare => {
                let a = self.compare.active;
                let v = if self.compare.sync || self.compare.split {
                    self.compare.shared
                } else {
                    *self.compare.views.get(a)?
                };
                (
                    *self.compare.panels.get(a)?,
                    self.panel_rects.get(a)?.shrink(2.0),
                    v,
                )
            }
            _ if self.items.is_empty() => return None,
            _ => (self.cur, self.last_canvas, self.view),
        };
        let nat = self.natural_of(item)?;
        let one = 1.0 / self.loader_ppp();
        Some((
            view.percent(nat, rect, one),
            matches!(view.zoom, super::Zoom::Fit),
        ))
    }

    fn loader_ppp(&self) -> f32 {
        self.ppp
    }

    pub(super) fn toast_overlay(&mut self, ui: &mut Ui, rect: Rect) {
        let Some((msg, t)) = &self.toast else {
            return;
        };
        let age = t.elapsed().as_secs_f32();
        if age > 1.8 {
            self.toast = None;
            return;
        }
        let alpha = ((1.8 - age) / 0.3).clamp(0.0, 1.0);
        let p = ui.painter_at(rect);
        style::pill(
            &p,
            rect.center_bottom() - vec2(0.0, 24.0),
            Align2::CENTER_BOTTOM,
            msg,
            TEXT.gamma_multiply(alpha),
            OVERLAY_BG.gamma_multiply(alpha),
            13.0,
        );
        ui.ctx().request_repaint_after(Duration::from_millis(30));
    }
}

fn trim_num(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

fn sharp_color(score: f32) -> Color32 {
    if score >= 0.5 {
        style::PICK
    } else if score >= 0.25 {
        style::STAR
    } else {
        style::REJECT
    }
}

/// Large rating/pick/label badge in the bottom-left of a canvas (cull & compare).
fn cull_badge(p: &Painter, rect: Rect, a: cull::Annotation) {
    cull_badge_at(
        p,
        rect.left_bottom() + vec2(12.0, -12.0),
        Align2::LEFT_BOTTOM,
        a,
    );
}

fn cull_badge_right(p: &Painter, rect: Rect, a: cull::Annotation) {
    cull_badge_at(
        p,
        rect.right_bottom() + vec2(-12.0, -12.0),
        Align2::RIGHT_BOTTOM,
        a,
    );
}

fn cull_badge_at(p: &Painter, anchor: Pos2, align: Align2, a: cull::Annotation) {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    let f = FontId::proportional(15.0);
    let r = a.rating.min(5) as usize;
    job.append(
        &"★".repeat(r),
        0.0,
        TextFormat::simple(f.clone(), style::STAR),
    );
    job.append(
        &"★".repeat(5 - r),
        0.0,
        TextFormat::simple(f.clone(), Color32::from_white_alpha(45)),
    );
    if let Some(t) = cull::pick_text(a.pick) {
        let sym = if a.pick > 0 { "  ⚑ " } else { "  ✖ " };
        job.append(
            sym,
            0.0,
            TextFormat::simple(FontId::proportional(13.0), style::pick_color(a.pick)),
        );
        job.append(
            t,
            0.0,
            TextFormat::simple(FontId::proportional(13.0), style::pick_color(a.pick)),
        );
    }
    let label_col = a
        .label
        .and_then(|l| style::LABEL_COLORS.get(l as usize))
        .copied();
    if label_col.is_some() {
        job.append(
            "      ",
            0.0,
            TextFormat::simple(FontId::proportional(13.0), TEXT),
        );
    }
    let g = p.layout_job(job);
    let pad = vec2(10.0, 5.0);
    let r = align.anchor_size(anchor, g.size() + pad * 2.0);
    p.rect_filled(r, CornerRadius::same(8), OVERLAY_BG);
    p.galley(r.min + pad, g, TEXT);
    if let Some(c) = label_col {
        p.circle(
            pos2(r.right() - 16.0, r.center().y),
            5.5,
            c,
            Stroke::new(1.0, Color32::from_black_alpha(150)),
        );
    }
}

/// Simple rotating-arc spinner.
pub(super) fn spinner(p: &Painter, c: Pos2, radius: f32) {
    let t = p.ctx().input(|i| i.time) as f32;
    let start = t * 5.0;
    let pts: Vec<Pos2> = (0..=24)
        .map(|k| {
            let a = start + k as f32 / 24.0 * std::f32::consts::PI * 1.4;
            c + vec2(a.cos(), a.sin()) * radius
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(2.0, ACCENT)));
}
