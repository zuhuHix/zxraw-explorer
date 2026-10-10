//! File list renderers. Both views are virtualized: only on-screen rows/tiles are laid out,
//! so a folder with 100k entries costs the same per frame as one with 100.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, Ui, Vec2, pos2, vec2};
use forge_core::model::{EntryKind, MediaKind, format_size};
use forge_core::{FileEntry, listing::SortKey};

use crate::commands::Cmd;
use crate::pane::Pane;
use crate::theme;

/// Payload for in-app drag and drop.
#[derive(Clone, Debug)]
pub struct DragPaths(pub Vec<PathBuf>);

#[derive(Debug)]
pub enum ViewEvent {
    /// Double-click / Enter on an entry (index into `visible`).
    Activate(usize),
    Command(Cmd),
    /// Paths dropped onto `target` folder. `copy` when Ctrl was held or cross-volume.
    Drop { paths: Vec<PathBuf>, target: PathBuf, copy: bool },
    /// The pane was clicked (gives it focus).
    Focus,
}

/// Something that can hand out thumbnail textures (implemented by the app's thumbnail
/// manager; returns `None` while the thumbnail is still being generated).
pub trait ThumbProvider {
    fn thumb(&mut self, ui: &Ui, entry: &FileEntry, size: u32) -> Option<egui::TextureHandle>;
    /// Optional per-file annotation (rating 0–5, color label index, pick state).
    fn annotation(&self, _path: &Path) -> Option<Annotation> {
        None
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Annotation {
    pub rating: u8,
    pub label: Option<u8>,
    /// 1 = pick, -1 = reject, 0 = none
    pub pick: i8,
}

pub fn icon_for(e: &FileEntry) -> &'static str {
    match e.kind {
        EntryKind::Dir => "🗀",
        EntryKind::Link => "🔗",
        EntryKind::File => match e.media_kind() {
            MediaKind::Image | MediaKind::Raw => "🖼",
            MediaKind::Video => "🎞",
            MediaKind::Audio => "🎵",
            MediaKind::Archive => "🗄",
            MediaKind::Sidecar => "🏷",
            MediaKind::Project => "🎬",
            MediaKind::Document => "🗋",
            MediaKind::Other => "🗋",
        },
    }
}

pub fn icon_color(e: &FileEntry) -> Color32 {
    match e.kind {
        EntryKind::Dir => Color32::from_rgb(232, 186, 80),
        EntryKind::Link => Color32::from_rgb(140, 170, 210),
        EntryKind::File => match e.media_kind() {
            MediaKind::Raw => Color32::from_rgb(230, 120, 90),
            MediaKind::Image => Color32::from_rgb(110, 180, 120),
            MediaKind::Video => Color32::from_rgb(170, 120, 220),
            MediaKind::Audio => Color32::from_rgb(90, 180, 200),
            MediaKind::Archive => Color32::from_rgb(190, 150, 100),
            _ => Color32::from_gray(150),
        },
    }
}

pub fn format_time(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
        None => String::new(),
    }
}

pub fn type_label(e: &FileEntry) -> String {
    match e.kind {
        EntryKind::Dir => "Folder".into(),
        EntryKind::Link => "Link".into(),
        EntryKind::File if e.ext.is_empty() => "File".into(),
        EntryKind::File => {
            let kind = match e.media_kind() {
                MediaKind::Raw => " RAW",
                MediaKind::Image => " image",
                MediaKind::Video => " video",
                MediaKind::Audio => " audio",
                MediaKind::Sidecar => " sidecar",
                MediaKind::Archive => " archive",
                _ => " file",
            };
            format!("{}{}", e.ext.to_uppercase(), kind)
        }
    }
}

const CONTEXT_FILE: &[Option<Cmd>] = &[
    Some(Cmd::Open),
    Some(Cmd::OpenWith),
    Some(Cmd::RevealInExplorer),
    None,
    Some(Cmd::Cut),
    Some(Cmd::Copy),
    Some(Cmd::CopyPath),
    Some(Cmd::CopyToOtherPane),
    Some(Cmd::MoveToOtherPane),
    None,
    Some(Cmd::Rename),
    Some(Cmd::BatchRename),
    Some(Cmd::Recycle),
    Some(Cmd::DeletePermanently),
    None,
    Some(Cmd::ChecksumFiles),
    Some(Cmd::CompareImages),
    Some(Cmd::Properties),
];

const CONTEXT_BACKGROUND: &[Option<Cmd>] = &[
    Some(Cmd::Paste),
    Some(Cmd::NewFolder),
    Some(Cmd::NewFile),
    None,
    Some(Cmd::Refresh),
    Some(Cmd::SelectAll),
    Some(Cmd::ToggleHidden),
    None,
    Some(Cmd::FolderSize),
    Some(Cmd::FindDuplicates),
    Some(Cmd::BookmarkFolder),
    Some(Cmd::RevealInExplorer),
];

fn menu_items(ui: &mut Ui, items: &[Option<Cmd>], out: &mut Vec<ViewEvent>) {
    for it in items {
        match it {
            None => {
                ui.separator();
            }
            Some(c) => {
                let sc = c.shortcut_text(ui.ctx());
                if ui
                    .add(egui::Button::new(c.label()).shortcut_text(sc))
                    .clicked()
                {
                    out.push(ViewEvent::Command(*c));
                    ui.close();
                }
            }
        }
    }
}

/// Drag-and-drop handling shared by both views. `target_dir` is the folder an item drops
/// into (the row's folder, or the pane's folder for the background).
fn handle_drop(resp: &egui::Response, target_dir: &Path, out: &mut Vec<ViewEvent>) -> bool {
    let hovering = resp.dnd_hover_payload::<DragPaths>().is_some();
    if let Some(p) = resp.dnd_release_payload::<DragPaths>() {
        // Dropping an item onto itself or its own parent is a no-op.
        let paths: Vec<PathBuf> = p
            .0
            .iter()
            .filter(|s| s.as_path() != target_dir && s.parent() != Some(target_dir))
            .cloned()
            .collect();
        if !paths.is_empty() {
            let ctrl = resp.ctx.input(|i| i.modifiers.command);
            let same_volume = paths
                .first()
                .and_then(|s| s.components().next())
                .zip(target_dir.components().next())
                .is_some_and(|(a, b)| a == b);
            out.push(ViewEvent::Drop {
                paths,
                target: target_dir.to_path_buf(),
                copy: ctrl || !same_volume,
            });
        }
    }
    hovering
}

fn begin_drag(resp: &egui::Response, pane: &Pane, entry: &FileEntry) {
    if resp.drag_started() {
        let paths = if pane.is_selected(entry) {
            pane.selected_entries().iter().map(|e| e.path.clone()).collect()
        } else {
            vec![entry.path.clone()]
        };
        resp.dnd_set_drag_payload(DragPaths(paths));
    }
}

fn row_click(resp: &egui::Response, pane: &mut Pane, vis: usize, out: &mut Vec<ViewEvent>) {
    let m = resp.ctx.input(|i| i.modifiers);
    if resp.clicked() {
        pane.click(vis, m.command, m.shift);
        out.push(ViewEvent::Focus);
    }
    if resp.secondary_clicked() {
        if let Some(e) = pane.entry_at(vis) {
            if !pane.is_selected(e) {
                pane.click(vis, false, false);
            }
        }
        out.push(ViewEvent::Focus);
    }
    if resp.double_clicked() {
        out.push(ViewEvent::Activate(vis));
    }
}

// ---------------------------------------------------------------------------
// Details view
// ---------------------------------------------------------------------------

pub fn details(ui: &mut Ui, pane: &mut Pane, focused: bool, id: egui::Id) -> Vec<ViewEvent> {
    let mut out = Vec::new();
    let row_h = 22.0;
    let width = ui.available_width();
    let cols = [
        ("Name", SortKey::Name, (width - 360.0).max(160.0)),
        ("Date modified", SortKey::Modified, 130.0),
        ("Type", SortKey::Type, 120.0),
        ("Size", SortKey::Size, 90.0),
    ];

    // Header
    let (hrect, _) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
    let mut x = hrect.left() + 8.0;
    let painter = ui.painter_at(hrect);
    painter.line_segment(
        [hrect.left_bottom(), hrect.right_bottom()],
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
    for (i, (label, key, w)) in cols.iter().enumerate() {
        let r = Rect::from_min_size(pos2(x - 8.0, hrect.top()), vec2(*w, row_h));
        let resp = ui.interact(r, id.with(("hdr", i)), Sense::click());
        if resp.hovered() {
            painter.rect_filled(r, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        let arrow = if pane.sort.key == *key {
            if pane.sort.descending { " ⏷" } else { " ⏶" }
        } else {
            ""
        };
        let align = if *key == SortKey::Size { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER };
        let tx = if *key == SortKey::Size { r.right() - 10.0 } else { x };
        painter.text(
            pos2(tx, r.center().y),
            align,
            format!("{label}{arrow}"),
            FontId::proportional(12.0),
            theme::muted(ui),
        );
        if resp.clicked() {
            pane.set_sort(*key);
        }
        x += w;
    }

    let n = pane.visible.len();
    let mut scroll = egui::ScrollArea::vertical()
        .id_salt(id.with("rows"))
        .auto_shrink([false, false]);
    if pane.scroll_to_cursor {
        if let Some(c) = pane.cursor {
            // Keep the cursor row on screen.
            let off = ui.ctx().data(|d| d.get_temp::<f32>(id.with("off"))).unwrap_or(0.0);
            let view_h = ui.available_height();
            let top = c as f32 * (row_h + ui.spacing().item_spacing.y * 0.0);
            let target = if top < off {
                top
            } else if top + row_h > off + view_h - row_h {
                top + 2.0 * row_h - view_h
            } else {
                off
            };
            scroll = scroll.vertical_scroll_offset(target.max(0.0));
        }
        pane.scroll_to_cursor = false;
    }

    let prev_spacing = ui.spacing().item_spacing;
    ui.spacing_mut().item_spacing.y = 0.0;
    let sa = scroll.show_rows(ui, row_h, n, |ui, range| {
        for vis in range {
            let Some(&ei) = pane.visible.get(vis) else { break };
            let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click_and_drag());
            let resp = resp.on_hover_cursor(egui::CursorIcon::Default);
            let entry = &pane.entries[ei];
            let selected = pane.is_selected(entry);
            let p = ui.painter_at(rect);
            let is_dir_target = entry.kind != EntryKind::File;
            let mut drop_hover = false;
            if is_dir_target {
                drop_hover = handle_drop(&resp, &entry.path.clone(), &mut out);
            }
            let entry = &pane.entries[ei];
            if selected {
                p.rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
            } else if resp.hovered() || drop_hover {
                p.rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
            } else if vis % 2 == 1 {
                p.rect_filled(rect, 0.0, ui.visuals().faint_bg_color.gamma_multiply(0.5));
            }
            if drop_hover {
                p.rect_stroke(rect, 3.0, Stroke::new(1.5, theme::ACCENT), egui::StrokeKind::Inside);
            }
            if focused && pane.cursor == Some(vis) {
                p.rect_stroke(
                    rect.shrink(0.5),
                    3.0,
                    Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.8)),
                    egui::StrokeKind::Inside,
                );
            }
            let text_color = if entry.hidden {
                ui.visuals().weak_text_color()
            } else {
                ui.visuals().text_color()
            };
            let font = FontId::proportional(13.0);
            let cy = rect.center().y;
            let mut x = rect.left() + 8.0;
            p.text(pos2(x, cy), Align2::LEFT_CENTER, icon_for(entry), font.clone(), icon_color(entry));
            let name_w = cols[0].2 - 34.0;
            let name_rect = Rect::from_min_size(pos2(x + 22.0, rect.top()), vec2(name_w, row_h));
            let galley = ui.painter().layout(entry.name.clone(), font.clone(), text_color, f32::INFINITY);
            let pn = p.with_clip_rect(name_rect.intersect(rect));
            pn.galley(pos2(name_rect.left(), cy - galley.size().y / 2.0), galley.clone(), text_color);
            let truncated = galley.size().x > name_w;
            x += cols[0].2;
            let muted = theme::muted(ui);
            let small = FontId::proportional(12.0);
            p.text(pos2(x, cy), Align2::LEFT_CENTER, format_time(entry.modified), small.clone(), muted);
            x += cols[1].2;
            p.with_clip_rect(Rect::from_min_size(pos2(x, rect.top()), vec2(cols[2].2 - 8.0, row_h)))
                .text(pos2(x, cy), Align2::LEFT_CENTER, type_label(entry), small.clone(), muted);
            x += cols[2].2;
            if entry.kind == EntryKind::File {
                p.text(
                    pos2(x + cols[3].2 - 10.0, cy),
                    Align2::RIGHT_CENTER,
                    format_size(entry.size),
                    small,
                    muted,
                );
            }
            let entry_clone = entry.clone();
            let resp = if truncated {
                resp.on_hover_text_at_pointer(entry_clone.name.clone())
            } else {
                resp
            };
            begin_drag(&resp, pane, &entry_clone);
            row_click(&resp, pane, vis, &mut out);
            resp.context_menu(|ui| menu_items(ui, CONTEXT_FILE, &mut out));
        }
    });
    ui.spacing_mut().item_spacing = prev_spacing;
    ui.ctx()
        .data_mut(|d| d.insert_temp(id.with("off"), sa.state.offset.y));

    // Background (empty area below rows): context menu, drops into current folder, click clears.
    let bg = ui.interact(sa.inner_rect, id.with("bg"), Sense::click());
    background_interactions(ui, &bg, pane, &mut out);
    out
}

fn background_interactions(ui: &Ui, bg: &egui::Response, pane: &mut Pane, out: &mut Vec<ViewEvent>) {
    let dir = pane.path.clone();
    if handle_drop(bg, &dir, out) {
        ui.painter().rect_stroke(
            bg.rect.shrink(1.0),
            4.0,
            Stroke::new(1.5, theme::ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    if bg.clicked() {
        pane.clear_selection();
        out.push(ViewEvent::Focus);
    }
    if bg.secondary_clicked() {
        out.push(ViewEvent::Focus);
    }
    bg.context_menu(|ui| menu_items(ui, CONTEXT_BACKGROUND, out));
}

// ---------------------------------------------------------------------------
// Grid (thumbnail) view
// ---------------------------------------------------------------------------

pub fn grid(
    ui: &mut Ui,
    pane: &mut Pane,
    focused: bool,
    id: egui::Id,
    thumb_size: f32,
    thumbs: &mut dyn ThumbProvider,
) -> Vec<ViewEvent> {
    let mut out = Vec::new();
    let pad = 8.0;
    let label_h = 34.0;
    let cell = vec2(thumb_size + pad * 2.0, thumb_size + label_h + pad);
    let width = ui.available_width();
    let per_row = ((width / cell.x).floor() as usize).max(1);
    // Stretch cells to fill the row exactly.
    let cell_w = width / per_row as f32;
    let n = pane.visible.len();
    let rows = n.div_ceil(per_row);
    pane_grid_columns_store(ui, id, per_row);

    let mut scroll = egui::ScrollArea::vertical()
        .id_salt(id.with("grid"))
        .auto_shrink([false, false]);
    if pane.scroll_to_cursor {
        if let Some(c) = pane.cursor {
            let row = c / per_row;
            let off = ui.ctx().data(|d| d.get_temp::<f32>(id.with("goff"))).unwrap_or(0.0);
            let view_h = ui.available_height();
            let top = row as f32 * cell.y;
            let target = if top < off {
                top
            } else if top + cell.y > off + view_h {
                top + cell.y - view_h
            } else {
                off
            };
            scroll = scroll.vertical_scroll_offset(target.max(0.0));
        }
        pane.scroll_to_cursor = false;
    }
    let prev_spacing = ui.spacing().item_spacing;
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    let sa = scroll.show_rows(ui, cell.y, rows, |ui, range| {
        for row in range {
            ui.horizontal(|ui| {
                for col in 0..per_row {
                    let vis = row * per_row + col;
                    let Some(&ei) = pane.visible.get(vis) else { break };
                    let (rect, resp) = ui.allocate_exact_size(vec2(cell_w, cell.y), Sense::click_and_drag());
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    let entry = pane.entries[ei].clone();
                    let mut drop_hover = false;
                    if entry.kind != EntryKind::File {
                        drop_hover = handle_drop(&resp, &entry.path, &mut out);
                    }
                    let selected = pane.is_selected(&entry);
                    let p = ui.painter_at(rect);
                    let inner = rect.shrink(3.0);
                    if selected {
                        p.rect_filled(inner, 6.0, ui.visuals().selection.bg_fill);
                    } else if resp.hovered() || drop_hover {
                        p.rect_filled(inner, 6.0, ui.visuals().widgets.hovered.weak_bg_fill);
                    }
                    if focused && pane.cursor == Some(vis) {
                        p.rect_stroke(inner, 6.0, Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Inside);
                    }
                    let img_rect = Rect::from_center_size(
                        pos2(rect.center().x, rect.top() + pad + thumb_size / 2.0),
                        Vec2::splat(thumb_size),
                    );
                    let mut drew = false;
                    if entry.kind == EntryKind::File
                        && matches!(entry.media_kind(), MediaKind::Image | MediaKind::Raw | MediaKind::Video)
                    {
                        if let Some(tex) = thumbs.thumb(ui, &entry, thumb_size.round() as u32) {
                            let sz = tex.size_vec2();
                            let scale = (thumb_size / sz.x).min(thumb_size / sz.y);
                            let r = Rect::from_center_size(img_rect.center(), sz * scale);
                            p.image(tex.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                            if entry.media_kind() == MediaKind::Video {
                                p.text(r.left_bottom() + vec2(4.0, -4.0), Align2::LEFT_BOTTOM, "▶", FontId::proportional(14.0), Color32::WHITE);
                            }
                            drew = true;
                        } else {
                            p.rect_filled(img_rect.shrink(thumb_size * 0.1), 4.0, ui.visuals().faint_bg_color);
                        }
                    }
                    if !drew {
                        p.text(
                            img_rect.center(),
                            Align2::CENTER_CENTER,
                            icon_for(&entry),
                            FontId::proportional((thumb_size * 0.45).clamp(16.0, 96.0)),
                            icon_color(&entry),
                        );
                    }
                    if let Some(a) = thumbs.annotation(&entry.path) {
                        draw_annotation(&p, img_rect, a);
                    }
                    let text_rect = Rect::from_min_max(
                        pos2(rect.left() + 4.0, img_rect.bottom() + 4.0),
                        pos2(rect.right() - 4.0, rect.bottom() - 2.0),
                    );
                    let galley = ui.painter().layout(
                        entry.name.clone(),
                        FontId::proportional(12.0),
                        ui.visuals().text_color(),
                        text_rect.width(),
                    );
                    p.with_clip_rect(text_rect).galley(
                        pos2(text_rect.center().x - galley.size().x / 2.0, text_rect.top()),
                        galley,
                        ui.visuals().text_color(),
                    );
                    let resp = resp.on_hover_text_at_pointer(entry.name.clone());
                    begin_drag(&resp, pane, &entry);
                    row_click(&resp, pane, vis, &mut out);
                    resp.context_menu(|ui| menu_items(ui, CONTEXT_FILE, &mut out));
                }
            });
        }
    });
    ui.spacing_mut().item_spacing = prev_spacing;
    ui.ctx().data_mut(|d| d.insert_temp(id.with("goff"), sa.state.offset.y));
    let bg = ui.interact(sa.inner_rect, id.with("bg"), Sense::click());
    background_interactions(ui, &bg, pane, &mut out);
    out
}

fn pane_grid_columns_store(ui: &Ui, id: egui::Id, per_row: usize) {
    ui.ctx().data_mut(|d| d.insert_temp(id.with("per_row"), per_row));
}

/// Columns of the last rendered grid (for Up/Down keyboard navigation).
pub fn grid_columns(ctx: &egui::Context, id: egui::Id) -> usize {
    ctx.data(|d| d.get_temp::<usize>(id.with("per_row"))).unwrap_or(1)
}

pub fn draw_annotation(p: &egui::Painter, img_rect: Rect, a: Annotation) {
    if a.rating > 0 {
        let stars: String = "★".repeat(a.rating as usize);
        p.text(
            img_rect.left_bottom() + vec2(4.0, -2.0),
            Align2::LEFT_BOTTOM,
            stars,
            FontId::proportional(12.0),
            Color32::from_rgb(250, 200, 60),
        );
    }
    if let Some(l) = a.label {
        if let Some(c) = theme::LABEL_COLORS.get(l as usize) {
            p.circle_filled(img_rect.right_top() + vec2(-8.0, 8.0), 5.0, *c);
        }
    }
    match a.pick {
        1 => {
            p.text(img_rect.left_top() + vec2(4.0, 2.0), Align2::LEFT_TOP, "⚑", FontId::proportional(14.0), Color32::from_rgb(90, 200, 110));
        }
        -1 => {
            p.rect_filled(img_rect, 4.0, Color32::from_black_alpha(130));
            p.text(img_rect.left_top() + vec2(4.0, 2.0), Align2::LEFT_TOP, "✖", FontId::proportional(14.0), Color32::from_rgb(230, 80, 80));
        }
        _ => {}
    }
}

/// Shared empty-thumbnail provider used before the media pipeline is available.
pub struct NoThumbs;
impl ThumbProvider for NoThumbs {
    fn thumb(&mut self, _ui: &Ui, _e: &FileEntry, _s: u32) -> Option<egui::TextureHandle> {
        None
    }
}
