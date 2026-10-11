//! `ui perf <verb> [args]` script verbs: frame-time measurement.
//!
//! * `ui perf begin` — discard earlier samples and start recording.
//! * `ui perf end <json>` — stop recording and write the statistics to `<json>`.
//!
//! Two series are recorded per frame while recording:
//!
//! * `work_ms` — from the start of the frame (`raw_input_hook`) to the end of the app's
//!   `ui` (the test hook tick): the app's CPU time for logic, layout and widget code. It
//!   excludes tessellation, GPU upload and presentation, which egui does after `ui`.
//! * `interval_ms` — time between the starts of consecutive frames, which includes
//!   painting and waiting for vsync. Scripts repaint continuously, so this is the frame
//!   pacing a user would see while interacting.

use std::path::Path;
use std::time::Instant;

use super::{Flow, StepState};
use crate::app::ZxrawApp;

#[derive(Default)]
pub(crate) struct FrameClock {
    frame_start: Option<Instant>,
    last_start: Option<Instant>,
    recording: bool,
    work_ms: Vec<f64>,
    interval_ms: Vec<f64>,
}

impl FrameClock {
    pub fn frame_started(&mut self) {
        let now = Instant::now();
        if self.recording
            && let Some(last) = self.last_start
        {
            self.interval_ms.push(ms(now - last));
        }
        self.last_start = Some(now);
        self.frame_start = Some(now);
    }

    pub fn frame_finished(&mut self) {
        if self.recording
            && let Some(start) = self.frame_start.take()
        {
            self.work_ms.push(ms(start.elapsed()));
        }
    }

    fn begin(&mut self) {
        self.work_ms.clear();
        self.interval_ms.clear();
        self.recording = true;
        // The frame in progress started before recording; don't count its partial work.
        self.frame_start = None;
        self.last_start = None;
    }
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Summary statistics (milliseconds). Percentiles use the nearest-rank method.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Stats {
    pub count: usize,
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

pub(crate) fn stats(samples: &[f64]) -> Option<Stats> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = |p: f64| {
        let i = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
        sorted[i.clamp(1, sorted.len()) - 1]
    };
    Some(Stats {
        count: sorted.len(),
        mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
        p50: rank(50.0),
        p95: rank(95.0),
        p99: rank(99.0),
        max: *sorted.last().unwrap(),
    })
}

fn stats_json(s: Option<Stats>) -> serde_json::Value {
    match s {
        None => serde_json::Value::Null,
        Some(s) => serde_json::json!({
            "count": s.count,
            "mean": s.mean,
            "p50": s.p50,
            "p95": s.p95,
            "p99": s.p99,
            "max": s.max,
        }),
    }
}

fn write_report(clock: &FrameClock, path: &Path) -> Result<(), String> {
    let report = serde_json::json!({
        "work_ms": stats_json(stats(&clock.work_ms)),
        "interval_ms": stats_json(stats(&clock.interval_ms)),
        "build": if cfg!(debug_assertions) { "debug" } else { "release" },
    });
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

pub(super) fn step(
    app: &mut ZxrawApp,
    _ctx: &egui::Context,
    verb: &str,
    args: &str,
    _st: &mut StepState,
) -> Result<Flow, String> {
    let clock = &mut app.testhook.perf;
    match verb {
        "begin" => {
            clock.begin();
            Ok(Flow::Done)
        }
        "end" => {
            if args.is_empty() {
                return Err("expected `ui perf end <json path>`".into());
            }
            if !clock.recording {
                return Err("`ui perf end` without `ui perf begin`".into());
            }
            // Confined like every other path: the sandbox, or the screenshot folder.
            let path = super::report_path(Path::new(args))?;
            clock.recording = false;
            write_report(clock, &path)?;
            Ok(Flow::Done)
        }
        other => Err(format!("unknown `ui perf` verb `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let samples: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = stats(&samples).unwrap();
        assert_eq!(
            (s.count, s.p50, s.p95, s.p99, s.max),
            (100, 50.0, 95.0, 99.0, 100.0)
        );
        assert!((s.mean - 50.5).abs() < 1e-9);
        let one = stats(&[7.0]).unwrap();
        assert_eq!((one.p50, one.p99), (7.0, 7.0));
        assert!(stats(&[]).is_none());
    }
}
