//! Application settings and external-tool integration.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{Color32, KeyboardShortcut, Modifiers, RichText};
use forge_db::{Db, ExclusionKind, Exclusions, IndexRoot};

use crate::app::ZxrawApp;
use crate::commands::{Cmd, set_binding, shortcut_text};
use crate::settings::{ExternalTool, Settings, ThemeChoice, ViewMode};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SettingsPage {
    #[default]
    Appearance,
    Browsing,
    Performance,
    FileOperations,
    Video,
    MediaLinks,
    Library,
    ExternalTools,
    DefaultApp,
    Updates,
    Privacy,
    Shortcuts,
}

impl SettingsPage {
    const ALL: [(Self, &'static str); 12] = [
        (Self::Appearance, "Appearance"),
        (Self::Browsing, "Browsing"),
        (Self::Performance, "Performance"),
        (Self::FileOperations, "File operations"),
        (Self::Video, "Video"),
        (Self::MediaLinks, "Media links"),
        (Self::Library, "Library"),
        (Self::ExternalTools, "External tools"),
        (Self::DefaultApp, "Default app"),
        (Self::Updates, "Updates"),
        (Self::Privacy, "Privacy"),
        (Self::Shortcuts, "Keyboard shortcuts"),
    ];
}

#[derive(Default)]
enum ProbeStatus {
    #[default]
    Unknown,
    Checking,
    Found(PathBuf),
    Missing,
}

struct ToolDraft {
    editing: Option<usize>,
    name: String,
    exe: PathBuf,
    args: String,
    extensions: String,
    error: Option<String>,
}

impl ToolDraft {
    fn new(editing: Option<usize>, tool: ExternalTool) -> Self {
        Self {
            editing,
            name: tool.name,
            exe: tool.exe,
            args: tool.args.join("\n"),
            extensions: tool.extensions.join(", "),
            error: None,
        }
    }

    fn build_tool(&self) -> Result<ExternalTool, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Enter a name for this application.".into());
        }
        if self.exe.as_os_str().is_empty() {
            return Err("Choose the application's executable.".into());
        }
        let args = self
            .args
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect();
        let extensions = parse_extensions(&self.extensions);
        Ok(ExternalTool {
            name: name.to_owned(),
            exe: self.exe.clone(),
            args,
            extensions,
        })
    }
}

struct LibrarySnapshot {
    roots: Vec<IndexRoot>,
    exclusions: Exclusions,
}

enum SettingsMessage {
    CacheUsage {
        generation: u64,
        result: Result<u64, String>,
    },
    CacheCleared {
        generation: u64,
        result: Result<u64, String>,
    },
    Probe {
        generation: u64,
        path: Option<PathBuf>,
    },
    Detected(Vec<ExternalTool>),
    LibrarySnapshot {
        generation: u64,
        result: Result<LibrarySnapshot, String>,
    },
    Integrity(Result<Vec<String>, String>),
}

#[derive(Clone)]
enum SettingsIntent {
    PickFfmpegFolder,
    UseFfmpegPath,
    DownloadFfmpeg,
    PickLibraryRoot,
    RemoveLibraryRoot(i64),
    AddExclusion(ExclusionKind, String),
    RemoveExclusion(ExclusionKind, String),
    Reindex(PathBuf),
    CheckIntegrity,
    DetectApps,
    BrowseToolExecutable,
    DefaultApp(crate::default_app::Action),
    CheckForUpdates,
    ShowUpdate,
    InstallUpdate,
}

struct ShortcutConflict {
    cmd: Cmd,
    text: String,
    other: Cmd,
}

pub struct SettingsUi {
    pub open: bool,
    /// Which apps open photos; shared with the first-run welcome.
    pub default_app: crate::default_app::State,
    page: SettingsPage,
    was_open: bool,
    picker_open: bool,
    picker_targets: Vec<PathBuf>,
    picker_dir: PathBuf,
    clear_cache_confirm: bool,
    cache_usage: Option<u64>,
    cache_error: Option<String>,
    cache_loading: bool,
    cache_generation: u64,
    probe_status: ProbeStatus,
    probe_generation: u64,
    detecting_apps: bool,
    detected_apps: Option<Vec<ExternalTool>>,
    tool_draft: Option<ToolDraft>,
    roots: Vec<IndexRoot>,
    exclusions: Exclusions,
    library_loading: bool,
    library_loaded: bool,
    library_error: Option<String>,
    library_generation: u64,
    exclusion_kind: ExclusionKind,
    exclusion_value: String,
    integrity_checking: bool,
    integrity_result: Option<Result<Vec<String>, String>>,
    /// Command whose shortcut is being recorded ("Change" was clicked). While set, the
    /// app's own key handling is suspended so the key press cannot run a command.
    pub capture: Option<Cmd>,
    /// A recorded shortcut that another command already uses, awaiting the user's choice.
    shortcut_conflict: Option<ShortcutConflict>,
    shortcut_filter: String,
    tx: Sender<SettingsMessage>,
    rx: Receiver<SettingsMessage>,
}

impl Default for SettingsUi {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self {
            open: false,
            default_app: Default::default(),
            page: SettingsPage::Appearance,
            was_open: false,
            picker_open: false,
            picker_targets: Vec::new(),
            picker_dir: PathBuf::new(),
            clear_cache_confirm: false,
            cache_usage: None,
            cache_error: None,
            cache_loading: false,
            cache_generation: 0,
            probe_status: ProbeStatus::Unknown,
            probe_generation: 0,
            detecting_apps: false,
            detected_apps: None,
            tool_draft: None,
            roots: Vec::new(),
            exclusions: Exclusions::default(),
            library_loading: false,
            library_loaded: false,
            library_error: None,
            library_generation: 0,
            exclusion_kind: ExclusionKind::Folder,
            exclusion_value: String::new(),
            integrity_checking: false,
            integrity_result: None,
            capture: None,
            shortcut_conflict: None,
            shortcut_filter: String::new(),
            tx,
            rx,
        }
    }
}

/// Drawn every frame by the main app. Background work reports through the local channel;
/// user-visible failures are also forwarded into the app's normal event/toast queue.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    // The library writer thread reads this flag when annotations change; keep it current.
    app.ops.library.set_xmp_sync(app.settings.xmp_sidecar_sync);
    drain_messages(app, ctx);
    start_open_tasks(app, ctx);

    if app.ops.settings_ui.open {
        show_settings_window(app, ctx);
    }
    if app.ops.settings_ui.picker_open {
        show_open_in_picker(app, ctx);
    }
    if app.ops.settings_ui.clear_cache_confirm {
        show_cache_confirmation(app, ctx);
    }
}

fn start_open_tasks(app: &mut ZxrawApp, ctx: &egui::Context) {
    let newly_opened = {
        let state = &mut app.ops.settings_ui;
        if !state.open {
            state.was_open = false;
            false
        } else if state.was_open {
            false
        } else {
            state.was_open = true;
            true
        }
    };

    if newly_opened {
        start_cache_scan(
            &mut app.ops.settings_ui,
            app.paths.cache.join("thumbs"),
            ctx,
        );
        start_probe_check(
            &mut app.ops.settings_ui,
            app.settings.ffmpeg_dir.clone(),
            ctx,
        );
    }

    if app.ops.settings_ui.open
        && !app.ops.settings_ui.library_loaded
        && !app.ops.settings_ui.library_loading
        && let Some(db) = app.ops.library.db.clone()
    {
        refresh_library(&mut app.ops.settings_ui, db, ctx);
    }
}

fn drain_messages(app: &mut ZxrawApp, ctx: &egui::Context) {
    while let Ok(message) = app.ops.settings_ui.rx.try_recv() {
        let state = &mut app.ops.settings_ui;
        match message {
            SettingsMessage::CacheUsage { generation, result }
                if generation == state.cache_generation =>
            {
                state.cache_loading = false;
                match result {
                    Ok(bytes) => {
                        state.cache_usage = Some(bytes);
                        state.cache_error = None;
                    }
                    Err(error) => state.cache_error = Some(error),
                }
            }
            SettingsMessage::CacheCleared { generation, result }
                if generation == state.cache_generation =>
            {
                state.cache_loading = false;
                match result {
                    Ok(bytes) => {
                        state.cache_usage = Some(bytes);
                        state.cache_error = None;
                        app.ops.thumbs.service().clear_memory();
                        let _ = app.ops.tx.send(crate::actions::Event::Toast(
                            "Thumbnail cache cleared".into(),
                        ));
                    }
                    Err(error) => {
                        state.cache_error = Some(error.clone());
                        let _ = app.ops.tx.send(crate::actions::Event::Error(format!(
                            "Could not clear thumbnail cache: {error}"
                        )));
                    }
                }
            }
            SettingsMessage::Probe { generation, path } if generation == state.probe_generation => {
                state.probe_status = path.map_or(ProbeStatus::Missing, ProbeStatus::Found);
            }
            SettingsMessage::Detected(tools) => {
                state.detecting_apps = false;
                state.detected_apps = Some(tools);
            }
            SettingsMessage::LibrarySnapshot { generation, result }
                if generation == state.library_generation =>
            {
                state.library_loading = false;
                state.library_loaded = true;
                match result {
                    Ok(snapshot) => {
                        state.roots = snapshot.roots;
                        state.exclusions = snapshot.exclusions;
                        state.library_error = None;
                    }
                    Err(error) => {
                        state.library_error = Some(error.clone());
                        let _ = app.ops.tx.send(crate::actions::Event::Error(format!(
                            "Could not read library settings: {error}"
                        )));
                    }
                }
            }
            SettingsMessage::Integrity(result) => {
                state.integrity_checking = false;
                if let Err(error) = &result {
                    let _ = app.ops.tx.send(crate::actions::Event::Error(format!(
                        "Database integrity check failed: {error}"
                    )));
                }
                state.integrity_result = Some(result);
            }
            _ => {}
        }
        ctx.request_repaint();
    }
}

fn show_settings_window(app: &mut ZxrawApp, ctx: &egui::Context) {
    let old_hidden = app.settings.show_hidden;
    let old_system = app.settings.show_system;
    let mut intents = Vec::new();
    let db_ready = app.ops.library.db.is_some();
    let db_open_error = app.ops.library.open_error.clone();
    let db_path = app
        .ops
        .library
        .db
        .as_ref()
        .and_then(|db| db.path())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| app.paths.data.join("forge.db"));
    let ffmpeg_download = app.ops.ffmpeg_setup.status();
    let updates = UpdatesView {
        checking: app.ops.updater.checking(),
        last_result: app.ops.updater.last_result.clone(),
        available: app
            .ops
            .updater
            .available
            .as_ref()
            .map(|u| u.version.clone()),
        downloading: app.ops.updater.download_status().running,
        ready: app.ops.updater.ready.is_some(),
        busy: app.ops.jobs.running_count() > 0,
    };

    {
        let (settings, state) = (&mut app.settings, &mut app.ops.settings_ui);
        let mut open = state.open;
        egui::Window::new("Settings")
            .open(&mut open)
            .default_size([850.0, 690.0])
            .min_size([710.0, 470.0])
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_min_width(154.0);
                        ui.add_space(4.0);
                        for (page, label) in SettingsPage::ALL {
                            if ui.selectable_label(state.page == page, label).clicked() {
                                state.page = page;
                            }
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("settings-content")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                match state.page {
                                    SettingsPage::Appearance => appearance_page(ui, settings),
                                    SettingsPage::Browsing => browsing_page(ui, settings),
                                    SettingsPage::Performance => {
                                        performance_page(ui, settings, state)
                                    }
                                    SettingsPage::FileOperations => {
                                        file_operations_page(ui, settings)
                                    }
                                    SettingsPage::Video => {
                                        video_page(ui, settings, state, ffmpeg_download, &mut intents)
                                    }
                                    SettingsPage::MediaLinks => media_links_page(ui, settings),
                                    SettingsPage::Library => library_page(
                                        ui,
                                        state,
                                        db_ready,
                                        db_open_error.as_deref(),
                                        &db_path,
                                        &mut intents,
                                    ),
                                    SettingsPage::ExternalTools => {
                                        external_tools_page(ui, settings, state, &mut intents)
                                    }
                                    SettingsPage::DefaultApp => {
                                        default_app_page(ui, state, &mut intents)
                                    }
                                    SettingsPage::Updates => {
                                        updates_page(ui, settings, &updates, &mut intents)
                                    }
                                    SettingsPage::Privacy => privacy_page(ui),
                                    SettingsPage::Shortcuts => shortcuts_page(ui, settings, state),
                                }
                            });
                        });
                });
            });
        state.open = open;
        if !state.open {
            state.capture = None;
        }
    }

    if old_hidden != app.settings.show_hidden || old_system != app.settings.show_system {
        for tab in &mut app.tabs {
            for pane in std::iter::once(&mut tab.left).chain(tab.right.iter_mut()) {
                pane.show_hidden = app.settings.show_hidden;
                pane.show_system = app.settings.show_system;
                pane.rebuild_visible();
            }
        }
    }

    handle_intents(app, ctx, intents);
}

fn appearance_page(ui: &mut egui::Ui, settings: &mut Settings) {
    page_heading(
        ui,
        "Appearance",
        "Adjust how ZXraw Explorer looks on this computer.",
    );
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label("Theme");
        egui::ComboBox::from_id_salt("settings-theme")
            .selected_text(theme_label(*settings.theme))
            .show_ui(ui, |ui| {
                settings.theme.edit(|theme| {
                    ui.selectable_value(theme, ThemeChoice::Dark, "Dark");
                    ui.selectable_value(theme, ThemeChoice::Light, "Light");
                    ui.selectable_value(theme, ThemeChoice::System, "System");
                });
            });
        ui.add_space(18.0);
        ui.label("Accent")
            .on_hover_text("Used for selections, highlights and the app icon.");
        for (accent, name) in crate::theme::ACCENTS {
            if accent_swatch(ui, accent, name, *settings.accent == accent).clicked() {
                settings.accent.set(accent);
            }
        }
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label("UI scale");
        ui.add(
            egui::Slider::new(&mut settings.ui_scale, 0.75..=1.5)
                .step_by(0.05)
                .suffix("×"),
        );
    });
    ui.label(
        RichText::new("Changes take effect immediately.")
            .weak()
            .small(),
    );
    ui.add_space(14.0);
    ui.label("App icon");
    ui.horizontal_wrapped(|ui| {
        for (choice, label) in crate::app_icon::ALL {
            let preview = crate::app_icon::preview(ui.ctx(), choice);
            let image = egui::Image::new(&preview).fit_to_exact_size(egui::vec2(40.0, 40.0));
            let response = ui
                .add(egui::Button::image(image).selected(*settings.app_icon == choice))
                .on_hover_text(label);
            if response.clicked() {
                settings.app_icon.set(choice);
            }
        }
    });
    ui.label(
        RichText::new(
            "ℹ The window and its taskbar button change right away. Windows caches pinned \
             taskbar and Start menu icons, so those may keep the old icon until you sign out \
             or restart your PC.",
        )
        .small(),
    );
}

/// A round, clickable colour swatch; the chosen one is ringed.
fn accent_swatch(
    ui: &mut egui::Ui,
    accent: crate::settings::Accent,
    name: &str,
    selected: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, selected, name)
    });
    let painter = ui.painter();
    let center = rect.center();
    let ring = if selected {
        Some(egui::Stroke::new(2.0, ui.visuals().strong_text_color()))
    } else if response.hovered() {
        Some(egui::Stroke::new(1.0, ui.visuals().weak_text_color()))
    } else {
        None
    };
    if let Some(ring) = ring {
        painter.circle_stroke(center, 11.0, ring);
    }
    painter.circle_filled(center, 8.0, crate::theme::accent_base(accent));
    if response.has_focus() {
        painter.circle_stroke(
            center,
            12.0,
            egui::Stroke::new(1.0, crate::theme::focus_ring(ui.ctx())),
        );
    }
    response.on_hover_text(name)
}

fn browsing_page(ui: &mut egui::Ui, settings: &mut Settings) {
    page_heading(
        ui,
        "Browsing",
        "Choose which files appear and how new folders are shown.",
    );
    ui.add_space(10.0);
    ui.checkbox(&mut settings.show_hidden, "Show hidden files");
    ui.checkbox(&mut settings.show_system, "Show system files");
    ui.checkbox(
        &mut settings.restore_session,
        "Reopen the last session's tabs at startup",
    );
    ui.checkbox(
        &mut settings.confirm_recycle,
        "Confirm before moving items to the Recycle Bin",
    );
    ui.checkbox(
        &mut settings.xmp_sidecar_sync,
        "Write ratings, labels and keywords to XMP sidecar files",
    );
    ui.label(
        RichText::new(
            "Write ratings, labels and keywords to .xmp sidecar files next to your photos so \
             Lightroom, Capture One and darktable can read them. Originals are never modified.",
        )
        .weak(),
    );
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label("Default view for new tabs");
        egui::ComboBox::from_id_salt("settings-default-view")
            .selected_text(match *settings.default_view {
                ViewMode::Details => "Details",
                ViewMode::Grid => "Thumbnail grid",
            })
            .show_ui(ui, |ui| {
                settings.default_view.edit(|view| {
                    ui.selectable_value(view, ViewMode::Details, "Details");
                    ui.selectable_value(view, ViewMode::Grid, "Thumbnail grid");
                });
            });
    });
    ui.horizontal(|ui| {
        use crate::settings::ListTyping;
        ui.label("Typing in the file list");
        egui::ComboBox::from_id_salt("settings-list-typing")
            .selected_text(match *settings.list_typing {
                ListTyping::Jump => "Jump to name",
                ListTyping::Filter => "Filter the list",
            })
            .show_ui(ui, |ui| {
                settings.list_typing.edit(|typing| {
                    ui.selectable_value(typing, ListTyping::Jump, "Jump to name");
                    ui.selectable_value(typing, ListTyping::Filter, "Filter the list");
                });
            });
    });
    ui.label(
        RichText::new(
            "Filter the list: typed letters go into the filter box so only matching names              stay. Backspace removes a letter, Esc clears the filter.",
        )
        .weak(),
    );
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label("Terminal for \"Open terminal here\"");
        egui::ComboBox::from_id_salt("settings-terminal")
            .selected_text(settings.terminal.label())
            .show_ui(ui, |ui| {
                settings.terminal.edit(|terminal| {
                    for choice in crate::terminal::TerminalChoice::ALL {
                        ui.selectable_value(terminal, choice, choice.label());
                    }
                });
            });
    });
    ui.checkbox(&mut settings.git_badges, "Git status badges and branch");
    ui.label(
        RichText::new(
            "Show Git status (modified, new, ignored, conflicted) and the branch in Git folders.",
        )
        .weak(),
    );
}

fn performance_page(ui: &mut egui::Ui, settings: &mut Settings, state: &mut SettingsUi) {
    page_heading(
        ui,
        "Performance",
        "Set the memory and disk budgets used for generated thumbnails.",
    );
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label("Thumbnail memory cache");
        ui.add(egui::Slider::new(&mut settings.thumb_mem_cache_mb, 32..=8192).suffix(" MB"));
    });
    ui.horizontal(|ui| {
        ui.label("Thumbnail disk cache limit");
        ui.add(egui::Slider::new(&mut settings.thumb_disk_cache_mb, 0..=65536).suffix(" MB"));
    });
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        ui.label("Current disk usage");
        if state.cache_loading {
            ui.spinner();
            ui.label("Scanning thumbnail cache…");
        } else if let Some(error) = &state.cache_error {
            ui.colored_label(ui.visuals().error_fg_color, format!("Unavailable: {error}"));
        } else if let Some(bytes) = state.cache_usage {
            ui.label(forge_core::model::format_size(bytes));
        } else {
            ui.label("Not measured yet");
        }
    });
    if ui.button("Clear thumbnail cache…").clicked() {
        state.clear_cache_confirm = true;
    }
    ui.label(
        RichText::new("Cached previews are regenerated as you browse.")
            .weak()
            .small(),
    );
}

fn file_operations_page(ui: &mut egui::Ui, settings: &mut Settings) {
    page_heading(
        ui,
        "File operations",
        "Choose how copies are checked and whether they run one at a time.",
    );
    ui.add_space(10.0);
    ui.checkbox(
        &mut settings.verify_copies,
        "Verify copies with a BLAKE3 checksum",
    );
    ui.add_space(12.0);
    ui.label("Run copies and moves:");
    ui.horizontal(|ui| {
        ui.radio_value(&mut settings.transfer_queue, true, "One at a time (queue)");
        ui.radio_value(&mut settings.transfer_queue, false, "All at once");
    });
    ui.label(
        RichText::new(
            "In queue mode, transfers wait their turn in the Operations panel. \
             Transfers between different physical drives can still run at the same time.",
        )
        .weak()
        .small(),
    );
}

fn video_page(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    state: &SettingsUi,
    download: crate::ffmpeg_setup::Status,
    intents: &mut Vec<SettingsIntent>,
) {
    page_heading(
        ui,
        "Video",
        "ZXraw Explorer uses external FFmpeg tools for video metadata and previews.",
    );
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label("FFmpeg folder");
        ui.label(
            RichText::new(
                settings
                    .ffmpeg_dir
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "Search PATH".into()),
            )
            .weak(),
        );
    });
    ui.horizontal(|ui| {
        if ui.button("Choose folder…").clicked() {
            intents.push(SettingsIntent::PickFfmpegFolder);
        }
        if settings.ffmpeg_dir.is_some() && ui.button("Use PATH").clicked() {
            intents.push(SettingsIntent::UseFfmpegPath);
        }
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label("ffprobe status");
        match &state.probe_status {
            ProbeStatus::Unknown | ProbeStatus::Checking => {
                ui.spinner();
                ui.label("Checking…");
            }
            ProbeStatus::Found(path) => {
                ui.colored_label(Color32::from_rgb(99, 190, 127), "ffprobe found:");
                ui.label(path.display().to_string());
            }
            ProbeStatus::Missing => {
                ui.colored_label(ui.visuals().error_fg_color, "ffprobe not found");
            }
        }
    });
    if download.running || matches!(state.probe_status, ProbeStatus::Missing) {
        ui.add_space(8.0);
        crate::ffmpeg_setup::offer(ui, download, || {
            intents.push(SettingsIntent::DownloadFfmpeg)
        });
    }
}

pub fn apply_export_rule(
    settings: &mut Settings,
    index: usize,
    rule: String,
) -> Result<(), String> {
    crate::export_assoc::validate_rule(&rule)?;
    let Some(existing) = settings.export_rules.get_mut(index) else {
        return Err("Rule row no longer exists".into());
    };
    *existing = rule;
    Ok(())
}

pub fn open_media_links(app: &mut ZxrawApp) {
    app.ops.settings_ui.open = true;
    app.ops.settings_ui.page = SettingsPage::MediaLinks;
}

fn media_links_page(ui: &mut egui::Ui, settings: &mut Settings) {
    page_heading(
        ui,
        "Media links",
        "Set the map link template and filename rules for related exports.",
    );
    ui.add_space(10.0);
    ui.label(RichText::new("Map URL template").strong());
    ui.add(
        egui::TextEdit::singleline(&mut settings.map_url_template)
            .desired_width(ui.available_width()),
    );
    ui.label(
        RichText::new("Use {lat} and {lon}; the URL opens only when you choose Open in maps.")
            .small()
            .weak(),
    );
    if !settings.map_url_template.contains("{lat}") || !settings.map_url_template.contains("{lon}")
    {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "The map URL should contain both {lat} and {lon}.",
        );
    }

    ui.add_space(16.0);
    ui.label(RichText::new("Export filename rules").strong());
    ui.label(
        RichText::new("{base} is the original name without its extension. {n} matches digits.")
            .small()
            .weak(),
    );
    let mut remove = None;
    for index in 0..settings
        .export_rules
        .len()
        .min(crate::export_assoc::MAX_EXPORT_RULES)
    {
        ui.push_id(("export-rule", index), |ui| {
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut settings.export_rules[index])
                        .desired_width((ui.available_width() - 80.0).max(180.0)),
                );
                if response.changed() {
                    let edited = settings.export_rules[index].clone();
                    let _ = apply_export_rule(settings, index, edited);
                }
                if ui.small_button("Remove").clicked() {
                    remove = Some(index);
                }
            });
            if let Err(error) = crate::export_assoc::validate_rule(&settings.export_rules[index]) {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
        });
    }
    if let Some(index) = remove {
        settings.export_rules.remove(index);
    }
    if settings.export_rules.len() > crate::export_assoc::MAX_EXPORT_RULES {
        ui.label(
            RichText::new(format!(
                "Only the first {} rules are used.",
                crate::export_assoc::MAX_EXPORT_RULES
            ))
            .weak(),
        );
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                settings.export_rules.len() < crate::export_assoc::MAX_EXPORT_RULES,
                egui::Button::new("Add rule"),
            )
            .clicked()
        {
            settings.export_rules.push("{base}-Edit".into());
        }
        if ui.button("Reset to defaults").clicked() {
            settings.export_rules = crate::export_assoc::default_rules();
        }
    });
}

fn library_page(
    ui: &mut egui::Ui,
    state: &mut SettingsUi,
    db_ready: bool,
    db_open_error: Option<&str>,
    db_path: &Path,
    intents: &mut Vec<SettingsIntent>,
) {
    page_heading(
        ui,
        "Library",
        "Manage folders included in search and the paths skipped during indexing.",
    );
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label("Database");
        ui.label(RichText::new(db_path.display().to_string()).weak());
    });

    if !db_ready {
        if let Some(error) = db_open_error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Database unavailable: {error}"),
            );
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Opening database…");
            });
        }
        return;
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.heading("Indexed folders");
        if ui
            .add_enabled(!state.library_loading, egui::Button::new("Add folder…"))
            .clicked()
        {
            intents.push(SettingsIntent::PickLibraryRoot);
        }
        if state.library_loading {
            ui.spinner();
        }
    });
    if let Some(error) = &state.library_error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    } else if state.roots.is_empty() && !state.library_loading {
        ui.label(RichText::new("No folders are indexed yet.").weak());
    }
    for root in &state.roots {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(root.path.display().to_string());
                    ui.label(
                        RichText::new(if root.last_complete_at.is_some() {
                            "Indexed before"
                        } else {
                            "Not indexed yet"
                        })
                        .weak()
                        .small(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(!state.library_loading, egui::Button::new("Remove"))
                        .clicked()
                    {
                        intents.push(SettingsIntent::RemoveLibraryRoot(root.id));
                    }
                    if ui
                        .add_enabled(!state.library_loading, egui::Button::new("Re-index"))
                        .clicked()
                    {
                        intents.push(SettingsIntent::Reindex(root.path.clone()));
                    }
                });
            });
        });
    }

    ui.add_space(8.0);
    ui.heading("Index exclusions");
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("settings-exclusion-kind")
            .selected_text(exclusion_kind_label(state.exclusion_kind))
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut state.exclusion_kind,
                    ExclusionKind::Folder,
                    "Folder path",
                );
                ui.selectable_value(&mut state.exclusion_kind, ExclusionKind::Ext, "Extension");
                ui.selectable_value(&mut state.exclusion_kind, ExclusionKind::Name, "Name");
            });
        let hint = match state.exclusion_kind {
            ExclusionKind::Folder => r"Folder path, e.g. D:\Archive\Temp",
            ExclusionKind::Ext => "Extension, e.g. tmp",
            ExclusionKind::Name => "File or folder name, e.g. node_modules",
        };
        ui.add(
            egui::TextEdit::singleline(&mut state.exclusion_value)
                .hint_text(hint)
                .desired_width(250.0),
        );
        if ui
            .add_enabled(
                !state.library_loading && !state.exclusion_value.trim().is_empty(),
                egui::Button::new("Add"),
            )
            .clicked()
        {
            intents.push(SettingsIntent::AddExclusion(
                state.exclusion_kind,
                std::mem::take(&mut state.exclusion_value),
            ));
        }
    });
    exclusion_rows(
        ui,
        state,
        ExclusionKind::Folder,
        &state.exclusions.folders,
        intents,
    );
    exclusion_rows(
        ui,
        state,
        ExclusionKind::Ext,
        &state.exclusions.exts,
        intents,
    );
    exclusion_rows(
        ui,
        state,
        ExclusionKind::Name,
        &state.exclusions.names,
        intents,
    );

    ui.add_space(10.0);
    if ui
        .add_enabled(
            !state.integrity_checking,
            egui::Button::new("Check database integrity"),
        )
        .clicked()
    {
        intents.push(SettingsIntent::CheckIntegrity);
    }
    if state.integrity_checking {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Checking database…");
        });
    } else if let Some(result) = &state.integrity_result {
        match result {
            Ok(lines) if lines.is_empty() => {
                ui.colored_label(
                    Color32::from_rgb(99, 190, 127),
                    "Database integrity check passed.",
                );
            }
            Ok(lines) => {
                ui.colored_label(ui.visuals().error_fg_color, lines.join("\n"));
            }
            Err(error) => {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
        }
    }
}

fn exclusion_rows(
    ui: &mut egui::Ui,
    state: &SettingsUi,
    kind: ExclusionKind,
    values: &[String],
    intents: &mut Vec<SettingsIntent>,
) {
    if values.is_empty() {
        return;
    }
    egui::Grid::new(format!("exclusion-rows-{kind:?}"))
        .num_columns(3)
        .striped(true)
        .show(ui, |ui| {
            for value in values {
                ui.label(exclusion_kind_label(kind));
                ui.label(value);
                if ui
                    .add_enabled(!state.library_loading, egui::Button::new("Remove"))
                    .clicked()
                {
                    intents.push(SettingsIntent::RemoveExclusion(kind, value.clone()));
                }
                ui.end_row();
            }
        });
}

fn external_tools_page(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    state: &mut SettingsUi,
    intents: &mut Vec<SettingsIntent>,
) {
    page_heading(
        ui,
        "External tools",
        "Send selected files to another application. Commands are launched with separate arguments, never through a shell.",
    );
    ui.add_space(8.0);
    let mut add_new = false;
    let mut edit_at = None;
    let mut remove_at = None;
    ui.horizontal(|ui| {
        if ui.button("Add application").clicked() {
            add_new = true;
        }
        if ui
            .add_enabled(
                !state.detecting_apps,
                egui::Button::new("Detect installed apps"),
            )
            .clicked()
        {
            intents.push(SettingsIntent::DetectApps);
        }
        if state.detecting_apps {
            ui.spinner();
            ui.label("Checking common install folders…");
        }
    });

    if settings.external_tools.is_empty() {
        ui.label(RichText::new("No external applications have been configured.").weak());
    } else {
        for (index, tool) in settings.external_tools.iter().enumerate() {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.strong(&tool.name);
                        ui.label(RichText::new(tool.exe.display().to_string()).weak().small());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Remove").clicked() {
                            remove_at = Some(index);
                        }
                        if ui.small_button("Edit").clicked() {
                            edit_at = Some(index);
                        }
                    });
                });
                let filters = if tool.extensions.is_empty() {
                    "All file types".to_owned()
                } else {
                    format!("Extensions: {}", tool.extensions.join(", "))
                };
                ui.label(RichText::new(filters).weak().small());
                ui.label(
                    RichText::new(format!("{} argument template(s)", tool.args.len()))
                        .weak()
                        .small(),
                );
            });
        }
    }

    if let Some(detected) = &state.detected_apps {
        ui.add_space(8.0);
        ui.heading("Detected applications");
        if detected.is_empty() {
            ui.label(
                RichText::new("No supported applications were found in the usual install folders.")
                    .weak(),
            );
        }
        let mut add_detected = None;
        for (index, tool) in detected.iter().enumerate() {
            let exists = settings
                .external_tools
                .iter()
                .any(|existing| same_executable(&existing.exe, &tool.exe));
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.strong(&tool.name);
                    ui.label(RichText::new(tool.exe.display().to_string()).weak().small());
                });
                if exists {
                    ui.label(RichText::new("Already added").weak());
                } else if ui.button("Add").clicked() {
                    add_detected = Some(index);
                }
            });
        }
        if let Some(index) = add_detected {
            settings.external_tools.push(detected[index].clone());
        }
    }

    if let Some(index) = remove_at {
        settings.external_tools.remove(index);
        if let Some(draft) = &mut state.tool_draft {
            match draft.editing {
                Some(editing) if editing == index => state.tool_draft = None,
                Some(editing) if editing > index => draft.editing = Some(editing - 1),
                _ => {}
            }
        }
    }
    if let Some(index) = edit_at
        && let Some(tool) = settings.external_tools.get(index).cloned()
    {
        state.tool_draft = Some(ToolDraft::new(Some(index), tool));
    }
    if add_new {
        state.tool_draft = Some(ToolDraft::new(
            None,
            ExternalTool {
                name: String::new(),
                exe: PathBuf::new(),
                args: vec!["{files}".into()],
                extensions: Vec::new(),
            },
        ));
    }

    let mut save: Option<(Option<usize>, ExternalTool)> = None;
    let mut cancel = false;
    if let Some(draft) = &mut state.tool_draft {
        ui.add_space(10.0);
        ui.separator();
        ui.heading(if draft.editing.is_some() {
            "Edit application"
        } else {
            "New application"
        });
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.add(
                egui::TextEdit::singleline(&mut draft.name)
                    .hint_text("e.g. Lightroom Classic")
                    .desired_width(280.0),
            );
        });
        ui.horizontal(|ui| {
            ui.label("Executable");
            let exe_label = if draft.exe.as_os_str().is_empty() {
                "Choose an .exe file…".to_owned()
            } else {
                draft.exe.display().to_string()
            };
            ui.label(RichText::new(exe_label).weak());
            if ui.button("Browse…").clicked() {
                intents.push(SettingsIntent::BrowseToolExecutable);
            }
        });
        ui.label("Arguments: one argument per line. Use {files} for the selected files and {dir} for the current folder.");
        ui.add(
            egui::TextEdit::multiline(&mut draft.args)
                .desired_rows(3)
                .desired_width(ui.available_width())
                .code_editor(),
        );
        ui.horizontal(|ui| {
            ui.label("Extensions");
            ui.add(
                egui::TextEdit::singleline(&mut draft.extensions)
                    .hint_text("jpg, raw, mp4 — leave empty for all files")
                    .desired_width(ui.available_width() - 100.0),
            );
        });
        if let Some(error) = &draft.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        ui.horizontal(|ui| {
            if ui.button("Save application").clicked() {
                match draft.build_tool() {
                    Ok(tool) => save = Some((draft.editing, tool)),
                    Err(error) => draft.error = Some(error),
                }
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    }
    if let Some((editing, tool)) = save {
        if let Some(index) = editing {
            if let Some(existing) = settings.external_tools.get_mut(index) {
                *existing = tool;
            }
        } else {
            settings.external_tools.push(tool);
        }
        state.tool_draft = None;
    } else if cancel {
        state.tool_draft = None;
    }
}

fn default_app_page(ui: &mut egui::Ui, state: &mut SettingsUi, intents: &mut Vec<SettingsIntent>) {
    page_heading(
        ui,
        "Default app",
        "Choose whether photos, RAW files, and filesystem folders open in ZXraw Explorer.",
    );
    ui.add_space(10.0);
    let mut action = None;
    crate::default_app::controls(ui, &mut state.default_app, false, &mut action);
    intents.extend(action.map(SettingsIntent::DefaultApp));
    ui.add_space(12.0);
    ui.label(
        RichText::new(
            "The choice applies to this Windows account only. Win+E, the taskbar File Explorer button, and other apps' \"Show in folder\" commands still open Windows Explorer. Restore previous apps puts back the apps that were set before.",
        )
        .weak(),
    );
}

/// What the Updates page shows, read before the settings window borrows the app.
struct UpdatesView {
    checking: bool,
    last_result: Option<crate::updater::CheckResult>,
    available: Option<String>,
    downloading: bool,
    ready: bool,
    busy: bool,
}

fn updates_page(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    view: &UpdatesView,
    intents: &mut Vec<SettingsIntent>,
) {
    use crate::updater::CheckResult;
    page_heading(
        ui,
        "Updates",
        "Find out about new versions of ZXraw Explorer.",
    );
    ui.add_space(10.0);
    ui.label(format!(
        "You have ZXraw Explorer {}.",
        crate::updater::CURRENT
    ));
    ui.add_space(6.0);
    ui.checkbox(
        &mut settings.check_updates,
        "Check for updates automatically",
    );
    ui.label(
        RichText::new(
            "Once a day ZXraw Explorer asks api.github.com for the latest release. The request carries only the app version (in the User-Agent); updates are never installed without your click.",
        )
        .weak(),
    );
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!view.checking, egui::Button::new("Check now"))
            .clicked()
        {
            intents.push(SettingsIntent::CheckForUpdates);
        }
        if view.checking {
            ui.spinner();
            ui.label("Checking…");
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
        } else {
            match &view.last_result {
                Some(CheckResult::UpToDate) => {
                    ui.label("You're up to date.");
                }
                Some(CheckResult::Available(version)) => {
                    ui.label(RichText::new(format!("{version} is available.")).strong());
                }
                Some(CheckResult::Failed(error)) => {
                    ui.label(
                        RichText::new(format!("Could not check: {error}"))
                            .color(ui.visuals().warn_fg_color),
                    );
                }
                None => {}
            }
        }
    });
    if view.available.is_some() {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if view.ready {
                if ui
                    .add_enabled(!view.busy, egui::Button::new("Restart and update"))
                    .on_disabled_hover_text("Wait for the running operations to finish")
                    .clicked()
                {
                    intents.push(SettingsIntent::InstallUpdate);
                }
            } else if view.downloading {
                ui.spinner();
                ui.label("Downloading the update…");
            } else if ui.button("Show update…").clicked() {
                intents.push(SettingsIntent::ShowUpdate);
            }
        });
    }
    if let Some(skipped) = settings.skipped_update.clone() {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Skipped version: {skipped}")).weak());
            if ui.small_button("Forget").clicked() {
                settings.skipped_update = None;
            }
        });
    }
}

fn privacy_page(ui: &mut egui::Ui) {
    page_heading(
        ui,
        "Privacy",
        "Your files and settings stay on this computer.",
    );
    ui.add_space(14.0);
    ui.group(|ui| {
        ui.label(RichText::new("No telemetry; your files never leave this computer.").strong());
        ui.label(
            RichText::new(
                "ZXraw Explorer does not send usage data or file information to a server.",
            )
            .weak(),
        );
    });
    ui.add_space(8.0);
    ui.group(|ui| {
        ui.label(RichText::new("Network access").strong());
        ui.label(
            RichText::new(
                "Once a day the app asks api.github.com whether a newer version exists. It is a standard web request that carries only the app version in its User-Agent; turn it off under Updates. FFmpeg and app updates are downloaded only when you click to get them, and are checked against their SHA-256 checksums.",
            )
            .weak(),
        );
    });
}

fn shortcuts_page(ui: &mut egui::Ui, settings: &mut Settings, state: &mut SettingsUi) {
    page_heading(
        ui,
        "Keyboard shortcuts",
        "Click Change, then press the new keys. Esc cancels. Changes apply immediately.",
    );
    let warn = ui.visuals().warn_fg_color;

    // The key press that ends a capture. Escape cancels; anything else is the new shortcut.
    if let Some(cmd) = state.capture {
        match record_shortcut(ui.ctx()) {
            None => {}
            Some(None) => state.capture = None,
            Some(Some(text)) => {
                state.capture = None;
                state.shortcut_conflict = None;
                match cmd.conflicting(&settings.keybindings, &text) {
                    Some(other) => {
                        state.shortcut_conflict = Some(ShortcutConflict { cmd, text, other });
                    }
                    None => set_binding(&mut settings.keybindings, cmd, Some(text)),
                }
            }
        }
    }

    if let Some(conflict) = state.shortcut_conflict.take() {
        let mut keep = true;
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                warn,
                format!("{} is already used by \"{}\".", conflict.text, conflict.other.label()),
            );
            if ui
                .button(format!("Unassign it from \"{}\" and use it here", conflict.other.label()))
                .clicked()
            {
                set_binding(&mut settings.keybindings, conflict.other, None);
                set_binding(&mut settings.keybindings, conflict.cmd, Some(conflict.text.clone()));
                keep = false;
            }
            if ui.button("Cancel").clicked() {
                keep = false;
            }
        });
        if keep {
            state.shortcut_conflict = Some(conflict);
        }
        ui.add_space(4.0);
    }

    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.shortcut_filter)
                .hint_text("Search commands…")
                .desired_width(260.0),
        );
        let any_custom = !settings.keybindings.is_empty();
        if ui
            .add_enabled(any_custom, egui::Button::new("Reset all"))
            .on_hover_text("Restore every default shortcut")
            .clicked()
        {
            settings.keybindings.clear();
            state.shortcut_conflict = None;
        }
    });
    ui.add_space(6.0);

    let filter = state.shortcut_filter.trim().to_lowercase();
    egui::Grid::new("settings-shortcuts")
        .num_columns(5)
        .striped(true)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.strong("Command");
            ui.strong("Shortcut");
            ui.label("");
            ui.label("");
            ui.label("");
            ui.end_row();
            for &cmd in Cmd::ALL {
                let text = cmd.binding_text(&settings.keybindings);
                if !filter.is_empty()
                    && !cmd.label().to_lowercase().contains(&filter)
                    && !text.to_lowercase().contains(&filter)
                {
                    continue;
                }
                let capturing = state.capture == Some(cmd);
                ui.label(cmd.label());
                if capturing {
                    ui.label(RichText::new("Press keys… Esc to cancel").italics().color(warn));
                } else if text.is_empty() {
                    ui.label(RichText::new("—").weak());
                } else {
                    ui.label(RichText::new(&text).monospace());
                }
                if capturing {
                    if ui.small_button("Cancel").clicked() {
                        state.capture = None;
                    }
                } else if ui.small_button("Change").clicked() {
                    state.capture = Some(cmd);
                    state.shortcut_conflict = None;
                }
                let clear = ui.add_enabled(
                    !capturing && !text.is_empty(),
                    egui::Button::new("Clear").small(),
                );
                if clear.on_hover_text("Leave this command without a shortcut").clicked() {
                    set_binding(&mut settings.keybindings, cmd, None);
                }
                let customised = settings.keybindings.contains_key(cmd.id());
                if ui
                    .add_enabled(!capturing && customised, egui::Button::new("Reset").small())
                    .on_hover_text("Restore the default shortcut")
                    .clicked()
                {
                    settings.keybindings.remove(cmd.id());
                }
                ui.end_row();
            }
        });
}

/// Takes the next key press from this frame's input and removes it, so no other handler
/// sees it. `Some(None)` is Escape (cancel); `Some(Some(text))` is the recorded shortcut.
fn record_shortcut(ctx: &egui::Context) -> Option<Option<String>> {
    ctx.input_mut(|i| take_shortcut(&mut i.events, i.modifiers))
}

/// The event handling behind [`record_shortcut`]. egui reports Ctrl+C/X/V as clipboard
/// events rather than key presses, so those are mapped back to keys. Clipboard events carry
/// no modifiers, so the frame's modifier state supplies them (Alt and Shift included).
fn take_shortcut(events: &mut Vec<egui::Event>, modifiers: Modifiers) -> Option<Option<String>> {
    let pos = events.iter().position(|e| {
        matches!(
            e,
            egui::Event::Key { pressed: true, repeat: false, .. }
                | egui::Event::Copy
                | egui::Event::Cut
                | egui::Event::Paste(_)
        )
    })?;
    let event = events.remove(pos);
    let clip_key = match &event {
        egui::Event::Copy => Some(egui::Key::C),
        egui::Event::Cut => Some(egui::Key::X),
        egui::Event::Paste(_) => Some(egui::Key::V),
        _ => None,
    };
    let result = match &event {
        egui::Event::Key { key: egui::Key::Escape, .. } => Some(None),
        egui::Event::Key { key, modifiers, .. } => {
            Some(Some(shortcut_text(&KeyboardShortcut::new(*modifiers, *key))))
        }
        _ => {
            // `command` is Ctrl on Windows; setting it keeps the recorded text on "Ctrl+".
            let m = Modifiers { command: true, ..modifiers };
            let key = clip_key.unwrap_or(egui::Key::V);
            Some(Some(shortcut_text(&KeyboardShortcut::new(m, key))))
        }
    };
    // egui may report one Ctrl+C/X/V press twice (a key press and a clipboard event). Drop
    // only that one companion so it is not recorded next frame; other presses stay queued.
    let same_chord = |m: &Modifiers| m.command && m.alt == modifiers.alt && m.shift == modifiers.shift;
    let companion = match (&event, clip_key) {
        (_, Some(k)) => events.iter().position(|e| {
            matches!(e, egui::Event::Key { key, pressed: true, modifiers: m, .. }
                if *key == k && same_chord(m))
        }),
        (egui::Event::Key { key, modifiers: m, .. }, None) if same_chord(m) => {
            events.iter().position(|e| match e {
                egui::Event::Copy => *key == egui::Key::C,
                egui::Event::Cut => *key == egui::Key::X,
                egui::Event::Paste(_) => *key == egui::Key::V,
                _ => false,
            })
        }
        _ => None,
    };
    if let Some(i) = companion {
        events.remove(i);
    }
    result
}

fn show_cache_confirmation(app: &mut ZxrawApp, ctx: &egui::Context) {
    let mut open = app.ops.settings_ui.clear_cache_confirm;
    let mut confirmed = false;
    let mut cancelled = false;
    egui::Window::new("Clear thumbnail cache?")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.label("Cached thumbnails will be removed from disk and regenerated as needed.");
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Clear cache").clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });
    if cancelled {
        open = false;
    }
    app.ops.settings_ui.clear_cache_confirm = open && !confirmed;
    if confirmed {
        clear_cache_background(app, ctx);
    }
}

fn show_open_in_picker(app: &mut ZxrawApp, ctx: &egui::Context) {
    let mut open = app.ops.settings_ui.picker_open;
    let targets = app.ops.settings_ui.picker_targets.clone();
    let folder = app.ops.settings_ui.picker_dir.clone();
    let applicable: Vec<ExternalTool> = app
        .settings
        .external_tools
        .iter()
        .filter(|tool| tool_applies(tool, &targets))
        .cloned()
        .collect();
    let configured = !app.settings.external_tools.is_empty();
    let mut chosen: Option<ExternalTool> = None;
    let mut settings_clicked = false;
    egui::Window::new("Open in external app")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(440.0)
        .show(ctx, |ui| {
            if !configured {
                ui.label("No external applications are configured yet.");
                ui.add_space(8.0);
                if ui.button("Open Settings › External tools").clicked() {
                    settings_clicked = true;
                }
            } else if targets.is_empty() {
                ui.label("Select one or more files to open them in an external application.");
            } else if applicable.is_empty() {
                ui.label("No configured application accepts the selected file types.");
                if ui.button("Review External tools settings").clicked() {
                    settings_clicked = true;
                }
            } else {
                ui.label(format!("Send {} selected item(s) to:", targets.len()));
                ui.add_space(6.0);
                for tool in &applicable {
                    let label = if tool.extensions.is_empty() {
                        tool.name.clone()
                    } else {
                        format!("{}  ·  {}", tool.name, tool.extensions.join(", "))
                    };
                    if ui.button(label).clicked() {
                        chosen = Some(tool.clone());
                    }
                    ui.label(RichText::new(tool.exe.display().to_string()).weak().small());
                }
            }
            ui.add_space(8.0);
            ui.label(
                RichText::new("Selected paths are passed as arguments; they are never executed.")
                    .weak()
                    .small(),
            );
        });
    app.ops.settings_ui.picker_open = open && chosen.is_none() && !settings_clicked;
    if settings_clicked {
        app.ops.settings_ui.page = SettingsPage::ExternalTools;
        app.ops.settings_ui.open = true;
    }
    if let Some(tool) = chosen {
        start_tool_launch(app, tool, targets, folder, ctx);
    }
}

fn handle_intents(app: &mut ZxrawApp, ctx: &egui::Context, intents: Vec<SettingsIntent>) {
    for intent in intents {
        match intent {
            SettingsIntent::PickFfmpegFolder => {
                if let Some(path) = crate::dialogs::pick_folder(
                    rfd::FileDialog::new().set_title("Choose FFmpeg folder"),
                ) {
                    use_ffmpeg_dir(app, Some(path));
                }
            }
            SettingsIntent::UseFfmpegPath => use_ffmpeg_dir(app, None),
            SettingsIntent::DownloadFfmpeg => crate::ffmpeg_setup::confirm(app),
            SettingsIntent::CheckForUpdates => crate::updater::check_now(app),
            SettingsIntent::ShowUpdate => {
                app.notices.update = app.ops.updater.available.clone();
            }
            SettingsIntent::InstallUpdate => crate::updater::install_when_idle(app, ctx),
            SettingsIntent::PickLibraryRoot => {
                if let Some(path) = crate::dialogs::pick_folder(
                    rfd::FileDialog::new().set_title("Add indexed folder"),
                ) {
                    add_library_root(app, path, ctx);
                }
            }
            SettingsIntent::RemoveLibraryRoot(id) => {
                mutate_library(app, ctx, move |db| {
                    db.remove_index_root(id)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                });
            }
            SettingsIntent::AddExclusion(kind, value) => {
                mutate_library(app, ctx, move |db| {
                    db.add_exclusion(kind, &value)
                        .map_err(|error| error.to_string())
                });
            }
            SettingsIntent::RemoveExclusion(kind, value) => {
                mutate_library(app, ctx, move |db| {
                    db.remove_exclusion(kind, &value)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                });
            }
            SettingsIntent::Reindex(root) => {
                if let Some(db) = app.ops.library.db.clone() {
                    let tx = app.ops.tx.clone();
                    let repaint = ctx.clone();
                    crate::library::start_index_job(&app.ops.jobs, db, root, move |message| {
                        let _ = tx.send(crate::actions::Event::Toast(message));
                        repaint.request_repaint();
                    });
                }
            }
            SettingsIntent::CheckIntegrity => check_integrity(app, ctx),
            SettingsIntent::DetectApps => detect_apps_background(&mut app.ops.settings_ui, ctx),
            SettingsIntent::DefaultApp(action) => crate::default_app::run(app, action),
            SettingsIntent::BrowseToolExecutable => {
                if let Some(path) = crate::dialogs::pick_file(
                    rfd::FileDialog::new()
                        .set_title("Choose application executable")
                        .add_filter("Applications", &["exe"]),
                ) && let Some(draft) = &mut app.ops.settings_ui.tool_draft
                {
                    draft.exe = path;
                    draft.error = None;
                }
            }
        }
    }
}

fn add_library_root(app: &mut ZxrawApp, path: PathBuf, ctx: &egui::Context) {
    mutate_library(app, ctx, move |db| {
        if !path.is_dir() {
            return Err(format!("{} is not an available folder", path.display()));
        }
        db.add_index_root(&path)
            .map(|_| ())
            .map_err(|error| error.to_string())
    });
}

/// Points Settings › Video at `dir` (`None` = search PATH), restarts the thumbnail
/// pipeline with it and re-checks for ffprobe.
pub fn use_ffmpeg_dir(app: &mut ZxrawApp, dir: Option<PathBuf>) {
    app.settings.ffmpeg_dir = dir;
    rebuild_thumb_manager(app);
    let ctx = app.ops.ctx.clone();
    start_probe_check(
        &mut app.ops.settings_ui,
        app.settings.ffmpeg_dir.clone(),
        &ctx,
    );
}

fn rebuild_thumb_manager(app: &mut ZxrawApp) {
    app.ops.thumbs = crate::thumbs::ThumbManager::new(
        app.paths.cache.join("thumbs"),
        app.settings.thumb_mem_cache_mb,
        app.settings.ffmpeg_dir.as_deref(),
    );
}

fn mutate_library(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    operation: impl FnOnce(&Db) -> Result<(), String> + Send + 'static,
) {
    let Some(db) = app.ops.library.db.clone() else {
        app.toast_err("Library database is not available");
        return;
    };
    let state = &mut app.ops.settings_ui;
    state.library_loading = true;
    state.library_loaded = false;
    state.library_error = None;
    state.library_generation = state.library_generation.wrapping_add(1);
    let generation = state.library_generation;
    let tx = state.tx.clone();
    let event_tx = app.ops.tx.clone();
    let repaint = ctx.clone();
    std::thread::spawn(move || {
        let result = operation(&db).and_then(|()| read_library_snapshot(&db));
        if let Err(error) = &result {
            let _ = event_tx.send(crate::actions::Event::Error(format!(
                "Could not update library settings: {error}"
            )));
        }
        let _ = tx.send(SettingsMessage::LibrarySnapshot { generation, result });
        repaint.request_repaint();
    });
}

fn refresh_library(state: &mut SettingsUi, db: Arc<Db>, ctx: &egui::Context) {
    state.library_loading = true;
    state.library_error = None;
    state.library_generation = state.library_generation.wrapping_add(1);
    let generation = state.library_generation;
    let tx = state.tx.clone();
    let repaint = ctx.clone();
    std::thread::spawn(move || {
        let result = read_library_snapshot(&db);
        let _ = tx.send(SettingsMessage::LibrarySnapshot { generation, result });
        repaint.request_repaint();
    });
}

fn read_library_snapshot(db: &Db) -> Result<LibrarySnapshot, String> {
    let roots = db.index_roots().map_err(|error| error.to_string())?;
    let exclusions = db.exclusions().map_err(|error| error.to_string())?;
    Ok(LibrarySnapshot { roots, exclusions })
}

fn check_integrity(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(db) = app.ops.library.db.clone() else {
        app.toast_err("Library database is not available");
        return;
    };
    let state = &mut app.ops.settings_ui;
    state.integrity_checking = true;
    state.integrity_result = None;
    let tx = state.tx.clone();
    let repaint = ctx.clone();
    std::thread::spawn(move || {
        let result = db.integrity_check().map_err(|error| error.to_string());
        let _ = tx.send(SettingsMessage::Integrity(result));
        repaint.request_repaint();
    });
}

fn start_cache_scan(state: &mut SettingsUi, root: PathBuf, ctx: &egui::Context) {
    state.cache_generation = state.cache_generation.wrapping_add(1);
    state.cache_loading = true;
    state.cache_error = None;
    let generation = state.cache_generation;
    let tx = state.tx.clone();
    let repaint = ctx.clone();
    std::thread::spawn(move || {
        let result = disk_usage(&root).map_err(|error| error.to_string());
        let _ = tx.send(SettingsMessage::CacheUsage { generation, result });
        repaint.request_repaint();
    });
}

fn clear_cache_background(app: &mut ZxrawApp, ctx: &egui::Context) {
    let state = &mut app.ops.settings_ui;
    state.cache_generation = state.cache_generation.wrapping_add(1);
    state.cache_loading = true;
    state.cache_error = None;
    let generation = state.cache_generation;
    let root = app.paths.cache.join("thumbs");
    let tx = state.tx.clone();
    let repaint = ctx.clone();
    std::thread::spawn(move || {
        let result = clear_cache_contents(&root)
            .and_then(|()| disk_usage(&root))
            .map_err(|error| error.to_string());
        let _ = tx.send(SettingsMessage::CacheCleared { generation, result });
        repaint.request_repaint();
    });
}

fn start_probe_check(state: &mut SettingsUi, configured: Option<PathBuf>, ctx: &egui::Context) {
    state.probe_generation = state.probe_generation.wrapping_add(1);
    state.probe_status = ProbeStatus::Checking;
    let generation = state.probe_generation;
    let tx = state.tx.clone();
    let repaint = ctx.clone();
    std::thread::spawn(move || {
        let tools = forge_media::FfTools::discover_with(configured.as_deref());
        let _ = tx.send(SettingsMessage::Probe {
            generation,
            path: tools.ffprobe,
        });
        repaint.request_repaint();
    });
}

fn detect_apps_background(state: &mut SettingsUi, ctx: &egui::Context) {
    state.detecting_apps = true;
    state.detected_apps = None;
    let tx = state.tx.clone();
    let repaint = ctx.clone();
    std::thread::spawn(move || {
        let tools = detect_apps_in(&install_roots_from_env());
        let _ = tx.send(SettingsMessage::Detected(tools));
        repaint.request_repaint();
    });
}

fn detect_apps_in(roots: &InstallRoots) -> Vec<ExternalTool> {
    APP_CATALOG
        .iter()
        .filter_map(|app| {
            let exe = app.candidates.iter().find_map(|candidate| {
                let base_roots = match candidate.root {
                    InstallRootKind::ProgramFiles => &roots.program_files,
                    InstallRootKind::ProgramFilesX86 => &roots.program_files_x86,
                    InstallRootKind::LocalPrograms => &roots.local_programs,
                };
                base_roots
                    .iter()
                    .find_map(|root| find_pattern(root, candidate.parts))
            })?;
            Some(ExternalTool {
                name: app.name.to_owned(),
                exe,
                args: vec!["{files}".into()],
                extensions: app
                    .extensions
                    .iter()
                    .map(|extension| (*extension).to_owned())
                    .collect(),
            })
        })
        .collect()
}

fn install_roots_from_env() -> InstallRoots {
    let mut roots = InstallRoots::default();
    for key in ["ProgramW6432", "ProgramFiles"] {
        if let Some(root) = std::env::var_os(key).map(PathBuf::from) {
            push_unique_path(&mut roots.program_files, root);
        }
    }
    if let Some(root) = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from) {
        push_unique_path(&mut roots.program_files_x86, root);
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        roots.local_programs.push(local.join("Programs"));
    }
    roots
}

fn push_unique_path(paths: &mut Vec<PathBuf>, new: PathBuf) {
    let key = new.to_string_lossy().to_lowercase();
    if !paths
        .iter()
        .any(|path| path.to_string_lossy().to_lowercase() == key)
    {
        paths.push(new);
    }
}

#[derive(Default)]
struct InstallRoots {
    program_files: Vec<PathBuf>,
    program_files_x86: Vec<PathBuf>,
    local_programs: Vec<PathBuf>,
}

#[derive(Clone, Copy)]
enum InstallRootKind {
    ProgramFiles,
    ProgramFilesX86,
    LocalPrograms,
}

struct AppCandidate {
    root: InstallRootKind,
    parts: &'static [&'static str],
}

struct AppDefinition {
    name: &'static str,
    extensions: &'static [&'static str],
    candidates: &'static [AppCandidate],
}

const APP_CATALOG: &[AppDefinition] = &[
    AppDefinition {
        name: "Adobe Lightroom Classic",
        extensions: &[
            "jpg", "jpeg", "tif", "tiff", "dng", "cr2", "cr3", "nef", "nrw", "arw", "raf", "orf",
            "rw2", "pef", "raw",
        ],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["Adobe", "Adobe Lightroom Classic*", "Lightroom.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["Adobe", "Adobe Lightroom Classic*", "Lightroom.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["Adobe", "Adobe Lightroom Classic*", "Lightroom.exe"],
            },
        ],
    },
    AppDefinition {
        name: "Adobe Photoshop",
        extensions: &[
            "jpg", "jpeg", "png", "tif", "tiff", "dng", "cr2", "cr3", "nef", "arw", "psd", "psb",
        ],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["Adobe", "Adobe Photoshop*", "Photoshop.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["Adobe", "Adobe Photoshop*", "Photoshop.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["Adobe", "Adobe Photoshop*", "Photoshop.exe"],
            },
        ],
    },
    AppDefinition {
        name: "Adobe Premiere Pro",
        extensions: &[
            "mp4", "mov", "mxf", "avi", "mkv", "mpg", "mpeg", "mts", "m2ts",
        ],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["Adobe", "Adobe Premiere Pro*", "Adobe Premiere Pro.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["Adobe", "Adobe Premiere Pro*", "Adobe Premiere Pro.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["Adobe", "Adobe Premiere Pro*", "Adobe Premiere Pro.exe"],
            },
        ],
    },
    AppDefinition {
        name: "Adobe After Effects",
        extensions: &["aep", "aepx", "mp4", "mov", "avi", "mxf"],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &[
                    "Adobe",
                    "Adobe After Effects*",
                    "Support Files",
                    "AfterFX.exe",
                ],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &[
                    "Adobe",
                    "Adobe After Effects*",
                    "Support Files",
                    "AfterFX.exe",
                ],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &[
                    "Adobe",
                    "Adobe After Effects*",
                    "Support Files",
                    "AfterFX.exe",
                ],
            },
        ],
    },
    AppDefinition {
        name: "Capture One",
        extensions: &[
            "jpg", "jpeg", "tif", "tiff", "dng", "cr2", "cr3", "nef", "arw", "raf", "orf", "rw2",
            "raw",
        ],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["Capture One", "Capture One.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["Capture One", "Capture One*", "CaptureOne.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["Capture One", "Capture One.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["Capture One", "Capture One*", "CaptureOne.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["Capture One", "Capture One.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["Capture One", "Capture One*", "CaptureOne.exe"],
            },
        ],
    },
    AppDefinition {
        name: "DaVinci Resolve",
        extensions: &[
            "mp4", "mov", "mxf", "avi", "mkv", "mpg", "mpeg", "wav", "tif", "tiff", "dng",
        ],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["Blackmagic Design", "DaVinci Resolve", "Resolve.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["Blackmagic Design", "DaVinci Resolve", "Resolve.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["Blackmagic Design", "DaVinci Resolve", "Resolve.exe"],
            },
        ],
    },
    AppDefinition {
        name: "Affinity Photo",
        extensions: &[
            "jpg", "jpeg", "png", "tif", "tiff", "dng", "cr2", "cr3", "nef", "arw", "psd",
            "afphoto",
        ],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["Affinity", "Photo 2", "Photo.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["Serif", "Affinity Photo 2", "Photo.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["Serif", "Affinity Photo", "Photo.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["Affinity", "Photo 2", "Photo.exe"],
            },
        ],
    },
    AppDefinition {
        name: "darktable",
        extensions: &[
            "jpg", "jpeg", "tif", "tiff", "dng", "cr2", "cr3", "nef", "arw", "raf", "orf", "rw2",
            "raw",
        ],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["darktable", "bin", "darktable.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["darktable", "bin", "darktable.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["darktable", "bin", "darktable.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["darktable", "darktable.exe"],
            },
        ],
    },
    AppDefinition {
        name: "RawTherapee",
        extensions: &[
            "jpg", "jpeg", "tif", "tiff", "dng", "cr2", "cr3", "nef", "arw", "raf", "orf", "rw2",
            "raw",
        ],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["RawTherapee*", "rawtherapee.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["RawTherapee*", "bin", "rawtherapee.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["RawTherapee", "*", "rawtherapee.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["RawTherapee", "*", "rawtherapee.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["RawTherapee*", "rawtherapee.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["RawTherapee*", "rawtherapee.exe"],
            },
        ],
    },
    AppDefinition {
        name: "GIMP",
        extensions: &["jpg", "jpeg", "png", "gif", "tif", "tiff", "xcf", "psd"],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["GIMP *", "bin", "gimp-*.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["GIMP *", "bin", "gimp-*.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["GIMP *", "bin", "gimp-*.exe"],
            },
        ],
    },
    AppDefinition {
        name: "VEGAS Pro",
        extensions: &["mp4", "mov", "mxf", "avi", "mkv", "mpg", "mpeg", "wav"],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["VEGAS", "VEGAS Pro*", "vegas*.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["VEGAS", "VEGAS Pro*", "vegas*.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["VEGAS", "VEGAS Pro*", "vegas*.exe"],
            },
        ],
    },
    AppDefinition {
        name: "Blender",
        extensions: &["blend", "fbx", "obj", "stl", "glb", "gltf", "abc"],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["Blender Foundation", "Blender *", "blender.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["Blender Foundation", "Blender *", "blender.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["Blender Foundation", "Blender *", "blender.exe"],
            },
        ],
    },
    AppDefinition {
        name: "Audacity",
        extensions: &["wav", "mp3", "flac", "aiff", "aif", "ogg", "m4a"],
        candidates: &[
            AppCandidate {
                root: InstallRootKind::ProgramFiles,
                parts: &["Audacity", "Audacity.exe"],
            },
            AppCandidate {
                root: InstallRootKind::ProgramFilesX86,
                parts: &["Audacity", "Audacity.exe"],
            },
            AppCandidate {
                root: InstallRootKind::LocalPrograms,
                parts: &["Audacity", "Audacity.exe"],
            },
        ],
    },
];

fn find_pattern(base: &Path, parts: &[&str]) -> Option<PathBuf> {
    let (part, rest) = parts.split_first()?;
    if part.contains('*') {
        let mut matches = fs::read_dir(base)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let expected_type = if rest.is_empty() {
                    file_type.is_file()
                } else {
                    file_type.is_dir()
                };
                (expected_type && wildcard_segment_match(&name, part)).then_some(path)
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|path| path.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        return matches.into_iter().find_map(|path| {
            if rest.is_empty() {
                Some(path)
            } else {
                find_pattern(&path, rest)
            }
        });
    }

    let path = base.join(part);
    if rest.is_empty() {
        path.is_file().then_some(path)
    } else if path.is_dir() {
        find_pattern(&path, rest)
    } else {
        None
    }
}

fn wildcard_segment_match(value: &str, pattern: &str) -> bool {
    let value = value.to_lowercase();
    let pattern = pattern.to_lowercase();
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return value == pattern;
    };
    value.starts_with(prefix)
        && value.ends_with(suffix)
        && value.len() >= prefix.len() + suffix.len()
}

fn tool_applies(tool: &ExternalTool, paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }
    if tool.extensions.is_empty() {
        return true;
    }
    paths.iter().all(|path| {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                tool.extensions
                    .iter()
                    .any(|candidate| normalize_extension(candidate).eq_ignore_ascii_case(extension))
            })
    })
}

fn normalize_extension(extension: &str) -> String {
    extension
        .trim()
        .trim_start_matches('*')
        .trim_start_matches('.')
        .to_lowercase()
}

fn parse_extensions(input: &str) -> Vec<String> {
    let mut extensions = Vec::new();
    for extension in input
        .split(',')
        .map(normalize_extension)
        .filter(|s| !s.is_empty())
    {
        if !extensions.iter().any(|existing| existing == &extension) {
            extensions.push(extension);
        }
    }
    extensions
}

fn expand_args(template: &[String], files: &[PathBuf], dir: &Path) -> Vec<OsString> {
    let mut expanded = Vec::new();
    for argument in template {
        if argument == "{files}" {
            expanded.extend(files.iter().map(|path| path.as_os_str().to_os_string()));
        } else if argument == "{dir}" {
            expanded.push(dir.as_os_str().to_os_string());
        } else if argument.contains("{files}") {
            for file in files {
                let with_files = argument.replace("{files}", &file.to_string_lossy());
                expanded.push(replace_dir(&with_files, dir));
            }
        } else {
            expanded.push(replace_dir(argument, dir));
        }
    }
    expanded
}

fn replace_dir(argument: &str, dir: &Path) -> OsString {
    if argument == "{dir}" {
        dir.as_os_str().to_os_string()
    } else if argument.contains("{dir}") {
        OsString::from(argument.replace("{dir}", &dir.to_string_lossy()))
    } else {
        OsString::from(argument)
    }
}

fn same_executable(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn start_tool_launch(
    app: &ZxrawApp,
    tool: ExternalTool,
    targets: Vec<PathBuf>,
    folder: PathBuf,
    ctx: &egui::Context,
) {
    let args = expand_args(&tool.args, &targets, &folder);
    let tx = app.ops.tx.clone();
    let repaint = ctx.clone();
    std::thread::spawn(move || {
        if let Err(error) = forge_win::launch(&tool.exe, &args, Some(&folder)) {
            let _ = tx.send(crate::actions::Event::Error(format!(
                "Could not open in {}: {error}",
                tool.name
            )));
        } else {
            let _ = tx.send(crate::actions::Event::Toast(format!(
                "Opened in {}",
                tool.name
            )));
        }
        repaint.request_repaint();
    });
}

fn clear_cache_contents(root: &Path) -> io::Result<()> {
    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "thumbnail cache path is not a regular directory",
        ));
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            if fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                fs::remove_dir(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        } else if file_type.is_dir() {
            // std::fs::remove_dir_all removes nested symlinks instead of following them.
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn disk_usage(root: &Path) -> io::Result<u64> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "thumbnail cache path is not a regular directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    }

    fn walk(directory: &Path) -> io::Result<u64> {
        let mut bytes = 0u64;
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                bytes = bytes.saturating_add(walk(&entry.path())?);
            } else if file_type.is_file() {
                bytes = bytes.saturating_add(entry.metadata()?.len());
            }
        }
        Ok(bytes)
    }

    walk(root)
}

fn theme_label(theme: ThemeChoice) -> &'static str {
    match theme {
        ThemeChoice::Dark => "Dark",
        ThemeChoice::Light => "Light",
        ThemeChoice::System => "System",
    }
}

fn exclusion_kind_label(kind: ExclusionKind) -> &'static str {
    match kind {
        ExclusionKind::Folder => "Folder",
        ExclusionKind::Ext => "Extension",
        ExclusionKind::Name => "Name",
    }
}

fn page_heading(ui: &mut egui::Ui, title: &str, description: &str) {
    ui.heading(title);
    ui.label(RichText::new(description).weak());
    ui.separator();
}

/// `Cmd::OpenIn`: show the picker for the current selection. The selected files are only
/// passed as arguments to the configured tool; they are never executed by ZXraw Explorer.
pub fn open_in(app: &mut ZxrawApp) {
    let targets = app.pane().action_targets();
    let directory = app.pane().path.clone();
    let state = &mut app.ops.settings_ui;
    state.picker_targets = targets;
    state.picker_dir = directory;
    state.picker_open = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestTree(PathBuf);

    impl TestTree {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("zxraw-settings-ui-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&path).expect("create test tree");
            Self(path)
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().expect("parent directory"))
                .expect("create app directory");
            fs::write(&path, b"fake executable").expect("write fake executable");
            path
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn files_expand_to_separate_unchanged_arguments() {
        let files = [
            PathBuf::from(r"C:\Photos\family portrait.jpg"),
            PathBuf::from(r#"C:\Photos\quoted"name.jpg"#),
            PathBuf::from(r"C:\Photos\-leading-dash.jpg"),
            PathBuf::from(r"C:\Photos\café-雪.jpg"),
        ];
        let directory = PathBuf::from(r"C:\Photos\Current Set");
        let args = expand_args(
            &["--import".into(), "{files}".into(), "--from={dir}".into()],
            &files,
            &directory,
        );

        assert_eq!(args[0], OsString::from("--import"));
        assert_eq!(
            &args[1..5],
            files
                .iter()
                .map(|path| path.as_os_str().to_os_string())
                .collect::<Vec<_>>()
                .as_slice()
        );
        assert_eq!(
            args[5],
            OsString::from(format!("--from={}", directory.display()))
        );
    }

    #[test]
    fn extension_filter_is_case_insensitive_and_requires_all_targets_to_match() {
        let tool = ExternalTool {
            name: "Photos".into(),
            exe: PathBuf::from("Photos.exe"),
            args: vec!["{files}".into()],
            extensions: vec![".JPG".into(), "raw".into()],
        };
        assert!(tool_applies(
            &tool,
            &[PathBuf::from("portrait.JPG"), PathBuf::from("camera.raw")]
        ));
        assert!(!tool_applies(
            &tool,
            &[PathBuf::from("portrait.jpg"), PathBuf::from("clip.mp4")]
        ));
        assert!(!tool_applies(&tool, &[]));
    }

    #[test]
    fn detection_matches_versioned_install_directories_in_a_fake_tree() {
        let tree = TestTree::new();
        let photoshop = tree.file("Program Files/Adobe/Adobe Photoshop 2025/Photoshop.exe");
        let blender = tree.file("Local/Blender Foundation/Blender 4.2/blender.exe");
        let roots = InstallRoots {
            program_files: vec![tree.0.join("Program Files")],
            program_files_x86: Vec::new(),
            local_programs: vec![tree.0.join("Local")],
        };

        let found = detect_apps_in(&roots);
        let photoshop_tool = found.iter().find(|tool| tool.name == "Adobe Photoshop");
        let blender_tool = found.iter().find(|tool| tool.name == "Blender");
        assert_eq!(
            photoshop_tool.map(|tool| tool.exe.as_path()),
            Some(photoshop.as_path())
        );
        assert_eq!(
            blender_tool.map(|tool| tool.exe.as_path()),
            Some(blender.as_path())
        );
    }
}

#[cfg(test)]
mod shortcut_capture_tests {
    use super::*;

    fn key(key: egui::Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    #[test]
    fn clipboard_event_keeps_alt_and_shift() {
        let held = Modifiers { alt: true, ..Modifiers::COMMAND };
        let mut events = vec![egui::Event::Copy];
        assert_eq!(take_shortcut(&mut events, held), Some(Some("Ctrl+Alt+C".into())));
        assert!(events.is_empty());

        let mut events = vec![egui::Event::Cut];
        let held = Modifiers { shift: true, ..Modifiers::COMMAND };
        assert_eq!(take_shortcut(&mut events, held), Some(Some("Ctrl+Shift+X".into())));
    }

    #[test]
    fn paste_without_modifiers_is_ctrl_v() {
        let mut events = vec![egui::Event::Paste("text".into())];
        assert_eq!(take_shortcut(&mut events, Modifiers::COMMAND), Some(Some("Ctrl+V".into())));
    }

    #[test]
    fn clipboard_press_is_recorded_once_with_its_key_event() {
        let held = Modifiers { alt: true, ..Modifiers::COMMAND };
        let mut events = vec![egui::Event::Copy, key(egui::Key::C, held)];
        assert_eq!(take_shortcut(&mut events, held), Some(Some("Ctrl+Alt+C".into())));
        assert!(events.is_empty(), "companion key press left behind: {events:?}");

        let mut events = vec![key(egui::Key::C, Modifiers::COMMAND), egui::Event::Copy];
        assert_eq!(take_shortcut(&mut events, Modifiers::COMMAND), Some(Some("Ctrl+C".into())));
        assert!(events.is_empty());
    }

    #[test]
    fn only_the_companion_is_dropped_and_other_chords_stay_queued() {
        let shifted = Modifiers { shift: true, ..Modifiers::COMMAND };
        let mut events = vec![
            egui::Event::Copy,
            key(egui::Key::C, Modifiers::COMMAND),
            key(egui::Key::C, shifted),
            egui::Event::Cut,
        ];
        assert_eq!(take_shortcut(&mut events, Modifiers::COMMAND), Some(Some("Ctrl+C".into())));
        assert_eq!(events, vec![key(egui::Key::C, shifted), egui::Event::Cut]);
    }

    #[test]
    fn key_press_and_escape_are_recorded_as_before() {
        let mut events = vec![key(egui::Key::Escape, Modifiers::NONE)];
        assert_eq!(take_shortcut(&mut events, Modifiers::NONE), Some(None));

        let held = Modifiers { alt: true, ..Modifiers::COMMAND };
        let mut events = vec![key(egui::Key::Delete, held)];
        assert_eq!(take_shortcut(&mut events, held), Some(Some("Ctrl+Alt+Delete".into())));
    }

    #[test]
    fn no_input_records_nothing() {
        let mut events = vec![egui::Event::PointerMoved(egui::Pos2::ZERO)];
        assert_eq!(take_shortcut(&mut events, Modifiers::NONE), None);
        assert_eq!(events.len(), 1);
    }
}
