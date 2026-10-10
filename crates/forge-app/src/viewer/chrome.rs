//! Viewer chrome: header bar, mode-specific sub bar, filmstrip and help overlay.

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Id, Layout, Rect, RichText, Sense, Stroke, Ui,
    UiBuilder, pos2, vec2,
};

use super::cull::{self, AnnotationStore, Filter};
use super::loader::Level;
use super::style::{self, ACCENT, BAR_BG, BAR_BORDER, FAINT, MUTED, STRIP_BG, TEXT};
use super::{HELP, Nav, Viewer, ViewerMode};

const CELL_W: f32 = 96.0;

impl Viewer {
    pub(super) fn header(&mut self, ui: &mut Ui, rect: Rect, store: &mut dyn AnnotationStore) {
        let p = ui.painter();
        p.rect_filled(rect, 0.0, BAR_BG);
        p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, BAR_BORDER));
        let inner = rect.shrink2(vec2(10.0, 0.0));
        let split = inner.left() + inner.width() * 0.46;
        let left = Rect::from_min_max(inner.min, pos2(split, inner.max.y));
        let right = Rect::from_min_max(pos2(split, inner.min.y), inner.max);

        let mut lui = ui.new_child(UiBuilder::new().max_rect(left).layout(Layout::left_to_right(Align::Center)));
        lui.spacing_mut().item_spacing.x = 4.0;
        if lui
            .add(egui::Button::new(RichText::new("⏴ Back").color(TEXT)).frame_when_inactive(false))
            .on_hover_text("Back to the browser (Esc)")
            .clicked()
        {
            self.close = true;
        }
        lui.add_space(6.0);
        for (m, label, key) in [
            (ViewerMode::Single, "Single", "Ctrl+1"),
            (ViewerMode::Cull, "Cull", "Ctrl+2"),
            (ViewerMode::Compare, "Compare", "Ctrl+3"),
        ] {
            if lui
                .add(egui::Button::selectable(self.mode == m, label))
                .on_hover_text(key)
                .clicked()
            {
                self.set_mode(m);
            }
        }
        lui.add_space(12.0);
        if let Some(item) = self.target_item() {
            let name = self.file_name(item);
            lui.add(egui::Label::new(RichText::new(name).strong().color(TEXT)).truncate());
        }

        let mut rui = ui.new_child(UiBuilder::new().max_rect(right).layout(Layout::right_to_left(Align::Center)));
        rui.spacing_mut().item_spacing.x = 6.0;
        if rui
            .add(egui::Button::new("?").frame_when_inactive(false))
            .on_hover_text("Keyboard shortcuts (? / F1)")
            .clicked()
        {
            self.show_help = true;
        }
        if rui
            .add(egui::Button::selectable(self.show_info, "Info"))
            .on_hover_text("EXIF info overlay (I)")
            .clicked()
        {
            self.show_info = !self.show_info;
        }
        if let Some((pct, fit)) = self.current_zoom_percent() {
            let t = if fit { format!("Fit · {pct:.0}%") } else { format!("{pct:.0}%") };
            let r = rui.add(egui::Button::new(RichText::new(t).color(MUTED)).frame_when_inactive(false));
            if r.on_hover_text("Toggle fit / 100 % (Z)").clicked() {
                self.zoom_key(super::ZoomKey::Toggle, None);
            }
        }
        rui.add_space(8.0);
        if let Some(item) = self.target_item() {
            if let Some(Some((pos, len))) = self.bursts.get(item) {
                burst_chip(&mut rui, *pos, *len);
            }
            let a = store.get(self.path(item));
            if let Some(c) = a.label.and_then(|l| style::LABEL_COLORS.get(l as usize)) {
                let (r, _) = rui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                rui.painter().circle(r.center(), 5.0, *c, Stroke::new(1.0, Color32::from_black_alpha(150)));
            }
            let pos = self.visible.binary_search(&item).ok();
            let mut job = LayoutJob::default();
            let f = FontId::proportional(13.5);
            let txt = |c| TextFormat::simple(f.clone(), c);
            match pos {
                Some(p) => job.append(
                    &format!("{} / {}", cull::format_count(p + 1), cull::format_count(self.visible.len())),
                    0.0,
                    txt(TEXT),
                ),
                None => job.append(
                    &format!("– / {}", cull::format_count(self.visible.len())),
                    0.0,
                    txt(MUTED),
                ),
            }
            job.append("  ·  ", 0.0, txt(FAINT));
            let r = a.rating.min(5) as usize;
            job.append(&"★".repeat(r), 0.0, txt(style::STAR));
            job.append(&"☆".repeat(5 - r), 0.0, txt(FAINT));
            if let Some(pt) = cull::pick_text(a.pick) {
                job.append("  ·  ", 0.0, txt(FAINT));
                job.append(pt, 0.0, txt(style::pick_color(a.pick)));
            }
            rui.label(job)
                .on_hover_text(cull::header_text(pos.unwrap_or(0), self.visible.len(), a));
        }
    }

    pub(super) fn sub_bar(&mut self, ui: &mut Ui, rect: Rect, store: &mut dyn AnnotationStore) {
        let p = ui.painter();
        p.rect_filled(rect, 0.0, style::STRIP_BG);
        p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, BAR_BORDER));
        let inner = rect.shrink2(vec2(10.0, 0.0));
        let mut lui = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
        lui.spacing_mut().item_spacing.x = 3.0;
        let small = |s: &str| RichText::new(s).size(12.5);
        match self.mode {
            ViewerMode::Cull | ViewerMode::Single => {
                lui.label(small("Show").color(MUTED));
                lui.add_space(4.0);
                let mut chosen = None;
                for f in [Filter::All, Filter::Picks, Filter::Rejects, Filter::Unrated] {
                    if lui.add(egui::Button::selectable(self.filter == f, small(&f.label()))).clicked() {
                        chosen = Some(f);
                    }
                }
                lui.add_space(8.0);
                lui.label(small("≥").color(MUTED));
                for n in 1..=5u8 {
                    let f = Filter::MinRating(n);
                    if lui
                        .add(egui::Button::selectable(self.filter == f, small(&format!("{n}★"))))
                        .on_hover_text(format!("Rated {n} stars or more"))
                        .clicked()
                    {
                        chosen = Some(f);
                    }
                }
                if let Some(f) = chosen {
                    self.set_filter(f, store);
                }
                lui.add_space(10.0);
                if self.filter != Filter::All {
                    lui.label(
                        small(&format!(
                            "{} of {}",
                            cull::format_count(self.visible.len()),
                            cull::format_count(self.items.len())
                        ))
                        .color(MUTED),
                    );
                }
            }
            ViewerMode::Compare => {
                lui.label(small("Panels").color(MUTED));
                lui.add_space(4.0);
                let n = self.compare.panels.len();
                for k in 2..=4usize {
                    let enabled = self.visible.len() >= k;
                    if lui
                        .add_enabled(enabled, egui::Button::selectable(n == k && !self.compare.split, small(&k.to_string())))
                        .clicked()
                    {
                        self.set_panel_count(k);
                    }
                }
                lui.add_space(12.0);
                if lui
                    .add(egui::Button::selectable(self.compare.sync, small("Sync zoom & pan")))
                    .on_hover_text("S")
                    .clicked()
                {
                    self.compare.sync = !self.compare.sync;
                }
                let can_split = self.items.len() == 2 && n == 2;
                if lui
                    .add_enabled(can_split, egui::Button::selectable(self.compare.split, small("Before / after")))
                    .on_hover_text("Split slider (B)")
                    .on_disabled_hover_text("Available when the viewer is opened with exactly two photos")
                    .clicked()
                {
                    self.compare.split = !self.compare.split;
                }
            }
        }

        let mut rui = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)));
        match self.mode {
            ViewerMode::Cull => {
                let mut aa = self.auto_advance;
                if rui.checkbox(&mut aa, small("Auto-advance")).on_hover_text("Move to the next photo after rating, pick or reject (A)").changed() {
                    self.auto_advance = aa;
                }
            }
            ViewerMode::Compare => {
                rui.label(small("Click or Tab: active panel  ·  ← →: replace it  ·  0–5, P, X: rate it").color(FAINT));
            }
            ViewerMode::Single => {}
        }
        if !self.meta.complete() && self.items.len() > 1 {
            rui.add_space(12.0);
            let pct = self.meta.done * 100 / self.items.len().max(1);
            rui.label(small(&format!("Reading capture times… {pct}%")).color(FAINT))
                .on_hover_text("Used for burst grouping");
        }
    }

    pub(super) fn filmstrip(&mut self, ui: &mut Ui, rect: Rect, store: &mut dyn AnnotationStore) {
        let p = ui.painter();
        p.rect_filled(rect, 0.0, STRIP_BG);
        p.hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, BAR_BORDER));
        let inner = rect.shrink2(vec2(6.0, 6.0));
        let mut sui = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Min)));
        let n = self.visible.len();
        let h = inner.height() - 8.0;
        let cur_pos = self.target_item().and_then(|i| self.visible.binary_search(&i).ok());
        let scroll_to = std::mem::take(&mut self.scroll_strip);
        let mut clicked = None;
        let mut range = 0..0;
        let panels: Vec<usize> = if self.mode == ViewerMode::Compare { self.compare.panels.clone() } else { Vec::new() };
        egui::ScrollArea::horizontal()
            .id_salt("viewer_filmstrip")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
            .show_viewport(&mut sui, |ui, vp| {
                ui.set_min_size(vec2(n as f32 * CELL_W, h));
                let origin = ui.max_rect().min;
                let first = ((vp.min.x / CELL_W).floor().max(0.0) as usize).min(n);
                let last = ((vp.max.x / CELL_W).ceil().max(0.0) as usize).min(n);
                range = first.saturating_sub(6)..(last + 6).min(n);
                if scroll_to && let Some(cp) = cur_pos {
                    let r = Rect::from_min_size(origin + vec2(cp as f32 * CELL_W, 0.0), vec2(CELL_W, h));
                    ui.scroll_to_rect(r, Some(Align::Center));
                }
                for pos in first..last {
                    let item = self.visible[pos];
                    let cell = Rect::from_min_size(origin + vec2(pos as f32 * CELL_W, 0.0), vec2(CELL_W, h)).shrink2(vec2(3.0, 2.0));
                    let resp = ui.interact(cell, Id::new(("viewer_strip_cell", item)), Sense::click());
                    let pt = ui.painter();
                    let is_cur = Some(pos) == cur_pos;
                    let in_panel = panels.contains(&item);
                    pt.rect_filled(cell, CornerRadius::same(4), Color32::from_rgb(30, 31, 35));
                    let area = cell.shrink(3.0);
                    let mut img_rect = area;
                    if let Some(t) = self.loader.get((item, Level::Small)) {
                        let (tw, th) = (t.size[0] as f32, t.size[1] as f32);
                        let s = (area.width() / tw).min(area.height() / th);
                        img_rect = Rect::from_center_size(area.center(), vec2(tw * s, th * s));
                        if let Some((tex, _)) = t.tiles.first() {
                            pt.image(tex.id(), img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                        }
                    } else if self.loader.error((item, Level::Small)).is_some() {
                        let icon = if super::decode::is_video(self.path(item)) { "🎞" } else { "⚠" };
                        pt.text(area.center(), Align2::CENTER_CENTER, icon, FontId::proportional(16.0), FAINT);
                    }
                    style::draw_annotation(pt, img_rect, store.get(self.path(item)), 0.9);
                    let stroke = if is_cur {
                        Stroke::new(2.0, ACCENT)
                    } else if in_panel {
                        Stroke::new(1.5, ACCENT.gamma_multiply(0.5))
                    } else if resp.hovered() {
                        Stroke::new(1.0, MUTED)
                    } else {
                        Stroke::NONE
                    };
                    if stroke != Stroke::NONE {
                        pt.rect_stroke(cell, CornerRadius::same(4), stroke, egui::StrokeKind::Inside);
                    }
                    if resp.on_hover_text(self.file_name(item)).clicked() {
                        clicked = Some(item);
                    }
                }
            });
        self.strip_range = range;
        if let Some(item) = clicked {
            self.go(Nav::To(item), store);
        }
    }

    pub(super) fn help_overlay(&mut self, ctx: &egui::Context) {
        let resp = egui::Modal::new(Id::new("viewer_help")).show(ctx, |ui| {
            ui.set_width(620.0);
            ui.horizontal(|ui| {
                ui.heading("Keyboard shortcuts");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        ui.close();
                    }
                });
            });
            ui.add_space(6.0);
            let sections = ["Navigate", "Zoom", "Cull", "Compare", "View"];
            ui.columns(2, |cols| {
                for (k, sec) in sections.iter().enumerate() {
                    let ui = &mut cols[if k < 2 { 0 } else { 1 }];
                    ui.add_space(4.0);
                    ui.label(RichText::new(*sec).strong().color(ACCENT));
                    egui::Grid::new(("viewer_help_grid", k)).num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
                        for (keys, what, s) in HELP {
                            if s == sec {
                                ui.label(RichText::new(*keys).monospace().size(11.5).color(TEXT));
                                ui.label(RichText::new(*what).size(12.0).color(MUTED));
                                ui.end_row();
                            }
                        }
                    });
                }
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Ratings, labels and flags are stored in the ZXraw Explorer catalog; image files are never modified.",
                )
                .size(11.5)
                .color(FAINT),
            );
        });
        if resp.should_close() {
            self.show_help = false;
        }
    }
}

fn burst_chip(ui: &mut Ui, pos: u32, len: u32) {
    let text = format!("Burst {pos}/{len}");
    let g = ui.painter().layout_no_wrap(text, FontId::proportional(12.0), TEXT);
    let (r, resp) = ui.allocate_exact_size(g.size() + vec2(14.0, 6.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::same(9), ACCENT.gamma_multiply(0.45));
    ui.painter().galley(r.min + vec2(7.0, 3.0), g, TEXT);
    resp.on_hover_text("Shots taken within 1 s of each other by the same camera");
}
