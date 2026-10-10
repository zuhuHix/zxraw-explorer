//! Navigation sidebar: quick access, bookmarks, drives.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use egui::{RichText, Ui};
use parking_lot::Mutex;

use crate::app::ForgeApp;
use crate::theme;
use crate::views::DragPaths;

#[derive(Clone, Debug)]
pub struct Drive {
    pub root: PathBuf,
    pub label: String,
    pub kind: &'static str,
    pub total: u64,
    pub free: u64,
    pub available: bool,
}

pub struct SidebarState {
    pub drives: Arc<Mutex<Vec<Drive>>>,
    pub known: Vec<(String, PathBuf)>,
}

impl SidebarState {
    pub fn new(ctx: egui::Context) -> Self {
        let drives: Arc<Mutex<Vec<Drive>>> = Arc::new(Mutex::new(Vec::new()));
        let d = drives.clone();
        // Drive enumeration can stall on disconnected network drives; it lives on its own
        // thread and refreshes periodically to pick up inserted cards / USB drives.
        let _ = std::thread::Builder::new()
            .name("drives".into())
            .spawn(move || {
                loop {
                    let list = crate::platform::drives();
                    // A timed-out drive query leaves its worker thread blocked in the OS;
                    // back off so a dead network share can't accumulate threads.
                    let any_hung = list.iter().any(|d| !d.available && d.kind == "Network");
                    let changed = {
                        let mut g = d.lock();
                        let changed = g.len() != list.len()
                            || g.iter().zip(&list).any(|(a, b)| a.root != b.root || a.available != b.available || a.free != b.free);
                        *g = list;
                        changed
                    };
                    if changed {
                        ctx.request_repaint();
                    }
                    std::thread::sleep(Duration::from_secs(if any_hung { 60 } else { 3 }));
                }
            });
        Self {
            drives,
            known: crate::platform::known_folders(),
        }
    }
}

fn nav_item(app: &mut ForgeApp, ui: &mut Ui, icon: &str, label: &str, path: &PathBuf, sub: Option<String>) -> egui::Response {
    let current = &app.pane().path == path;
    let text = match &sub {
        Some(s) => format!("{icon}  {label}   {s}"),
        None => format!("{icon}  {label}"),
    };
    let r = ui.add(egui::Button::selectable(current, text).frame_when_inactive(false).min_size(egui::vec2(ui.available_width(), 0.0)));
    let r = r.on_hover_text(path.to_string_lossy());
    if r.clicked() {
        app.navigate(path.clone());
    }
    if r.middle_clicked() {
        app.run(crate::commands::Cmd::NewTab, &ui.ctx().clone());
        app.navigate(path.clone());
    }
    // Sidebar entries accept drops (copy/move into that folder).
    if let Some(p) = r.dnd_release_payload::<DragPaths>() {
        let copy = ui.input(|i| i.modifiers.command);
        crate::actions::transfer(app, p.0.clone(), path.clone(), copy);
    }
    r
}

pub fn show(app: &mut ForgeApp, ui: &mut Ui) {
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(4.0);
        section(ui, "Quick access");
        for (name, path) in app.sidebar.known.clone() {
            nav_item(app, ui, "★", &name, &path, None);
        }

        if !app.settings.bookmarks.is_empty() {
            ui.add_space(6.0);
            section(ui, "Bookmarks");
            let mut remove = None;
            for (i, path) in app.settings.bookmarks.clone().iter().enumerate() {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                let r = nav_item(app, ui, "🔖", &name, path, None);
                r.context_menu(|ui| {
                    if ui.button("Remove bookmark").clicked() {
                        remove = Some(i);
                        ui.close();
                    }
                    if i > 0 && ui.button("Move up").clicked() {
                        app.settings.bookmarks.swap(i, i - 1);
                        ui.close();
                    }
                });
            }
            if let Some(i) = remove {
                app.settings.bookmarks.remove(i);
            }
        }

        ui.add_space(6.0);
        section(ui, "Drives");
        let drives = app.sidebar.drives.lock().clone();
        for d in drives {
            let label = if d.label.is_empty() {
                format!("{} ({})", d.kind, d.root.display())
            } else {
                format!("{} ({})", d.label, d.root.display().to_string().trim_end_matches('\\'))
            };
            let icon = match d.kind {
                "Removable" => "💾",
                "Network" => "🖧",
                "Optical" => "💿",
                _ => "🖴",
            };
            if !d.available {
                ui.label(RichText::new(format!("{icon}  {label}  (unavailable)")).color(theme::muted(ui)));
                continue;
            }
            nav_item(app, ui, icon, &label, &d.root, None);
            if d.total > 0 {
                let used = 1.0 - d.free as f32 / d.total as f32;
                let bar = egui::ProgressBar::new(used)
                    .desired_height(3.0)
                    .desired_width(ui.available_width() - 8.0)
                    .fill(if used > 0.9 { ui.visuals().error_fg_color } else { theme::ACCENT });
                ui.add(bar).on_hover_text(format!(
                    "{} free of {}",
                    forge_core::model::format_size(d.free),
                    forge_core::model::format_size(d.total)
                ));
            }
        }
    });
}

fn section(ui: &mut Ui, title: &str) {
    ui.label(RichText::new(title.to_uppercase()).small().strong().color(theme::muted(ui)));
}
