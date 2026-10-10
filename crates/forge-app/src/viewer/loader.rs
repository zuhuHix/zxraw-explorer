//! Background decode + texture cache for the viewer.
//!
//! * Own bounded [`WorkerPool`] (2–4 threads). The current image is requested at `High`
//!   priority (quick 512 px preview + screen-resolution preview, plus a full-resolution
//!   decode on demand when zoomed past the screen preview), neighbours at `Low`, filmstrip
//!   thumbnails at `Normal`.
//! * Textures are uploaded on the worker thread (egui's texture manager is thread-safe), so
//!   the UI thread never blocks on decode or upload.
//! * Two byte-bounded LRUs plus a header-dimension decode budget; oversized images run alone.
//! * Stale work: every request carries the navigation `generation`; per-request
//!   [`CancelToken`]s are cancelled as soon as a key is no longer wanted, queued tasks
//!   check their token before starting, and late results for keys that are neither
//!   current-generation nor still wanted are dropped.
//! * Tasks dropped from the pool queue (evicted by backpressure) report back through a
//!   drop guard, so nothing is left marked "in flight" forever.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{ColorImage, Rect, TextureHandle, TextureOptions, pos2};
use forge_jobs::{CancelToken, Priority, WorkerPool};
use forge_media::analysis::Sharpness;
use forge_media::{DecodeLimits, FfTools};
use image::{DynamicImage, RgbaImage};
use parking_lot::Mutex;

use super::cache::ByteLru;
use super::decode::{self, DecodeError, Decoded};

pub const SMALL_EDGE: u32 = 192;
pub const QUICK_EDGE: u32 = 512;
pub const DEFAULT_BUDGET: usize = 512 * 1024 * 1024;
pub const THUMB_BUDGET: usize = 64 * 1024 * 1024;
const MAX_INFLIGHT_BUDGET: u64 = 1536 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    /// Filmstrip thumbnail (~192 px).
    Small,
    /// Fast low-res placeholder (512 px) — also used for the sharpness estimate.
    Quick,
    /// Screen-resolution preview.
    Screen,
    /// Full resolution (on demand, for 100 % and beyond).
    Full,
}

pub type Key = (usize, Level);

/// An uploaded image, possibly split into tiles if larger than the GPU's max texture side.
pub struct Tex {
    /// Tiles with their placement in normalised (0..1) image coordinates.
    pub tiles: Vec<(TextureHandle, Rect)>,
    pub size: [u32; 2],
    pub natural: [u32; 2],
    pub video: bool,
    pub bytes: usize,
}

enum Msg {
    Done {
        key: Key,
        generation: u64,
        result: Result<Tex, DecodeError>,
        sharp: Option<Sharpness>,
    },
    Skipped {
        key: Key,
    },
    Probed {
        item: usize,
        bytes: u64,
    },
    ProbeSkipped {
        item: usize,
    },
}

struct Completion {
    closed: AtomicBool,
    send_lock: Mutex<()>,
}

impl Completion {
    fn close(&self) {
        let _lock = self.send_lock.lock();
        self.closed.store(true, Ordering::Release);
    }

    fn send(&self, tx: &Sender<Msg>, messages: impl IntoIterator<Item = Msg>, ctx: &egui::Context) {
        let _lock = self.send_lock.lock();
        if !self.closed.load(Ordering::Acquire) {
            for message in messages {
                let _ = tx.send(message);
            }
            ctx.request_repaint();
        }
    }
}

struct Guard {
    tx: Sender<Msg>,
    keys: Vec<Key>,
    ctx: egui::Context,
    completion: Arc<Completion>,
    sent: bool,
}

impl Guard {
    fn send(mut self, messages: impl IntoIterator<Item = Msg>) {
        self.sent = true;
        self.completion.send(&self.tx, messages, &self.ctx);
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.sent {
            self.completion.send(
                &self.tx,
                self.keys.iter().copied().map(|key| Msg::Skipped { key }),
                &self.ctx,
            );
        }
    }
}

struct ProbeGuard {
    tx: Sender<Msg>,
    item: usize,
    ctx: egui::Context,
    completion: Arc<Completion>,
    sent: bool,
}

impl ProbeGuard {
    fn send(mut self, bytes: u64) {
        self.sent = true;
        self.completion.send(
            &self.tx,
            [Msg::Probed {
                item: self.item,
                bytes,
            }],
            &self.ctx,
        );
    }
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        if !self.sent {
            self.completion
                .send(&self.tx, [Msg::ProbeSkipped { item: self.item }], &self.ctx);
        }
    }
}

#[derive(Clone)]
struct Pending {
    keys: Vec<Key>,
    priority: Priority,
}

struct JobReservation {
    keys: Vec<Key>,
    bytes: u64,
}

fn budget_allows(active_bytes: u64, active_jobs: usize, estimate: u64, budget: u64) -> bool {
    active_jobs == 0 || active_bytes.saturating_add(estimate.max(1)) <= budget
}

#[derive(Clone)]
pub struct LoaderConfig {
    pub threads: usize,
    pub budget_bytes: usize,
    pub thumb_budget_bytes: usize,
    pub limits: DecodeLimits,
    pub ff: FfTools,
}

impl Default for LoaderConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            threads: (cores / 2).clamp(2, 4),
            budget_bytes: DEFAULT_BUDGET,
            thumb_budget_bytes: THUMB_BUDGET,
            limits: DecodeLimits {
                max_input_pixels: 150_000_000,
                ..DecodeLimits::default()
            },
            ff: FfTools::global().clone(),
        }
    }
}

pub struct Loader {
    pool: Option<WorkerPool>,
    completion: Arc<Completion>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    paths: Arc<Vec<PathBuf>>,
    limits: DecodeLimits,
    ff: Arc<FfTools>,
    max_tex: usize,
    pub screen_edge: u32,
    generation: u64,
    inflight: HashMap<Key, CancelToken>,
    jobs: HashMap<u64, JobReservation>,
    job_for_key: HashMap<Key, u64>,
    next_job: u64,
    reserved_bytes: u64,
    decode_budget: u64,
    estimated_bytes: HashMap<usize, u64>,
    probing: HashSet<usize>,
    pending: Vec<Pending>,
    wanted: HashSet<Key>,
    big: ByteLru<Key, Arc<Tex>>,
    small: ByteLru<usize, Arc<Tex>>,
    errors: HashMap<Key, DecodeError>,
    sharp: HashMap<usize, Sharpness>,
}

impl Loader {
    pub fn new(paths: Arc<Vec<PathBuf>>, cfg: LoaderConfig, ctx: &egui::Context) -> Self {
        let (tx, rx) = unbounded();
        let max_tex = ctx.input(|i| i.max_texture_side).max(1024);
        let decode_budget = ((cfg.budget_bytes as u64) / 4).clamp(1, MAX_INFLIGHT_BUDGET);
        Self {
            pool: Some(WorkerPool::new(
                "viewer-decode",
                cfg.threads.clamp(1, 8),
                64,
            )),
            completion: Arc::new(Completion {
                closed: AtomicBool::new(false),
                send_lock: Mutex::new(()),
            }),
            tx,
            rx,
            ctx: ctx.clone(),
            paths,
            limits: cfg.limits,
            ff: Arc::new(cfg.ff),
            max_tex,
            screen_edge: 2560,
            generation: 0,
            inflight: HashMap::new(),
            jobs: HashMap::new(),
            job_for_key: HashMap::new(),
            next_job: 1,
            reserved_bytes: 0,
            decode_budget,
            estimated_bytes: HashMap::new(),
            probing: HashSet::new(),
            pending: Vec::new(),
            wanted: HashSet::new(),
            big: ByteLru::new(cfg.budget_bytes),
            small: ByteLru::new(cfg.thumb_budget_bytes),
            errors: HashMap::new(),
            sharp: HashMap::new(),
        }
    }

    #[allow(dead_code)] // via Viewer::set_memory_budget (host API)
    pub fn set_budget(&mut self, bytes: usize) {
        let bytes = bytes.max(16 * 1024 * 1024);
        self.big.set_budget(bytes);
        self.decode_budget = ((bytes as u64) / 4).clamp(1, MAX_INFLIGHT_BUDGET);
    }

    #[allow(dead_code)]
    pub fn budget(&self) -> usize {
        self.big.budget()
    }

    /// Call when the user navigates: newer generation marks older results stale.
    pub fn bump_generation(&mut self) {
        self.generation += 1;
    }

    /// Drains finished work into the caches.
    pub fn pump(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Skipped { key } => {
                    self.finish_job(key);
                }
                Msg::Probed { item, bytes } => {
                    self.probing.remove(&item);
                    self.estimated_bytes.insert(item, bytes.max(1));
                }
                Msg::ProbeSkipped { item } => {
                    self.probing.remove(&item);
                }
                Msg::Done {
                    key,
                    generation,
                    result,
                    sharp,
                } => {
                    self.finish_job(key);
                    if let Some(s) = sharp {
                        self.sharp.insert(key.0, s);
                    }
                    if generation < self.generation && !self.wanted.contains(&key) {
                        continue; // stale and no longer needed: drop (frees the texture)
                    }
                    match result {
                        Ok(t) => {
                            let bytes = t.bytes;
                            if key.1 == Level::Small {
                                self.small.insert(key.0, Arc::new(t), bytes);
                            } else {
                                self.big.insert(key, Arc::new(t), bytes);
                            }
                        }
                        Err(DecodeError::Cancelled) => {}
                        Err(e) => {
                            self.errors.insert(key, e);
                        }
                    }
                }
            }
        }
        self.start_pending();
    }

    pub fn get(&mut self, key: Key) -> Option<Arc<Tex>> {
        if key.1 == Level::Small {
            self.small.get(&key.0).cloned()
        } else {
            self.big.get(&key).cloned()
        }
    }

    fn has(&self, key: Key) -> bool {
        if key.1 == Level::Small {
            self.small.contains(&key.0)
        } else {
            self.big.contains(&key)
        }
    }

    pub fn has_level(&self, item: usize, level: Level) -> bool {
        self.has((item, level))
    }

    /// Texture to draw at `need_px` physical pixels wide: the smallest loaded level that is
    /// at least that large (no GPU mipmaps, so a much larger texture would alias), else
    /// the largest loaded level.
    pub fn pick(&mut self, item: usize, need_px: f32) -> Option<(Level, Arc<Tex>)> {
        let mut best = None;
        for l in [Level::Small, Level::Quick, Level::Screen, Level::Full] {
            if let Some(t) = self.get((item, l)) {
                let enough = t.size[0] as f32 >= need_px * 0.98;
                best = Some((l, t));
                if enough && l != Level::Small {
                    break;
                }
            }
        }
        best
    }

    /// True natural size if a screen/full decode is loaded.
    pub fn natural(&self, item: usize) -> Option<[u32; 2]> {
        [Level::Full, Level::Screen]
            .into_iter()
            .find_map(|l| self.big.peek(&(item, l)).map(|t| t.natural))
    }

    /// Size of the best placeholder (quick / small) if any.
    pub fn placeholder_size(&self, item: usize) -> Option<([u32; 2], bool)> {
        self.big
            .peek(&(item, Level::Quick))
            .or_else(|| self.small.peek(&item))
            .map(|t| (t.size, t.video))
    }

    pub fn error(&self, key: Key) -> Option<&DecodeError> {
        self.errors.get(&key)
    }

    pub fn sharpness(&self, item: usize) -> Option<Sharpness> {
        self.sharp.get(&item).copied()
    }

    pub fn loading(&self, key: Key) -> bool {
        self.inflight.contains_key(&key)
    }

    /// Declares what is wanted this frame, in order of importance. Cancels in-flight work
    /// that is no longer wanted and submits what is missing.
    pub fn update(&mut self, requests: &[(Key, Priority)]) {
        self.wanted = requests.iter().map(|(k, _)| *k).collect();
        let requested: HashSet<Key> = self.wanted.clone();
        // Requests are a full declaration for this frame; rebuild waiting work so a
        // previously separate Quick/Screen request can be coalesced on the next frame.
        self.pending.clear();
        // Cancel work nobody wants any more. The entry stays until the task reports back
        // (Skipped/Done), so a still-running decode is not submitted twice.
        for (k, tok) in &self.inflight {
            if !self.wanted.contains(k) {
                tok.cancel();
            }
        }
        // Submit in reverse importance: the pool is LIFO within a priority.
        for &(key, prio) in requests.iter().rev() {
            if key.0 >= self.paths.len()
                || self.has(key)
                || self.errors.contains_key(&key)
                || self.inflight.contains_key(&key)
                || self.pending.iter().any(|job| job.keys.contains(&key))
            {
                continue;
            }
            let quick = (key.0, Level::Quick);
            let screen = (key.0, Level::Screen);
            let pair_requested = (key.1 == Level::Quick && requested.contains(&screen))
                || (key.1 == Level::Screen && requested.contains(&quick));
            if pair_requested {
                let other = if key.1 == Level::Quick { screen } else { quick };
                if self.inflight.contains_key(&other) {
                    continue;
                }
                if !self.has(other) && !self.errors.contains_key(&other) {
                    let keys = vec![quick, screen];
                    self.pending.push(Pending {
                        keys,
                        priority: prio,
                    });
                    continue;
                }
            }
            self.pending.push(Pending {
                keys: vec![key],
                priority: prio,
            });
        }
        self.start_pending();
    }

    fn finish_job(&mut self, key: Key) {
        let Some(job_id) = self.job_for_key.get(&key).copied() else {
            self.inflight.remove(&key);
            return;
        };
        let Some(job) = self.jobs.remove(&job_id) else {
            self.inflight.remove(&key);
            self.job_for_key.remove(&key);
            return;
        };
        self.reserved_bytes = self.reserved_bytes.saturating_sub(job.bytes);
        for key in job.keys {
            self.inflight.remove(&key);
            self.job_for_key.remove(&key);
        }
    }

    fn start_pending(&mut self) {
        let mut waiting = Vec::new();
        let mut pending = std::mem::take(&mut self.pending);
        pending.sort_by_key(|job| std::cmp::Reverse(job.priority));
        for mut job in pending {
            job.keys.retain(|key| {
                key.0 < self.paths.len()
                    && !self.has(*key)
                    && !self.errors.contains_key(key)
                    && !self.inflight.contains_key(key)
            });
            if job.keys.is_empty() {
                continue;
            }
            let item = job.keys[0].0;
            if self.inflight.keys().any(|key| key.0 == item) {
                waiting.push(job);
                continue;
            }
            let Some(&bytes) = self.estimated_bytes.get(&item) else {
                self.start_probe(item, job.priority);
                waiting.push(job);
                continue;
            };
            if !budget_allows(
                self.reserved_bytes,
                self.jobs.len(),
                bytes,
                self.decode_budget,
            ) {
                waiting.push(job);
                continue;
            }
            if !self.submit_job(&job.keys, job.priority, bytes) {
                waiting.push(job);
            }
        }
        self.pending = waiting;
    }

    fn start_probe(&mut self, item: usize, priority: Priority) {
        if self.estimated_bytes.contains_key(&item) || self.probing.contains(&item) {
            return;
        }
        let Some(pool) = self.pool.as_ref() else {
            return;
        };
        let path = self.paths[item].clone();
        let screen_edge = self.screen_edge;
        let guard = ProbeGuard {
            tx: self.tx.clone(),
            item,
            ctx: self.ctx.clone(),
            completion: self.completion.clone(),
            sent: false,
        };
        let task = move || {
            let bytes = decode::estimated_input_bytes(&path, screen_edge);
            guard.send(bytes);
        };
        if pool.submit(priority, task).is_ok() {
            self.probing.insert(item);
        }
    }

    fn submit_job(&mut self, keys: &[Key], prio: Priority, estimate: u64) -> bool {
        let Some(&key) = keys.first() else {
            return false;
        };
        let keys = keys.to_vec();
        let cancel = CancelToken::new();
        let guard = Guard {
            tx: self.tx.clone(),
            keys: keys.clone(),
            ctx: self.ctx.clone(),
            completion: self.completion.clone(),
            sent: false,
        };
        let path = self.paths[key.0].clone();
        let limits = self.limits;
        let ff = self.ff.clone();
        let ctx = self.ctx.clone();
        let max_tex = self.max_tex;
        let screen_edge = self.screen_edge;
        let generation = self.generation;
        let budget = self.big.budget();
        let tok = cancel.clone();
        let task_keys = keys.clone();
        let task = move || {
            if tok.is_cancelled() {
                return; // guard reports Skipped
            }
            let mut decoded = Vec::with_capacity(task_keys.len());
            if task_keys.contains(&(key.0, Level::Quick))
                && task_keys.contains(&(key.0, Level::Screen))
            {
                let quick_key = (key.0, Level::Quick);
                let screen_key = (key.0, Level::Screen);
                if decode::has_jpeg_thumbnail(&path, QUICK_EDGE) {
                    // forge_media's JPEG helper prefers a sufficiently large EXIF thumbnail.
                    let quick = decode::thumbnail(&path, QUICK_EDGE, &limits, &ff, &tok);
                    let screen = if tok.is_cancelled() {
                        Err(DecodeError::Cancelled)
                    } else {
                        decode::decode(&path, Some(screen_edge), &limits, &ff, &tok)
                    };
                    decoded.push((quick_key, quick));
                    decoded.push((screen_key, screen));
                } else {
                    let screen = decode::decode(&path, Some(screen_edge), &limits, &ff, &tok);
                    let quick = screen
                        .as_ref()
                        .map(|d| decode::quick_from_screen(d, QUICK_EDGE))
                        .map_err(Clone::clone);
                    decoded.push((quick_key, quick));
                    decoded.push((screen_key, screen));
                }
            } else {
                for &key in &task_keys {
                    let result = match key.1 {
                        Level::Small => decode::thumbnail(&path, SMALL_EDGE, &limits, &ff, &tok),
                        Level::Quick => decode::thumbnail(&path, QUICK_EDGE, &limits, &ff, &tok),
                        Level::Screen => {
                            decode::decode(&path, Some(screen_edge), &limits, &ff, &tok)
                        }
                        Level::Full => decode::decode(&path, None, &limits, &ff, &tok)
                            // Never let one full decode exceed ~60 % of the cache budget.
                            .map(|d| decode::bound_pixels(d, (budget as u64 * 6 / 10) / 4)),
                    };
                    decoded.push((key, result));
                }
            }

            let messages = decoded.into_iter().map(|(key, res)| {
                let sharp = match (&res, key.1) {
                    (Ok(d), Level::Quick) if !d.video => Some(forge_media::analysis::sharpness(
                        &DynamicImage::ImageRgba8(d.rgba.clone()),
                    )),
                    _ => None,
                };
                let result = res.map(|d| upload(&ctx, &path, key.1, d, max_tex));
                Msg::Done {
                    key,
                    generation,
                    result,
                    sharp,
                }
            });
            guard.send(messages);
        };
        let submitted = if let Some(pool) = self.pool.as_ref() {
            pool.submit(prio, task).is_ok()
        } else {
            false
        };
        if !submitted {
            return false;
        }
        let job_id = self.next_job;
        self.next_job = self.next_job.wrapping_add(1).max(1);
        self.reserved_bytes = self.reserved_bytes.saturating_add(estimate.max(1));
        self.jobs.insert(
            job_id,
            JobReservation {
                keys: keys.clone(),
                bytes: estimate.max(1),
            },
        );
        for key in keys {
            self.inflight.insert(key, cancel.clone());
            self.job_for_key.insert(key, job_id);
        }
        true
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.completion.close();
        for tok in self.inflight.values() {
            tok.cancel();
        }
        if let Some(pool) = self.pool.take() {
            pool.clear_queue();
            let result = std::thread::Builder::new()
                .name("viewer-decode-reaper".into())
                .spawn(move || drop(pool));
            if let Err(e) = result {
                tracing::error!("viewer: cannot start decode pool reaper: {e}");
            }
        }
    }
}

fn upload(
    ctx: &egui::Context,
    path: &std::path::Path,
    level: Level,
    d: Decoded,
    max_tex: usize,
) -> Tex {
    let opts = if level == Level::Full {
        // Crisp pixels when inspecting beyond 100 %.
        TextureOptions {
            magnification: egui::TextureFilter::Nearest,
            ..TextureOptions::LINEAR
        }
    } else {
        TextureOptions::LINEAR
    };
    let (w, h) = d.rgba.dimensions();
    let name = format!("viewer:{level:?}:{}", path.display());
    let mut tiles = Vec::new();
    let ms = max_tex as u32;
    if w <= ms && h <= ms {
        let ci = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], d.rgba.as_raw());
        tiles.push((
            ctx.load_texture(name, ci, opts),
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
        ));
    } else {
        let mut y = 0;
        while y < h {
            let th = ms.min(h - y);
            let mut x = 0;
            while x < w {
                let tw = ms.min(w - x);
                let sub: RgbaImage = image::imageops::crop_imm(&d.rgba, x, y, tw, th).to_image();
                let ci =
                    ColorImage::from_rgba_unmultiplied([tw as usize, th as usize], sub.as_raw());
                let r = Rect::from_min_max(
                    pos2(x as f32 / w as f32, y as f32 / h as f32),
                    pos2((x + tw) as f32 / w as f32, (y + th) as f32 / h as f32),
                );
                tiles.push((ctx.load_texture(format!("{name}#{x},{y}"), ci, opts), r));
                x += tw;
            }
            y += th;
        }
    }
    Tex {
        tiles,
        size: [w, h],
        natural: d.natural,
        video: d.video,
        bytes: w as usize * h as usize * 4 + 256,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_budget_serializes_large_work_but_always_allows_one() {
        let budget = 128_u64;
        assert!(budget_allows(0, 0, 1024, budget));
        assert!(budget_allows(32, 1, 96, budget));
        assert!(!budget_allows(32, 1, 97, budget));
        assert!(!budget_allows(u64::MAX, 1, 1, budget));
    }

    #[test]
    fn viewer_default_input_limit_is_150_megapixels() {
        assert_eq!(LoaderConfig::default().limits.max_input_pixels, 150_000_000);
    }

    #[test]
    fn stale_result_for_previous_item_is_not_cached_for_current_item() {
        let ctx = egui::Context::default();
        let paths = Arc::new(vec![PathBuf::from("old.png"), PathBuf::from("new.png")]);
        let mut loader = Loader::new(paths, LoaderConfig::default(), &ctx);
        loader.generation = 1;
        loader.wanted.insert((1, Level::Screen));
        loader
            .tx
            .send(Msg::Done {
                key: (0, Level::Screen),
                generation: 0,
                result: Err(DecodeError::Failed("stale".into())),
                sharp: None,
            })
            .unwrap();

        loader.pump();

        assert!(!loader.has_level(0, Level::Screen));
        assert!(!loader.has_level(1, Level::Screen));
        assert!(loader.error((0, Level::Screen)).is_none());
    }
}
