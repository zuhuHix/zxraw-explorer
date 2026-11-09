//! Settings > Default app on Linux: folders, photos and archives through the freedesktop
//! MIME Apps mechanism (`forge_win::xdg_apps`), and the applications menu entry that an
//! AppImage or a build folder needs before the desktop can open anything with it.

use std::path::{Path, PathBuf};

use egui::RichText;
use forge_win::xdg_apps::{self, Launcher, MenuEntry, XdgDirs};

use super::{Action, State, brief_error};
use crate::app::ZxrawApp;

/// The folder type.
pub const FOLDER_MIME_TYPES: &[&str] = &["inode/directory"];

/// The photo and RAW types the viewer opens (`file_types()`), by their shared-mime-info
/// names. Extensions with no shared-mime-info type are left out. The same list is in
/// `packaging/linux/zxraw.desktop`.
pub const PHOTO_MIME_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/tiff",
    "image/webp",
    "image/gif",
    "image/bmp",
    "image/heic",
    "image/heif",
    "image/avif",
    "image/x-canon-cr2",
    "image/x-canon-cr3",
    "image/x-nikon-nef",
    "image/x-nikon-nrw",
    "image/x-sony-arw",
    "image/x-sony-srf",
    "image/x-sony-sr2",
    "image/x-fuji-raf",
    "image/x-olympus-orf",
    "image/x-panasonic-rw2",
    "image/x-adobe-dng",
    "image/x-pentax-pef",
];

/// `(extension, type)` for the archive extensions (`ARCHIVE_EXTENSIONS`) that have a
/// shared-mime-info type. `.br` has none.
pub const ARCHIVE_MIME_TYPES: &[(&str, &str)] = &[
    ("zip", "application/zip"),
    ("7z", "application/x-7z-compressed"),
    ("rar", "application/vnd.rar"),
    ("tar", "application/x-tar"),
    ("tgz", "application/x-compressed-tar"),
    ("tbz2", "application/x-bzip2-compressed-tar"),
    ("tbz", "application/x-bzip1-compressed-tar"),
    ("txz", "application/x-xz-compressed-tar"),
    ("tzst", "application/x-zstd-compressed-tar"),
    ("tlz4", "application/x-lz4-compressed-tar"),
    ("gz", "application/gzip"),
    ("bz2", "application/x-bzip2"),
    ("xz", "application/x-xz"),
    ("zst", "application/zstd"),
    ("lz4", "application/x-lz4"),
];

/// Every type the menu entry lists (its `MimeType`), in the packaged entry's order.
pub fn menu_mime_types() -> Vec<&'static str> {
    PHOTO_MIME_TYPES
        .iter()
        .copied()
        .chain(ARCHIVE_MIME_TYPES.iter().map(|(_, mime)| *mime))
        .chain(FOLDER_MIME_TYPES.iter().copied())
        .collect()
}

/// A switch on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Folders,
    Photos,
    Archives,
}

impl Group {
    pub const ALL: [Group; 3] = [Group::Folders, Group::Photos, Group::Archives];

    pub fn mime_types(self) -> Vec<&'static str> {
        match self {
            Group::Folders => FOLDER_MIME_TYPES.to_vec(),
            Group::Photos => PHOTO_MIME_TYPES.to_vec(),
            Group::Archives => ARCHIVE_MIME_TYPES.iter().map(|(_, mime)| *mime).collect(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Group::Folders => "Open folders with ZXraw",
            Group::Photos => "Open photos and RAW files with ZXraw",
            Group::Archives => "Open archives with ZXraw",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Group::Folders => {
                "Apps that ask the desktop which app opens folders (\"Show in folder\", a folder on the desktop, a mounted drive) open ZXraw Explorer. The desktop's own Files launcher and shortcuts still open its file manager."
            }
            Group::Photos => "JPEG, PNG, TIFF, WebP, HEIC, AVIF and the camera RAW types the viewer opens.",
            Group::Archives => {
                ".zip, .7z, .rar, .tar and the compressed tars, .gz, .bz2, .xz, .zst, .lz4. An archive opened this way opens as a folder in a tab."
            }
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Group::Folders => "Folders",
            Group::Photos => "Photos and RAW files",
            Group::Archives => "Archives",
        }
    }
}

/// What the user asked for on the Linux page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxAction {
    /// Turn a switch on (true) or off.
    Group(Group, bool),
    /// Write (or rewrite, after a move) the applications menu entry.
    AddMenuEntry,
    /// Restore every default, then remove the menu entry.
    RemoveMenuEntry,
}

/// One type and the app that opens it.
#[derive(Debug, Clone)]
pub struct TypeStatus {
    pub mime: &'static str,
    pub app: Option<String>,
    pub ours: bool,
}

#[derive(Debug, Clone)]
pub struct LinuxStatus {
    pub menu: MenuEntry,
    /// Where the entry should point: the AppImage file, else this executable.
    pub launcher: Launcher,
    /// One per [`Group::ALL`].
    pub groups: Vec<Vec<TypeStatus>>,
}

impl LinuxStatus {
    pub fn check() -> Option<Self> {
        let dirs = XdgDirs::from_env()?;
        let launcher = xdg_apps::launcher();
        // With no usable path, any entry of ours counts as out of date.
        let exe = match &launcher {
            Launcher::Path(exe) => exe.as_path(),
            Launcher::AppImageGone(_) | Launcher::Unknown => Path::new(""),
        };
        let menu = xdg_apps::menu_entry(&dirs, exe);
        let groups = Group::ALL
            .iter()
            .map(|group| {
                let mimes = group.mime_types();
                xdg_apps::defaults_for(&dirs, &mimes)
                    .into_iter()
                    .zip(mimes)
                    .map(|(handler, mime)| TypeStatus {
                        mime,
                        ours: handler.as_ref().is_some_and(|h| h.is_zxraw()),
                        app: handler.map(|h| h.name),
                    })
                    .collect()
            })
            .collect();
        Some(Self {
            menu,
            launcher,
            groups,
        })
    }

    fn group(&self, group: Group) -> &[TypeStatus] {
        let index = Group::ALL.iter().position(|g| *g == group).unwrap_or(0);
        self.groups.get(index).map(Vec::as_slice).unwrap_or(&[])
    }

    /// A switch is on when every type in it opens with ZXraw Explorer.
    pub fn is_on(&self, group: Group) -> bool {
        let types = self.group(group);
        !types.is_empty() && types.iter().all(|t| t.ours)
    }
}

/// The Linux page: the menu entry first when it is needed, then one switch per group.
pub fn controls(ui: &mut egui::Ui, state: &mut State, compact: bool, action: &mut Option<Action>) {
    let ctx = ui.ctx().clone();
    let Some(status) = state.current(&ctx) else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Checking current defaults…");
        });
        return;
    };
    let Some(status) = &status.linux else {
        ui.label("The desktop's default apps could not be read here (no home folder).");
        return;
    };
    let mut send = |a: LinuxAction| *action = Some(Action::Linux(a));
    // The AppImage moved while running: the new path is unknown until it is started again.
    let moved_while_running = matches!(status.launcher, Launcher::AppImageGone(_));
    let warn = |ui: &mut egui::Ui, text: &str| {
        ui.label(RichText::new(text).color(ui.visuals().warn_fg_color));
    };

    match &status.menu {
        MenuEntry::Missing => {
            ui.label("ZXraw Explorer is not in the applications menu yet, so the desktop cannot open files with it. Add it first.");
            if moved_while_running {
                warn(ui, MOVED_WHILE_RUNNING);
            }
            if ui
                .add_enabled(
                    !moved_while_running,
                    egui::Button::new("Add ZXraw to the applications menu"),
                )
                .on_hover_text(
                    "Writes ~/.local/share/applications/zxraw.desktop and the icon. It starts this copy of ZXraw Explorer.",
                )
                .clicked()
            {
                send(LinuxAction::AddMenuEntry);
            }
            ui.add_space(6.0);
        }
        MenuEntry::Ours { path, current } => {
            if moved_while_running {
                warn(ui, MOVED_WHILE_RUNNING);
            } else if !current {
                warn(ui, "ZXraw moved; update the menu entry");
            }
            ui.horizontal(|ui| {
                if !current
                    && ui
                        .add_enabled(!moved_while_running, egui::Button::new("Update the menu entry"))
                        .clicked()
                {
                    send(LinuxAction::AddMenuEntry);
                }
                if !compact
                    && ui
                        .button("Remove from the applications menu")
                        .on_hover_text(format!(
                            "Puts back the previous apps for every type below, then deletes {}.",
                            path.display()
                        ))
                        .clicked()
                {
                    send(LinuxAction::RemoveMenuEntry);
                }
            });
            if let (true, Launcher::Path(launcher)) = (*current && !compact, &status.launcher) {
                ui.label(
                    RichText::new(format!("In the applications menu; starts {}", launcher.display()))
                        .weak(),
                );
            }
            ui.add_space(6.0);
        }
        MenuEntry::Installed(_) => {}
    }

    let ready = !matches!(status.menu, MenuEntry::Missing);
    for group in Group::ALL {
        let on = status.is_on(group);
        let mut checked = on;
        let types = status.group(group);
        let ours = types.iter().filter(|t| t.ours).count();
        ui.horizontal(|ui| {
            let response = ui
                .add_enabled(ready, egui::Checkbox::new(&mut checked, group.label()))
                .on_hover_text(group.hint());
            if response.changed() && checked != on {
                send(LinuxAction::Group(group, checked));
            }
            if !on && ours > 0 {
                ui.label(RichText::new(format!("{ours}/{} types", types.len())).weak());
            }
        });
    }
    if compact {
        return;
    }
    ui.add_space(6.0);
    for group in Group::ALL {
        type_list(ui, group, status.group(group));
    }
}

fn type_list(ui: &mut egui::Ui, group: Group, types: &[TypeStatus]) {
    let id = format!("default-app-linux-{group:?}");
    egui::CollapsingHeader::new(format!("{} ({})", group.noun(), types.len()))
        .id_salt(&id)
        .show(ui, |ui| {
            egui::Grid::new(format!("{id}-grid"))
                .num_columns(2)
                .spacing([16.0, 2.0])
                .striped(true)
                .show(ui, |ui| {
                    for t in types {
                        ui.label(RichText::new(t.mime).monospace());
                        let app = t.app.as_deref().unwrap_or("System default");
                        if t.ours {
                            ui.label(RichText::new(app).strong());
                        } else {
                            ui.label(RichText::new(app).weak());
                        }
                        ui.end_row();
                    }
                });
        });
}

/// Where the previous defaults are kept until a switch is turned off.
pub fn record_path(config: &Path) -> PathBuf {
    config.join("linux-default-apps.tsv")
}

/// Shown when `$APPIMAGE` no longer exists.
const MOVED_WHILE_RUNNING: &str =
    "ZXraw was moved while running: start it again from its new location, then update the menu entry";

/// The toast for a record file that was refused (`xdg_apps::is_damaged_record`).
const DAMAGED_RECORD: &str = "Can't restore previous defaults; set them in System Settings";

/// The icon sizes in `packaging/linux/icons/hicolor`.
#[cfg(test)]
const ICON_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn icons() -> Vec<(u32, &'static [u8])> {
    macro_rules! icon {
        ($n:literal) => {
            include_bytes!(concat!(
                "../../../packaging/linux/icons/hicolor/",
                $n,
                "x",
                $n,
                "/apps/zxraw.png"
            ))
            .as_slice()
        };
    }
    vec![
        (16, icon!("16")),
        (24, icon!("24")),
        (32, icon!("32")),
        (48, icon!("48")),
        (64, icon!("64")),
        (128, icon!("128")),
        (256, icon!("256")),
    ]
}

/// Carries out a [`LinuxAction`]. Returns the toast text, or the error.
pub fn run(config: &Path, action: LinuxAction) -> Result<String, String> {
    let dirs = XdgDirs::from_env().ok_or("The desktop's default apps could not be read here")?;
    let record = record_path(config);
    // The full error goes to the log (see `apply`); these are what the user reads.
    let explain = |what: &str| {
        let what = what.to_string();
        move |error: forge_win::WinError| {
            tracing::warn!("{what}: {error}");
            if xdg_apps::is_damaged_record(&error) {
                DAMAGED_RECORD.to_string()
            } else if xdg_apps::is_changed_by_another_program(&error) {
                "Settings changed by another program; try again".to_string()
            } else {
                format!("{what}: {error}")
            }
        }
    };
    match action {
        LinuxAction::Group(group, true) => {
            xdg_apps::set_defaults(&dirs, &group.mime_types(), &record)
                .map_err(explain("Could not set the default"))?;
            Ok(format!("{} now open with ZXraw Explorer", group.noun()))
        }
        LinuxAction::Group(group, false) => {
            xdg_apps::restore_defaults(&dirs, &group.mime_types(), &record)
                .map_err(explain("Could not restore the previous app"))?;
            Ok(format!("{}: previous apps restored", group.noun()))
        }
        LinuxAction::AddMenuEntry => {
            let launcher = match xdg_apps::launcher() {
                Launcher::Path(path) => path,
                Launcher::AppImageGone(_) => return Err(MOVED_WHILE_RUNNING.into()),
                Launcher::Unknown => return Err("Could not find this program's path".into()),
            };
            xdg_apps::install_menu_entry(&dirs, &launcher, &menu_mime_types(), &icons())
                .map_err(explain("Could not add the menu entry"))?;
            Ok("ZXraw Explorer added to the applications menu".into())
        }
        LinuxAction::RemoveMenuEntry => {
            // Nothing may be left pointing at the entry about to go.
            xdg_apps::restore_defaults(&dirs, &menu_mime_types(), &record)
                .map_err(explain("Could not restore the previous apps"))?;
            xdg_apps::remove_menu_entry(&dirs, &icons())
                .map_err(explain("Could not remove the menu entry"))?;
            Ok("ZXraw Explorer removed from the applications menu; previous apps restored".into())
        }
    }
}

/// Automated runs change defaults only when both XDG homes are inside the test's sandbox, so
/// a test can never touch the user's real `mimeapps.list` or applications menu.
pub fn automation_may_change_defaults() -> bool {
    let Some(sandbox) = std::env::var_os("ZXRAW_SCRIPT_SANDBOX").map(PathBuf::from) else {
        return false;
    };
    let Ok(sandbox) = std::fs::canonicalize(&sandbox) else {
        return false;
    };
    ["XDG_CONFIG_HOME", "XDG_DATA_HOME"].iter().all(|name| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .and_then(|dir| {
                std::fs::create_dir_all(&dir).ok()?;
                std::fs::canonicalize(dir).ok()
            })
            .is_some_and(|dir| dir.starts_with(&sandbox))
    })
}

pub(super) fn apply(app: &mut ZxrawApp, action: LinuxAction) {
    match run(&app.paths.config, action) {
        Ok(message) => app.toast(message),
        Err(error) => {
            tracing::warn!("default app update failed: {error}");
            app.toast_err(brief_error(&[error]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_entry_lists_the_same_types() {
        let entry = include_str!("../../../packaging/linux/zxraw.desktop");
        let line = entry
            .lines()
            .find_map(|l| l.strip_prefix("MimeType="))
            .expect("MimeType line");
        let packaged: Vec<&str> = line.split(';').filter(|m| !m.is_empty()).collect();
        assert_eq!(packaged, menu_mime_types());
        // The generated entry carries everything the packaged one has, except Exec and Icon.
        let generated = xdg_apps::desktop_entry_text(Path::new("/x/ZXraw.AppImage"), &menu_mime_types());
        for line in entry.lines().filter(|l| l.contains('=') && !l.starts_with('#')) {
            // Exec is the AppImage's path; Icon is ZXraw's own name, never a package's `zxraw`.
            if !line.starts_with("Exec=") && !line.starts_with("Icon=") {
                assert!(generated.lines().any(|g| g == line), "{line} missing from the generated entry");
            }
        }
    }

    #[test]
    fn archive_types_are_offered_archive_extensions_that_open() {
        for (extension, _) in ARCHIVE_MIME_TYPES {
            assert!(super::super::ARCHIVE_EXTENSIONS.contains(extension), "{extension}");
            let name = PathBuf::from(format!("x.{extension}"));
            assert!(forge_core::archive::is_archive_name(&name), "{extension} opens");
        }
    }

    #[test]
    fn icons_cover_every_size() {
        let sizes: Vec<u32> = icons().iter().map(|(size, _)| *size).collect();
        assert_eq!(sizes, ICON_SIZES);
        assert!(icons().iter().all(|(_, png)| png.starts_with(b"\x89PNG")));
    }
}
