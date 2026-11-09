//! Startup notices: the first-run welcome, the "closed unexpectedly" crash notice and the
//! "update available" notice.
//!
//! All are ordinary non-modal windows; none blocks the file browser. The welcome is
//! shown once per profile (a `first-run.done` marker in the config folder). The crash
//! notice is shown when the previous session left a crash report
//! ([`crate::crash::pending_crash_report`]); reports stay on this PC and nothing is sent.

use std::path::{Path, PathBuf};

use egui::RichText;

use crate::app::ZxrawApp;
use crate::commands::Cmd;
use crate::settings::{ThemeChoice, ViewMode};

const FIRST_RUN_MARKER: &str = "first-run.done";

#[derive(Default)]
pub struct Notices {
    /// Crash report from the previous session, until dismissed.
    pub crash: Option<PathBuf>,
    /// The welcome window, while open.
    pub welcome: Option<Welcome>,
    /// A newer release found by the update check, until dismissed (see `updater`).
    pub update: Option<crate::updater::UpdateInfo>,
    marker: PathBuf,
    automated: bool,
}

pub struct Welcome {
    /// `Some(found)` once ffmpeg/ffprobe discovery has run.
    video_tools: Option<VideoTools>,
}

struct VideoTools {
    ffmpeg: bool,
    ffprobe: bool,
}

impl Notices {
    /// `automated` runs skip the welcome unless `ZXRAW_FIRST_RUN=1` asks for it, so
    /// scripted scenarios see the plain browser.
    pub fn new(config_dir: &Path, crash: Option<PathBuf>, automated: bool) -> Self {
        let marker = config_dir.join(FIRST_RUN_MARKER);
        let forced = std::env::var_os("ZXRAW_FIRST_RUN").is_some_and(|v| v == "1");
        let first_run = !marker.exists() && (!automated || forced);
        Self {
            crash,
            welcome: first_run.then_some(Welcome { video_tools: None }),
            update: None,
            marker,
            automated,
        }
    }

    /// FFmpeg was installed or reconfigured: re-detect it the next time the welcome draws.
    pub fn video_tools_changed(&mut self) {
        if let Some(welcome) = &mut self.welcome {
            welcome.video_tools = None;
        }
    }

    /// Records that the welcome was seen. Never replaces anything: the marker is created
    /// with `create_new`, and an existing marker already means "seen".
    pub fn mark_welcome_done(&mut self) {
        self.welcome = None;
        let created = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.marker);
        if let Err(e) = created
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            tracing::warn!("cannot record first run: {e}");
        }
    }
}

/// Shows whichever notices are pending. Call once per frame.
pub fn show(app: &mut ZxrawApp, ctx: &egui::Context) {
    crash_notice(app, ctx);
    welcome(app, ctx);
    update_notice(app, ctx);
}

fn update_notice(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(update) = app.notices.update.clone() else {
        return;
    };
    let download = app.ops.updater.download_status();
    let ready = app.ops.updater.restart_ready();
    let busy = app.ops.jobs.running_count() > 0;
    let method = crate::updater::self_update();
    let installed = method.installs();
    let linux = app.ops.updater.linux.clone();
    let mut open = true;
    let mut dismiss = false;
    let mut action = None;
    egui::Window::new("Update available")
        .id(egui::Id::new("update-notice"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::RIGHT_TOP, [-16.0, 84.0])
        .show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.label(RichText::new(format!("ZXraw Explorer {} is available", update.version)).strong());
            ui.label(RichText::new(format!("You have {}.", crate::updater::CURRENT)).weak());
            if !update.notes.is_empty() {
                egui::CollapsingHeader::new("Release notes").id_salt("update-notes").show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(&update.notes).small()).wrap());
                    });
                });
            }
            ui.add_space(6.0);
            if download.running && download.waiting_for_password {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Waiting for your password…");
                });
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            } else if download.running {
                ui.horizontal(|ui| {
                    match download.fraction {
                        Some(f) => {
                            ui.add(egui::ProgressBar::new(f).desired_width(160.0).show_percentage());
                        }
                        None => {
                            ui.spinner();
                        }
                    }
                    ui.label("Downloading…");
                    if ui.button("Cancel").clicked() {
                        app.ops.updater.cancel_download();
                    }
                });
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            } else if ready {
                let (text, button) = if cfg!(windows) {
                    ("The update is downloaded and verified. ZXraw Explorer closes, updates and starts again.", "Restart and update")
                } else {
                    ("The update is installed. Restart ZXraw Explorer to use it.", "Restart now")
                };
                ui.label(text);
                let restart = ui.add_enabled(!busy, egui::Button::new(RichText::new(button).strong()));
                if busy {
                    restart.on_disabled_hover_text("Wait for the running operations to finish");
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
                } else if restart.clicked() {
                    action = Some(UpdateAction::Install);
                }
            } else {
                if let Some(status) = &linux {
                    linux_status(ui, status, &mut action);
                    ui.add_space(4.0);
                }
                let explain = match method {
                    crate::updater::SelfUpdate::Installer => {
                        Some("Update now downloads the installer, checks its SHA-256, then restarts ZXraw Explorer.")
                    }
                    crate::updater::SelfUpdate::AppImage => {
                        Some("Update now downloads the new AppImage, checks its SHA-256 and puts it in place of this one. No password is needed.")
                    }
                    crate::updater::SelfUpdate::Deb => {
                        Some("Update now downloads the .deb, checks its SHA-256 and installs it. You are asked for your password.")
                    }
                    crate::updater::SelfUpdate::PageOnly => None,
                };
                if let Some(explain) = explain.filter(|_| linux.is_none()) {
                    ui.label(RichText::new(explain).weak());
                }
                ui.horizontal(|ui| {
                    let label = if installed {
                        "Update now"
                    } else if cfg!(windows) {
                        "Download"
                    } else {
                        "Open the release page"
                    };
                    let hover = if installed {
                        "Download, verify and install the update, then restart"
                    } else if cfg!(windows) {
                        "This is a portable copy: opens the release page to download the new zip"
                    } else {
                        "This copy was not installed from the AppImage or the .deb: opens the release page with both"
                    };
                    if ui.button(RichText::new(label).strong()).on_hover_text(hover).clicked() {
                        action = Some(UpdateAction::Update);
                    }
                    if ui.button("What's new").clicked() {
                        action = Some(UpdateAction::Page);
                    }
                    if ui.button("Skip this version").clicked() {
                        action = Some(UpdateAction::Skip);
                    }
                    if ui.button("Not now").clicked() {
                        dismiss = true;
                    }
                });
            }
        });
    match action {
        Some(UpdateAction::Update) => crate::updater::update_now(app, &update),
        Some(UpdateAction::Page) => crate::updater::open_release_page(app, &update),
        Some(UpdateAction::Skip) => crate::updater::skip(app, &update),
        Some(UpdateAction::Install) => crate::updater::install_when_idle(app, ctx),
        Some(UpdateAction::OpenFile(path)) => crate::updater::open_kept_file(app, path, false),
        Some(UpdateAction::Reveal(path)) => crate::updater::open_kept_file(app, path, true),
        None => {}
    }
    if !open || dismiss {
        app.ops.updater.cancel_download();
        app.notices.update = None;
    }
}

/// How the last Linux "Update now" stopped short of "Restart now".
fn linux_status(ui: &mut egui::Ui, status: &crate::updater::LinuxStatus, action: &mut Option<UpdateAction>) {
    use crate::updater::LinuxStatus;
    let warn = ui.visuals().warn_fg_color;
    match status {
        LinuxStatus::Installed { .. } => {}
        LinuxStatus::Cancelled { file } => {
            ui.label("Update cancelled. The verified package is kept; Update now asks for the password again.");
            if ui.button("Open the file").on_hover_text("Opens the .deb in the package installer").clicked() {
                *action = Some(UpdateAction::OpenFile(file.clone()));
            }
        }
        LinuxStatus::Failed { error, file } => {
            ui.label(RichText::new("The update could not be installed:").color(warn));
            egui::ScrollArea::vertical().id_salt("update-error").max_height(120.0).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(error).small().monospace()).wrap());
            });
            if ui.button("Open the file").on_hover_text("Opens the verified .deb in the package installer").clicked() {
                *action = Some(UpdateAction::OpenFile(file.clone()));
            }
        }
        LinuxStatus::SavedInstead { file, reason } => {
            ui.label(RichText::new(format!("ZXraw Explorer cannot replace its AppImage ({reason}).")).color(warn));
            ui.label(format!(
                "The verified new AppImage was saved as {}. Move it in place of the old one, or start it from there.",
                file.display()
            ));
            if ui.button("Open download folder").clicked() {
                *action = Some(UpdateAction::Reveal(file.clone()));
            }
        }
        LinuxStatus::PackageInstaller { .. } => {
            ui.label("Finish the install in the Package Installer, then restart ZXraw Explorer.");
        }
    }
}

enum UpdateAction {
    Update,
    Page,
    Skip,
    Install,
    OpenFile(PathBuf),
    Reveal(PathBuf),
}

fn crash_notice(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(report) = app.notices.crash.clone() else {
        return;
    };
    let mut open = true;
    let mut dismiss = false;
    egui::Window::new("ZXraw Explorer closed unexpectedly")
        .id(egui::Id::new("crash-notice"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
        .show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.label("The last session ended with an error. File operations that were running at that moment may not have finished; check their destination folders.");
            ui.add_space(6.0);
            ui.label("A crash report was saved on this PC. Nothing was sent anywhere.");
            ui.add(egui::Label::new(RichText::new(report.display().to_string()).monospace()).wrap());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Show report in Explorer").clicked() {
                    if app.notices.automated {
                        tracing::info!("automated run: not opening Explorer for {}", report.display());
                    } else if let Err(e) = crate::platform::reveal(&report) {
                        app.toast_err(format!("Cannot open Explorer: {e}"));
                    }
                }
                if ui.button("Copy path").clicked() {
                    ui.ctx().copy_text(report.display().to_string());
                }
                if ui.button("Dismiss").clicked() {
                    dismiss = true;
                }
            });
        });
    if !open || dismiss {
        app.notices.crash = None;
    }
}

fn welcome(app: &mut ZxrawApp, ctx: &egui::Context) {
    let keys = app.settings.keybindings.clone();
    let Some(state) = app.notices.welcome.as_mut() else {
        return;
    };
    let tools = state.video_tools.get_or_insert_with(|| {
        let tools = match &app.settings.ffmpeg_dir {
            Some(dir) => forge_media::video::FfTools::discover_in(std::slice::from_ref(dir)),
            None => forge_media::video::FfTools::global().clone(),
        };
        VideoTools {
            ffmpeg: tools.has_ffmpeg(),
            ffprobe: tools.has_ffprobe(),
        }
    });
    let (has_ffmpeg, has_ffprobe) = (tools.ffmpeg, tools.ffprobe);
    let download = app.ops.ffmpeg_setup.status();

    let mut open = true;
    let mut done = false;
    let mut run: Option<Cmd> = None;
    let mut default_app = None;
    let mut view_changed = false;
    egui::Window::new("Welcome to ZXraw Explorer")
        .id(egui::Id::new("welcome"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_max_width(500.0);
            let logo = crate::app_icon::preview(ctx, *app.settings.app_icon);
            ui.horizontal(|ui| {
                ui.image((logo.id(), egui::vec2(36.0, 36.0)));
                ui.label("A fast file manager for photographers and videographers.");
            });
            let privacy = if cfg!(windows) {
                "Everything stays on this PC: no account and no telemetry. The only network access is a daily update check (Settings › Updates can turn it off) and the optional FFmpeg download."
            } else {
                "Everything stays on this PC: no account, no telemetry and no network access."
            };
            ui.label(RichText::new(privacy).weak());
            ui.add_space(8.0);

            ui.heading("Quick setup");
            egui::Grid::new("welcome-setup").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Theme");
                ui.horizontal(|ui| {
                    app.settings.theme.edit(|theme| {
                        ui.selectable_value(theme, ThemeChoice::Dark, "Dark");
                        ui.selectable_value(theme, ThemeChoice::Light, "Light");
                        ui.selectable_value(theme, ThemeChoice::System, "System");
                    });
                });
                ui.end_row();
                ui.label("Folders open in");
                ui.horizontal(|ui| {
                    app.settings.default_view.edit(|view| {
                        view_changed |= ui.selectable_value(view, ViewMode::Details, "Details").changed();
                        view_changed |= ui.selectable_value(view, ViewMode::Grid, "Thumbnails").changed();
                    });
                });
                ui.end_row();
                ui.label("Ratings & labels");
                ui.checkbox(
                    &mut app.settings.xmp_sidecar_sync,
                    "Also write .xmp sidecars (Lightroom, Capture One, darktable)",
                );
                ui.end_row();
            });
            ui.add_space(8.0);

            if cfg!(any(windows, target_os = "linux")) {
                ui.heading("Default app");
                ui.label(if cfg!(windows) {
                    "Choose ZXraw Explorer for your photo and RAW files, and optionally for filesystem folders."
                } else {
                    "Open folders, photos and RAW files, or archives with ZXraw Explorer. Settings › Default app lists every type."
                });
                crate::default_app::controls(ui, &mut app.ops.settings_ui.default_app, true, &mut default_app);
                ui.add_space(8.0);
            }

            ui.heading("Video");
            if has_ffmpeg && has_ffprobe {
                ui.label("FFmpeg found: video thumbnails and clip metadata are on.");
            } else {
                if cfg!(windows) {
                    ui.label("FFmpeg was not found, so video thumbnails and clip metadata are off. ZXraw Explorer does not bundle it: download it here, or install it yourself and point Settings at its folder.");
                } else {
                    ui.label("FFmpeg was not found, so video thumbnails and clip metadata are off. ZXraw Explorer does not bundle it.");
                    ui.label(egui::RichText::new(crate::ffmpeg_setup::PACKAGE_MANAGER_HINT).weak());
                }
                crate::ffmpeg_setup::offer(ui, download, || run = Some(Cmd::DownloadFfmpeg));
                if !download.running && ui.button("Open Settings…").clicked() {
                    run = Some(Cmd::Settings);
                }
            }
            ui.add_space(8.0);

            ui.heading("Handy keys");
            egui::Grid::new("welcome-keys").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
                for cmd in [Cmd::CommandPalette, Cmd::Ingest, Cmd::CullMode, Cmd::Search, Cmd::ToggleDualPane, Cmd::Rename] {
                    ui.label(RichText::new(cmd.binding_text(&keys)).monospace());
                    ui.label(cmd.label());
                    ui.end_row();
                }
                ui.label(RichText::new("0–5 · P · X").monospace());
                ui.label("Rate, pick and reject while culling");
                ui.end_row();
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Get started").strong()).clicked() {
                    done = true;
                }
                if ui.button("Import from a card…").clicked() {
                    done = true;
                    run = Some(Cmd::Ingest);
                }
            });
        });
    if view_changed {
        // Apply to the folder already on screen, not just to folders opened later.
        let view = *app.settings.default_view;
        for tab in &mut app.tabs {
            tab.left.view = view;
            if let Some(right) = &mut tab.right {
                right.view = view;
            }
        }
    }
    if !open || done {
        app.notices.mark_welcome_done();
        app.save_all();
    }
    if let Some(action) = default_app {
        crate::default_app::run(app, action);
    }
    if let Some(cmd) = run {
        app.run(cmd, ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_shows_once_per_profile() {
        let dir = tempfile::tempdir().unwrap();
        let mut notices = Notices::new(dir.path(), None, false);
        assert!(notices.welcome.is_some(), "a fresh profile gets the welcome");
        notices.mark_welcome_done();
        assert!(notices.welcome.is_none());
        assert!(dir.path().join(FIRST_RUN_MARKER).is_file());
        assert!(Notices::new(dir.path(), None, false).welcome.is_none());
        // Marking again (e.g. a second window) keeps the existing marker.
        let mut again = Notices::new(dir.path(), None, false);
        again.mark_welcome_done();
        assert!(dir.path().join(FIRST_RUN_MARKER).is_file());
    }

    #[test]
    fn automated_runs_skip_the_welcome_but_keep_the_crash_notice() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("crash-1.txt");
        let notices = Notices::new(dir.path(), Some(report.clone()), true);
        // ZXRAW_FIRST_RUN is not set by the unit test runner.
        assert!(notices.welcome.is_none());
        assert_eq!(notices.crash, Some(report));
    }
}
