//! Background metadata reader: key EXIF for the info overlay and capture times for burst
//! grouping. A single shared worker scans one viewer generation at a time, so closing and
//! reopening the viewer cannot create an unbounded number of blocked filesystem threads.
//! Reads are synchronous: an OS read already blocked on a network path cannot be interrupted.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use forge_media::FfTools;
use forge_media::analysis::{BurstItem, group_bursts};

use super::decode::is_video;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoInfo {
    pub duration: Option<String>,
    pub codec: Option<String>,
    pub fps: Option<f64>,
    pub dims: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetaSummary {
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub focal_mm: Option<f64>,
    pub aperture: Option<f64>,
    pub shutter: Option<String>,
    pub iso: Option<u32>,
    pub exposure_comp: Option<f64>,
    pub date: Option<String>,
    pub dims: Option<(u32, u32)>,
    pub format: Option<String>,
    /// Seconds (any consistent epoch, sub-second precision when available).
    pub capture_time: Option<f64>,
    pub file_size: Option<u64>,
    pub is_video: bool,
    pub video: Option<VideoInfo>,
    pub error: Option<String>,
}

fn camera_name(make: Option<&str>, model: Option<&str>) -> Option<String> {
    let make = make.map(str::trim).filter(|s| !s.is_empty());
    let model = model.map(str::trim).filter(|s| !s.is_empty());
    match (make, model) {
        (Some(mk), Some(md)) => {
            // "Canon" + "Canon EOS R5" → "Canon EOS R5"
            let first = mk.split_whitespace().next().unwrap_or(mk);
            if md
                .to_ascii_lowercase()
                .starts_with(&first.to_ascii_lowercase())
            {
                Some(md.to_string())
            } else {
                Some(format!("{mk} {md}"))
            }
        }
        (a, b) => a.or(b).map(str::to_string),
    }
}

#[allow(dead_code)] // Synchronous metadata helper for non-cancellable callers.
pub fn read_summary(path: &std::path::Path, probe_video: bool, ff: &FfTools) -> MetaSummary {
    read_summary_inner(path, probe_video, ff, || false).unwrap_or_default()
}

fn read_summary_stoppable(
    path: &std::path::Path,
    probe_video: bool,
    ff: &FfTools,
    stop: &AtomicBool,
) -> Option<MetaSummary> {
    read_summary_inner(path, probe_video, ff, || stop.load(Ordering::Acquire))
}

/// What the sequential scan records for an online-only file: listing data only.
fn online_only_summary(path: &std::path::Path) -> MetaSummary {
    MetaSummary {
        is_video: is_video(path),
        file_size: std::fs::symlink_metadata(path).ok().map(|m| m.len()),
        ..Default::default()
    }
}

fn read_summary_inner(
    path: &std::path::Path,
    probe_video: bool,
    ff: &FfTools,
    stopped: impl Fn() -> bool,
) -> Option<MetaSummary> {
    if stopped() {
        return None;
    }
    let file_size = std::fs::metadata(path).ok().map(|m| m.len());
    if stopped() {
        return None;
    }
    if is_video(path) {
        let mut s = MetaSummary {
            is_video: true,
            file_size,
            ..Default::default()
        };
        if probe_video && ff.has_ffprobe() {
            if stopped() {
                return None;
            }
            match ff.probe(path) {
                Ok(m) => {
                    s.camera = camera_name(m.make.as_deref(), m.model.as_deref());
                    s.date = m.creation_time.clone();
                    s.dims = m.width.zip(m.height);
                    s.video = Some(VideoInfo {
                        duration: m.duration_s.map(forge_media::video::format_duration),
                        codec: m.codec.clone(),
                        fps: m.fps,
                        dims: m.width.zip(m.height),
                    });
                }
                Err(e) => s.error = Some(e.to_string()),
            }
        }
        return Some(s);
    }
    if stopped() {
        return None;
    }
    Some(match forge_media::read_image_metadata(path) {
        Ok(m) => MetaSummary {
            camera: camera_name(m.make.as_deref(), m.model.as_deref()),
            lens: m.lens.clone(),
            focal_mm: m.focal_length_mm,
            aperture: m.aperture,
            shutter: m.shutter.clone().or_else(|| {
                m.exposure_time_s
                    .and_then(forge_media::image_meta::format_shutter)
            }),
            iso: m.iso,
            exposure_comp: m.exposure_compensation,
            date: m.date_taken.clone(),
            dims: m.display_dimensions(),
            format: m.format.map(|f| format!("{f:?}")),
            capture_time: m
                .capture_timestamp
                .map(|t| t as f64 + m.capture_millis.unwrap_or(0) as f64 / 1000.0),
            file_size,
            is_video: false,
            video: None,
            error: None,
        },
        Err(e) => MetaSummary {
            file_size,
            error: Some(e.to_string()),
            ..Default::default()
        },
    })
}

/// Burst membership per item: `Some((position 1-based, group length))` for groups > 1.
pub fn burst_positions(
    metas: &[Option<Arc<MetaSummary>>],
    max_gap_secs: f64,
) -> Vec<Option<(u32, u32)>> {
    let items: Vec<BurstItem> = metas
        .iter()
        .map(|m| BurstItem {
            capture_time: m.as_ref().and_then(|m| m.capture_time).unwrap_or(f64::NAN),
            camera: m.as_ref().and_then(|m| m.camera.clone()),
        })
        .collect();
    let mut out = vec![None; metas.len()];
    for g in group_bursts(&items, max_gap_secs) {
        if g.len() < 2 {
            continue;
        }
        for (pos, &i) in g.iter().enumerate() {
            out[i] = Some((pos as u32 + 1, g.len() as u32));
        }
    }
    out
}

pub struct MetaScanner {
    prio: Sender<usize>,
    rx: Receiver<(usize, MetaSummary, bool)>,
    stop: Arc<AtomicBool>,
    pub metas: Vec<Option<Arc<MetaSummary>>>,
    full: Vec<bool>,
    requested: Vec<bool>,
    pub done: usize,
}

struct MetaJob {
    paths: Arc<Vec<PathBuf>>,
    start: usize,
    ff: FfTools,
    prio: Receiver<usize>,
    tx: Sender<(usize, MetaSummary, bool)>,
    stop: Arc<AtomicBool>,
    ctx: egui::Context,
}

fn shared_meta_queue() -> &'static Sender<MetaJob> {
    static QUEUE: OnceLock<Sender<MetaJob>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (tx, rx) = unbounded::<MetaJob>();
        let spawn = std::thread::Builder::new()
            .name("viewer-meta".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    if !job.stop.load(Ordering::Acquire) {
                        scan(
                            job.paths, job.start, job.ff, job.prio, job.tx, job.stop, job.ctx,
                        );
                    }
                }
            });
        if let Err(e) = spawn {
            tracing::error!("viewer: cannot start shared metadata thread: {e}");
        }
        tx
    })
}

impl MetaScanner {
    pub fn new(paths: Arc<Vec<PathBuf>>, start: usize, ff: FfTools, ctx: egui::Context) -> Self {
        let (prio_tx, prio_rx) = unbounded::<usize>();
        let (tx, rx) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let n = paths.len();
        let job = MetaJob {
            paths,
            start,
            ff,
            prio: prio_rx,
            tx,
            stop: stop.clone(),
            ctx,
        };
        if shared_meta_queue().send(job).is_err() {
            tracing::error!("viewer: shared metadata thread is unavailable");
        }
        Self {
            prio: prio_tx,
            rx,
            stop,
            metas: vec![None; n],
            full: vec![false; n],
            requested: vec![false; n],
            done: 0,
        }
    }

    /// Ask for item `i` ahead of the sequential scan (with a full video probe).
    pub fn want(&mut self, i: usize) {
        if i < self.full.len() && !self.full[i] && !self.requested[i] {
            self.requested[i] = true;
            let _ = self.prio.send(i);
        }
    }

    /// Drains results; returns true if anything arrived.
    pub fn pump(&mut self) -> bool {
        let mut any = false;
        while let Ok((i, m, full)) = self.rx.try_recv() {
            if i >= self.metas.len() {
                continue;
            }
            if self.metas[i].is_none() {
                self.done += 1;
            }
            if full || self.metas[i].is_none() {
                self.metas[i] = Some(Arc::new(m));
            }
            self.full[i] |= full;
            any = true;
        }
        any
    }

    pub fn get(&self, i: usize) -> Option<&Arc<MetaSummary>> {
        self.metas.get(i).and_then(|m| m.as_ref())
    }

    pub fn complete(&self) -> bool {
        self.done >= self.metas.len()
    }
}

impl Drop for MetaScanner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn scan(
    paths: Arc<Vec<PathBuf>>,
    start: usize,
    ff: FfTools,
    prio: Receiver<usize>,
    tx: Sender<(usize, MetaSummary, bool)>,
    stop: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    let n = paths.len();
    let mut seen = vec![false; n];
    let mut order = (0..n).map(|k| (start + k) % n.max(1));
    let mut since_repaint = 0usize;
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let (i, full) = match prio.try_recv() {
            Ok(i) => (i, true),
            Err(_) => match order.by_ref().find(|&i| !seen[i]) {
                Some(i) => (i, false),
                None => match prio.recv_timeout(Duration::from_millis(250)) {
                    Ok(i) => (i, true),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(_) => return,
                },
            },
        };
        if i >= n {
            continue;
        }
        if seen[i] && !full {
            continue;
        }
        seen[i] = true;
        let p = &paths[i];
        // Check before every metadata operation. An individual filesystem call remains
        // synchronous and cannot be interrupted once the OS blocks inside it.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if !full && !forge_core::cloud::background_read_allowed(p) {
                // Online-only cloud file: only the image being viewed may download it.
                return Some(online_only_summary(p));
            }
            read_summary_stoppable(p, full, &ff, &stop)
        }))
        .unwrap_or_else(|_| {
            Some(MetaSummary {
                error: Some("metadata reader crashed on this file".into()),
                ..Default::default()
            })
        });
        let Some(r) = r else {
            return;
        };
        if tx.send((i, r, full)).is_err() {
            return;
        }
        since_repaint += 1;
        if full || since_repaint >= 64 {
            since_repaint = 0;
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_names() {
        assert_eq!(
            camera_name(Some("Canon"), Some("Canon EOS R5")).as_deref(),
            Some("Canon EOS R5")
        );
        assert_eq!(
            camera_name(Some("SONY"), Some("ILCE-7M4")).as_deref(),
            Some("SONY ILCE-7M4")
        );
        assert_eq!(camera_name(None, Some("X100V")).as_deref(), Some("X100V"));
        assert_eq!(camera_name(Some(" "), None), None);
    }

    #[test]
    fn burst_badges() {
        let m = |t: f64| {
            Some(Arc::new(MetaSummary {
                capture_time: Some(t),
                camera: Some("A".into()),
                ..Default::default()
            }))
        };
        let metas = vec![m(0.0), m(0.3), m(0.6), m(100.0), None];
        let b = burst_positions(&metas, 1.0);
        assert_eq!(
            b,
            vec![Some((1, 3)), Some((2, 3)), Some((3, 3)), None, None]
        );
    }

    #[test]
    fn stopped_scanner_does_not_start_another_file_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-read.jpg");
        let stop = AtomicBool::new(true);
        assert!(read_summary_stoppable(&path, false, &FfTools::default(), &stop).is_none());
    }
}
