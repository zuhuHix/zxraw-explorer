//! Background decode + texture cache for the viewer.
//!
//! * Own bounded [`WorkerPool`] (2–4 threads). The current image is requested at `High`
//!   priority (quick 512 px preview + screen-resolution preview, plus a full-resolution
//!   decode on demand when zoomed past the screen preview), neighbours at `Low`, filmstrip
//!   thumbnails at `Normal`.
//! * Textures are uploaded on the worker thread (egui's texture manager is thread-safe), so
//!   the UI thread never blocks on decode or upload.
//! * Two byte-bounded LRUs: big previews (default 512 MB) and filmstrip thumbnails.
//! * Stale work: every request carries the navigation `generation`; per-request
//!   [`CancelToken`]s are cancelled as soon as a key is no longer wanted, queued tasks
//!   check their token before starting, and late results for keys that are neither
//!   current-generation nor still wanted are dropped.
//! * Tasks dropped from the pool queue (evicted by backpressure) report back through a
//!   drop guard, so nothing is left marked "in flight" forever.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{ColorImage, Rect, TextureHandle, TextureOptions, pos2};
use forge_jobs::{CancelToken, Priority, WorkerPool};
use forge_media::analysis::Sharpness;
use forge_media::{DecodeLimits, FfTools};
use image::{DynamicImage, RgbaImage};

use super::cache::ByteLru;
use super::decode::{self, DecodeError, Decoded};

pub const SMALL_EDGE: u32 = 192;
pub const QUICK_EDGE: u32 = 512;
pub const DEFAULT_BUDGET: usize = 512 * 1024 * 1024;
pub const THUMB_BUDGET: usize = 64 * 1024 * 1024;

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
}

struct Guard {
    tx: Sender<Msg>,
    key: Key,
    ctx: egui::Context,
    sent: bool,
}

impl Guard {
    fn send(mut self, m: Msg) {
        self.sent = true;
        let _ = self.tx.send(m);
        self.ctx.request_repaint();
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.sent {
            let _ = self.tx.send(Msg::Skipped { key: self.key });
            self.ctx.request_repaint();
        }
    }
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
            limits: DecodeLimits::default(),
            ff: FfTools::global().clone(),
        }
    }
}

pub struct Loader {
    pool: WorkerPool,
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
        Self {
            pool: WorkerPool::new("viewer-decode", cfg.threads.clamp(1, 8), 64),
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
            wanted: HashSet::new(),
            big: ByteLru::new(cfg.budget_bytes),
            small: ByteLru::new(cfg.thumb_budget_bytes),
            errors: HashMap::new(),
            sharp: HashMap::new(),
        }
    }

    #[allow(dead_code)] // via Viewer::set_memory_budget (host API)
    pub fn set_budget(&mut self, bytes: usize) {
        self.big.set_budget(bytes.max(16 * 1024 * 1024));
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
                    self.inflight.remove(&key);
                }
                Msg::Done {
                    key,
                    generation,
                    result,
                    sharp,
                } => {
                    self.inflight.remove(&key);
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
            {
                continue;
            }
            if !self.submit(key, prio) {
                break;
            }
        }
    }

    fn submit(&mut self, key: Key, prio: Priority) -> bool {
        let cancel = CancelToken::new();
        let guard = Guard {
            tx: self.tx.clone(),
            key,
            ctx: self.ctx.clone(),
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
        let task = move || {
            if tok.is_cancelled() {
                return; // guard reports Skipped
            }
            let res = match key.1 {
                Level::Small => decode::thumbnail(&path, SMALL_EDGE, &limits, &ff, &tok),
                Level::Quick => decode::thumbnail(&path, QUICK_EDGE, &limits, &ff, &tok),
                Level::Screen => decode::decode(&path, Some(screen_edge), &limits, &ff, &tok),
                Level::Full => decode::decode(&path, None, &limits, &ff, &tok)
                    // Never let one full decode exceed ~60 % of the cache budget.
                    .map(|d| decode::bound_pixels(d, (budget as u64 * 6 / 10) / 4)),
            };
            let sharp = match (&res, key.1) {
                (Ok(d), Level::Quick) if !d.video => Some(forge_media::analysis::sharpness(
                    &DynamicImage::ImageRgba8(d.rgba.clone()),
                )),
                _ => None,
            };
            let result = res.map(|d| upload(&ctx, &path, key.1, d, max_tex));
            guard.send(Msg::Done {
                key,
                generation,
                result,
                sharp,
            });
        };
        match self.pool.submit(prio, task) {
            Ok(()) => {
                self.inflight.insert(key, cancel);
                true
            }
            Err(_) => false,
        }
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        // Stop running decodes / ffmpeg children promptly; the pool then joins its threads.
        for tok in self.inflight.values() {
            tok.cancel();
        }
        self.pool.clear_queue();
    }
}

fn upload(ctx: &egui::Context, path: &std::path::Path, level: Level, d: Decoded, max_tex: usize) -> Tex {
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
                let ci = ColorImage::from_rgba_unmultiplied([tw as usize, th as usize], sub.as_raw());
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
