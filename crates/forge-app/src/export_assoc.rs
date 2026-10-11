//! Finds photo exports from configurable filename rules.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use egui::{RichText, Ui};
use forge_core::model::{EntryKind, FileEntry, MediaKind, ext_of, media_kind_for_ext};

use crate::app::ZxrawApp;

const MAX_QUERY_CACHE: usize = 96;
const MAX_NEARBY_DIRS: usize = 32;
const MAX_DIRECTORY_ENTRIES: usize = 4096;
const MAX_DIRECTORY_DISCOVERY_ENTRIES: usize = 512;
pub const MAX_EXPORT_RULES: usize = 64;
const MAX_COMMAND_TARGETS: usize = 256;
const MAX_COMMAND_CANDIDATES: usize = 4096;

pub const DEFAULT_EXPORT_RULES: [&str; 6] = [
    "{base}-Edit",
    "{base}-Edit-{n}",
    "{base}_edited",
    "{base}-HDR",
    "{base}-Pano",
    "{base}-{n}",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Associations {
    pub originals: Vec<PathBuf>,
    pub exports: Vec<PathBuf>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryState {
    Pending,
    Complete(Associations),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct QueryKey {
    path: PathBuf,
    rules: Vec<String>,
}

static QUERY_CACHE: OnceLock<Mutex<HashMap<QueryKey, QueryState>>> = OnceLock::new();

fn query_cache() -> &'static Mutex<HashMap<QueryKey, QueryState>> {
    QUERY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn default_rules() -> Vec<String> {
    DEFAULT_EXPORT_RULES
        .iter()
        .map(|rule| (*rule).to_string())
        .collect()
}

pub fn active_rules(rules: &[String]) -> Vec<String> {
    rules.iter().take(MAX_EXPORT_RULES).cloned().collect()
}

/// Validates a filename template. `{base}` is required; `{n}` is an optional numeric token.
pub fn validate_rule(rule: &str) -> Result<(), String> {
    if rule.trim().is_empty() {
        return Err("Rule cannot be empty".into());
    }
    if rule.len() > 160 {
        return Err("Rule is longer than 160 bytes".into());
    }
    if rule.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '/' | '\\' | ':' | '"' | '<' | '>' | '|' | '?' | '*'
            )
    }) {
        return Err("Rules must be filename patterns without path separators".into());
    }
    if rule.match_indices("{base}").count() != 1 {
        return Err("Rule must contain `{base}` exactly once".into());
    }
    if rule.match_indices("{n}").count() > 1 {
        return Err("Rule can contain `{n}` at most once".into());
    }
    let residue = rule.replace("{base}", "").replace("{n}", "");
    if residue.contains('{') || residue.contains('}') {
        return Err("Only `{base}` and `{n}` placeholders are supported".into());
    }
    Ok(())
}

fn matches_export(rule: &str, original_stem: &str, export_stem: &str) -> bool {
    if validate_rule(rule).is_err() {
        return false;
    }
    // Split before substituting `{base}`, so a literal `{n}` in a filename cannot act as the number token.
    if let Some((prefix, suffix)) = rule.split_once("{n}") {
        let prefix = prefix.replace("{base}", original_stem);
        let suffix = suffix.replace("{base}", original_stem);
        if export_stem.len() < prefix.len() + suffix.len() {
            return false;
        }
        let Some(found_prefix) = export_stem.get(..prefix.len()) else {
            return false;
        };
        let Some(found_suffix) = export_stem.get(export_stem.len() - suffix.len()..) else {
            return false;
        };
        if !found_prefix.eq_ignore_ascii_case(&prefix)
            || !found_suffix.eq_ignore_ascii_case(&suffix)
        {
            return false;
        }
        let number = export_stem.get(prefix.len()..export_stem.len() - suffix.len());
        number.is_some_and(|value| {
            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
        })
    } else {
        rule.replace("{base}", original_stem)
            .eq_ignore_ascii_case(export_stem)
    }
}

#[cfg(windows)]
pub(crate) fn has_reparse_attribute(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(crate) fn has_reparse_attribute(_metadata: &fs::Metadata) -> bool {
    false
}

/// `FileType::is_symlink` does not report Windows junctions, so directories also check the reparse attribute.
pub(crate) fn is_link_or_junction(entry: &fs::DirEntry, file_type: &fs::FileType) -> bool {
    file_type.is_symlink()
        || (file_type.is_dir()
            && entry
                .metadata()
                .is_ok_and(|metadata| has_reparse_attribute(&metadata)))
}

fn is_export_of(original: &Path, export: &Path, rules: &[String]) -> bool {
    let (Some(original_stem), Some(export_stem)) = (original.file_stem(), export.file_stem())
    else {
        return false;
    };
    let original_stem = original_stem.to_string_lossy();
    let export_stem = export_stem.to_string_lossy();
    rules
        .iter()
        .take(MAX_EXPORT_RULES)
        .any(|rule| matches_export(rule, &original_stem, &export_stem))
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}

fn is_media_file(path: &Path) -> bool {
    let ext = ext_of(path);
    matches!(
        media_kind_for_ext(&ext),
        MediaKind::Image | MediaKind::Raw | MediaKind::Video
    )
}

fn nearby_directories(path: &Path) -> Vec<PathBuf> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let mut directories = vec![parent.to_path_buf()];

    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.take(MAX_DIRECTORY_DISCOVERY_ENTRIES).flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir()
                && !is_link_or_junction(&entry, &file_type)
                && matches!(
                    entry
                        .file_name()
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .as_str(),
                    "export" | "exports"
                )
            {
                directories.push(entry.path());
            }
        }
    }

    // A file stored in an Export(s) folder is commonly paired with originals one level up.
    if parent.file_name().is_some_and(|name| {
        matches!(
            name.to_string_lossy().to_ascii_lowercase().as_str(),
            "export" | "exports"
        )
    }) && let Some(grandparent) = parent.parent()
    {
        directories.push(grandparent.to_path_buf());
    }

    // Export sets also commonly use a sibling folder such as RAW and JPEG.
    if let Some(grandparent) = parent.parent()
        && let Ok(entries) = fs::read_dir(grandparent)
    {
        for entry in entries.take(MAX_DIRECTORY_DISCOVERY_ENTRIES).flatten() {
            if entry.path() == parent {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() && !is_link_or_junction(&entry, &file_type) {
                directories.push(entry.path());
                if directories.len() >= MAX_NEARBY_DIRS {
                    break;
                }
            }
        }
    }

    let mut seen = HashSet::new();
    directories.retain(|directory| seen.insert(normalized_path(directory)));
    directories.truncate(MAX_NEARBY_DIRS);
    directories
}

fn scan_for(path: &Path, rules: &[String]) -> Associations {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut entries_seen = 0usize;
    let mut truncated = false;

    for directory in nearby_directories(path) {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries {
            if entries_seen >= MAX_DIRECTORY_ENTRIES {
                truncated = true;
                break;
            }
            entries_seen += 1;
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() || is_link_or_junction(&entry, &file_type) {
                continue;
            }
            let candidate = entry.path();
            if !is_media_file(&candidate) || !seen.insert(normalized_path(&candidate)) {
                continue;
            }
            candidates.push(candidate);
        }
        if truncated {
            break;
        }
    }

    let mut associations = Associations {
        truncated,
        ..Associations::default()
    };
    let mut originals = HashSet::new();
    let mut exports = HashSet::new();
    for candidate in candidates {
        if normalized_path(&candidate) == normalized_path(path) {
            continue;
        }
        if is_export_of(path, &candidate, rules) {
            if exports.insert(normalized_path(&candidate)) {
                associations.exports.push(candidate);
            }
        } else if is_export_of(&candidate, path, rules)
            && originals.insert(normalized_path(&candidate))
        {
            associations.originals.push(candidate);
        }
    }
    associations.originals.sort();
    associations.exports.sort();
    associations
}

/// Forgets finished association scans (a folder changed, so exports may have appeared or
/// disappeared). Scans still running keep their slot and publish when they finish.
pub fn invalidate_cache() {
    query_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .retain(|_, state| *state == QueryState::Pending);
}

pub fn request_for(path: &Path, rules: &[String], ctx: &egui::Context) {
    let key = QueryKey {
        path: path.to_path_buf(),
        rules: active_rules(rules),
    };
    {
        let mut cache = query_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if cache.contains_key(&key) {
            return;
        }
        if cache.len() >= MAX_QUERY_CACHE
            && let Some(oldest) = cache
                .iter()
                .find_map(|(key, state)| (*state != QueryState::Pending).then_some(key.clone()))
        {
            cache.remove(&oldest);
        }
        cache.insert(key.clone(), QueryState::Pending);
    }

    let repaint = ctx.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("export-association-scan".into())
        .spawn(move || {
            let state = QueryState::Complete(scan_for(&key.path, &key.rules));
            query_cache()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key, state);
            repaint.request_repaint();
        })
    {
        query_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                QueryKey {
                    path: path.to_path_buf(),
                    rules: active_rules(rules),
                },
                QueryState::Failed(error.to_string()),
            );
    }
}

pub fn cached_for(path: &Path, rules: &[String]) -> Option<QueryState> {
    query_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&QueryKey {
            path: path.to_path_buf(),
            rules: active_rules(rules),
        })
        .cloned()
}

/// Related originals/exports in the preview panel.
pub fn panel(app: &mut ZxrawApp, ui: &mut Ui, entry: &FileEntry) {
    if entry.kind != EntryKind::File || !is_media_file(&entry.path) {
        return;
    }
    let rules = active_rules(&app.settings.export_rules);
    request_for(&entry.path, &rules, ui.ctx());
    let state = cached_for(&entry.path, &rules);
    let associations = match state {
        Some(QueryState::Complete(associations)) => associations,
        Some(QueryState::Pending) | None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Checking related originals and exports…");
            });
            return;
        }
        Some(QueryState::Failed(error)) => {
            ui.label(RichText::new(format!("Related files unavailable: {error}")).weak());
            return;
        }
    };

    let mut action = None;
    if !associations.originals.is_empty() {
        ui.separator();
        ui.label(RichText::new("Original(s)").strong());
        for path in &associations.originals {
            if let Some(row_action) = related_row(ui, &entry.path, path) {
                action = Some(row_action);
            }
        }
    }
    if !associations.exports.is_empty() {
        ui.separator();
        ui.label(RichText::new("Exports").strong());
        for path in &associations.exports {
            if let Some(row_action) = related_row(ui, &entry.path, path) {
                action = Some(row_action);
            }
        }
    }
    if associations.truncated {
        ui.label(
            RichText::new("Nearby-folder scan reached its file limit.")
                .small()
                .weak(),
        );
    }

    if let Some((kind, path)) = action {
        match kind {
            RelatedActionKind::Open => crate::actions::open_file(app, &path),
            RelatedActionKind::Reveal => app.ops.spawn_shell("Reveal related file", move || {
                forge_win::reveal_in_explorer(&path)
            }),
            RelatedActionKind::ShowInFolder => app.navigate(path),
        }
    }
}

#[derive(Clone, Copy)]
enum RelatedActionKind {
    Open,
    Reveal,
    ShowInFolder,
}

fn related_row(ui: &mut Ui, current: &Path, path: &Path) -> Option<(RelatedActionKind, PathBuf)> {
    let mut action = None;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    ui.horizontal_wrapped(|ui| {
        ui.label(name);
        if ui.small_button("Open").clicked() {
            action = Some(RelatedActionKind::Open);
        }
        if ui.small_button("Reveal").clicked() {
            action = Some(RelatedActionKind::Reveal);
        }
        let label = if path.parent() == current.parent() {
            "Select in pane"
        } else {
            "Show in folder"
        };
        if ui.small_button(label).clicked() {
            action = Some(RelatedActionKind::ShowInFolder);
        }
    });
    action.map(|kind| (kind, path.to_path_buf()))
}

/// `Cmd::SelectOriginals` selects matches visible in the current pane.
pub fn select_originals(app: &mut ZxrawApp) {
    select_counterparts(app, false);
}

/// `Cmd::SelectExports` selects matches visible in the current pane.
pub fn select_exports(app: &mut ZxrawApp) {
    select_counterparts(app, true);
}

fn select_counterparts(app: &mut ZxrawApp, want_exports: bool) {
    let rules = active_rules(&app.settings.export_rules);
    let pane = app.pane();
    let targets = pane
        .action_targets()
        .into_iter()
        .filter(|target| is_media_file(target))
        .take(MAX_COMMAND_TARGETS)
        .collect::<Vec<_>>();
    let candidates = pane
        .visible
        .iter()
        .filter_map(|&index| pane.entries.get(index))
        .filter(|entry| entry.kind == EntryKind::File && is_media_file(&entry.path))
        .map(|entry| entry.path.clone())
        .take(MAX_COMMAND_CANDIDATES)
        .collect::<Vec<_>>();
    let mut matches = Vec::new();
    let mut seen = HashSet::new();
    for target in targets {
        for candidate in &candidates {
            if normalized_path(candidate) == normalized_path(&target) {
                continue;
            }
            let found = if want_exports {
                is_export_of(&target, candidate, &rules)
            } else {
                is_export_of(candidate, &target, &rules)
            };
            if found && seen.insert(normalized_path(candidate)) {
                matches.push(candidate.clone());
            }
        }
    }
    if matches.is_empty() {
        let what = if want_exports { "exports" } else { "originals" };
        app.show_preview = true;
        app.toast(format!(
            "No matching {what} are in this folder. Nearby matches appear in the preview with Show in folder."
        ));
        return;
    }
    let matched = matches
        .iter()
        .map(|path| normalized_path(path))
        .collect::<HashSet<_>>();
    let pane = app.pane_mut();
    pane.selected.clear();
    pane.selected.extend(matches.iter().cloned());
    pane.cursor = pane.visible.iter().position(|&index| {
        pane.entries
            .get(index)
            .is_some_and(|entry| matched.contains(&normalized_path(&entry.path)))
    });
    pane.scroll_to_cursor = true;
    app.toast(format!("Selected {} counterpart(s)", matches.len()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"fixture").unwrap();
    }

    #[test]
    fn default_rules_match_all_supported_export_names() {
        let original = Path::new("IMG_1234.CR3");
        for export in [
            "IMG_1234-Edit.jpg",
            "IMG_1234-Edit-2.jpg",
            "IMG_1234_edited.tif",
            "IMG_1234-HDR.jpg",
            "IMG_1234-Pano.tif",
            "IMG_1234-3.jpg",
        ] {
            assert!(
                is_export_of(original, Path::new(export), &default_rules()),
                "{export}"
            );
        }
    }

    #[test]
    fn matching_is_exact_and_numeric_tokens_do_not_accept_text() {
        let rules = default_rules();
        assert!(!is_export_of(
            Path::new("IMG_12345.CR3"),
            Path::new("IMG_1234-Edit.jpg"),
            &rules
        ));
        assert!(!is_export_of(
            Path::new("IMG_1234.CR3"),
            Path::new("IMG_1234-Edit-copy.jpg"),
            &rules
        ));
        assert!(!is_export_of(
            Path::new("IMG_1234.CR3"),
            Path::new("IMG_12345.jpg"),
            &rules
        ));
    }

    #[test]
    fn numeric_token_needs_digits_and_literal_braces_in_names_are_plain_text() {
        let rules = default_rules();
        for export in ["IMG_1234--1.jpg", "IMG_1234-+1.jpg", "IMG_1234-.jpg"] {
            assert!(
                !is_export_of(Path::new("IMG_1234.CR3"), Path::new(export), &rules),
                "{export}"
            );
        }
        // A literal `{n}` in the original name must not be read as the number token.
        assert!(is_export_of(
            Path::new("IMG{n}.CR3"),
            Path::new("IMG{n}-7.jpg"),
            &rules
        ));
        assert!(!is_export_of(
            Path::new("IMG{n}.CR3"),
            Path::new("IMG-7.jpg"),
            &rules
        ));
    }

    #[test]
    fn invalid_rules_are_rejected_without_interpreting_filename_text() {
        for rule in [
            "",
            "Edit",
            "{base}-{unknown}",
            "{base}/{n}",
            "{base}-{n}-{n}",
            "{base}{base}",
        ] {
            assert!(validate_rule(rule).is_err(), "{rule}");
        }
        let rules = vec!["{base}.*".to_string()];
        assert!(!is_export_of(
            Path::new("photo.jpg"),
            Path::new("photo.anything.jpg"),
            &rules
        ));
    }

    #[test]
    fn association_scan_finds_exports_in_export_and_sibling_folders() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let original = root.join("RAW").join("IMG_1234.CR3");
        let export_subfolder = root.join("RAW").join("Exports").join("IMG_1234-Edit.jpg");
        let export_sibling = root.join("JPEG").join("IMG_1234-HDR.jpg");
        touch(&original);
        touch(&export_subfolder);
        touch(&export_sibling);

        let result = scan_for(&original, &default_rules());
        assert!(result.exports.contains(&export_subfolder));
        assert!(result.exports.contains(&export_sibling));
    }

    #[test]
    fn export_path_resolves_its_original_in_a_sibling_folder() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let original = root.join("RAW").join("IMG_1234.CR3");
        let export = root.join("JPEG").join("IMG_1234-Edit.jpg");
        touch(&original);
        touch(&export);

        let result = scan_for(&export, &default_rules());
        assert_eq!(result.originals, vec![original]);
    }
}
