//! Contact sheets, archive browsing/extraction/creation, and file split/join.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont, point};
use crossbeam_channel::{Receiver, unbounded};
use egui::{RichText, Ui};
use forge_core::archive::{ArchiveEntry, ExtractOptions};
use forge_core::model::{EntryKind, MediaKind};
use forge_core::ops::JobCtx;
use forge_core::paths::rename_no_replace;
use forge_jobs::{JobHandle, JobRegistry};

use crate::actions::Event;
use crate::app::ZxrawApp;

static OUTPUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
pub struct ExtraToolsUi {
    contact_sheet: Option<ContactSheetWindow>,
    archive: Option<ArchiveWindow>,
    create_archive: Option<CreateArchiveWindow>,
    split: Option<SplitWindow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SheetFormat {
    Png,
    Jpeg,
}

impl SheetFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
        }
    }
}

#[derive(Debug, Clone)]
struct SheetSource {
    path: PathBuf,
    rating: Option<u8>,
}

#[derive(Debug, Clone)]
struct SheetOptions {
    columns: usize,
    rows: usize,
    thumbnail_size: u32,
    captions_file_name: bool,
    captions_rating: bool,
    captions_exif: bool,
    page_title: String,
    dark_background: bool,
    format: SheetFormat,
    output_dir: PathBuf,
    output_stem: String,
}

struct ContactSheetWindow {
    sources: Vec<SheetSource>,
    options: SheetOptions,
    output_text: String,
    job: Option<JobHandle>,
    open: bool,
    auto_generate: bool,
    auto_started: bool,
}

struct ArchiveWindow {
    path: PathBuf,
    entries: Vec<ArchiveEntry>,
    selected: HashSet<String>,
    list_result: Option<Receiver<std::result::Result<Vec<ArchiveEntry>, String>>>,
    loading_job: Option<JobHandle>,
    extract_job: Option<JobHandle>,
    destination: PathBuf,
    destination_text: String,
    notice: Option<String>,
    open: bool,
}

struct CreateArchiveWindow {
    sources: Vec<PathBuf>,
    directory: PathBuf,
    name: String,
    job: Option<JobHandle>,
    open: bool,
}

struct SplitWindow {
    source: PathBuf,
    preset_mib: u64,
    custom: bool,
    custom_mib: u64,
    job: Option<JobHandle>,
    open: bool,
}

enum UiAction {
    StartContactSheet,
    ExtractArchive {
        selected_only: bool,
        destination: PathBuf,
        entries: Vec<ArchiveEntry>,
        selected_names: HashSet<String>,
    },
    StartCreateArchive {
        sources: Vec<PathBuf>,
        directory: PathBuf,
        name: String,
    },
    StartSplit {
        source: PathBuf,
        bytes: u64,
    },
}

/// Called every frame.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    let jobs = app.ops.jobs.clone();
    let event_tx = app.ops.tx.clone();
    let app_ctx = app.ops.ctx.clone();
    let mut actions = Vec::new();
    let tools = &mut app.ops.extra_tools;

    if let Some(state) = &mut tools.contact_sheet {
        if state.auto_generate && !state.auto_started {
            state.auto_started = true;
            actions.push(UiAction::StartContactSheet);
        }
        render_contact_sheet(state, ctx, &mut actions);
        if !state.open {
            tools.contact_sheet = None;
        }
    }

    if let Some(state) = &mut tools.archive {
        if let Some(rx) = &state.list_result
            && let Ok(result) = rx.try_recv()
        {
            match result {
                Ok(entries) => {
                    state.entries = entries;
                    state.list_result = None;
                }
                Err(error) => {
                    state.notice = Some(error);
                    state.list_result = None;
                }
            }
        }
        render_archive(state, ctx, &mut actions);
        if !state.open {
            tools.archive = None;
        }
    }

    if let Some(state) = &mut tools.create_archive {
        render_create_archive(state, ctx, &mut actions);
        if !state.open {
            tools.create_archive = None;
        }
    }

    if let Some(state) = &mut tools.split {
        render_split(state, ctx, &mut actions);
        if !state.open {
            tools.split = None;
        }
    }

    for action in actions {
        match action {
            UiAction::StartContactSheet => {
                if let Some(state) = tools.contact_sheet.as_mut() {
                    let job = start_contact_sheet(
                        &jobs,
                        &event_tx,
                        &app_ctx,
                        &state.sources,
                        &state.options,
                    );
                    state.job = Some(job);
                    state.auto_generate = false;
                }
            }
            UiAction::ExtractArchive {
                selected_only,
                destination,
                entries,
                selected_names,
            } => {
                if let Some(state) = tools.archive.as_mut() {
                    let job = start_extract_archive(
                        &jobs,
                        &event_tx,
                        &app_ctx,
                        &state.path,
                        destination,
                        entries,
                        selected_names,
                    );
                    state.extract_job = Some(job);
                    state.notice = Some(if selected_only {
                        "Extracting selected entries through the safety-checked staging area…"
                            .into()
                    } else {
                        "Extracting archive…".into()
                    });
                }
            }
            UiAction::StartCreateArchive {
                sources,
                directory,
                name,
            } => {
                if let Some(state) = tools.create_archive.as_mut() {
                    let job =
                        start_create_archive(&jobs, &event_tx, &app_ctx, sources, directory, name);
                    state.job = Some(job);
                }
            }
            UiAction::StartSplit { source, bytes } => {
                if let Some(state) = tools.split.as_mut() {
                    let job = start_split(&jobs, &event_tx, &app_ctx, source, bytes);
                    state.job = Some(job);
                }
            }
        }
    }

    if has_live_tool_job(&app.ops.extra_tools) {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

fn has_live_tool_job(tools: &ExtraToolsUi) -> bool {
    tools
        .contact_sheet
        .as_ref()
        .and_then(|w| w.job.as_ref())
        .is_some_and(|j| !j.state().is_finished())
        || tools.archive.as_ref().is_some_and(|w| {
            w.loading_job
                .as_ref()
                .is_some_and(|j| !j.state().is_finished())
                || w.extract_job
                    .as_ref()
                    .is_some_and(|j| !j.state().is_finished())
        })
        || tools
            .create_archive
            .as_ref()
            .and_then(|w| w.job.as_ref())
            .is_some_and(|j| !j.state().is_finished())
        || tools
            .split
            .as_ref()
            .and_then(|w| w.job.as_ref())
            .is_some_and(|j| !j.state().is_finished())
}

/// `Cmd::ContactSheet` for the selected images, or all images in the folder.
pub fn contact_sheet(app: &mut ZxrawApp) {
    let pane = app.pane();
    let selected = pane.selected_entries();
    let entries: Vec<_> = if selected.is_empty() {
        pane.entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::File && is_image(entry.media_kind()))
            .collect()
    } else {
        selected
            .into_iter()
            .filter(|entry| entry.kind == EntryKind::File && is_image(entry.media_kind()))
            .collect()
    };
    if entries.is_empty() {
        app.toast_err("Select images, or clear the selection to use all images in this folder");
        return;
    }
    let sources: Vec<_> = entries
        .iter()
        .map(|entry| SheetSource {
            path: entry.path.clone(),
            rating: app
                .ops
                .library
                .annotations
                .get(&entry.path)
                .map(|a| a.rating)
                .filter(|r| *r > 0),
        })
        .collect();
    let source_title = pane.title();
    let test_auto = screenshot_contact_sheet_requested();
    let output_dir = if test_auto {
        std::env::temp_dir().join("ZXraw Explorer Contact Sheets")
    } else {
        pane.path.clone()
    };
    let mut options = SheetOptions {
        columns: 4,
        rows: 3,
        thumbnail_size: 256,
        captions_file_name: true,
        captions_rating: false,
        captions_exif: false,
        page_title: source_title,
        dark_background: true,
        format: SheetFormat::Png,
        output_dir: output_dir.clone(),
        output_stem: "contact-sheet".into(),
    };
    if test_auto {
        options.columns = 6;
        options.rows = 6;
        options.thumbnail_size = 128;
        options.page_title = "Forge demo photos".into();
    }
    app.ops.extra_tools.contact_sheet = Some(ContactSheetWindow {
        sources,
        options,
        output_text: output_dir.to_string_lossy().into_owned(),
        job: None,
        open: true,
        auto_generate: test_auto,
        auto_started: false,
    });
}

fn screenshot_contact_sheet_requested() -> bool {
    std::env::var_os("ZXRAW_SCREENSHOT").is_some()
        && std::env::var("ZXRAW_CMDS")
            .ok()
            .is_some_and(|s| s.split(',').any(|cmd| cmd.trim() == "ContactSheet"))
}

fn is_image(kind: MediaKind) -> bool {
    matches!(kind, MediaKind::Image | MediaKind::Raw)
}

/// `Cmd::BrowseArchive` / opening a selected archive: show its contents.
pub fn browse_archive(app: &mut ZxrawApp) {
    let target = pane_action_target(app);
    let Some(path) = target else {
        app.toast_err("Select a ZIP archive to browse");
        return;
    };
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ext == "7z" || ext == "rar" {
        app.toast_err(format!("{ext} archive browsing is not supported yet"));
        return;
    }
    if ext != "zip" {
        app.toast_err("Archive browsing currently supports ZIP files");
        return;
    }
    let jobs = app.ops.jobs.clone();
    let app_ctx = app.ops.ctx.clone();
    app.ops.extra_tools.archive = Some(open_archive_state(&jobs, &app_ctx, path));
}

/// Create a ZIP archive from the current selection.
pub fn create_archive(app: &mut ZxrawApp) {
    let sources = pane_action_targets(app);
    if sources.is_empty() {
        app.toast_err("Select files or folders to add to a ZIP archive");
        return;
    }
    let directory = app.pane().path.clone();
    let stem = app.pane().title();
    app.ops.extra_tools.create_archive = Some(CreateArchiveWindow {
        sources,
        directory,
        name: format!("{stem}.zip"),
        job: None,
        open: true,
    });
}

/// Open the split dialog for the selected file.
pub fn split_file(app: &mut ZxrawApp) {
    let Some(source) = pane_action_target(app) else {
        app.toast_err("Select a file to split");
        return;
    };
    if !source.is_file() {
        app.toast_err("Only regular files can be split");
        return;
    }
    app.ops.extra_tools.split = Some(SplitWindow {
        source,
        preset_mib: 700,
        custom: false,
        custom_mib: 1024,
        job: None,
        open: true,
    });
}

/// Join a selected `.b3split` manifest or its `.001` first part.
pub fn join_files(app: &mut ZxrawApp) {
    let Some(input) = pane_action_target(app) else {
        app.toast_err("Select a .b3split manifest or the first .001 part");
        return;
    };
    let jobs = app.ops.jobs.clone();
    let event_tx = app.ops.tx.clone();
    let app_ctx = app.ops.ctx.clone();
    let input_for_job = input.clone();
    let job = jobs.spawn(
        "file-join",
        format!("Joining {}", input.display()),
        move |job| {
            let ctx = JobCtx::from_job(job);
            match forge_core::split::join_files(&input_for_job, &ctx) {
                Ok(report) => {
                    let _ = event_tx.send(Event::Toast(format!(
                        "Joined {} part(s) to {}",
                        report.parts,
                        report.output.display()
                    )));
                    if let Some(parent) = report.output.parent() {
                        let _ = event_tx.send(Event::Changed(parent.to_path_buf()));
                    }
                    app_ctx.request_repaint();
                    Ok(())
                }
                Err(error) => {
                    let message = error.to_string();
                    let _ = event_tx.send(Event::Error(message.clone()));
                    app_ctx.request_repaint();
                    Err(message)
                }
            }
        },
    );
    app.toast(format!("Started {}", job.title));
}

fn pane_action_target(app: &ZxrawApp) -> Option<PathBuf> {
    pane_action_targets(app).into_iter().next()
}

fn pane_action_targets(app: &ZxrawApp) -> Vec<PathBuf> {
    app.pane().action_targets()
}

fn render_contact_sheet(
    state: &mut ContactSheetWindow,
    ctx: &egui::Context,
    actions: &mut Vec<UiAction>,
) {
    let mut open = state.open;
    egui::Window::new("Contact sheet")
        .id(egui::Id::new("extra-contact-sheet"))
        .open(&mut open)
        .default_size([520.0, 620.0])
        .resizable(true)
        .show(ctx, |ui| {
            ui.label(format!("{} image(s)", state.sources.len()));
            ui.horizontal(|ui| {
                ui.label("Columns");
                ui.add(egui::DragValue::new(&mut state.options.columns).range(1..=10));
                ui.label("Rows");
                ui.add(egui::DragValue::new(&mut state.options.rows).range(1..=10));
            });
            ui.horizontal(|ui| {
                ui.label("Thumbnail");
                ui.add(
                    egui::DragValue::new(&mut state.options.thumbnail_size)
                        .range(64..=512)
                        .speed(16),
                );
                ui.label("px");
            });
            ui.separator();
            ui.label("Captions");
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut state.options.captions_file_name, "File name");
                ui.checkbox(&mut state.options.captions_rating, "Rating");
                ui.checkbox(
                    &mut state.options.captions_exif,
                    "Date, camera and exposure",
                );
            });
            ui.horizontal(|ui| {
                ui.label("Page title");
                ui.text_edit_singleline(&mut state.options.page_title);
            });
            ui.horizontal(|ui| {
                ui.label("Background");
                ui.selectable_value(&mut state.options.dark_background, true, "Dark");
                ui.selectable_value(&mut state.options.dark_background, false, "Light");
            });
            ui.horizontal(|ui| {
                ui.label("Format");
                ui.selectable_value(&mut state.options.format, SheetFormat::Png, "PNG");
                ui.selectable_value(&mut state.options.format, SheetFormat::Jpeg, "JPEG");
            });
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut state.options.output_stem);
            });
            ui.horizontal(|ui| {
                ui.label("Output folder");
                ui.text_edit_singleline(&mut state.output_text);
                if ui.button("Choose…").clicked()
                    && let Some(path) = crate::dialogs::pick_folder(
                        rfd::FileDialog::new().set_directory(&state.options.output_dir),
                    )
                {
                    state.output_text = path.to_string_lossy().into_owned();
                }
            });
            if state.job.is_none() {
                if ui.button("Create contact sheet").clicked() {
                    state.options.output_dir = PathBuf::from(state.output_text.trim());
                    actions.push(UiAction::StartContactSheet);
                }
            } else if let Some(job) = &state.job {
                draw_job_progress(ui, job);
            }
            if let Some(job) = &state.job
                && job.state().is_finished()
            {
                for line in job.log_lines().iter().rev().take(3).rev() {
                    ui.small(line);
                }
            }
        });
    state.open = open;
}

fn start_contact_sheet(
    jobs: &JobRegistry,
    event_tx: &crossbeam_channel::Sender<Event>,
    app_ctx: &egui::Context,
    sources: &[SheetSource],
    options: &SheetOptions,
) -> JobHandle {
    let sources = sources.to_vec();
    let options = options.clone();
    let tx = event_tx.clone();
    let ctx = app_ctx.clone();
    let title = format!("Contact sheet: {} image(s)", sources.len());
    jobs.spawn("contact-sheet", title, move |job| {
        let job_ctx = JobCtx::from_job(job);
        match build_contact_sheets(&sources, &options, &job_ctx) {
            Ok(paths) => {
                let message = if paths.len() == 1 {
                    format!("Contact sheet saved: {}", paths[0].display())
                } else {
                    format!(
                        "Saved {} contact-sheet pages to {}",
                        paths.len(),
                        options.output_dir.display()
                    )
                };
                let _ = tx.send(Event::Toast(message));
                let _ = tx.send(Event::Changed(options.output_dir.clone()));
                if let Err(error) = forge_win::open_default(&options.output_dir) {
                    let _ = tx.send(Event::Error(format!(
                        "Could not open result folder: {error}"
                    )));
                }
                ctx.request_repaint();
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                let _ = tx.send(Event::Error(format!("Contact sheet failed: {message}")));
                ctx.request_repaint();
                Err(message)
            }
        }
    })
}

fn build_contact_sheets(
    sources: &[SheetSource],
    options: &SheetOptions,
    ctx: &JobCtx<'_>,
) -> forge_core::Result<Vec<PathBuf>> {
    if sources.is_empty() {
        return Err(forge_core::CoreError::Invalid(
            "no images were selected".into(),
        ));
    }
    fs::create_dir_all(&options.output_dir)
        .map_err(|e| forge_core::CoreError::io(&options.output_dir, e))?;
    let output_md = fs::symlink_metadata(&options.output_dir)
        .map_err(|e| forge_core::CoreError::io(&options.output_dir, e))?;
    if !output_md.is_dir() || output_md.file_type().is_symlink() {
        return Err(forge_core::CoreError::Invalid(
            "contact-sheet output must be a real folder".into(),
        ));
    }
    let layout = sheet_layout(
        options.columns,
        options.rows,
        options.thumbnail_size,
        2,
        !options.page_title.trim().is_empty(),
    );
    debug_assert_eq!(layout.slots, layout.columns * layout.rows);
    let ranges = page_ranges(sources.len(), layout.columns, layout.rows);
    let page_paths = unique_sheet_paths(options, ranges.len());
    ctx.progress
        .add_total_items((sources.len() + ranges.len()) as u64);
    let mut cleanup = OutputCleanup {
        paths: Vec::with_capacity(page_paths.len()),
        keep: false,
    };
    // Fonts are read once per job, not per glyph or per page.
    let text = TextEngine::load();

    let mut tiles = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        ctx.cancel.check()?;
        ctx.progress.set_current(format!(
            "Preparing {} ({}/{})",
            source
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            index + 1,
            sources.len()
        ));
        let meta = if options.captions_exif {
            forge_media::read_image_metadata(&source.path).ok()
        } else {
            None
        };
        let file_name = source
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut details = Vec::new();
        if options.captions_rating
            && let Some(rating) = source.rating
        {
            details.push(format!("Rating {rating}/5"));
        }
        if let Some(meta) = meta {
            if let Some(date) = meta.date_taken {
                details.push(date.replace('T', " "));
            }
            let camera = match (meta.make, meta.model) {
                (Some(make), Some(model))
                    if !model.to_lowercase().contains(&make.to_lowercase()) =>
                {
                    Some(format!("{make} {model}"))
                }
                (_, Some(model)) => Some(model),
                (Some(make), None) => Some(make),
                _ => None,
            };
            if let Some(camera) = camera {
                details.push(camera);
            }
            let mut exposure = Vec::new();
            if let Some(shutter) = meta.shutter {
                exposure.push(shutter);
            } else if let Some(seconds) = meta.exposure_time_s {
                exposure.push(format!("{seconds:.3}s"));
            }
            if let Some(aperture) = meta.aperture {
                exposure.push(format!("f/{aperture:.1}"));
            }
            if let Some(iso) = meta.iso {
                exposure.push(format!("ISO {iso}"));
            }
            if !exposure.is_empty() {
                details.push(exposure.join(" "));
            }
        }
        let name = options.captions_file_name.then_some(file_name);
        let details = (!details.is_empty()).then(|| details.join(" · "));
        tiles.push(SheetTile {
            image: None,
            name,
            details,
        });
    }

    let mut output_paths = Vec::with_capacity(ranges.len());
    for (page_index, range) in ranges.iter().enumerate() {
        ctx.cancel.check()?;
        ctx.progress.set_current(format!(
            "Rendering page {} of {}",
            page_index + 1,
            ranges.len()
        ));
        let page_tiles = load_sheet_page(range.clone(), |index| {
            ctx.cancel.check()?;
            let source = &sources[index];
            let image = forge_media::generate_thumbnail(
                &source.path,
                options.thumbnail_size,
                &forge_media::DecodeLimits::default(),
                None,
                0,
                Some(ctx.cancel),
            )
            .ok()
            .and_then(|thumbnail| thumbnail.to_rgba_image());
            if image.is_none() {
                ctx.progress.add_error();
                ctx.log(format!(
                    "{}: could not decode image; a placeholder was included",
                    source.path.display()
                ));
            }
            ctx.progress.add_items(1);
            Ok(SheetTile {
                image,
                name: tiles[index].name.clone(),
                details: tiles[index].details.clone(),
            })
        })?;
        let title = if options.page_title.trim().is_empty() {
            String::new()
        } else if ranges.len() > 1 {
            format!(
                "{}  ·  Page {} of {}",
                options.page_title.trim(),
                page_index + 1,
                ranges.len()
            )
        } else {
            options.page_title.trim().to_owned()
        };
        let page = render_sheet_page(&page_tiles, layout, &title, options.dark_background, &text);
        let mut encoded = Cursor::new(Vec::new());
        match options.format {
            SheetFormat::Png => image::DynamicImage::ImageRgba8(page)
                .write_to(&mut encoded, image::ImageFormat::Png)
                .map_err(|e| forge_core::CoreError::Invalid(format!("PNG encode failed: {e}")))?,
            SheetFormat::Jpeg => {
                let rgb = image::DynamicImage::ImageRgba8(page).to_rgb8();
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 92)
                    .encode_image(&image::DynamicImage::ImageRgb8(rgb))
                    .map_err(|e| {
                        forge_core::CoreError::Invalid(format!("JPEG encode failed: {e}"))
                    })?;
            }
        }
        let path = &page_paths[page_index];
        publish_bytes_no_replace(path, &encoded.into_inner())
            .map_err(|error| forge_core::CoreError::io(path, error))?;
        cleanup.paths.push(path.clone());
        output_paths.push(path.clone());
        ctx.progress.add_items(1);
    }
    ctx.progress.set_current("Contact sheet complete");
    cleanup.keep = true;
    Ok(output_paths)
}

fn load_sheet_page<T>(
    range: Range<usize>,
    mut load: impl FnMut(usize) -> forge_core::Result<T>,
) -> forge_core::Result<Vec<T>> {
    let mut page = Vec::with_capacity(range.len());
    for index in range {
        page.push(load(index)?);
    }
    Ok(page)
}

fn publish_bytes_no_replace(destination: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    loop {
        let sequence = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".zxraw-output-{}-{sequence:016x}.tmp",
            std::process::id()
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        drop(file);
        match rename_no_replace(&temporary, destination) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
        }
    }
}

struct OutputCleanup {
    paths: Vec<PathBuf>,
    keep: bool,
}

impl Drop for OutputCleanup {
    fn drop(&mut self) {
        if !self.keep {
            for path in self.paths.iter().rev() {
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[derive(Clone)]
struct SheetTile {
    image: Option<image::RgbaImage>,
    /// File name line; middle-shortened so the extension stays visible.
    name: Option<String>,
    /// Secondary line with date, camera and exposure or rating.
    details: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SheetLayout {
    columns: usize,
    rows: usize,
    slots: usize,
    thumbnail_size: u32,
    cell_width: u32,
    cell_height: u32,
    width: u32,
    height: u32,
    header_height: u32,
}

fn sheet_layout(
    columns: usize,
    rows: usize,
    thumbnail_size: u32,
    caption_lines: usize,
    title: bool,
) -> SheetLayout {
    let columns = columns.clamp(1, 10);
    let rows = rows.clamp(1, 10);
    let thumbnail_size = thumbnail_size.clamp(64, 512);
    let cell_width = thumbnail_size.saturating_add(28);
    let caption_height = (caption_lines.clamp(0, 3) as u32).saturating_mul(24);
    let cell_height = thumbnail_size
        .saturating_add(caption_height)
        .saturating_add(22);
    let header_height: u32 = if title { 58 } else { 22 };
    let width = 28u32
        .saturating_add(columns as u32 * cell_width)
        .saturating_add(28)
        .min(16_384);
    let height = header_height
        .saturating_add(22)
        .saturating_add(rows as u32 * cell_height)
        .saturating_add(28)
        .min(16_384);
    SheetLayout {
        columns,
        rows,
        slots: columns * rows,
        thumbnail_size,
        cell_width,
        cell_height,
        width,
        height,
        header_height,
    }
}

fn page_ranges(count: usize, columns: usize, rows: usize) -> Vec<Range<usize>> {
    let capacity = columns.clamp(1, 10) * rows.clamp(1, 10);
    (0..count)
        .step_by(capacity)
        .map(|start| start..(start + capacity).min(count))
        .collect()
}

fn unique_sheet_paths(options: &SheetOptions, pages: usize) -> Vec<PathBuf> {
    let stem = sanitize_filename(&options.output_stem);
    for suffix in 1..100_000usize {
        let candidate = if suffix == 1 {
            stem.clone()
        } else {
            format!("{stem} ({suffix})")
        };
        let paths: Vec<_> = (0..pages)
            .map(|page| {
                let name = if pages == 1 {
                    format!("{candidate}.{}", options.format.extension())
                } else {
                    format!("{candidate}-{:03}.{}", page + 1, options.format.extension())
                };
                options.output_dir.join(name)
            })
            .collect();
        if paths.iter().all(|p| fs::symlink_metadata(p).is_err()) {
            return paths;
        }
    }
    (0..pages)
        .map(|page| {
            options.output_dir.join(format!(
                "contact-sheet-{}-{:03}.{}",
                std::process::id(),
                page + 1,
                options.format.extension()
            ))
        })
        .collect()
}

fn sanitize_filename(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.');
    if cleaned.is_empty() {
        "contact-sheet".into()
    } else {
        cleaned.into()
    }
}

const TITLE_PX: f32 = 24.0;
const NAME_PX: f32 = 14.0;
const DETAIL_PX: f32 = 12.5;
/// Vertical distance between caption lines, measured from the top of each line box.
const CAPTION_LINE_HEIGHT: f32 = 22.0;

fn render_sheet_page(
    tiles: &[SheetTile],
    layout: SheetLayout,
    title: &str,
    dark: bool,
    text: &TextEngine,
) -> image::RgbaImage {
    let bg = if dark {
        image::Rgba([28, 31, 36, 255])
    } else {
        image::Rgba([244, 245, 247, 255])
    };
    let tile_bg = if dark {
        image::Rgba([42, 46, 53, 255])
    } else {
        image::Rgba([255, 255, 255, 255])
    };
    let ink = if dark {
        image::Rgba([236, 238, 242, 255])
    } else {
        image::Rgba([28, 32, 38, 255])
    };
    let muted = if dark {
        image::Rgba([170, 178, 189, 255])
    } else {
        image::Rgba([88, 96, 106, 255])
    };
    let mut canvas = image::RgbaImage::from_pixel(layout.width, layout.height, bg);
    if !title.is_empty() {
        let title_box = TextBox {
            x: 24.0,
            y: 18.0,
            width: layout.width.saturating_sub(48) as f32,
            px: TITLE_PX,
            bold: true,
            color: ink,
        };
        draw_text_fit(&mut canvas, text, title, title_box, Fit::End);
    }
    for (index, tile) in tiles.iter().enumerate() {
        let col = index % layout.columns;
        let row = index / layout.columns;
        let x = 28 + col as u32 * layout.cell_width;
        let y = layout.header_height + 12 + row as u32 * layout.cell_height;
        fill_rect(
            &mut canvas,
            x,
            y,
            layout.cell_width.saturating_sub(8),
            layout.cell_height.saturating_sub(6),
            tile_bg,
        );
        if let Some(img) = &tile.image {
            let ix = x + 14 + (layout.thumbnail_size.saturating_sub(img.width()) / 2);
            let iy = y + 10 + (layout.thumbnail_size.saturating_sub(img.height()) / 2);
            image::imageops::overlay(&mut canvas, img, i64::from(ix), i64::from(iy));
        } else {
            fill_rect(
                &mut canvas,
                x + 14,
                y + 10,
                layout.thumbnail_size,
                layout.thumbnail_size,
                if dark {
                    image::Rgba([68, 72, 80, 255])
                } else {
                    image::Rgba([220, 223, 227, 255])
                },
            );
            // Centred in the thumbnail box, vertically around its middle.
            let label = "Image unavailable";
            let label_width = text.width(label, NAME_PX, false);
            let label_box = TextBox {
                x: (x + 14) as f32 + (layout.thumbnail_size as f32 - label_width).max(0.0) / 2.0,
                y: (y + 10) as f32 + layout.thumbnail_size as f32 / 2.0 - NAME_PX / 2.0,
                width: layout.thumbnail_size as f32,
                px: NAME_PX,
                bold: false,
                color: muted,
            };
            draw_text_fit(&mut canvas, text, label, label_box, Fit::End);
        }
        // Captions sit under the thumbnail, aligned with its left edge.
        let mut caption_y = (y + 18 + layout.thumbnail_size) as f32;
        let caption_box = |caption_y: f32, px: f32, color| TextBox {
            x: (x + 14) as f32,
            y: caption_y,
            width: layout.thumbnail_size as f32,
            px,
            bold: false,
            color,
        };
        if let Some(name) = &tile.name {
            let boxed = caption_box(caption_y, NAME_PX, ink);
            draw_text_fit(&mut canvas, text, name, boxed, Fit::FileName);
            caption_y += CAPTION_LINE_HEIGHT;
        }
        if let Some(details) = &tile.details {
            // Secondary detail is muted unless it is the only caption line.
            let color = if tile.name.is_some() { muted } else { ink };
            let boxed = caption_box(caption_y, DETAIL_PX, color);
            draw_text_fit(&mut canvas, text, details, boxed, Fit::End);
        }
    }
    canvas
}

fn fill_rect(
    img: &mut image::RgbaImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    color: image::Rgba<u8>,
) {
    let x_end = x.saturating_add(width).min(img.width());
    let y_end = y.saturating_add(height).min(img.height());
    for py in y.min(img.height())..y_end {
        for px in x.min(img.width())..x_end {
            img.put_pixel(px, py, color);
        }
    }
}

/// Windows UI faces as `(file, collection index, bold)`. Faces are tried in order for each
/// character; the first one that has the glyph is used, so CJK and symbols fall through.
const SYSTEM_FONTS: &[(&str, u32, bool)] = &[
    ("segoeui.ttf", 0, false),
    ("segoeuib.ttf", 0, true),
    ("msyh.ttc", 0, false),
    ("msyhbd.ttc", 0, true),
    ("YuGothM.ttc", 0, false),
    ("malgun.ttf", 0, false),
    ("malgunbd.ttf", 0, true),
    ("seguisym.ttf", 0, false),
];

/// Shown when a caption is cut. A single character, so it never reads as "..".
const ELLIPSIS: char = '…';

/// Characters of a file-name stem kept after the middle ellipsis (usually a counter).
const MIDDLE_TAIL_CHARS: usize = 5;

/// Text backend for captions and titles. Built once per sheet job.
enum TextEngine {
    Vector(SystemFonts),
    /// Used when no Windows UI font can be read (non-Windows or missing fonts).
    Bitmap,
}

struct SystemFonts {
    faces: Vec<FontVec>,
    /// Face indices in lookup order for regular and bold text. Both orders include every face.
    regular: Vec<usize>,
    bold: Vec<usize>,
}

/// A glyph placed in a laid-out line; `x` is relative to the line start.
struct PlacedGlyph {
    face: usize,
    id: GlyphId,
    x: f32,
}

#[derive(Clone, Copy)]
struct TextBox {
    x: f32,
    /// Top of the line box; the baseline sits at `y` plus the font ascent.
    y: f32,
    width: f32,
    px: f32,
    bold: bool,
    color: image::Rgba<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    /// Plain end ellipsis.
    End,
    /// Keeps the extension and the end of the stem: "Very long name…_0001.jpg".
    FileName,
}

impl TextEngine {
    fn load() -> Self {
        SystemFonts::load().map_or(Self::Bitmap, Self::Vector)
    }

    /// Advance width of `text` in pixels.
    fn width(&self, text: &str, px: f32, bold: bool) -> f32 {
        match self {
            Self::Vector(fonts) => fonts.layout(text, px, bold).1,
            Self::Bitmap => bitmap_cell_width(px) * text.chars().count() as f32,
        }
    }

    fn draw(&self, img: &mut image::RgbaImage, text: &str, boxed: TextBox) {
        match self {
            Self::Vector(fonts) => fonts.draw(img, text, boxed),
            Self::Bitmap => draw_bitmap_text(img, text, boxed),
        }
    }
}

impl SystemFonts {
    fn load() -> Option<Self> {
        let dir = windows_font_dir();
        let mut faces = Vec::new();
        let mut bold_flags = Vec::new();
        for &(file, index, bold) in SYSTEM_FONTS {
            let Ok(data) = fs::read(dir.join(file)) else {
                continue;
            };
            let Ok(face) = FontVec::try_from_vec_and_index(data, index) else {
                continue;
            };
            faces.push(face);
            bold_flags.push(bold);
        }
        if faces.is_empty() {
            return None;
        }
        let order = |want_bold: bool| -> Vec<usize> {
            (0..faces.len())
                .filter(|&i| bold_flags[i] == want_bold)
                .chain((0..faces.len()).filter(|&i| bold_flags[i] != want_bold))
                .collect()
        };
        Some(Self {
            regular: order(false),
            bold: order(true),
            faces,
        })
    }

    fn order(&self, bold: bool) -> &[usize] {
        if bold { &self.bold } else { &self.regular }
    }

    /// First face in `order` that has `ch`; falls back to the primary face (its .notdef glyph).
    fn pick(&self, order: &[usize], ch: char) -> usize {
        order
            .iter()
            .copied()
            .find(|&i| self.faces[i].glyph_id(ch).0 != 0)
            .unwrap_or(order[0])
    }

    /// Places the glyphs of `text` with kerning (within one face) and returns the advance width.
    fn layout(&self, text: &str, px: f32, bold: bool) -> (Vec<PlacedGlyph>, f32) {
        let scale = PxScale::from(px);
        let order = self.order(bold);
        let mut placed = Vec::with_capacity(text.len());
        let mut pen = 0.0f32;
        let mut prev: Option<(usize, GlyphId)> = None;
        for ch in text.chars() {
            let ch = if ch.is_control() { ' ' } else { ch };
            let face = self.pick(order, ch);
            let font = self.faces[face].as_scaled(scale);
            let id = self.faces[face].glyph_id(ch);
            if let Some((prev_face, prev_id)) = prev
                && prev_face == face
            {
                pen += font.kern(prev_id, id);
            }
            placed.push(PlacedGlyph { face, id, x: pen });
            pen += font.h_advance(id);
            prev = Some((face, id));
        }
        (placed, pen)
    }

    fn draw(&self, img: &mut image::RgbaImage, text: &str, boxed: TextBox) {
        let scale = PxScale::from(boxed.px);
        let primary = self.order(boxed.bold)[0];
        let baseline = boxed.y + self.faces[primary].as_scaled(scale).ascent();
        let (glyphs, _) = self.layout(text, boxed.px, boxed.bold);
        for glyph in glyphs {
            let face = &self.faces[glyph.face];
            let positioned = glyph
                .id
                .with_scale_and_position(scale, point(boxed.x + glyph.x, baseline));
            let Some(outlined) = face.outline_glyph(positioned) else {
                continue;
            };
            let bounds = outlined.px_bounds();
            let (left, top) = (bounds.min.x.floor() as i64, bounds.min.y.floor() as i64);
            outlined.draw(|gx, gy, coverage| {
                blend_pixel(
                    img,
                    left + i64::from(gx),
                    top + i64::from(gy),
                    coverage,
                    boxed.color,
                );
            });
        }
    }
}

fn windows_font_dir() -> PathBuf {
    let windir =
        std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    windir.join("Fonts")
}

/// Source-over blend of `color` at `coverage` onto an opaque pixel; alpha is left unchanged.
fn blend_pixel(img: &mut image::RgbaImage, x: i64, y: i64, coverage: f32, color: image::Rgba<u8>) {
    if coverage <= 0.0
        || x < 0
        || y < 0
        || x >= i64::from(img.width())
        || y >= i64::from(img.height())
    {
        return;
    }
    let alpha = coverage.min(1.0);
    let dst = img.get_pixel_mut(x as u32, y as u32);
    for channel in 0..3 {
        let fg = f32::from(color.0[channel]);
        let bg = f32::from(dst.0[channel]);
        dst.0[channel] = (fg * alpha + bg * (1.0 - alpha)).round() as u8;
    }
}

/// Draws `text` into `boxed.width`, shortening it when it does not fit.
fn draw_text_fit(
    img: &mut image::RgbaImage,
    engine: &TextEngine,
    text: &str,
    boxed: TextBox,
    fit: Fit,
) {
    let measure = |s: &str| engine.width(s, boxed.px, boxed.bold);
    let fitted = match fit {
        Fit::End => fit_end(text, boxed.width, measure),
        Fit::FileName => fit_file_name(text, boxed.width, measure),
    };
    if !fitted.is_empty() {
        engine.draw(img, &fitted, boxed);
    }
}

/// Joins the kept `head` (trailing spaces trimmed), one ellipsis and `tail`.
fn join_ellipsis(head: &[char], tail: &str) -> String {
    let mut out: String = head.iter().collect();
    let kept = out.trim_end().len();
    out.truncate(kept);
    out.push(ELLIPSIS);
    out.push_str(tail);
    out
}

/// Cuts `text` at a char boundary and appends one "…" so it fits `max_width`.
/// Returns an empty string when not even the ellipsis fits.
fn fit_end<M: Fn(&str) -> f32>(text: &str, max_width: f32, measure: M) -> String {
    if measure(text) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len())
        .rev()
        .map(|keep| join_ellipsis(&chars[..keep], ""))
        .find(|candidate| measure(candidate) <= max_width)
        .unwrap_or_default()
}

/// Shortens a file name from the middle so the extension and the last few stem characters
/// survive: "Very long name with many words_0001.jpg" → "Very long name…_0001.jpg".
fn fit_file_name<M: Fn(&str) -> f32>(name: &str, max_width: f32, measure: M) -> String {
    if measure(name) <= max_width {
        return name.to_owned();
    }
    let (stem, ext) = split_extension(name);
    let stem: Vec<char> = stem.chars().collect();
    let tail_len = MIDDLE_TAIL_CHARS.min(stem.len() / 2);
    let head_len = stem.len() - tail_len;
    let tail = stem[head_len..].iter().collect::<String>() + ext;
    (0..head_len)
        .rev()
        .map(|keep| join_ellipsis(&stem[..keep], &tail))
        .find(|candidate| measure(candidate) <= max_width)
        .unwrap_or_else(|| fit_end(name, max_width, &measure))
}

/// Splits `name.ext` into `("name", ".ext")`. The extension is empty when there is no
/// plausible one (no dot, leading dot, or an implausibly long or spaced suffix).
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot)
            if dot > 0
                && name.len() - dot <= 10
                && !name[dot..].chars().any(char::is_whitespace) =>
        {
            (&name[..dot], &name[dot..])
        }
        _ => (name, ""),
    }
}

/// Bitmap-font scale: about 8 px per glyph cell, at least 1.
fn bitmap_scale(px: f32) -> u32 {
    (px / 8.0).round().max(1.0) as u32
}

fn bitmap_cell_width(px: f32) -> f32 {
    (6 * bitmap_scale(px)) as f32
}

fn draw_bitmap_text(img: &mut image::RgbaImage, text: &str, boxed: TextBox) {
    let scale = bitmap_scale(boxed.px);
    let x = boxed.x.max(0.0).round() as u32;
    let y = boxed.y.max(0.0).round() as u32;
    for (i, ch) in text.chars().enumerate() {
        draw_glyph(img, ch, x + i as u32 * 6 * scale, y, scale, boxed.color);
    }
}

fn draw_glyph(
    img: &mut image::RgbaImage,
    ch: char,
    x: u32,
    y: u32,
    scale: u32,
    color: image::Rgba<u8>,
) {
    let rows = glyph_rows(ch);
    for (gy, row) in rows.iter().enumerate() {
        for gx in 0..5 {
            if row & (1 << (4 - gx)) == 0 {
                continue;
            }
            fill_rect(
                img,
                x + gx * scale,
                y + gy as u32 * scale,
                scale,
                scale,
                color,
            );
        }
    }
}

fn glyph_rows(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [14, 4, 4, 4, 4, 4, 14],
        'J' => [7, 2, 2, 2, 18, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '_' => [0, 0, 0, 0, 0, 0, 31],
        '.' => [0, 0, 0, 0, 0, 12, 12],
        ',' => [0, 0, 0, 0, 4, 4, 8],
        ':' => [0, 12, 12, 0, 12, 12, 0],
        '/' => [1, 2, 2, 4, 8, 8, 16],
        '\\' => [16, 8, 8, 4, 2, 2, 1],
        '(' => [2, 4, 8, 8, 8, 4, 2],
        ')' => [8, 4, 2, 2, 2, 4, 8],
        '+' => [0, 4, 4, 31, 4, 4, 0],
        '!' => [4, 4, 4, 4, 4, 0, 4],
        '…' => [0, 0, 0, 0, 0, 0, 21],
        '?' => [14, 17, 1, 2, 4, 0, 4],
        '[' => [14, 8, 8, 8, 8, 8, 14],
        ']' => [14, 2, 2, 2, 2, 2, 14],
        ' ' => [0; 7],
        _ => [31, 17, 1, 2, 4, 0, 4],
    }
}

fn open_archive_state(jobs: &JobRegistry, app_ctx: &egui::Context, path: PathBuf) -> ArchiveWindow {
    let (result_tx, result_rx) = unbounded();
    let path_for_job = path.clone();
    let result_tx_for_job = result_tx.clone();
    let ctx = app_ctx.clone();
    let job = jobs.spawn(
        "archive-list",
        format!("Listing {}", path.display()),
        move |job| match forge_core::archive::list_zip(&path_for_job) {
            Ok(entries) => {
                job.progress.add_total_items(entries.len() as u64);
                job.progress.add_items(entries.len() as u64);
                job.progress.set_current("Archive listing complete");
                let _ = result_tx_for_job.send(Ok(entries));
                ctx.request_repaint();
                Ok(())
            }
            Err(error) => {
                let error = error.to_string();
                job.log(format!("listing failed: {error}"));
                let _ = result_tx_for_job.send(Err(error.clone()));
                ctx.request_repaint();
                Err(error)
            }
        },
    );
    let destination = archive_default_dir(&path);
    ArchiveWindow {
        path,
        entries: Vec::new(),
        selected: HashSet::new(),
        list_result: Some(result_rx),
        loading_job: Some(job),
        extract_job: None,
        destination_text: destination.to_string_lossy().into_owned(),
        destination,
        notice: None,
        open: true,
    }
}

fn archive_default_dir(path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default();
    path.parent().unwrap_or(Path::new(".")).join(stem)
}

fn render_archive(state: &mut ArchiveWindow, ctx: &egui::Context, actions: &mut Vec<UiAction>) {
    let mut open = state.open;
    let title = format!(
        "Archive: {}",
        state.path.file_name().unwrap_or_default().to_string_lossy()
    );
    egui::Window::new(title)
        .id(egui::Id::new(("extra-archive", state.path.clone())))
        .open(&mut open)
        .default_size([880.0, 650.0])
        .resizable(true)
        .show(ctx, |ui| {
            ui.label(
                RichText::new(state.path.display().to_string())
                    .small()
                    .weak(),
            );
            if let Some(job) = &state.loading_job {
                draw_job_progress(ui, job);
            }
            if let Some(message) = &state.notice {
                ui.label(message);
            }
            ui.horizontal(|ui| {
                ui.label("Extract to");
                ui.text_edit_singleline(&mut state.destination_text);
                if ui.button("Choose…").clicked()
                    && let Some(path) = crate::dialogs::pick_folder(
                        rfd::FileDialog::new().set_directory(&state.destination),
                    )
                {
                    state.destination_text = path.to_string_lossy().into_owned();
                }
            });
            if !state.entries.is_empty() {
                ui.horizontal(|ui| {
                    // Same width as the row checkbox so the headers line up with the cells.
                    ui.add_space(ui.spacing().icon_width);
                    table_cell(ui, ARCHIVE_NAME_W, RichText::new("Entry").strong());
                    table_cell(ui, ARCHIVE_SIZE_W, RichText::new("Size").strong());
                    table_cell(ui, ARCHIVE_SIZE_W, RichText::new("Packed").strong());
                    table_cell(ui, ARCHIVE_DATE_W, RichText::new("Modified").strong());
                    table_cell(ui, ARCHIVE_SAFETY_W, RichText::new("Safety").strong());
                });
                egui::ScrollArea::vertical().max_height(380.0).show_rows(
                    ui,
                    26.0,
                    state.entries.len(),
                    |ui, rows| {
                        for index in rows {
                            let entry = &state.entries[index];
                            ui.horizontal(|ui| {
                                let mut checked = state.selected.contains(&entry.name);
                                if ui.checkbox(&mut checked, "").changed() {
                                    if checked {
                                        state.selected.insert(entry.name.clone());
                                    } else {
                                        state.selected.remove(&entry.name);
                                    }
                                }
                                table_cell(
                                    ui,
                                    ARCHIVE_NAME_W,
                                    if entry.is_dir {
                                        RichText::new(format!("📁 {}", entry.name)).strong()
                                    } else {
                                        RichText::new(&entry.name)
                                    },
                                );
                                table_cell(
                                    ui,
                                    ARCHIVE_SIZE_W,
                                    RichText::new(format_size(entry.size)),
                                );
                                table_cell(
                                    ui,
                                    ARCHIVE_SIZE_W,
                                    RichText::new(format_size(entry.compressed_size)),
                                );
                                table_cell(
                                    ui,
                                    ARCHIVE_DATE_W,
                                    RichText::new(format_system_time(entry.modified)),
                                );
                                let safety = if !entry.safe_name {
                                    "Unsafe name"
                                } else if entry.is_symlink {
                                    "Link (skipped)"
                                } else if entry.encrypted {
                                    "Encrypted"
                                } else {
                                    "Safe"
                                };
                                table_cell(ui, ARCHIVE_SAFETY_W, RichText::new(safety));
                            });
                        }
                    },
                );
                ui.label(format!(
                    "{} entries · {} selected",
                    state.entries.len(),
                    state.selected.len()
                ));
                ui.horizontal(|ui| {
                    if ui.button("Extract all").clicked() {
                        state.destination = PathBuf::from(state.destination_text.trim());
                        actions.push(UiAction::ExtractArchive {
                            selected_only: false,
                            destination: state.destination.clone(),
                            entries: state.entries.clone(),
                            selected_names: HashSet::new(),
                        });
                    }
                    if ui
                        .add_enabled(
                            !state.selected.is_empty(),
                            egui::Button::new("Extract selected"),
                        )
                        .clicked()
                    {
                        state.destination = PathBuf::from(state.destination_text.trim());
                        actions.push(UiAction::ExtractArchive {
                            selected_only: true,
                            destination: state.destination.clone(),
                            entries: state.entries.clone(),
                            selected_names: state.selected.clone(),
                        });
                    }
                });
            } else if state.list_result.is_some() {
                ui.spinner();
                ui.label("Reading archive contents…");
            }
            if let Some(job) = &state.extract_job {
                draw_job_progress(ui, job);
                if job.state().is_finished() {
                    for line in job.log_lines().iter().rev().take(4).rev() {
                        ui.small(line);
                    }
                }
            }
        });
    state.open = open;
}

const ARCHIVE_NAME_W: f32 = 330.0;
const ARCHIVE_SIZE_W: f32 = 88.0;
const ARCHIVE_DATE_W: f32 = 140.0;
const ARCHIVE_SAFETY_W: f32 = 112.0;

/// One fixed-width, left-aligned, truncated table cell; shared by header and rows.
fn table_cell(ui: &mut Ui, width: f32, text: RichText) {
    ui.scope(|ui| {
        ui.set_min_width(width);
        ui.set_max_width(width);
        ui.add(egui::Label::new(text).truncate());
    });
}

fn start_extract_archive(
    jobs: &JobRegistry,
    event_tx: &crossbeam_channel::Sender<Event>,
    app_ctx: &egui::Context,
    archive: &Path,
    destination: PathBuf,
    entries: Vec<ArchiveEntry>,
    selected_names: HashSet<String>,
) -> JobHandle {
    // Only the "Extract selected" button supplies names; "Extract all" passes an empty set.
    let selected_only = !selected_names.is_empty();
    let archive = archive.to_path_buf();
    let title = if selected_only {
        format!("Extracting selected entries from {}", archive.display())
    } else {
        format!("Extracting {}", archive.display())
    };
    let tx = event_tx.clone();
    let ctx = app_ctx.clone();
    jobs.spawn("archive-extract", title, move |job| {
        let job_ctx = JobCtx::from_job(job);
        let result = if selected_only {
            extract_selected(&archive, &destination, &entries, &selected_names, &job_ctx)
        } else {
            extract_all(&archive, &destination, &job_ctx)
        };
        match result {
            Ok((count, failures)) => {
                for line in failures.iter().take(1000) {
                    job.log(line.clone());
                    job.progress.add_error();
                }
                let _ = tx.send(Event::Toast(format!(
                    "Extracted {count} item(s) to {}",
                    destination.display()
                )));
                let _ = tx.send(Event::Changed(destination.clone()));
                ctx.request_repaint();
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                let _ = tx.send(Event::Error(format!(
                    "Archive extraction failed: {message}"
                )));
                ctx.request_repaint();
                Err(message)
            }
        }
    })
}

fn extract_all(
    archive: &Path,
    destination: &Path,
    ctx: &JobCtx<'_>,
) -> forge_core::Result<(usize, Vec<String>)> {
    ensure_output_root(destination)?;
    let report =
        forge_core::archive::extract_zip(archive, destination, &ExtractOptions::default(), ctx)?;
    let mut failures: Vec<String> = report
        .skipped
        .into_iter()
        .map(|issue| format!("Skipped {}: {}", issue.path.display(), issue.reason))
        .collect();
    failures.extend(
        report
            .failed
            .into_iter()
            .map(|issue| format!("Failed {}: {}", issue.path.display(), issue.reason)),
    );
    if report.cancelled {
        return Err(forge_core::CoreError::Cancelled);
    }
    Ok((report.extracted.len(), failures))
}

fn extract_selected(
    archive: &Path,
    destination: &Path,
    entries: &[ArchiveEntry],
    selected_names: &HashSet<String>,
    ctx: &JobCtx<'_>,
) -> forge_core::Result<(usize, Vec<String>)> {
    ensure_output_root(destination)?;
    let selected: Vec<String> = entries
        .iter()
        .filter(|entry| is_selected_entry(entry, selected_names))
        .map(|entry| entry.name.clone())
        .collect();
    let report = forge_core::archive::extract_zip_entries(
        archive,
        destination,
        &selected,
        &ExtractOptions::default(),
        ctx,
    )?;
    if report.cancelled {
        return Err(forge_core::CoreError::Cancelled);
    }
    let mut failures = Vec::new();
    for issue in report.skipped {
        failures.push(format!(
            "Skipped {}: {}",
            issue.path.display(),
            issue.reason
        ));
    }
    for issue in report.failed {
        failures.push(format!("Failed {}: {}", issue.path.display(), issue.reason));
    }
    Ok((report.extracted.len(), failures))
}

fn is_selected_entry(entry: &ArchiveEntry, selected_names: &HashSet<String>) -> bool {
    if selected_names.contains(&entry.name) {
        return true;
    }
    let mut parent = entry.name.trim_end_matches(['/', '\\']);
    while let Some((prefix, _)) = parent.rsplit_once('/') {
        if selected_names.contains(&format!("{prefix}/")) {
            return true;
        }
        parent = prefix;
    }
    false
}

fn ensure_output_root(path: &Path) -> forge_core::Result<()> {
    fs::create_dir_all(path).map_err(|e| forge_core::CoreError::io(path, e))?;
    let md = fs::symlink_metadata(path).map_err(|e| forge_core::CoreError::io(path, e))?;
    if !md.is_dir() || md.file_type().is_symlink() {
        return Err(forge_core::CoreError::Invalid(format!(
            "{} is not a real destination folder",
            path.display()
        )));
    }
    Ok(())
}

fn render_create_archive(
    state: &mut CreateArchiveWindow,
    ctx: &egui::Context,
    actions: &mut Vec<UiAction>,
) {
    let mut open = state.open;
    egui::Window::new("Create ZIP archive")
        .id(egui::Id::new("extra-create-archive"))
        .open(&mut open)
        .default_size([500.0, 300.0])
        .resizable(true)
        .show(ctx, |ui| {
            ui.label(format!("{} item(s) will be added", state.sources.len()));
            ui.horizontal(|ui| {
                ui.label("Archive name");
                ui.text_edit_singleline(&mut state.name);
            });
            ui.label(format!("Saved in {}", state.directory.display()));
            if state.job.is_none() {
                if ui.button("Create ZIP").clicked() && !state.name.trim().is_empty() {
                    actions.push(UiAction::StartCreateArchive {
                        sources: state.sources.clone(),
                        directory: state.directory.clone(),
                        name: state.name.clone(),
                    });
                }
            } else if let Some(job) = &state.job {
                draw_job_progress(ui, job);
            }
        });
    state.open = open;
}

fn start_create_archive(
    jobs: &JobRegistry,
    event_tx: &crossbeam_channel::Sender<Event>,
    app_ctx: &egui::Context,
    sources: Vec<PathBuf>,
    directory: PathBuf,
    name: String,
) -> JobHandle {
    let filename = archive_filename(&name);
    let requested = directory.join(filename);
    let output = unique_path(&requested);
    let title = format!(
        "Creating {}",
        output.file_name().unwrap_or_default().to_string_lossy()
    );
    let tx = event_tx.clone();
    let ctx = app_ctx.clone();
    jobs.spawn("archive-create", title, move |job| {
        let job_ctx = JobCtx::from_job(job);
        let result = match ArchiveStaging::create(&directory) {
            Ok(mut staging) => {
                match forge_core::archive::create_zip(&sources, staging.path(), &job_ctx) {
                    Ok(report) => {
                        staging.owns_archive = true;
                        staging.publish(&requested).map(|output| (report, output))
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        match result {
            Ok((report, output)) => {
                for issue in &report.skipped {
                    job.log(format!(
                        "Skipped {}: {}",
                        issue.path.display(),
                        issue.reason
                    ));
                    job.progress.add_error();
                }
                let _ = tx.send(Event::Toast(format!("Created {}", output.display())));
                let _ = tx.send(Event::Changed(directory));
                ctx.request_repaint();
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                let _ = tx.send(Event::Error(format!("ZIP creation failed: {message}")));
                ctx.request_repaint();
                Err(message)
            }
        }
    })
}

fn archive_filename(name: &str) -> String {
    let clean = sanitize_filename(name);
    if clean.to_ascii_lowercase().ends_with(".zip") {
        clean
    } else {
        format!("{clean}.zip")
    }
}

fn unique_path(requested: &Path) -> PathBuf {
    if fs::symlink_metadata(requested).is_err() {
        return requested.to_path_buf();
    }
    let stem = requested.file_stem().unwrap_or_default().to_string_lossy();
    let extension = requested
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    for number in 2..100_000usize {
        let filename = if extension.is_empty() {
            format!("{stem} ({number})")
        } else {
            format!("{stem} ({number}).{extension}")
        };
        let candidate = requested.with_file_name(filename);
        if fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
    }
    requested.with_file_name(format!("{}-{}.zip", stem, std::process::id()))
}

struct ArchiveStaging {
    directory: PathBuf,
    path: PathBuf,
    owns_archive: bool,
}

impl ArchiveStaging {
    fn create(destination: &Path) -> forge_core::Result<Self> {
        loop {
            let sequence = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let directory = destination.join(format!(
                ".zxraw-archive-{}-{sequence:016x}",
                std::process::id()
            ));
            match fs::create_dir(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        path: directory.join("archive.zip"),
                        directory,
                        owns_archive: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(forge_core::CoreError::io(&directory, error)),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn publish(&mut self, requested: &Path) -> forge_core::Result<PathBuf> {
        for suffix in 1usize.. {
            let output = numbered_archive_path(requested, suffix);
            match rename_no_replace(&self.path, &output) {
                Ok(()) => {
                    self.owns_archive = false;
                    return Ok(output);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(forge_core::CoreError::io(&output, error)),
            }
        }
        Err(forge_core::CoreError::Invalid(
            "could not find an unused ZIP output name".into(),
        ))
    }
}

impl Drop for ArchiveStaging {
    fn drop(&mut self) {
        if self.owns_archive {
            let _ = fs::remove_file(&self.path);
        }
        let _ = fs::remove_dir(&self.directory);
    }
}

fn numbered_archive_path(requested: &Path, suffix: usize) -> PathBuf {
    if suffix == 1 {
        return requested.to_path_buf();
    }
    let stem = requested.file_stem().unwrap_or_default().to_string_lossy();
    let extension = requested
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let filename = if extension.is_empty() {
        format!("{stem} ({suffix})")
    } else {
        format!("{stem} ({suffix}).{extension}")
    };
    requested.with_file_name(filename)
}

fn render_split(state: &mut SplitWindow, ctx: &egui::Context, actions: &mut Vec<UiAction>) {
    let mut open = state.open;
    let title = format!(
        "Split file: {}",
        state
            .source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    );
    egui::Window::new(title)
        .id(egui::Id::new("extra-split-file"))
        .open(&mut open)
        .default_size([480.0, 270.0])
        .resizable(true)
        .show(ctx, |ui| {
            ui.label(format!(
                "Parts are written beside {}",
                state.source.display()
            ));
            ui.horizontal(|ui| {
                ui.label("Part size");
                egui::ComboBox::from_id_salt("split-size-preset")
                    .selected_text(if state.custom {
                        "Custom".into()
                    } else {
                        format!("{} MiB", state.preset_mib)
                    })
                    .show_ui(ui, |ui| {
                        for size in [100u64, 700, 2047, 4095] {
                            if ui
                                .selectable_value(
                                    &mut state.preset_mib,
                                    size,
                                    format!("{size} MiB"),
                                )
                                .clicked()
                            {
                                state.custom = false;
                            }
                        }
                        ui.selectable_value(&mut state.custom, true, "Custom…");
                    });
                if state.custom {
                    ui.add(
                        egui::DragValue::new(&mut state.custom_mib)
                            .range(1..=1_048_576)
                            .suffix(" MiB"),
                    );
                }
            });
            ui.label("4095 MiB keeps each part below FAT32's 4 GiB file-size limit.");
            if state.job.is_none() {
                if ui.button("Split file").clicked() {
                    let mib = if state.custom {
                        state.custom_mib
                    } else {
                        state.preset_mib
                    };
                    actions.push(UiAction::StartSplit {
                        source: state.source.clone(),
                        bytes: mib.saturating_mul(1024 * 1024),
                    });
                }
            } else if let Some(job) = &state.job {
                draw_job_progress(ui, job);
            }
        });
    state.open = open;
}

fn start_split(
    jobs: &JobRegistry,
    event_tx: &crossbeam_channel::Sender<Event>,
    app_ctx: &egui::Context,
    source: PathBuf,
    bytes: u64,
) -> JobHandle {
    let title = format!(
        "Splitting {}",
        source.file_name().unwrap_or_default().to_string_lossy()
    );
    let tx = event_tx.clone();
    let ctx = app_ctx.clone();
    let source_for_job = source.clone();
    jobs.spawn("file-split", title, move |job| {
        let job_ctx = JobCtx::from_job(job);
        match forge_core::split::split_file(&source_for_job, bytes, &job_ctx) {
            Ok(report) => {
                let _ = tx.send(Event::Toast(format!(
                    "Created {} part(s) and {}",
                    report.parts.len(),
                    report.manifest.display()
                )));
                if let Some(parent) = report.manifest.parent() {
                    let _ = tx.send(Event::Changed(parent.to_path_buf()));
                }
                ctx.request_repaint();
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                let _ = tx.send(Event::Error(format!("File split failed: {message}")));
                ctx.request_repaint();
                Err(message)
            }
        }
    })
}

fn draw_job_progress(ui: &mut Ui, job: &JobHandle) {
    let progress = job.progress.snapshot();
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} · {:?}", job.title, job.state())).strong());
        if !job.state().is_finished() && ui.small_button("Cancel").clicked() {
            job.cancel.cancel();
        }
    });
    let text = if progress.bytes_total > 0 {
        format!(
            "{} / {}",
            format_size(progress.bytes_done),
            format_size(progress.bytes_total)
        )
    } else if progress.items_total > 0 {
        format!("{} / {} items", progress.items_done, progress.items_total)
    } else {
        format!("{} item(s)", progress.items_done)
    };
    if let Some(fraction) = progress.fraction() {
        ui.add(egui::ProgressBar::new(fraction).text(text));
    } else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(text);
        });
    }
    if !progress.current.is_empty() {
        ui.label(RichText::new(progress.current).small().weak());
    }
}

fn format_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if size < 1024 {
        return format!("{size} B");
    }
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn format_system_time(time: Option<SystemTime>) -> String {
    time.map(|time| {
        let local: chrono::DateTime<chrono::Local> = time.into();
        local.format("%Y-%m-%d %H:%M").to_string()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn contact_sheet_pagination_uses_rows_times_columns() {
        assert!(page_ranges(0, 4, 3).is_empty());
        assert_eq!(page_ranges(12, 4, 3), vec![0..12]);
        assert_eq!(page_ranges(13, 4, 3), vec![0..12, 12..13]);
        assert_eq!(page_ranges(11, 3, 2), vec![0..6, 6..11]);
    }

    #[test]
    fn contact_sheet_page_loader_holds_at_most_one_page_of_thumbnails() {
        struct LiveThumbnail(Arc<std::sync::atomic::AtomicUsize>);

        impl Drop for LiveThumbnail {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }

        let capacity = 6;
        let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut maximum = 0;
        for range in page_ranges(31, 3, 2) {
            let page = load_sheet_page(range, |_| {
                let held = live.fetch_add(1, Ordering::Relaxed) + 1;
                maximum = maximum.max(held);
                Ok(LiveThumbnail(live.clone()))
            })
            .unwrap();
            assert!(page.len() <= capacity);
            assert_eq!(live.load(Ordering::Relaxed), page.len());
            drop(page);
            assert_eq!(live.load(Ordering::Relaxed), 0);
        }
        assert!(maximum <= capacity);
    }

    #[test]
    fn contact_sheet_publish_never_replaces_an_existing_output() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("sheet.png");
        fs::write(&output, b"existing").unwrap();

        let error = publish_bytes_no_replace(&output, b"new sheet").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(output).unwrap(), b"existing");
    }

    #[test]
    fn archive_publish_chooses_a_free_name_without_replacing_existing_zip() {
        let directory = tempfile::tempdir().unwrap();
        let requested = directory.path().join("archive.zip");
        fs::write(&requested, b"existing archive").unwrap();
        let mut staging = ArchiveStaging::create(directory.path()).unwrap();
        fs::write(staging.path(), b"new archive").unwrap();
        staging.owns_archive = true;

        let published = staging.publish(&requested).unwrap();
        assert_ne!(published, requested);
        assert_eq!(fs::read(&requested).unwrap(), b"existing archive");
        assert_eq!(fs::read(&published).unwrap(), b"new archive");
    }

    #[test]
    fn contact_sheet_layout_is_bounded_and_accounts_for_title_and_captions() {
        let plain = sheet_layout(4, 3, 256, 0, false);
        let titled = sheet_layout(4, 3, 256, 2, true);
        assert_eq!(plain.slots, 12);
        assert_eq!(titled.slots, 12);
        assert_eq!(titled.width, plain.width);
        assert!(titled.height > plain.height);
        assert!(sheet_layout(usize::MAX, usize::MAX, u32::MAX, 9, true).width <= 16_384);
        assert!(sheet_layout(usize::MAX, usize::MAX, u32::MAX, 9, true).height <= 16_384);
    }

    /// Mono-spaced stand-in for real advances: every character is 7 px wide.
    fn mono(text: &str) -> f32 {
        text.chars().count() as f32 * 7.0
    }

    #[test]
    fn end_ellipsis_is_single_char_and_never_double_dot() {
        let text = "DSC_0001.final.export.jpg";
        for max in (0..=200u32).step_by(3) {
            let max = max as f32;
            let out = fit_end(text, max, mono);
            assert!(!out.contains(".."), "{out:?}");
            assert!(mono(&out) <= max, "{out:?} wider than {max}");
            // An empty result means not even the ellipsis fits; otherwise exactly one ellipsis.
            if !out.is_empty() && out != text {
                assert!(out.ends_with(ELLIPSIS), "{out:?}");
                assert_eq!(out.matches(ELLIPSIS).count(), 1, "{out:?}");
            }
        }
        assert_eq!(fit_end(text, 6.0, mono), "");
    }

    #[test]
    fn text_that_fits_is_unchanged() {
        let name = "DSC_0001.jpg";
        assert_eq!(fit_end(name, mono(name), mono), name);
        assert_eq!(fit_file_name(name, mono(name), mono), name);
        assert_eq!(fit_end("", 0.0, mono), "");
    }

    #[test]
    fn middle_ellipsis_keeps_extension_and_counter() {
        let name = "Very long name with many words_0001.jpg";
        let out = fit_file_name(name, 22.0 * 7.0, mono);
        assert!(out.ends_with("_0001.jpg"), "{out:?}");
        assert!(out.starts_with("Very"), "{out:?}");
        assert!(out.contains(ELLIPSIS), "{out:?}");
        assert!(!out.contains(".."), "{out:?}");
        assert!(mono(&out) <= 22.0 * 7.0);

        // When the tail alone is too wide, fall back to an end ellipsis within bounds.
        let tiny = fit_file_name(name, 3.0 * 7.0, mono);
        assert_eq!(tiny, "Ve…");
    }

    #[test]
    fn split_extension_handles_dotless_and_dotfile_names() {
        assert_eq!(split_extension("IMG_01.jpg"), ("IMG_01", ".jpg"));
        assert_eq!(split_extension("README"), ("README", ""));
        assert_eq!(split_extension(".hidden"), (".hidden", ""));
        assert_eq!(split_extension("a. b"), ("a. b", ""));
    }

    #[test]
    fn text_width_is_monotonic_in_length() {
        let text = "Ünïcödé 日本 test_0001.jpg";
        for engine in [TextEngine::Bitmap, TextEngine::load()] {
            let mut last = 0.0f32;
            for end in 0..=text.chars().count() {
                let prefix: String = text.chars().take(end).collect();
                let width = engine.width(&prefix, 14.0, false);
                assert!(width >= last, "{prefix:?}: {width} < {last}");
                last = width;
            }
        }
    }

    #[test]
    fn unicode_caption_draws_pixels_with_system_fonts() {
        if !windows_font_dir().join("segoeui.ttf").exists() {
            eprintln!("skipped: segoeui.ttf not found");
            return;
        }
        let engine = TextEngine::load();
        let mut canvas = image::RgbaImage::from_pixel(260, 40, image::Rgba([0, 0, 0, 255]));
        let blank = canvas.clone();
        let boxed = TextBox {
            x: 4.0,
            y: 8.0,
            width: 252.0,
            px: 16.0,
            bold: false,
            color: image::Rgba([255, 255, 255, 255]),
        };
        draw_text_fit(&mut canvas, &engine, "Ünïcödé 日本 test", boxed, Fit::End);
        let changed = canvas
            .pixels()
            .zip(blank.pixels())
            .filter(|(a, b)| a != b)
            .count();
        assert!(changed > 0);
    }

    /// Renders a real sheet from the demo photos into `%TEMP%`; run with `--ignored`.
    #[test]
    #[ignore = "writes a demo contact sheet from C:\\tmp\\forge-demo\\Photos"]
    fn demo_contact_sheet_from_photos() {
        let dir = Path::new(r"C:\tmp\forge-demo\Photos");
        let mut paths: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg")))
            .collect();
        paths.sort();
        let sources: Vec<SheetSource> = paths
            .into_iter()
            .enumerate()
            .map(|(i, path)| SheetSource {
                path,
                rating: Some((i % 5) as u8 + 1),
            })
            .collect();
        let output_dir = std::env::temp_dir().join("zxraw-contact-sheet-demo");
        let _ = fs::remove_dir_all(&output_dir);
        let options = SheetOptions {
            columns: 6,
            rows: 6,
            thumbnail_size: 128,
            captions_file_name: true,
            captions_rating: true,
            captions_exif: true,
            page_title: "ZXraw Explorer demo".into(),
            dark_background: true,
            format: SheetFormat::Png,
            output_dir: output_dir.clone(),
            output_stem: "contact-sheet".into(),
        };
        let cancel = forge_jobs::CancelToken::new();
        let progress = forge_jobs::Progress::default();
        let pages =
            build_contact_sheets(&sources, &options, &JobCtx::new(&cancel, &progress)).unwrap();
        assert_eq!(pages.len(), 1);
        println!("contact sheet: {}", pages[0].display());
    }
}
