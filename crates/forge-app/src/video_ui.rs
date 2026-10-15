//! Video workflow UI: preview filmstrip/scrubbing, related clips, and proxy generation.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::panic::{self, AssertUnwindSafe};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{RichText, Sense, TextureHandle, Ui};
use forge_core::model::{EntryKind, FileEntry, MediaKind, ext_of, format_size};
use forge_core::paths::rename_no_replace;
use forge_jobs::{CancelToken, JobHandle};
use forge_media::{FfTools, FilmstripFrame, VideoMetadata};
use image::RgbaImage;
use lru::LruCache;

use crate::actions::Event;
use crate::app::ZxrawApp;
use crate::export_assoc::{has_reparse_attribute, is_link_or_junction};

const FILMSTRIP_FRAMES: usize = 8;
const FILMSTRIP_FRAME_EDGE: u32 = 256;
const SCRUB_FRAME_EDGE: u32 = 480;
const FRAME_CACHE_CAPACITY: usize = 128;
const STRIP_CACHE_CAPACITY: usize = 12;
const MAX_SESSION_GAP: Duration = Duration::from_secs(30 * 60);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ENCODER_PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const RELATED_FOLDER_CACHE_CAPACITY: usize = 16;
const RELATED_PATHS_PER_FOLDER: usize = 64;
const MAX_LOCAL_RELATED_ENTRIES: usize = 10_000;
const MAX_LOCAL_RELATED_DEPTH: usize = 8;
const MAX_DRAWN_RELATED_ROWS: usize = 50;
const MAX_PROXY_SCAN_ENTRIES: usize = 40_000;
const MAX_PROXY_SCAN_DIRECTORIES: usize = 4_096;
const MAX_PROXY_SCAN_DEPTH: usize = 32;
const MAX_PROXY_SCAN_MATCHES: usize = 1_000;
const MAX_PROXY_SCAN_SOURCES: usize = 256;
const MAX_PROXY_SCAN_TIME: Duration = Duration::from_secs(30);

type MediaResult<T> = Result<T, String>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MediaKey {
    path: PathBuf,
    modified: Option<SystemTime>,
}

impl MediaKey {
    fn from_entry(entry: &FileEntry) -> Self {
        Self {
            path: entry.path.clone(),
            modified: entry.modified,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FrameKey {
    media: MediaKey,
    time_ms: u64,
}

impl FrameKey {
    fn at(media: &MediaKey, seconds: f64) -> Self {
        let seconds = if seconds.is_finite() {
            seconds.max(0.0)
        } else {
            0.0
        };
        Self {
            media: media.clone(),
            time_ms: (seconds * 1_000.0).round().clamp(0.0, u64::MAX as f64) as u64,
        }
    }

    fn seconds(&self) -> f64 {
        self.time_ms as f64 / 1_000.0
    }
}

enum VideoMessage {
    Probe {
        media: MediaKey,
        generation: u64,
        result: Box<MediaResult<VideoMetadata>>,
    },
    Filmstrip {
        media: MediaKey,
        generation: u64,
        result: MediaResult<Vec<FilmstripFrame>>,
    },
    Frame {
        key: FrameKey,
        generation: u64,
        result: MediaResult<RgbaImage>,
    },
    Estimate {
        dialog_id: u64,
        generation: u64,
        result: MediaResult<ProxyEstimate>,
    },
    Relations {
        media: MediaKey,
        generation: u64,
        take_label: Option<String>,
        related: Vec<Related>,
    },
    ProxyScan {
        scan_id: u64,
        result: Result<ProxyScanResults, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Related {
    pub path: PathBuf,
    pub label: String,
}

#[derive(Debug, Clone, Default)]
struct ProxyScanResults {
    related: Vec<Related>,
    truncated: bool,
}

struct ProxyScanRequest {
    root: PathBuf,
    sources: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipGroupKind {
    Take,
    Chapter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipGroup {
    pub files: Vec<PathBuf>,
    pub kind: ClipGroupKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RelatedFolderKey {
    directory: PathBuf,
    modified: Option<(SystemTime, SystemTime)>,
}

#[derive(Clone)]
struct RelatedScan {
    take_label: Option<String>,
    related: Vec<Related>,
}

struct RelationScanRequest {
    id: u64,
    path: PathBuf,
    media: MediaKey,
    generation: u64,
    cancel: CancelToken,
    cache: Arc<Mutex<LruCache<RelatedFolderKey, HashMap<PathBuf, RelatedScan>>>>,
    tx: Sender<VideoMessage>,
    repaint: egui::Context,
}

#[derive(Default)]
struct RelatedScanState {
    pending: Option<RelationScanRequest>,
    running: Option<(u64, CancelToken)>,
    shutdown: bool,
}

struct RelatedScanWorker {
    state: Mutex<RelatedScanState>,
    ready: Condvar,
}

static RELATED_SCAN_WORKER: OnceLock<Option<Arc<RelatedScanWorker>>> = OnceLock::new();
static NEXT_RELATED_SCAN_ID: AtomicU64 = AtomicU64::new(1);

impl RelatedScanWorker {
    fn new() -> Self {
        Self {
            state: Mutex::new(RelatedScanState::default()),
            ready: Condvar::new(),
        }
    }

    fn enqueue(&self, request: RelationScanRequest) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some((_, cancel)) = &state.running {
            cancel.cancel();
        }
        if let Some(pending) = state.pending.replace(request) {
            pending.cancel.cancel();
        }
        self.ready.notify_one();
    }

    #[cfg(test)]
    fn shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.shutdown = true;
        if let Some((_, cancel)) = &state.running {
            cancel.cancel();
        }
        if let Some(pending) = state.pending.take() {
            pending.cancel.cancel();
        }
        self.ready.notify_one();
    }
}

fn related_scan_worker() -> Option<Arc<RelatedScanWorker>> {
    RELATED_SCAN_WORKER
        .get_or_init(|| {
            let worker = Arc::new(RelatedScanWorker::new());
            let thread_worker = worker.clone();
            std::thread::Builder::new()
                .name("video-related-scan".into())
                .spawn(move || {
                    run_related_scan_worker(thread_worker, |request| {
                        related_scan_cached(&request.path, &request.cache, &request.cancel)
                    });
                })
                .ok()?;
            Some(worker)
        })
        .clone()
}

fn run_related_scan_worker<F>(worker: Arc<RelatedScanWorker>, scan: F)
where
    F: Fn(&RelationScanRequest) -> Option<RelatedScan> + Send + 'static,
{
    loop {
        let request = {
            let mut state = worker
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            loop {
                if state.shutdown {
                    return;
                }
                if let Some(request) = state.pending.take() {
                    state.running = Some((request.id, request.cancel.clone()));
                    break request;
                }
                state = worker
                    .ready
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
        };

        let result = panic::catch_unwind(AssertUnwindSafe(|| scan(&request)))
            .unwrap_or_else(|_| Some(RelatedScan {
                take_label: None,
                related: Vec::new(),
            }));
        if !request.cancel.is_cancelled()
            && let Some(result) = result
        {
            let _ = request.tx.send(VideoMessage::Relations {
                media: request.media,
                generation: request.generation,
                take_label: result.take_label,
                related: result.related,
            });
            request.repaint.request_repaint();
        }

        let mut state = worker
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state
            .running
            .as_ref()
            .is_some_and(|(id, _)| *id == request.id)
        {
            state.running = None;
        }
    }
}

/// State for preview extraction. GPU textures and frame data are cached by file path,
/// modification time, and requested time; only the selected file's work is allowed to run.
const ONLINE_ONLY_VIDEO: &str = "Online-only file: open it to download it from the cloud.";

/// Checked inside each worker, just before ffprobe/ffmpeg reads the clip (the listing flag can
/// be stale by then). The returned handle is the guard: hold it while ffmpeg runs. It was
/// checked on the open handle, and while it is open the sync client cannot turn the clip back
/// into a placeholder, so ffmpeg's reads never download it.
fn open_checked(path: &Path) -> Result<std::fs::File, String> {
    use forge_core::cloud::{BackgroundOpenError, open_for_background_read};
    open_for_background_read(path).map_err(|error| match error {
        BackgroundOpenError::OnlineOnly => ONLINE_ONLY_VIDEO.into(),
        BackgroundOpenError::Missing => format!("{} no longer exists", path.display()),
        BackgroundOpenError::Io(error) => format!("{}: {error}", path.display()),
    })
}

pub struct VideoUi {
    tx: Sender<VideoMessage>,
    rx: Receiver<VideoMessage>,
    selection: Option<MediaKey>,
    generation: u64,
    generation_cancel: CancelToken,
    ffmpeg_dir: Option<PathBuf>,
    ffmpeg_available: bool,
    ffprobe_available: bool,
    missing_tools: Option<String>,
    probe_pending: bool,
    strip_pending: bool,
    probe_cache: LruCache<MediaKey, MediaResult<VideoMetadata>>,
    strip_cache: LruCache<MediaKey, Vec<FrameKey>>,
    frame_cache: LruCache<FrameKey, TextureHandle>,
    frame_pending: HashSet<FrameKey>,
    frame_errors: HashSet<FrameKey>,
    hover_key: Option<FrameKey>,
    hover_cancel: Option<CancelToken>,
    probe_error: Option<String>,
    strip_error: Option<String>,
    scrub_time: f64,
    exact_scrub_time: bool,
    relation_key: Option<MediaKey>,
    relation_pending: bool,
    relation_scan_cancel: Option<CancelToken>,
    relation_cache: Arc<Mutex<LruCache<RelatedFolderKey, HashMap<PathBuf, RelatedScan>>>>,
    take_label: Option<String>,
    related: Vec<Related>,
    proxy_scan_request: Option<ProxyScanRequest>,
    proxy_scan_id: u64,
    proxy_scan_cancel: Option<CancelToken>,
    proxy_scan_pending: bool,
    proxy_scan_error: Option<String>,
    proxy_scan_truncated: bool,
    located_proxies: Vec<Related>,
    proxy_dialog: Option<ProxyDialog>,
}

impl Default for VideoUi {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self {
            tx,
            rx,
            selection: None,
            generation: 0,
            generation_cancel: CancelToken::new(),
            ffmpeg_dir: None,
            ffmpeg_available: false,
            ffprobe_available: false,
            missing_tools: None,
            probe_pending: false,
            strip_pending: false,
            probe_cache: LruCache::new(NonZeroUsize::new(STRIP_CACHE_CAPACITY).expect("non-zero")),
            strip_cache: LruCache::new(NonZeroUsize::new(STRIP_CACHE_CAPACITY).expect("non-zero")),
            frame_cache: LruCache::new(NonZeroUsize::new(FRAME_CACHE_CAPACITY).expect("non-zero")),
            frame_pending: HashSet::new(),
            frame_errors: HashSet::new(),
            hover_key: None,
            hover_cancel: None,
            probe_error: None,
            strip_error: None,
            scrub_time: 0.0,
            exact_scrub_time: false,
            relation_key: None,
            relation_pending: false,
            relation_scan_cancel: None,
            relation_cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(RELATED_FOLDER_CACHE_CAPACITY).expect("non-zero"),
            ))),
            take_label: None,
            related: Vec::new(),
            proxy_scan_request: None,
            proxy_scan_id: 0,
            proxy_scan_cancel: None,
            proxy_scan_pending: false,
            proxy_scan_error: None,
            proxy_scan_truncated: false,
            located_proxies: Vec::new(),
            proxy_dialog: None,
        }
    }
}

impl VideoUi {
    /// Whether the selected clip cannot be previewed because FFmpeg tools are unavailable.
    pub fn tools_unavailable(&self) -> bool {
        self.missing_tools.is_some()
    }

    fn sync_selection(
        &mut self,
        entry: Option<&FileEntry>,
        ffmpeg_dir: Option<&Path>,
        ctx: &egui::Context,
    ) {
        let media = entry
            .filter(|e| e.kind == EntryKind::File && e.media_kind() == MediaKind::Video)
            .map(MediaKey::from_entry);
        // The listing's flag, no syscall: this runs on the UI thread. Each worker checks the
        // file itself again before ffmpeg opens it.
        let listed_online_only =
            entry.is_some_and(|e| !forge_core::cloud::background_read_allowed_entry(e));
        let ffmpeg_dir = ffmpeg_dir.map(Path::to_path_buf);
        if self.selection == media && self.ffmpeg_dir == ffmpeg_dir {
            return;
        }

        self.generation_cancel.cancel();
        if let Some(cancel) = self.relation_scan_cancel.take() {
            cancel.cancel();
        }
        if let Some(cancel) = &self.hover_cancel {
            cancel.cancel();
        }
        self.generation = self.generation.wrapping_add(1);
        self.generation_cancel = CancelToken::new();
        self.selection = media.clone();
        self.ffmpeg_dir = ffmpeg_dir.clone();
        self.probe_pending = false;
        self.strip_pending = false;
        self.frame_pending.clear();
        self.frame_errors.clear();
        self.hover_key = None;
        self.hover_cancel = None;
        self.probe_error = None;
        self.strip_error = None;
        self.missing_tools = None;
        self.scrub_time = 0.0;
        self.exact_scrub_time = false;
        self.relation_key = None;
        self.relation_pending = false;
        self.take_label = None;
        self.related.clear();
        // Located proxies belong to the clips that were selected when the scan was requested.
        self.cancel_proxy_scan();

        let Some(media) = media else { return };
        // A new FFmpeg location may repair a previous missing-tool probe for this file.
        let cached_tool_error = matches!(
            self.probe_cache.peek(&media),
            Some(Err(error)) if error.contains("not installed") || error.contains("not configured")
        );
        if cached_tool_error {
            self.probe_cache.pop(&media);
        }
        let tools = FfTools::discover_with(ffmpeg_dir.as_deref());
        self.ffmpeg_available = tools.has_ffmpeg();
        self.ffprobe_available = tools.has_ffprobe();
        let missing: Vec<&str> = [
            (!self.ffmpeg_available).then_some("ffmpeg"),
            (!self.ffprobe_available).then_some("ffprobe"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !missing.is_empty() {
            self.missing_tools = Some(missing.join(" and "));
        }
        // Selecting is not opening: probing or extracting frames would download an online-only
        // clip. The flags also stop hover and scrub frames for this selection.
        if listed_online_only {
            self.ffmpeg_available = false;
            self.ffprobe_available = false;
            self.probe_error = Some(ONLINE_ONLY_VIDEO.into());
            self.strip_error = Some(ONLINE_ONLY_VIDEO.into());
            return;
        }

        if let Some(result) = self.probe_cache.get(&media).cloned() {
            self.probe_error = result.err();
        } else if !self.ffprobe_available {
            self.probe_error = Some("ffprobe is not available".into());
        } else {
            self.probe_pending = true;
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            let path = media.path.clone();
            let media_key = media.clone();
            let generation = self.generation;
            std::thread::spawn(move || {
                let result = open_checked(&path).and_then(|_guard| {
                    FfTools::discover_with(ffmpeg_dir.as_deref())
                        .probe(&path)
                        .map_err(|e| e.to_string())
                });
                let _ = tx.send(VideoMessage::Probe {
                    media: media_key,
                    generation,
                    result: Box::new(result),
                });
                ctx.request_repaint();
            });
        }

        if !self.ffmpeg_available || !self.ffprobe_available {
            self.strip_error = Some(format!(
                "Frame preview needs {}.",
                self.missing_tools.as_deref().unwrap_or("FFmpeg")
            ));
            return;
        }
        let cached_strip_is_ready = self.strip_cache.peek(&media).is_some_and(|keys| {
            !keys.is_empty() && keys.iter().all(|key| self.frame_cache.peek(key).is_some())
        });
        if !cached_strip_is_ready {
            self.start_filmstrip(media, ctx);
        }
    }

    fn start_filmstrip(&mut self, media: MediaKey, ctx: &egui::Context) {
        self.strip_pending = true;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let cancel = self.generation_cancel.child();
        let path = media.path.clone();
        let media_key = media.clone();
        let ffmpeg_dir = self.ffmpeg_dir.clone();
        let generation = self.generation;
        std::thread::spawn(move || {
            let result = open_checked(&path).and_then(|_guard| {
                FfTools::discover_with(ffmpeg_dir.as_deref())
                    .filmstrip(&path, FILMSTRIP_FRAMES, FILMSTRIP_FRAME_EDGE, Some(&cancel))
                    .map_err(|e| e.to_string())
            });
            let _ = tx.send(VideoMessage::Filmstrip {
                media: media_key,
                generation,
                result,
            });
            ctx.request_repaint();
        });
    }

    fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(message) = self.rx.try_recv() {
            match message {
                VideoMessage::Probe {
                    media,
                    generation,
                    result,
                } => {
                    let result = *result;
                    // Not cached: the clip is probed once it has been downloaded.
                    if result.as_ref().err().map(String::as_str) != Some(ONLINE_ONLY_VIDEO) {
                        self.probe_cache.put(media.clone(), result.clone());
                    }
                    if self.selection.as_ref() == Some(&media) && self.generation == generation {
                        self.probe_pending = false;
                        self.probe_error = result.err();
                    }
                }
                VideoMessage::Filmstrip {
                    media,
                    generation,
                    result,
                } => {
                    if let Ok(frames) = &result {
                        let mut keys = Vec::with_capacity(frames.len());
                        for frame in frames {
                            let key = FrameKey::at(&media, frame.at_seconds);
                            keys.push(key.clone());
                            self.insert_texture(ctx, key, &frame.image);
                        }
                        self.strip_cache.put(media.clone(), keys);
                    }
                    if self.selection.as_ref() == Some(&media) && self.generation == generation {
                        self.strip_pending = false;
                        self.strip_error = result.err();
                    }
                }
                VideoMessage::Frame {
                    key,
                    generation,
                    result,
                } => {
                    self.frame_pending.remove(&key);
                    if let Ok(image) = &result {
                        self.insert_texture(ctx, key.clone(), image);
                    } else if self.generation == generation
                        && self.selection.as_ref() == Some(&key.media)
                        && let Err(error) = result
                        && error != "operation cancelled"
                    {
                        self.frame_errors.insert(key.clone());
                    }
                    if self.hover_key.as_ref() == Some(&key) {
                        self.hover_key = None;
                        self.hover_cancel = None;
                    }
                }
                VideoMessage::Estimate {
                    dialog_id,
                    generation,
                    result,
                } => {
                    if let Some(dialog) = self.proxy_dialog.as_mut()
                        && dialog.id == dialog_id
                        && dialog.estimate_generation == generation
                    {
                        dialog.estimate_pending = false;
                        match result {
                            Ok(estimate) => {
                                dialog.estimate = Some(estimate);
                                dialog.estimate_error = None;
                            }
                            Err(error) => {
                                dialog.estimate = None;
                                dialog.estimate_error = Some(error);
                            }
                        }
                    }
                }
                VideoMessage::Relations {
                    media,
                    generation,
                    take_label,
                    related,
                } => {
                    if self.selection.as_ref() == Some(&media)
                        && self.generation == generation
                        && self.relation_key.as_ref() == Some(&media)
                    {
                        self.relation_pending = false;
                        self.relation_scan_cancel = None;
                        self.take_label = take_label;
                        self.related = related;
                    }
                }
                VideoMessage::ProxyScan { scan_id, result } if scan_id == self.proxy_scan_id => {
                    self.proxy_scan_pending = false;
                    self.proxy_scan_cancel = None;
                    match result {
                        Ok(results) => {
                            self.located_proxies = results.related;
                            self.proxy_scan_truncated = results.truncated;
                            self.proxy_scan_error = None;
                        }
                        Err(error) => {
                            self.located_proxies.clear();
                            self.proxy_scan_truncated = false;
                            self.proxy_scan_error = Some(error);
                        }
                    }
                }
                VideoMessage::ProxyScan { .. } => {}
            }
        }
    }

    fn start_proxy_scan_if_requested(&mut self, ctx: &egui::Context) {
        let Some(request) = self.proxy_scan_request.take() else {
            return;
        };
        if let Some(cancel) = &self.proxy_scan_cancel {
            cancel.cancel();
        }
        self.proxy_scan_id = self.proxy_scan_id.wrapping_add(1);
        let scan_id = self.proxy_scan_id;
        let cancel = CancelToken::new();
        self.proxy_scan_cancel = Some(cancel.clone());
        self.proxy_scan_pending = true;
        self.proxy_scan_error = None;
        self.proxy_scan_truncated = false;
        self.located_proxies.clear();

        let tx = self.tx.clone();
        let repaint = ctx.clone();
        let spawn = std::thread::Builder::new()
            .name("video-proxy-locate".into())
            .spawn(move || {
                let result = locate_proxy_tree(&request.root, &request.sources, &cancel);
                let _ = tx.send(VideoMessage::ProxyScan { scan_id, result });
                repaint.request_repaint();
            });
        if let Err(error) = spawn {
            self.proxy_scan_pending = false;
            self.proxy_scan_cancel = None;
            self.proxy_scan_error = Some(format!("Could not start proxy scan: {error}"));
        }
    }

    fn cancel_proxy_scan(&mut self) {
        if let Some(cancel) = &self.proxy_scan_cancel {
            cancel.cancel();
        }
        self.proxy_scan_id = self.proxy_scan_id.wrapping_add(1);
        self.proxy_scan_request = None;
        self.proxy_scan_cancel = None;
        self.proxy_scan_pending = false;
        self.proxy_scan_error = None;
        self.proxy_scan_truncated = false;
        self.located_proxies.clear();
    }

    pub fn related_results(&self) -> &[Related] {
        &self.related
    }

    pub fn ensure_related_scan(
        &mut self,
        entry: &FileEntry,
        ffmpeg_dir: Option<&Path>,
        ctx: &egui::Context,
    ) {
        self.sync_selection(Some(entry), ffmpeg_dir, ctx);
        self.drain_messages(ctx);
        let media = MediaKey::from_entry(entry);
        if self.relation_key.as_ref() != Some(&media) {
            self.request_relation_scan(entry, ctx);
        }
    }

    pub fn relation_scan_pending(&self) -> bool {
        self.relation_pending
    }

    pub fn located_proxy_results(&self) -> &[Related] {
        &self.located_proxies
    }

    pub fn proxy_scan_pending(&self) -> bool {
        self.proxy_scan_pending || self.proxy_scan_request.is_some()
    }

    pub fn proxy_scan_error(&self) -> Option<&str> {
        self.proxy_scan_error.as_deref()
    }

    fn request_relation_scan(&mut self, entry: &FileEntry, ctx: &egui::Context) {
        let Some(media) = self.selection.clone() else {
            return;
        };
        if let Some(cancel) = self.relation_scan_cancel.take() {
            cancel.cancel();
        }
        let generation = self.generation;
        let path = entry.path.clone();
        let cache = self.relation_cache.clone();
        self.relation_key = Some(media.clone());
        self.relation_pending = true;
        let Some(worker) = related_scan_worker() else {
            self.relation_pending = false;
            return;
        };
        let cancel = CancelToken::new();
        self.relation_scan_cancel = Some(cancel.clone());
        worker.enqueue(RelationScanRequest {
            id: NEXT_RELATED_SCAN_ID.fetch_add(1, Ordering::Relaxed),
            path,
            media,
            generation,
            cancel,
            cache,
            tx: self.tx.clone(),
            repaint: ctx.clone(),
        });
    }

    fn insert_texture(&mut self, ctx: &egui::Context, key: FrameKey, image: &RgbaImage) {
        static NEXT_TEXTURE: AtomicU64 = AtomicU64::new(0);
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            image.as_raw(),
        );
        let id = NEXT_TEXTURE.fetch_add(1, Ordering::Relaxed);
        let texture = ctx.load_texture(
            format!("video-frame-{id}-{}ms", key.time_ms),
            color_image,
            egui::TextureOptions::LINEAR,
        );
        self.frame_cache.put(key, texture);
    }

    fn request_frame(&mut self, key: FrameKey, seconds: f64, ctx: &egui::Context) {
        if self.frame_cache.peek(&key).is_some()
            || self.frame_pending.contains(&key)
            || self.frame_errors.contains(&key)
            || !self.ffmpeg_available
            || !self.ffprobe_available
        {
            return;
        }
        if let Some(old_key) = self.hover_key.take() {
            if let Some(cancel) = self.hover_cancel.take() {
                cancel.cancel();
            }
            self.frame_pending.remove(&old_key);
        }
        let media = key.media.clone();
        let path = media.path.clone();
        let ffmpeg_dir = self.ffmpeg_dir.clone();
        let generation = self.generation;
        let token = self.generation_cancel.child();
        self.hover_key = Some(key.clone());
        self.hover_cancel = Some(token.clone());
        self.frame_pending.insert(key.clone());
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = open_checked(&path).and_then(|_guard| {
                FfTools::discover_with(ffmpeg_dir.as_deref())
                    .extract_frame_cancellable(&path, seconds, SCRUB_FRAME_EDGE, Some(&token))
                    .map_err(|e| e.to_string())
            });
            let _ = tx.send(VideoMessage::Frame {
                key,
                generation,
                result,
            });
            ctx.request_repaint();
        });
    }

    fn frame_for_time(
        &mut self,
        time: f64,
        duration: f64,
        exact: bool,
        ctx: &egui::Context,
    ) -> (FrameKey, f64) {
        let media = self
            .selection
            .as_ref()
            .expect("video selection is active")
            .clone();
        let bounded = time.clamp(0.0, duration.max(0.0));
        let request_time = if exact || duration <= 0.0 {
            bounded
        } else {
            let step = (bounded / duration * 100.0).round().clamp(0.0, 100.0);
            duration * step / 100.0
        };
        let key = FrameKey::at(&media, request_time);
        self.request_frame(key.clone(), request_time, ctx);
        (key, request_time)
    }

    fn nearest_cached_frame(&mut self, media: &MediaKey, time: f64) -> Option<TextureHandle> {
        let keys = self.strip_cache.get(media)?.clone();
        let nearest = keys.into_iter().min_by(|a, b| {
            (a.seconds() - time)
                .abs()
                .total_cmp(&(b.seconds() - time).abs())
        })?;
        self.frame_cache.get(&nearest).cloned()
    }

    fn draw(
        &mut self,
        ui: &mut Ui,
        entry: &FileEntry,
        download: crate::ffmpeg_setup::Status,
    ) -> Vec<VideoAction> {
        let media = self
            .selection
            .as_ref()
            .expect("preview selection synchronized")
            .clone();
        let mut actions = Vec::new();
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.heading("Video");
            if ui.button("Open in default player").clicked() {
                actions.push(VideoAction::OpenPlayer(entry.path.clone()));
            }
        });
        ui.label(
            RichText::new("Frame preview and seeking only; video playback is out of scope.")
                .small()
                .weak(),
        );

        if let Some(label) = &self.take_label {
            ui.label(
                RichText::new(label)
                    .strong()
                    .color(crate::theme::accent(ui.ctx())),
            );
        }
        if self.relation_pending {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Scanning related files…");
            });
        }
        draw_related_section(ui, self, &mut actions);

        if let Some(missing) = &self.missing_tools {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(RichText::new("Video tools are not available").strong());
                ui.label(format!(
                    "Frame previews and proxy generation need {missing}."
                ));
                ui.label(
                    "Enable FFmpeg under Settings › Video by choosing its folder or using PATH.",
                );
                crate::ffmpeg_setup::offer(ui, download, || {
                    actions.push(VideoAction::DownloadFfmpeg)
                });
            });
        }

        let metadata = self.probe_cache.get(&media).cloned().and_then(Result::ok);
        if self.probe_pending && metadata.is_none() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading video timing…");
            });
        } else if let Some(error) = self.probe_error.as_deref()
            && self.missing_tools.is_none()
        {
            ui.label(
                RichText::new(format!("Video timing unavailable: {error}"))
                    .small()
                    .weak(),
            );
        }

        let duration = metadata
            .as_ref()
            .and_then(|m| m.duration_s)
            .filter(|d| d.is_finite() && *d > 0.0);
        let fps = metadata
            .as_ref()
            .and_then(|m| m.fps)
            .filter(|f| f.is_finite() && *f > 0.0);
        let mut hover_time = None;
        if let Some(duration) = duration {
            let mut time = self.scrub_time.clamp(0.0, duration);
            let mut changed_time = false;
            let mut frame_step = false;
            let fps_step = fps.map(|f| 1.0 / f);
            let slider_response = ui
                .horizontal(|ui| {
                    if ui
                        .add_enabled(fps_step.is_some(), egui::Button::new("‹ frame"))
                        .clicked()
                    {
                        time = (time - fps_step.unwrap_or(0.0)).max(0.0);
                        changed_time = true;
                        frame_step = true;
                    }
                    let response = ui.add(
                        egui::Slider::new(&mut time, 0.0..=duration)
                            .show_value(false)
                            .step_by(fps_step.unwrap_or(0.001)),
                    );
                    changed_time |= response.changed();
                    if response.hovered()
                        && let Some(pointer) = ui.input(|i| i.pointer.hover_pos())
                    {
                        let width = response.rect.width().max(1.0);
                        let t = ((pointer.x - response.rect.left()) / width).clamp(0.0, 1.0) as f64;
                        hover_time = Some(t * duration);
                    }
                    if ui
                        .add_enabled(fps_step.is_some(), egui::Button::new("frame ›"))
                        .clicked()
                    {
                        time = (time + fps_step.unwrap_or(0.0)).min(duration);
                        changed_time = true;
                        frame_step = true;
                    }
                    response
                })
                .inner;
            if changed_time {
                self.scrub_time = time;
                self.exact_scrub_time = frame_step;
            }
            if slider_response.hovered() && hover_time.is_some() {
                self.exact_scrub_time = false;
            }
            let display_time = hover_time.unwrap_or(self.scrub_time).clamp(0.0, duration);
            let exact = hover_time.is_none() && self.exact_scrub_time;
            let (key, request_time) = self.frame_for_time(display_time, duration, exact, ui.ctx());
            let tc = metadata
                .as_ref()
                .and_then(|m| m.timecode.as_deref())
                .and_then(|start| format_timecode_at(start, display_time, fps.unwrap_or(30.0)))
                .unwrap_or_else(|| format_elapsed(display_time));
            ui.horizontal(|ui| {
                ui.label(RichText::new(tc).monospace());
                ui.label(RichText::new(format!("/ {}", format_elapsed(duration))).weak());
                if let Some(rate) = fps {
                    ui.label(RichText::new(format!("{rate:.3} fps")).small().weak());
                }
            });

            let main_texture = self
                .frame_cache
                .get(&key)
                .cloned()
                .or_else(|| self.nearest_cached_frame(&media, request_time));
            if let Some(texture) = main_texture {
                let source = texture.size_vec2();
                let max = egui::vec2(ui.available_width().max(1.0), 180.0);
                let scale = (max.x / source.x.max(1.0))
                    .min(max.y / source.y.max(1.0))
                    .min(1.0);
                let size = source * scale;
                ui.vertical_centered(|ui| {
                    ui.add(egui::Image::new((texture.id(), size)).fit_to_exact_size(size));
                });
            } else if self.frame_pending.contains(&key) || self.strip_pending {
                ui.vertical_centered(|ui| ui.spinner());
            } else if self.frame_errors.contains(&key) {
                ui.label(
                    RichText::new("No frame could be decoded at this position.")
                        .small()
                        .weak(),
                );
            }
        } else {
            ui.label(
                RichText::new("Duration is not available for scrubbing.")
                    .small()
                    .weak(),
            );
        }

        if self.strip_pending && self.strip_cache.peek(&media).is_none() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Building filmstrip…");
            });
        } else if let Some(error) = self.strip_error.as_deref()
            && self.missing_tools.is_none()
        {
            ui.label(
                RichText::new(format!("Filmstrip unavailable: {error}"))
                    .small()
                    .weak(),
            );
        }

        if let Some(keys) = self.strip_cache.get(&media).cloned() {
            egui::ScrollArea::horizontal()
                .id_salt("video-filmstrip")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for key in keys {
                            let Some(texture) = self.frame_cache.get(&key).cloned() else {
                                continue;
                            };
                            let source = texture.size_vec2();
                            let scale = (76.0 / source.x.max(1.0)).min(50.0 / source.y.max(1.0));
                            let size = source * scale;
                            let response = ui.add(
                                egui::Image::new((texture.id(), size))
                                    .sense(Sense::click())
                                    .fit_to_exact_size(size),
                            );
                            if response.clicked() {
                                self.scrub_time = key.seconds();
                                self.exact_scrub_time = true;
                            }
                        }
                    });
                });
        }

        actions
    }
}

fn draw_related_section(ui: &mut Ui, video: &VideoUi, actions: &mut Vec<VideoAction>) {
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Related files").strong());
        if ui.button("Locate proxies…").clicked() {
            actions.push(VideoAction::LocateProxies);
        }
    });
    if video.proxy_scan_pending {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Searching the selected folder tree for proxies…");
            if ui.small_button("Cancel").clicked() {
                actions.push(VideoAction::CancelProxyScan);
            }
        });
    }
    if let Some(error) = &video.proxy_scan_error {
        ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
    }
    let (rows, hidden_count) = visible_related_rows(&video.related, &video.located_proxies);
    for related in rows {
        let name = related
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| related.path.display().to_string());
        // Buttons first, from the right, so the label gets exactly the width that is left
        // and truncates; a fixed reservation overflowed and pushed the whole panel left.
        let row = egui::Layout::right_to_left(egui::Align::Center);
        let size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
        let row = ui.allocate_ui_with_layout(size, row, |ui| {
            if ui.small_button("Select in pane").clicked() {
                actions.push(VideoAction::SelectRelated(related.path.clone()));
            }
            if ui.small_button("Reveal").clicked() {
                actions.push(VideoAction::RevealRelated(related.path.clone()));
            }
            if ui.small_button("Open").clicked() {
                actions.push(VideoAction::OpenRelated(related.path.clone()));
            }
            let label = related_display_label(&related.label);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new(format!("{label}  ·  {name}")).small()).truncate())
                    .on_hover_text(&related.label);
            });
        });
        crate::testhook::note_entry_rect(
            ui,
            &related_row_test_key(&related.path),
            row.response.rect,
        );
    }
    if hidden_count > 0 {
        ui.label(
            RichText::new(format!("{hidden_count} more related files not shown"))
                .small()
                .weak(),
        );
    }
    if video.proxy_scan_truncated {
        ui.label(
            RichText::new("Proxy scan reached its safety limit; some folders were skipped.")
                .small()
                .weak(),
        );
    }
}

fn visible_related_rows<'a>(
    related: &'a [Related],
    located: &'a [Related],
) -> (Vec<&'a Related>, usize) {
    let mut seen = HashSet::new();
    let mut rows = Vec::with_capacity(MAX_DRAWN_RELATED_ROWS);
    let mut hidden = 0;
    for item in related.iter().chain(located) {
        if !seen.insert(normalized_path(&item.path)) {
            continue;
        }
        if rows.len() < MAX_DRAWN_RELATED_ROWS {
            rows.push(item);
        } else {
            hidden += 1;
        }
    }
    (rows, hidden)
}

fn related_row_test_key(path: &Path) -> PathBuf {
    let mut key = path.as_os_str().to_os_string();
    key.push("::related-row");
    PathBuf::from(key)
}

fn related_display_label(label: &str) -> String {
    let (location, label) = if let Some(located) = label.strip_prefix("Located for ") {
        (
            "Located · ",
            located
                .split_once(": ")
                .map(|(_, label)| label)
                .unwrap_or(located),
        )
    } else {
        ("", label)
    };
    let label = match label {
        "GoPro low-resolution companion" => "GoPro LRV",
        "DJI low-resolution companion" => "DJI LRF",
        "Sony XAVC S proxy sub-clip" => "Sony sub-clip",
        "GoPro thumbnail companion" => "GoPro thumbnail",
        "Proxy / optimized media" => "Proxy",
        "Same-basename variant" => "Video variant",
        label => label,
    };
    format!("{location}{label}")
}

enum VideoAction {
    OpenPlayer(PathBuf),
    OpenRelated(PathBuf),
    RevealRelated(PathBuf),
    SelectRelated(PathBuf),
    LocateProxies,
    CancelProxyScan,
    DownloadFfmpeg,
}

/// Extra preview-panel content for a video file, drawn below generic information/metadata.
pub fn preview(app: &mut ZxrawApp, ui: &mut egui::Ui, entry: &FileEntry) {
    let ffmpeg_dir = app.settings.ffmpeg_dir.clone();
    app.ops
        .video_ui
        .sync_selection(Some(entry), ffmpeg_dir.as_deref(), ui.ctx());
    app.ops.video_ui.drain_messages(ui.ctx());

    let media = MediaKey::from_entry(entry);
    if app.ops.video_ui.relation_key.as_ref() != Some(&media) {
        app.ops.video_ui.request_relation_scan(entry, ui.ctx());
    }

    let download = app.ops.ffmpeg_setup.status();
    let actions = app.ops.video_ui.draw(ui, entry, download);
    for action in actions {
        match action {
            VideoAction::OpenPlayer(path) => {
                app.ops.spawn_shell("Open in default player", move || {
                    forge_win::open_default(&path)
                });
            }
            VideoAction::OpenRelated(path) => crate::actions::open_file(app, &path),
            VideoAction::RevealRelated(path) => {
                app.ops.spawn_shell("Reveal related file", move || {
                    forge_win::reveal_in_explorer(&path)
                })
            }
            VideoAction::SelectRelated(path) => app.navigate(path),
            VideoAction::LocateProxies => locate_proxies(app),
            VideoAction::CancelProxyScan => app.ops.video_ui.cancel_proxy_scan(),
            VideoAction::DownloadFfmpeg => crate::ffmpeg_setup::confirm(app),
        }
    }
}

/// Called every frame so selection changes cancel stale preview work and dialogs keep running.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    let selected = app
        .pane()
        .cursor_entry()
        .filter(|e| e.media_kind() == MediaKind::Video)
        .cloned();
    let ffmpeg_dir = app.settings.ffmpeg_dir.clone();
    app.ops
        .video_ui
        .sync_selection(selected.as_ref(), ffmpeg_dir.as_deref(), ctx);
    app.ops.video_ui.start_proxy_scan_if_requested(ctx);
    app.ops.video_ui.drain_messages(ctx);
    proxy_window(app, ctx);
}

/// Opens the proxy dialog for selected video files (or the cursor video if none is selected).
/// Move a finished temp file to `output` without ever replacing an existing file.
fn place_no_replace(temp: &Path, output: &Path) -> std::io::Result<()> {
    rename_no_replace(temp, output)
}

/// Pick a root folder, then locate matching proxies in a bounded background scan.
pub fn locate_proxies(app: &mut ZxrawApp) {
    let pane = app.pane();
    let clips = pane
        .entries
        .iter()
        .filter(|entry| is_clip_entry(entry))
        .map(|entry| entry.path.clone())
        .collect::<HashSet<_>>();
    let sources = pane
        .action_targets()
        .into_iter()
        .filter(|path| clips.contains(path))
        .collect::<Vec<_>>();
    if sources.is_empty() {
        app.toast("Select one or more video clips before locating proxies");
        return;
    }
    let dialog = rfd::FileDialog::new().set_directory(pane.path.clone());
    let Some(root) = crate::dialogs::pick_folder(dialog) else {
        return;
    };
    let omitted = sources.len().saturating_sub(MAX_PROXY_SCAN_SOURCES);
    let sources = sources.into_iter().take(MAX_PROXY_SCAN_SOURCES).collect();
    app.ops.video_ui.proxy_scan_request = Some(ProxyScanRequest { root, sources });
    app.ops.video_ui.proxy_scan_pending = true;
    app.ops.video_ui.proxy_scan_error = None;
    if omitted == 0 {
        app.toast("Proxy search queued");
    } else {
        app.toast(format!(
            "Proxy search queued for the first {MAX_PROXY_SCAN_SOURCES} clips; {omitted} selected clips were skipped"
        ));
    }
}

pub fn generate_proxies(app: &mut ZxrawApp) {
    // One pass over the listing, then a set lookup per target: a nested search was
    // O(selected × listed) on the UI thread.
    let pane = app.pane();
    let clips = pane
        .entries
        .iter()
        .filter(|entry| is_clip_entry(entry))
        .map(|entry| entry.path.clone())
        .collect::<HashSet<_>>();
    let sources: Vec<PathBuf> = pane
        .action_targets()
        .into_iter()
        .filter(|path| clips.contains(path))
        .collect();
    if sources.is_empty() {
        app.toast("Select one or more video files to generate proxies");
        return;
    }
    app.ops.video_ui.proxy_dialog = Some(ProxyDialog::new(sources));
}

// -----------------------------------------------------------------------------
// Proxy dialog and jobs
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyPreset {
    H2641080,
    H264720,
    ProResProxy,
}

impl ProxyPreset {
    fn label(self) -> &'static str {
        match self {
            Self::H2641080 => "1080p H.264 · CRF 23",
            Self::H264720 => "720p H.264 · CRF 23",
            Self::ProResProxy => "ProRes Proxy",
        }
    }

    fn bitrate(self) -> u64 {
        match self {
            Self::H2641080 => 4_000_000,
            Self::H264720 => 2_000_000,
            Self::ProResProxy => 45_000_000,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::ProResProxy => "mov",
            Self::H2641080 | Self::H264720 => "mp4",
        }
    }
}

struct ProxyDialog {
    id: u64,
    sources: Vec<PathBuf>,
    preset: ProxyPreset,
    destination: Option<PathBuf>,
    estimate: Option<ProxyEstimate>,
    estimate_error: Option<String>,
    estimate_generation: u64,
    estimate_pending: bool,
    confirming: bool,
    continue_without_space_check: bool,
}

static NEXT_DIALOG_ID: AtomicU64 = AtomicU64::new(1);

impl ProxyDialog {
    fn new(sources: Vec<PathBuf>) -> Self {
        Self {
            id: NEXT_DIALOG_ID.fetch_add(1, Ordering::Relaxed),
            sources,
            preset: ProxyPreset::H2641080,
            destination: None,
            estimate: None,
            estimate_error: None,
            estimate_generation: 0,
            estimate_pending: false,
            confirming: false,
            continue_without_space_check: false,
        }
    }

    fn destination_for(&self, source: &Path) -> PathBuf {
        self.destination.clone().unwrap_or_else(|| {
            source
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("Proxy")
        })
    }
}

#[derive(Debug, Clone)]
struct VolumeSpace {
    root: PathBuf,
    free_bytes: u64,
}

#[derive(Debug, Clone)]
struct ProxyEstimate {
    durations_s: Vec<Option<f64>>,
    duration_errors: Vec<Option<String>>,
    free_spaces: Vec<Option<VolumeSpace>>,
}

fn proxy_window(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(mut dialog) = app.ops.video_ui.proxy_dialog.take() else {
        return;
    };
    let mut open = true;
    let mut start = false;
    let mut destination_changed = false;
    let download = app.ops.ffmpeg_setup.status();
    let mut download_clicked = false;
    let ff = FfTools::discover_with(app.settings.ffmpeg_dir.as_deref());
    if let Some(exe) = ff.ffmpeg.as_deref() {
        ensure_encoder_probe(exe, ctx);
    }
    let encoder_result = ff.ffmpeg.as_deref().and_then(cached_encoders);
    let encoders_ready = encoder_result.is_some();
    let encoders = encoder_result
        .as_ref()
        .and_then(|result| result.as_ref().ok());
    let proxy_encoder_available = encoders.is_some_and(|e| e.contains("prores_ks"));
    let h264_available = encoders.is_some_and(|e| e.contains("libx264"));

    egui::Window::new("Generate video proxies")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.label(format!("{} selected video file(s)", dialog.sources.len()));
            ui.add_space(6.0);

            ui.label(RichText::new("Preset").strong());
            for preset in [ProxyPreset::H2641080, ProxyPreset::H264720, ProxyPreset::ProResProxy] {
                let available = match preset {
                    ProxyPreset::H2641080 | ProxyPreset::H264720 => h264_available,
                    ProxyPreset::ProResProxy => proxy_encoder_available,
                };
                let response = ui.add_enabled(
                    available,
                    egui::RadioButton::new(dialog.preset == preset, preset.label()),
                );
                if response.clicked() {
                    dialog.preset = preset;
                    dialog.confirming = false;
                }
            }
            if !encoders_ready && ff.ffmpeg.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Checking FFmpeg encoders…").small().weak());
                });
            } else if let Some(Err(error)) = &encoder_result {
                ui.label(RichText::new(format!("Could not read FFmpeg encoders: {error}")).small().weak());
            } else if encoders_ready && !h264_available {
                ui.label(RichText::new("This FFmpeg build does not include libx264.").small().weak());
            }

            ui.add_space(6.0);
            ui.label(RichText::new("Destination").strong());
            ui.horizontal(|ui| {
                let shown = dialog
                    .destination
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "Proxy folder next to each source".into());
                ui.label(RichText::new(shown).small());
                if ui.button("Choose folder…").clicked() {
                    let picker = rfd::FileDialog::new();
                    if let Some(path) = crate::dialogs::pick_folder(picker) {
                        dialog.destination = Some(path);
                        destination_changed = true;
                    }
                }
                if dialog.destination.is_some() && ui.button("Use source folders").clicked() {
                    dialog.destination = None;
                    destination_changed = true;
                }
            });

            if !ff.has_ffmpeg() || !ff.has_ffprobe() {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(RichText::new("FFmpeg is not ready").strong());
                    let mut missing = Vec::new();
                    if !ff.has_ffmpeg() {
                        missing.push("ffmpeg");
                    }
                    if !ff.has_ffprobe() {
                        missing.push("ffprobe");
                    }
                    ui.label(format!("Missing {}. Enable the tools under Settings › Video.", missing.join(" and ")));
                    crate::ffmpeg_setup::offer(ui, download, || download_clicked = true);
                });
            }

            if destination_changed {
                dialog.estimate = None;
                dialog.estimate_error = None;
                dialog.estimate_pending = false;
                dialog.estimate_generation = dialog.estimate_generation.wrapping_add(1);
                dialog.confirming = false;
                dialog.continue_without_space_check = false;
            }
            if !dialog.estimate_pending && dialog.estimate.is_none() && dialog.estimate_error.is_none() {
                start_estimate(&mut dialog, &app.ops.video_ui.tx, app.settings.ffmpeg_dir.clone(), ctx);
            }

            ui.separator();
            ui.label(RichText::new("Estimate before running").strong());
            if dialog.estimate_pending {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Probing clips and checking destination space…");
                });
            } else if let Some(error) = &dialog.estimate_error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            } else if let Some(estimate) = &dialog.estimate {
                let total_duration: Option<f64> = estimate
                    .durations_s
                    .iter()
                    .copied()
                    .collect::<Option<Vec<_>>>()
                    .map(|values| values.iter().sum());
                let output_bytes = total_duration.map(|d| estimated_output_bytes(dialog.preset.bitrate(), d));
                let available_count = estimate.durations_s.iter().filter(|d| d.is_some()).count();
                if let Some(duration) = total_duration {
                    ui.label(format!("Total duration: {}", format_elapsed(duration)));
                    ui.label(format!("Approx. output: {}", format_size(output_bytes.unwrap_or(0))));
                } else {
                    ui.label(format!("Duration: {available_count}/{} clips probed", dialog.sources.len()));
                    for (path, error) in dialog.sources.iter().zip(&estimate.duration_errors) {
                        if let Some(error) = error {
                            ui.label(RichText::new(format!("{}: {error}", path.file_name().unwrap_or_default().to_string_lossy())).small().weak());
                        }
                    }
                }
                let space = proxy_space_sufficient(&dialog.sources, estimate, dialog.preset.bitrate());
                match space {
                    Some(true) => ui.label("Free space check: enough space reported on every target volume."),
                    Some(false) => ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "Free space check: the estimate is larger than available space on at least one target volume.",
                    ),
                    None => ui.label(RichText::new("Free space could not be checked for every destination volume.").small().weak()),
                };
            }

            ui.separator();
            if dialog.confirming {
                ui.label(format!(
                    "Confirm: generate {} {} proxy file(s). Originals will remain untouched; existing outputs will be skipped.",
                    dialog.sources.len(),
                    dialog.preset.label(),
                ));
                let space = dialog
                    .estimate
                    .as_ref()
                    .and_then(|estimate| proxy_space_sufficient(&dialog.sources, estimate, dialog.preset.bitrate()));
                if space != Some(true) {
                    ui.checkbox(
                        &mut dialog.continue_without_space_check,
                        "Continue even though space is low or could not be checked",
                    );
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            ff.has_ffmpeg()
                                && (dialog.preset != ProxyPreset::ProResProxy || proxy_encoder_available)
                                && dialog
                                    .estimate
                                    .as_ref()
                                    .is_some_and(|e| e.durations_s.iter().all(Option::is_some))
                                && (space == Some(true) || dialog.continue_without_space_check),
                            egui::Button::new("Generate proxies"),
                        )
                        .clicked()
                    {
                        start = true;
                    }
                    if ui.button("Back").clicked() {
                        dialog.confirming = false;
                    }
                });
            } else {
                let can_review = ff.has_ffmpeg()
                    && encoders_ready
                    && match dialog.preset {
                        ProxyPreset::H2641080 | ProxyPreset::H264720 => h264_available,
                        ProxyPreset::ProResProxy => proxy_encoder_available,
                    }
                    && dialog
                        .estimate
                        .as_ref()
                        .is_some_and(|e| e.durations_s.iter().all(Option::is_some));
                if ui
                    .add_enabled(can_review, egui::Button::new("Review generation…"))
                    .clicked()
                {
                    dialog.confirming = true;
                }
            }
        });

    if start {
        start_proxy_job(app, dialog);
    } else if open {
        app.ops.video_ui.proxy_dialog = Some(dialog);
    }
    if download_clicked {
        crate::ffmpeg_setup::confirm(app);
    }
}

fn start_estimate(
    dialog: &mut ProxyDialog,
    tx: &Sender<VideoMessage>,
    ffmpeg_dir: Option<PathBuf>,
    ctx: &egui::Context,
) {
    dialog.estimate_pending = true;
    let dialog_id = dialog.id;
    let generation = dialog.estimate_generation;
    let sources = dialog.sources.clone();
    let destinations = sources
        .iter()
        .map(|p| dialog.destination_for(p))
        .collect::<Vec<_>>();
    let tx = tx.clone();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let ff = FfTools::discover_with(ffmpeg_dir.as_deref());
        let result = if !ff.has_ffprobe() {
            Err(
                "ffprobe is missing; configure it under Settings › Video to estimate proxy sizes"
                    .to_string(),
            )
        } else {
            let mut durations_s = Vec::with_capacity(sources.len());
            let mut duration_errors = Vec::with_capacity(sources.len());
            for path in &sources {
                // The estimate is a preview: an online-only clip is not downloaded to probe it.
                if !forge_core::cloud::background_read_allowed(path) {
                    durations_s.push(None);
                    duration_errors.push(Some("online-only, not probed".to_string()));
                    continue;
                }
                match ff.probe(path) {
                    Ok(meta) => {
                        let duration = meta.duration_s.filter(|d| d.is_finite() && *d > 0.0);
                        duration_errors.push(
                            duration
                                .is_none()
                                .then(|| "duration is unknown".to_string()),
                        );
                        durations_s.push(duration);
                    }
                    Err(error) => {
                        duration_errors.push(Some(error.to_string()));
                        durations_s.push(None);
                    }
                }
            }
            let drives = forge_win::list_drives();
            let free_spaces = destinations
                .iter()
                .map(|destination| free_space_for(destination, &drives))
                .collect();
            Ok(ProxyEstimate {
                durations_s,
                duration_errors,
                free_spaces,
            })
        };
        let _ = tx.send(VideoMessage::Estimate {
            dialog_id,
            generation,
            result,
        });
        ctx.request_repaint();
    });
}

fn start_proxy_job(app: &mut ZxrawApp, dialog: ProxyDialog) {
    let ff = FfTools::discover_with(app.settings.ffmpeg_dir.as_deref());
    let Some(ffmpeg) = ff.ffmpeg else {
        app.toast_err("ffmpeg is missing; configure it under Settings › Video");
        return;
    };
    let Some(estimate) = dialog.estimate else {
        app.toast_err("Proxy estimates are not ready");
        return;
    };
    if estimate.durations_s.iter().any(Option::is_none) {
        app.toast_err("Every selected video needs a duration estimate before proxy generation");
        return;
    }

    let sources = dialog.sources;
    let durations: Vec<f64> = estimate.durations_s.into_iter().flatten().collect();
    let custom_destination = dialog.destination;
    let preset = dialog.preset;
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    let title = format!("Generating {} proxy file(s)", sources.len());
    let changed_dirs = sources
        .iter()
        .flat_map(|source| {
            let target = output_dir_for(source, custom_destination.as_deref());
            [
                target,
                source
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf(),
            ]
        })
        .collect::<BTreeSet<_>>();

    app.ops
        .jobs
        .spawn("video-proxy", title.clone(), move |job| {
            job.progress.add_total_items(sources.len() as u64);
            let mut failures = Vec::new();
            let mut skipped = Vec::new();
            let mut completed = 0usize;
            for (index, (source, duration)) in sources.iter().zip(&durations).enumerate() {
                if job.cancel.is_cancelled() {
                    break;
                }
                let directory = output_dir_for(source, custom_destination.as_deref());
                let output = proxy_output_path(source, &directory, preset);
                let name = source.file_name().unwrap_or_default().to_string_lossy();
                job.progress
                    .set_current(format!("{} ({}/{})", name, index + 1, sources.len()));
                // The estimate refused online-only clips, but a clip can go online-only while
                // the job waits: ffmpeg would download it.
                // Held until ffmpeg is done with the clip: see `open_checked`.
                let _guard = match forge_core::cloud::open_for_background_read(source) {
                    Ok(file) => file,
                    Err(forge_core::cloud::BackgroundOpenError::OnlineOnly) => {
                        let line = format!("skipped {}: {ONLINE_ONLY_VIDEO}", source.display());
                        job.log(line.clone());
                        skipped.push(line);
                        job.progress.add_items(1);
                        continue;
                    }
                    Err(error) => {
                        let line = match error {
                            forge_core::cloud::BackgroundOpenError::Io(error) => {
                                format!("{}: {error}", source.display())
                            }
                            _ => format!("{}: the clip no longer exists", source.display()),
                        };
                        job.log(line.clone());
                        failures.push(line);
                        job.progress.add_error();
                        job.progress.add_items(1);
                        continue;
                    }
                };
                if let Err(error) = std::fs::create_dir_all(&directory) {
                    failures.push(format!(
                        "{}: could not create {}: {error}",
                        source.display(),
                        directory.display()
                    ));
                    job.log(failures.last().expect("just pushed").clone());
                    job.progress.add_error();
                    job.progress.add_items(1);
                    continue;
                }
                if forge_core::paths::exists_no_follow(&output) {
                    let line = format!("skipped {}: output already exists", output.display());
                    job.log(line.clone());
                    skipped.push(line);
                    job.progress.add_items(1);
                    continue;
                }
                let mut temp = match ProxyTemp::create(&directory, &output, job.id, index) {
                    Ok(temp) => temp,
                    Err(error) => {
                        let line = format!(
                            "{}: could not create private proxy temp: {error}",
                            source.display()
                        );
                        job.log(line.clone());
                        failures.push(line);
                        job.progress.add_error();
                        job.progress.add_items(1);
                        continue;
                    }
                };
                let args = build_proxy_args(source, temp.path(), preset);
                match run_proxy_child(&ffmpeg, &args, *duration, job, &name) {
                    Ok(()) if !temp.path().is_file() => {
                        let line = format!(
                            "{}: ffmpeg completed without creating a proxy file",
                            source.display()
                        );
                        job.log(line.clone());
                        failures.push(line);
                        job.progress.add_error();
                        job.progress.add_items(1);
                    }
                    Ok(()) => {
                        match place_no_replace(temp.path(), &output) {
                            Ok(()) => {
                                temp.disarm_file();
                                completed += 1;
                                job.log(format!("created {}", output.display()));
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                                let line = format!(
                                    "skipped {}: output appeared while encoding",
                                    output.display()
                                );
                                job.log(line.clone());
                                skipped.push(line);
                            }
                            Err(error) => {
                                let line = format!(
                                    "{}: could not finalize {}: {error}",
                                    source.display(),
                                    output.display()
                                );
                                job.log(line.clone());
                                failures.push(line);
                                job.progress.add_error();
                            }
                        }
                        job.progress.add_items(1);
                    }
                    Err(error) if error == "operation cancelled" || job.cancel.is_cancelled() => {
                        break;
                    }
                    Err(error) => {
                        let line = format!("{}: {error}", source.display());
                        job.log(line.clone());
                        failures.push(line);
                        job.progress.add_error();
                        job.progress.add_items(1);
                    }
                }
            }
            for directory in changed_dirs {
                let _ = tx.send(Event::Changed(directory));
            }
            if !failures.is_empty() {
                let mut lines = vec![format!(
                    "{completed} created, {} skipped, {} failed",
                    skipped.len(),
                    failures.len()
                )];
                lines.extend(failures);
                lines.extend(skipped);
                let _ = tx.send(Event::Report {
                    title: "Proxy generation completed with errors".into(),
                    lines,
                });
            } else if job.cancel.is_cancelled() {
                let _ = tx.send(Event::Toast(format!(
                    "{completed} proxy file(s) created; generation cancelled"
                )));
            } else {
                let _ = tx.send(Event::Toast(format!(
                    "{completed} proxy file(s) created, {} skipped",
                    skipped.len()
                )));
            }
            ctx.request_repaint();
            Ok(())
        });
    app.toast("Proxy job started; progress and file results are in Operations");
}

fn output_dir_for(source: &Path, custom_destination: Option<&Path>) -> PathBuf {
    custom_destination
        .map(Path::to_path_buf)
        .unwrap_or_else(|| {
            source
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("Proxy")
        })
}

fn proxy_output_path(source: &Path, directory: &Path, preset: ProxyPreset) -> PathBuf {
    let stem = source.file_stem().unwrap_or_default().to_string_lossy();
    directory.join(format!("{stem}_proxy.{}", preset.extension()))
}

struct ProxyTemp {
    directory: PathBuf,
    path: PathBuf,
    owns_file: bool,
}

impl ProxyTemp {
    fn create(destination: &Path, output: &Path, job_id: u64, index: usize) -> io::Result<Self> {
        loop {
            let directory = create_proxy_temp_dir(destination, job_id, index)?;
            match Self::reserve(&directory, output) {
                Ok(temp) => return Ok(temp),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn reserve(directory: &Path, output: &Path) -> io::Result<Self> {
        let filename = output.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "proxy output has no file name")
        })?;
        let path = directory.join(filename);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                drop(file);
                Ok(Self {
                    directory: directory.to_path_buf(),
                    path,
                    owns_file: true,
                })
            }
            Err(error) => {
                // This removes only an empty directory we created. If a foreign file is in it,
                // remove_dir fails and leaves that file untouched.
                let _ = fs::remove_dir(directory);
                Err(error)
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm_file(&mut self) {
        self.owns_file = false;
    }
}

impl Drop for ProxyTemp {
    fn drop(&mut self) {
        if self.owns_file {
            let _ = fs::remove_file(&self.path);
        }
        // Never recurse into the private directory; a non-empty directory may contain files
        // this job did not create.
        let _ = fs::remove_dir(&self.directory);
    }
}

fn create_proxy_temp_dir(destination: &Path, job_id: u64, index: usize) -> io::Result<PathBuf> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    loop {
        let unique = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let candidate = destination.join(format!(".zxraw-proxy-{job_id}-{index}-{unique}"));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn build_proxy_args(source: &Path, temp_output: &Path, preset: ProxyPreset) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-progress",
        "pipe:1",
        "-nostats",
        "-n",
        "-i",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(file_protocol_arg(source));
    args.extend([OsString::from("-map"), OsString::from("0:v:0")]);
    args.extend([OsString::from("-map"), OsString::from("0:a?")]);
    match preset {
        ProxyPreset::H2641080 | ProxyPreset::H264720 => {
            let (width, height) = if preset == ProxyPreset::H2641080 {
                (1920, 1080)
            } else {
                (1280, 720)
            };
            args.extend([
                OsString::from("-vf"),
                OsString::from(format!(
                    "scale=w='min({width},iw)':h='min({height},ih)':force_original_aspect_ratio=decrease"
                )),
                OsString::from("-c:v"),
                OsString::from("libx264"),
                OsString::from("-preset"),
                OsString::from("medium"),
                OsString::from("-crf"),
                OsString::from("23"),
                OsString::from("-pix_fmt"),
                OsString::from("yuv420p"),
                OsString::from("-c:a"),
                OsString::from("aac"),
                OsString::from("-b:a"),
                OsString::from("128k"),
                OsString::from("-movflags"),
                OsString::from("+faststart"),
                OsString::from("-f"),
                OsString::from("mp4"),
            ]);
        }
        ProxyPreset::ProResProxy => args.extend([
            OsString::from("-c:v"),
            OsString::from("prores_ks"),
            OsString::from("-profile:v"),
            OsString::from("0"),
            OsString::from("-pix_fmt"),
            OsString::from("yuv422p10le"),
            OsString::from("-c:a"),
            OsString::from("pcm_s16le"),
            OsString::from("-f"),
            OsString::from("mov"),
        ]),
    }
    args.push(file_protocol_arg(temp_output));
    args
}

fn file_protocol_arg(path: &Path) -> OsString {
    let mut value = OsString::from("file:");
    value.push(path.as_os_str());
    value
}

/// Kills `child` and waits until it has exited, so ffmpeg is gone (and no longer reading
/// the clip) before the caller returns and drops the clip's guard handle.
/// Returns the exit status, which exists only once the process has been reaped.
fn stop_child(child: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    let _ = child.kill();
    child.wait().ok()
}

fn run_proxy_child(
    exe: &Path,
    args: &[OsString],
    duration_s: f64,
    job: &JobHandle,
    name: &str,
) -> Result<(), String> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start ffmpeg: {e}"))?;
    let Some(stdout) = child.stdout.take() else {
        stop_child(&mut child);
        return Err("ffmpeg progress pipe was unavailable".into());
    };
    let Some(stderr) = child.stderr.take() else {
        stop_child(&mut child);
        return Err("ffmpeg error pipe was unavailable".into());
    };
    // Keep the last non-empty stderr line so failures say why FFmpeg stopped. The pipe is
    // drained to EOF even when the line is unused, so FFmpeg can never block on a full pipe.
    let error_reader = std::thread::spawn(move || {
        BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
            .filter(|line| !line.trim().is_empty())
            .last()
            .unwrap_or_default()
    });
    let (line_tx, line_rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::Builder::new()
        .name("proxy-progress-reader".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if line_tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .map_err(|e| {
            stop_child(&mut child);
            format!("could not start progress reader: {e}")
        })?;

    loop {
        while let Ok(line) = line_rx.try_recv() {
            if let Some(progress_us) = parse_progress_ms(&line) {
                let fraction = if duration_s.is_finite() && duration_s > 0.0 {
                    (progress_us as f64 / (duration_s * 1_000_000.0)).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                job.progress
                    .set_current(format!("{name} · {:.0}%", fraction * 100.0));
            }
        }
        if job.cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            let _ = error_reader.join();
            return Err("operation cancelled".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = reader.join();
                while let Ok(line) = line_rx.try_recv() {
                    if let Some(progress_us) = parse_progress_ms(&line) {
                        let fraction = if duration_s > 0.0 {
                            (progress_us as f64 / (duration_s * 1_000_000.0)).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        job.progress
                            .set_current(format!("{name} · {:.0}%", fraction * 100.0));
                    }
                }
                let detail = error_reader.join().unwrap_or_default();
                return if status.success() {
                    Ok(())
                } else if detail.is_empty() {
                    Err(format!("ffmpeg exited with {status}"))
                } else {
                    Err(format!("ffmpeg exited with {status}: {}", detail.trim()))
                };
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                let _ = error_reader.join();
                return Err(format!("could not poll ffmpeg: {error}"));
            }
        }
    }
}

/// Parses FFmpeg's `out_time_ms` progress field. FFmpeg reports this value in microseconds.
fn parse_progress_ms(line: &str) -> Option<u64> {
    let (name, value) = line.trim().split_once('=')?;
    (name.trim() == "out_time_ms")
        .then(|| value.trim().parse::<u64>().ok())
        .flatten()
}

fn estimated_output_bytes(target_bitrate_bps: u64, duration_s: f64) -> u64 {
    if !duration_s.is_finite() || duration_s <= 0.0 {
        return 0;
    }
    ((target_bitrate_bps as f64 * duration_s) / 8.0)
        .round()
        .clamp(0.0, u64::MAX as f64) as u64
}

/// Sums the estimated output per target volume and compares it with that volume's free space.
/// `None` means at least one source has no probed duration or no known volume.
fn proxy_space_sufficient(
    sources: &[PathBuf],
    estimate: &ProxyEstimate,
    bitrate: u64,
) -> Option<bool> {
    if estimate.durations_s.len() != sources.len() || estimate.free_spaces.len() != sources.len() {
        return None;
    }
    let mut volumes: HashMap<String, (u64, u64)> = HashMap::new();
    for index in 0..sources.len() {
        let duration = estimate.durations_s[index]?;
        let space = estimate.free_spaces[index].as_ref()?;
        let key = normalized_path(&space.root);
        let need = estimated_output_bytes(bitrate, duration);
        let entry = volumes.entry(key).or_insert((space.free_bytes, 0));
        entry.0 = entry.0.min(space.free_bytes);
        entry.1 = entry.1.saturating_add(need);
    }
    (!volumes.is_empty()).then(|| volumes.values().all(|(free, need)| need <= free))
}

fn free_space_for(path: &Path, drives: &[forge_win::DriveInfo]) -> Option<VolumeSpace> {
    drives
        .iter()
        .filter(|drive| drive.available)
        .filter_map(|drive| {
            drive
                .free_bytes
                .filter(|_| path_is_within(path, &drive.root))
                .map(|free_bytes| {
                    (
                        drive.root.components().count(),
                        VolumeSpace {
                            root: drive.root.clone(),
                            free_bytes,
                        },
                    )
                })
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, space)| space)
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = normalized_path(path);
    let root = normalized_path(root);
    path == root || path.strip_prefix(&root_with_separator(&root)).is_some()
}

fn normalized_path(path: &Path) -> String {
    let value = path.to_string_lossy().replace('/', "\\").to_lowercase();
    value.trim_end_matches('\\').to_string()
}

fn root_with_separator(root: &str) -> String {
    if root.ends_with('\\') {
        root.to_string()
    } else {
        format!("{root}\\")
    }
}

#[derive(Default)]
struct EncoderCache {
    results: HashMap<PathBuf, MediaResult<HashSet<String>>>,
    pending: HashSet<PathBuf>,
}

fn encoder_cache() -> &'static Mutex<EncoderCache> {
    static CACHE: OnceLock<Mutex<EncoderCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(EncoderCache::default()))
}

fn ensure_encoder_probe(exe: &Path, ctx: &egui::Context) {
    let exe = exe.to_path_buf();
    let should_start = {
        let mut cache = encoder_cache().lock().unwrap_or_else(|e| e.into_inner());
        if cache.results.contains_key(&exe) || cache.pending.contains(&exe) {
            false
        } else {
            cache.pending.insert(exe.clone());
            true
        }
    };
    if !should_start {
        return;
    }
    let ctx = ctx.clone();
    let probe_exe = exe.clone();
    let spawn = std::thread::Builder::new()
        .name("ffmpeg-encoder-probe".into())
        .spawn(move || {
            let result = probe_encoders(&probe_exe);
            let mut cache = encoder_cache().lock().unwrap_or_else(|e| e.into_inner());
            cache.pending.remove(&probe_exe);
            cache.results.insert(probe_exe, result);
            drop(cache);
            ctx.request_repaint();
        });
    if let Err(error) = spawn {
        let mut cache = encoder_cache().lock().unwrap_or_else(|e| e.into_inner());
        cache.pending.remove(&exe);
        cache
            .results
            .insert(exe, Err(format!("could not start encoder probe: {error}")));
    }
}

fn cached_encoders(exe: &Path) -> Option<MediaResult<HashSet<String>>> {
    encoder_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .results
        .get(exe)
        .cloned()
}

fn probe_encoders(exe: &Path) -> MediaResult<HashSet<String>> {
    let mut command = Command::new(exe);
    command
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start ffmpeg: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ffmpeg encoder output was unavailable".to_string())?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut reader = stdout;
        let mut output = Vec::new();
        let mut buffer = [0u8; 8192];
        let mut too_large = false;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if output.len() < 8 * 1024 * 1024 {
                        let remaining = 8 * 1024 * 1024 - output.len();
                        output.extend_from_slice(&buffer[..count.min(remaining)]);
                        too_large |= count > remaining;
                    } else {
                        too_large = true;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send((output, too_large));
    });
    let deadline = Instant::now() + ENCODER_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err("ffmpeg encoder probe timed out".into());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("could not poll ffmpeg encoder probe: {error}"));
            }
        }
    };
    let _ = reader.join();
    let (output, too_large) = rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| "ffmpeg encoder output timed out".to_string())?;
    if too_large {
        return Err("ffmpeg encoder listing exceeded 8 MiB".into());
    }
    let status = status.expect("child status captured");
    if !status.success() {
        return Err(format!("ffmpeg -encoders exited with {status}"));
    }
    Ok(parse_encoder_listing(&String::from_utf8_lossy(&output)))
}

fn parse_encoder_listing(text: &str) -> HashSet<String> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let flags = fields.next()?;
            let name = fields.next()?;
            (flags.len() >= 6 && flags.chars().all(|c| c == '.' || c.is_ascii_alphabetic()))
                .then(|| name.to_string())
        })
        .collect()
}

// -----------------------------------------------------------------------------
// Clip groups and related-media discovery
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilenameGroupKind {
    Take,
    Chapter,
}

#[derive(Debug)]
struct SequenceName {
    key: String,
    ordinal: u32,
    kind: FilenameGroupKind,
}

/// Group clips into takes using common camera filename sequences and capture-time gaps.
/// This helper has no filesystem or UI dependencies, so its grouping rules are testable.
pub fn group_clips(entries: &[FileEntry]) -> Vec<ClipGroup> {
    let mut sequences: HashMap<String, Vec<(u32, &FileEntry, FilenameGroupKind)>> = HashMap::new();
    let mut valid = Vec::new();
    for entry in entries.iter().filter(|e| is_clip_entry(e)) {
        valid.push(entry);
        if let Some(sequence) = sequence_name(&entry.path) {
            let folder = entry.path.parent().map(normalized_path).unwrap_or_default();
            let key = format!("{folder}|{}", sequence.key);
            sequences
                .entry(key)
                .or_default()
                .push((sequence.ordinal, entry, sequence.kind));
        }
    }

    let mut groups = Vec::new();
    let mut consumed = HashSet::new();
    for mut clips in sequences.into_values() {
        clips.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        if clips
            .first()
            .is_some_and(|(_, _, kind)| *kind == FilenameGroupKind::Chapter)
        {
            if clips.len() > 1 {
                let files = clips
                    .iter()
                    .map(|(_, entry, _)| entry.path.clone())
                    .collect::<Vec<_>>();
                consumed.extend(files.iter().cloned());
                groups.push(ClipGroup {
                    files,
                    kind: ClipGroupKind::Chapter,
                });
            }
            continue;
        }
        let mut current: Vec<&FileEntry> = Vec::new();
        let mut previous: Option<(u32, &FileEntry)> = None;
        for (ordinal, entry, _) in clips {
            let joins = previous.is_some_and(|(prev_ordinal, prev_entry)| {
                ordinal == prev_ordinal.saturating_add(1)
                    && !capture_gap_exceeded(prev_entry, entry, MAX_SESSION_GAP)
            });
            if !joins && current.len() > 1 {
                let files = current.iter().map(|e| e.path.clone()).collect::<Vec<_>>();
                consumed.extend(files.iter().cloned());
                groups.push(ClipGroup {
                    files,
                    kind: ClipGroupKind::Take,
                });
            }
            if !joins {
                current.clear();
            }
            current.push(entry);
            previous = Some((ordinal, entry));
        }
        if current.len() > 1 {
            let files = current.iter().map(|e| e.path.clone()).collect::<Vec<_>>();
            consumed.extend(files.iter().cloned());
            groups.push(ClipGroup {
                files,
                kind: ClipGroupKind::Take,
            });
        }
    }

    // Unrecognized clip names can still form a recording session when capture timestamps
    // are close. Missing timestamps never cause unrelated files to be grouped together.
    let mut by_folder: HashMap<PathBuf, Vec<(SystemTime, &FileEntry)>> = HashMap::new();
    for entry in valid
        .into_iter()
        .filter(|entry| !consumed.contains(&entry.path))
    {
        if let (Some(folder), Some(time)) = (entry.path.parent(), capture_time(entry)) {
            by_folder
                .entry(folder.to_path_buf())
                .or_default()
                .push((time, entry));
        }
    }
    for mut timed in by_folder.into_values() {
        timed.sort_by(|(at_a, a), (at_b, b)| at_a.cmp(at_b).then_with(|| a.name.cmp(&b.name)));
        let mut session: Vec<&FileEntry> = Vec::new();
        let mut last: Option<SystemTime> = None;
        for (time, entry) in timed {
            if last
                .is_some_and(|prev| time.duration_since(prev).unwrap_or_default() > MAX_SESSION_GAP)
            {
                if session.len() > 1 {
                    groups.push(ClipGroup {
                        files: session.iter().map(|e| e.path.clone()).collect(),
                        kind: ClipGroupKind::Take,
                    });
                }
                session.clear();
            }
            session.push(entry);
            last = Some(time);
        }
        if session.len() > 1 {
            groups.push(ClipGroup {
                files: session.iter().map(|e| e.path.clone()).collect(),
                kind: ClipGroupKind::Take,
            });
        }
    }

    groups.sort_by(|a, b| {
        a.files
            .first()
            .map(|p| p.to_string_lossy())
            .cmp(&b.files.first().map(|p| p.to_string_lossy()))
    });
    groups
}

fn clip_label(entries: &[FileEntry], path: &Path) -> Option<String> {
    group_clips(entries).into_iter().find_map(|group| {
        let index = group.files.iter().position(|item| item == path)?;
        let name = match group.kind {
            ClipGroupKind::Take => "Take",
            ClipGroupKind::Chapter => "Chapter",
        };
        Some(format!("{name} {} of {}", index + 1, group.files.len()))
    })
}

fn related_scan_cached(
    path: &Path,
    cache: &Arc<Mutex<LruCache<RelatedFolderKey, HashMap<PathBuf, RelatedScan>>>>,
    cancel: &CancelToken,
) -> Option<RelatedScan> {
    if cancel.is_cancelled() {
        return None;
    }
    let directory = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let modified = fs::metadata(&directory)
        .ok()
        .and_then(|metadata| metadata.modified().ok());
    // The related scan also reads the grandparent for sibling proxy folders, so its modified time is part of the key.
    let sibling_modified = directory
        .parent()
        .and_then(|parent| fs::metadata(parent).ok())
        .and_then(|metadata| metadata.modified().ok());
    let folder_key = RelatedFolderKey {
        directory,
        modified: Some((
            modified.unwrap_or(SystemTime::UNIX_EPOCH),
            sibling_modified.unwrap_or(SystemTime::UNIX_EPOCH),
        )),
    };

    {
        let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(scan) = cache
            .get(&folder_key)
            .and_then(|scans| scans.get(path))
            .cloned()
        {
            return (!cancel.is_cancelled()).then_some(scan);
        }
    }

    let take_label = scan_clip_label(path, cancel);
    if cancel.is_cancelled() {
        return None;
    }
    let related = related_media_cancellable(path, cancel)?;
    let scan = RelatedScan {
        take_label,
        related,
    };
    let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
    if cancel.is_cancelled() {
        return None;
    }
    let mut scans = cache.pop(&folder_key).unwrap_or_default();
    if scans.len() >= RELATED_PATHS_PER_FOLDER
        && !scans.contains_key(path)
        && let Some(oldest_path) = scans.keys().next().cloned()
    {
        scans.remove(&oldest_path);
    }
    scans.insert(path.to_path_buf(), scan.clone());
    cache.put(folder_key, scans);
    Some(scan)
}

fn scan_clip_label(path: &Path, cancel: &CancelToken) -> Option<String> {
    let directory = path.parent()?;
    let entries = fs::read_dir(directory).ok()?;
    let mut files = Vec::new();
    for entry in entries {
        if cancel.is_cancelled() {
            return None;
        }
        let Ok(entry) = entry else { continue };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let candidate = entry.path();
        let ext = ext_of(&candidate);
        if forge_core::model::media_kind_for_ext(&ext) != MediaKind::Video {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        files.push(FileEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: candidate,
            kind: EntryKind::File,
            size: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            hidden: false,
            readonly: metadata.permissions().readonly(),
            system: false,
            online_only: false,
            ext,
        });
    }
    clip_label(&files, path)
}

fn sequence_name(path: &Path) -> Option<SequenceName> {
    let stem = path.file_stem()?.to_string_lossy().to_ascii_uppercase();
    if let Some(number) = stem.strip_prefix("MVI_").and_then(parse_digits) {
        return Some(SequenceName {
            key: "canon-mvi".into(),
            ordinal: number,
            kind: FilenameGroupKind::Take,
        });
    }
    if let Some(number) = stem.strip_prefix("DJI_").and_then(parse_digits) {
        return Some(SequenceName {
            key: "dji".into(),
            ordinal: number,
            kind: FilenameGroupKind::Take,
        });
    }
    // GX is HEVC and GH is AVC; both use the same chapter scheme (GX01 0123 = chapter 1, take 0123).
    if let Some(rest) = stem.strip_prefix("GX").or_else(|| stem.strip_prefix("GH"))
        && rest.len() == 6
        && rest.bytes().all(|b| b.is_ascii_digit())
    {
        let chapter = parse_digits(&rest[..2])?;
        let take = &rest[2..];
        return Some(SequenceName {
            key: format!("gopro-{take}"),
            ordinal: chapter,
            kind: FilenameGroupKind::Chapter,
        });
    }
    if let Some(rest) = stem.strip_prefix('C')
        && (4..=6).contains(&rest.len())
        && let Some(number) = parse_digits(rest)
    {
        return Some(SequenceName {
            key: "sony-c".into(),
            ordinal: number,
            kind: FilenameGroupKind::Take,
        });
    }
    if let Some(rest) = stem.strip_prefix('P')
        && rest.len() >= 7
        && rest.bytes().all(|b| b.is_ascii_digit())
    {
        let camera = &rest[..1];
        let number = parse_digits(&rest[rest.len() - 6..])?;
        return Some(SequenceName {
            key: format!("panasonic-p{camera}"),
            ordinal: number,
            kind: FilenameGroupKind::Take,
        });
    }
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() >= 3
        && parts[0].starts_with('A')
        && parts[0].len() > 1
        && parts[0][1..].bytes().all(|b| b.is_ascii_digit())
        && let Some(last) = parts.last()
        && let Some(number) = last.strip_prefix('C').and_then(parse_digits)
    {
        return Some(SequenceName {
            key: format!("blackmagic-{}", parts[0]),
            ordinal: number,
            kind: FilenameGroupKind::Take,
        });
    }
    None
}

fn parse_digits(value: &str) -> Option<u32> {
    (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn capture_time(entry: &FileEntry) -> Option<SystemTime> {
    entry.created.or(entry.modified)
}

fn is_clip_entry(entry: &FileEntry) -> bool {
    entry.kind == EntryKind::File
        && (entry.media_kind() == MediaKind::Video || entry.ext == "braw")
        && entry.ext != "lrv"
}

fn capture_gap_exceeded(a: &FileEntry, b: &FileEntry, max_gap: Duration) -> bool {
    match (capture_time(a), capture_time(b)) {
        (Some(a), Some(b)) => {
            a.duration_since(b)
                .unwrap_or_else(|_| b.duration_since(a).unwrap_or_default())
                > max_gap
        }
        _ => false,
    }
}

/// Finds likely proxy/derivative files, GoPro companions, and same-basename variants.
#[cfg(test)]
pub fn related_media(path: &Path) -> Vec<Related> {
    related_media_cancellable(path, &CancelToken::new()).unwrap_or_default()
}

fn related_media_cancellable(path: &Path, cancel: &CancelToken) -> Option<Vec<Related>> {
    if cancel.is_cancelled() {
        return None;
    }
    let Some(parent) = path.parent() else {
        return Some(Vec::new());
    };
    if path.file_stem().is_none() {
        return Some(Vec::new());
    }
    let mut found = HashMap::<String, Related>::new();
    let mut scanned_entries = 0;
    scan_related_directory(path, parent, &mut found, 0, &mut scanned_entries, cancel).ok()?;
    if parent
        .file_name()
        .is_some_and(|name| is_proxy_folder(&name.to_string_lossy()))
        && let Some(source_dir) = parent.parent()
    {
        scan_related_directory(path, source_dir, &mut found, 0, &mut scanned_entries, cancel)
            .ok()?;
    }
    if cancel.is_cancelled() {
        return None;
    }
    if let Some(grandparent) = parent.parent()
        && let Ok(entries) = fs::read_dir(grandparent)
    {
        for entry in entries.take(MAX_LOCAL_RELATED_ENTRIES).flatten() {
            if cancel.is_cancelled() {
                return None;
            }
            if entry.path() == parent || scanned_entries >= MAX_LOCAL_RELATED_ENTRIES {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir()
                && !file_type.is_symlink()
                && is_proxy_folder(&entry.file_name().to_string_lossy())
            {
                scan_related_directory(
                    path,
                    &entry.path(),
                    &mut found,
                    0,
                    &mut scanned_entries,
                    cancel,
                )
                .ok()?;
            }
        }
    }
    if cancel.is_cancelled() {
        return None;
    }
    let mut values: Vec<_> = found.into_values().collect();
    values.sort_by(|a, b| {
        a.label.cmp(&b.label).then_with(|| {
            a.path
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.path.to_string_lossy().to_lowercase())
        })
    });
    Some(values)
}

fn scan_related_directory(
    path: &Path,
    directory: &Path,
    found: &mut HashMap<String, Related>,
    depth: usize,
    scanned_entries: &mut usize,
    cancel: &CancelToken,
) -> Result<(), ()> {
    if cancel.is_cancelled() {
        return Err(());
    }
    if *scanned_entries >= MAX_LOCAL_RELATED_ENTRIES || depth > MAX_LOCAL_RELATED_DEPTH {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        if cancel.is_cancelled() {
            return Err(());
        }
        if *scanned_entries >= MAX_LOCAL_RELATED_ENTRIES {
            break;
        }
        *scanned_entries += 1;
        let candidate = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if is_link_or_junction(&entry, &file_type) {
            continue;
        }
        if file_type.is_dir() {
            if is_proxy_folder(&entry.file_name().to_string_lossy()) {
                scan_related_directory(
                    path,
                    &candidate,
                    found,
                    depth + 1,
                    scanned_entries,
                    cancel,
                )?;
            }
            continue;
        }
        if !file_type.is_file()
            || normalized_path(&candidate) == normalized_path(path)
            || !is_media_derivative(&candidate)
        {
            continue;
        }
        let Some(label) = proxy_match_label(path, &candidate) else {
            continue;
        };
        found.insert(
            normalized_path(&candidate),
            Related {
                path: candidate,
                label,
            },
        );
    }
    Ok(())
}

fn is_media_derivative(path: &Path) -> bool {
    matches!(ext_of(path).as_str(), "lrv" | "lrf" | "thm")
        || MediaKind::Video == forge_core::model::media_kind_for_ext(&ext_of(path))
}

fn is_proxy_folder(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "proxy" | "proxies" | "_proxies" | "optimized media" | "sub"
    )
}

fn is_proxy_suffix(path: &Path) -> bool {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|s| s.ends_with("_proxy") || s.ends_with("-proxy") || s.ends_with(".proxy"))
}

fn normalized_stem(path: &Path) -> String {
    let Some(stem) = path.file_stem() else {
        return String::new();
    };
    let mut value = stem.to_string_lossy().to_ascii_lowercase();
    if let Some((root, suffix)) = value.rsplit_once(['_', '-'])
        && let Some((width, height)) = suffix.split_once('x')
        && width.len() >= 3
        && height.len() >= 3
        && width.bytes().all(|b| b.is_ascii_digit())
        && height.bytes().all(|b| b.is_ascii_digit())
    {
        value = root.to_string();
    }
    if let Some((root, _)) = value.split_once(".proxy.") {
        value = root.to_string();
    }
    for suffix in ["_proxy", "-proxy", ".proxy"] {
        if let Some(root) = value.strip_suffix(suffix) {
            value = root.to_string();
            break;
        }
    }
    value.trim_end_matches(['_', '-']).to_string()
}

// Sony's HXR-NX80 manual shows C0001.MP4 under M4ROOT/CLIP and its proxy
// C0001S03.MP4 under M4ROOT/SUB, so S plus two digits identifies the sub-clip.
fn sony_xavc_base(stem: &str) -> Option<(String, bool)> {
    let upper = stem.to_ascii_uppercase();
    let digits = upper.strip_prefix('C')?;
    let (clip_number, is_subclip) = match digits.split_once('S') {
        Some((clip_number, suffix))
            if suffix.len() == 2 && suffix.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            (clip_number, true)
        }
        Some(_) => return None,
        None => (digits, false),
    };
    ((4..=6).contains(&clip_number.len()) && clip_number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| (format!("C{clip_number}"), is_subclip))
}

fn proxy_match_label(source: &Path, candidate: &Path) -> Option<String> {
    let source_stem = source.file_stem()?.to_string_lossy();
    let candidate_stem = candidate.file_stem()?.to_string_lossy();
    let source_upper = source_stem.to_ascii_uppercase();
    let candidate_upper = candidate_stem.to_ascii_uppercase();
    let extension = ext_of(candidate);
    let in_proxy_folder = candidate
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| is_proxy_folder(&name.to_string_lossy()));

    let gopro_lrv = extension == "lrv"
        && (source_upper.starts_with("GX") || source_upper.starts_with("GH"))
        && source_upper.len() == 8
        && source_upper[2..].bytes().all(|byte| byte.is_ascii_digit())
        && candidate_upper == format!("GL{}", &source_upper[2..]);
    let sony_subclip = match (
        sony_xavc_base(&source_stem),
        sony_xavc_base(&candidate_stem),
    ) {
        (Some((source_base, source_is_subclip)), Some((candidate_base, candidate_is_subclip))) => {
            source_base == candidate_base && (source_is_subclip || candidate_is_subclip)
        }
        _ => false,
    };
    let same_stem = normalized_stem(source) == normalized_stem(candidate);

    if gopro_lrv {
        Some("GoPro low-resolution companion".into())
    } else if extension == "lrf" && same_stem {
        Some("DJI low-resolution companion".into())
    } else if sony_subclip {
        Some("Sony XAVC S proxy sub-clip".into())
    } else if same_stem {
        Some(
            match extension.as_str() {
                "lrv" => "GoPro low-resolution companion",
                "lrf" => "DJI low-resolution companion",
                "thm" => "GoPro thumbnail companion",
                _ if in_proxy_folder => "Proxy / optimized media",
                _ if is_proxy_suffix(candidate) => "Proxy derivative",
                _ => "Same-basename variant",
            }
            .to_string(),
        )
    } else {
        None
    }
}

fn locate_proxy_tree(
    root: &Path,
    sources: &[PathBuf],
    cancel: &CancelToken,
) -> Result<ProxyScanResults, String> {
    let root_metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("Cannot read selected folder: {error}"))?;
    let root_type = root_metadata.file_type();
    if root_type.is_symlink() || has_reparse_attribute(&root_metadata) {
        return Err("The selected root is a link or junction; choose a regular folder".into());
    }
    if !root_type.is_dir() {
        return Err("The selected root is not a folder".into());
    }

    let started = Instant::now();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut entries_seen = 0usize;
    let mut directories_seen = 0usize;
    let mut found = HashMap::<String, Related>::new();
    let mut truncated = false;

    while let Some((directory, depth)) = stack.pop() {
        if cancel.is_cancelled() {
            return Err("Proxy scan cancelled".into());
        }
        if started.elapsed() >= MAX_PROXY_SCAN_TIME
            || entries_seen >= MAX_PROXY_SCAN_ENTRIES
            || directories_seen >= MAX_PROXY_SCAN_DIRECTORIES
            || found.len() >= MAX_PROXY_SCAN_MATCHES
        {
            truncated = true;
            break;
        }
        directories_seen += 1;
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            if cancel.is_cancelled() {
                return Err("Proxy scan cancelled".into());
            }
            if started.elapsed() >= MAX_PROXY_SCAN_TIME
                || entries_seen >= MAX_PROXY_SCAN_ENTRIES
                || found.len() >= MAX_PROXY_SCAN_MATCHES
            {
                truncated = true;
                break;
            }
            let Ok(entry) = entry else { continue };
            entries_seen += 1;
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            // Symlinks and junctions are never followed, so the scan stays inside the chosen tree.
            if is_link_or_junction(&entry, &file_type) {
                continue;
            }
            let candidate = entry.path();
            if file_type.is_dir() {
                if depth < MAX_PROXY_SCAN_DEPTH {
                    stack.push((candidate, depth + 1));
                } else {
                    truncated = true;
                }
                continue;
            }
            if !file_type.is_file() || !is_media_derivative(&candidate) {
                continue;
            }
            for source in sources {
                if normalized_path(source) == normalized_path(&candidate) {
                    continue;
                }
                let Some(label) = proxy_match_label(source, &candidate) else {
                    continue;
                };
                let source_name = source
                    .file_name()
                    .map(|name| name.to_string_lossy())
                    .unwrap_or_default();
                found.entry(normalized_path(&candidate)).or_insert(Related {
                    path: candidate.clone(),
                    label: format!("Located for {source_name}: {label}"),
                });
                break;
            }
        }
    }

    let mut related: Vec<_> = found.into_values().collect();
    related.sort_by(|a, b| {
        a.label.cmp(&b.label).then_with(|| {
            a.path
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.path.to_string_lossy().to_lowercase())
        })
    });
    Ok(ProxyScanResults { related, truncated })
}

// -----------------------------------------------------------------------------
// Timecode helpers
// -----------------------------------------------------------------------------

fn format_elapsed(seconds: f64) -> String {
    let millis = (seconds.max(0.0) * 1_000.0).round() as u64;
    let hours = millis / 3_600_000;
    let minutes = (millis / 60_000) % 60;
    let secs = (millis / 1_000) % 60;
    let ms = millis % 1_000;
    format!("{hours:02}:{minutes:02}:{secs:02}.{ms:03}")
}

fn format_timecode_at(start: &str, offset_s: f64, fps: f64) -> Option<String> {
    let separator = if start.contains(';') { ';' } else { ':' };
    let fields: Vec<&str> = start.split([':', ';']).collect();
    if fields.len() != 4 || !offset_s.is_finite() || !fps.is_finite() || fps <= 0.0 {
        return None;
    }
    let hours: u64 = fields[0].parse().ok()?;
    let minutes: u64 = fields[1].parse().ok()?;
    let seconds: u64 = fields[2].parse().ok()?;
    let frames: u64 = fields[3].parse().ok()?;
    let nominal = fps.round().clamp(1.0, 120.0) as u64;
    let drop = if separator == ';' && nominal == 30 && (fps - 29.97).abs() < 0.1 {
        2
    } else if separator == ';' && nominal == 60 && (fps - 59.94).abs() < 0.1 {
        4
    } else {
        0
    };
    if minutes >= 60 || seconds >= 60 || frames >= nominal {
        return None;
    }
    let total_minutes = hours.saturating_mul(60).saturating_add(minutes);
    let labeled_frames = (hours
        .saturating_mul(3_600)
        .saturating_add(minutes.saturating_mul(60))
        .saturating_add(seconds))
    .saturating_mul(nominal)
    .saturating_add(frames);
    let dropped_before_start = if drop > 0 {
        drop * (total_minutes.saturating_sub(total_minutes / 10))
    } else {
        0
    };
    let start_real_frame = labeled_frames.saturating_sub(dropped_before_start);
    let offset_frames = (offset_s.max(0.0) * fps)
        .round()
        .clamp(0.0, u64::MAX as f64) as u64;
    let real_frame = start_real_frame.saturating_add(offset_frames);
    let extra_drop = if drop > 0 {
        let frames_per_ten_minutes = nominal * 600 - drop * 9;
        let frames_per_minute = nominal * 60 - drop;
        let ten_minute_blocks = real_frame / frames_per_ten_minutes;
        let remainder = real_frame % frames_per_ten_minutes;
        drop * 9 * ten_minute_blocks + drop * (remainder.saturating_sub(drop) / frames_per_minute)
    } else {
        0
    };
    let labeled = real_frame.saturating_add(extra_drop);
    let frame = labeled % nominal;
    let whole_seconds = labeled / nominal;
    let second = whole_seconds % 60;
    let minute = (whole_seconds / 60) % 60;
    let hour = (whole_seconds / 3_600) % 24;
    Some(format!(
        "{hour:02}:{minute:02}:{second:02}{separator}{frame:02}"
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn workers_refuse_a_clip_that_is_online_only_when_they_start() {
        let d = tempfile::tempdir().unwrap();
        let clip = d.path().join("clip.mp4");
        std::fs::write(&clip, b"x").unwrap();
        assert!(super::open_checked(&clip).is_ok());
        forge_core::cloud::simulate_online_only(&clip);
        let refused = super::open_checked(&clip).map(|_| ());
        forge_core::cloud::clear_simulated_online_only(&clip);
        assert_eq!(refused, Err(super::ONLINE_ONLY_VIDEO.to_string()));
        std::fs::remove_file(&clip).unwrap();
        let missing = super::open_checked(&clip).unwrap_err();
        assert!(missing.ends_with("no longer exists"), "{missing}");
    }

    #[test]
    fn placing_a_proxy_never_replaces_an_existing_file() {
        let d = tempfile::tempdir().unwrap();
        let temp = d.path().join("clip.partial");
        let out = d.path().join("clip_proxy.mp4");
        std::fs::write(&temp, b"new").unwrap();
        std::fs::write(&out, b"existing").unwrap();
        assert!(super::place_no_replace(&temp, &out).is_err());
        assert_eq!(std::fs::read(&out).unwrap(), b"existing");
        std::fs::remove_file(&out).unwrap();
        super::place_no_replace(&temp, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"new");
        assert!(!temp.exists());
    }

    #[test]
    fn proxy_temp_collision_and_cleanup_never_delete_a_foreign_file() {
        let destination = tempfile::tempdir().unwrap();
        let output = destination.path().join("clip_proxy.mp4");
        let private_dir = create_proxy_temp_dir(destination.path(), 44, 0).unwrap();
        let foreign = private_dir.join(output.file_name().unwrap());
        std::fs::write(&foreign, b"foreign output").unwrap();

        assert!(ProxyTemp::reserve(&private_dir, &output).is_err());
        assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign output");
        assert!(private_dir.exists());

        let owned = ProxyTemp::create(destination.path(), &output, 45, 1).unwrap();
        std::fs::write(owned.path(), b"job output").unwrap();
        let foreign_sibling = owned.directory.join("foreign.txt");
        std::fs::write(&foreign_sibling, b"keep this").unwrap();
        let owned_path = owned.path().to_path_buf();
        let owned_directory = owned.directory.clone();
        drop(owned);
        assert!(!owned_path.exists());
        assert_eq!(std::fs::read(&foreign_sibling).unwrap(), b"keep this");
        assert!(owned_directory.exists());
    }

    use super::*;
    use tempfile::TempDir;

    #[test]
    fn related_rows_are_bounded_and_report_hidden_matches() {
        let related = (0..320)
            .map(|index| Related {
                path: PathBuf::from(format!(r"C:\Media\clip-{index:03}_proxy.mp4")),
                label: "Proxy".into(),
            })
            .collect::<Vec<_>>();
        let located = related[..12].to_vec();

        let (rows, hidden) = visible_related_rows(&related, &located);

        assert_eq!(rows.len(), MAX_DRAWN_RELATED_ROWS);
        assert_eq!(hidden, related.len() - MAX_DRAWN_RELATED_ROWS);
    }

    #[test]
    fn related_scan_worker_cancels_and_coalesces_rapid_requests() {
        use std::sync::atomic::{AtomicBool, AtomicUsize};

        let worker = Arc::new(RelatedScanWorker::new());
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let first_started = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = unbounded();
        let (messages_tx, messages_rx) = unbounded();
        let scan_active = active.clone();
        let scan_max_active = max_active.clone();
        let scan_first_started = first_started.clone();
        let scan = move |request: &RelationScanRequest| {
            let count = scan_active.fetch_add(1, Ordering::SeqCst) + 1;
            scan_max_active.fetch_max(count, Ordering::SeqCst);
            if !scan_first_started.swap(true, Ordering::SeqCst) {
                started_tx.send(()).unwrap();
            }
            for _ in 0..200 {
                if request.cancel.is_cancelled() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            scan_active.fetch_sub(1, Ordering::SeqCst);
            (!request.cancel.is_cancelled()).then_some(RelatedScan {
                take_label: None,
                related: Vec::new(),
            })
        };
        let thread_worker = worker.clone();
        let handle = std::thread::spawn(move || run_related_scan_worker(thread_worker, scan));
        let cache = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(RELATED_FOLDER_CACHE_CAPACITY).unwrap(),
        )));
        let context = egui::Context::default();
        let make_request = |number: u64| RelationScanRequest {
            id: number,
            path: PathBuf::from(format!(r"C:\Media\clip-{number}.mp4")),
            media: MediaKey {
                path: PathBuf::from(format!(r"C:\Media\clip-{number}.mp4")),
                modified: None,
            },
            generation: number,
            cancel: CancelToken::new(),
            cache: cache.clone(),
            tx: messages_tx.clone(),
            repaint: context.clone(),
        };

        worker.enqueue(make_request(1));
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the first request starts");
        for number in 2..=80 {
            worker.enqueue(make_request(number));
        }

        // Generous: on a loaded machine the 200 short sleeps of the last scan can exceed 2 s.
        match messages_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the latest request completes")
        {
            VideoMessage::Relations {
                media, generation, ..
            } => {
                assert_eq!(generation, 80);
                assert_eq!(media.path, PathBuf::from(r"C:\Media\clip-80.mp4"));
            }
            _ => panic!("expected the latest related-scan result"),
        }
        assert!(messages_rx.try_recv().is_err());
        assert_eq!(max_active.load(Ordering::SeqCst), 1);

        worker.shutdown();
        handle.join().unwrap();
    }

    #[test]
    fn a_stopped_ffmpeg_has_exited_before_the_caller_returns() {
        let mut child = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        let status = stop_child(&mut child).expect("an exit status: the process was reaped");
        assert!(!status.success(), "killed, not run to the end");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn selecting_a_clip_uses_the_listing_flag_not_the_filesystem() {
        // The path does not exist: any filesystem check would say "missing", not online-only.
        let mut clip = entry("cloud.mp4", None);
        clip.online_only = true;
        let mut ui = VideoUi::default();
        ui.sync_selection(Some(&clip), None, &egui::Context::default());
        assert_eq!(ui.probe_error.as_deref(), Some(ONLINE_ONLY_VIDEO));
        assert_eq!(ui.strip_error.as_deref(), Some(ONLINE_ONLY_VIDEO));
        assert!(!ui.probe_pending && !ui.strip_pending, "no worker for a listed placeholder");
    }

    fn entry(name: &str, at: Option<SystemTime>) -> FileEntry {
        let path = PathBuf::from(r"C:\Media").join(name);
        let ext = ext_of(&path);
        FileEntry {
            path,
            name: name.to_string(),
            kind: EntryKind::File,
            size: 1,
            modified: at,
            created: None,
            hidden: false,
            readonly: false,
            system: false,
            online_only: false,
            ext,
        }
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"fixture").unwrap();
    }

    #[test]
    fn groups_common_camera_sequences_and_gopro_chapters() {
        let entries = [
            "MVI_1234.MP4",
            "MVI_1235.MP4",
            "C0001.MP4",
            "C0002.MP4",
            "DJI_0001.MP4",
            "DJI_0002.MP4",
            "P1000123.MP4",
            "P1000124.MP4",
            "A001_01011523_C001.braw",
            "A001_01011524_C002.braw",
            "GX010123.MP4",
            "GX020123.MP4",
        ]
        .into_iter()
        .map(|name| entry(name, None))
        .collect::<Vec<_>>();

        let groups = group_clips(&entries);
        assert_eq!(groups.len(), 6);
        let chapter = groups
            .iter()
            .find(|group| group.kind == ClipGroupKind::Chapter)
            .unwrap();
        assert_eq!(chapter.files.len(), 2);
        assert!(chapter.files[0].to_string_lossy().ends_with("GX010123.MP4"));
        assert!(chapter.files[1].to_string_lossy().ends_with("GX020123.MP4"));
        assert_eq!(
            clip_label(&entries, Path::new(r"C:\Media\GX020123.MP4")).as_deref(),
            Some("Chapter 2 of 2")
        );
        assert!(
            groups
                .iter()
                .filter(|group| group.kind == ClipGroupKind::Take)
                .all(|group| group.files.len() == 2)
        );
    }

    #[test]
    fn gopro_avc_chapters_group_with_hevc_naming_and_takes_stay_separate() {
        let entries = ["GH010456.MP4", "GH020456.MP4", "GH010457.MP4"]
            .into_iter()
            .map(|name| entry(name, None))
            .collect::<Vec<_>>();
        let groups = group_clips(&entries);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].kind, ClipGroupKind::Chapter);
        assert_eq!(groups[0].files.len(), 2);
        assert!(
            groups[0].files[0]
                .to_string_lossy()
                .ends_with("GH010456.MP4")
        );
    }

    #[test]
    fn capture_time_groups_unknown_names_but_splits_long_gaps() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let close = vec![
            entry("wide_angle.mp4", Some(start)),
            entry("camera_b.mp4", Some(start + Duration::from_secs(90))),
        ];
        let groups = group_clips(&close);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].kind, ClipGroupKind::Take);

        let far_apart = vec![
            entry("MVI_1234.mp4", Some(start)),
            entry("MVI_1235.mp4", Some(start + Duration::from_secs(31 * 60))),
        ];
        assert!(group_clips(&far_apart).is_empty());
    }

    #[test]
    fn related_media_finds_proxy_folders_suffixes_resolutions_and_gopro_companions() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let source = root.join("GX010123.mp4");
        let proxy = root.join("Proxy").join("GX010123_proxy.mp4");
        let optimized = root.join("Optimized Media").join("GX010123.mp4");
        let alternate = root.join("GX010123_1920x1080.mov");
        let low_res = root.join("GX010123.lrv");
        let thumb = root.join("GX010123.thm");
        for path in [&source, &proxy, &optimized, &alternate, &low_res, &thumb] {
            touch(path);
        }

        let related = related_media(&source);
        let paths: HashSet<_> = related.iter().map(|r| r.path.clone()).collect();
        assert!(paths.contains(&proxy));
        assert!(paths.contains(&optimized));
        assert!(paths.contains(&alternate));
        assert!(paths.contains(&low_res));
        assert!(paths.contains(&thumb));
        assert!(
            related
                .iter()
                .any(|r| r.path == low_res && r.label.contains("low-resolution"))
        );
        assert!(
            related
                .iter()
                .any(|r| r.path == proxy && r.label.contains("Proxy"))
        );
    }

    #[test]
    fn related_media_finds_generated_proxies_and_all_named_proxy_folders() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let source = root.join("clip.mp4");
        let candidates = [
            root.join("Proxy").join("clip_proxy.mp4"),
            root.join("Proxies").join("clip_Proxy.mp4"),
            root.join("_Proxies").join("clip.proxy.1080.mp4"),
        ];
        touch(&source);
        for candidate in &candidates {
            touch(candidate);
        }
        let found: HashSet<_> = related_media(&source)
            .into_iter()
            .map(|item| item.path)
            .collect();
        for candidate in candidates {
            assert!(found.contains(&candidate), "{}", candidate.display());
        }
    }

    #[test]
    fn related_media_finds_gopro_dji_and_sony_xavc_proxy_names() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let gopro = root.join("GX010001.MP4");
        let gopro_lrv = root.join("GL010001.LRV");
        let dji = root.join("DJI_0001.MP4");
        let dji_lrf = root.join("DJI_0001.LRF");
        let sony = root.join("C0001.MP4");
        let sony_proxy = root.join("SUB").join("C0001S03.MP4");
        for path in [&gopro, &gopro_lrv, &dji, &dji_lrf, &sony, &sony_proxy] {
            touch(path);
        }

        let found_paths = |path: &Path| {
            related_media(path)
                .into_iter()
                .map(|item| item.path)
                .collect::<HashSet<_>>()
        };
        assert!(found_paths(&gopro).contains(&gopro_lrv));
        assert!(found_paths(&dji).contains(&dji_lrf));
        let sony_results = related_media(&sony);
        assert!(
            sony_results
                .iter()
                .any(|item| { item.path == sony_proxy && item.label.contains("Sony XAVC S") })
        );
        assert_eq!(sony_xavc_base("C0001S03"), Some(("C0001".into(), true)));
        assert_eq!(sony_xavc_base("C0001S3"), None);
    }

    #[test]
    fn related_media_finds_proxy_in_a_sibling_folder_but_not_a_neighboring_name() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let source = root.join("Originals").join("clip.mp4");
        let proxy = root.join("Proxies").join("clip_proxy.mp4");
        let unrelated = root.join("Proxies").join("clip_backup.mp4");
        touch(&source);
        touch(&proxy);
        touch(&unrelated);

        let paths: HashSet<_> = related_media(&source)
            .into_iter()
            .map(|item| item.path)
            .collect();
        assert!(paths.contains(&proxy));
        assert!(!paths.contains(&unrelated));
    }

    #[test]
    fn locate_proxy_tree_scans_selected_root_and_honors_cancellation() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("Originals").join("clip.mp4");
        let root = temp.path().join("elsewhere");
        let proxy = root
            .join("day-one")
            .join("optimized")
            .join("clip_proxy.mp4");
        touch(&source);
        touch(&proxy);

        let result =
            locate_proxy_tree(&root, std::slice::from_ref(&source), &CancelToken::new()).unwrap();
        assert!(result.related.iter().any(|item| item.path == proxy));

        let cancelled = CancelToken::new();
        cancelled.cancel();
        assert!(locate_proxy_tree(&root, &[source], &cancelled).is_err());
    }

    #[test]
    fn estimate_math_and_space_check_use_duration_and_bitrate() {
        assert_eq!(estimated_output_bytes(2_000_000, 60.0), 15_000_000);
        assert_eq!(estimated_output_bytes(8_000_000, f64::NAN), 0);
        let sources = vec![
            PathBuf::from(r"C:\Media\a.mp4"),
            PathBuf::from(r"C:\Media\b.mp4"),
        ];
        let estimate = ProxyEstimate {
            durations_s: vec![Some(60.0), Some(30.0)],
            duration_errors: vec![None, None],
            free_spaces: vec![
                Some(VolumeSpace {
                    root: PathBuf::from(r"C:\"),
                    free_bytes: 30_000_000,
                }),
                Some(VolumeSpace {
                    root: PathBuf::from(r"C:\"),
                    free_bytes: 30_000_000,
                }),
            ],
        };
        assert_eq!(
            proxy_space_sufficient(&sources, &estimate, 2_000_000),
            Some(true)
        );
        assert_eq!(
            proxy_space_sufficient(&sources, &estimate, 8_000_000),
            Some(false)
        );
    }

    #[test]
    fn proxy_arguments_keep_paths_as_single_file_protocol_arguments() {
        let source = Path::new("-camera take ünicode.mp4");
        let target = Path::new(r"C:\Proxy Output\-take_proxy.mp4");
        let args = build_proxy_args(source, target, ProxyPreset::H2641080);
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-i", "file:-camera take ünicode.mp4"])
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("file:C:\\Proxy Output\\-take_proxy.mp4")
        );
        assert!(args.contains(&"-progress".to_string()));
        assert!(args.contains(&"-n".to_string()));
        assert!(args.contains(&"libx264".to_string()));

        let prores = build_proxy_args(source, target, ProxyPreset::ProResProxy);
        assert!(prores.iter().any(|arg| arg == "prores_ks"));
    }

    #[test]
    fn progress_parser_reads_microseconds_and_ignores_other_lines() {
        assert_eq!(parse_progress_ms("out_time_ms=1250000\n"), Some(1_250_000));
        assert_eq!(parse_progress_ms("progress=end"), None);
        assert_eq!(parse_progress_ms("out_time_ms=N/A"), None);
        let fraction = parse_progress_ms("out_time_ms=5000000").unwrap() as f64 / 10_000_000.0;
        assert!((fraction - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn timecode_offsets_preserve_smpte_start_and_drop_frame_separator() {
        assert_eq!(
            format_timecode_at("01:02:03:04", 2.0, 25.0).as_deref(),
            Some("01:02:05:04")
        );
        assert_eq!(
            format_timecode_at("00:00:59;28", 1.0, 29.97).as_deref(),
            Some("00:01:01;00")
        );
    }
}
