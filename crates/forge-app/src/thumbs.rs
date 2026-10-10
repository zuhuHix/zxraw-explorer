//! UI side of the thumbnail pipeline: turns `forge_media` results into GPU textures,
//! bounded by a byte budget, with per-frame upload limits so scrolling stays smooth.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_core::FileEntry;
use forge_jobs::Priority;
use forge_media::{ThumbConfig, ThumbImage, ThumbRequest, ThumbnailService};
use lru::LruCache;

use crate::views::{Annotation, ThumbProvider};

/// Thumbnails are generated at a few fixed sizes so changing the zoom slider reuses them.
const BUCKETS: [u32; 4] = [128, 256, 384, 512];
const MAX_UPLOADS_PER_FRAME: usize = 24;

pub fn bucket_for(display_px: u32) -> u32 {
    BUCKETS
        .iter()
        .copied()
        .find(|b| *b >= display_px)
        .unwrap_or(*BUCKETS.last().expect("non-empty"))
}

type Key = (PathBuf, u32);

pub struct ThumbManager {
    service: ThumbnailService,
    textures: LruCache<Key, egui::TextureHandle>,
    tex_bytes: usize,
    tex_budget: usize,
    pending: HashSet<Key>,
    failed: HashSet<Key>,
    ready: Vec<(Key, Arc<ThumbImage>)>,
    generation: u64,
    context_key: Vec<PathBuf>,
    pub annotations: HashMap<PathBuf, Annotation>,
}

impl ThumbManager {
    pub fn new(cache_dir: PathBuf, mem_mb: u64, ffmpeg_dir: Option<&Path>) -> Self {
        let cfg = ThumbConfig {
            cache_dir: Some(cache_dir),
            memory_budget_bytes: (mem_mb as usize / 2).max(32) * 1024 * 1024,
            ff_tools: Some(forge_media::FfTools::discover_with(ffmpeg_dir)),
            ..Default::default()
        };
        Self {
            service: ThumbnailService::new(cfg),
            textures: LruCache::unbounded(),
            tex_bytes: 0,
            tex_budget: (mem_mb as usize / 2).max(32) * 1024 * 1024,
            pending: HashSet::new(),
            failed: HashSet::new(),
            ready: Vec::new(),
            generation: 1,
            context_key: Vec::new(),
            annotations: HashMap::new(),
        }
    }

    pub fn service(&self) -> &ThumbnailService {
        &self.service
    }

    /// Call once per frame with the folders currently on screen. When they change, queued
    /// work for the old folders is dropped so the new view gets the workers immediately.
    pub fn begin_frame(&mut self, folders: Vec<PathBuf>) -> bool {
        if folders != self.context_key {
            self.context_key = folders;
            self.generation += 1;
            self.service.cancel_stale(self.generation);
            self.pending.clear();
        }
        let mut changed = false;
        while let Ok(r) = self.service.results().try_recv() {
            let key = (r.path, r.size);
            self.pending.remove(&key);
            match r.image {
                Ok(img) => self.ready.push((key, img)),
                Err(e) => {
                    tracing::debug!("thumbnail failed: {e}");
                    self.failed.insert(key);
                }
            }
            changed = true;
        }
        changed || !self.ready.is_empty()
    }

    fn upload(&mut self, ctx: &egui::Context, key: Key, img: &ThumbImage) -> egui::TextureHandle {
        let ci = egui::ColorImage::from_rgba_unmultiplied(
            [img.width as usize, img.height as usize],
            &img.rgba,
        );
        let tex = ctx.load_texture(
            format!("thumb:{}:{}", key.0.display(), key.1),
            ci,
            egui::TextureOptions::LINEAR,
        );
        self.tex_bytes += img.byte_size();
        if let Some((_, old)) = self.textures.push(key, tex.clone()) {
            self.tex_bytes = self.tex_bytes.saturating_sub(tex_size(&old));
        }
        while self.tex_bytes > self.tex_budget {
            match self.textures.pop_lru() {
                Some((_, t)) => self.tex_bytes = self.tex_bytes.saturating_sub(tex_size(&t)),
                None => break,
            }
        }
        tex
    }

    /// Upload a few finished thumbnails per frame (GPU uploads are the expensive part).
    pub fn upload_ready(&mut self, ctx: &egui::Context) -> bool {
        let n = self.ready.len().min(MAX_UPLOADS_PER_FRAME);
        let batch: Vec<_> = self.ready.drain(..n).collect();
        for (key, img) in batch {
            let _ = self.upload(ctx, key, &img);
        }
        if !self.ready.is_empty() {
            ctx.request_repaint();
        }
        n > 0
    }

    pub fn busy(&self) -> bool {
        !self.pending.is_empty() || !self.ready.is_empty()
    }

    /// Forget failures (e.g. after installing ffmpeg) and textures for a folder that changed.
    pub fn invalidate_dir(&mut self, dir: &Path) {
        self.failed.retain(|(p, _)| p.parent() != Some(dir));
        let stale: Vec<Key> = self
            .textures
            .iter()
            .filter(|((p, _), _)| p.parent() == Some(dir))
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            if let Some(t) = self.textures.pop(&k) {
                self.tex_bytes = self.tex_bytes.saturating_sub(tex_size(&t));
            }
        }
    }
}

fn tex_size(t: &egui::TextureHandle) -> usize {
    let [w, h] = t.size();
    w * h * 4
}

impl ThumbProvider for ThumbManager {
    fn thumb(
        &mut self,
        ui: &egui::Ui,
        entry: &FileEntry,
        size: u32,
    ) -> Option<egui::TextureHandle> {
        let px = (size as f32 * ui.ctx().pixels_per_point()).round() as u32;
        let bucket = bucket_for(px);
        let key = (entry.path.clone(), bucket);
        if let Some(t) = self.textures.get(&key) {
            return Some(t.clone());
        }
        if self.failed.contains(&key) {
            return None;
        }
        // Fall back to any other bucket already on the GPU while the right one loads.
        let fallback = BUCKETS
            .iter()
            .filter(|b| **b != bucket)
            .find_map(|b| self.textures.peek(&(entry.path.clone(), *b)).cloned());
        if !self.pending.contains(&key) {
            let req = ThumbRequest::new(entry.path.clone(), bucket)
                .priority(Priority::High)
                .generation(self.generation)
                .stat(entry.modified, entry.size);
            match self.service.request(req) {
                forge_media::RequestStatus::MemoryHit(img) => {
                    return Some(self.upload(ui.ctx(), key, &img));
                }
                forge_media::RequestStatus::Queued
                | forge_media::RequestStatus::AlreadyInFlight => {
                    self.pending.insert(key);
                }
                // Queue full / stale: try again on a later frame.
                _ => ui
                    .ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        fallback
    }

    fn annotation(&self, path: &Path) -> Option<Annotation> {
        self.annotations.get(path).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::bucket_for;

    #[test]
    fn buckets() {
        assert_eq!(bucket_for(10), 128);
        assert_eq!(bucket_for(128), 128);
        assert_eq!(bucket_for(200), 256);
        assert_eq!(bucket_for(5000), 512);
    }
}
