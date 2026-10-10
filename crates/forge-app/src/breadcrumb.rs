//! Address bar: clickable breadcrumb segments, switching to an editable text field with
//! folder autocomplete on click or Ctrl+L.

use std::path::{Path, PathBuf};

use egui::{Key, RichText, Ui};

use crate::app::ZxrawApp;

fn segments(path: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut acc = PathBuf::new();
    for c in path.components() {
        acc.push(c.as_os_str());
        let label = match c {
            std::path::Component::Prefix(p) => p.as_os_str().to_string_lossy().into_owned(),
            std::path::Component::RootDir => continue,
            other => other.as_os_str().to_string_lossy().into_owned(),
        };
        // Make "C:" navigable as "C:\".
        let target = if matches!(c, std::path::Component::Prefix(_)) {
            let mut p = acc.clone();
            p.push("\\");
            p
        } else {
            acc.clone()
        };
        out.push((label, target));
    }
    out
}

/// Folder-name completions for a partially typed path (cheap: one read_dir of the parent,
/// capped).
fn completions(text: &str) -> Vec<String> {
    let t = text.trim_matches('"');
    let (dir, prefix) = match t.rfind(['\\', '/']) {
        Some(i) => (&t[..=i], &t[i + 1..]),
        None => return Vec::new(),
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let p = prefix.to_lowercase();
    let mut v: Vec<String> = rd
        .take(5000)
        .filter_map(Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().starts_with(&p))
        .take(12)
        .map(|n| format!("{dir}{n}"))
        .collect();
    v.sort_by(|a, b| forge_core::listing::natural_cmp(a, b));
    v
}

pub fn address_bar(app: &mut ZxrawApp, ui: &mut Ui) {
    let editing = app.pane().editing_path.is_some() || app.focus_address;
    if !editing {
        let path = app.pane().path.clone();
        let frame = egui::Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .corner_radius(4)
            .inner_margin(egui::Margin::symmetric(4, 2));
        let resp = frame
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let segs = segments(&path);
                    let mut nav = None;
                    for (i, (label, target)) in segs.iter().enumerate() {
                        if i > 0 {
                            ui.label(RichText::new(" › ").weak());
                        }
                        if ui.add(egui::Button::new(label.as_str()).frame_when_inactive(false)).clicked() {
                            nav = Some(target.clone());
                        }
                    }
                    if let Some(t) = nav {
                        app.navigate(t);
                    }
                    // Click on the blank part of the bar to edit.
                    let rest = ui.allocate_response(egui::vec2(ui.available_width(), 20.0), egui::Sense::click());
                    if rest.clicked() {
                        app.pane_mut().editing_path = Some(path.to_string_lossy().into_owned());
                        app.focus_address = true;
                    }
                });
            })
            .response;
        let _ = resp;
        return;
    }

    let mut text = app
        .pane()
        .editing_path
        .clone()
        .unwrap_or_else(|| app.pane().path.to_string_lossy().into_owned());
    let id = egui::Id::new("address-edit");
    let r = ui.add(
        egui::TextEdit::singleline(&mut text)
            .id(id)
            .desired_width(ui.available_width())
            .hint_text("Type a path, e.g. D:\\Shoots or \\\\server\\share"),
    );
    if app.focus_address {
        r.request_focus();
        if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), id) {
            let all = egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(text.chars().count()),
            );
            st.cursor.set_char_range(Some(all));
            st.store(ui.ctx(), id);
        }
        app.focus_address = false;
    }
    let enter = ui.input(|i| i.key_pressed(Key::Enter));
    let esc = ui.input(|i| i.key_pressed(Key::Escape));
    let tab = r.has_focus() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Tab));

    // Autocomplete popup.
    let comps = if r.has_focus() { completions(&text) } else { Vec::new() };
    if tab {
        if let Some(first) = comps.first() {
            text = format!("{first}\\");
            ui.ctx().memory_mut(|m| m.request_focus(id));
        }
    }
    if r.has_focus() && !comps.is_empty() {
        egui::Area::new(id.with("ac"))
            .fixed_pos(r.rect.left_bottom() + egui::vec2(0.0, 2.0))
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(r.rect.width().min(600.0));
                    for c in &comps {
                        if ui.selectable_label(false, c).clicked() {
                            text = format!("{c}\\");
                        }
                    }
                    ui.label(RichText::new("Tab to complete").small().weak());
                });
            });
    }

    if r.lost_focus() && enter {
        let target = forge_core::paths::normalize_user_path(&text);
        app.pane_mut().editing_path = None;
        app.navigate(target);
    } else if esc || (r.lost_focus() && !enter && !tab) {
        app.pane_mut().editing_path = None;
    } else {
        app.pane_mut().editing_path = Some(text);
    }
}
