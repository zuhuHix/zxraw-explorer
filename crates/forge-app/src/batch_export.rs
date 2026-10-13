//! Batch export / convert of the selected images (or every image in the folder).
//!
//! Each source is decoded (RAW files through their embedded JPEG preview), optionally
//! resized (never upscaled), encoded and published into the output folder with a
//! no-replace rename so an existing file is never overwritten.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use forge_core::model::{FileEntry, MediaKind, media_kind_for_ext};
use forge_core::paths::{is_reserved_name, normalize_user_path, rename_no_replace};
use forge_jobs::{JobHandle, JobState};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::tiff::TiffEncoder;
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use parking_lot::Mutex;

use crate::app::ZxrawApp;

const PREVIEW_ROWS: usize = 20;
static BATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutFormat {
    Jpeg,
    Png,
    Tiff,
    WebP,
}

impl OutFormat {
    const ALL: [Self; 4] = [Self::Jpeg, Self::Png, Self::Tiff, Self::WebP];

    fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Tiff => "tif",
            Self::WebP => "webp",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::Tiff => "TIFF",
            Self::WebP => "WebP",
        }
    }
}

/// Resize request after validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resize {
    Original,
    /// Fit the long edge to N pixels.
    LongEdge(u32),
    /// Fit within W x H, keeping aspect ratio.
    Within(u32, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeMode {
    Original,
    LongEdge,
    Within,
}

/// Validated export settings used by the worker.
#[derive(Debug)]
struct Options {
    format: OutFormat,
    quality: u8,
    resize: Resize,
    template: String,
    out_dir: PathBuf,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Summary {
    exported: usize,
    failed: usize,
    cancelled: bool,
}

struct BatchRun {
    handle: JobHandle,
    outcome: Arc<Mutex<Option<Result<Summary, String>>>>,
    reported: bool,
}

struct BatchUi {
    open: bool,
    format: OutFormat,
    quality: u8,
    resize: ResizeMode,
    long_edge: String,
    width: String,
    height: String,
    out_dir: String,
    template: String,
    run: Option<BatchRun>,
    /// Header dimensions of previewed sources (None = unreadable, e.g. RAW).
    dims_cache: HashMap<PathBuf, Option<(u32, u32)>>,
}

impl Default for BatchUi {
    fn default() -> Self {
        Self {
            open: false,
            format: OutFormat::Jpeg,
            quality: 90,
            resize: ResizeMode::Original,
            long_edge: "2048".into(),
            width: "1920".into(),
            height: "1080".into(),
            out_dir: String::new(),
            template: "{name}".into(),
            run: None,
            dims_cache: HashMap::new(),
        }
    }
}

static UI: std::sync::OnceLock<Mutex<BatchUi>> = std::sync::OnceLock::new();

fn ui_state() -> &'static Mutex<BatchUi> {
    UI.get_or_init(|| Mutex::new(BatchUi::default()))
}

/// `Cmd::BatchExport`.
pub fn open(app: &mut ZxrawApp) {
    let mut state = ui_state().lock();
    state.open = true;
    state.out_dir = app.pane().path.join("export").display().to_string();
}

/// Called every frame by the app shell.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    // The state is moved out for the frame so the window closure can borrow it freely.
    let mut state = std::mem::take(&mut *ui_state().lock());
    report_finished(app, &mut state);
    if state.open || state.run.as_ref().is_some_and(|run| !run.handle.state().is_finished()) {
        draw(app, ctx, &mut state);
    }
    *ui_state().lock() = state;
}

fn report_finished(app: &mut ZxrawApp, state: &mut BatchUi) {
    let Some(run) = state.run.as_mut() else {
        return;
    };
    if run.reported || !run.handle.state().is_finished() {
        return;
    }
    run.reported = true;
    let cancelled = run.handle.state() == JobState::Cancelled;
    let (is_error, text) = match run.outcome.lock().as_ref() {
        Some(Ok(summary)) => (
            summary.failed > 0,
            format!(
                "Batch export {}: exported {}, failed {}",
                if cancelled { "cancelled" } else { "finished" },
                summary.exported,
                summary.failed
            ),
        ),
        Some(Err(error)) => (true, format!("Batch export failed: {error}")),
        None => (true, "Batch export failed".to_string()),
    };
    if is_error {
        app.toast_err(text);
    } else {
        app.toast(text);
    }
}

fn is_image_entry(entry: &FileEntry) -> bool {
    !entry.is_dir() && matches!(entry.media_kind(), MediaKind::Image | MediaKind::Raw)
}

/// Selected images, or every image in the folder when nothing is selected.
/// The bool is true when the list comes from the selection.
fn collect_sources(app: &mut ZxrawApp) -> (Vec<PathBuf>, bool) {
    let selected: Vec<PathBuf> = app
        .pane()
        .selected_entries()
        .into_iter()
        .filter(|entry| is_image_entry(entry))
        .map(|entry| entry.path.clone())
        .collect();
    if !selected.is_empty() {
        return (selected, true);
    }
    let all = app
        .pane()
        .entries
        .iter()
        .filter(|entry| is_image_entry(entry))
        .map(|entry| entry.path.clone())
        .collect();
    (all, false)
}

fn draw(app: &mut ZxrawApp, ctx: &egui::Context, state: &mut BatchUi) {
    let (sources, from_selection) = collect_sources(app);
    let active = state
        .run
        .as_ref()
        .filter(|run| !run.handle.state().is_finished())
        .map(|run| run.handle.clone());

    let mut is_open = state.open;
    let mut start = false;
    let mut close_after = false;
    egui::Window::new("Batch export")
        .collapsible(false)
        .resizable(false)
        .open(&mut is_open)
        .show(ctx, |ui| {
            if let Some(handle) = &active {
                let progress = handle.progress.snapshot();
                ui.label("Exporting…");
                if let Some(fraction) = progress.fraction() {
                    ui.add(egui::ProgressBar::new(fraction).show_percentage());
                } else {
                    ui.add(egui::ProgressBar::new(0.0).animate(true));
                }
                ui.label(format!(
                    "{} · {} processed, {} failed",
                    progress.current,
                    progress.items_done,
                    progress.errors
                ));
                if ui.button("Cancel export").clicked() {
                    handle.cancel.cancel();
                }
                return;
            }

            ui.label("Convert or resize the images into a new folder. Originals are never changed.");
            if sources.is_empty() {
                ui.label(egui::RichText::new("No images in the selection or this folder.").weak());
            } else if from_selection {
                ui.label(format!("{} selected image(s)", sources.len()));
            } else {
                ui.label(format!("All {} image(s) in this folder", sources.len()));
            }
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Format");
                for format in OutFormat::ALL {
                    ui.selectable_value(&mut state.format, format, format.label());
                }
            });
            if state.format == OutFormat::Jpeg {
                ui.add(egui::Slider::new(&mut state.quality, 1..=100).text("JPEG quality"));
            } else if state.format == OutFormat::WebP {
                ui.label(egui::RichText::new("WebP export is lossless (no quality setting).").weak());
            }

            ui.horizontal(|ui| {
                ui.label("Resize");
                ui.selectable_value(&mut state.resize, ResizeMode::Original, "Original size");
                ui.selectable_value(&mut state.resize, ResizeMode::LongEdge, "Long edge");
                ui.selectable_value(&mut state.resize, ResizeMode::Within, "Fit within");
            });
            match state.resize {
                ResizeMode::Original => {}
                ResizeMode::LongEdge => {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut state.long_edge).desired_width(80.0));
                        ui.label("px");
                    });
                }
                ResizeMode::Within => {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut state.width).desired_width(80.0));
                        ui.label("×");
                        ui.add(egui::TextEdit::singleline(&mut state.height).desired_width(80.0));
                        ui.label("px");
                    });
                }
            }

            ui.horizontal(|ui| {
                ui.label("Output folder");
                ui.add(egui::TextEdit::singleline(&mut state.out_dir).desired_width(320.0));
            });
            ui.horizontal(|ui| {
                ui.label("Naming");
                ui.add(egui::TextEdit::singleline(&mut state.template).desired_width(200.0));
            });
            ui.label(
                egui::RichText::new(
                    "Tokens: {name} {ext} {n} {n:3} {w} {h}. Without {ext} the extension is added. \
                     Existing files are never overwritten.",
                )
                .weak(),
            );

            match build_options(state) {
                Ok(options) => {
                    if !sources.is_empty() {
                        let (names, conflicts) =
                            preview_names(&mut state.dims_cache, &sources, &options);
                        ui.separator();
                        ui.label(format!(
                            "Preview (first {} of {}):",
                            names.len(),
                            sources.len()
                        ));
                        for name in &names {
                            ui.label(egui::RichText::new(name).monospace());
                        }
                        if conflicts > 0 {
                            ui.label(format!(
                                "{conflicts} name collision(s) in this preview were numbered (2), (3)…"
                            ));
                        }
                        ui.label(
                            egui::RichText::new("Width/height show _ for RAW files until export.")
                                .weak(),
                        );
                    }
                }
                Err(error) => {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
            }

            ui.horizontal(|ui| {
                let can_start = !sources.is_empty() && build_options(state).is_ok();
                if ui
                    .add_enabled(can_start, egui::Button::new("Export…"))
                    .clicked()
                {
                    start = true;
                }
                if ui.button("Close").clicked() {
                    close_after = true;
                }
            });
        });

    state.open = is_open && !close_after;
    if start {
        let result = build_options(state).and_then(|options| start_job(app, sources, options, state));
        if let Err(error) = result {
            app.toast_err(error);
        }
    }
}

fn start_job(
    app: &mut ZxrawApp,
    sources: Vec<PathBuf>,
    options: Options,
    state: &mut BatchUi,
) -> Result<(), String> {
    if sources.is_empty() {
        return Err("No images to export.".into());
    }
    let outcome = Arc::new(Mutex::new(None));
    let worker_outcome = outcome.clone();
    let total = sources.len() as u64;
    let handle = app.ops.jobs.spawn("batch-export", "Batch export", move |job| {
        job.progress.add_total_items(total);
        let result = run_export(
            &sources,
            &options,
            || job.cancel.is_cancelled(),
            |name, outcome| {
                job.progress.set_current(name.to_string());
                match outcome {
                    Ok(_) => job.progress.add_items(1),
                    Err(error) => {
                        job.progress.add_error();
                        job.log(format!("{name}: {error}"));
                    }
                }
            },
        );
        let status = result.as_ref().map(|_| ()).map_err(Clone::clone);
        *worker_outcome.lock() = Some(result);
        status
    });
    state.run = Some(BatchRun {
        handle,
        outcome,
        reported: false,
    });
    Ok(())
}

fn positive(text: &str, what: &str) -> Result<u32, String> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{what} must be a whole number above 0."))
}

fn build_options(state: &BatchUi) -> Result<Options, String> {
    let resize = match state.resize {
        ResizeMode::Original => Resize::Original,
        ResizeMode::LongEdge => Resize::LongEdge(positive(&state.long_edge, "Long edge")?),
        ResizeMode::Within => Resize::Within(
            positive(&state.width, "Width")?,
            positive(&state.height, "Height")?,
        ),
    };
    if state.template.trim().is_empty() {
        return Err("Naming template is empty.".into());
    }
    if state.out_dir.trim().is_empty() {
        return Err("Choose an output folder.".into());
    }
    Ok(Options {
        format: state.format,
        quality: state.quality.clamp(1, 100),
        resize,
        template: state.template.clone(),
        out_dir: normalize_user_path(state.out_dir.trim()),
    })
}

/// Output names for the first rows of the batch, plus the number of collisions.
fn preview_names(
    cache: &mut HashMap<PathBuf, Option<(u32, u32)>>,
    sources: &[PathBuf],
    options: &Options,
) -> (Vec<String>, usize) {
    let mut taken = HashSet::new();
    let mut names = Vec::new();
    let mut conflicts = 0;
    for (index, source) in sources.iter().take(PREVIEW_ROWS).enumerate() {
        let dims = *cache
            .entry(source.clone())
            .or_insert_with(|| image::image_dimensions(source).ok());
        let out_dims = dims.map(|(w, h)| fit_size(w, h, options.resize));
        let raw = build_name(
            &options.template,
            &file_stem(source),
            options.format,
            index + 1,
            out_dims,
        );
        let name = dedupe(&raw, &taken);
        if name != raw {
            conflicts += 1;
        }
        taken.insert(name.to_lowercase());
        names.push(name);
    }
    (names, conflicts)
}

/// Runs the whole batch on the calling thread. `cancelled` is polled between files and
/// `report` is called once per source with its outcome.
fn run_export(
    sources: &[PathBuf],
    options: &Options,
    cancelled: impl Fn() -> bool,
    mut report: impl FnMut(&str, &Result<String, String>),
) -> Result<Summary, String> {
    fs::create_dir_all(&options.out_dir).map_err(|error| {
        format!("cannot create {}: {error}", options.out_dir.display())
    })?;
    let mut taken = HashSet::new();
    let mut summary = Summary::default();
    for (index, source) in sources.iter().enumerate() {
        if cancelled() {
            summary.cancelled = true;
            break;
        }
        let label = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let outcome = export_one(source, index + 1, options, &mut taken);
        match &outcome {
            Ok(_) => summary.exported += 1,
            Err(_) => summary.failed += 1,
        }
        report(&label, &outcome);
    }
    Ok(summary)
}

fn export_one(
    source: &Path,
    number: usize,
    options: &Options,
    taken: &mut HashSet<String>,
) -> Result<String, String> {
    let image = load_source(source)?;
    let image = resize_to(image, options.resize);
    let dims = image.dimensions();
    let raw = build_name(
        &options.template,
        &file_stem(source),
        options.format,
        number,
        Some(dims),
    );
    let name = dedupe(&raw, taken);
    let bytes = encode(&image, options.format, options.quality)?;
    let published = publish_no_replace(&options.out_dir, &name, &bytes)
        .map_err(|error| format!("write failed: {error}"))?;
    taken.insert(published.to_lowercase());
    Ok(published)
}

fn is_raw(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    media_kind_for_ext(&ext) == MediaKind::Raw
}

fn load_source(path: &Path) -> Result<DynamicImage, String> {
    if is_raw(path) {
        let preview = forge_media::raw_preview::extract_preview(
            path,
            forge_media::raw_preview::PreviewPick::Largest,
        )
        .map_err(|error| format!("no embedded preview: {error}"))?;
        let image = image::load_from_memory(&preview.jpeg)
            .map_err(|error| format!("preview decode failed: {error}"))?;
        return Ok(apply_orientation(image, preview.orientation));
    }
    let image = image::open(path).map_err(|error| format!("decode failed: {error}"))?;
    let orientation = forge_media::image_meta::read_orientation(path).unwrap_or(1);
    Ok(apply_orientation(image, orientation))
}

fn apply_orientation(mut image: DynamicImage, exif: u16) -> DynamicImage {
    if exif != 1
        && let Ok(exif) = u8::try_from(exif)
        && let Some(orientation) = image::metadata::Orientation::from_exif(exif)
    {
        image.apply_orientation(orientation);
    }
    image
}

/// Fit `(w, h)` into `resize`. Never upscales; returns the input size when no shrink is
/// needed.
fn fit_size(w: u32, h: u32, resize: Resize) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (w, h);
    }
    let scale = match resize {
        Resize::Original => return (w, h),
        Resize::LongEdge(edge) => {
            if edge == 0 || w.max(h) <= edge {
                return (w, h);
            }
            f64::from(edge) / f64::from(w.max(h))
        }
        Resize::Within(bound_w, bound_h) => {
            if bound_w == 0 || bound_h == 0 || (w <= bound_w && h <= bound_h) {
                return (w, h);
            }
            (f64::from(bound_w) / f64::from(w)).min(f64::from(bound_h) / f64::from(h))
        }
    };
    let new_w = ((f64::from(w) * scale).round() as u32).clamp(1, w);
    let new_h = ((f64::from(h) * scale).round() as u32).clamp(1, h);
    (new_w, new_h)
}

fn resize_to(image: DynamicImage, resize: Resize) -> DynamicImage {
    let (w, h) = image.dimensions();
    let (new_w, new_h) = fit_size(w, h, resize);
    if (new_w, new_h) == (w, h) {
        image
    } else {
        image.resize_exact(new_w, new_h, FilterType::Lanczos3)
    }
}

fn encode(image: &DynamicImage, format: OutFormat, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Cursor::new(Vec::new());
    let result = match format {
        OutFormat::Jpeg => DynamicImage::ImageRgb8(image.to_rgb8())
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, quality)),
        OutFormat::Png => image.write_with_encoder(PngEncoder::new(&mut out)),
        OutFormat::Tiff => image.write_with_encoder(TiffEncoder::new(&mut out)),
        OutFormat::WebP => {
            let pixels = if image.color().has_alpha() {
                DynamicImage::ImageRgba8(image.to_rgba8())
            } else {
                DynamicImage::ImageRgb8(image.to_rgb8())
            };
            pixels.write_with_encoder(WebPEncoder::new_lossless(&mut out))
        }
    };
    result.map_err(|error| format!("encode failed: {error}"))?;
    Ok(out.into_inner())
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Expands `{name}`, `{ext}`, `{n}`, `{n:W}`, `{w}`, `{h}`. Unknown tokens stay literal;
/// unknown dimensions render as `?`.
fn expand_template(
    template: &str,
    stem: &str,
    ext: &str,
    number: usize,
    dims: Option<(u32, u32)>,
) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let token = &after[..close];
        match expand_token(token, stem, ext, number, dims) {
            Some(text) => out.push_str(&text),
            None => {
                out.push('{');
                out.push_str(token);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

fn expand_token(
    token: &str,
    stem: &str,
    ext: &str,
    number: usize,
    dims: Option<(u32, u32)>,
) -> Option<String> {
    match token {
        "name" => Some(stem.to_string()),
        "ext" => Some(ext.to_string()),
        "n" => Some(number.to_string()),
        "w" => Some(dims.map_or_else(|| "?".to_string(), |(w, _)| w.to_string())),
        "h" => Some(dims.map_or_else(|| "?".to_string(), |(_, h)| h.to_string())),
        _ => {
            let width = token.strip_prefix("n:")?.parse::<usize>().ok()?;
            Some(format!("{number:0width$}"))
        }
    }
}

/// Replaces characters Windows rejects, trims trailing dots and spaces, and avoids
/// reserved device names.
fn sanitize_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = replaced.trim_end_matches(['.', ' ']).to_string();
    if trimmed.is_empty() {
        return "export".to_string();
    }
    if is_reserved_name(&trimmed) {
        return format!("_{trimmed}");
    }
    trimmed
}

/// Full output file name for one source (sanitized, with extension).
fn build_name(
    template: &str,
    stem: &str,
    format: OutFormat,
    number: usize,
    dims: Option<(u32, u32)>,
) -> String {
    let ext = format.extension();
    let mut name = expand_template(template, stem, ext, number, dims);
    if !template.contains("{ext}") {
        name.push('.');
        name.push_str(ext);
    }
    sanitize_name(&name)
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(index) if index > 0 => (&name[..index], &name[index..]),
        _ => (name, ""),
    }
}

/// Returns `name` if unused in this batch, otherwise `name (2)`, `name (3)`, ….
/// `taken` holds lowercase names; comparison is case-insensitive like Windows.
fn dedupe(name: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(&name.to_lowercase()) {
        return name.to_string();
    }
    let (stem, ext) = split_ext(name);
    let mut counter = 2;
    loop {
        let candidate = format!("{stem} ({counter}){ext}");
        if !taken.contains(&candidate.to_lowercase()) {
            return candidate;
        }
        counter += 1;
    }
}

/// Writes `bytes` to a temp file in `dir` and publishes it under `name`, or under
/// `name (k)` when that exists. Never replaces an existing file.
fn publish_no_replace(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<String> {
    let temporary = loop {
        let sequence = BATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            ".zxraw-batch-{}-{sequence:016x}.tmp",
            std::process::id()
        ));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        break path;
    };

    let (stem, ext) = split_ext(name);
    let mut counter = 1;
    loop {
        let candidate = if counter == 1 {
            name.to_string()
        } else {
            format!("{stem} ({counter}){ext}")
        };
        match rename_no_replace(&temporary, &dir.join(&candidate)) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && counter < 10_000 => {
                counter += 1;
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};

    #[test]
    fn template_tokens_and_padding() {
        assert_eq!(
            expand_template("{name}_{n:3}_{w}x{h}.{ext}", "IMG", "jpg", 7, Some((640, 480))),
            "IMG_007_640x480.jpg"
        );
        assert_eq!(expand_template("{n}", "x", "png", 12, None), "12");
        assert_eq!(expand_template("{n:2}", "x", "png", 3, None), "03");
        assert_eq!(expand_template("{w}", "x", "png", 1, None), "?");
        assert_eq!(expand_template("{foo}-{n:}", "x", "png", 1, None), "{foo}-{n:}");
        assert_eq!(expand_template("open {name", "x", "png", 1, None), "open {name");
    }

    #[test]
    fn build_name_appends_extension_only_when_missing() {
        assert_eq!(
            build_name("{name}", "IMG_1", OutFormat::Jpeg, 1, None),
            "IMG_1.jpg"
        );
        assert_eq!(
            build_name("{name}.{ext}", "IMG_1", OutFormat::WebP, 1, None),
            "IMG_1.webp"
        );
        assert_eq!(
            build_name("{name}_{w}", "a", OutFormat::Png, 1, Some((10, 20))),
            "a_10.png"
        );
    }

    #[test]
    fn sanitize_replaces_invalid_characters() {
        assert_eq!(sanitize_name("a:b/c?*.jpg"), "a_b_c__.jpg");
        assert_eq!(sanitize_name("name. "), "name");
        assert_eq!(sanitize_name(""), "export");
        assert_eq!(sanitize_name("CON.jpg"), "_CON.jpg");
    }

    #[test]
    fn fit_size_never_upscales() {
        assert_eq!(fit_size(4000, 3000, Resize::LongEdge(1000)), (1000, 750));
        assert_eq!(fit_size(4000, 3000, Resize::LongEdge(5000)), (4000, 3000));
        assert_eq!(fit_size(100, 50, Resize::LongEdge(1000)), (100, 50));
        assert_eq!(fit_size(4000, 3000, Resize::Within(800, 800)), (800, 600));
        assert_eq!(fit_size(4000, 3000, Resize::Within(5000, 5000)), (4000, 3000));
        assert_eq!(fit_size(100, 50, Resize::Within(80, 80)), (80, 40));
        assert_eq!(fit_size(100, 50, Resize::Original), (100, 50));
    }

    #[test]
    fn in_batch_names_are_deduplicated() {
        let mut taken = HashSet::new();
        let mut names = Vec::new();
        for stem in ["IMG", "IMG", "img"] {
            let raw = build_name("{name}", stem, OutFormat::Jpeg, 1, None);
            let name = dedupe(&raw, &taken);
            taken.insert(name.to_lowercase());
            names.push(name);
        }
        assert_eq!(names, ["IMG.jpg", "IMG (2).jpg", "img (3).jpg"]);
    }

    #[test]
    fn batch_exports_resized_jpegs_and_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.png");
        let b = dir.path().join("b.png");
        RgbImage::from_pixel(200, 100, Rgb([10, 20, 30])).save(&a).unwrap();
        RgbImage::from_pixel(50, 50, Rgb([200, 100, 0])).save(&b).unwrap();
        let out = dir.path().join("out");
        let options = Options {
            format: OutFormat::Jpeg,
            quality: 90,
            resize: Resize::LongEdge(100),
            template: "{name}".into(),
            out_dir: out.clone(),
        };
        let sources = vec![a, b, dir.path().join("missing.png")];
        let summary = run_export(&sources, &options, || false, |_, _| {}).unwrap();
        assert_eq!(
            summary,
            Summary {
                exported: 2,
                failed: 1,
                cancelled: false
            }
        );

        let a_out = out.join("a.jpg");
        assert_eq!(image::image_dimensions(&a_out).unwrap(), (100, 50));
        assert_eq!(image::guess_format(&fs::read(&a_out).unwrap()).unwrap(), ImageFormat::Jpeg);
        // Never upscaled: the 50x50 source stays 50x50.
        assert_eq!(image::image_dimensions(out.join("b.jpg")).unwrap(), (50, 50));
    }

    #[test]
    fn existing_destination_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.png");
        RgbImage::from_pixel(20, 20, Rgb([1, 2, 3])).save(&source).unwrap();
        let out = dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("a.jpg"), b"keep me").unwrap();
        let options = Options {
            format: OutFormat::Jpeg,
            quality: 80,
            resize: Resize::Original,
            template: "{name}".into(),
            out_dir: out.clone(),
        };
        let summary = run_export(&[source.clone()], &options, || false, |_, _| {}).unwrap();
        assert_eq!(summary.exported, 1);
        assert_eq!(fs::read(out.join("a.jpg")).unwrap(), b"keep me");
        assert_eq!(
            image::guess_format(&fs::read(out.join("a (2).jpg")).unwrap()).unwrap(),
            ImageFormat::Jpeg
        );
        assert!(source.exists());
    }

    #[test]
    fn png_and_webp_outputs_are_readable() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("s.png");
        RgbImage::from_pixel(30, 10, Rgb([9, 9, 9])).save(&source).unwrap();
        for format in [OutFormat::Png, OutFormat::WebP, OutFormat::Tiff] {
            let bytes = encode(&image::open(&source).unwrap(), format, 90).unwrap();
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert_eq!(decoded.dimensions(), (30, 10), "{}", format.label());
        }
    }
}
