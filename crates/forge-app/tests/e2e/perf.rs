//! End-to-end scenarios: responsiveness of a 50,000-file folder, and of a 5,000-photo folder
//! while burst grouping runs and scrolls.
//!
//! The scenario lists the folder, then records frame times (`ui perf begin`/`end`) while it
//! scrolls the details view and the grid with the real mouse wheel and pages with the
//! keyboard. The burst scenario records the grouping analysis, grouped scrolling and
//! scrolling with 1,000 files selected. The JSON reports land in `%TEMP%\zxfr-shots\perf\`.
//! The test asserts only a
//! generous ceiling that catches a pathological regression (e.g. per-frame work that is
//! linear in the folder size). The numbers in BENCHMARKS.md come from running the same
//! script against a release build with the `automation` feature (see docs/BENCHMARKS.md).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use image::RgbImage;

use crate::*;

const FILES: usize = 50_000;
/// Photos for the grouping scenario: `BURST_LENGTH` consecutive shots per burst.
const BURST_FILES: usize = 5_000;
const BURST_LENGTH: usize = 4;
/// Files selected in the grouping scenario before its scroll is measured.
const SELECTED: usize = 1_000;

fn out_dir() -> PathBuf {
    let dir = shots_dir().join("perf");
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Empty files with photo-like names and a few extensions, so sorting and icons have work
/// to do. Thumbnails of empty images fail fast, which keeps decode cost out of the numbers.
fn make_folder(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let exts = ["JPG", "CR3", "MP4", "XMP", "TXT"];
    for i in 0..FILES {
        let name = format!("IMG_{:05}.{}", i, exts[i % exts.len()]);
        fs::File::create(dir.join(name)).unwrap();
    }
}

fn scroll_steps(n: usize, delta: i32) -> String {
    (0..n).map(|_| format!("scroll {delta}\n")).collect()
}

fn read_report(path: &Path) -> serde_json::Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("no perf report at {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

fn p95(report: &serde_json::Value, series: &str) -> f64 {
    report[series]["p95"]
        .as_f64()
        .unwrap_or_else(|| panic!("report has no {series}.p95: {report}"))
}

#[test]
fn fifty_thousand_files_scroll_without_long_frames() {
    // Frame timings: runs alone, with no other app window running.
    let _alone = exclusive();
    let sandbox = tempfile::tempdir().unwrap();
    let folder = sandbox.path().join("flat50k");
    let profile = sandbox.path().join("profile");
    make_folder(&folder);

    let out = out_dir();
    let details = out.join("flat50k-details.json");
    let grid = out.join("flat50k-grid.json");
    let keys = out.join("flat50k-keys.json");
    for p in [&details, &grid, &keys] {
        let _ = fs::remove_file(p);
    }
    let last = format!("IMG_{:05}.TXT", FILES - 1);

    let script = format!(
        "limit 300\n\
         navigate {folder}\n\
         wait-entry {last} | 120\n\
         cmd ViewDetails\n\
         wait-frames 10\n\
         pointer 700 420\n\
         ui perf begin\n\
         {down}{up}\
         ui perf end {details}\n\
         cmd ViewGrid\n\
         wait-frames 10\n\
         pointer 700 420\n\
         ui perf begin\n\
         {down}{up}\
         ui perf end {grid}\n\
         cmd ViewDetails\n\
         wait-frames 10\n\
         select IMG_00000.JPG\n\
         ui perf begin\n\
         {pages}\
         key End\n\
         wait-frames 5\n\
         key Home\n\
         wait-frames 5\n\
         ui perf end {keys}\n\
         screenshot {shot}\n\
         exit\n",
        folder = shown(&folder),
        down = scroll_steps(150, -240),
        up = scroll_steps(150, 240),
        pages = "key PageDown\n".repeat(60),
        details = shown(&details),
        grid = shown(&grid),
        keys = shown(&keys),
        shot = shown(&out.join("flat50k-details.png")),
    );
    let outcome = run_app_with(
        &sandbox.path().join("work"),
        &profile,
        &script,
        &RunOpts {
            limit: Some(std::time::Duration::from_secs(320)),
            ..Default::default()
        },
    );
    expect_ok(&outcome);
    assert_no_crash(&profile);

    // Debug builds are several times slower than release; healthy frames here take about
    // 3-10 ms (p95). The ceiling catches work that grows with the folder: a status-bar walk
    // over all 50k entries per frame once cost ~200 ms per frame while paging.
    for (name, report) in [("details", &details), ("grid", &grid), ("keys", &keys)] {
        let report = read_report(report);
        assert!(
            report["work_ms"]["count"].as_u64().unwrap_or(0) >= 50,
            "{name}: too few frames recorded: {report}"
        );
        let work = p95(&report, "work_ms");
        assert!(work < 100.0, "{name}: p95 frame work {work:.1} ms: {report}");
    }
}

fn set_mtime(path: &Path, seconds: u64) {
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
        .unwrap();
}

/// A distinct pseudo-random value per burst (splitmix64 finalizer).
fn burst_seed(run: usize) -> u64 {
    let mut z = (run as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A 16×16 JPEG whose gradient follows `seed`, so each burst looks different.
fn tiny_jpeg(seed: u64) -> Vec<u8> {
    const EDGE: u32 = 16;
    let image = RgbImage::from_fn(EDGE, EDGE, |x, y| {
        let cell_x = (x * 9 / EDGE).min(8) as usize;
        let cell_y = (y * 8 / EDGE).min(7) as usize;
        let mut level = 112i16;
        for bit in cell_y * 8..cell_y * 8 + cell_x {
            level += if (seed >> bit) & 1 == 1 { -12 } else { 12 };
        }
        let value = level.clamp(0, 255) as u8;
        image::Rgb([value, value, value])
    });
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90)
        .encode_image(&image)
        .unwrap();
    bytes
}

/// Writes `BURST_FILES` tiny JPEGs. Each burst has identical bytes and one timestamp, and
/// bursts are a minute apart, so every burst is one group. Returns the last file's name.
fn make_burst_folder(dir: &Path) -> String {
    fs::create_dir_all(dir).unwrap();
    let base = 1_703_000_000;
    let mut last = String::new();
    for run in 0..BURST_FILES / BURST_LENGTH {
        let bytes = tiny_jpeg(burst_seed(run));
        for shot in 0..BURST_LENGTH {
            let name = format!("burst-{:05}.jpg", run * BURST_LENGTH + shot);
            let path = dir.join(&name);
            fs::write(&path, &bytes).unwrap();
            set_mtime(&path, base + run as u64 * 60);
            last = name;
        }
    }
    last
}

#[test]
fn five_thousand_burst_photos_keep_frames_short_while_grouping_and_scrolling() {
    // Frame timings: runs alone, with no other app window running.
    let _alone = exclusive();
    let sandbox = tempfile::tempdir().unwrap();
    let folder = sandbox.path().join("bursts5k");
    let profile = sandbox.path().join("profile");
    let last = make_burst_folder(&folder);

    let out = out_dir();
    let analysis = out.join("bursts5k-analysis.json");
    let scroll = out.join("bursts5k-scroll.json");
    let selection = out.join("bursts5k-selection.json");
    for p in [&analysis, &scroll, &selection] {
        let _ = fs::remove_file(p);
    }
    let selected = (0..SELECTED)
        .map(|index| format!("burst-{index:05}.jpg"))
        .collect::<Vec<_>>()
        .join(", ");

    let script = format!(
        "limit 280\n\
         navigate {folder}\n\
         wait-entry {last} | 120\n\
         cmd ViewGrid\n\
         wait-frames 10\n\
         ui perf begin\n\
         ui browser toggle\n\
         ui browser wait 240\n\
         ui perf end {analysis}\n\
         pointer 700 420\n\
         ui perf begin\n\
         {down}{up}\
         ui perf end {scroll}\n\
         select {selected}\n\
         wait-frames 5\n\
         pointer 700 420\n\
         ui perf begin\n\
         {down}{up}\
         ui perf end {selection}\n\
         screenshot {shot}\n\
         exit\n",
        folder = shown(&folder),
        down = scroll_steps(60, -240),
        up = scroll_steps(60, 240),
        analysis = shown(&analysis),
        scroll = shown(&scroll),
        selection = shown(&selection),
        shot = shown(&out.join("bursts5k-grouped.png")),
    );
    let outcome = run_app_with(
        &sandbox.path().join("work"),
        &profile,
        &script,
        &RunOpts {
            limit: Some(std::time::Duration::from_secs(290)),
            ..Default::default()
        },
    );
    expect_ok(&outcome);
    assert_no_crash(&profile);

    // Same ceiling as the 50,000-file scenario. The three phases are: grouping analysis
    // running, scrolling the grouped grid, and scrolling with 1,000 files selected.
    for (name, report) in [
        ("analysis", &analysis),
        ("scroll", &scroll),
        ("selection", &selection),
    ] {
        let report = read_report(report);
        assert!(
            report["work_ms"]["count"].as_u64().unwrap_or(0) >= 50,
            "{name}: too few frames recorded: {report}"
        );
        let work = p95(&report, "work_ms");
        assert!(
            work < 100.0,
            "{name}: p95 frame work {work:.1} ms: {report}"
        );
    }
}
