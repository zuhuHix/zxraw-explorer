//! Batch export / convert of the selected images (or every image in the folder), and
//! "Deliver to client": the same export driven by a saved delivery preset.
//!
//! Each source is decoded (RAW files through their embedded JPEG preview), optionally
//! resized (never upscaled), watermarked, encoded with the chosen metadata and published
//! into the output folder with a no-replace rename so an existing file is never
//! overwritten. A zip output is written under a temporary name and renamed when complete.

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
use image::{DynamicImage, GenericImageView, ImageEncoder};
use parking_lot::Mutex;

use crate::app::ZxrawApp;
use crate::delivery_meta::{MetadataPolicy, exif_for_export};
use crate::delivery_presets::{
    DeliveryPreset, OutputKind, PresetFormat, PresetResize, PresetStore, SourceFilter,
    filter_sources, zip_file_name,
};
use crate::watermark::{Anchor, PreparedWatermark, Watermark, WatermarkSource};

const PREVIEW_ROWS: usize = 20;
/// Long edge of the watermark preview as shown in the window.
const WATERMARK_PREVIEW_EDGE: u32 = 360;
/// Long edge of the image the preview is built from (sharp on high-DPI screens, cheap
/// to decode).
const PREVIEW_SOURCE_EDGE: u32 = 1280;
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

impl From<PresetFormat> for OutFormat {
    fn from(format: PresetFormat) -> Self {
        match format {
            PresetFormat::Jpeg => Self::Jpeg,
            PresetFormat::Png => Self::Png,
            PresetFormat::Tiff => Self::Tiff,
            PresetFormat::WebP => Self::WebP,
        }
    }
}

impl From<OutFormat> for PresetFormat {
    fn from(format: OutFormat) -> Self {
        match format {
            OutFormat::Jpeg => Self::Jpeg,
            OutFormat::Png => Self::Png,
            OutFormat::Tiff => Self::Tiff,
            OutFormat::WebP => Self::WebP,
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
struct Options {
    format: OutFormat,
    quality: u8,
    resize: Resize,
    template: String,
    out_dir: PathBuf,
    watermark: Option<Arc<PreparedWatermark>>,
    metadata: MetadataPolicy,
    /// File name of the zip to write into `out_dir`; `None` writes loose files.
    zip_name: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Summary {
    exported: usize,
    failed: usize,
    cancelled: bool,
    /// Published zip file name, for a zip output.
    archive: Option<String>,
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
    watermark_on: bool,
    watermark: Watermark,
    /// Text kept while the watermark is switched to an image, and the other way round.
    watermark_text: String,
    watermark_path: String,
    output: OutputKind,
    zip_name: String,
    filter: SourceFilter,
    metadata: MetadataPolicy,
    /// Loaded on the first frame; `None` until then.
    presets: Option<PresetStore>,
    presets_path: PathBuf,
    /// Selected preset in `presets`.
    preset: Option<usize>,
    preset_name: String,
    /// Preset requested from the "Deliver with" menu, applied by [`deliver`].
    pending_preset: Option<String>,
    prepared: Option<(Watermark, Result<Arc<PreparedWatermark>, String>)>,
    preview: WatermarkPreview,
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
            watermark_on: false,
            watermark: Watermark::default(),
            watermark_text: String::new(),
            watermark_path: String::new(),
            output: OutputKind::Folder,
            zip_name: "{folder}_{date}".into(),
            filter: SourceFilter::All,
            metadata: MetadataPolicy::None,
            presets: None,
            presets_path: PathBuf::new(),
            preset: None,
            preset_name: String::new(),
            pending_preset: None,
            prepared: None,
            preview: WatermarkPreview::default(),
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
    ensure_presets(app, &mut state);
    state.open = true;
    state.out_dir = app.pane().path.join("export").display().to_string();
}

/// `Cmd::Deliver`: the export window set up from the preset picked in the "Deliver with"
/// menu, else from the last-used preset.
pub fn deliver(app: &mut ZxrawApp) {
    let mut state = ui_state().lock();
    ensure_presets(app, &mut state);
    state.open = true;
    state.out_dir = app.pane().path.join("delivery").display().to_string();
    let wanted = state.pending_preset.take();
    let index = state.presets.as_ref().and_then(|store| {
        wanted
            .and_then(|name| store.index_of(&name))
            .or_else(|| store.last_used_index())
    });
    match index {
        Some(index) => select_preset(&mut state, index),
        None => {
            drop(state);
            app.toast("No delivery presets yet: set up the export and save it as a preset.");
        }
    }
}

/// "Deliver with ▸ <preset>" submenu for the file context menu. Returns true when a preset
/// was clicked; the caller then runs `Cmd::Deliver`, which applies that preset.
pub fn deliver_menu(ui: &mut egui::Ui) -> bool {
    let names: Vec<String> = ui_state()
        .lock()
        .presets
        .as_ref()
        .map(|store| {
            store
                .presets
                .iter()
                .map(|preset| preset.name.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut chosen = None;
    ui.menu_button("Deliver with", |ui| {
        if names.is_empty() {
            ui.label(egui::RichText::new("No presets").weak());
        }
        for name in &names {
            if ui.button(name).clicked() {
                chosen = Some(name.clone());
                ui.close();
            }
        }
    });
    let clicked = chosen.is_some();
    if clicked {
        ui_state().lock().pending_preset = chosen;
    }
    clicked
}

/// Called every frame by the app shell.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    // The state is moved out for the frame so the window closure can borrow it freely.
    let mut state = std::mem::take(&mut *ui_state().lock());
    // Loaded up front so the "Deliver with" menu can list the presets.
    ensure_presets(app, &mut state);
    report_finished(app, &mut state);
    if state.open
        || state
            .run
            .as_ref()
            .is_some_and(|run| !run.handle.state().is_finished())
    {
        draw(app, ctx, &mut state);
    }
    // A "Deliver with" click during this frame landed in the placeholder state.
    let pending = ui_state().lock().pending_preset.take();
    if pending.is_some() {
        state.pending_preset = pending;
    }
    *ui_state().lock() = state;
}

fn ensure_presets(app: &ZxrawApp, state: &mut BatchUi) {
    if state.presets.is_none() {
        let path = PresetStore::path(&app.paths.config);
        state.presets = Some(PresetStore::load(&path));
        state.presets_path = path;
    }
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
                "Batch export {}: exported {}, failed {}{}",
                if cancelled { "cancelled" } else { "finished" },
                summary.exported,
                summary.failed,
                summary
                    .archive
                    .as_ref()
                    .map(|name| format!(" → {name}"))
                    .unwrap_or_default()
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

/// Sets the window up from preset `index`.
fn select_preset(state: &mut BatchUi, index: usize) {
    let Some(preset) = state
        .presets
        .as_ref()
        .and_then(|store| store.presets.get(index))
        .cloned()
    else {
        return;
    };
    state.preset = Some(index);
    state.preset_name = preset.name.clone();
    apply_preset(state, &preset);
}

fn apply_preset(state: &mut BatchUi, preset: &DeliveryPreset) {
    state.format = preset.format.into();
    state.quality = preset.quality.clamp(1, 100);
    match preset.resize {
        PresetResize::Original => state.resize = ResizeMode::Original,
        PresetResize::LongEdge { px } => {
            state.resize = ResizeMode::LongEdge;
            state.long_edge = px.to_string();
        }
        PresetResize::Within { width, height } => {
            state.resize = ResizeMode::Within;
            state.width = width.to_string();
            state.height = height.to_string();
        }
    }
    state.template = preset.template.clone();
    state.watermark_on = preset.watermark.is_some();
    if let Some(watermark) = &preset.watermark {
        state.watermark = watermark.clone();
        match &watermark.source {
            WatermarkSource::Text { text } => state.watermark_text = text.clone(),
            WatermarkSource::Image { path } => {
                state.watermark_path = path.display().to_string();
            }
        }
    }
    state.output = preset.output;
    state.zip_name = preset.zip_name.clone();
    state.filter = preset.filter;
    state.metadata = preset.metadata;
}

/// The window's current settings as a preset named `name`.
fn preset_from_state(state: &BatchUi, name: &str) -> Result<DeliveryPreset, String> {
    let resize = match parse_resize(state)? {
        Resize::Original => PresetResize::Original,
        Resize::LongEdge(px) => PresetResize::LongEdge { px },
        Resize::Within(width, height) => PresetResize::Within { width, height },
    };
    Ok(DeliveryPreset {
        name: name.trim().to_string(),
        format: state.format.into(),
        quality: state.quality.clamp(1, 100),
        resize,
        template: state.template.clone(),
        watermark: state.watermark_on.then(|| state.watermark.clone()),
        output: state.output,
        zip_name: state.zip_name.clone(),
        filter: state.filter,
        metadata: state.metadata,
    })
}

/// Saves the preset store, reporting a failure as a toast.
fn save_presets(app: &mut ZxrawApp, state: &BatchUi) {
    if let Some(store) = &state.presets
        && let Err(error) = store.save(&state.presets_path)
    {
        app.toast_err(format!("Could not save delivery presets: {error}"));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PresetAction {
    Select(usize),
    Save,
    SaveAsNew,
    Duplicate,
    Delete,
    RestoreBuiltins,
}

fn preset_bar(ui: &mut egui::Ui, state: &mut BatchUi) -> Option<PresetAction> {
    let mut action = None;
    let names: Vec<String> = state
        .presets
        .as_ref()
        .map(|store| {
            store
                .presets
                .iter()
                .map(|preset| preset.name.clone())
                .collect()
        })
        .unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label("Preset");
        let current = state
            .preset
            .and_then(|index| names.get(index))
            .map_or("(none)", String::as_str);
        egui::ComboBox::from_id_salt("batch-export-preset")
            .selected_text(current)
            .width(280.0)
            .show_ui(ui, |ui| {
                for (index, name) in names.iter().enumerate() {
                    if ui
                        .selectable_label(state.preset == Some(index), name)
                        .clicked()
                    {
                        action = Some(PresetAction::Select(index));
                    }
                }
            });
    });
    ui.horizontal(|ui| {
        ui.label("Name");
        ui.add(egui::TextEdit::singleline(&mut state.preset_name).desired_width(220.0));
        let named = !state.preset_name.trim().is_empty();
        if ui
            .add_enabled(named, egui::Button::new("Save"))
            .on_hover_text("Save these settings into the selected preset (or a new one)")
            .clicked()
        {
            action = Some(PresetAction::Save);
        }
        if ui
            .add_enabled(named, egui::Button::new("Save as new"))
            .clicked()
        {
            action = Some(PresetAction::SaveAsNew);
        }
        let selected = state.preset.is_some();
        if ui
            .add_enabled(selected, egui::Button::new("Duplicate"))
            .clicked()
        {
            action = Some(PresetAction::Duplicate);
        }
        if ui
            .add_enabled(selected, egui::Button::new("Delete"))
            .clicked()
        {
            action = Some(PresetAction::Delete);
        }
        if ui
            .button("Restore built-ins")
            .on_hover_text("Add back any built-in preset that was deleted")
            .clicked()
        {
            action = Some(PresetAction::RestoreBuiltins);
        }
    });
    action
}

fn run_preset_action(app: &mut ZxrawApp, state: &mut BatchUi, action: PresetAction) {
    if let PresetAction::Select(index) = action {
        select_preset(state, index);
        return;
    }
    let name = state.preset_name.trim().to_string();
    let result: Result<(), String> = (|| {
        let preset = match action {
            PresetAction::Save | PresetAction::SaveAsNew => Some(preset_from_state(state, &name)?),
            _ => None,
        };
        let selected = state.preset;
        let store = state.presets.as_mut().ok_or("Presets are not loaded.")?;
        match action {
            PresetAction::Select(_) => {}
            PresetAction::Save => {
                let mut preset = preset.expect("built above");
                if let Some(index) = selected.filter(|index| *index < store.presets.len()) {
                    store.rename(index, &name)?;
                    preset.name = store.presets[index].name.clone();
                    store.presets[index] = preset;
                } else {
                    preset.name = store.unique_name(&name);
                    state.preset = Some(store.upsert(preset));
                }
            }
            PresetAction::SaveAsNew => {
                let mut preset = preset.expect("built above");
                preset.name = store.unique_name(&name);
                state.preset = Some(store.upsert(preset));
            }
            PresetAction::Duplicate => {
                if let Some(index) = selected {
                    state.preset = Some(store.duplicate(index));
                }
            }
            PresetAction::Delete => {
                if let Some(index) = selected {
                    store.delete(index);
                    state.preset = None;
                }
            }
            PresetAction::RestoreBuiltins => store.restore_builtins(),
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Some(index) = state.preset {
                select_preset(state, index);
            }
            save_presets(app, state);
        }
        Err(error) => app.toast_err(error),
    }
}

/// Re-prepares the watermark when its settings changed since the last frame.
fn refresh_watermark(state: &mut BatchUi) {
    if !state.watermark_on {
        return;
    }
    if let WatermarkSource::Image { path } = &mut state.watermark.source {
        *path = normalize_user_path(state.watermark_path.trim());
    }
    let stale = state
        .prepared
        .as_ref()
        .is_none_or(|(watermark, _)| *watermark != state.watermark);
    if stale {
        let prepared = PreparedWatermark::prepare(&state.watermark).map(Arc::new);
        state.prepared = Some((state.watermark.clone(), prepared));
    }
}

fn prepared_watermark(state: &BatchUi) -> Result<Option<Arc<PreparedWatermark>>, String> {
    if !state.watermark_on {
        return Ok(None);
    }
    match &state.prepared {
        Some((watermark, Ok(prepared))) if *watermark == state.watermark => {
            Ok(Some(prepared.clone()))
        }
        Some((watermark, Err(error))) if *watermark == state.watermark => Err(error.clone()),
        _ => Err("The watermark is not ready yet.".into()),
    }
}

fn watermark_section(ui: &mut egui::Ui, state: &mut BatchUi) {
    ui.checkbox(&mut state.watermark_on, "Watermark");
    if !state.watermark_on {
        return;
    }
    let is_image = matches!(state.watermark.source, WatermarkSource::Image { .. });
    ui.horizontal(|ui| {
        if ui.selectable_label(!is_image, "Text").clicked() && is_image {
            let text = if state.watermark_text.trim().is_empty() {
                "© Your Name".to_string()
            } else {
                state.watermark_text.clone()
            };
            state.watermark.source = WatermarkSource::Text { text };
        }
        if ui.selectable_label(is_image, "Image (PNG)").clicked() && !is_image {
            if let WatermarkSource::Text { text } = &state.watermark.source {
                state.watermark_text = text.clone();
            }
            state.watermark.source = WatermarkSource::Image {
                path: PathBuf::from(state.watermark_path.trim()),
            };
        }
    });
    match &mut state.watermark.source {
        WatermarkSource::Text { text } => {
            ui.add(egui::TextEdit::singleline(text).desired_width(320.0));
        }
        WatermarkSource::Image { .. } => {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut state.watermark_path).desired_width(260.0));
                if ui.button("Browse…").clicked()
                    && let Some(path) = crate::dialogs::pick_file(
                        rfd::FileDialog::new()
                            .add_filter("Images", &["png", "webp", "jpg", "jpeg"]),
                    )
                {
                    state.watermark_path = path.display().to_string();
                }
            });
        }
    }
    ui.horizontal(|ui| {
        ui.label("Position");
        egui::Grid::new("batch-export-anchor")
            .spacing([2.0, 2.0])
            .show(ui, |ui| {
                for (index, anchor) in Anchor::ALL.into_iter().enumerate() {
                    let symbol = ["↖", "↑", "↗", "←", "•", "→", "↙", "↓", "↘"][index];
                    ui.selectable_value(&mut state.watermark.anchor, anchor, symbol)
                        .on_hover_text(anchor.label());
                    if index % 3 == 2 {
                        ui.end_row();
                    }
                }
            });
        ui.vertical(|ui| {
            ui.add(
                egui::Slider::new(&mut state.watermark.size_pct, 1.0..=100.0)
                    .text("size % of width"),
            );
            let mut opacity = state.watermark.opacity * 100.0;
            ui.add(egui::Slider::new(&mut opacity, 0.0..=100.0).text("opacity %"));
            state.watermark.opacity = opacity / 100.0;
            ui.add(egui::Slider::new(&mut state.watermark.margin_pct, 0.0..=25.0).text("margin %"));
        });
    });
}

/// What a preview image shows: one source with one watermark setting.
#[derive(Clone, PartialEq)]
struct PreviewKey {
    source: PathBuf,
    /// `None` = no watermark.
    watermark: Option<Watermark>,
}

/// A preview built on a worker thread.
struct PreviewResult {
    key: PreviewKey,
    /// The downscaled source, kept so the next watermark change does not decode again.
    /// `None` when the source cannot be decoded.
    base: Option<Arc<image::RgbaImage>>,
    image: Option<image::RgbaImage>,
}

/// Downscaled copy of the first source with the current watermark applied. Decoding and
/// compositing run on a worker thread; at most one request is in flight.
#[derive(Default)]
struct WatermarkPreview {
    /// Key of the texture on screen.
    shown: Option<PreviewKey>,
    texture: Option<egui::TextureHandle>,
    /// Last decoded source and its downscaled pixels.
    base: Option<(PathBuf, Option<Arc<image::RgbaImage>>)>,
    pending: Option<(PreviewKey, crossbeam_channel::Receiver<PreviewResult>)>,
}

/// The downscaled source for the preview, from the thumbnail pipeline: RAW files use
/// their embedded preview, JPEGs a large enough EXIF thumbnail, everything else a decode
/// under the usual pixel and allocation limits. Orientation is applied.
fn preview_base(source: &Path) -> Option<image::RgbaImage> {
    let limits = forge_media::thumbs::DecodeLimits::default();
    forge_media::thumbs::generate_thumbnail(source, PREVIEW_SOURCE_EDGE, &limits, None, 0, None)
        .ok()?
        .to_rgba_image()
}

/// Shows `source` downscaled with the current watermark, with a spinner while it loads.
fn watermark_preview(ui: &mut egui::Ui, state: &mut BatchUi, source: &Path) {
    let wanted = PreviewKey {
        source: source.to_path_buf(),
        watermark: state.watermark_on.then(|| state.watermark.clone()),
    };
    let watermark = prepared_watermark(state);
    let preview = &mut state.preview;

    if let Some((_, receiver)) = &preview.pending {
        match receiver.try_recv() {
            Ok(result) => {
                preview.pending = None;
                preview.base = Some((result.key.source.clone(), result.base));
                // A result for an older file or watermark is dropped, not shown.
                if result.key == wanted {
                    preview.texture = result.image.map(|image| {
                        let size = [image.width() as usize, image.height() as usize];
                        let pixels = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
                        ui.ctx().load_texture(
                            "batch-export-watermark-preview",
                            pixels,
                            egui::TextureOptions::LINEAR,
                        )
                    });
                    preview.shown = Some(result.key);
                }
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => preview.pending = None,
        }
    }

    if preview.shown.as_ref() != Some(&wanted) && preview.pending.is_none() {
        let base = preview
            .base
            .as_ref()
            .filter(|(path, _)| *path == wanted.source)
            .map(|(_, base)| base.clone());
        let mark = watermark.as_ref().ok().cloned().flatten();
        let key = wanted.clone();
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let ctx = ui.ctx().clone();
        std::thread::spawn(move || {
            let base = base.unwrap_or_else(|| preview_base(&key.source).map(Arc::new));
            let image = base.as_ref().map(|base| {
                let mut image = image::RgbaImage::clone(base);
                if let Some(mark) = &mark
                    && key.watermark.is_some()
                {
                    mark.apply_rgba(&mut image);
                }
                image
            });
            // The receiver is gone when a newer request replaced this one.
            if sender.send(PreviewResult { key, base, image }).is_ok() {
                ctx.request_repaint();
            }
        });
        preview.pending = Some((wanted.clone(), receiver));
    }

    ui.horizontal(|ui| {
        ui.label(format!(
            "Preview: {}",
            source.file_name().unwrap_or_default().to_string_lossy()
        ));
        if preview.pending.is_some() {
            ui.spinner();
        }
    });
    let showing_source = preview
        .shown
        .as_ref()
        .is_some_and(|shown| shown.source == source);
    match &preview.texture {
        Some(texture) if showing_source => {
            let size = texture.size_vec2();
            let scale = (WATERMARK_PREVIEW_EDGE as f32 / size.x.max(size.y)).min(1.0);
            ui.image((texture.id(), size * scale));
        }
        _ if showing_source && preview.pending.is_none() => {
            ui.label(egui::RichText::new("No preview: this image cannot be decoded.").weak());
        }
        _ => {}
    }
    if let Err(error) = watermark {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
}

fn draw(app: &mut ZxrawApp, ctx: &egui::Context, state: &mut BatchUi) {
    let (all_sources, from_selection) = collect_sources(app);
    let annotations = &app.ops.library.annotations;
    let sources = filter_sources(&all_sources, state.filter, |path| {
        annotations.get(path).copied()
    });
    let folder = folder_label(&app.pane().path);
    refresh_watermark(state);
    let active = state
        .run
        .as_ref()
        .filter(|run| !run.handle.state().is_finished())
        .map(|run| run.handle.clone());

    let mut is_open = state.open;
    let mut start = false;
    let mut close_after = false;
    let mut preset_action = None;
    let max_height = ctx.content_rect().height() * 0.8;
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

            egui::ScrollArea::vertical().max_height(max_height).show(ui, |ui| {
            preset_action = preset_bar(ui, state);
            ui.separator();

            ui.label("Convert or resize the images into a new folder. Originals are never changed.");
            if all_sources.is_empty() {
                ui.label(egui::RichText::new("No images in the selection or this folder.").weak());
            } else if from_selection {
                ui.label(format!("{} selected image(s)", all_sources.len()));
            } else {
                ui.label(format!("All {} image(s) in this folder", all_sources.len()));
            }
            ui.horizontal(|ui| {
                ui.label("Include");
                let mut stars = match state.filter {
                    SourceFilter::MinRating { stars } => stars,
                    _ => 3,
                };
                ui.selectable_value(&mut state.filter, SourceFilter::All, "All");
                ui.selectable_value(&mut state.filter, SourceFilter::PicksOnly, "Only picks");
                let is_rating = matches!(state.filter, SourceFilter::MinRating { .. });
                if ui.selectable_label(is_rating, "Rating at least").clicked() {
                    state.filter = SourceFilter::MinRating { stars };
                }
                if is_rating {
                    ui.add(egui::Slider::new(&mut stars, 1..=5).suffix("★"));
                    state.filter = SourceFilter::MinRating { stars };
                }
            });
            if state.filter != SourceFilter::All {
                ui.label(format!(
                    "{} of {} image(s) match ({})",
                    sources.len(),
                    all_sources.len(),
                    state.filter.label()
                ));
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
                ui.label("Metadata");
                egui::ComboBox::from_id_salt("batch-export-metadata")
                    .selected_text(state.metadata.label())
                    .show_ui(ui, |ui| {
                        for policy in MetadataPolicy::ALL {
                            ui.selectable_value(&mut state.metadata, policy, policy.label());
                        }
                    });
            });
            if state.metadata != MetadataPolicy::None && state.format == OutFormat::Tiff {
                ui.label(egui::RichText::new("TIFF output carries no metadata.").weak());
            }
            ui.separator();

            watermark_section(ui, state);
            if state.watermark_on
                && let Some(first) = sources.first()
            {
                watermark_preview(ui, state, first);
            }
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Output folder");
                ui.add(egui::TextEdit::singleline(&mut state.out_dir).desired_width(320.0));
            });
            ui.horizontal(|ui| {
                ui.label("Save as");
                ui.selectable_value(&mut state.output, OutputKind::Folder, "Files");
                ui.selectable_value(&mut state.output, OutputKind::Zip, "One .zip");
                if state.output == OutputKind::Zip {
                    ui.add(egui::TextEdit::singleline(&mut state.zip_name).desired_width(160.0));
                    ui.label(
                        egui::RichText::new(zip_file_name(&state.zip_name, &folder, &today())).weak(),
                    );
                }
            });
            ui.horizontal(|ui| {
                ui.label("Naming");
                ui.add(egui::TextEdit::singleline(&mut state.template).desired_width(200.0));
            });
            ui.label(
                egui::RichText::new(
                    "Tokens: {name} {ext} {n} {n:3} {w} {h}; zip name: {folder} {date}. Without \
                     {ext} the extension is added. Existing files are never overwritten.",
                )
                .weak(),
            );

            match build_options(state, &folder) {
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
            });

            ui.horizontal(|ui| {
                let can_start = !sources.is_empty() && build_options(state, &folder).is_ok();
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

    if let Some(action) = preset_action {
        run_preset_action(app, state, action);
    }
    state.open = is_open && !close_after;
    if start {
        let result = build_options(state, &folder)
            .and_then(|options| start_job(app, sources, options, state));
        match result {
            Ok(()) => {
                let used = state
                    .preset
                    .and_then(|index| state.presets.as_ref()?.presets.get(index))
                    .map(|preset| preset.name.clone());
                if let Some(store) = state.presets.as_mut()
                    && used.is_some()
                    && store.last_used != used
                {
                    store.last_used = used;
                    save_presets(app, state);
                }
            }
            Err(error) => app.toast_err(error),
        }
    }
}

/// Name of `dir` for `{folder}` in zip names.
fn folder_label(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "delivery".into())
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
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
    let handle = app
        .ops
        .jobs
        .spawn("batch-export", "Batch export", move |job| {
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

fn parse_resize(state: &BatchUi) -> Result<Resize, String> {
    Ok(match state.resize {
        ResizeMode::Original => Resize::Original,
        ResizeMode::LongEdge => Resize::LongEdge(positive(&state.long_edge, "Long edge")?),
        ResizeMode::Within => Resize::Within(
            positive(&state.width, "Width")?,
            positive(&state.height, "Height")?,
        ),
    })
}

/// `folder` is the name `{folder}` expands to in the zip name.
fn build_options(state: &BatchUi, folder: &str) -> Result<Options, String> {
    let resize = parse_resize(state)?;
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
        watermark: prepared_watermark(state)?,
        metadata: state.metadata,
        zip_name: (state.output == OutputKind::Zip)
            .then(|| zip_file_name(&state.zip_name, folder, &today())),
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
        // Reading a header is only worth it when the template shows the size.
        let dims = if options.template.contains("{w") || options.template.contains("{h") {
            *cache
                .entry(source.clone())
                .or_insert_with(|| image::image_dimensions(source).ok())
        } else {
            None
        };
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
    fs::create_dir_all(&options.out_dir)
        .map_err(|error| format!("cannot create {}: {error}", options.out_dir.display()))?;
    let mut zip = match &options.zip_name {
        Some(_) => {
            let (path, file) = create_temporary(&options.out_dir)
                .map_err(|error| format!("cannot create the zip: {error}"))?;
            Some((path, zip::ZipWriter::new(file)))
        }
        None => None,
    };
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
        let outcome = render_one(source, index + 1, options, &taken).and_then(|(name, bytes)| {
            let published = match &mut zip {
                Some((_, writer)) => add_to_zip(writer, &name, &bytes, options.format)
                    .map(|()| name)
                    .map_err(|error| format!("zip write failed: {error}"))?,
                None => publish_no_replace(&options.out_dir, &name, &bytes)
                    .map_err(|error| format!("write failed: {error}"))?,
            };
            taken.insert(published.to_lowercase());
            Ok(published)
        });
        match &outcome {
            Ok(_) => summary.exported += 1,
            Err(_) => summary.failed += 1,
        }
        report(&label, &outcome);
    }
    if let (Some((temporary, writer)), Some(zip_name)) = (zip, &options.zip_name) {
        summary.archive = finish_zip(temporary, writer, &options.out_dir, zip_name, &summary)?;
    }
    Ok(summary)
}

/// Decodes, resizes, watermarks and encodes one source. Returns the output name (unique
/// within `taken`) and the encoded bytes.
fn render_one(
    source: &Path,
    number: usize,
    options: &Options,
    taken: &HashSet<String>,
) -> Result<(String, Vec<u8>), String> {
    let image = load_source(source)?;
    let mut image = resize_to(image, options.resize);
    if let Some(watermark) = &options.watermark {
        image = watermark.apply(image)?;
    }
    let dims = image.dimensions();
    let raw = build_name(
        &options.template,
        &file_stem(source),
        options.format,
        number,
        Some(dims),
    );
    let name = dedupe(&raw, taken);
    let exif = exif_for_export(source, options.metadata);
    let bytes = encode(&image, options.format, options.quality, exif)?;
    Ok((name, bytes))
}

fn add_to_zip(
    writer: &mut zip::ZipWriter<fs::File>,
    name: &str,
    bytes: &[u8],
    format: OutFormat,
) -> zip::result::ZipResult<()> {
    // JPEG and WebP are already compressed; deflating them only costs time.
    let method = match format {
        OutFormat::Jpeg | OutFormat::WebP => zip::CompressionMethod::Stored,
        OutFormat::Png | OutFormat::Tiff => zip::CompressionMethod::Deflated,
    };
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(method)
        .large_file(bytes.len() as u64 >= u64::from(u32::MAX));
    writer.start_file(name, options)?;
    writer.write_all(bytes)?;
    Ok(())
}

/// Completes the temporary zip and publishes it as `zip_name` (never replacing a file).
/// A cancelled or empty batch leaves no zip behind.
fn finish_zip(
    temporary: PathBuf,
    writer: zip::ZipWriter<fs::File>,
    dir: &Path,
    zip_name: &str,
    summary: &Summary,
) -> Result<Option<String>, String> {
    let finished = writer
        .finish()
        .map_err(io::Error::other)
        .and_then(|file| file.sync_all());
    if let Err(error) = finished {
        let _ = fs::remove_file(&temporary);
        return Err(format!("cannot finish the zip: {error}"));
    }
    if summary.cancelled || summary.exported == 0 {
        let _ = fs::remove_file(&temporary);
        return Ok(None);
    }
    publish_temporary(&temporary, dir, zip_name)
        .map(Some)
        .map_err(|error| format!("cannot save the zip: {error}"))
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

/// Encodes `image`, embedding `exif` (TIFF-structured EXIF) where the format supports it.
fn encode(
    image: &DynamicImage,
    format: OutFormat,
    quality: u8,
    exif: Option<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    fn with_exif<E: ImageEncoder>(mut encoder: E, exif: Option<Vec<u8>>) -> E {
        if let Some(exif) = exif {
            // Unsupported metadata is dropped rather than failing the export.
            let _ = encoder.set_exif_metadata(exif);
        }
        encoder
    }
    let mut out = Cursor::new(Vec::new());
    let result = match format {
        OutFormat::Jpeg => DynamicImage::ImageRgb8(image.to_rgb8()).write_with_encoder(with_exif(
            JpegEncoder::new_with_quality(&mut out, quality),
            exif,
        )),
        OutFormat::Png => image.write_with_encoder(with_exif(PngEncoder::new(&mut out), exif)),
        OutFormat::Tiff => image.write_with_encoder(TiffEncoder::new(&mut out)),
        OutFormat::WebP => {
            let pixels = if image.color().has_alpha() {
                DynamicImage::ImageRgba8(image.to_rgba8())
            } else {
                DynamicImage::ImageRgb8(image.to_rgb8())
            };
            pixels.write_with_encoder(with_exif(WebPEncoder::new_lossless(&mut out), exif))
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

/// Creates a uniquely named, empty temporary file in `dir`.
fn create_temporary(dir: &Path) -> io::Result<(PathBuf, fs::File)> {
    loop {
        let sequence = BATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            ".zxraw-batch-{}-{sequence:016x}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Writes `bytes` to a temp file in `dir` and publishes it under `name`, or under
/// `name (k)` when that exists. Never replaces an existing file.
fn publish_no_replace(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<String> {
    let (temporary, mut file) = create_temporary(dir)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    publish_temporary(&temporary, dir, name)
}

/// Renames the complete file `temporary` to `name` in `dir`, or to `name (k)` when that
/// exists. Removes `temporary` on failure.
fn publish_temporary(temporary: &Path, dir: &Path, name: &str) -> io::Result<String> {
    let (stem, ext) = split_ext(name);
    let mut counter = 1;
    loop {
        let candidate = if counter == 1 {
            name.to_string()
        } else {
            format!("{stem} ({counter}){ext}")
        };
        match rename_no_replace(temporary, &dir.join(&candidate)) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && counter < 10_000 => {
                counter += 1;
            }
            Err(error) => {
                let _ = fs::remove_file(temporary);
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
            expand_template(
                "{name}_{n:3}_{w}x{h}.{ext}",
                "IMG",
                "jpg",
                7,
                Some((640, 480))
            ),
            "IMG_007_640x480.jpg"
        );
        assert_eq!(expand_template("{n}", "x", "png", 12, None), "12");
        assert_eq!(expand_template("{n:2}", "x", "png", 3, None), "03");
        assert_eq!(expand_template("{w}", "x", "png", 1, None), "?");
        assert_eq!(
            expand_template("{foo}-{n:}", "x", "png", 1, None),
            "{foo}-{n:}"
        );
        assert_eq!(
            expand_template("open {name", "x", "png", 1, None),
            "open {name"
        );
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
        assert_eq!(
            fit_size(4000, 3000, Resize::Within(5000, 5000)),
            (4000, 3000)
        );
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
        RgbImage::from_pixel(200, 100, Rgb([10, 20, 30]))
            .save(&a)
            .unwrap();
        RgbImage::from_pixel(50, 50, Rgb([200, 100, 0]))
            .save(&b)
            .unwrap();
        let out = dir.path().join("out");
        let options = Options {
            format: OutFormat::Jpeg,
            quality: 90,
            resize: Resize::LongEdge(100),
            template: "{name}".into(),
            out_dir: out.clone(),
            watermark: None,
            metadata: MetadataPolicy::None,
            zip_name: None,
        };
        let sources = vec![a, b, dir.path().join("missing.png")];
        let summary = run_export(&sources, &options, || false, |_, _| {}).unwrap();
        assert_eq!(
            summary,
            Summary {
                exported: 2,
                failed: 1,
                cancelled: false,
                archive: None,
            }
        );

        let a_out = out.join("a.jpg");
        assert_eq!(image::image_dimensions(&a_out).unwrap(), (100, 50));
        assert_eq!(
            image::guess_format(&fs::read(&a_out).unwrap()).unwrap(),
            ImageFormat::Jpeg
        );
        // Never upscaled: the 50x50 source stays 50x50.
        assert_eq!(
            image::image_dimensions(out.join("b.jpg")).unwrap(),
            (50, 50)
        );
    }

    #[test]
    fn existing_destination_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.png");
        RgbImage::from_pixel(20, 20, Rgb([1, 2, 3]))
            .save(&source)
            .unwrap();
        let out = dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("a.jpg"), b"keep me").unwrap();
        let options = Options {
            format: OutFormat::Jpeg,
            quality: 80,
            resize: Resize::Original,
            template: "{name}".into(),
            out_dir: out.clone(),
            watermark: None,
            metadata: MetadataPolicy::None,
            zip_name: None,
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
        RgbImage::from_pixel(30, 10, Rgb([9, 9, 9]))
            .save(&source)
            .unwrap();
        for format in [OutFormat::Png, OutFormat::WebP, OutFormat::Tiff] {
            let bytes = encode(&image::open(&source).unwrap(), format, 90, None).unwrap();
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert_eq!(decoded.dimensions(), (30, 10), "{}", format.label());
        }
    }

    fn options(out: &Path, format: OutFormat) -> Options {
        Options {
            format,
            quality: 90,
            resize: Resize::Original,
            template: "{name}".into(),
            out_dir: out.to_path_buf(),
            watermark: None,
            metadata: MetadataPolicy::None,
            zip_name: None,
        }
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn zip_output_holds_every_image_and_never_replaces_a_zip() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.png");
        let b = dir.path().join("b.png");
        RgbImage::from_pixel(40, 20, Rgb([10, 20, 30])).save(&a).unwrap();
        RgbImage::from_pixel(20, 20, Rgb([200, 100, 0])).save(&b).unwrap();
        let out = dir.path().join("out");
        let mut options = options(&out, OutFormat::Jpeg);
        options.zip_name = Some("client.zip".into());
        let sources = vec![a.clone(), b, dir.path().join("missing.png"), a];

        let summary = run_export(&sources, &options, || false, |_, _| {}).unwrap();
        assert_eq!((summary.exported, summary.failed), (3, 1));
        assert_eq!(summary.archive.as_deref(), Some("client.zip"));
        // Only the finished zip is left: no temporary file, no loose images.
        assert_eq!(dir_names(&out), ["client.zip"]);

        let file = fs::File::open(out.join("client.zip")).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut names: Vec<String> = archive
            .file_names()
            .map(|name| name.unwrap().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["a (2).jpg", "a.jpg", "b.jpg"]);
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut archive.by_name("a.jpg").unwrap(), &mut bytes).unwrap();
        assert_eq!(
            image::load_from_memory(&bytes).unwrap().dimensions(),
            (40, 20)
        );

        // A second delivery with the same name is numbered; the first zip is kept.
        let first = fs::read(out.join("client.zip")).unwrap();
        let summary = run_export(&sources[..1], &options, || false, |_, _| {}).unwrap();
        assert_eq!(summary.archive.as_deref(), Some("client (2).zip"));
        assert_eq!(fs::read(out.join("client.zip")).unwrap(), first);
    }

    #[test]
    fn cancelled_or_empty_zip_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.png");
        RgbImage::from_pixel(8, 8, Rgb([1, 2, 3])).save(&source).unwrap();
        let out = dir.path().join("out");
        let mut options = options(&out, OutFormat::Jpeg);
        options.zip_name = Some("client.zip".into());

        let summary = run_export(&[source], &options, || true, |_, _| {}).unwrap();
        assert!(summary.cancelled);
        assert_eq!(summary.archive, None);
        assert!(dir_names(&out).is_empty());

        let missing = dir.path().join("missing.png");
        let summary = run_export(&[missing], &options, || false, |_, _| {}).unwrap();
        assert_eq!((summary.exported, summary.failed), (0, 1));
        assert_eq!(summary.archive, None);
        assert!(dir_names(&out).is_empty());
    }

    /// Writes a JPEG whose EXIF has a copyright, a serial number and a GPS position.
    fn jpeg_with_private_exif(path: &Path) {
        use exif::{Field, In, Rational, Tag, Value};
        let ascii = |text: &str| Value::Ascii(vec![text.as_bytes().to_vec()]);
        let field = |tag, value| Field {
            tag,
            ifd_num: In::PRIMARY,
            value,
        };
        let position = Value::Rational(vec![
            Rational { num: 48, denom: 1 },
            Rational { num: 51, denom: 1 },
            Rational { num: 0, denom: 1 },
        ]);
        let fields = [
            field(Tag::Make, ascii("TestCam")),
            field(Tag::Copyright, ascii("Jane Doe")),
            field(Tag::BodySerialNumber, ascii("SN123")),
            field(Tag::GPSLatitudeRef, ascii("N")),
            field(Tag::GPSLatitude, position),
        ];
        let mut writer = exif::experimental::Writer::new();
        for field in &fields {
            writer.push_field(field);
        }
        let mut tiff = Cursor::new(Vec::new());
        writer.write(&mut tiff, false).unwrap();
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(32, 16, Rgb([90, 90, 90])));
        let mut bytes = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut bytes, 90);
        encoder.set_exif_metadata(tiff.into_inner()).unwrap();
        image.write_with_encoder(encoder).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn read_exif(path: &Path) -> Option<exif::Exif> {
        let mut reader = io::BufReader::new(fs::File::open(path).unwrap());
        exif::Reader::new().read_from_container(&mut reader).ok()
    }

    #[test]
    fn exported_jpeg_keeps_copyright_but_not_gps_or_serials() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photo.jpg");
        jpeg_with_private_exif(&source);
        let original = read_exif(&source).unwrap();
        assert!(original.fields().any(|f| f.tag == exif::Tag::GPSLatitude));

        let clean_out = dir.path().join("clean");
        let mut options = options(&clean_out, OutFormat::Jpeg);
        options.metadata = MetadataPolicy::Clean;
        run_export(std::slice::from_ref(&source), &options, || false, |_, _| {}).unwrap();
        let exif = read_exif(&clean_out.join("photo.jpg")).expect("EXIF carried over");
        let copyright = exif.get_field(exif::Tag::Copyright, exif::In::PRIMARY).unwrap();
        assert!(copyright.display_value().to_string().contains("Jane Doe"));
        assert!(exif.get_field(exif::Tag::Make, exif::In::PRIMARY).is_some());
        assert!(exif.fields().all(|f| f.tag != exif::Tag::BodySerialNumber));
        assert!(exif.fields().all(|f| f.tag.context() != exif::Context::Gps));

        let bare_out = dir.path().join("bare");
        let options = Options {
            out_dir: bare_out.clone(),
            metadata: MetadataPolicy::None,
            ..options
        };
        run_export(&[source], &options, || false, |_, _| {}).unwrap();
        assert!(read_exif(&bare_out.join("photo.jpg")).is_none());
    }

    #[test]
    fn exported_image_carries_the_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let logo = dir.path().join("logo.png");
        image::RgbaImage::from_pixel(10, 10, image::Rgba([255, 0, 0, 255]))
            .save(&logo)
            .unwrap();
        let source = dir.path().join("s.png");
        RgbImage::from_pixel(100, 50, Rgb([0, 0, 0])).save(&source).unwrap();
        let watermark = Watermark {
            source: WatermarkSource::Image { path: logo },
            anchor: Anchor::BottomRight,
            size_pct: 20.0,
            opacity: 1.0,
            margin_pct: 0.0,
        };
        let out = dir.path().join("out");
        let mut options = options(&out, OutFormat::Png);
        options.watermark = Some(Arc::new(PreparedWatermark::prepare(&watermark).unwrap()));
        run_export(&[source], &options, || false, |_, _| {}).unwrap();

        let image = image::open(out.join("s.png")).unwrap().to_rgb8();
        assert_eq!(image.dimensions(), (100, 50));
        // A 20 x 20 mark in the bottom-right corner; the rest of the image is untouched.
        assert_eq!(image.get_pixel(99, 49), &Rgb([255, 0, 0]));
        assert_eq!(image.get_pixel(80, 30), &Rgb([255, 0, 0]));
        assert_eq!(image.get_pixel(79, 49), &Rgb([0, 0, 0]));
        assert_eq!(image.get_pixel(0, 0), &Rgb([0, 0, 0]));
    }

    #[test]
    fn presets_map_onto_the_window_and_back() {
        for preset in PresetStore::builtins() {
            let mut state = BatchUi::default();
            apply_preset(&mut state, &preset);
            assert_eq!(preset_from_state(&state, &preset.name).unwrap(), preset);
        }
        let mut state = BatchUi {
            resize: ResizeMode::LongEdge,
            long_edge: "0".into(),
            ..BatchUi::default()
        };
        assert!(preset_from_state(&state, "Broken").is_err());
        state.long_edge = "1600".into();
        assert_eq!(
            preset_from_state(&state, " Web ").unwrap().resize,
            PresetResize::LongEdge { px: 1600 }
        );
    }

    #[test]
    fn zip_option_names_the_archive_from_the_folder() {
        let mut state = BatchUi {
            out_dir: "C:/out".into(),
            output: OutputKind::Zip,
            zip_name: "{folder}_proofs".into(),
            ..BatchUi::default()
        };
        let options = build_options(&state, "Wedding").unwrap();
        assert_eq!(options.zip_name.as_deref(), Some("Wedding_proofs.zip"));
        state.output = OutputKind::Folder;
        assert_eq!(build_options(&state, "Wedding").unwrap().zip_name, None);
    }
}
