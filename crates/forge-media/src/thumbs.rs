//! Thumbnail pipeline: decode strategy per format, bounded worker pools, generation-based
//! cancellation, in-flight dedupe, a byte-bounded memory LRU and a persistent disk cache.
//!
//! Flow for [`ThumbnailService::request`]:
//! 1. memory LRU (synchronous, only when the caller supplies mtime + size) →
//! 2. dedupe against in-flight requests →
//! 3. queued on the `image` or `video` [`WorkerPool`] with the caller's priority →
//! 4. worker: memory → disk cache → decode → resize → orientation → write disk cache →
//!    deliver a [`ThumbResult`] on the results channel.
//!
//! Stale work: every request carries a `generation`. A request with a newer generation
//! (or [`ThumbnailService::cancel_stale`]) clears queued work, cancels the shared
//! [`CancelToken`] of the old generation (killing running ffmpeg children) and drops their
//! results. Cancelled requests produce no result.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, unbounded};
use forge_core::model::{MediaKind, ext_of, media_kind_for_ext};
use forge_jobs::{CancelToken, Priority, WorkerPool};
use image::{DynamicImage, ImageDecoder, ImageReader, Limits, RgbaImage};
use lru::LruCache;
use parking_lot::Mutex;

use crate::bin_util::{self, parse_jpeg_header};
use crate::error::{MediaError, Result};
use crate::image_meta::{self, ImageFormat, detect_format};
use crate::raw_preview::{self, PreviewPick};
use crate::video::FfTools;

/// Decoded thumbnail: tightly packed RGBA8, orientation already applied.
#[derive(Clone, PartialEq, Eq)]
pub struct ThumbImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for ThumbImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ThumbImage({}x{})", self.width, self.height)
    }
}

impl ThumbImage {
    pub fn from_rgba(img: RgbaImage) -> Self {
        Self {
            width: img.width(),
            height: img.height(),
            rgba: img.into_raw(),
        }
    }
    pub fn byte_size(&self) -> usize {
        self.rgba.len() + std::mem::size_of::<Self>()
    }
    pub fn to_rgba_image(&self) -> Option<RgbaImage> {
        RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
    }
}

/// Decoder safety limits (decompression-bomb defence).
#[derive(Debug, Clone, Copy)]
pub struct DecodeLimits {
    /// Maximum declared width × height of any decoded input.
    pub max_input_pixels: u64,
    /// Passed to `image::Limits::max_alloc`.
    pub max_alloc_bytes: u64,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_input_pixels: 200_000_000,
            max_alloc_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThumbConfig {
    /// Persistent cache directory (created if missing). `None` = memory only.
    pub cache_dir: Option<PathBuf>,
    /// Memory LRU budget in bytes.
    pub memory_budget_bytes: usize,
    pub image_threads: usize,
    pub video_threads: usize,
    pub queue_capacity: usize,
    pub limits: DecodeLimits,
    /// JPEG quality for the disk cache.
    pub jpeg_quality: u8,
    /// FFmpeg tools; `None` = discover on PATH.
    pub ff_tools: Option<FfTools>,
    /// Frames sampled to choose a representative video frame.
    pub video_samples: usize,
}

impl Default for ThumbConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self {
            cache_dir: None,
            memory_budget_bytes: 256 * 1024 * 1024,
            image_threads: (cores / 2).max(1),
            video_threads: 2,
            queue_capacity: 512,
            limits: DecodeLimits::default(),
            jpeg_quality: 85,
            ff_tools: None,
            video_samples: 5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThumbRequest {
    pub path: PathBuf,
    /// Long edge in pixels (thumbnails are never upscaled).
    pub size: u32,
    pub priority: Priority,
    pub generation: u64,
    /// Supply the file's mtime and size (from the directory listing) to enable synchronous
    /// memory-cache hits and avoid a `stat` on the worker.
    pub modified: Option<SystemTime>,
    pub file_size: Option<u64>,
}

impl ThumbRequest {
    pub fn new(path: impl Into<PathBuf>, size: u32) -> Self {
        Self {
            path: path.into(),
            size,
            priority: Priority::Normal,
            generation: 0,
            modified: None,
            file_size: None,
        }
    }
    pub fn priority(mut self, p: Priority) -> Self {
        self.priority = p;
        self
    }
    pub fn generation(mut self, g: u64) -> Self {
        self.generation = g;
        self
    }
    pub fn stat(mut self, modified: Option<SystemTime>, size: u64) -> Self {
        self.modified = modified;
        self.file_size = Some(size);
        self
    }
}

#[derive(Debug)]
pub struct ThumbResult {
    pub path: PathBuf,
    pub size: u32,
    pub generation: u64,
    pub image: Result<Arc<ThumbImage>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestStatus {
    /// Served synchronously from memory; no result will be sent.
    MemoryHit(Arc<ThumbImage>),
    Queued,
    /// The same thumbnail is already queued/running; one result will be sent.
    AlreadyInFlight,
    /// The request's generation is older than the current one; ignored.
    Stale,
    /// Queue full of higher-priority work; retry later.
    QueueFull,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThumbStats {
    pub memory_hits: u64,
    pub disk_hits: u64,
    pub generated: u64,
    pub errors: u64,
    pub cancelled: u64,
    pub memory_bytes: usize,
    pub memory_entries: usize,
}

pub type CacheKey = [u8; 32];

/// Cache key = BLAKE3(lowercased path, file size, mtime, thumbnail size).
pub fn cache_key(path: &Path, file_size: u64, modified: Option<SystemTime>, size: u32) -> CacheKey {
    let mut h = blake3::Hasher::new();
    h.update(path.to_string_lossy().to_lowercase().as_bytes());
    h.update(&[0]);
    h.update(&file_size.to_le_bytes());
    let mt: i128 = match modified {
        Some(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        },
        None => i128::MIN,
    };
    h.update(&mt.to_le_bytes());
    h.update(&size.to_le_bytes());
    *h.finalize().as_bytes()
}

struct MemCache {
    lru: LruCache<CacheKey, Arc<ThumbImage>>,
    bytes: usize,
    budget: usize,
}

impl MemCache {
    fn get(&mut self, k: &CacheKey) -> Option<Arc<ThumbImage>> {
        self.lru.get(k).cloned()
    }
    fn insert(&mut self, k: CacheKey, v: Arc<ThumbImage>) {
        let sz = v.byte_size();
        if sz > self.budget {
            return;
        }
        if let Some(old) = self.lru.put(k, v) {
            self.bytes -= old.byte_size();
        }
        self.bytes += sz;
        while self.bytes > self.budget {
            match self.lru.pop_lru() {
                Some((_, old)) => self.bytes -= old.byte_size(),
                None => break,
            }
        }
    }
}

struct GenState {
    current: u64,
    token: CancelToken,
}

struct Inner {
    cfg: ThumbConfig,
    ff: FfTools,
    tx: Sender<ThumbResult>,
    mem: Mutex<MemCache>,
    inflight: Mutex<HashMap<(PathBuf, u32), u64>>,
    gen_state: Mutex<GenState>,
    memory_hits: AtomicU64,
    disk_hits: AtomicU64,
    generated: AtomicU64,
    errors: AtomicU64,
    cancelled: AtomicU64,
}

/// Removes the in-flight entry when a task finishes *or is dropped unrun* (evicted from a
/// full queue or cleared on navigation).
struct InflightGuard {
    inner: Arc<Inner>,
    key: (PathBuf, u32),
    generation: u64,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let mut m = self.inner.inflight.lock();
        if m.get(&self.key) == Some(&self.generation) {
            m.remove(&self.key);
        }
    }
}

pub struct ThumbnailService {
    inner: Arc<Inner>,
    rx: Receiver<ThumbResult>,
    image_pool: WorkerPool,
    video_pool: WorkerPool,
}

impl ThumbnailService {
    pub fn new(cfg: ThumbConfig) -> Self {
        let (tx, rx) = unbounded();
        if let Some(d) = &cfg.cache_dir
            && let Err(e) = std::fs::create_dir_all(d)
        {
            tracing::warn!("thumbnail cache dir unavailable: {e}");
        }
        let ff = cfg.ff_tools.clone().unwrap_or_else(FfTools::discover);
        let image_pool = WorkerPool::new("thumb-image", cfg.image_threads, cfg.queue_capacity);
        let video_pool = WorkerPool::new(
            "thumb-video",
            cfg.video_threads,
            (cfg.queue_capacity / 4).max(16),
        );
        let inner = Arc::new(Inner {
            mem: Mutex::new(MemCache {
                lru: LruCache::unbounded(),
                bytes: 0,
                budget: cfg.memory_budget_bytes,
            }),
            ff,
            cfg,
            tx,
            inflight: Mutex::new(HashMap::new()),
            gen_state: Mutex::new(GenState {
                current: 0,
                token: CancelToken::new(),
            }),
            memory_hits: AtomicU64::new(0),
            disk_hits: AtomicU64::new(0),
            generated: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
        });
        Self {
            inner,
            rx,
            image_pool,
            video_pool,
        }
    }

    /// Results channel (cloneable receiver).
    pub fn results(&self) -> &Receiver<ThumbResult> {
        &self.rx
    }

    pub fn current_generation(&self) -> u64 {
        self.inner.gen_state.lock().current
    }

    /// Makes `generation` current: drops all queued work, cancels running work of older
    /// generations and forgets their in-flight entries. No-op if not newer.
    pub fn cancel_stale(&self, generation: u64) {
        {
            let mut g = self.inner.gen_state.lock();
            if generation <= g.current {
                return;
            }
            g.current = generation;
            g.token.cancel();
            g.token = CancelToken::new();
        }
        let n = self.image_pool.clear_queue() + self.video_pool.clear_queue();
        self.inner.cancelled.fetch_add(n as u64, Ordering::Relaxed);
        self.inner.inflight.lock().retain(|_, g| *g >= generation);
    }

    /// Synchronous memory-cache lookup (no I/O).
    pub fn get_cached(
        &self,
        path: &Path,
        size: u32,
        modified: Option<SystemTime>,
        file_size: u64,
    ) -> Option<Arc<ThumbImage>> {
        let k = cache_key(path, file_size, modified, size);
        self.inner.mem.lock().get(&k)
    }

    pub fn request(&self, req: ThumbRequest) -> RequestStatus {
        let size = req.size.clamp(16, 4096);
        if let (Some(m), Some(fs)) = (req.modified, req.file_size)
            && let Some(img) = self.get_cached(&req.path, size, Some(m), fs)
        {
            self.inner.memory_hits.fetch_add(1, Ordering::Relaxed);
            return RequestStatus::MemoryHit(img);
        }
        let current = self.current_generation();
        if req.generation < current {
            return RequestStatus::Stale;
        }
        if req.generation > current {
            self.cancel_stale(req.generation);
        }
        let token = self.inner.gen_state.lock().token.clone();
        let key = (req.path.clone(), size);
        {
            let mut inf = self.inner.inflight.lock();
            if inf.contains_key(&key) {
                return RequestStatus::AlreadyInFlight;
            }
            inf.insert(key.clone(), req.generation);
        }
        let guard = InflightGuard {
            inner: self.inner.clone(),
            key,
            generation: req.generation,
        };
        let is_video = media_kind_for_ext(&ext_of(&req.path)) == MediaKind::Video;
        let pool = if is_video {
            &self.video_pool
        } else {
            &self.image_pool
        };
        let inner = self.inner.clone();
        let priority = req.priority;
        let task = move || {
            let _guard = guard;
            inner.run(req, size, token);
        };
        match pool.submit(priority, task) {
            Ok(()) => RequestStatus::Queued,
            // The rejected closure (and its guard) is dropped → in-flight entry removed.
            Err(_) => RequestStatus::QueueFull,
        }
    }

    pub fn stats(&self) -> ThumbStats {
        let i = &self.inner;
        let mem = i.mem.lock();
        ThumbStats {
            memory_hits: i.memory_hits.load(Ordering::Relaxed),
            disk_hits: i.disk_hits.load(Ordering::Relaxed),
            generated: i.generated.load(Ordering::Relaxed),
            errors: i.errors.load(Ordering::Relaxed),
            cancelled: i.cancelled.load(Ordering::Relaxed),
            memory_bytes: mem.bytes,
            memory_entries: mem.lru.len(),
        }
    }

    pub fn in_flight(&self) -> usize {
        self.inner.inflight.lock().len()
    }

    pub fn is_idle(&self) -> bool {
        self.image_pool.is_idle() && self.video_pool.is_idle()
    }

    pub fn clear_memory(&self) {
        let mut m = self.inner.mem.lock();
        m.lru.clear();
        m.bytes = 0;
    }

    pub fn ff_tools(&self) -> &FfTools {
        &self.inner.ff
    }

    /// Deletes the oldest cache files until the disk cache is ≤ `max_bytes`.
    /// Returns (files removed, bytes now used).
    pub fn prune_disk_cache(&self, max_bytes: u64) -> std::io::Result<(usize, u64)> {
        match &self.inner.cfg.cache_dir {
            Some(d) => prune_cache_dir(d, max_bytes),
            None => Ok((0, 0)),
        }
    }
}

impl Inner {
    fn is_stale(&self, generation: u64) -> bool {
        generation < self.gen_state.lock().current
    }

    fn run(&self, req: ThumbRequest, size: u32, token: CancelToken) {
        if token.is_cancelled() || self.is_stale(req.generation) {
            self.cancelled.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let res = self.produce(&req, size, &token);
        if matches!(res, Err(MediaError::Cancelled)) || self.is_stale(req.generation) {
            self.cancelled.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if res.is_err() {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        let _ = self.tx.send(ThumbResult {
            path: req.path,
            size,
            generation: req.generation,
            image: res,
        });
    }

    fn produce(
        &self,
        req: &ThumbRequest,
        size: u32,
        token: &CancelToken,
    ) -> Result<Arc<ThumbImage>> {
        let (modified, file_size) = match (req.modified, req.file_size) {
            (Some(m), Some(s)) => (Some(m), s),
            _ => {
                let md = std::fs::metadata(&req.path).map_err(|e| MediaError::io(&req.path, e))?;
                (md.modified().ok(), md.len())
            }
        };
        let key = cache_key(&req.path, file_size, modified, size);
        if let Some(img) = self.mem.lock().get(&key) {
            self.memory_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(img);
        }
        if let Some(dir) = &self.cfg.cache_dir
            && let Some(img) = load_cached(dir, &key)
        {
            let img = Arc::new(img);
            self.disk_hits.fetch_add(1, Ordering::Relaxed);
            self.mem.lock().insert(key, img.clone());
            return Ok(img);
        }
        token.check()?;
        let img = generate_thumbnail(
            &req.path,
            size,
            &self.cfg.limits,
            Some(&self.ff),
            self.cfg.video_samples,
            Some(token),
        )?;
        self.generated.fetch_add(1, Ordering::Relaxed);
        if let Some(dir) = &self.cfg.cache_dir
            && let Err(e) = store_cached(dir, &key, &img, self.cfg.jpeg_quality)
        {
            tracing::debug!("thumbnail cache write failed: {e}");
        }
        let img = Arc::new(img);
        self.mem.lock().insert(key, img.clone());
        Ok(img)
    }
}

// ---------------------------------------------------------------------------
// Disk cache
// ---------------------------------------------------------------------------

fn hex(k: &CacheKey) -> String {
    k.iter().map(|b| format!("{b:02x}")).collect()
}

fn cache_paths(dir: &Path, k: &CacheKey) -> (PathBuf, PathBuf) {
    let h = hex(k);
    let sub = dir.join(&h[..2]);
    (sub.join(format!("{h}.jpg")), sub.join(format!("{h}.png")))
}

fn load_cached(dir: &Path, k: &CacheKey) -> Option<ThumbImage> {
    let (jpg, png) = cache_paths(dir, k);
    for (p, fmt) in [
        (jpg, image::ImageFormat::Jpeg),
        (png, image::ImageFormat::Png),
    ] {
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        match image::load_from_memory_with_format(&bytes, fmt) {
            Ok(img) => return Some(ThumbImage::from_rgba(img.to_rgba8())),
            Err(_) => {
                // Corrupt cache entry: remove so it is regenerated.
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    None
}

fn store_cached(dir: &Path, k: &CacheKey, img: &ThumbImage, quality: u8) -> Result<()> {
    let (jpg, png) = cache_paths(dir, k);
    let parent = jpg.parent().unwrap_or(dir);
    std::fs::create_dir_all(parent).map_err(|e| MediaError::io(parent, e))?;
    let (px, _) = img.rgba.as_chunks::<4>();
    let opaque = px.iter().all(|p| p[3] == 255);
    let mut buf = Vec::new();
    let target = if opaque {
        let rgb: Vec<u8> = px.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality.clamp(1, 100))
            .encode(&rgb, img.width, img.height, image::ExtendedColorType::Rgb8)?;
        jpg
    } else {
        let enc = image::codecs::png::PngEncoder::new_with_quality(
            &mut buf,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::Adaptive,
        );
        image::ImageEncoder::write_image(
            enc,
            &img.rgba,
            img.width,
            img.height,
            image::ExtendedColorType::Rgba8,
        )?;
        png
    };
    // Write a private temp file, then publish it. Readers only look at `.jpg`/`.png` names,
    // so a temp file left by a crash is never read as a thumbnail.
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = target.with_extension(format!(
        "tmp{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(e) = std::fs::write(&tmp, &buf) {
        let _ = std::fs::remove_file(&tmp);
        return Err(MediaError::io(&tmp, e));
    }
    publish_new(&tmp, &target)
}

/// Moves a finished temp file to `target` without replacing an existing entry. A hard link
/// fails atomically when `target` exists; volumes without hard links fall back to a
/// check-then-rename. An existing entry is already a complete thumbnail, so it is kept.
fn publish_new(tmp: &Path, target: &Path) -> Result<()> {
    let placed = match std::fs::hard_link(tmp, target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) if target.symlink_metadata().is_ok() => Ok(()),
        Err(_) => std::fs::rename(tmp, target).map_err(|e| MediaError::io(target, e)),
    };
    let _ = std::fs::remove_file(tmp);
    placed
}

/// Deletes the least-recently-modified cache files until the directory is ≤ `max_bytes`.
pub fn prune_cache_dir(dir: &Path, max_bytes: u64) -> std::io::Result<(usize, u64)> {
    let mut files = Vec::new();
    let mut total = 0u64;
    for sub in std::fs::read_dir(dir)? {
        let sub = sub?;
        if !sub.file_type()?.is_dir() {
            continue;
        }
        for f in std::fs::read_dir(sub.path())? {
            let f = f?;
            let md = f.metadata()?;
            if md.is_file() {
                total += md.len();
                files.push((md.modified().unwrap_or(UNIX_EPOCH), md.len(), f.path()));
            }
        }
    }
    files.sort_by_key(|f| f.0);
    let mut removed = 0;
    for (_, len, p) in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            total -= len;
            removed += 1;
        }
    }
    Ok((removed, total))
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

fn image_limits(l: &DecodeLimits) -> Limits {
    let mut lim = Limits::default();
    lim.max_image_width = Some(65_535 * 4);
    lim.max_image_height = Some(65_535 * 4);
    lim.max_alloc = Some(l.max_alloc_bytes);
    lim
}

fn check_pixels(w: u32, h: u32, l: &DecodeLimits) -> Result<()> {
    if w == 0 || h == 0 {
        return Err(MediaError::corrupt("zero image dimension"));
    }
    if w as u64 * h as u64 > l.max_input_pixels {
        return Err(MediaError::TooLarge {
            width: w,
            height: h,
            limit: l.max_input_pixels,
        });
    }
    Ok(())
}

/// Decodes via the `image` crate after checking declared dimensions; returns the image and
/// the orientation the decoder found in its metadata (EXIF in JPEG/PNG/WebP/TIFF).
fn decode_limited(
    reader: ImageReader<impl std::io::BufRead + std::io::Seek>,
    l: &DecodeLimits,
) -> Result<(DynamicImage, u8)> {
    let mut reader = reader;
    reader.limits(image_limits(l));
    let mut dec = reader.into_decoder()?;
    let (w, h) = dec.dimensions();
    check_pixels(w, h, l)?;
    let orient = dec.orientation().map(|o| o.to_exif()).unwrap_or(1);
    let img = DynamicImage::from_decoder(dec)?;
    Ok((img, orient))
}

fn decode_file(path: &Path, l: &DecodeLimits) -> Result<(DynamicImage, u8)> {
    let r = ImageReader::open(path)
        .map_err(|e| MediaError::io(path, e))?
        .with_guessed_format()
        .map_err(|e| MediaError::io(path, e))?;
    if r.format().is_none() {
        return Err(MediaError::Unsupported(format!(
            "unrecognised image format: {}",
            path.display()
        )));
    }
    decode_limited(r, l)
}

fn decode_jpeg_bytes(b: &[u8], l: &DecodeLimits) -> Result<(DynamicImage, u8)> {
    let info = parse_jpeg_header(b).ok_or_else(|| MediaError::Decode("invalid JPEG".into()))?;
    check_pixels(info.width, info.height, l)?;
    let mut r = ImageReader::new(Cursor::new(b));
    r.set_format(image::ImageFormat::Jpeg);
    decode_limited(r, l)
}

/// Generates a thumbnail synchronously (used by the service workers; also usable directly).
///
/// * JPEG: EXIF embedded thumbnail if it is ≥ `size` and has the same aspect ratio, else a
///   full decode + fast box-downscale.
/// * RAW: smallest embedded preview ≥ `size` (largest otherwise).
/// * PNG/TIFF/WebP/GIF/BMP: `image` crate decode with pixel/alloc limits.
/// * Video: representative frame via ffmpeg (requires `ff`).
/// * HEIC/AVIF/PSD etc.: `Unsupported` (WIC fallback is a later phase).
pub fn generate_thumbnail(
    path: &Path,
    size: u32,
    limits: &DecodeLimits,
    ff: Option<&FfTools>,
    video_samples: usize,
    cancel: Option<&CancelToken>,
) -> Result<ThumbImage> {
    let size = size.clamp(16, 4096);
    let ext = ext_of(path);
    let check = || -> Result<()> {
        match cancel {
            Some(c) => Ok(c.check()?),
            None => Ok(()),
        }
    };

    if media_kind_for_ext(&ext) == MediaKind::Video {
        let ff = ff.ok_or(MediaError::ToolMissing("ffmpeg"))?;
        let frame = ff.representative_frame(path, size, video_samples, cancel)?;
        let img = fit(DynamicImage::ImageRgba8(frame), size);
        return Ok(ThumbImage::from_rgba(img));
    }

    let head = bin_util::read_prefix(path, 128)?;
    if head.is_empty() {
        return Err(MediaError::corrupt("empty file"));
    }
    let fmt = detect_format(&head, &ext);
    let (img, orientation) = match fmt {
        ImageFormat::Jpeg => {
            let prefix = bin_util::read_jpeg_header_prefix(path)?;
            let main = parse_jpeg_header(&prefix);
            if let Some(m) = main {
                check_pixels(m.width, m.height, limits)?;
            }
            let exif = image_meta::jpeg_exif_from_prefix(&prefix);
            let exif_orient = exif
                .as_ref()
                .and_then(|e| e.get_field(exif::Tag::Orientation, exif::In::PRIMARY))
                .and_then(|f| f.value.get_uint(0))
                .filter(|o| (1..=8).contains(o))
                .map(|o| o as u8);
            let thumb = exif.as_ref().and_then(|e| {
                let t = image_meta::exif_thumbnail(e)?;
                let ti = parse_jpeg_header(t)?;
                let m = main?;
                let ar_t = ti.width as f64 / ti.height.max(1) as f64;
                let ar_m = m.width as f64 / m.height.max(1) as f64;
                (ti.width.max(ti.height) >= size && (ar_t / ar_m - 1.0).abs() < 0.03)
                    .then(|| decode_jpeg_bytes(t, limits).ok())
                    .flatten()
            });
            check()?;
            match thumb {
                Some((img, _)) => (img, exif_orient.unwrap_or(1)),
                None => {
                    let (img, o) = decode_file(path, limits)?;
                    (img, exif_orient.unwrap_or(o))
                }
            }
        }
        ImageFormat::TiffRaw | ImageFormat::Cr3 | ImageFormat::Raf => {
            let p = raw_preview::extract_preview(path, PreviewPick::AtLeast(size))?;
            check()?;
            let (img, _) = decode_jpeg_bytes(&p.jpeg, limits)?;
            (img, p.orientation as u8)
        }
        ImageFormat::Heif => {
            return Err(MediaError::Unsupported(
                "HEIC/HEIF/AVIF decoding is not available yet".into(),
            ));
        }
        _ => decode_file(path, limits)?,
    };
    check()?;
    let mut out = DynamicImage::ImageRgba8(fit(img, size));
    if let Some(o) = image::metadata::Orientation::from_exif(orientation)
        && orientation != 1
    {
        out.apply_orientation(o);
    }
    Ok(ThumbImage::from_rgba(out.to_rgba8()))
}

/// Downscales so the long edge is ≤ `size` (never upscales): integer box reduction to
/// within 2× of the target, then a Triangle filter for the final step.
pub fn fit(img: DynamicImage, size: u32) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    let long = w.max(h);
    if long <= size {
        return img.to_rgba8();
    }
    // Target from the original dimensions (box reduction floors and would skew aspect).
    let scale = size as f64 / long as f64;
    let tw = ((w as f64 * scale).round() as u32).clamp(1, size);
    let th = ((h as f64 * scale).round() as u32).clamp(1, size);
    let k = long / size;
    let img = if k >= 2 { box_reduce(img, k) } else { img };
    let (w2, h2) = (img.width(), img.height());
    let rgba = img.to_rgba8();
    if (tw, th) == (w2, h2) {
        return rgba;
    }
    image::imageops::resize(&rgba, tw, th, image::imageops::FilterType::Triangle)
}

/// Area-average reduction by an integer factor `k`, on 8-bit buffers in their native layout.
fn box_reduce(img: DynamicImage, k: u32) -> DynamicImage {
    match img {
        DynamicImage::ImageRgb8(b) => {
            let (w, h) = b.dimensions();
            let (ow, oh, data) = box_reduce_raw(b.as_raw(), w, h, 3, k);
            image::RgbImage::from_raw(ow, oh, data)
                .map(DynamicImage::ImageRgb8)
                .unwrap_or_else(|| DynamicImage::ImageRgb8(b))
        }
        DynamicImage::ImageRgba8(b) => {
            let (w, h) = b.dimensions();
            let (ow, oh, data) = box_reduce_raw(b.as_raw(), w, h, 4, k);
            RgbaImage::from_raw(ow, oh, data)
                .map(DynamicImage::ImageRgba8)
                .unwrap_or_else(|| DynamicImage::ImageRgba8(b))
        }
        DynamicImage::ImageLuma8(b) => {
            let (w, h) = b.dimensions();
            let (ow, oh, data) = box_reduce_raw(b.as_raw(), w, h, 1, k);
            image::GrayImage::from_raw(ow, oh, data)
                .map(DynamicImage::ImageLuma8)
                .unwrap_or_else(|| DynamicImage::ImageLuma8(b))
        }
        other => {
            let b = other.to_rgba8();
            let (w, h) = b.dimensions();
            let (ow, oh, data) = box_reduce_raw(b.as_raw(), w, h, 4, k);
            RgbaImage::from_raw(ow, oh, data)
                .map(DynamicImage::ImageRgba8)
                .unwrap_or_else(|| DynamicImage::ImageRgba8(b))
        }
    }
}

fn box_reduce_raw(src: &[u8], w: u32, h: u32, c: usize, k: u32) -> (u32, u32, Vec<u8>) {
    let k = k.max(1);
    let (ow, oh) = ((w / k).max(1), (h / k).max(1));
    let (w, ku) = (w as usize, k as usize);
    let (kx, ky) = (ku.min(w), ku.min(h as usize));
    let row_len = w * c;
    let mut out = vec![0u8; ow as usize * oh as usize * c];
    let mut acc = vec![0u32; ow as usize * c];
    let div = (kx * ky) as u32;
    let half = div / 2;
    for oy in 0..oh as usize {
        acc.iter_mut().for_each(|a| *a = 0);
        for dy in 0..ky {
            let y = oy * ky + dy;
            let Some(row) = src.get(y * row_len..(y + 1) * row_len) else {
                break;
            };
            for ox in 0..ow as usize {
                let base = ox * kx * c;
                let a = &mut acc[ox * c..ox * c + c];
                for px in row[base..base + kx * c].chunks_exact(c) {
                    for ch in 0..c {
                        a[ch] += px[ch] as u32;
                    }
                }
            }
        }
        let orow = &mut out[oy * ow as usize * c..(oy + 1) * ow as usize * c];
        for (o, a) in orow.iter_mut().zip(acc.iter()) {
            *o = ((a + half) / div) as u8;
        }
    }
    (ow, oh, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_reduce_averages() {
        let src: Vec<u8> = vec![0, 100, 200, 255];
        let (w, h, d) = box_reduce_raw(&src, 2, 2, 1, 2);
        assert_eq!((w, h), (1, 1));
        assert_eq!(d, vec![139]);
    }

    #[test]
    fn fit_never_upscales_and_keeps_aspect() {
        let img = DynamicImage::new_rgb8(100, 50);
        let t = fit(img, 256);
        assert_eq!(t.dimensions(), (100, 50));
        let img = DynamicImage::new_rgb8(6000, 4000);
        let t = fit(img, 256);
        assert_eq!(t.dimensions(), (256, 171));
        let img = DynamicImage::new_rgb8(4000, 6000);
        let t = fit(img, 300);
        assert_eq!(t.dimensions(), (200, 300));
    }

    #[test]
    fn mem_cache_respects_budget() {
        let mut c = MemCache {
            lru: LruCache::unbounded(),
            bytes: 0,
            budget: 3 * (400 + std::mem::size_of::<ThumbImage>()),
        };
        for i in 0..10u8 {
            let img = Arc::new(ThumbImage {
                width: 10,
                height: 10,
                rgba: vec![0; 400],
            });
            c.insert([i; 32], img);
        }
        assert_eq!(c.lru.len(), 3);
        assert!(c.bytes <= c.budget);
        assert!(c.get(&[9; 32]).is_some());
        assert!(c.get(&[0; 32]).is_none());
    }

    #[test]
    fn publish_never_replaces_an_existing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("entry.jpg");
        let tmp = dir.path().join("entry.tmp1");
        std::fs::write(&target, b"first").unwrap();
        std::fs::write(&tmp, b"second").unwrap();
        publish_new(&tmp, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"first");
        assert!(!tmp.exists());

        let fresh = dir.path().join("fresh.jpg");
        std::fs::write(&tmp, b"second").unwrap();
        publish_new(&tmp, &fresh).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"second");
        assert!(!tmp.exists());
    }

    #[test]
    fn disk_cache_round_trips_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let img =
            ThumbImage::from_rgba(RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 255])));
        let key = [7u8; 32];
        store_cached(dir.path(), &key, &img, 85).unwrap();
        store_cached(dir.path(), &key, &img, 85).unwrap();
        let back = load_cached(dir.path(), &key).unwrap();
        assert_eq!((back.width, back.height), (8, 8));
        let (jpg, _) = cache_paths(dir.path(), &key);
        let names: Vec<_> = std::fs::read_dir(jpg.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names.len(),
            1,
            "only the published entry remains: {names:?}"
        );
    }

    /// A PNG whose IHDR claims `w`×`h`: enough to exercise the declared-size limit.
    fn png_claiming(w: u32, h: u32) -> Vec<u8> {
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend((data.len() as u32).to_be_bytes());
            let start = out.len();
            out.extend(kind);
            out.extend(data);
            let mut crc = 0xFFFF_FFFFu32;
            for b in &out[start..] {
                crc ^= *b as u32;
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        0xEDB8_8320 ^ (crc >> 1)
                    } else {
                        crc >> 1
                    };
                }
            }
            out.extend((!crc).to_be_bytes());
        }
        let mut ihdr = Vec::new();
        ihdr.extend(w.to_be_bytes());
        ihdr.extend(h.to_be_bytes());
        ihdr.extend([8, 2, 0, 0, 0]); // 8-bit RGB, deflate, no filter, no interlace
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(
            &mut png,
            b"IDAT",
            &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
        );
        chunk(&mut png, b"IEND", &[]);
        png
    }

    #[test]
    fn enormous_declared_png_is_rejected_before_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.png");
        std::fs::write(&path, png_claiming(60_000, 60_000)).unwrap();
        let err =
            generate_thumbnail(&path, 256, &DecodeLimits::default(), None, 0, None).unwrap_err();
        assert!(
            matches!(
                err,
                MediaError::TooLarge {
                    width: 60_000,
                    height: 60_000,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn requests_from_an_older_generation_are_refused() {
        let svc = ThumbnailService::new(ThumbConfig {
            cache_dir: None,
            ff_tools: None,
            ..Default::default()
        });
        svc.cancel_stale(2);
        let status = svc.request(ThumbRequest::new("missing.jpg", 128).generation(1));
        assert_eq!(status, RequestStatus::Stale);
        assert_eq!(svc.current_generation(), 2);
    }
}
