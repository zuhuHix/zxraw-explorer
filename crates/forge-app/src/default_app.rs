//! One-click photo and RAW defaults, with optional filesystem-folder defaults, offered by the
//! first-run welcome and Settings > Default app. After the user's explicit click, the Windows
//! integration registers photo types and writes per-user `UserChoice` values, then applies the
//! reversible `Directory` and `Drive` shell verbs when selected.
//!
//! Settings > Default app also has the archive integration: the archive types (offered in
//! "Open with" and on ZXraw Explorer's Windows Settings page, where the user confirms the
//! default: archive `UserChoice` values are never written), and the "ZXraw" menu in
//! Explorer's right-click menu (`forge_win::archive_menu`). Each applies when toggled.

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use egui::RichText;
use forge_core::model::{IMAGE_EXTS, RAW_EXTS};

use crate::app::ZxrawApp;

/// Image types the viewer cannot decode; they keep opening in their current apps.
const NOT_VIEWABLE: &[&str] = &["psd", "jxl", "ico", "crw", "x3f", "mrw"];

/// How long a status check stays fresh while it is on screen. The user changes defaults in
/// Settings, outside the app, so the page keeps re-checking.
const RECHECK: Duration = Duration::from_secs(1);

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

/// Archive extensions ZXraw Explorer registers for, without the dot. `.tar.gz` and the other
/// compressed tars are covered by their last extension (`.gz` …).
pub const ARCHIVE_EXTENSIONS: &[&str] = &[
    "zip", "7z", "rar", "tar", "tgz", "tbz2", "tbz", "txz", "tzst", "tlz4", "gz", "bz2", "xz",
    "zst", "lz4", "br",
];

pub fn archive_extensions() -> Vec<&'static str> {
    ARCHIVE_EXTENSIONS.to_vec()
}

/// The archive types offered, each with its Explorer type name.
pub fn archive_file_types() -> Vec<forge_win::FileType> {
    ARCHIVE_EXTENSIONS
        .iter()
        .map(|&extension| forge_win::FileType {
            extension: extension.to_string(),
            description: describe_archive(extension).to_string(),
        })
        .collect()
}

fn describe_archive(extension: &str) -> &'static str {
    match extension {
        "zip" => "ZIP archive",
        "7z" => "7z archive",
        "rar" => "RAR archive",
        "tar" => "Tar archive",
        "tgz" => "Gzip-compressed tar archive",
        "tbz2" | "tbz" => "Bzip2-compressed tar archive",
        "txz" => "XZ-compressed tar archive",
        "tzst" => "Zstandard-compressed tar archive",
        "tlz4" => "LZ4-compressed tar archive",
        "gz" => "Gzip-compressed file",
        "bz2" => "Bzip2-compressed file",
        "xz" => "XZ-compressed file",
        "zst" => "Zstandard-compressed file",
        "lz4" => "LZ4-compressed file",
        "br" => "Brotli-compressed file",
        _ => "Archive",
    }
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
    /// Whether Directory and Drive open with ZXraw Explorer.
    pub folders: bool,
    /// `(extension, app name)`, and whether that app is ZXraw Explorer.
    pub types: Vec<(String, Option<String>, bool)>,
    /// ZXraw Explorer is registered for the archive types.
    pub archives_registered: bool,
    /// As `types`, for the archive types.
    pub archive_types: Vec<(String, Option<String>, bool)>,
    /// The ZXraw menu is in Explorer's right-click menu.
    pub archive_menu: bool,
}

fn handlers(types: Vec<forge_win::FileType>) -> Vec<(String, Option<String>, bool)> {
    types
        .into_iter()
        .map(|file_type| {
            let app = forge_win::default_app(&file_type.extension);
            let ours = app.as_ref().is_some_and(|app| app.is_zxraw);
            (file_type.extension, app.map(|app| app.name), ours)
        })
        .collect()
}

impl Status {
    fn check() -> Self {
        Self {
            registered: forge_win::file_types_registered().unwrap_or(false),
            folders: forge_win::default_folder_handler_enabled().unwrap_or(false),
            types: handlers(file_types()),
            archives_registered: forge_win::archive_types_registered().unwrap_or(false),
            archive_types: handlers(archive_file_types()),
            archive_menu: forge_win::archive_menu_registered().unwrap_or(false),
        }
    }

    pub fn ours(&self) -> usize {
        self.types.iter().filter(|(_, _, ours)| *ours).count()
    }

    pub fn archives_ours(&self) -> usize {
        self.archive_types.iter().filter(|(_, _, ours)| *ours).count()
    }
}

/// Background-refreshed [`Status`] for whichever window shows it.
pub struct State {
    status: Option<Status>,
    checked: Option<Instant>,
    rx: Option<Receiver<Status>>,
    pub photos_selected: bool,
    pub folders_selected: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            status: None,
            checked: None,
            rx: None,
            photos_selected: true,
            folders_selected: true,
        }
    }
}

impl State {
    /// Latest status, re-checked off the UI thread while it is being shown. The short poll keeps
    /// this current while Windows Settings is in front and after the user returns to the app.
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
    Apply { photos: bool, folders: bool },
    OpenWindowsSettings,
    RestorePreviousApps,
    /// Register (true) or remove the archive types.
    ArchiveTypes(bool),
    /// Register the archive types if needed and open Windows Settings to confirm them.
    ArchiveDefaults,
    /// Add (true) or remove the ZXraw menu in Explorer's right-click menu.
    ArchiveMenu(bool),
}

/// Status line plus buttons. `compact` (the welcome) leaves out the per-type list and
/// "Stop using".
pub fn controls(ui: &mut egui::Ui, state: &mut State, compact: bool, action: &mut Option<Action>) {
    let ctx = ui.ctx().clone();
    ui.checkbox(
        &mut state.photos_selected,
        "Open photo and RAW files in ZXraw Explorer",
    );
    ui.checkbox(
        &mut state.folders_selected,
        "Open folders in ZXraw Explorer",
    )
    .on_hover_text(
        "Changes the Directory and Drive open verbs. Win+E, the taskbar File Explorer icon, and other apps' Show in folder commands still open Windows Explorer.",
    );
    ui.add_space(4.0);
    let photos_selected = state.photos_selected;
    let folders_selected = state.folders_selected;
    let Some(status) = state.current(&ctx) else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Checking current defaults…");
        });
        return;
    };
    let total = status.types.len();
    if status.ours() == total && status.folders {
        ui.label("ZXraw Explorer opens your photos, RAW files and folders.");
    } else if status.ours() > 0 || status.folders {
        ui.label("ZXraw Explorer defaults are partially applied.");
    } else {
        ui.label("Choose which files and folders ZXraw Explorer should open.");
    }
    ui.horizontal(|ui| {
        ui.label(format!("Photos: {}/{} types", status.ours(), total));
        ui.label(format!(
            "Folders: {}",
            if status.folders { "yes" } else { "no" }
        ));
    });
    ui.horizontal(|ui| {
        if ui
            .button("Make ZXraw Explorer my default")
            .on_hover_text("Applies the selected options. Uncheck either option and click again to restore that part's previous app.")
            .clicked()
        {
            *action = Some(Action::Apply {
                photos: photos_selected,
                folders: folders_selected,
            });
        }
        if status.registered
            && status.ours() < total
            && ui
                .button("Open Windows Settings for remaining photo types…")
                .on_hover_text("Use the ZXraw Explorer Default apps page only for the types Windows did not accept here.")
                .clicked()
        {
            *action = Some(Action::OpenWindowsSettings);
        }
        if (status.registered || status.ours() > 0 || status.folders)
            && ui.button("Restore previous apps").clicked()
        {
            *action = Some(Action::RestorePreviousApps);
        }
    });
    if compact {
        return;
    }
    ui.add_space(6.0);
    type_list(ui, "Photo types", "default-app-types", &status.types);
    archive_controls(ui, status, action);
}

/// The archive part of Settings > Default app.
fn archive_controls(ui: &mut egui::Ui, status: &Status, action: &mut Option<Action>) {
    ui.add_space(14.0);
    ui.label(RichText::new("Archives").strong());
    let mut offered = status.archives_registered;
    if ui
        .checkbox(
            &mut offered,
            "Offer ZXraw Explorer for archives (.zip, .7z, .rar, .tar, .gz and more)",
        )
        .on_hover_text(
            "Adds ZXraw Explorer to \"Open with\" and to its page in Windows Settings > Default apps. An archive opened with it opens as a folder in a tab of the running window.",
        )
        .changed()
    {
        *action = Some(Action::ArchiveTypes(offered));
    }
    let total = status.archive_types.len();
    ui.horizontal(|ui| {
        ui.label(format!(
            "Archives: {}/{} types open in ZXraw Explorer",
            status.archives_ours(),
            total
        ));
        if ui
            .button("Make ZXraw Explorer the default for archives…")
            .on_hover_text(
                "Windows asks you to confirm archive defaults yourself: on the ZXraw Explorer page that opens in Settings, choose it for each archive type.",
            )
            .clicked()
        {
            *action = Some(Action::ArchiveDefaults);
        }
    });
    let mut menu = status.archive_menu;
    if ui
        .checkbox(
            &mut menu,
            "Show the ZXraw menu when right-clicking archives, files and folders",
        )
        .on_hover_text(
            "Open in ZXraw, Extract here, Extract to…, Add to archive…, Add to .zip or .7z. On Windows 11 it is under \"Show more options\".",
        )
        .changed()
    {
        *action = Some(Action::ArchiveMenu(menu));
    }
    ui.add_space(6.0);
    type_list(ui, "Archive types", "default-app-archive-types", &status.archive_types);
}

fn type_list(ui: &mut egui::Ui, title: &str, id: &str, types: &[(String, Option<String>, bool)]) {
    egui::CollapsingHeader::new(format!("{title} ({})", types.len()))
        .id_salt(id)
        .show(ui, |ui| {
            egui::Grid::new(format!("{id}-grid"))
                .num_columns(2)
                .spacing([16.0, 2.0])
                .striped(true)
                .show(ui, |ui| {
                    for (extension, app, ours) in types {
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
        Action::Apply { photos, folders } => apply_selected(app, photos, folders),
        Action::OpenWindowsSettings => open_windows_settings(app),
        Action::RestorePreviousApps => restore_previous_apps(app),
        Action::ArchiveTypes(on) => set_archive_types(app, on),
        Action::ArchiveDefaults => archive_defaults(app),
        Action::ArchiveMenu(on) => set_archive_menu(app, on),
    }
    app.ops.settings_ui.default_app.invalidate();
}

/// Applies the selected defaults. Windows 10/11 blocks programmatic `UserChoice` writes, so some
/// photo types not being accepted is expected: they are counted for a neutral notice, and the
/// raw per-type detail goes to the log only.
fn apply_selected(app: &mut ZxrawApp, photos: bool, folders: bool) {
    let mut errors = Vec::new();
    let mut needs_windows = 0;
    if photos {
        let registered = std::env::current_exe()
            .map_err(|error| error.to_string())
            .and_then(|exe| {
                forge_win::register_file_types(exe, &file_types())
                    .map_err(|error| error.to_string())
            });
        match registered {
            Ok(()) => match forge_win::set_file_type_defaults(&file_types()) {
                Ok(report) if report.all_succeeded() => {
                    tracing::info!(
                        "set ZXraw as the default for {} photo types",
                        report.succeeded()
                    );
                }
                Ok(report) => {
                    needs_windows = report.failed().count();
                    tracing::warn!(
                        "Windows did not accept {needs_windows} photo type defaults: {}",
                        report.failure_summary()
                    );
                }
                Err(error) => errors.push(format!("Could not set photo defaults: {error}")),
            },
            Err(error) => errors.push(format!("Could not register photo types: {error}")),
        }
    } else {
        match forge_win::restore_file_type_defaults() {
            Ok(report) if report.all_succeeded() => match forge_win::unregister_file_types() {
                Ok(()) => app.toast("Previous photo apps restored"),
                Err(error) => errors.push(format!("Could not remove photo registration: {error}")),
            },
            Ok(report) => {
                tracing::warn!("could not restore photo apps: {}", report.failure_summary());
                errors.push(format!(
                    "Some photo apps could not be restored; registration was kept ({} types)",
                    report.failed().count()
                ));
            }
            Err(error) => errors.push(format!("Could not restore photo apps: {error}")),
        }
    }

    if folders {
        let result = std::env::current_exe()
            .map_err(|error| error.to_string())
            .and_then(|exe| {
                forge_win::register_default_folder_handler(exe).map_err(|error| error.to_string())
            });
        if let Err(error) = result {
            errors.push(format!("Could not set folder defaults: {error}"));
        }
    } else if let Err(error) = forge_win::unregister_default_folder_handler() {
        errors.push(format!("Could not restore folder opening: {error}"));
    }

    if !errors.is_empty() {
        tracing::warn!("default app update failed: {}", errors.join("; "));
        app.toast_err(brief_error(&errors));
    } else if needs_windows > 0 {
        app.toast(format!(
            "Windows needs you to confirm {needs_windows} photo types in Settings. Use \"Open Windows Settings\" below."
        ));
    } else {
        app.toast("ZXraw Explorer default updated");
    }
}

/// One short line for the user; the full text is in the log.
fn brief_error(errors: &[String]) -> String {
    let first = errors
        .first()
        .map(String::as_str)
        .unwrap_or("Unknown error");
    let mut text: String = first.chars().take(140).collect();
    if first.chars().count() > 140 {
        text.push('…');
    }
    if errors.len() > 1 {
        text.push_str(&format!(" (+{} more, see log)", errors.len() - 1));
    }
    text
}

fn restore_previous_apps(app: &mut ZxrawApp) {
    let mut errors = Vec::new();
    match forge_win::restore_file_type_defaults() {
        Ok(report) if report.all_succeeded() => {
            if let Err(error) = forge_win::unregister_file_types() {
                errors.push(format!("Could not remove photo registration: {error}"));
            }
        }
        Ok(report) => {
            tracing::warn!("could not restore photo apps: {}", report.failure_summary());
            errors.push(format!(
                "Some photo apps could not be restored; registration was kept ({} types)",
                report.failed().count()
            ));
        }
        Err(error) => errors.push(format!("Could not restore photo apps: {error}")),
    }
    if let Err(error) = forge_win::unregister_default_folder_handler() {
        errors.push(format!("Could not restore folder opening: {error}"));
    }
    if errors.is_empty() {
        app.toast("Previous photo apps and folder settings restored");
    } else {
        tracing::warn!("restore previous apps failed: {}", errors.join("; "));
        app.toast_err(brief_error(&errors));
    }
}

fn open_windows_settings(app: &mut ZxrawApp) {
    match forge_win::open_default_apps_settings() {
        Ok(()) => app.toast(
            "In Windows Settings, select only the photo types ZXraw could not set",
        ),
        Err(error) => app.toast_err(format!(
            "Could not open Windows Settings ({error}). Open Settings > Apps > Default apps > ZXraw Explorer."
        )),
    }
}
fn register_archives() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    forge_win::register_archive_types(exe, &archive_file_types()).map_err(|error| error.to_string())
}

fn set_archive_types(app: &mut ZxrawApp, on: bool) {
    let result = if on {
        register_archives()
    } else {
        forge_win::unregister_archive_types().map_err(|error| error.to_string())
    };
    match result {
        Ok(()) if on => app.toast("ZXraw Explorer is offered for archives in \"Open with\" and in Settings"),
        Ok(()) => app.toast("Archive types removed; their previous apps are back"),
        Err(error) => {
            tracing::warn!("archive type registration failed: {error}");
            app.toast_err(brief_error(&[format!("Could not change the archive types: {error}")]));
        }
    }
}

fn archive_defaults(app: &mut ZxrawApp) {
    if let Err(error) = register_archives() {
        tracing::warn!("archive type registration failed: {error}");
        app.toast_err(brief_error(&[format!("Could not register the archive types: {error}")]));
        return;
    }
    match forge_win::open_default_apps_settings() {
        Ok(()) => app.toast("In Windows Settings, choose ZXraw Explorer for .zip, .7z and the other archive types"),
        Err(error) => app.toast_err(format!(
            "Could not open Windows Settings ({error}). Open Settings > Apps > Default apps > ZXraw Explorer."
        )),
    }
}

fn set_archive_menu(app: &mut ZxrawApp, on: bool) {
    let result = if on {
        std::env::current_exe()
            .map_err(|error| error.to_string())
            .and_then(|exe| {
                forge_win::register_archive_menu(exe, ARCHIVE_EXTENSIONS)
                    .map_err(|error| error.to_string())
            })
    } else {
        forge_win::unregister_archive_menu().map_err(|error| error.to_string())
    };
    match result {
        Ok(()) if on => app.toast("ZXraw menu added to Explorer's right-click menu"),
        Ok(()) => app.toast("ZXraw menu removed from Explorer's right-click menu"),
        Err(error) => {
            tracing::warn!("archive menu registration failed: {error}");
            app.toast_err(brief_error(&[format!("Could not change the Explorer menu: {error}")]));
        }
    }
}

/// Re-points the archive types and the ZXraw menu at this executable after it moved or was
/// updated, for whichever the user turned on.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn refresh_archive_registration() {
    let current_exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::warn!("could not determine the executable path for archives: {error}");
            return;
        }
    };
    match forge_win::refresh_archive_types_if_registered(&current_exe, &archive_file_types()) {
        Ok(true) => tracing::info!("refreshed the archive file-type registration"),
        Ok(false) => {}
        Err(error) => tracing::warn!("could not refresh the archive file-type registration: {error}"),
    }
    match forge_win::refresh_archive_menu_if_registered(&current_exe, ARCHIVE_EXTENSIONS) {
        Ok(true) => tracing::info!("refreshed the Explorer archive menu"),
        Ok(false) => {}
        Err(error) => tracing::warn!("could not refresh the Explorer archive menu: {error}"),
    }
}

/// Re-points the registration at this executable after it moved, if the user opted in.
#[cfg_attr(not(windows), allow(dead_code))]
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

    #[test]
    fn every_archive_type_is_described_and_opens_as_an_archive() {
        for file_type in archive_file_types() {
            assert_ne!(file_type.description, "Archive", "{}", file_type.extension);
            let name = std::path::PathBuf::from(format!("x.{}", file_type.extension));
            // .rar opens once the RAR backend is in; every other type opens today.
            if file_type.extension != "rar" {
                assert!(forge_core::archive::is_archive_name(&name), "{}", file_type.extension);
            }
        }
        assert!(!archive_extensions().iter().any(|e| file_types().iter().any(|t| t.extension == *e)));
    }

}
