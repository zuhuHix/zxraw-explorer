//! Library search window: query the index with the same language as the folder filter,
//! plus metadata filters (camera:, iso:, rating:, fps:, …) that need the database.
//!
//! The selector at the top can instead send the same box to a name index: Everything
//! (`es.exe`) or the Windows Search index (see `forge_win::ext_search`). Those run on worker
//! threads and never need the library database.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use egui::{RichText, Sense};
use forge_core::model::format_size;
use forge_win::ext_search::{self, Backend, Hit};

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

/// Most hits an external name search returns to the window.
const EXTERNAL_LIMIT: usize = 2000;

/// Where the search box looks. Stored in settings as "index", "everything" or "windows".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Index,
    Everything,
    Windows,
}

impl Source {
    fn from_setting(s: &str) -> Self {
        match s {
            "everything" => Self::Everything,
            "windows" => Self::Windows,
            _ => Self::Index,
        }
    }

    fn setting(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Everything => "everything",
            Self::Windows => "windows",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Index => "Library index",
            Self::Everything => "Everything",
            Self::Windows => "Windows Search",
        }
    }

    fn backend(self) -> Option<Backend> {
        match self {
            Self::Index => None,
            Self::Everything => Some(Backend::Everything),
            Self::Windows => Some(Backend::WindowsSearch),
        }
    }
}

/// State of the external backends, shared with their worker threads.
#[derive(Default)]
struct ExtState {
    /// Whether the availability probes were started (they run once per window session).
    started: bool,
    /// `None` while a probe runs; `Some(Err(reason))` when the backend cannot be used.
    everything: Option<Result<(), String>>,
    windows: Option<Result<(), String>>,
    /// Limit the external search to the folder currently shown.
    scope_only: bool,
    /// Bumped by every search and by backend changes, so stale results are dropped.
    generation: u64,
    running: bool,
    /// The query whose results are shown.
    query: String,
    results: Vec<Hit>,
    error: Option<String>,
}

type Shared = Arc<Mutex<ExtState>>;

fn shared(ctx: &egui::Context) -> Shared {
    let id = egui::Id::new("search_ui_external_state");
    ctx.data_mut(|d| {
        if let Some(s) = d.get_temp::<Shared>(id) {
            return s;
        }
        let s: Shared = Arc::default();
        d.insert_temp(id, s.clone());
        s
    })
}

fn lock(s: &Shared) -> MutexGuard<'_, ExtState> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

/// Checks both external backends once, on a worker thread (Windows Search starts PowerShell).
fn start_probe(ext: &Shared, ctx: &egui::Context) {
    {
        let mut st = lock(ext);
        if st.started {
            return;
        }
        st.started = true;
    }
    let ext = ext.clone();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let every = ext_search::availability(Backend::Everything);
        lock(&ext).everything = Some(every);
        ctx.request_repaint();
        let win = ext_search::availability(Backend::WindowsSearch);
        lock(&ext).windows = Some(win);
        ctx.request_repaint();
    });
}

fn start_external(ctx: &egui::Context, ext: &Shared, backend: Backend, query: String, scope: Option<PathBuf>) {
    let generation = {
        let mut st = lock(ext);
        st.generation += 1;
        st.running = true;
        st.error = None;
        st.results.clear();
        st.query = query.clone();
        st.generation
    };
    let ext = ext.clone();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let result = ext_search::search(backend, &query, scope.as_deref(), EXTERNAL_LIMIT);
        {
            let mut st = lock(&ext);
            if st.generation == generation {
                st.running = false;
                match result {
                    Ok(hits) => st.results = hits,
                    Err(e) => st.error = Some(e),
                }
            }
        }
        ctx.request_repaint();
    });
}

/// The row used by both the index and the external results: name, folder, and a
/// right-aligned detail. Returns the response so the caller can handle clicks.
fn result_row(ui: &mut egui::Ui, row_h: f32, name: &str, folder: &str, right: &str, dim: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let p = ui.painter_at(rect);
    let cy = rect.center().y;
    let col = if dim { ui.visuals().weak_text_color() } else { ui.visuals().text_color() };
    p.text(egui::pos2(rect.left() + 6.0, cy), egui::Align2::LEFT_CENTER, name, egui::FontId::proportional(13.0), col);
    p.text(egui::pos2(rect.left() + 300.0, cy), egui::Align2::LEFT_CENTER, folder, egui::FontId::proportional(11.5), theme::muted(ui));
    p.text(egui::pos2(rect.right() - 8.0, cy), egui::Align2::RIGHT_CENTER, right, egui::FontId::proportional(11.5), theme::muted(ui));
    resp
}

/// The backend selector and, for external backends, the "Only under current folder" box.
/// A click on an available backend is written to `choose`.
fn backend_bar(ui: &mut egui::Ui, source: Source, ext: &Shared, choose: &mut Option<Source>) {
    let (every, win) = {
        let st = lock(ext);
        (st.everything.clone(), st.windows.clone())
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new("Search in:").weak());
        for src in [Source::Index, Source::Everything, Source::Windows] {
            let status: Option<Result<(), String>> = match src {
                Source::Index => Some(Ok(())),
                Source::Everything => every.clone(),
                Source::Windows => win.clone(),
            };
            let why = match &status {
                None => Some("Checking…".to_string()),
                Some(Err(e)) => Some(e.clone()),
                Some(Ok(())) => None,
            };
            let enabled = matches!(status, Some(Ok(())));
            let mut resp = ui.add_enabled(enabled, egui::Button::new(src.label()).selected(source == src));
            if let Some(why) = why {
                resp = resp.on_disabled_hover_text(why);
            }
            if resp.clicked() && source != src {
                *choose = Some(src);
            }
        }
        if source != Source::Index {
            let mut st = lock(ext);
            ui.checkbox(&mut st.scope_only, "Only under current folder");
        }
    });
}

pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    // Runs every frame until the library is open, then once: picks up index passes that an
    // earlier run left unfinished. It starts background jobs and never blocks.
    crate::library::resume_on_start(app, ctx);
    if !app.ops.library.search.open {
        return;
    }
    let ext = shared(ctx);
    start_probe(&ext, ctx);
    let source = Source::from_setting(&app.settings.search_backend);
    let current_dir = app.pane().path.clone();
    let mut open = true;
    let mut navigate: Option<PathBuf> = None;
    let mut index_now: Option<PathBuf> = None;
    let mut choose: Option<Source> = None;
    egui::Window::new("Search library")
        .open(&mut open)
        .default_size([760.0, 520.0])
        .resizable(true)
        .show(ctx, |ui| {
            backend_bar(ui, source, &ext, &mut choose);
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
                match source.backend() {
                    None => lib.run_search(),
                    Some(backend) => {
                        let query = lib.search.query.trim().to_string();
                        if !query.is_empty() {
                            let scope = lock(&ext).scope_only.then(|| current_dir.clone());
                            start_external(ui.ctx(), &ext, backend, query, scope);
                        }
                    }
                }
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
            if source != Source::Index {
                external_results(ui, &ext, &mut navigate);
                return;
            }
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
                        let mut right = format_size(r.size);
                        if r.rating > 0 {
                            right = format!("{}  {right}", "★".repeat(r.rating as usize));
                        }
                        if !r.present {
                            right = format!("offline  {right}");
                        }
                        let folder = r.parent_path.display().to_string();
                        let resp = result_row(ui, row_h, &r.name, &folder, &right, !r.present)
                            .on_hover_text(r.path.display().to_string());
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
    if let Some(src) = choose {
        // Results from the previous source no longer apply; in-flight searches are dropped.
        {
            let mut st = lock(&ext);
            st.generation += 1;
            st.running = false;
            st.results.clear();
            st.error = None;
            st.query.clear();
        }
        app.settings.search_backend = src.setting().to_string();
        app.save_all();
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

/// The result list of an external search: status line, then one row per path.
fn external_results(ui: &mut egui::Ui, ext: &Shared, navigate: &mut Option<PathBuf>) {
    let st = lock(ext);
    if st.running {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Searching…");
        });
    } else if let Some(e) = &st.error {
        ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
    } else if !st.query.is_empty() {
        ui.label(RichText::new(format!(
            "{} result(s){}",
            st.results.len(),
            if st.results.len() >= EXTERNAL_LIMIT { format!(" (showing first {EXTERNAL_LIMIT})") } else { String::new() }
        ))
        .weak());
    }
    let row_h = 22.0;
    egui::ScrollArea::vertical()
        .max_height(ui.available_height() - 20.0)
        .auto_shrink([false, false])
        .show_rows(ui, row_h, st.results.len(), |ui, range| {
            for hit in &st.results[range] {
                let name = hit
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| hit.path.display().to_string());
                let folder = hit.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                let right = if hit.is_dir { "folder" } else { "" };
                let resp = result_row(ui, row_h, &name, &folder, right, false)
                    .on_hover_text(hit.path.display().to_string());
                if resp.double_clicked() || resp.clicked() {
                    *navigate = Some(hit.path.clone());
                }
            }
        });
}
