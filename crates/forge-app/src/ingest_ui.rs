//! Professional media ingest window: background preview, verified transfer and a
//! conservative post-copy format check.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDateTime};
use egui::{Color32, RichText, Ui};
use forge_core::ingest::{
    self, CaptureInfo, FormatCheck, IngestMetadataProvider, IngestPlan, IngestReport, IngestSpec,
    IngestStatus,
};
use forge_core::model::{self, MediaKind, format_size};
use forge_core::ops::JobCtx;
use forge_jobs::{CancelToken, JobHandle, JobRegistry, Progress};
use serde::{Deserialize, Serialize};

use crate::app::ZxrawApp;

const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(450);
const PREFS_DEBOUNCE: Duration = Duration::from_millis(700);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
enum MediaFilter {
    Photos,
    Videos,
    #[default]
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct IngestPrefs {
    destination: String,
    folder_template: String,
    file_template: String,
    project: String,
    client: String,
    shoot: String,
    filter: MediaFilter,
}

impl Default for IngestPrefs {
    fn default() -> Self {
        Self {
            destination: String::new(),
            folder_template: "{YYYY}/{date}".into(),
            file_template: "{orig_name}".into(),
            project: String::new(),
            client: String::new(),
            shoot: String::new(),
            filter: MediaFilter::All,
        }
    }
}

struct PlanMessage {
    revision: u64,
    result: Result<IngestPlan, String>,
    free_space: Option<u64>,
}

struct CardMessage {
    sources: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IngestBinding {
    sources: Vec<PathBuf>,
    destination: PathBuf,
    folder_template: String,
    file_template: String,
    project: String,
    client: String,
    shoot: String,
    extensions: Option<Vec<String>>,
}

impl IngestBinding {
    fn from_spec(spec: &IngestSpec) -> Self {
        Self {
            sources: spec.sources.clone(),
            destination: spec.dest_root.clone(),
            folder_template: spec.folder_template.clone(),
            file_template: spec.file_template.clone(),
            project: spec.project.clone(),
            client: spec.client.clone(),
            shoot: spec.shoot.clone(),
            extensions: spec.extensions.clone(),
        }
    }
}

struct ImportCompletion {
    binding: IngestBinding,
    report: IngestReport,
    format_check: Option<FormatCheck>,
    format_error: Option<String>,
}

type ImportMessage = Result<ImportCompletion, String>;

/// State is owned by `Actions`; public shape is intentionally small for command wiring.
pub struct IngestState {
    pub open: bool,
    sources: Vec<PathBuf>,
    destination: String,
    folder_template: String,
    file_template: String,
    project: String,
    client: String,
    shoot: String,
    filter: MediaFilter,
    prefs_loaded: bool,
    prefs_save_due: Option<Instant>,
    revision: u64,
    planned_revision: Option<u64>,
    preview_due: Option<Instant>,
    preview_busy: bool,
    preview_cancel: Option<CancelToken>,
    plan: Option<IngestPlan>,
    plan_error: Option<String>,
    free_space: Option<u64>,
    free_space_checked: bool,
    plan_tx: Sender<PlanMessage>,
    plan_rx: Receiver<PlanMessage>,
    card_scan_tx: Sender<CardMessage>,
    card_scan_rx: Receiver<CardMessage>,
    card_scan_busy: bool,
    card_scan_error: Option<String>,
    import_running: bool,
    import_job: Option<JobHandle>,
    import_tx: Sender<ImportMessage>,
    import_rx: Receiver<ImportMessage>,
    report: Option<ImportCompletion>,
    import_error: Option<String>,
}

impl Default for IngestState {
    fn default() -> Self {
        let (plan_tx, plan_rx) = mpsc::channel();
        let (card_scan_tx, card_scan_rx) = mpsc::channel();
        let (import_tx, import_rx) = mpsc::channel();
        let prefs = IngestPrefs::default();
        Self {
            open: false,
            sources: Vec::new(),
            destination: prefs.destination,
            folder_template: prefs.folder_template,
            file_template: prefs.file_template,
            project: prefs.project,
            client: prefs.client,
            shoot: prefs.shoot,
            filter: prefs.filter,
            prefs_loaded: false,
            prefs_save_due: None,
            revision: 0,
            planned_revision: None,
            preview_due: None,
            preview_busy: false,
            preview_cancel: None,
            plan: None,
            plan_error: None,
            free_space: None,
            free_space_checked: false,
            plan_tx,
            plan_rx,
            card_scan_tx,
            card_scan_rx,
            card_scan_busy: false,
            card_scan_error: None,
            import_running: false,
            import_job: None,
            import_tx,
            import_rx,
            report: None,
            import_error: None,
        }
    }
}

impl IngestState {
    fn load_prefs(&mut self, config: &Path) {
        if self.prefs_loaded {
            return;
        }
        let prefs: IngestPrefs = crate::settings::load_json(&config.join("ingest.json"));
        self.destination = prefs.destination;
        self.folder_template = prefs.folder_template;
        self.file_template = prefs.file_template;
        self.project = prefs.project;
        self.client = prefs.client;
        self.shoot = prefs.shoot;
        self.filter = prefs.filter;
        self.prefs_loaded = true;
    }

    fn invalidate_preview(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.report = None;
        if let Some(cancel) = &self.preview_cancel {
            cancel.cancel();
        }
        self.preview_cancel = None;
        self.preview_due = Some(Instant::now() + PREVIEW_DEBOUNCE);
        self.preview_busy = false;
        self.plan_error = None;
        self.free_space_checked = false;
        self.prefs_save_due = Some(Instant::now() + PREFS_DEBOUNCE);
    }

    fn spec(&self) -> IngestSpec {
        let mut spec =
            IngestSpec::new(self.sources.clone(), PathBuf::from(self.destination.trim()));
        spec.folder_template = self.folder_template.clone();
        spec.file_template = self.file_template.clone();
        spec.project = self.project.clone();
        spec.client = self.client.clone();
        spec.shoot = self.shoot.clone();
        spec.extensions = filter_extensions(self.filter);
        // The engine always verifies each destination copy before it appears at its final
        // name. Keep a manifest so a later safety check can be audited.
        spec.write_manifest = true;
        spec
    }

    fn report_binding(&self) -> IngestBinding {
        IngestBinding {
            sources: self.sources.clone(),
            destination: PathBuf::from(self.destination.trim()),
            folder_template: self.folder_template.clone(),
            file_template: self.file_template.clone(),
            project: self.project.clone(),
            client: self.client.clone(),
            shoot: self.shoot.clone(),
            extensions: filter_extensions(self.filter),
        }
    }

    fn prefs(&self) -> IngestPrefs {
        IngestPrefs {
            destination: self.destination.clone(),
            folder_template: self.folder_template.clone(),
            file_template: self.file_template.clone(),
            project: self.project.clone(),
            client: self.client.clone(),
            shoot: self.shoot.clone(),
            filter: self.filter,
        }
    }

    fn accept_plan_message(&mut self, message: PlanMessage) {
        if message.revision != self.revision {
            return;
        }
        self.preview_busy = false;
        self.preview_cancel = None;
        self.preview_due = None;
        self.free_space = message.free_space;
        self.free_space_checked = true;
        self.planned_revision = Some(message.revision);
        match message.result {
            Ok(plan) => {
                self.plan_error = None;
                self.plan = Some(plan);
            }
            Err(error) => {
                self.plan_error = Some(error);
                self.plan = None;
            }
        }
    }

    fn poll(&mut self) {
        while let Ok(message) = self.card_scan_rx.try_recv() {
            self.card_scan_busy = false;
            self.sources = message.sources;
            self.card_scan_error = None;
            self.invalidate_preview();
        }
        while let Ok(message) = self.plan_rx.try_recv() {
            self.accept_plan_message(message);
        }
        while let Ok(message) = self.import_rx.try_recv() {
            self.import_running = false;
            self.import_job = None;
            match message {
                Ok(completion) if completion.binding == self.report_binding() => {
                    self.report = Some(completion);
                    self.import_error = None;
                }
                Ok(_) => self.report = None,
                Err(error) => self.import_error = Some(error),
            }
        }
        if self.import_running
            && self
                .import_job
                .as_ref()
                .is_some_and(|job| job.state().is_finished())
        {
            self.import_running = false;
            self.import_job = None;
            if self.report.is_none() {
                self.import_error =
                    Some("The ingest worker stopped before returning a report.".into());
            }
        }
    }

    fn start_card_detection(&mut self, root: &Path, ctx: &egui::Context) {
        if self.card_scan_busy {
            return;
        }
        self.card_scan_busy = true;
        self.card_scan_error = None;
        self.sources.clear();
        self.invalidate_preview();
        let root = root.to_path_buf();
        let tx = self.card_scan_tx.clone();
        let repaint = ctx.clone();
        let spawn = std::thread::Builder::new()
            .name("ingest-card-detect".into())
            .spawn(move || {
                let sources = detect_card_sources(&root);
                let _ = tx.send(CardMessage { sources });
                repaint.request_repaint();
            });
        if let Err(error) = spawn {
            self.card_scan_busy = false;
            self.card_scan_error = Some(format!("Could not scan the card: {error}"));
        }
    }

    fn advance(&mut self, config_file: &Path, ctx: &egui::Context, cached_drives: Vec<FreeDrive>) {
        self.poll();

        if self.prefs_save_due.is_some_and(|at| at <= Instant::now()) {
            self.prefs_save_due = None;
            let path = config_file.to_path_buf();
            let prefs = self.prefs();
            let _ = std::thread::Builder::new()
                .name("ingest-prefs".into())
                .spawn(move || {
                    if let Err(error) = crate::settings::save_json(&path, &prefs) {
                        tracing::warn!("saving ingest preferences failed: {error}");
                    }
                });
        }

        let due = self.preview_due;
        if !self.preview_busy && should_launch_plan(due, Instant::now()) {
            self.preview_due = None;
            if !self.sources.is_empty() && !self.destination.trim().is_empty() {
                let spec = self.spec();
                let revision = self.revision;
                let cancel = CancelToken::new();
                self.preview_cancel = Some(cancel.clone());
                self.preview_busy = true;
                let tx = self.plan_tx.clone();
                let repaint = ctx.clone();
                let spawn = std::thread::Builder::new()
                    .name("ingest-preview".into())
                    .spawn(move || {
                        let progress = Progress::new();
                        let job_ctx = JobCtx::new(&cancel, progress.as_ref());
                        let result = ingest::plan_ingest(&spec, &MediaMetaProvider, &job_ctx)
                            .map_err(|error| error.to_string());
                        let free_space = if cancel.is_cancelled() {
                            None
                        } else {
                            destination_free_space(
                                &spec.dest_root,
                                &forge_win::list_drives(),
                                &cached_drives,
                            )
                        };
                        let _ = tx.send(PlanMessage {
                            revision,
                            result,
                            free_space,
                        });
                        repaint.request_repaint();
                    });
                if let Err(error) = spawn {
                    self.preview_busy = false;
                    self.plan_error = Some(format!("Could not start preview worker: {error}"));
                    self.planned_revision = Some(revision);
                }
            }
        }

        if self.preview_busy || self.preview_due.is_some() || self.import_running {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        if self.prefs_save_due.is_some() {
            ctx.request_repaint_after(Duration::from_millis(PREFS_DEBOUNCE.as_millis() as u64));
        }
        if let Some(at) = self.preview_due {
            ctx.request_repaint_after(at.saturating_duration_since(Instant::now()));
        }
    }
}

/// Open the ingest window with `path` (a card root) as its source. Camera folders are picked the
/// same way as "Use this card": DCIM/PRIVATE if present, otherwise the root itself.
pub fn open_with_source(app: &mut ZxrawApp, path: &Path) {
    let ctx = app.ops.ctx.clone();
    let state = &mut app.ops.ingest;
    state.open = true;
    state.start_card_detection(path, &ctx);
}

/// Non-modal "card detected" prompts, one per card that has a DCIM/PRIVATE folder. Drive
/// detection and the camera-folder check run off the UI thread; see `sidebar::CardWatch`.
fn card_prompts(app: &mut ZxrawApp, ctx: &egui::Context) {
    let drives = app.sidebar.drives.lock().clone();
    app.sidebar.cards.tick(&drives, ctx);
    let prompts = app.sidebar.cards.prompts.clone();
    for (index, card) in prompts.iter().enumerate() {
        let drive = card.root.display().to_string();
        let drive = drive.trim_end_matches('\\');
        let heading = if card.label.is_empty() {
            format!("Card detected: {drive}")
        } else {
            format!("Card detected: {} ({drive})", card.label)
        };
        let mut import = false;
        let mut not_now = false;
        egui::Window::new(format!("card-prompt-{}", card.root.display()))
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .anchor(
                egui::Align2::RIGHT_BOTTOM,
                [-14.0, -44.0 - index as f32 * 88.0],
            )
            .show(ctx, |ui| {
                ui.label(RichText::new(heading).strong());
                ui.horizontal(|ui| {
                    import = ui.button("Import media…").clicked();
                    not_now = ui.button("Not now").clicked();
                });
            });
        if import {
            app.sidebar.cards.dismiss(&card.root);
            open_with_source(app, &card.root);
        } else if not_now {
            app.sidebar.cards.dismiss(&card.root);
        }
    }
}

/// Called each frame by the app shell. Planning, metadata reads, copying, verification and
/// deep format-safety checking all happen on worker threads.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    card_prompts(app, ctx);
    let config_file = app.paths.config.join("ingest.json");
    let cached_drives = app
        .sidebar
        .drives
        .lock()
        .iter()
        .map(|drive| FreeDrive {
            root: drive.root.clone(),
            free: drive.free,
            available: drive.available,
        })
        .collect();
    let removable_drives = app
        .sidebar
        .drives
        .lock()
        .iter()
        .filter(|drive| drive.kind == "Removable" && drive.available)
        .cloned()
        .collect::<Vec<_>>();
    let focused_folder = app.pane().path.clone();
    let jobs = app.ops.jobs.clone();
    let event_tx = app.ops.tx.clone();
    let state = &mut app.ops.ingest;
    state.load_prefs(&app.paths.config);
    state.advance(&config_file, ctx, cached_drives);

    if !state.open {
        return;
    }

    let mut open = state.open;
    let mut start_import = false;
    egui::Window::new("Media ingest")
        .open(&mut open)
        .default_size([1020.0, 780.0])
        .min_width(760.0)
        .resizable(true)
        .show(ctx, |ui| {
            start_import = show_contents(ui, state, &removable_drives, &focused_folder);
        });
    state.open = open;

    if start_import && let Some(plan) = state.plan.clone() {
        state.import_running = true;
        state.import_error = None;
        state.report = None;
        let job = start_import_job(
            &jobs,
            plan,
            state.import_tx.clone(),
            Some(event_tx),
            ctx.clone(),
        );
        state.import_job = Some(job);
    }
}

fn show_contents(
    ui: &mut Ui,
    state: &mut IngestState,
    removable_drives: &[crate::sidebar::Drive],
    focused_folder: &Path,
) -> bool {
    let mut changed = false;
    let mut start_import = false;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.heading("Media ingest");
                    ui.label(RichText::new("Copy camera media into your working library, verify every file, then review the card safety check.").weak());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("SOURCE IS NEVER MODIFIED").small().strong().color(crate::theme::muted(ui)));
                });
            });
            ui.add_space(10.0);

            section_title(ui, "1", "Source cards and folders");
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!state.card_scan_busy, egui::Button::new("Add folder…")).clicked()
                    && let Some(path) = crate::dialogs::pick_folder(rfd::FileDialog::new())
                {
                    changed |= add_source(&mut state.sources, path);
                }
                if ui.add_enabled(!state.card_scan_busy, egui::Button::new("Use focused folder")).clicked() {
                    changed |= add_source(&mut state.sources, focused_folder.to_path_buf());
                }
            });
            if removable_drives.is_empty() {
                ui.label(RichText::new("No removable card is currently available.").small().weak());
            } else {
                ui.label(RichText::new("Selecting a card checks for DCIM and PRIVATE, then falls back to the card root.").small().weak());
                for drive in removable_drives {
                    let title = if drive.label.is_empty() {
                        drive.root.display().to_string()
                    } else {
                        format!("{}  ·  {}", drive.label, drive.root.display())
                    };
                    let button = format!("Use this card  ·  {title}");
                    if ui.add_enabled(!state.card_scan_busy, egui::Button::new(button))
                        .on_hover_text("Select detected photo and video folders")
                        .clicked()
                    {
                        state.start_card_detection(&drive.root, ui.ctx());
                    }
                }
            }
            if state.card_scan_busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading card folders…");
                });
            }
            if let Some(error) = &state.card_scan_error {
                ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
            }

            if state.sources.is_empty() {
                ui.label(RichText::new("Choose a removable card or add a source folder to begin.").color(crate::theme::muted(ui)));
            } else {
                let mut remove = None;
                for (index, source) in state.sources.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(source.display().to_string()).monospace().small());
                        if ui.add_enabled(!state.card_scan_busy, egui::Button::new("Remove"))
                            .on_hover_text("Remove this source")
                            .clicked()
                        {
                            remove = Some(index);
                        }
                    });
                }
                if let Some(index) = remove {
                    state.sources.remove(index);
                    changed = true;
                }
            }

            ui.add_space(10.0);
            section_title(ui, "2", "Destination and naming");
            ui.horizontal(|ui| {
                ui.label(RichText::new("Destination folder").strong());
                let response = ui.add_sized(
                    [ui.available_width() - 118.0, 24.0],
                    egui::TextEdit::singleline(&mut state.destination)
                        .hint_text("Choose a destination folder"),
                );
                changed |= response.changed();
                if ui.button("Browse…").clicked()
                    && let Some(path) = crate::dialogs::pick_folder(rfd::FileDialog::new())
                {
                    state.destination = path.to_string_lossy().into_owned();
                    changed = true;
                }
            });

            egui::Grid::new("ingest-templates")
                .num_columns(2)
                .spacing([12.0, 7.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("Folder template").strong());
                    let response = ui.add_sized(
                        [ui.available_width(), 24.0],
                        egui::TextEdit::singleline(&mut state.folder_template)
                            .hint_text("{YYYY}/{date}"),
                    );
                    changed |= response.changed();
                    ui.end_row();

                    ui.label(RichText::new("Filename template").strong());
                    let response = ui.add_sized(
                        [ui.available_width(), 24.0],
                        egui::TextEdit::singleline(&mut state.file_template)
                            .hint_text("{orig_name}"),
                    );
                    changed |= response.changed();
                    ui.end_row();
                });
            let folder_valid = ingest::validate_template(&state.folder_template).is_ok();
            let file_valid = ingest::validate_template(&state.file_template).is_ok();
            if !folder_valid || !file_valid {
                ui.label(RichText::new("A template has an unknown or malformed token. Fix it to build a preview.").color(ui.visuals().error_fg_color));
            }
            ui.collapsing("Template token help", |ui| {
                ui.label("Date/time: {YYYY} {YY} {MM} {DD} {hh} {mm} {ss} {date} {time}");
                ui.label("Metadata: {camera} {project} {client} {shoot}");
                ui.label("Filename: {orig_name} {ext} {seq} {seq:N} (for example {seq:4})");
                ui.label("Use / in the folder template to create subfolders. If the filename template omits {ext}, the source extension is appended.");
                ui.label(RichText::new("The folder template controls output folders; the engine does not preserve the original card folder layout.").small().weak());
            });
            ui.horizontal(|ui| {
                ui.label("Project");
                changed |= ui.add_sized([150.0, 24.0], egui::TextEdit::singleline(&mut state.project)).changed();
                ui.label("Client");
                changed |= ui.add_sized([150.0, 24.0], egui::TextEdit::singleline(&mut state.client)).changed();
                ui.label("Shoot");
                changed |= ui.add_sized([150.0, 24.0], egui::TextEdit::singleline(&mut state.shoot)).changed();
            });

            ui.horizontal(|ui| {
                ui.label(RichText::new("File types").strong());
                egui::ComboBox::from_id_salt("ingest-file-types")
                    .selected_text(match state.filter {
                        MediaFilter::All => "All files",
                        MediaFilter::Photos => "Photos and RAW",
                        MediaFilter::Videos => "Videos",
                    })
                    .show_ui(ui, |ui| {
                        changed |= ui.selectable_value(&mut state.filter, MediaFilter::All, "All files").changed();
                        changed |= ui.selectable_value(&mut state.filter, MediaFilter::Photos, "Photos and RAW").changed();
                        changed |= ui.selectable_value(&mut state.filter, MediaFilter::Videos, "Videos").changed();
                    });
                let mut verify = true;
                ui.add_enabled(false, egui::Checkbox::new(&mut verify, "Verify every copied file (always on)"));
            });

            if changed {
                state.invalidate_preview();
            }
            ui.add_space(10.0);
            section_title(ui, "3", "Preview");
            show_preview(ui, state);

            ui.add_space(10.0);
            if let Some(report) = state
                .report
                .as_ref()
                .filter(|report| report.binding == state.report_binding())
            {
                show_report(ui, report);
            }
            if let Some(error) = &state.import_error {
                ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Ingest only copies media. It never formats or erases a card.").small().weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let current = state.planned_revision == Some(state.revision);
                    let enough_space = state
                        .plan
                        .as_ref()
                        .is_none_or(|plan| !space_is_insufficient(plan.bytes_to_copy, state.free_space));
                    let can_start = current
                        && state.plan.as_ref().is_some_and(|plan| !plan.items.is_empty() && plan.errors.is_empty())
                        && enough_space
                        && !state.card_scan_busy
                        && !state.import_running;
                    let label = if state.import_running {
                        "Import running…"
                    } else if current && !enough_space {
                        "Not enough space"
                    } else {
                        "Start verified import"
                    };
                    start_import = ui.add_enabled(can_start, egui::Button::new(RichText::new(label).strong())).clicked();
                });
            });
        });
    start_import
}

fn show_preview(ui: &mut Ui, state: &IngestState) {
    if state.sources.is_empty() {
        ui.label(RichText::new("Select a source to plan the ingest.").weak());
        return;
    }
    if state.destination.trim().is_empty() {
        ui.label(RichText::new("Choose a destination folder to plan the ingest.").weak());
        return;
    }
    if state.preview_due.is_some() || state.preview_busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(if state.preview_due.is_some() {
                "Waiting for edits to settle…"
            } else {
                "Scanning media and building destination preview…"
            });
        });
    }
    if let Some(error) = &state.plan_error {
        ui.label(
            RichText::new(format!("Preview failed: {error}")).color(ui.visuals().error_fg_color),
        );
    }
    let Some(plan) = &state.plan else {
        return;
    };
    let fresh = state.planned_revision == Some(state.revision);
    if !fresh {
        ui.label(
            RichText::new(
                "Showing the last completed preview; it will refresh after the current edits.",
            )
            .small()
            .weak(),
        );
    }

    let new = plan.count(IngestStatus::New);
    let renamed = plan.count(IngestStatus::Renamed);
    let present = plan.count(IngestStatus::AlreadyImported);
    let total_bytes: u64 = plan.items.iter().map(|item| item.size).sum();
    ui.horizontal_wrapped(|ui| {
        metric(ui, "New", new.to_string());
        metric(ui, "Renamed for conflicts", renamed.to_string());
        metric(ui, "Already imported", present.to_string());
        metric(ui, "Problems", plan.errors.len().to_string());
        metric(
            ui,
            "Selected media",
            format!("{} · {} files", format_size(total_bytes), plan.items.len()),
        );
        metric(ui, "To copy", format_size(plan.bytes_to_copy));
    });

    if state.free_space_checked {
        match state.free_space {
            Some(free) if free < plan.bytes_to_copy => {
                ui.label(RichText::new(format!(
                    "Not enough destination space: {} required, {} available. Choose another destination or free space before importing.",
                    format_size(plan.bytes_to_copy),
                    format_size(free)
                )).strong().color(ui.visuals().error_fg_color));
            }
            Some(free) => {
                ui.label(
                    RichText::new(format!(
                        "Destination free space: {} · {} needed",
                        format_size(free),
                        format_size(plan.bytes_to_copy)
                    ))
                    .small()
                    .weak(),
                );
            }
            None => {
                ui.label(RichText::new("Destination free space could not be queried. Review available space before importing.").small().color(Color32::from_rgb(220, 165, 60)));
            }
        }
    }
    if !plan.errors.is_empty() {
        ui.collapsing(format!("Planning problems ({})", plan.errors.len()), |ui| {
            for issue in &plan.errors {
                ui.label(
                    RichText::new(format!("{} · {}", issue.path.display(), issue.reason))
                        .small()
                        .color(ui.visuals().error_fg_color),
                );
            }
        });
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let widths = [
            ui.available_width() * 0.38,
            ui.available_width() * 0.46,
            ui.available_width() * 0.16,
        ];
        ui.add_sized(
            [widths[0], 24.0],
            egui::Label::new(RichText::new("SOURCE").small().strong()),
        );
        ui.add_sized(
            [widths[1], 24.0],
            egui::Label::new(RichText::new("DESTINATION").small().strong()),
        );
        ui.add_sized(
            [widths[2], 24.0],
            egui::Label::new(RichText::new("STATUS").small().strong()),
        );
    });
    egui::ScrollArea::vertical()
        .max_height(245.0)
        .auto_shrink([false, false])
        .show_rows(ui, 24.0, plan.items.len(), |ui, rows| {
            for index in rows {
                let item = &plan.items[index];
                let widths = [
                    ui.available_width() * 0.38,
                    ui.available_width() * 0.46,
                    ui.available_width() * 0.16,
                ];
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [widths[0], 24.0],
                        egui::Label::new(
                            RichText::new(item.source.display().to_string())
                                .small()
                                .monospace(),
                        )
                        .truncate(),
                    )
                    .on_hover_text(item.source.display().to_string());
                    ui.add_sized(
                        [widths[1], 24.0],
                        egui::Label::new(
                            RichText::new(item.dest.display().to_string())
                                .small()
                                .monospace(),
                        )
                        .truncate(),
                    )
                    .on_hover_text(item.dest.display().to_string());
                    let (label, color) = match item.status {
                        IngestStatus::New => ("New", ui.visuals().text_color()),
                        IngestStatus::Renamed => ("Renamed", Color32::from_rgb(225, 170, 65)),
                        IngestStatus::AlreadyImported => ("Present", crate::theme::muted(ui)),
                    };
                    ui.add_sized(
                        [widths[2], 24.0],
                        egui::Label::new(RichText::new(label).small().color(color)),
                    );
                });
            }
        });
}

fn show_report(ui: &mut Ui, completion: &ImportCompletion) {
    let report = &completion.report;
    ui.separator();
    ui.heading("Import report");
    ui.horizontal_wrapped(|ui| {
        metric(ui, "Verified", report.verified.len().to_string());
        metric(
            ui,
            "Already present",
            report.already_present.len().to_string(),
        );
        metric(ui, "Failed", report.failed.len().to_string());
        metric(ui, "Missing", report.missing.len().to_string());
        if report.cancelled {
            metric(ui, "Run", "Cancelled".into());
        }
    });
    if let Some(path) = &report.manifest {
        ui.label(
            RichText::new(format!("Manifest: {}", path.display()))
                .small()
                .monospace(),
        );
    } else {
        ui.label(RichText::new("No manifest was written.").small().weak());
    }

    match (&completion.format_check, &completion.format_error) {
        (Some(check), _) if check.safe => {
            ui.label(
                RichText::new(format!(
                    "All files verified — safe to format ({} files checked).",
                    check.checked
                ))
                .strong()
                .color(Color32::from_rgb(80, 190, 110)),
            );
        }
        (Some(check), _) => {
            ui.label(
                RichText::new(format!(
                    "Not safe to format: {} file(s) are not verified.",
                    check.unverified.len()
                ))
                .strong()
                .color(ui.visuals().error_fg_color),
            );
            egui::ScrollArea::vertical()
                .max_height(110.0)
                .show(ui, |ui| {
                    for issue in &check.unverified {
                        ui.label(
                            RichText::new(format!("{} · {}", issue.path.display(), issue.reason))
                                .small()
                                .color(ui.visuals().error_fg_color),
                        );
                    }
                });
        }
        (None, Some(error)) => {
            ui.label(RichText::new(format!("Safe-to-format check incomplete: {error}. Treat the card as not safe to format.")).strong().color(ui.visuals().error_fg_color));
        }
        (None, None) => {
            ui.label(
                RichText::new(
                    "Safe-to-format status is unavailable. Treat the card as not safe to format.",
                )
                .color(ui.visuals().error_fg_color),
            );
        }
    }

    if !report.failed.is_empty() || !report.missing.is_empty() {
        ui.collapsing("Transfer issues", |ui| {
            for issue in &report.failed {
                ui.label(
                    RichText::new(format!(
                        "Failed · {} · {}",
                        issue.path.display(),
                        issue.reason
                    ))
                    .small()
                    .color(ui.visuals().error_fg_color),
                );
            }
            for path in &report.missing {
                ui.label(
                    RichText::new(format!("Missing · {}", path.display()))
                        .small()
                        .color(ui.visuals().error_fg_color),
                );
            }
        });
    }
    ui.label(RichText::new("ZXraw Explorer never formats the card. Use the camera or operating system only after reviewing this result.").small().weak());
}

fn section_title(ui: &mut Ui, number: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(number).strong().color(crate::theme::ACCENT));
        ui.heading(title);
    });
}

fn metric(ui: &mut Ui, name: &str, value: String) {
    ui.group(|ui| {
        ui.label(RichText::new(name).small().weak());
        ui.label(RichText::new(value).strong());
    });
}

#[derive(Clone)]
struct FreeDrive {
    root: PathBuf,
    free: u64,
    available: bool,
}

fn destination_free_space(
    destination: &Path,
    windows: &[forge_win::DriveInfo],
    cached: &[FreeDrive],
) -> Option<u64> {
    let mut matches: Vec<(usize, u64)> = windows
        .iter()
        .filter(|drive| drive.available)
        .filter_map(|drive| {
            drive
                .free_bytes
                .filter(|_| path_is_within(destination, &drive.root))
                .map(|free| (drive.root.components().count(), free))
        })
        .collect();
    matches.extend(
        cached
            .iter()
            .filter(|drive| drive.available)
            .filter(|drive| path_is_within(destination, &drive.root))
            .map(|drive| (drive.root.components().count(), drive.free)),
    );
    matches
        .into_iter()
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, free)| free)
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        let path = path.to_string_lossy().replace('/', "\\").to_lowercase();
        let mut root = root.to_string_lossy().replace('/', "\\").to_lowercase();
        if !root.ends_with('\\') {
            root.push('\\');
        }
        path == root.trim_end_matches('\\') || path.starts_with(&root)
    } else {
        path.starts_with(root)
    }
}

fn should_launch_plan(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|deadline| deadline <= now)
}

fn space_is_insufficient(required: u64, free: Option<u64>) -> bool {
    free.is_some_and(|available| available < required)
}

fn filter_extensions(filter: MediaFilter) -> Option<Vec<String>> {
    match filter {
        MediaFilter::All => None,
        MediaFilter::Photos => Some(
            model::IMAGE_EXTS
                .iter()
                .chain(model::RAW_EXTS)
                .map(|extension| (*extension).to_string())
                .collect(),
        ),
        MediaFilter::Videos => Some(
            model::VIDEO_EXTS
                .iter()
                .map(|extension| (*extension).to_string())
                .collect(),
        ),
    }
}

fn add_source(sources: &mut Vec<PathBuf>, path: PathBuf) -> bool {
    if sources
        .iter()
        .any(|existing| forge_core::paths::paths_equal(existing, &path))
    {
        return false;
    }
    sources.push(path);
    true
}

fn detect_card_sources(root: &Path) -> Vec<PathBuf> {
    let mut detected = Vec::new();
    for name in ["DCIM", "PRIVATE"] {
        if let Some(path) = find_child_directory(root, name) {
            detected.push(path);
        }
    }
    if detected.is_empty() {
        detected.push(root.to_path_buf());
    }
    detected
}

pub(crate) fn find_child_directory(root: &Path, expected: &str) -> Option<PathBuf> {
    std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let name_matches = entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(expected);
            let is_dir = entry.file_type().is_ok_and(|file_type| file_type.is_dir());
            (name_matches && is_dir).then(|| entry.path())
        })
}

fn start_import_job(
    jobs: &JobRegistry,
    plan: IngestPlan,
    completion_tx: Sender<ImportMessage>,
    event_tx: Option<crossbeam_channel::Sender<crate::actions::Event>>,
    repaint: egui::Context,
) -> JobHandle {
    let dest_root = plan.spec.dest_root.clone();
    let title = format!(
        "Ingesting {} file(s) to {}",
        plan.items.len(),
        dest_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| dest_root.display().to_string())
    );
    jobs.spawn("transfer", title, move |job| {
        let result = execute_import(&plan, job);
        if let Ok(completion) = &result {
            for issue in &completion.report.failed {
                job.log(format!(
                    "failed: {}: {}",
                    issue.path.display(),
                    issue.reason
                ));
            }
            for path in &completion.report.missing {
                job.log(format!("missing source: {}", path.display()));
            }
            job.log(format!(
                "verified {}, already present {}, failed {}, missing {}",
                completion.report.verified.len(),
                completion.report.already_present.len(),
                completion.report.failed.len(),
                completion.report.missing.len()
            ));
            if let Some(path) = &completion.report.manifest {
                job.log(format!("manifest: {}", path.display()));
            }
            if let Some(check) = &completion.format_check {
                job.log(format!(
                    "safe to format: {}, checked {}, unverified {}",
                    check.safe,
                    check.checked,
                    check.unverified.len()
                ));
            } else if let Some(error) = &completion.format_error {
                job.log(format!("safe-to-format check incomplete: {error}"));
            }
        } else if let Err(error) = &result {
            job.log(format!("ingest failed: {error}"));
        }
        if let Some(tx) = event_tx {
            let _ = tx.send(crate::actions::Event::Changed(dest_root));
        }
        let _ = completion_tx.send(result);
        repaint.request_repaint();
        Ok(())
    })
}

fn execute_import(plan: &IngestPlan, job: &JobHandle) -> Result<ImportCompletion, String> {
    let ctx = JobCtx::from_job(job);
    let report = ingest::execute_ingest(plan, &ctx).map_err(|error| error.to_string())?;
    let format_check = ingest::safe_to_format(&plan.spec.sources, &[&report], true, &ctx);
    let (format_check, format_error) = match format_check {
        Ok(check) => (Some(check), None),
        Err(error) => (None, Some(error.to_string())),
    };
    Ok(ImportCompletion {
        binding: IngestBinding::from_spec(&plan.spec),
        report,
        format_check,
        format_error,
    })
}

/// EXIF provider matching the CLI: camera-local capture time when present; file modified
/// time remains the engine's fallback for video and unreadable metadata.
struct MediaMetaProvider;

impl IngestMetadataProvider for MediaMetaProvider {
    fn capture_info(&self, path: &Path) -> CaptureInfo {
        let extension = model::ext_of(path);
        if !matches!(
            model::media_kind_for_ext(&extension),
            MediaKind::Image | MediaKind::Raw
        ) {
            return CaptureInfo::default();
        }
        match forge_media::read_image_metadata(path) {
            Ok(metadata) => CaptureInfo {
                captured: captured_of(&metadata),
                camera: camera_of(&metadata),
            },
            Err(_) => CaptureInfo::default(),
        }
    }
}

fn captured_of(metadata: &forge_media::ImageMetadata) -> Option<NaiveDateTime> {
    if let Some(local) = metadata
        .date_taken
        .as_deref()
        .and_then(|value| value.get(..19))
        && let Ok(captured) = NaiveDateTime::parse_from_str(local, "%Y-%m-%dT%H:%M:%S")
    {
        return Some(captured);
    }
    metadata
        .capture_timestamp
        .and_then(|timestamp| DateTime::from_timestamp(timestamp, 0))
        .map(|datetime| datetime.naive_utc())
}

fn camera_of(metadata: &forge_media::ImageMetadata) -> Option<String> {
    let camera = metadata
        .model
        .as_deref()
        .or(metadata.make.as_deref())?
        .trim();
    (!camera.is_empty()).then(|| camera.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "zxraw-ingest-ui-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn detects_camera_photo_and_video_directories() {
        let root = temp_dir("card-detect");
        fs::create_dir_all(root.join("dcim/100canon")).unwrap();
        fs::create_dir_all(root.join("PRIVATE/M4ROOT/CLIP")).unwrap();

        let mut sources = detect_card_sources(&root);
        sources.sort();
        let mut expected = vec![root.join("dcim"), root.join("PRIVATE")];
        expected.sort();
        assert_eq!(sources, expected);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preview_debounce_waits_until_deadline() {
        let now = Instant::now();
        assert!(!should_launch_plan(
            Some(now + Duration::from_millis(1)),
            now
        ));
        assert!(should_launch_plan(Some(now), now));
        assert!(!should_launch_plan(None, now));

        let mut state = IngestState::default();
        state.invalidate_preview();
        assert!(state.preview_due.is_some());
        assert_eq!(state.planned_revision, None);
        assert!(!should_launch_plan(state.preview_due, Instant::now()));
    }

    #[test]
    fn ingest_report_is_cleared_when_sources_destination_or_templates_change() {
        fn completion(binding: IngestBinding) -> ImportCompletion {
            ImportCompletion {
                binding,
                report: IngestReport::default(),
                format_check: None,
                format_error: None,
            }
        }

        let mut state = IngestState::default();
        state.sources = vec![PathBuf::from("card-a")];
        state.destination = "library".into();
        let card_a_binding = state.report_binding();
        state.report = Some(completion(card_a_binding.clone()));
        state.sources = vec![PathBuf::from("card-b")];
        state.invalidate_preview();
        assert!(state.import_tx.send(Ok(completion(card_a_binding))).is_ok());
        state.poll();
        assert!(state.report.is_none());

        state.report = Some(completion(state.report_binding()));
        state.destination = "other-library".into();
        state.invalidate_preview();
        assert!(state.report.is_none());

        state.report = Some(completion(state.report_binding()));
        state.folder_template = "{YYYY}".into();
        state.invalidate_preview();
        assert!(state.report.is_none());

        state.report = Some(completion(state.report_binding()));
        state.file_template = "{orig_name}-{seq}".into();
        state.invalidate_preview();
        assert!(state.report.is_none());

        let old_template_values = state.report_binding();
        state.report = Some(completion(old_template_values.clone()));
        state.client = "new client".into();
        state.invalidate_preview();
        assert!(
            state
                .import_tx
                .send(Ok(completion(old_template_values)))
                .is_ok()
        );
        state.poll();
        assert!(state.report.is_none());
    }

    #[test]
    fn destination_space_uses_longest_matching_drive_and_blocks_shortfall() {
        let drives = [
            FreeDrive {
                root: PathBuf::from("/media"),
                free: 900,
                available: true,
            },
            FreeDrive {
                root: PathBuf::from("/media/card"),
                free: 250,
                available: true,
            },
        ];
        let free = destination_free_space(Path::new("/media/card/ingest"), &[], &drives);
        assert_eq!(free, Some(250));
        assert!(space_is_insufficient(251, free));
        assert!(!space_is_insufficient(250, free));
        assert!(!space_is_insufficient(500, None));
    }

    #[test]
    fn registered_ingest_job_copies_verifies_and_runs_format_check() {
        let root = temp_dir("job-e2e");
        let source = root.join("card");
        let destination = root.join("library");
        fs::create_dir_all(source.join("DCIM/100CAM")).unwrap();
        fs::write(source.join("DCIM/100CAM/IMG_0001.JPG"), b"verified media").unwrap();

        let mut spec = IngestSpec::new(vec![source.clone()], destination.clone());
        spec.folder_template = "{YYYY}".into();
        let cancel = CancelToken::new();
        let progress = Progress::new();
        let plan = ingest::plan_ingest(&spec, &MediaMetaProvider, &JobCtx::new(&cancel, &progress))
            .unwrap();
        let (completion_tx, completion_rx) = mpsc::channel();
        let jobs = JobRegistry::new();
        let repaint = egui::Context::default();
        let job = start_import_job(&jobs, plan, completion_tx, None, repaint);
        let completion = completion_rx
            .recv_timeout(Duration::from_secs(20))
            .unwrap()
            .unwrap();

        assert_eq!(completion.report.verified.len(), 1);
        assert!(completion.report.failed.is_empty());
        assert!(
            completion
                .format_check
                .as_ref()
                .is_some_and(|check| check.safe)
        );
        assert!(
            completion
                .report
                .manifest
                .as_ref()
                .is_some_and(|path| path.exists())
        );
        let copied = &completion.report.verified[0].dest;
        assert_eq!(fs::read(copied).unwrap(), b"verified media");
        // The job thread sets its final state just after sending the report; wait briefly.
        let t0 = std::time::Instant::now();
        while !job.state().is_finished() && t0.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(job.state().is_finished());
        fs::remove_dir_all(root).unwrap();
    }
}
