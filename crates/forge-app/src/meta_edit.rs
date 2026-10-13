//! Batch metadata editor (`Cmd::EditMetadata`). Writes title, caption, creator, copyright and
//! keyword changes into the XMP sidecars of the selected files. Only sidecars are written;
//! the original RAW or JPEG files are never modified.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_jobs::{JobHandle, JobState};
use forge_media::image_meta::read_xmp_sidecar;
use forge_media::xmp_write::{
    SidecarFields, SidecarOutcome, SidecarUpdate, read_sidecar_fields, sidecar_path_for,
    split_creators, update_sidecar,
};
use parking_lot::Mutex;

use crate::app::ZxrawApp;

/// Files read for the preview table. The apply step still covers every selected file.
const PREVIEW_LIMIT: usize = 50;

/// One form field: a "change" checkbox plus its text. A ticked, blank field removes the value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Field {
    enabled: bool,
    text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Form {
    title: Field,
    caption: Field,
    creator: Field,
    copyright: Field,
    add_keywords: Field,
    remove_keywords: Field,
}

/// Current sidecar state of one previewed file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    name: String,
    sidecar: PathBuf,
    exists: bool,
    fields: SidecarFields,
    keywords: Vec<String>,
}

/// One field that an update would change, for the preview table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    field: &'static str,
    current: String,
    new: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Summary {
    total: usize,
    processed: usize,
    created: usize,
    updated: usize,
    unchanged: usize,
    failed: usize,
    cancelled: bool,
    first_error: Option<String>,
}

impl Summary {
    fn record(&mut self, result: Result<SidecarOutcome, String>) {
        self.processed += 1;
        match result {
            Ok(SidecarOutcome::Created(_)) => self.created += 1,
            Ok(SidecarOutcome::Updated(_)) => self.updated += 1,
            Ok(SidecarOutcome::Unchanged(_)) => self.unchanged += 1,
            Err(error) => {
                self.failed += 1;
                self.first_error.get_or_insert(error);
            }
        }
    }

    fn message(&self) -> String {
        let head = if self.cancelled {
            format!("Metadata edit cancelled after {} of {} files", self.processed, self.total)
        } else {
            format!("Metadata edit finished for {} files", self.total)
        };
        let mut text = format!(
            "{head}: {} created, {} updated, {} unchanged, {} failed",
            self.created, self.updated, self.unchanged, self.failed
        );
        if let Some(error) = &self.first_error {
            text.push_str(&format!(" (first error: {error})"));
        }
        text
    }
}

struct Run {
    handle: JobHandle,
    outcome: Arc<Mutex<Option<Summary>>>,
    reported: bool,
}

#[derive(Default)]
struct MetaEditUi {
    open: bool,
    form: Form,
    /// Every editable file in the selection (folders and `.xmp` files excluded).
    targets: Vec<PathBuf>,
    skipped: usize,
    /// Sidecar state of the first `PREVIEW_LIMIT` targets; rebuilt when the selection changes.
    snapshots: Vec<Snapshot>,
    snapshot_key: Vec<PathBuf>,
    run: Option<Run>,
}

static UI: std::sync::OnceLock<Mutex<MetaEditUi>> = std::sync::OnceLock::new();

fn ui_state() -> &'static Mutex<MetaEditUi> {
    UI.get_or_init(|| Mutex::new(MetaEditUi::default()))
}

/// `Cmd::EditMetadata`.
pub fn open(app: &mut ZxrawApp) {
    let (targets, _) = selected_targets(app);
    if targets.is_empty() {
        app.toast_err("Select one or more files before editing metadata.");
        return;
    }
    ui_state().lock().open = true;
}

/// Called every frame by the app shell.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    report_finished_run(app);
    if !ui_state().lock().open {
        return;
    }
    let (targets, skipped) = selected_targets(app);
    let key: Vec<PathBuf> = targets.iter().take(PREVIEW_LIMIT).cloned().collect();
    if ui_state().lock().snapshot_key != key {
        let snapshots: Vec<Snapshot> = key.iter().map(|path| snapshot(path)).collect();
        let mut state = ui_state().lock();
        state.snapshots = snapshots;
        state.snapshot_key = key;
    }

    let mut start = false;
    let mut close = false;
    {
        let mut guard = ui_state().lock();
        let state = &mut *guard;
        state.targets = targets;
        state.skipped = skipped;
        let mut is_open = state.open;
        let active = state.run.as_ref().filter(|run| !run.handle.state().is_finished());
        if let Some(run) = active {
            let handle = run.handle.clone();
            let total = state.targets.len();
            egui::Window::new("Batch metadata")
                .collapsible(false)
                .open(&mut is_open)
                .show(ctx, |ui| {
                    let progress = handle.progress.snapshot();
                    ui.label(format!("Writing sidecars for {total} files"));
                    if let Some(fraction) = progress.fraction() {
                        ui.add(egui::ProgressBar::new(fraction).show_percentage());
                    } else {
                        ui.add(egui::ProgressBar::new(0.0).animate(true));
                    }
                    ui.label(format!("{} processed · {}", progress.items_done, progress.current));
                    if ui.button("Cancel").clicked() {
                        handle.cancel.cancel();
                    }
                });
            state.open = is_open;
        } else {
            let form = &mut state.form;
            let targets_len = state.targets.len();
            let skipped = state.skipped;
            let snapshots = &state.snapshots;
            let update = build_update(form);
            let applicable = targets_len > 0 && !update.is_empty();
            egui::Window::new("Batch metadata")
                .collapsible(false)
                .open(&mut is_open)
                .default_width(680.0)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "{targets_len} files selected{}",
                        if skipped > 0 {
                            format!(" ({skipped} folders or sidecars skipped)")
                        } else {
                            String::new()
                        }
                    ));
                    ui.label(
                        egui::RichText::new(
                            "Only XMP sidecars are written. Original RAW and JPEG files are never modified.",
                        )
                        .weak(),
                    );
                    if targets_len == 0 {
                        ui.label(egui::RichText::new("Select one or more files first.").weak());
                    }
                    ui.separator();
                    egui::Grid::new("meta_edit_fields")
                        .num_columns(2)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            field_row(ui, "Title", &mut form.title, "");
                            field_row(ui, "Caption", &mut form.caption, "");
                            field_row(ui, "Creator", &mut form.creator, "Separate several names with ;");
                            field_row(ui, "Copyright", &mut form.copyright, "");
                            field_row(ui, "Keywords to add", &mut form.add_keywords, "Separate with , or ;");
                            field_row(ui, "Keywords to remove", &mut form.remove_keywords, "Separate with , or ;");
                        });
                    ui.separator();
                    ui.label(format!(
                        "Preview: first {} of {} files",
                        snapshots.len(),
                        targets_len
                    ));
                    egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                        egui::Grid::new("meta_edit_preview")
                            .striped(true)
                            .num_columns(3)
                            .show(ui, |ui| {
                                ui.strong("File");
                                ui.strong("Sidecar written");
                                ui.strong("Changes");
                                ui.end_row();
                                for snap in snapshots {
                                    ui.label(snap.name.as_str());
                                    let sidecar = snap.sidecar.display().to_string();
                                    if snap.exists {
                                        ui.label(sidecar);
                                    } else {
                                        ui.label(format!("{sidecar} (new)"));
                                    }
                                    let changes = changes_for(&snap.fields, &snap.keywords, &update);
                                    ui.vertical(|ui| {
                                        if changes.is_empty() {
                                            ui.label(egui::RichText::new("No change").weak());
                                        }
                                        for change in &changes {
                                            ui.label(format!(
                                                "{}: {} → {}",
                                                change.field, change.current, change.new
                                            ));
                                        }
                                    });
                                    ui.end_row();
                                }
                            });
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(applicable, egui::Button::new("Apply to sidecars"))
                            .clicked()
                        {
                            start = true;
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                });
            state.open = is_open && !close;
        }
    }

    if start && let Err(error) = start_apply(app) {
        app.toast_err(error);
    }
}

fn field_row(ui: &mut egui::Ui, label: &str, field: &mut Field, hint: &str) {
    ui.checkbox(&mut field.enabled, label);
    ui.add_enabled(
        field.enabled,
        egui::TextEdit::singleline(&mut field.text)
            .hint_text(hint)
            .desired_width(420.0),
    );
    ui.end_row();
}

/// Editable files of the current selection, plus how many entries were skipped.
fn selected_targets(app: &ZxrawApp) -> (Vec<PathBuf>, usize) {
    let mut files = Vec::new();
    let mut skipped = 0;
    for entry in app.pane().selected_entries() {
        if entry.is_dir() || entry.ext == "xmp" {
            skipped += 1;
        } else {
            files.push(entry.path.clone());
        }
    }
    (files, skipped)
}

fn snapshot(path: &Path) -> Snapshot {
    let sidecar = sidecar_path_for(path);
    let exists = sidecar.is_file();
    let fields = read_sidecar_fields(path);
    let keywords = read_xmp_sidecar(path)
        .ok()
        .flatten()
        .map(|data| data.keywords)
        .unwrap_or_default();
    Snapshot {
        name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        sidecar,
        exists,
        fields,
        keywords,
    }
}

/// Shows the summary toast once when a run finishes, and forces the preview to be re-read.
fn report_finished_run(app: &mut ZxrawApp) {
    let notice = {
        let mut guard = ui_state().lock();
        let state = &mut *guard;
        let notice = match state.run.as_mut() {
            Some(run) if !run.reported && run.handle.state().is_finished() => {
                run.reported = true;
                Some(match run.outcome.lock().clone() {
                    Some(summary) => (summary.message(), summary.failed > 0),
                    None if run.handle.state() == JobState::Cancelled => {
                        ("Metadata edit cancelled".to_owned(), false)
                    }
                    None => ("Metadata edit failed".to_owned(), true),
                })
            }
            _ => None,
        };
        if notice.is_some() {
            state.snapshot_key.clear();
        }
        notice
    };
    if let Some((text, failed)) = notice {
        if failed {
            app.toast_err(text);
        } else {
            app.toast(text);
        }
    }
}

fn start_apply(app: &mut ZxrawApp) -> Result<(), String> {
    let (targets, update) = {
        let state = ui_state().lock();
        (state.targets.clone(), build_update(&state.form))
    };
    if targets.is_empty() {
        return Err("Select one or more files before editing metadata.".into());
    }
    if update.is_empty() {
        return Err("Tick at least one field to change.".into());
    }
    let outcome = Arc::new(Mutex::new(None));
    let worker_outcome = outcome.clone();
    let handle = app
        .ops
        .jobs
        .spawn("metadata-edit", "Batch metadata edit", move |job| {
            job.progress.add_total_items(targets.len() as u64);
            let mut summary = Summary {
                total: targets.len(),
                ..Summary::default()
            };
            for path in &targets {
                if job.cancel.check().is_err() {
                    summary.cancelled = true;
                    break;
                }
                job.progress.set_current(
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                );
                let result = update_sidecar(path, &update).map_err(|error| error.to_string());
                if result.is_err() {
                    job.progress.add_error();
                }
                summary.record(result);
                job.progress.add_items(1);
            }
            *worker_outcome.lock() = Some(summary);
            Ok(())
        });
    ui_state().lock().run = Some(Run {
        handle,
        outcome,
        reported: false,
    });
    Ok(())
}

/// The update described by the form. Unticked fields are left alone; a ticked blank removes.
fn build_update(form: &Form) -> SidecarUpdate {
    SidecarUpdate {
        title: field_value(&form.title),
        description: field_value(&form.caption),
        creator: field_value(&form.creator),
        rights: field_value(&form.copyright),
        add_keywords: keyword_value(&form.add_keywords),
        remove_keywords: keyword_value(&form.remove_keywords),
        ..Default::default()
    }
}

/// `None` when unticked; `Some(None)` removes the value when the text is blank.
fn field_value(field: &Field) -> Option<Option<String>> {
    field.enabled.then(|| {
        let text = field.text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    })
}

fn keyword_value(field: &Field) -> Vec<String> {
    if field.enabled {
        split_keywords(&field.text)
    } else {
        Vec::new()
    }
}

/// Splits on `,` or `;`, trims, drops blanks and removes case-insensitive duplicates.
fn split_keywords(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for keyword in input
        .split([',', ';'])
        .map(str::trim)
        .filter(|keyword| !keyword.is_empty())
    {
        if !out
            .iter()
            .any(|existing| existing.to_lowercase() == keyword.to_lowercase())
        {
            out.push(keyword.to_owned());
        }
    }
    out
}

/// Keyword list after an update, with the same case-insensitive rules as the sidecar writer.
fn apply_keywords(current: &[String], add: &[String], remove: &[String]) -> Vec<String> {
    let same = |a: &str, b: &str| a.to_lowercase() == b.to_lowercase();
    let mut out: Vec<String> = current
        .iter()
        .filter(|keyword| !remove.iter().any(|r| same(keyword, r.trim())))
        .cloned()
        .collect();
    for keyword in add.iter().map(|k| k.trim()).filter(|k| !k.is_empty()) {
        if !out.iter().any(|existing| same(existing, keyword)) {
            out.push(keyword.to_owned());
        }
    }
    out
}

/// Fields that `update` would actually change for a file with the given current state.
fn changes_for(fields: &SidecarFields, keywords: &[String], update: &SidecarUpdate) -> Vec<Change> {
    let mut out = Vec::new();
    push_change(
        &mut out,
        "Title",
        fields.title.as_deref().unwrap_or(""),
        &update.title,
    );
    push_change(
        &mut out,
        "Caption",
        fields.description.as_deref().unwrap_or(""),
        &update.description,
    );
    let creators = fields.creator.join("; ");
    let creator_request = update
        .creator
        .as_ref()
        .map(|raw| raw.as_deref().map(|raw| split_creators(raw).join("; ")));
    push_change(&mut out, "Creator", &creators, &creator_request);
    push_change(
        &mut out,
        "Copyright",
        fields.rights.as_deref().unwrap_or(""),
        &update.rights,
    );
    if !update.add_keywords.is_empty() || !update.remove_keywords.is_empty() {
        let new = apply_keywords(keywords, &update.add_keywords, &update.remove_keywords);
        if new != keywords {
            out.push(Change {
                field: "Keywords",
                current: keywords.join(", "),
                new: new.join(", "),
            });
        }
    }
    out
}

/// `requested`: `None` = field untouched; `Some(None)` = removal; `Some(Some(v))` = set to `v`.
fn push_change(
    out: &mut Vec<Change>,
    field: &'static str,
    current: &str,
    requested: &Option<Option<String>>,
) {
    let Some(requested) = requested else {
        return;
    };
    let new = requested
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let unchanged = match new {
        Some(value) => value == current,
        None => current.is_empty(),
    };
    if unchanged {
        return;
    }
    out.push(Change {
        field,
        current: if current.is_empty() {
            "(empty)".to_owned()
        } else {
            current.to_owned()
        },
        new: new.unwrap_or("(removed)").to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(text: &str) -> Field {
        Field {
            enabled: true,
            text: text.to_owned(),
        }
    }

    fn s(v: &str) -> String {
        v.to_owned()
    }

    #[test]
    fn keywords_split_on_comma_and_semicolon_and_dedupe() {
        assert_eq!(
            split_keywords(" beach, Sun; beach ,, SUN ;  "),
            vec![s("beach"), s("Sun")]
        );
        assert!(split_keywords(" , ; ").is_empty());
    }

    #[test]
    fn unticked_fields_build_an_empty_update() {
        let form = Form {
            title: Field {
                enabled: false,
                text: s("ignored"),
            },
            add_keywords: Field {
                enabled: false,
                text: s("ignored"),
            },
            ..Form::default()
        };
        assert!(build_update(&form).is_empty());
    }

    #[test]
    fn ticked_fields_set_or_remove() {
        let form = Form {
            title: field("  Lisbon  "),
            caption: field("   "),
            creator: field("Ana; Bruno"),
            copyright: field(""),
            add_keywords: field("sun, sea"),
            remove_keywords: field("rain"),
        };
        let update = build_update(&form);
        assert_eq!(update.title, Some(Some(s("Lisbon"))));
        assert_eq!(update.description, Some(None));
        assert_eq!(update.creator, Some(Some(s("Ana; Bruno"))));
        assert_eq!(update.rights, Some(None));
        assert_eq!(update.add_keywords, vec![s("sun"), s("sea")]);
        assert_eq!(update.remove_keywords, vec![s("rain")]);
        assert!(update.label.is_none() && update.rating.is_none());
    }

    #[test]
    fn keywords_apply_case_insensitively() {
        let current = vec![s("Beach"), s("sun")];
        let result = apply_keywords(&current, &[s("beach"), s("Dog")], &[s("SUN")]);
        assert_eq!(result, vec![s("Beach"), s("Dog")]);
    }

    #[test]
    fn changes_list_only_real_differences() {
        let fields = SidecarFields {
            title: Some(s("Old")),
            description: None,
            creator: vec![s("Ana")],
            rights: Some(s("(c) Ana")),
            ..SidecarFields::default()
        };
        let keywords = vec![s("beach")];
        let update = SidecarUpdate {
            title: Some(Some(s("New"))),
            description: Some(None),
            creator: Some(Some(s("Ana"))),
            rights: Some(None),
            add_keywords: vec![s("sun")],
            ..SidecarUpdate::default()
        };
        let changes = changes_for(&fields, &keywords, &update);
        let names: Vec<&str> = changes.iter().map(|c| c.field).collect();
        assert_eq!(names, vec!["Title", "Copyright", "Keywords"]);
        assert_eq!(changes[0].current, "Old");
        assert_eq!(changes[0].new, "New");
        assert_eq!(changes[1].new, "(removed)");
        assert_eq!(changes[2].new, "beach, sun");
    }

    #[test]
    fn creator_change_shows_the_split_names() {
        let fields = SidecarFields::default();
        let update = SidecarUpdate {
            creator: Some(Some(s("Ana ; Bruno"))),
            ..SidecarUpdate::default()
        };
        let changes = changes_for(&fields, &[], &update);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].current, "(empty)");
        assert_eq!(changes[0].new, "Ana; Bruno");
    }

    #[test]
    fn summary_counts_outcomes_and_reports_first_error() {
        let mut summary = Summary {
            total: 3,
            ..Summary::default()
        };
        summary.record(Ok(SidecarOutcome::Created(PathBuf::from("a.xmp"))));
        summary.record(Ok(SidecarOutcome::Unchanged(PathBuf::from("b.xmp"))));
        summary.record(Err(s("disk full")));
        assert_eq!(
            summary.message(),
            "Metadata edit finished for 3 files: 1 created, 0 updated, 1 unchanged, 1 failed (first error: disk full)"
        );
        summary.cancelled = true;
        assert!(summary.message().starts_with("Metadata edit cancelled after 3 of 3 files"));
    }
}
