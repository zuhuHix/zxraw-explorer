//! End-to-end scenarios: responsiveness of a 50,000-file folder.
//!
//! The scenario lists the folder, then records frame times (`ui perf begin`/`end`) while it
//! scrolls the details view and the grid with the real mouse wheel and pages with the
//! keyboard. The JSON reports land in `%TEMP%\zxfr-shots\perf\`. The test asserts only a
//! generous ceiling that catches a pathological regression (e.g. per-frame work that is
//! linear in the folder size). The numbers in BENCHMARKS.md come from running the same
//! script against a release build with the `automation` feature (see docs/BENCHMARKS.md).

use std::fs;
use std::path::{Path, PathBuf};

use crate::*;

const FILES: usize = 50_000;

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
