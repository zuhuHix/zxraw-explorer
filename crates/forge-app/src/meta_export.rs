//! Asynchronous CSV and JSON metadata reports.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use chrono::{DateTime, SecondsFormat, Utc};
use forge_core::model::{MediaKind, media_kind_for_ext};
use forge_core::paths::rename_no_replace;
use forge_jobs::{JobHandle, JobState};
use parking_lot::Mutex;
use serde::Serialize;

use crate::app::ZxrawApp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportFormat {
    Csv,
    Json,
}

impl ReportFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Json => "JSON",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Selection,
    Folder,
}

#[derive(Default)]
struct ExportUi {
    open: bool,
    scope: Option<Scope>,
    recursive: bool,
    format: Option<ReportFormat>,
    run: Option<ExportRun>,
}

struct ExportRun {
    handle: JobHandle,
    output: PathBuf,
    outcome: Arc<Mutex<Option<Result<(), String>>>>,
    reported: bool,
}

static UI: std::sync::OnceLock<Mutex<ExportUi>> = std::sync::OnceLock::new();
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn ui_state() -> &'static Mutex<ExportUi> {
    UI.get_or_init(|| Mutex::new(ExportUi::default()))
}

/// `Cmd::ExportMetadata`.
pub fn open(app: &mut ZxrawApp) {
    let mut state = ui_state().lock();
    state.open = true;
    state.scope = Some(if app.pane().selected_entries().is_empty() {
        Scope::Folder
    } else {
        Scope::Selection
    });
    state.format.get_or_insert(ReportFormat::Csv);
}

/// Called every frame by the app shell.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    let notice = {
        let mut state = ui_state().lock();
        state.run.as_mut().and_then(|run| {
            if run.reported || !run.handle.state().is_finished() {
                return None;
            }
            run.reported = true;
            let outcome = run.outcome.lock().clone();
            Some(match outcome {
                Some(Ok(())) => format!("Metadata report saved to {}", run.output.display()),
                Some(Err(error)) if run.handle.state() == JobState::Cancelled => {
                    format!("Metadata report export cancelled: {error}")
                }
                Some(Err(error)) => format!("Metadata report export failed: {error}"),
                None if run.handle.state() == JobState::Cancelled => {
                    "Metadata report export cancelled".to_string()
                }
                None => "Metadata report export failed".to_string(),
            })
        })
    };
    if let Some(notice) = notice {
        if notice.contains("failed") {
            app.toast_err(notice);
        } else {
            app.toast(notice);
        }
    }

    let (mut is_open, mut scope, mut recursive, mut format, active) = {
        let state = ui_state().lock();
        let active = state
            .run
            .as_ref()
            .filter(|run| !run.handle.state().is_finished())
            .map(|run| (run.handle.clone(), run.output.clone()));
        (
            state.open,
            state.scope.unwrap_or(Scope::Folder),
            state.recursive,
            state.format.unwrap_or(ReportFormat::Csv),
            active,
        )
    };
    let library_ready = app.ops.library.db.is_some();
    let library_error = app.ops.library.open_error.clone();
    if !is_open && active.is_none() {
        return;
    }
    let selected_count = app.selected_listed_count();

    let mut start = false;
    let mut close_after = false;
    egui::Window::new("Export metadata report")
        .collapsible(false)
        .open(&mut is_open)
        .show(ctx, |ui| {
            if let Some((handle, output)) = &active {
                let progress = handle.progress.snapshot();
                ui.label(format!("Saving {}", output.display()));
                if let Some(fraction) = progress.fraction() {
                    ui.add(egui::ProgressBar::new(fraction).show_percentage());
                } else {
                    ui.add(egui::ProgressBar::new(0.0).animate(true));
                }
                ui.label(if progress.current.is_empty() {
                    format!("{} items processed", progress.items_done)
                } else {
                    format!(
                        "{} · {} items processed",
                        progress.current, progress.items_done
                    )
                });
                if ui.button("Cancel export").clicked() {
                    handle.cancel.cancel();
                }
                return;
            }

            ui.label("Export file and media metadata from the library.");
            ui.horizontal(|ui| {
                ui.radio_value(
                    &mut scope,
                    Scope::Selection,
                    format!("Selection ({selected_count})"),
                );
                ui.radio_value(&mut scope, Scope::Folder, "Current folder");
            });
            if scope == Scope::Folder {
                ui.checkbox(
                    &mut recursive,
                    "Include subfolders (skip junctions and symlinks)",
                );
            }
            ui.horizontal(|ui| {
                ui.label("Format");
                ui.selectable_value(&mut format, ReportFormat::Csv, "CSV");
                ui.selectable_value(&mut format, ReportFormat::Json, "JSON");
            });
            ui.label("An existing output file is never overwritten; export will fail instead.");
            if let Some(error) = &library_error {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("The library database is unavailable: {error}"),
                );
            } else if !library_ready {
                ui.label(egui::RichText::new("Waiting for the library database to open…").weak());
            }
            if scope == Scope::Selection && selected_count == 0 {
                ui.label(egui::RichText::new("Select one or more items first.").weak());
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        library_ready && (scope != Scope::Selection || selected_count > 0),
                        egui::Button::new("Export…"),
                    )
                    .clicked()
                {
                    start = true;
                }
                if ui.button("Close").clicked() {
                    close_after = true;
                }
            });
        });

    {
        let mut state = ui_state().lock();
        state.open = is_open && !close_after;
        state.scope = Some(scope);
        state.recursive = recursive;
        state.format = Some(format);
    }
    if start && let Err(error) = start_with_dialog(app, scope, recursive, format) {
        app.toast_err(error);
    }
}

/// Test-hook entry point. The save path still comes from `dialogs::save_file`, whose
/// automated answer is supplied by the script's preceding `dialog <path>` step.
pub(crate) fn test_start(
    app: &mut ZxrawApp,
    format: &str,
    scope: &str,
    recursive: bool,
) -> Result<(), String> {
    let format = parse_format(format)?;
    let scope = match scope {
        "selection" => Scope::Selection,
        "folder" => Scope::Folder,
        _ => return Err("export scope must be `selection` or `folder`".into()),
    };
    {
        let mut state = ui_state().lock();
        state.open = true;
        state.scope = Some(scope);
        state.recursive = recursive;
        state.format = Some(format);
    }
    start_with_dialog(app, scope, recursive, format)
}

pub(crate) type TestStatus = (JobState, PathBuf, Option<Result<(), String>>);

pub(crate) fn test_status() -> Option<TestStatus> {
    let state = ui_state().lock();
    state.run.as_ref().map(|run| {
        (
            run.handle.state(),
            run.output.clone(),
            run.outcome.lock().clone(),
        )
    })
}

fn parse_format(format: &str) -> Result<ReportFormat, String> {
    match format.to_ascii_lowercase().as_str() {
        "csv" => Ok(ReportFormat::Csv),
        "json" => Ok(ReportFormat::Json),
        _ => Err("export format must be `csv` or `json`".into()),
    }
}

fn start_with_dialog(
    app: &mut ZxrawApp,
    scope: Scope,
    recursive: bool,
    format: ReportFormat,
) -> Result<(), String> {
    let database = app.ops.library.db.clone().ok_or_else(|| {
        app.ops.library.open_error.as_ref().map_or_else(
            || "The library database is still opening; try again shortly.".to_string(),
            |error| format!("The library database is unavailable: {error}"),
        )
    })?;
    let selected: Vec<PathBuf> = if scope == Scope::Selection {
        app.pane()
            .selected_entries()
            .into_iter()
            .map(|entry| entry.path.clone())
            .collect()
    } else {
        Vec::new()
    };
    if scope == Scope::Selection && selected.is_empty() {
        return Err("Select one or more items before exporting.".into());
    }
    let output = crate::dialogs::save_file(
        rfd::FileDialog::new()
            .add_filter(format.label(), &[format.extension()])
            .set_file_name(format!("metadata.{}", format.extension())),
    );
    let Some(output) = output else {
        return Ok(());
    };
    let folder = app.pane().path.clone();
    let ffprobe_available = forge_media::FfTools::global().has_ffprobe();
    let outcome = Arc::new(Mutex::new(None));
    let worker_outcome = outcome.clone();
    let write_output = output.clone();
    let job = app
        .ops
        .jobs
        .spawn("metadata-export", "Export metadata report", move |handle| {
            let result = export_report(
                ExportRequest {
                    output: &write_output,
                    selected: &selected,
                    folder: &folder,
                    scope,
                    recursive,
                    format,
                    database: Some(database.as_ref()),
                    ffprobe_available,
                },
                handle,
            );
            *worker_outcome.lock() = Some(result.as_ref().map(|_| ()).map_err(Clone::clone));
            result
        });
    ui_state().lock().run = Some(ExportRun {
        handle: job,
        output,
        outcome,
        reported: false,
    });
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct Report {
    version: u32,
    records: Vec<ReportRecord>,
    notes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct ReportRecord {
    path: String,
    name: String,
    size: Option<u64>,
    modified: Option<String>,
    kind: String,
    width: Option<u32>,
    height: Option<u32>,
    camera_make: Option<String>,
    camera_model: Option<String>,
    lens: Option<String>,
    iso: Option<u32>,
    exposure_seconds: Option<f64>,
    aperture: Option<f64>,
    focal_length_mm: Option<f64>,
    capture_time: Option<String>,
    gps_lat: Option<f64>,
    gps_lon: Option<f64>,
    gps_alt_m: Option<f64>,
    rating: Option<u8>,
    label: Option<String>,
    pick: Option<String>,
    tags: Vec<String>,
    duration_seconds: Option<f64>,
    codec: Option<String>,
    fps: Option<f64>,
    resolution: Option<String>,
    audio: Option<String>,
    video_note: Option<String>,
    metadata_error: Option<String>,
}

#[derive(Debug, Clone)]
struct Target {
    path: PathBuf,
}

struct ExportRequest<'a> {
    output: &'a Path,
    selected: &'a [PathBuf],
    folder: &'a Path,
    scope: Scope,
    recursive: bool,
    format: ReportFormat,
    database: Option<&'a forge_db::Db>,
    ffprobe_available: bool,
}

fn export_report(request: ExportRequest<'_>, job: &JobHandle) -> Result<(), String> {
    let ExportRequest {
        output,
        selected,
        folder,
        scope,
        recursive,
        format,
        database,
        ffprobe_available,
    } = request;
    job.cancel.check().map_err(|error| error.to_string())?;
    job.progress.set_current("Scanning items");
    let targets = match scope {
        Scope::Selection => selected
            .iter()
            .cloned()
            .map(|path| Target { path })
            .collect(),
        Scope::Folder => collect_folder(folder, recursive, job)?,
    };
    job.progress
        .items_total
        .store(targets.len() as u64, Ordering::Relaxed);
    let mut records = Vec::with_capacity(targets.len());
    let mut notes = Vec::new();
    if !ffprobe_available
        && targets.iter().any(|target| {
            media_kind_for_ext(&forge_core::model::ext_of(&target.path)) == MediaKind::Video
        })
    {
        notes.push("ffprobe is unavailable; video metadata fields are blank.".to_string());
    }
    for target in targets {
        job.cancel.check().map_err(|error| error.to_string())?;
        job.progress
            .set_current(format!("Reading {}", target.path.display()));
        let record = build_record(&target.path, database, ffprobe_available);
        if record.metadata_error.is_some() {
            job.progress.add_error();
        }
        job.progress.add_items(1);
        records.push(record);
    }
    job.cancel.check().map_err(|error| error.to_string())?;
    job.progress.set_current("Writing report");
    publish_report(
        output,
        format,
        &Report {
            version: 1,
            records,
            notes,
        },
        job,
    )
}

fn collect_folder(folder: &Path, recursive: bool, job: &JobHandle) -> Result<Vec<Target>, String> {
    let root = fs::symlink_metadata(folder)
        .map_err(|error| format!("cannot inspect folder {}: {error}", folder.display()))?;
    if recursive && is_reparse(&root) {
        return Err("recursive export cannot follow a junction or symlink root".into());
    }
    if !root.is_dir() {
        return Err(format!("{} is not a folder", folder.display()));
    }
    let mut pending = vec![folder.to_path_buf()];
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    while let Some(directory) = pending.pop() {
        job.cancel.check().map_err(|error| error.to_string())?;
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("cannot list folder {}: {error}", directory.display()))?;
        let mut children = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| format!("cannot read folder entry: {error}"))?;
            children.push(entry.path());
        }
        children.sort();
        for path in children {
            job.cancel.check().map_err(|error| error.to_string())?;
            if !seen.insert(path.clone()) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
            let reparse = is_reparse(&metadata);
            if recursive && metadata.is_dir() && !reparse {
                pending.push(path.clone());
            }
            paths.push(Target { path });
        }
    }
    Ok(paths)
}

fn is_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn build_record(
    path: &Path,
    database: Option<&forge_db::Db>,
    ffprobe_available: bool,
) -> ReportRecord {
    let mut record = ReportRecord {
        path: path.to_string_lossy().into_owned(),
        name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        ..Default::default()
    };
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            record.metadata_error = Some(format!("filesystem metadata: {error}"));
            return record;
        }
    };
    record.size = Some(metadata.len());
    record.modified = metadata.modified().ok().map(format_time);
    if is_reparse(&metadata) {
        record.kind = "link".into();
        return record;
    }
    if metadata.is_dir() {
        record.kind = "directory".into();
        return record;
    }
    if !metadata.is_file() {
        record.kind = "other".into();
        return record;
    }
    let media_kind = media_kind_for_ext(&forge_core::model::ext_of(path));
    record.kind = media_kind_name(media_kind).into();
    match media_kind {
        MediaKind::Image | MediaKind::Raw => match forge_media::read_image_metadata(path) {
            Ok(metadata) => {
                record.width = metadata.width;
                record.height = metadata.height;
                record.camera_make = metadata.make;
                record.camera_model = metadata.model;
                record.lens = metadata.lens;
                record.iso = metadata.iso;
                record.exposure_seconds = metadata.exposure_time_s;
                record.aperture = metadata.aperture;
                record.focal_length_mm = metadata.focal_length_mm;
                record.capture_time = metadata.date_taken;
                if !metadata.gps_invalid {
                    record.gps_lat = metadata.gps_lat;
                    record.gps_lon = metadata.gps_lon;
                    record.gps_alt_m = metadata.gps_alt_m;
                }
            }
            Err(error) => record.metadata_error = Some(format!("image metadata: {error}")),
        },
        MediaKind::Video if ffprobe_available => match forge_media::FfTools::global().probe(path) {
            Ok(metadata) => {
                record.duration_seconds = metadata.duration_s;
                record.codec = metadata.codec;
                record.fps = metadata.fps;
                record.resolution = metadata
                    .width
                    .zip(metadata.height)
                    .map(|(w, h)| format!("{w}x{h}"));
                record.audio = Some(
                    metadata
                        .audio
                        .iter()
                        .map(|audio| {
                            format!(
                                "{} {}ch {}Hz",
                                audio.codec.as_deref().unwrap_or("audio"),
                                audio.channels.unwrap_or(0),
                                audio.sample_rate.unwrap_or(0)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; "),
                );
            }
            Err(error) => record.metadata_error = Some(format!("video metadata: {error}")),
        },
        MediaKind::Video => {
            record.video_note =
                Some("ffprobe is unavailable; video metadata fields are blank.".into());
        }
        _ => {}
    }
    if let Some(database) = database {
        match database.file_id(path) {
            Ok(Some(file_id)) => match database.annotation(file_id) {
                Ok(annotation) => {
                    record.rating = Some(annotation.rating);
                    record.label = annotation.label.map(|label| label.as_str().to_string());
                    record.pick = Some(
                        match annotation.pick {
                            forge_core::query::PickState::Pick => "pick",
                            forge_core::query::PickState::Reject => "reject",
                            forge_core::query::PickState::None => "none",
                        }
                        .into(),
                    );
                    record.tags = annotation.tags;
                }
                Err(error) => {
                    record.metadata_error = append_error(
                        record.metadata_error,
                        format!("library annotations: {error}"),
                    )
                }
            },
            Ok(None) => {}
            Err(error) => {
                record.metadata_error =
                    append_error(record.metadata_error, format!("library lookup: {error}"))
            }
        }
    }
    record
}

fn append_error(previous: Option<String>, next: String) -> Option<String> {
    Some(previous.map_or(next.clone(), |previous| format!("{previous}; {next}")))
}

fn media_kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Raw => "raw",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Sidecar => "sidecar",
        MediaKind::Document => "document",
        MediaKind::Archive => "archive",
        MediaKind::Project => "project",
        MediaKind::Other => "other",
    }
}

fn format_time(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Millis, true)
}

struct TempOutput(PathBuf);

impl Drop for TempOutput {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn create_temp(parent: &Path, output_name: &str) -> io::Result<(TempOutput, File)> {
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{output_name}.zxraw-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((TempOutput(candidate), file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a unique temp file",
    ))
}

fn publish_report(
    output: &Path,
    format: ReportFormat,
    report: &Report,
    job: &JobHandle,
) -> Result<(), String> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let output_name = output
        .file_name()
        .ok_or_else(|| "output path has no file name".to_string())?
        .to_string_lossy();
    let (temp, mut file) = create_temp(parent, &output_name)
        .map_err(|error| format!("cannot create a temp file beside the output: {error}"))?;
    match format {
        ReportFormat::Csv => write_csv(&mut file, report, Some(job))?,
        ReportFormat::Json => write_json(&mut file, report, Some(job))?,
    }
    file.write_all(b"\n")
        .map_err(|error| format!("cannot finish report: {error}"))?;
    file.flush()
        .map_err(|error| format!("cannot flush report: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync report: {error}"))?;
    drop(file);
    job.cancel.check().map_err(|error| error.to_string())?;
    rename_no_replace(&temp.0, output).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            "the output file already exists; choose another name".to_string()
        } else {
            format!("cannot publish report {}: {error}", output.display())
        }
    })?;
    Ok(())
}

const CSV_HEADERS: &[&str] = &[
    "path",
    "name",
    "size",
    "modified",
    "kind",
    "width",
    "height",
    "camera_make",
    "camera_model",
    "lens",
    "iso",
    "exposure_seconds",
    "aperture",
    "focal_length_mm",
    "capture_time",
    "gps_lat",
    "gps_lon",
    "gps_alt_m",
    "rating",
    "label",
    "pick",
    "tags",
    "duration_seconds",
    "codec",
    "fps",
    "resolution",
    "audio",
    "video_note",
    "metadata_error",
];

fn csv_values(record: &ReportRecord) -> Vec<String> {
    vec![
        record.path.clone(),
        record.name.clone(),
        opt_display(record.size),
        opt_display(record.modified.as_deref()),
        record.kind.clone(),
        opt_display(record.width),
        opt_display(record.height),
        opt_display(record.camera_make.as_deref()),
        opt_display(record.camera_model.as_deref()),
        opt_display(record.lens.as_deref()),
        opt_display(record.iso),
        opt_display(record.exposure_seconds),
        opt_display(record.aperture),
        opt_display(record.focal_length_mm),
        opt_display(record.capture_time.as_deref()),
        opt_display(record.gps_lat),
        opt_display(record.gps_lon),
        opt_display(record.gps_alt_m),
        opt_display(record.rating),
        opt_display(record.label.as_deref()),
        opt_display(record.pick.as_deref()),
        record.tags.join("; "),
        opt_display(record.duration_seconds),
        opt_display(record.codec.as_deref()),
        opt_display(record.fps),
        opt_display(record.resolution.as_deref()),
        opt_display(record.audio.as_deref()),
        opt_display(record.video_note.as_deref()),
        opt_display(record.metadata_error.as_deref()),
    ]
}

fn opt_display(value: Option<impl ToString>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn write_csv(
    writer: &mut impl Write,
    report: &Report,
    job: Option<&JobHandle>,
) -> Result<(), String> {
    writer
        .write_all(&[0xEF, 0xBB, 0xBF])
        .map_err(|error| format!("cannot write CSV report: {error}"))?;
    write_csv_row(writer, CSV_HEADERS.iter().map(|value| value.to_string()))
        .map_err(|error| format!("cannot write CSV headers: {error}"))?;
    for record in &report.records {
        if let Some(job) = job {
            job.cancel.check().map_err(|error| error.to_string())?;
        }
        write_csv_row(writer, csv_values(record))
            .map_err(|error| format!("cannot write CSV row: {error}"))?;
    }
    Ok(())
}

fn write_json(
    writer: &mut impl Write,
    report: &Report,
    job: Option<&JobHandle>,
) -> Result<(), String> {
    writer
        .write_all(b"{\"version\":1,\"records\":[")
        .map_err(|error| format!("cannot write JSON report: {error}"))?;
    for (index, record) in report.records.iter().enumerate() {
        if let Some(job) = job {
            job.cancel.check().map_err(|error| error.to_string())?;
        }
        if index > 0 {
            writer
                .write_all(b",")
                .map_err(|error| format!("cannot write JSON report: {error}"))?;
        }
        serde_json::to_writer(&mut *writer, record)
            .map_err(|error| format!("cannot serialize JSON report row: {error}"))?;
    }
    writer
        .write_all(b"],\"notes\":")
        .map_err(|error| format!("cannot write JSON report: {error}"))?;
    serde_json::to_writer(&mut *writer, &report.notes)
        .map_err(|error| format!("cannot serialize JSON report notes: {error}"))?;
    writer
        .write_all(b"}")
        .map_err(|error| format!("cannot finish JSON report: {error}"))
}

/// Whether a spreadsheet could read the cell as a formula. Plain numbers (a western
/// longitude, a negative altitude) stay numbers; any other text whose first non-blank
/// character starts a formula, or that starts with a tab or carriage return, gets a `'`.
fn needs_formula_guard(value: &str) -> bool {
    if value.trim().parse::<f64>().is_ok_and(f64::is_finite) {
        return false;
    }
    value.starts_with(['\t', '\r'])
        || value
            .trim_start()
            .starts_with(['=', '+', '-', '@', '\t', '\r'])
}

fn write_csv_row(
    writer: &mut impl Write,
    values: impl IntoIterator<Item = String>,
) -> io::Result<()> {
    let mut first = true;
    for value in values {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        let escaped = if needs_formula_guard(&value) {
            format!("'{value}")
        } else {
            value
        };
        writer.write_all(b"\"")?;
        for byte in escaped.bytes() {
            if byte == b'"' {
                writer.write_all(b"\"\"")?;
            } else {
                writer.write_all(&[byte])?;
            }
        }
        writer.write_all(b"\"")?;
    }
    writer.write_all(b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_commas_quotes_and_newlines_according_to_rfc_4180() {
        let mut csv = Vec::new();
        write_csv_row(
            &mut csv,
            ["comma,here".into(), "a\"b".into(), "one\r\ntwo".into()],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(csv).unwrap(),
            "\"comma,here\",\"a\"\"b\",\"one\r\ntwo\"\r\n"
        );
    }

    #[test]
    fn csv_keeps_numbers_and_guards_formulas_behind_leading_blanks() {
        let mut csv = Vec::new();
        write_csv_row(
            &mut csv,
            [
                "-74.006".into(),
                "-12.5".into(),
                " =HYPERLINK(\"http://x\")".into(),
                "  -1+cmd".into(),
                "plain".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(csv).unwrap(),
            "\"-74.006\",\"-12.5\",\"' =HYPERLINK(\"\"http://x\"\")\",\"'  -1+cmd\",\"plain\"\r\n"
        );
    }

    #[test]
    fn csv_prefixes_formula_and_control_character_cells_and_writes_bom() {
        let report = Report {
            version: 1,
            records: vec![ReportRecord {
                path: "=1+1".into(),
                name: "\tformula".into(),
                ..Default::default()
            }],
            notes: Vec::new(),
        };
        let mut csv = Vec::new();
        write_csv(&mut csv, &report, None).unwrap();
        assert!(csv.starts_with(&[0xEF, 0xBB, 0xBF]));
        let csv = String::from_utf8(csv).unwrap();
        assert!(csv.contains("\"'=1+1\",\"'\tformula\""));
        for value in ["+SUM(A1)", "-1+2", "@name", "\rvalue"] {
            let mut row = Vec::new();
            write_csv_row(&mut row, [value.into()]).unwrap();
            assert!(String::from_utf8(row).unwrap().starts_with("\"'"));
        }
    }

    #[test]
    fn report_json_has_a_stable_versioned_shape() {
        let report = Report {
            version: 1,
            records: vec![ReportRecord::default()],
            notes: Vec::new(),
        };
        let mut bytes = Vec::new();
        write_json(&mut bytes, &report, None).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["version"], 1);
        assert!(json["records"][0].get("gps_lat").is_some());
        assert!(json["records"][0].get("video_note").is_some());
    }
}
