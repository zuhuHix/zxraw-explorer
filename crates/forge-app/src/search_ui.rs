//! Library search window: query the index with the same language as the folder filter,
//! plus metadata filters (camera:, iso:, rating:, fps:, …) that need the database.

use std::path::PathBuf;

use egui::{RichText, Sense};
use forge_core::model::format_size;

use crate::app::ZxrawApp;
use crate::theme;

const EXAMPLES: &[&str] = &[
    "ext:cr3,nef,arw rating:>=4",
    "camera:\"EOS R5\" iso:>=3200",
    "kind:video fps:>=50 width:>=3840",
    "label:red -is:offline",
    "modified:7d size:>100mb",
    "missing:gps kind:image",
];

pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    // Runs every frame until the library is open, then once: picks up index passes that an
    // earlier run left unfinished. It starts background jobs and never blocks.
    crate::library::resume_on_start(app, ctx);
    if !app.ops.library.search.open {
        return;
    }
    let mut open = true;
    let mut navigate: Option<PathBuf> = None;
    let mut index_now: Option<PathBuf> = None;
    egui::Window::new("Search library")
        .open(&mut open)
        .default_size([760.0, 520.0])
        .resizable(true)
        .show(ctx, |ui| {
            let lib = &mut app.ops.library;
            let mut go = false;
            let st = &mut lib.search;
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut st.query)
                        .hint_text("Search indexed files…  e.g. camera:\"EOS R5\" rating:>=4")
                        .desired_width(ui.available_width() - 90.0),
                );
                if st.focus {
                    r.request_focus();
                    st.focus = false;
                }
                go = ui.button("Search").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            });
            if go {
                lib.run_search();
            }
            let st = &mut lib.search;
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Try:").weak().small());
                for ex in EXAMPLES {
                    if ui.small_button(*ex).clicked() {
                        st.query = ex.to_string();
                    }
                }
            });
            ui.separator();
            if lib.db.is_none() {
                match &lib.open_error {
                    Some(e) => ui.label(RichText::new(format!("Library database unavailable: {e}")).color(ui.visuals().error_fg_color)),
                    None => ui.label("Opening library…"),
                };
                return;
            }
            let st = &lib.search;
            if st.running {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Searching…");
                });
            } else if let Some(e) = &st.error {
                ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
            } else if !st.last_query.is_empty() {
                ui.label(RichText::new(format!(
                    "{} result(s){}",
                    st.results.len(),
                    if st.results.len() >= 5000 { " (showing first 5,000)" } else { "" }
                )).weak());
            }

            let row_h = 22.0;
            let results = &st.results;
            egui::ScrollArea::vertical()
                .max_height(ui.available_height() - 110.0)
                .auto_shrink([false, false])
                .show_rows(ui, row_h, results.len(), |ui, range| {
                    for r in &results[range] {
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click());
                        if resp.hovered() {
                            ui.painter().rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
                        }
                        let p = ui.painter_at(rect);
                        let cy = rect.center().y;
                        let col = if r.present { ui.visuals().text_color() } else { ui.visuals().weak_text_color() };
                        p.text(egui::pos2(rect.left() + 6.0, cy), egui::Align2::LEFT_CENTER, &r.name, egui::FontId::proportional(13.0), col);
                        let folder = r.parent_path.display().to_string();
                        p.text(egui::pos2(rect.left() + 300.0, cy), egui::Align2::LEFT_CENTER, folder, egui::FontId::proportional(11.5), theme::muted(ui));
                        let mut right = format_size(r.size);
                        if r.rating > 0 {
                            right = format!("{}  {right}", "★".repeat(r.rating as usize));
                        }
                        if !r.present {
                            right = format!("offline  {right}");
                        }
                        p.text(egui::pos2(rect.right() - 8.0, cy), egui::Align2::RIGHT_CENTER, right, egui::FontId::proportional(11.5), theme::muted(ui));
                        let resp = resp.on_hover_text(r.path.display().to_string());
                        if resp.double_clicked() || resp.clicked() {
                            navigate = Some(r.path.clone());
                        }
                    }
                });

            ui.separator();
            ui.collapsing("Indexed folders", |ui| {
                let roots = lib.db.as_ref().and_then(|d| d.index_roots().ok()).unwrap_or_default();
                if roots.is_empty() {
                    ui.label(RichText::new("No folders indexed yet. Use “Index this folder for search” from the command palette.").weak());
                }
                for root in roots {
                    ui.horizontal(|ui| {
                        ui.label(root.path.display().to_string());
                        if ui.small_button("Re-index").clicked() {
                            index_now = Some(root.path.clone());
                        }
                    });
                }
            });
        });
    if !open {
        app.ops.library.search.open = false;
    }
    if let Some(p) = navigate {
        app.navigate(p);
    }
    if let Some(root) = index_now
        && let Some(db) = app.ops.library.db.clone()
    {
        let tx = app.ops.tx.clone();
        let c = ctx.clone();
        crate::library::start_index_job(&app.ops.jobs, db, root, move |m| {
            let _ = tx.send(crate::actions::Event::Toast(m));
            c.request_repaint();
        });
    }
}
