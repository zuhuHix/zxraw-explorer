//! Total running time of the selected videos and audio files for the status bar.
//!
//! Durations come from `ffprobe` on one background thread and are cached by path and
//! modification time, so the status bar only ever does hash lookups.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crossbeam_channel::{Receiver, Sender};
use forge_media::FfTools;
use parking_lot::Mutex;

/// At most this many files are probed for one selection; the rest are reported as unknown.
pub const MAX_PROBED: usize = 500;

type Key = (PathBuf, Option<SystemTime>);

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DurationTotal {
    /// Media files in the selection.
    pub files: usize,
    /// Sum of the durations known so far.
    pub seconds: f64,
    /// Files whose duration is not known (still probing, no ffprobe, or unreadable).
    pub unknown: usize,
    /// Probes still queued or running.
    pub pending: usize,
}

struct Request {
    key: Key,
    ffmpeg_dir: Option<PathBuf>,
}

#[derive(Default)]
struct Shared {
    /// `None` = probed, but no duration (unreadable or the tool is missing).
    done: HashMap<Key, Option<f64>>,
    queued: HashSet<Key>,
}

pub struct MediaDurations {
    shared: Arc<Mutex<Shared>>,
    tx: Option<Sender<Request>>,
    ctx: Option<egui::Context>,
}

impl Default for MediaDurations {
    fn default() -> Self {
        Self { shared: Arc::default(), tx: None, ctx: None }
    }
}

impl MediaDurations {
    /// Totals for `files`, queueing a probe for each file not seen before.
    pub fn total(
        &mut self,
        files: &[Key],
        ffmpeg_dir: Option<&Path>,
        ctx: &egui::Context,
    ) -> DurationTotal {
        let mut total = DurationTotal { files: files.len(), ..Default::default() };
        let mut to_queue = Vec::new();
        {
            let mut shared = self.shared.lock();
            for key in files.iter().take(MAX_PROBED) {
                match shared.done.get(key) {
                    Some(Some(seconds)) => total.seconds += seconds,
                    Some(None) => total.unknown += 1,
                    None => {
                        total.unknown += 1;
                        total.pending += 1;
                        if shared.queued.insert(key.clone()) {
                            to_queue.push(key.clone());
                        }
                    }
                }
            }
        }
        total.unknown += files.len().saturating_sub(MAX_PROBED);
        if !to_queue.is_empty() {
            let tx = self.sender(ctx);
            for key in to_queue {
                let _ = tx.send(Request { key, ffmpeg_dir: ffmpeg_dir.map(Path::to_path_buf) });
            }
        }
        total
    }

    fn sender(&mut self, ctx: &egui::Context) -> Sender<Request> {
        if self.ctx.is_none() {
            self.ctx = Some(ctx.clone());
        }
        if let Some(tx) = &self.tx {
            return tx.clone();
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        let shared = self.shared.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("media-durations".into())
            .spawn(move || worker(rx, shared, ctx));
        if spawned.is_err() {
            tracing::warn!("could not start the media duration thread");
        }
        self.tx = Some(tx.clone());
        tx
    }

    #[cfg(test)]
    fn insert(&self, key: Key, seconds: Option<f64>) {
        self.shared.lock().done.insert(key, seconds);
    }
}

fn worker(rx: Receiver<Request>, shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    let mut tools: Option<(Option<PathBuf>, FfTools)> = None;
    while let Ok(request) = rx.recv() {
        let ff = match &tools {
            Some((dir, ff)) if *dir == request.ffmpeg_dir => ff,
            _ => {
                let ff = FfTools::discover_with(request.ffmpeg_dir.as_deref());
                &tools.insert((request.ffmpeg_dir.clone(), ff)).1
            }
        };
        let seconds = ff
            .probe(&request.key.0)
            .ok()
            .and_then(|m| m.duration_s)
            .filter(|d| d.is_finite() && *d > 0.0);
        {
            let mut shared = shared.lock();
            shared.queued.remove(&request.key);
            shared.done.insert(request.key, seconds);
        }
        if rx.is_empty() {
            ctx.request_repaint();
        }
    }
}

/// `" · 3 videos, 12:04"` style suffix; empty when the selection has no media.
pub fn status_text(total: &DurationTotal) -> String {
    if total.files == 0 {
        return String::new();
    }
    let noun = if total.files == 1 { "media file" } else { "media files" };
    let known = total.files - total.unknown;
    if known == 0 {
        return if total.pending > 0 {
            format!("   ·   {} {noun}, measuring…", total.files)
        } else {
            format!("   ·   {} {noun}", total.files)
        };
    }
    let mut text = format!(
        "   ·   {} {noun}, {}",
        total.files,
        forge_media::video::format_duration(total.seconds)
    );
    if total.unknown > 0 {
        text.push_str(if total.pending > 0 { " so far…" } else { "+" });
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> Key {
        (PathBuf::from(name), None)
    }

    #[test]
    fn sums_known_durations_and_counts_unknown_ones() {
        let mut d = MediaDurations::default();
        d.insert(key("a.mp4"), Some(60.0));
        d.insert(key("b.mp4"), Some(30.5));
        d.insert(key("c.mp4"), None);
        let ctx = egui::Context::default();
        let total = d.total(&[key("a.mp4"), key("b.mp4"), key("c.mp4")], None, &ctx);
        assert_eq!(total.files, 3);
        assert_eq!(total.seconds, 90.5);
        assert_eq!(total.unknown, 1);
        assert_eq!(total.pending, 0);
    }

    #[test]
    fn files_past_the_probe_cap_are_unknown() {
        let mut d = MediaDurations::default();
        let files: Vec<Key> = (0..MAX_PROBED + 3).map(|i| key(&format!("{i}.mp4"))).collect();
        for k in &files[..MAX_PROBED] {
            d.insert(k.clone(), Some(1.0));
        }
        let total = d.total(&files, None, &egui::Context::default());
        assert_eq!(total.seconds, MAX_PROBED as f64);
        assert_eq!(total.unknown, 3);
        assert_eq!(total.pending, 0);
    }

    #[test]
    fn status_text_reports_partial_and_complete_totals() {
        let none = DurationTotal::default();
        assert_eq!(status_text(&none), "");
        let done = DurationTotal { files: 2, seconds: 125.0, unknown: 0, pending: 0 };
        assert!(status_text(&done).contains("2 media files, "));
        assert!(!status_text(&done).ends_with('+'));
        let partial = DurationTotal { files: 2, seconds: 60.0, unknown: 1, pending: 1 };
        assert!(status_text(&partial).ends_with("so far…"));
        let missing = DurationTotal { files: 2, seconds: 60.0, unknown: 1, pending: 0 };
        assert!(status_text(&missing).ends_with('+'));
        let measuring = DurationTotal { files: 1, seconds: 0.0, unknown: 1, pending: 1 };
        assert!(status_text(&measuring).ends_with("1 media file, measuring…"));
    }
}
