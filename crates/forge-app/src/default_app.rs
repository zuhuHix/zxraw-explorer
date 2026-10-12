//! "Open photos with ZXraw Explorer": the opt-in default-app setup offered by the first-run
//! welcome and Settings > Default app.
//!
//! Windows does not let a program make itself the default: [`make_default`] registers the
//! photo types for this user (see `forge_win::register_file_types`, which also covers
//! programs that bypass the Settings choice) and opens Settings, where the user confirms.

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use egui::RichText;
use forge_core::model::{IMAGE_EXTS, RAW_EXTS};

use crate::app::ZxrawApp;

/// Image types the viewer cannot decode; they keep opening in their current apps.
const NOT_VIEWABLE: &[&str] = &["psd", "jxl", "ico", "crw", "x3f", "mrw"];

/// How long a status check stays fresh while it is on screen. The user changes defaults in
/// Settings, outside the app, so the page keeps re-checking.
const RECHECK: Duration = Duration::from_secs(2);

/// The photo types offered, all of which the built-in viewer opens.
pub fn file_types() -> Vec<forge_win::FileType> {
    IMAGE_EXTS
        .iter()
        .chain(RAW_EXTS)
        .filter(|extension| !NOT_VIEWABLE.contains(extension))
        .map(|&extension| forge_win::FileType {
            extension: extension.to_string(),
            description: describe(extension),
        })
        .collect()
}

fn describe(extension: &str) -> String {
    match extension {
        "jpg" | "jpeg" | "jpe" | "jfif" => "JPEG image".into(),
        "tif" | "tiff" => "TIFF image".into(),
        "heic" | "heif" => "HEIF image".into(),
        "bmp" => "Bitmap image".into(),
        "webp" => "WebP image".into(),
        image if IMAGE_EXTS.contains(&image) => format!("{} image", image.to_uppercase()),
        raw => format!("{} raw image", raw.to_uppercase()),
    }
}

/// Which app each offered type opens with.
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// The user opted in (ZXraw Explorer is registered for the types).
    pub registered: bool,
    /// `(extension, app name)`, and whether that app is ZXraw Explorer.
    pub types: Vec<(String, Option<String>, bool)>,
}

impl Status {
    fn check() -> Self {
        let types = file_types()
            .into_iter()
            .map(|file_type| {
                let app = forge_win::default_app(&file_type.extension);
                let ours = app.as_ref().is_some_and(|app| app.is_zxraw);
                (file_type.extension, app.map(|app| app.name), ours)
            })
            .collect();
        Self {
            registered: forge_win::file_types_registered().unwrap_or(false),
            types,
        }
    }

    pub fn ours(&self) -> usize {
        self.types.iter().filter(|(_, _, ours)| *ours).count()
    }

    fn all_ours(&self) -> bool {
        self.ours() == self.types.len()
    }

    /// The app JPEGs open with, the type people notice.
    fn jpeg_app(&self) -> Option<&str> {
        self.types
            .iter()
            .find(|(extension, _, _)| extension == "jpg")
            .and_then(|(_, app, _)| app.as_deref())
    }
}

/// Background-refreshed [`Status`] for whichever window shows it.
#[derive(Default)]
pub struct State {
    status: Option<Status>,
    checked: Option<Instant>,
    rx: Option<Receiver<Status>>,
}

impl State {
    /// Latest status, re-checked off the UI thread while it is being shown.
    fn current(&mut self, ctx: &egui::Context) -> Option<&Status> {
        if let Some(rx) = &self.rx
            && let Ok(status) = rx.try_recv()
        {
            self.status = Some(status);
            self.rx = None;
        }
        let stale = self.checked.is_none_or(|at| at.elapsed() >= RECHECK);
        if stale && self.rx.is_none() {
            let (tx, rx) = bounded(1);
            let repaint = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Status::check());
                repaint.request_repaint();
            });
            self.rx = Some(rx);
            self.checked = Some(Instant::now());
        }
        ctx.request_repaint_after(RECHECK);
        self.status.as_ref()
    }

    fn invalidate(&mut self) {
        self.checked = None;
    }
}

/// What the user asked for in [`controls`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    MakeDefault,
    OpenWindowsSettings,
    Remove,
}

/// Status line plus buttons. `compact` (the welcome) leaves out the per-type list and
/// "Stop using".
pub fn controls(ui: &mut egui::Ui, state: &mut State, compact: bool, action: &mut Option<Action>) {
    let ctx = ui.ctx().clone();
    let Some(status) = state.current(&ctx) else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Checking which apps open your photos…");
        });
        return;
    };
    let total = status.types.len();
    if status.all_ours() {
        ui.label(format!(
            "ZXraw Explorer opens all {total} photo and RAW types."
        ));
    } else if status.registered {
        ui.label(format!(
            "ZXraw Explorer opens {} of {total} photo and RAW types. To finish, Windows asks you to confirm: on ZXraw Explorer's page in Settings, click \"Set default\" at the top.",
            status.ours()
        ));
    } else if let Some(app) = status.jpeg_app() {
        ui.label(RichText::new(format!("JPEG photos currently open in {app}.")).weak());
    }
    ui.horizontal(|ui| {
        if !status.registered {
            if ui
                .button("Make ZXraw Explorer the default photo viewer…")
                .on_hover_text("Registers JPEG, PNG, TIFF, HEIC, WebP and RAW files for you only, then opens Windows Settings to confirm.")
                .clicked()
            {
                *action = Some(Action::MakeDefault);
            }
        } else if !status.all_ours()
            && ui.button("Open Windows Settings…").clicked()
        {
            *action = Some(Action::OpenWindowsSettings);
        }
        if !compact
            && status.registered
            && ui
                .button("Stop using as default")
                .on_hover_text("Removes ZXraw Explorer's file types and restores what other apps used before.")
                .clicked()
        {
            *action = Some(Action::Remove);
        }
    });
    if compact {
        return;
    }
    ui.add_space(6.0);
    egui::CollapsingHeader::new(format!("Photo types ({total})"))
        .id_salt("default-app-types")
        .show(ui, |ui| {
            egui::Grid::new("default-app-type-grid")
                .num_columns(2)
                .spacing([16.0, 2.0])
                .striped(true)
                .show(ui, |ui| {
                    for (extension, app, ours) in &status.types {
                        ui.label(RichText::new(format!(".{extension}")).monospace());
                        let app = app.as_deref().unwrap_or("No app");
                        if *ours {
                            ui.label(RichText::new(app).strong());
                        } else {
                            ui.label(RichText::new(app).weak());
                        }
                        ui.end_row();
                    }
                });
        });
}

/// Carries out an [`Action`] from [`controls`].
pub fn run(app: &mut ZxrawApp, action: Action) {
    if app.testhook.active() {
        // Automated runs never change the user's file associations.
        tracing::info!("automated run: not changing file associations ({action:?})");
        return;
    }
    match action {
        Action::MakeDefault => {
            let registered = std::env::current_exe()
                .map_err(|error| error.to_string())
                .and_then(|exe| {
                    forge_win::register_file_types(exe, &file_types())
                        .map_err(|error| error.to_string())
                });
            if let Err(error) = registered {
                app.toast_err(format!(
                    "Could not register ZXraw Explorer for photos: {error}"
                ));
                return;
            }
            open_windows_settings(app);
        }
        Action::OpenWindowsSettings => open_windows_settings(app),
        Action::Remove => match forge_win::unregister_file_types() {
            Ok(()) => app.toast("Photos open with your previous apps again"),
            Err(error) => app.toast_err(format!(
                "Could not remove ZXraw Explorer's file types: {error}"
            )),
        },
    }
    app.ops.settings_ui.default_app.invalidate();
}

fn open_windows_settings(app: &mut ZxrawApp) {
    match forge_win::open_default_apps_settings() {
        Ok(()) => app.toast("In Settings, click \"Set default\" to finish"),
        Err(error) => app.toast_err(format!(
            "Could not open Windows Settings ({error}). Open Settings > Apps > Default apps > ZXraw Explorer."
        )),
    }
}

/// Re-points the registration at this executable after it moved, if the user opted in.
pub fn refresh_registration() {
    let current_exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::warn!("could not determine the executable path for file types: {error}");
            return;
        }
    };
    match forge_win::refresh_file_types_if_registered(&current_exe, &file_types()) {
        Ok(true) => tracing::info!("refreshed the photo file-type registration"),
        Ok(false) => {}
        Err(error) => tracing::warn!("could not refresh the photo file-type registration: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offered_types_are_viewable_and_described() {
        let types = file_types();
        let extensions: Vec<&str> = types.iter().map(|t| t.extension.as_str()).collect();
        for expected in ["jpg", "png", "heic", "cr3", "nef", "arw", "dng", "raf"] {
            assert!(extensions.contains(&expected), "{expected} offered");
        }
        for skipped in NOT_VIEWABLE {
            assert!(!extensions.contains(skipped), "{skipped} not offered");
        }
        let description = |extension: &str| {
            types
                .iter()
                .find(|t| t.extension == extension)
                .map(|t| t.description.clone())
        };
        assert_eq!(description("jpeg").as_deref(), Some("JPEG image"));
        assert_eq!(description("png").as_deref(), Some("PNG image"));
        assert_eq!(description("cr3").as_deref(), Some("CR3 raw image"));
    }
}
