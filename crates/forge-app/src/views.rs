//! File list renderers. Both views are virtualized: only on-screen rows/tiles are laid out,
//! so a folder with 100k entries costs the same per frame as one with 100.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, Ui, Vec2, pos2, vec2};
use forge_core::model::{EntryKind, MediaKind, format_size};
use forge_core::{FileEntry, listing::SortKey};

use crate::commands::Cmd;
use crate::pane::{LoadState, Pane};
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
    Drop {
        paths: Vec<PathBuf>,
        target: PathBuf,
        copy: bool,
    },
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
    Some(Cmd::OpenIn),
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
        let paths: Vec<PathBuf> =
            p.0.iter()
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
            pane.selected_entries()
                .iter()
                .map(|e| e.path.clone())
                .collect()
        } else {
            vec![entry.path.clone()]
        };
        resp.dnd_set_drag_payload(DragPaths(paths));
    }
}

fn row_click(resp: &egui::Response, pane: &mut Pane, vis: usize, out: &mut Vec<ViewEvent>) {
    let m = resp.ctx.input(|i| i.modifiers);
    if resp.clicked() {
        resp.request_focus();
        pane.click(vis, m.command, m.shift);
        out.push(ViewEvent::Focus);
    }
    if resp.secondary_clicked() {
        resp.request_focus();
        if let Some(e) = pane.entry_at(vis)
            && !pane.is_selected(e)
        {
            pane.click(vis, false, false);
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

const DETAILS_WIDTHS_DEFAULT: [f32; 5] = [130.0, 120.0, 90.0, 130.0, 90.0];

#[derive(Clone, Copy)]
struct DetailsColumn {
    label: &'static str,
    sort: Option<SortKey>,
    width_index: Option<usize>,
    width: f32,
}

fn details_columns(
    available_width: f32,
    widths: [f32; 5],
    show_created: bool,
    show_rating: bool,
) -> Vec<DetailsColumn> {
    let mut columns = vec![
        DetailsColumn {
            label: "Name",
            sort: Some(SortKey::Name),
            width_index: None,
            width: 0.0,
        },
        DetailsColumn {
            label: "Date modified",
            sort: Some(SortKey::Modified),
            width_index: Some(0),
            width: widths[0],
        },
        DetailsColumn {
            label: "Type",
            sort: Some(SortKey::Type),
            width_index: Some(1),
            width: widths[1],
        },
        DetailsColumn {
            label: "Size",
            sort: Some(SortKey::Size),
            width_index: Some(2),
            width: widths[2],
        },
    ];
    if show_created {
        columns.push(DetailsColumn {
            label: "Date created",
            sort: Some(SortKey::Created),
            width_index: Some(3),
            width: widths[3],
        });
    }
    if show_rating {
        columns.push(DetailsColumn {
            label: "Rating",
            sort: None,
            width_index: Some(4),
            width: widths[4],
        });
    }
    let fixed_width: f32 = columns.iter().skip(1).map(|column| column.width).sum();
    columns[0].width = (available_width - fixed_width).max(140.0);
    columns
}

fn remember_accessible_focus(ui: &Ui, view_id: egui::Id, response: &egui::Response) {
    if response.has_focus() {
        ui.ctx()
            .data_mut(|data| data.insert_temp(view_id.with("accessible-focus"), response.id));
    }
}

fn remember_cursor_accessible_focus(ui: &Ui, view_id: egui::Id, response: &egui::Response) {
    if response.has_focus() {
        ui.ctx().data_mut(|data| {
            data.insert_temp(view_id.with("cursor-accessible-focus"), response.id);
        });
    }
}

fn view_owns_focus(ui: &Ui, view_id: egui::Id) -> bool {
    let focus = ui.memory(|memory| memory.focused());
    let owner = ui
        .ctx()
        .data(|data| data.get_temp::<egui::Id>(view_id.with("cursor-accessible-focus")));
    focus.is_some() && focus == owner
}

fn register_accessible_node(
    response: &egui::Response,
    role: egui::accesskit::Role,
    label: &str,
    description: Option<&str>,
    selected: Option<bool>,
) {
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Other, response.enabled(), label)
    });
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_role(role);
        node.set_label(label);
        if let Some(description) = description {
            node.set_description(description);
        }
        if let Some(selected) = selected {
            node.set_selected(selected);
        }
    });
}

fn entry_accessibility_description(entry: &FileEntry, annotation: Option<Annotation>) -> String {
    let mut parts = vec![format!("Type: {}", type_label(entry))];
    if entry.kind == EntryKind::File {
        parts.push(format!("Size: {}", format_size(entry.size)));
    }
    if let Some(modified) = entry.modified {
        parts.push(format!("Date modified: {}", format_time(Some(modified))));
    }
    if let Some(created) = entry.created {
        parts.push(format!("Date created: {}", format_time(Some(created))));
    }
    if let Some(annotation) = annotation {
        if annotation.rating > 0 {
            parts.push(format!("Rating: {} of 5 stars", annotation.rating.min(5)));
        }
        if let Some(label) = annotation
            .label
            .and_then(|index| ["red", "yellow", "green", "blue", "purple"].get(index as usize))
        {
            parts.push(format!("Color label: {label}"));
        }
        match annotation.pick {
            1 => parts.push("Picked".into()),
            -1 => parts.push("Rejected".into()),
            _ => {}
        }
    }
    parts.join(". ")
}

fn consume_exact_shortcut(input: &mut egui::InputState, shortcut: &egui::KeyboardShortcut) -> bool {
    if matches!(
        shortcut.logical_key,
        egui::Key::Plus | egui::Key::Equals | egui::Key::Minus
    ) {
        return input.consume_shortcut(shortcut);
    }
    let mut matched = false;
    input.events.retain(|event| match event {
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } if !matched
            && *key == shortcut.logical_key
            && modifiers.matches_exact(shortcut.modifiers) =>
        {
            matched = true;
            false
        }
        _ => true,
    });
    matched
}

fn handle_focused_view_keys(
    ui: &Ui,
    pane: &mut Pane,
    view_id: egui::Id,
    columns: usize,
    out: &mut Vec<ViewEvent>,
) {
    if !view_owns_focus(ui, view_id) {
        return;
    }

    // The app's global handler treats every focused widget as a text field. Keep its
    // shortcuts and cursor navigation available while an accessible row/tile owns focus.
    let mut seen_commands = Vec::new();
    ui.ctx().input_mut(|input| {
        for command in Cmd::ALL {
            if let Some(shortcut) = command.shortcut()
                && consume_exact_shortcut(input, &shortcut)
                && !seen_commands.contains(command)
            {
                seen_commands.push(*command);
            }
        }
        input.events.retain(|event| match event {
            egui::Event::Copy => {
                if !seen_commands.contains(&Cmd::Copy) {
                    seen_commands.push(Cmd::Copy);
                }
                false
            }
            egui::Event::Cut => {
                if !seen_commands.contains(&Cmd::Cut) {
                    seen_commands.push(Cmd::Cut);
                }
                false
            }
            egui::Event::Paste(_) => {
                if !seen_commands.contains(&Cmd::Paste) {
                    seen_commands.push(Cmd::Paste);
                }
                false
            }
            _ => true,
        });
    });
    out.extend(seen_commands.into_iter().map(ViewEvent::Command));

    let (modifiers, keys, text) = ui.ctx().input_mut(|input| {
        let modifiers = input.modifiers;
        let keys = [
            egui::Key::ArrowUp,
            egui::Key::ArrowDown,
            egui::Key::ArrowLeft,
            egui::Key::ArrowRight,
            egui::Key::PageUp,
            egui::Key::PageDown,
            egui::Key::Home,
            egui::Key::End,
            egui::Key::Enter,
            egui::Key::Backspace,
            egui::Key::Space,
            egui::Key::Escape,
        ]
        .into_iter()
        .filter(|key| input.key_pressed(*key))
        .collect::<Vec<_>>();
        for key in &keys {
            input.consume_key(modifiers, *key);
        }
        let text = input
            .events
            .iter()
            .filter_map(|event| match event {
                egui::Event::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<String>();
        (modifiers, keys, text)
    });
    if !modifiers.alt {
        for key in keys {
            match key {
                egui::Key::ArrowDown => {
                    pane.move_cursor(columns as isize, modifiers.shift, modifiers.command)
                }
                egui::Key::ArrowUp => {
                    pane.move_cursor(-(columns as isize), modifiers.shift, modifiers.command)
                }
                egui::Key::ArrowRight if columns > 1 => {
                    pane.move_cursor(1, modifiers.shift, modifiers.command)
                }
                egui::Key::ArrowLeft if columns > 1 => {
                    pane.move_cursor(-1, modifiers.shift, modifiers.command)
                }
                egui::Key::PageDown => pane.move_cursor(20, modifiers.shift, modifiers.command),
                egui::Key::PageUp => pane.move_cursor(-20, modifiers.shift, modifiers.command),
                egui::Key::Home => pane.move_cursor(-(1 << 30), modifiers.shift, modifiers.command),
                egui::Key::End => pane.move_cursor(1 << 30, modifiers.shift, modifiers.command),
                egui::Key::Space if modifiers.command => {
                    if let Some(cursor) = pane.cursor {
                        pane.click(cursor, true, false);
                    }
                }
                egui::Key::Enter => {
                    if let Some(cursor) = pane.cursor {
                        out.push(ViewEvent::Activate(cursor));
                    }
                }
                egui::Key::Backspace => out.push(ViewEvent::Command(Cmd::Up)),
                egui::Key::Escape => {
                    if !pane.filter_text.is_empty() {
                        pane.set_filter(String::new());
                    } else {
                        pane.clear_selection();
                    }
                }
                _ => {}
            }
        }
    }
    let text: String = text
        .chars()
        .filter(|character| !character.is_control() && *character != ' ')
        .collect();
    if !text.is_empty() && !modifiers.command {
        let now = ui.input(|input| input.time);
        let key = view_id.with("type-ahead");
        let (mut prefix, previous) = ui
            .ctx()
            .data(|data| data.get_temp::<(String, f64)>(key).unwrap_or_default());
        if now - previous > 0.9 {
            prefix.clear();
        }
        prefix.push_str(&text);
        pane.type_ahead(&prefix);
        ui.ctx()
            .data_mut(|data| data.insert_temp(key, (prefix, now)));
    }
}

fn resize_column_pair(
    columns: &[DetailsColumn],
    boundary: usize,
    widths: &mut [f32; 5],
    name_width: f32,
    delta: f32,
) {
    let left = columns[boundary].width_index;
    let right = columns[boundary + 1].width_index;
    match (left, right) {
        (None, Some(right)) => {
            let applied = delta.clamp(
                -(name_width - 140.0).max(0.0),
                (widths[right] - 68.0).max(0.0),
            );
            widths[right] = (widths[right] - applied).max(68.0);
        }
        (Some(left), Some(right)) => {
            let applied = delta.clamp(68.0 - widths[left], widths[right] - 68.0);
            widths[left] += applied;
            widths[right] -= applied;
        }
        _ => {}
    }
}

fn truncated_name_galley(
    painter: &egui::Painter,
    name: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> (std::sync::Arc<egui::Galley>, bool) {
    let full = painter.layout_no_wrap(name.to_owned(), font.clone(), color);
    if full.size().x <= max_width {
        return (full, false);
    }
    let ends: Vec<usize> = name
        .char_indices()
        .map(|(index, character)| index + character.len_utf8())
        .collect();
    let mut low = 0;
    let mut high = ends.len();
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let prefix = &name[..ends[middle - 1]];
        let candidate = painter.layout_no_wrap(format!("{prefix}\u{2026}"), font.clone(), color);
        if candidate.size().x <= max_width {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let text = if low == 0 {
        "\u{2026}".to_owned()
    } else {
        format!("{}\u{2026}", &name[..ends[low - 1]])
    };
    (painter.layout_no_wrap(text, font, color), true)
}

pub fn details(
    ui: &mut Ui,
    pane: &mut Pane,
    focused: bool,
    id: egui::Id,
    thumbs: &dyn ThumbProvider,
) -> Vec<ViewEvent> {
    let mut out = Vec::new();
    let row_h = ui
        .spacing()
        .interact_size
        .y
        .max(ui.text_style_height(&egui::TextStyle::Body) + 8.0);
    let width = ui.available_width();
    let widths_key = id.with("details-column-widths");
    let mut widths = ui
        .ctx()
        .data_mut(|data| data.get_persisted::<[f32; 5]>(widths_key))
        .unwrap_or(DETAILS_WIDTHS_DEFAULT);
    let visibility_key = id.with("details-column-visibility");
    let mut visible_columns = ui
        .ctx()
        .data_mut(|data| data.get_persisted::<[bool; 2]>(visibility_key))
        .unwrap_or([false, false]);
    let columns = details_columns(width, widths, visible_columns[0], visible_columns[1]);

    if let LoadState::Failed(error) = &pane.load {
        ui.label(format!("Folder listing failed: {error}"));
    }
    if focused {
        handle_focused_view_keys(ui, pane, id, 1, &mut out);
    }

    let header_width = width.max(columns.iter().map(|column| column.width).sum());
    let (header_rect, _) = ui.allocate_exact_size(vec2(header_width, row_h), Sense::hover());
    let header_painter = ui.painter_at(header_rect);
    header_painter.line_segment(
        [header_rect.left_bottom(), header_rect.right_bottom()],
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
    let mut x = header_rect.left();
    for (index, column) in columns.iter().enumerate() {
        let rect = Rect::from_min_size(pos2(x, header_rect.top()), vec2(column.width, row_h));
        let response = ui.interact(rect, id.with(("hdr", index)), Sense::click());
        remember_accessible_focus(ui, id, &response);
        if response.hovered() {
            header_painter.rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        let sorted = column.sort.is_some_and(|key| pane.sort.key == key);
        let arrow = if sorted {
            if pane.sort.descending {
                " \u{2193}"
            } else {
                " \u{2191}"
            }
        } else {
            ""
        };
        let right_aligned = matches!(column.label, "Size" | "Rating");
        let text_x = if right_aligned {
            rect.right() - 10.0
        } else {
            rect.left() + 8.0
        };
        header_painter.text(
            pos2(text_x, rect.center().y),
            if right_aligned {
                Align2::RIGHT_CENTER
            } else {
                Align2::LEFT_CENTER
            },
            format!("{}{arrow}", column.label),
            FontId::proportional(12.0),
            theme::muted(ui),
        );
        response
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, column.label));
        response.ctx.accesskit_node_builder(response.id, |node| {
            node.set_role(egui::accesskit::Role::ColumnHeader);
            node.set_label(column.label);
            if let Some(key) = column.sort {
                if pane.sort.key == key {
                    node.set_sort_direction(if pane.sort.descending {
                        egui::accesskit::SortDirection::Descending
                    } else {
                        egui::accesskit::SortDirection::Ascending
                    });
                    node.set_description(format!(
                        "Sorted {}. Activate to change sort order.",
                        if pane.sort.descending {
                            "descending"
                        } else {
                            "ascending"
                        }
                    ));
                } else {
                    node.set_description(format!("Activate to sort by {}.", column.label));
                }
            }
        });
        if response.clicked() {
            ui.ctx()
                .data_mut(|data| data.insert_temp(id.with("accessible-focus"), response.id));
            if let Some(key) = column.sort {
                pane.set_sort(key);
            }
        }
        response.context_menu(|menu| {
            menu.label("Columns");
            let mut created = visible_columns[0];
            if menu.checkbox(&mut created, "Date created").changed() {
                visible_columns[0] = created;
                menu.ctx()
                    .data_mut(|data| data.insert_persisted(visibility_key, visible_columns));
            }
            let mut rating = visible_columns[1];
            if menu.checkbox(&mut rating, "Rating").changed() {
                visible_columns[1] = rating;
                menu.ctx()
                    .data_mut(|data| data.insert_persisted(visibility_key, visible_columns));
            }
        });
        x += column.width;

        if index + 1 < columns.len() {
            let separator_rect = Rect::from_center_size(pos2(x, rect.center().y), vec2(8.0, row_h));
            let separator = ui
                .interact(separator_rect, id.with(("resize", index)), Sense::drag())
                .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
            remember_accessible_focus(ui, id, &separator);
            let label = format!(
                "Resize {} and {} columns",
                column.label,
                columns[index + 1].label
            );
            separator.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::ResizeHandle, true, label.clone())
            });
            separator.ctx.accesskit_node_builder(separator.id, |node| {
                node.set_role(egui::accesskit::Role::Splitter);
                node.set_label(label);
                node.set_orientation(egui::accesskit::Orientation::Horizontal);
            });
            if separator.drag_started() {
                separator
                    .ctx
                    .data_mut(|data| data.insert_temp(id.with(("resize-delta", index)), 0.0_f32));
            }
            if separator.dragged() {
                let delta_key = id.with(("resize-delta", index));
                let total_delta = separator.drag_delta().x;
                let previous_delta = separator
                    .ctx
                    .data(|data| data.get_temp::<f32>(delta_key))
                    .unwrap_or(0.0);
                resize_column_pair(
                    &columns,
                    index,
                    &mut widths,
                    columns[0].width,
                    total_delta - previous_delta,
                );
                separator.ctx.data_mut(|data| {
                    data.insert_temp(delta_key, total_delta);
                    data.insert_persisted(widths_key, widths);
                });
            }
            if separator.drag_stopped() {
                separator
                    .ctx
                    .data_mut(|data| data.remove_temp::<f32>(id.with(("resize-delta", index))));
            }
            header_painter.line_segment(
                [pos2(x, rect.top() + 4.0), pos2(x, rect.bottom() - 4.0)],
                ui.visuals().widgets.noninteractive.bg_stroke,
            );
        }
    }
    let columns = details_columns(width, widths, visible_columns[0], visible_columns[1]);
    let table_width = width.max(columns.iter().map(|column| column.width).sum());

    let n = pane.visible.len();
    let mut scroll = egui::ScrollArea::vertical()
        .id_salt(id.with("rows"))
        .auto_shrink([false, false]);
    if pane.scroll_to_cursor {
        if let Some(cursor) = pane.cursor {
            let offset = ui
                .ctx()
                .data(|data| data.get_temp::<f32>(id.with("off")))
                .unwrap_or(0.0);
            let view_h = ui.available_height();
            let top = cursor as f32 * row_h;
            let target = if top < offset {
                top
            } else if top + row_h > offset + view_h - row_h {
                top + 2.0 * row_h - view_h
            } else {
                offset
            };
            scroll = scroll.vertical_scroll_offset(target.max(0.0));
        }
        pane.scroll_to_cursor = false;
    }

    if n == 0 {
        let message = match &pane.load {
            LoadState::Loading { .. } => "Loading folder...",
            LoadState::Ready { .. } if pane.entries.is_empty() => "This folder is empty.",
            LoadState::Ready { .. } => "No files match this filter.",
            LoadState::Failed(_) => "No files are available.",
        };
        ui.add_space(12.0);
        ui.label(message);
    }

    let previous_spacing = ui.spacing().item_spacing;
    ui.spacing_mut().item_spacing.y = 0.0;
    let sa = scroll.show_rows(ui, row_h, n, |ui, range| {
        for vis in range {
            let Some(&entry_index) = pane.visible.get(vis) else {
                break;
            };
            let entry = &pane.entries[entry_index];
            let annotation = thumbs.annotation(&entry.path);
            let selected = pane.is_selected(entry);
            let (rect, response) =
                ui.allocate_exact_size(vec2(table_width, row_h), Sense::click_and_drag());
            let response = response.on_hover_cursor(egui::CursorIcon::Default);
            let painter = ui.painter_at(rect);
            let drop_hover = if entry.kind != EntryKind::File {
                handle_drop(&response, &entry.path, &mut out)
            } else {
                false
            };
            if selected {
                painter.rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
            } else if response.hovered() || drop_hover {
                painter.rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
            } else if vis % 2 == 1 {
                painter.rect_filled(rect, 0.0, ui.visuals().faint_bg_color.gamma_multiply(0.5));
            }
            if drop_hover {
                painter.rect_stroke(
                    rect,
                    3.0,
                    Stroke::new(1.5, theme::ACCENT),
                    egui::StrokeKind::Inside,
                );
            }
            if focused && pane.cursor == Some(vis) {
                let focus_color = if ui.visuals().dark_mode {
                    Color32::WHITE
                } else {
                    Color32::from_rgb(20, 66, 125)
                };
                painter.rect_stroke(
                    rect.shrink(1.0),
                    3.0,
                    Stroke::new(2.0, focus_color),
                    egui::StrokeKind::Inside,
                );
            }
            let text_color = if selected {
                ui.visuals().selection.stroke.color
            } else if entry.hidden {
                ui.visuals().weak_text_color()
            } else {
                ui.visuals().text_color()
            };
            let font = FontId::proportional(13.0);
            let small = FontId::proportional(12.0);
            let cy = rect.center().y;
            let mut col_x = rect.left();
            let name_column = columns[0];
            painter.text(
                pos2(col_x + 8.0, cy),
                Align2::LEFT_CENTER,
                icon_for(entry),
                font.clone(),
                icon_color(entry),
            );
            let mut name_left = col_x + 30.0;
            if let Some(label_color) = annotation
                .and_then(|value| value.label)
                .and_then(|index| theme::LABEL_COLORS.get(index as usize))
            {
                painter.circle_filled(pos2(name_left + 4.0, cy), 4.0, *label_color);
                name_left += 13.0;
            }
            let name_width = (name_column.width - (name_left - col_x) - 10.0).max(0.0);
            let name_rect =
                Rect::from_min_size(pos2(name_left, rect.top()), vec2(name_width, row_h));
            let (galley, truncated) =
                truncated_name_galley(&painter, &entry.name, font, text_color, name_width);
            painter.with_clip_rect(name_rect.intersect(rect)).galley(
                pos2(name_rect.left(), cy - galley.size().y / 2.0),
                galley,
                text_color,
            );
            col_x += name_column.width;

            for column in columns.iter().skip(1) {
                let text = match column.label {
                    "Date modified" => format_time(entry.modified),
                    "Type" => type_label(entry),
                    "Size" if entry.kind == EntryKind::File => format_size(entry.size),
                    "Size" => String::new(),
                    "Date created" => {
                        let created = format_time(entry.created);
                        if created.is_empty() {
                            "\u{2014}".to_owned()
                        } else {
                            created
                        }
                    }
                    "Rating" => String::new(),
                    _ => String::new(),
                };
                if column.label == "Rating" {
                    let rating = annotation.map_or(0, |value| value.rating.min(5));
                    let stars = format!(
                        "{}{}",
                        "\u{2605}".repeat(rating as usize),
                        "\u{2606}".repeat(5 - rating as usize)
                    );
                    let color = if selected {
                        text_color
                    } else {
                        Color32::from_rgb(190, 139, 32)
                    };
                    painter.text(
                        pos2(col_x + column.width - 9.0, cy),
                        Align2::RIGHT_CENTER,
                        stars,
                        small.clone(),
                        color,
                    );
                } else if column.label == "Size" {
                    painter
                        .with_clip_rect(Rect::from_min_size(
                            pos2(col_x + 4.0, rect.top()),
                            vec2((column.width - 8.0).max(0.0), row_h),
                        ))
                        .text(
                            pos2(col_x + column.width - 9.0, cy),
                            Align2::RIGHT_CENTER,
                            text,
                            small.clone(),
                            text_color,
                        );
                } else {
                    painter
                        .with_clip_rect(Rect::from_min_size(
                            pos2(col_x + 4.0, rect.top()),
                            vec2((column.width - 8.0).max(0.0), row_h),
                        ))
                        .text(
                            pos2(col_x + 6.0, cy),
                            Align2::LEFT_CENTER,
                            text,
                            small.clone(),
                            text_color,
                        );
                }
                col_x += column.width;
            }

            let description = entry_accessibility_description(entry, annotation);
            register_accessible_node(
                &response,
                egui::accesskit::Role::ListItem,
                &entry.name,
                Some(&description),
                Some(selected),
            );
            let should_follow_cursor = focused && pane.cursor == Some(vis);
            let focused_id = ui.memory(|memory| memory.focused());
            let cursor_owner = ui
                .ctx()
                .data(|data| data.get_temp::<egui::Id>(id.with("cursor-accessible-focus")));
            if should_follow_cursor && (focused_id.is_none() || focused_id == cursor_owner) {
                response.request_focus();
                ui.ctx().data_mut(|data| {
                    data.insert_temp(id.with("accessible-focus"), response.id);
                    data.insert_temp(id.with("cursor-accessible-focus"), response.id);
                });
            }
            remember_accessible_focus(ui, id, &response);
            remember_cursor_accessible_focus(ui, id, &response);
            let response = if truncated {
                response.on_hover_text_at_pointer(entry.name.clone())
            } else {
                response
            };
            let entry_clone = entry.clone();
            begin_drag(&response, pane, &entry_clone);
            row_click(&response, pane, vis, &mut out);
            if response.clicked() || response.secondary_clicked() {
                ui.ctx().data_mut(|data| {
                    data.insert_temp(id.with("accessible-focus"), response.id);
                    data.insert_temp(id.with("cursor-accessible-focus"), response.id);
                });
            }
            remember_accessible_focus(ui, id, &response);
            remember_cursor_accessible_focus(ui, id, &response);
            response.context_menu(|menu| menu_items(menu, CONTEXT_FILE, &mut out));
        }
    });
    ui.spacing_mut().item_spacing = previous_spacing;
    ui.ctx()
        .data_mut(|data| data.insert_temp(id.with("off"), sa.state.offset.y));
    ui.ctx()
        .accesskit_node_builder(sa.id, |node| node.set_role(egui::accesskit::Role::List));

    let background = ui.interact(sa.inner_rect, id.with("bg"), Sense::click());
    background_interactions(ui, &background, pane, &mut out);
    out
}

fn background_interactions(
    ui: &Ui,
    bg: &egui::Response,
    pane: &mut Pane,
    out: &mut Vec<ViewEvent>,
) {
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
    if focused {
        handle_focused_view_keys(ui, pane, id, per_row, &mut out);
    }
    // Stretch cells to fill the row exactly.
    let cell_w = width / per_row as f32;
    let n = pane.visible.len();
    let rows = n.div_ceil(per_row);
    pane_grid_columns_store(ui, id, per_row);

    if n == 0 {
        let message = match &pane.load {
            LoadState::Loading { .. } => "Loading folder…",
            LoadState::Ready { .. } if pane.entries.is_empty() => "This folder is empty.",
            LoadState::Ready { .. } => "No files match this filter.",
            LoadState::Failed(error) => {
                ui.label(format!("Folder listing failed: {error}"));
                "No files are available."
            }
        };
        ui.add_space(12.0);
        ui.label(message);
    }

    let mut scroll = egui::ScrollArea::vertical()
        .id_salt(id.with("grid"))
        .auto_shrink([false, false]);
    if pane.scroll_to_cursor {
        if let Some(c) = pane.cursor {
            let row = c / per_row;
            let off = ui
                .ctx()
                .data(|d| d.get_temp::<f32>(id.with("goff")))
                .unwrap_or(0.0);
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
                    let Some(&ei) = pane.visible.get(vis) else {
                        break;
                    };
                    let (rect, resp) =
                        ui.allocate_exact_size(vec2(cell_w, cell.y), Sense::click_and_drag());
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
                        let focus_color = if ui.visuals().dark_mode {
                            Color32::WHITE
                        } else {
                            Color32::from_rgb(20, 66, 125)
                        };
                        p.rect_stroke(
                            inner,
                            6.0,
                            Stroke::new(2.0, focus_color),
                            egui::StrokeKind::Inside,
                        );
                    }
                    let img_rect = Rect::from_center_size(
                        pos2(rect.center().x, rect.top() + pad + thumb_size / 2.0),
                        Vec2::splat(thumb_size),
                    );
                    let mut drew = false;
                    let mut badge_rect = img_rect;
                    if entry.kind == EntryKind::File
                        && matches!(
                            entry.media_kind(),
                            MediaKind::Image | MediaKind::Raw | MediaKind::Video
                        )
                    {
                        if let Some(tex) = thumbs.thumb(ui, &entry, thumb_size.round() as u32) {
                            let sz = tex.size_vec2();
                            let scale = (thumb_size / sz.x).min(thumb_size / sz.y);
                            let r = Rect::from_center_size(img_rect.center(), sz * scale);
                            p.image(
                                tex.id(),
                                r,
                                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                                Color32::WHITE,
                            );
                            badge_rect = r;
                            if entry.media_kind() == MediaKind::Video {
                                p.text(
                                    r.left_bottom() + vec2(4.0, -4.0),
                                    Align2::LEFT_BOTTOM,
                                    "▶",
                                    FontId::proportional(14.0),
                                    Color32::WHITE,
                                );
                            }
                            drew = true;
                        } else {
                            p.rect_filled(
                                img_rect.shrink(thumb_size * 0.1),
                                4.0,
                                ui.visuals().faint_bg_color,
                            );
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
                    let annotation = thumbs.annotation(&entry.path);
                    if let Some(a) = annotation {
                        draw_annotation(&p, badge_rect, a);
                    }
                    let text_rect = Rect::from_min_max(
                        pos2(rect.left() + 4.0, img_rect.bottom() + 4.0),
                        pos2(rect.right() - 4.0, rect.bottom() - 2.0),
                    );
                    let label_color = if selected {
                        ui.visuals().selection.stroke.color
                    } else {
                        ui.visuals().text_color()
                    };
                    let galley = ui.painter().layout(
                        entry.name.clone(),
                        FontId::proportional(12.0),
                        label_color,
                        text_rect.width(),
                    );
                    p.with_clip_rect(text_rect).galley(
                        pos2(
                            text_rect.center().x - galley.size().x / 2.0,
                            text_rect.top(),
                        ),
                        galley,
                        label_color,
                    );
                    let description = entry_accessibility_description(&entry, annotation);
                    register_accessible_node(
                        &resp,
                        egui::accesskit::Role::GridCell,
                        &entry.name,
                        Some(&description),
                        Some(selected),
                    );
                    let should_follow_cursor = focused && pane.cursor == Some(vis);
                    let focused_id = ui.memory(|memory| memory.focused());
                    let cursor_owner = ui
                        .ctx()
                        .data(|data| data.get_temp::<egui::Id>(id.with("cursor-accessible-focus")));
                    if should_follow_cursor && (focused_id.is_none() || focused_id == cursor_owner)
                    {
                        resp.request_focus();
                        ui.ctx().data_mut(|data| {
                            data.insert_temp(id.with("accessible-focus"), resp.id);
                            data.insert_temp(id.with("cursor-accessible-focus"), resp.id);
                        });
                    }
                    remember_accessible_focus(ui, id, &resp);
                    remember_cursor_accessible_focus(ui, id, &resp);
                    let resp = resp.on_hover_text_at_pointer(entry.name.clone());
                    begin_drag(&resp, pane, &entry);
                    row_click(&resp, pane, vis, &mut out);
                    if resp.clicked() || resp.secondary_clicked() {
                        ui.ctx().data_mut(|data| {
                            data.insert_temp(id.with("accessible-focus"), resp.id);
                            data.insert_temp(id.with("cursor-accessible-focus"), resp.id);
                        });
                    }
                    remember_accessible_focus(ui, id, &resp);
                    remember_cursor_accessible_focus(ui, id, &resp);
                    resp.context_menu(|ui| menu_items(ui, CONTEXT_FILE, &mut out));
                }
            });
        }
    });
    ui.spacing_mut().item_spacing = prev_spacing;
    ui.ctx()
        .data_mut(|d| d.insert_temp(id.with("goff"), sa.state.offset.y));
    ui.ctx()
        .accesskit_node_builder(sa.id, |node| node.set_role(egui::accesskit::Role::Grid));
    let bg = ui.interact(sa.inner_rect, id.with("bg"), Sense::click());
    background_interactions(ui, &bg, pane, &mut out);
    out
}

fn pane_grid_columns_store(ui: &Ui, id: egui::Id, per_row: usize) {
    ui.ctx()
        .data_mut(|d| d.insert_temp(id.with("per_row"), per_row));
}

/// Columns of the last rendered grid (for Up/Down keyboard navigation).
pub fn grid_columns(ctx: &egui::Context, id: egui::Id) -> usize {
    ctx.data(|d| d.get_temp::<usize>(id.with("per_row")))
        .unwrap_or(1)
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
    if let Some(l) = a.label
        && let Some(c) = theme::LABEL_COLORS.get(l as usize)
    {
        p.circle_filled(img_rect.right_top() + vec2(-8.0, 8.0), 5.0, *c);
    }
    match a.pick {
        1 => {
            p.text(
                img_rect.left_top() + vec2(4.0, 2.0),
                Align2::LEFT_TOP,
                "⚑",
                FontId::proportional(14.0),
                Color32::from_rgb(90, 200, 110),
            );
        }
        -1 => {
            p.rect_filled(img_rect, 4.0, Color32::from_black_alpha(130));
            p.text(
                img_rect.left_top() + vec2(4.0, 2.0),
                Align2::LEFT_TOP,
                "✖",
                FontId::proportional(14.0),
                Color32::from_rgb(230, 80, 80),
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod accessibility_tests {
    use std::time::SystemTime;

    use egui::{Rect, Ui, pos2, vec2};
    use forge_core::listing::SortSpec;
    use forge_core::{FileEntry, model::EntryKind};

    use super::{LoadState, ThumbProvider, details};
    use crate::{pane::Pane, settings::ViewMode};

    struct NoThumbs;

    impl ThumbProvider for NoThumbs {
        fn thumb(
            &mut self,
            _ui: &Ui,
            _entry: &FileEntry,
            _size: u32,
        ) -> Option<egui::TextureHandle> {
            None
        }
    }

    #[test]
    fn details_rows_emit_named_selected_accesskit_nodes() {
        let directory = tempfile::tempdir().expect("temp directory");
        let mut pane = Pane::new(
            directory.path().to_path_buf(),
            SortSpec::default(),
            ViewMode::Details,
        );
        let selected_path = directory.path().join("selected.png");
        pane.entries = vec![
            FileEntry {
                path: selected_path.clone(),
                name: "selected.png".into(),
                kind: EntryKind::File,
                size: 1024,
                modified: Some(SystemTime::UNIX_EPOCH),
                created: None,
                hidden: false,
                readonly: false,
                system: false,
                ext: "png".into(),
            },
            FileEntry {
                path: directory.path().join("notes.txt"),
                name: "notes.txt".into(),
                kind: EntryKind::File,
                size: 12,
                modified: None,
                created: None,
                hidden: false,
                readonly: false,
                system: false,
                ext: "txt".into(),
            },
        ];
        pane.visible = vec![0, 1];
        pane.load = LoadState::Ready { errors: 0 };
        pane.selected.insert(selected_path);
        pane.cursor = Some(0);

        let context = egui::Context::default();
        context.enable_accesskit();
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 500.0))),
                ..Default::default()
            },
            |ui| {
                details(
                    ui,
                    &mut pane,
                    true,
                    egui::Id::new("details-a11y-test"),
                    &NoThumbs,
                );
            },
        );
        let update = output
            .platform_output
            .accesskit_update
            .take()
            .expect("AccessKit tree update from enabled context");

        let selected_node = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("selected.png"))
            .expect("selected file node");
        assert_eq!(selected_node.1.role(), egui::accesskit::Role::ListItem);
        assert_eq!(selected_node.1.is_selected(), Some(true));
        assert!(
            selected_node
                .1
                .description()
                .unwrap_or_default()
                .contains("PNG image")
        );
        assert_eq!(
            &selected_node.0, &update.focus,
            "keyboard cursor row owns AccessKit focus"
        );

        let unselected_node = update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("notes.txt"))
            .expect("unselected file node");
        assert_eq!(unselected_node.1.role(), egui::accesskit::Role::ListItem);
        assert_eq!(unselected_node.1.is_selected(), Some(false));

        output.drop_without_applying_deltas();
    }
}
