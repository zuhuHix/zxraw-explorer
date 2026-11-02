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

/// `names` as "a", "a and b", "a, b and c", or "a, b, c and N more".
pub fn names_text(names: &[String]) -> String {
    const SHOWN: usize = 3;
    let quoted: Vec<String> = names.iter().take(SHOWN).map(|n| format!("\"{n}\"")).collect();
    match (quoted.len(), names.len().saturating_sub(SHOWN)) {
        (0, _) => String::new(),
        (1, 0) => quoted[0].clone(),
        (n, 0) => format!("{} and {}", quoted[..n - 1].join(", "), quoted[n - 1]),
        (_, more) => format!("{} and {more} more", quoted.join(", ")),
    }
}

/// Why a menu command did not run: some selected items have no file system path.
pub fn unresolved_message(verb: MenuVerb, names: &[String]) -> String {
    format!(
        "\"{}\" did not run: {} {} not a file or folder on disk. Nothing was changed; select only files and folders and try again.",
        verb.label(),
        names_text(names),
        if names.len() == 1 { "is" } else { "are" },
    )
}

/// Why a menu command did not run: some items are gone (or, for the extract commands, are
/// not archives). `None` when all are usable.
fn refusal(verb: MenuVerb, paths: &[PathBuf]) -> Option<String> {
    let name = |p: &PathBuf| forge_core::paths::name_of(p);
    let missing: Vec<String> = paths.iter().filter(|p| !p.exists()).map(name).collect();
    if !missing.is_empty() {
        return Some(format!(
            "\"{}\" did not run: {} no longer {}. Nothing was changed.",
            verb.label(),
            names_text(&missing),
            if missing.len() == 1 { "exists" } else { "exist" },
        ));
    }
    if verb.on_everything() {
        return None;
    }
    let other: Vec<String> = paths
        .iter()
        .filter(|p| !(archive::is_archive_name(p) && p.is_file()))
        .map(name)
        .collect();
    (!other.is_empty()).then(|| {
        format!(
            "\"{}\" did not run: {} {} not an archive ZXraw can open. Nothing was changed.",
            verb.label(),
            names_text(&other),
            if other.len() == 1 { "is" } else { "are" },
        )
    })
}

/// Runs `verb` on `paths` (absolute, as Explorer or the pipe hands them over). The whole
/// selection or nothing: an item that is gone (or not an archive, for the extract
/// commands) refuses the command with a message naming it, rather than running it on the
/// rest.
pub fn run(app: &mut ZxrawApp, ctx: &egui::Context, verb: MenuVerb, paths: Vec<PathBuf>) {
    tracing::info!("running {} on {} item(s)", verb.name(), paths.len());
    if paths.is_empty() {
        return;
    }
    if let Some(why) = refusal(verb, &paths) {
        tracing::warn!("{why}");
        app.toast_err(why);
        return;
    }
    match verb {
        MenuVerb::Open => {
            for path in paths {
                app.open_launch_path(ctx, path.as_os_str());
            }
        }
        MenuVerb::ExtractHere | MenuVerb::ExtractToFolder => {
            let place = if verb == MenuVerb::ExtractHere { Place::Smart } else { Place::Named };
            for archive in paths {
                let parent = parent_of(&archive);
                extract(app, archive, parent, place);
            }
        }
        MenuVerb::ExtractDialog => {
            let found = paths;
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
            let sources = paths;
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
    fn a_selection_with_a_missing_or_unusable_item_is_refused_whole() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let zip = dir.path().join("pack.zip");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&zip, b"x").unwrap();
        let gone = dir.path().join("gone.txt");
        assert_eq!(refusal(MenuVerb::AddZip, &[a.clone(), zip.clone()]), None);
        let why = refusal(MenuVerb::AddZip, &[a.clone(), gone.clone()]).unwrap();
        assert!(why.contains("\"gone.txt\" no longer exists"), "{why}");
        assert!(why.contains("Nothing was changed"), "{why}");
        assert_eq!(refusal(MenuVerb::ExtractHere, &[zip.clone()]), None);
        let why = refusal(MenuVerb::ExtractHere, &[zip.clone(), a.clone()]).unwrap();
        assert!(why.contains("\"a.txt\" is not an archive"), "{why}");
        let why = refusal(MenuVerb::ExtractDialog, &[gone.clone(), zip]).unwrap();
        assert!(why.contains("no longer exists"), "{why}");
    }

    #[test]
    fn unresolved_items_are_named_in_the_message() {
        let names = |n: usize| (1..=n).map(|i| format!("Item {i}")).collect::<Vec<_>>();
        assert_eq!(names_text(&names(1)), "\"Item 1\"");
        assert_eq!(names_text(&names(2)), "\"Item 1\" and \"Item 2\"");
        assert_eq!(names_text(&names(3)), "\"Item 1\", \"Item 2\" and \"Item 3\"");
        assert_eq!(names_text(&names(5)), "\"Item 1\", \"Item 2\", \"Item 3\" and 2 more");
        let text = unresolved_message(MenuVerb::AddZip, &["This PC".into()]);
        assert!(text.contains("\"This PC\" is not a file or folder on disk"), "{text}");
        assert!(text.starts_with("\"Add to \"<name>.zip\"\" did not run"), "{text}");
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
