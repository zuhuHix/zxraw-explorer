//! The Explorer menu's commands and their command-line forms, run in the window:
//! `zxraw.exe --extract-here <archives>`, `--extract-to`, `--extract-dialog`,
//! `--open-archive`, `--add-dialog`, `--add-zip` and `--add-7z` `<files and folders>`.
//!
//! They arrive from the menu's COM server (`forge_win::shell_verb_server`), from a command
//! line, or from another launch over the instance pipe (see `instance`), always with the
//! whole selection at once, and run the same jobs as the app's own commands: Extract here
//! uses the in-app smart-folder rule and asks about existing files the same way, and the
//! progress and errors show in this window.

use std::path::{Path, PathBuf};

use forge_core::archive::{self, ArchiveFormat};
use forge_win::MenuVerb;

use crate::app::ZxrawApp;
use crate::archive_extract::{ExtractRequest, Place};

/// The verb a command-line switch names (`--extract-here` …), if any.
pub fn verb_of_switch(argument: &std::ffi::OsStr) -> Option<MenuVerb> {
    MenuVerb::from_name(argument.to_str()?.strip_prefix("--")?)
}

/// Runs `verb` on `paths` (absolute, as Explorer or the pipe hands them over).
pub fn run(app: &mut ZxrawApp, ctx: &egui::Context, verb: MenuVerb, paths: Vec<PathBuf>) {
    tracing::info!("running {} on {} item(s)", verb.name(), paths.len());
    if paths.is_empty() {
        return;
    }
    match verb {
        MenuVerb::Open => {
            for path in archives(app, &paths) {
                app.open_launch_path(ctx, path.as_os_str());
            }
        }
        MenuVerb::ExtractHere | MenuVerb::ExtractToFolder => {
            let place = if verb == MenuVerb::ExtractHere { Place::Smart } else { Place::Named };
            for archive in archives(app, &paths) {
                let parent = parent_of(&archive);
                extract(app, archive, parent, place);
            }
        }
        MenuVerb::ExtractDialog => {
            let found = archives(app, &paths);
            let Some(first) = found.first() else {
                return;
            };
            let picked = crate::dialogs::pick_folder(
                rfd::FileDialog::new()
                    .set_title("Extract to")
                    .set_directory(parent_of(first)),
            );
            let Some(folder) = picked else {
                return;
            };
            for archive in found {
                extract(app, archive, folder.clone(), Place::Into);
            }
        }
        MenuVerb::AddDialog | MenuVerb::AddZip | MenuVerb::Add7z => {
            let sources: Vec<PathBuf> = paths
                .into_iter()
                .filter(|p| {
                    let exists = p.exists();
                    if !exists {
                        tracing::warn!("not adding a missing item: {}", p.display());
                    }
                    exists
                })
                .collect();
            if sources.is_empty() {
                app.toast_err("The selected items no longer exist");
                return;
            }
            let (directory, stem) = archive_place(&sources);
            match verb {
                MenuVerb::AddDialog => crate::extra_tools_ui::open_create_dialog(
                    app,
                    sources,
                    directory,
                    format!("{stem}.zip"),
                ),
                MenuVerb::AddZip => crate::extra_tools_ui::quick_create_archive(
                    app,
                    sources,
                    directory,
                    format!("{stem}.zip"),
                    ArchiveFormat::Zip,
                ),
                _ => crate::extra_tools_ui::quick_create_archive(
                    app,
                    sources,
                    directory,
                    format!("{stem}.7z"),
                    ArchiveFormat::SevenZip,
                ),
            }
        }
    }
}

fn extract(app: &mut ZxrawApp, archive: PathBuf, parent: PathBuf, place: Place) {
    crate::archive_extract::start_extract(
        app,
        ExtractRequest {
            archive,
            parent,
            place,
            selection: None,
            policy: None,
            allow_unchecked: false,
            then_remove: None,
        },
    );
}

/// The archive files among `paths`; anything else is named in one error toast.
fn archives(app: &mut ZxrawApp, paths: &[PathBuf]) -> Vec<PathBuf> {
    let (found, other): (Vec<PathBuf>, Vec<PathBuf>) = paths
        .iter()
        .cloned()
        .partition(|p| archive::is_archive_name(p) && p.is_file());
    if let Some(first) = other.first() {
        let name = forge_core::paths::name_of(first);
        app.toast_err(if other.len() == 1 {
            format!("Not an archive ZXraw can open: {name}")
        } else {
            format!("Not archives ZXraw can open: {name} and {} more", other.len() - 1)
        });
    }
    found
}

fn parent_of(path: &Path) -> PathBuf {
    path.parent().unwrap_or(Path::new(".")).to_path_buf()
}

/// Where "Add to <name>.zip" puts the archive and its name without extension: next to the
/// first item; named after the item when there is one (a file without its extension), else
/// after the folder the items are in ("Archive" at a drive root).
pub fn archive_place(sources: &[PathBuf]) -> (PathBuf, String) {
    let first = &sources[0];
    let directory = parent_of(first);
    let stem = if sources.len() == 1 {
        let is_dir = first.is_dir();
        let name = forge_core::paths::name_of(first);
        if is_dir {
            name
        } else {
            match name.rsplit_once('.') {
                Some((stem, _)) if !stem.is_empty() => stem.to_string(),
                _ => name,
            }
        }
    } else {
        directory
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Archive".to_string())
    };
    (directory, stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_name_the_menu_verbs() {
        assert_eq!(verb_of_switch("--extract-here".as_ref()), Some(MenuVerb::ExtractHere));
        assert_eq!(verb_of_switch("--add-7z".as_ref()), Some(MenuVerb::Add7z));
        assert_eq!(verb_of_switch("extract-here".as_ref()), None);
        assert_eq!(verb_of_switch("--new-window".as_ref()), None);
    }

    #[test]
    fn add_names_follow_the_item_or_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let shoot = dir.path().join("Shoot 2026");
        std::fs::create_dir_all(shoot.join("raw")).unwrap();
        std::fs::write(shoot.join("a.tar.gz"), b"x").unwrap();
        std::fs::write(shoot.join("photo.jpg"), b"x").unwrap();
        std::fs::write(shoot.join(".hidden"), b"x").unwrap();

        assert_eq!(archive_place(&[shoot.join("photo.jpg")]), (shoot.clone(), "photo".into()));
        assert_eq!(archive_place(&[shoot.join("a.tar.gz")]), (shoot.clone(), "a.tar".into()));
        assert_eq!(archive_place(&[shoot.join(".hidden")]), (shoot.clone(), ".hidden".into()));
        assert_eq!(archive_place(&[shoot.join("raw")]), (shoot.clone(), "raw".into()));
        assert_eq!(
            archive_place(&[shoot.join("photo.jpg"), shoot.join("raw")]),
            (shoot.clone(), "Shoot 2026".into())
        );
        assert_eq!(
            archive_place(&[PathBuf::from(r"C:\a.txt"), PathBuf::from(r"C:\b.txt")]).1,
            "Archive"
        );
    }
}
