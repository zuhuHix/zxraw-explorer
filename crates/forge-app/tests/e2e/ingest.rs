//! End-to-end scenarios: media ingest from a fake camera card (verified copy, a conflicting
//! destination file, a verification mismatch), and a card pulled mid-copy for copy, move and
//! ingest, followed by a retry. Screenshots go to `%TEMP%\zxfr-shots\ingest\`.

use std::fs::{self, File};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{Local, TimeZone};

use crate::*;

/// Every file of the fake card's DCIM folder, with its destination relative to the library
/// (folder template `{YYYY}/{MM}-{DD}`, filename `{orig_name}`). A clean "safe to format" run
/// has to ingest all of them: the safety check covers every file under the source.
const CARD_FILES: [(&str, &str); 6] = [
    ("IMG_0001.JPG", "2024/05-17/IMG_0001.JPG"),
    ("IMG_0002.JPG", "2024/05-17/IMG_0002.JPG"),
    ("IMG_0003.JPG", "2024/05-18/IMG_0003.JPG"),
    ("IMG_0003.XMP", "2024/05-18/IMG_0003.XMP"),
    ("IMG_0004.CR2", "2024/05-18/IMG_0004.CR2"),
    ("MVI_0005.MP4", "2024/05-18/MVI_0005.MP4"),
];

/// Whole-run limits for these scripts. They hash real files and wait for background jobs, which
/// is far slower on a busy machine than the default 75 s process and 60 s script limits.
const RUN_SECS: u64 = 240;

/// Runs an ingest or transfer script with the raised limits.
fn run_ingest(sandbox: &Path, text: &str) -> Outcome {
    let text = format!("limit {RUN_SECS}\n{text}");
    let opts = RunOpts {
        limit: Some(Duration::from_secs(RUN_SECS + 60)),
        ..Default::default()
    };
    run_app_with(sandbox, &sandbox.join("profile"), &text, &opts)
}

fn shot(name: &str) -> String {
    let dir = std::env::temp_dir().join("zxfr-shots").join("ingest");
    fs::create_dir_all(&dir).unwrap();
    shown(&dir.join(name))
}

/// A `ui ingest` script: the steps, then `exit`.
fn script(steps: &[&str]) -> String {
    let mut text = String::new();
    for step in steps {
        text.push_str(step);
        text.push('\n');
    }
    text.push_str("exit\n");
    text
}

fn jpeg(seed: u8) -> Vec<u8> {
    let img = image::RgbImage::from_fn(160, 120, |x, y| {
        image::Rgb([
            (x as u8).wrapping_mul(3) ^ seed,
            (y as u8).wrapping_add(seed),
            seed.wrapping_mul(7),
        ])
    });
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .unwrap();
    out.into_inner()
}

fn local(day: u32, hour: u32, minute: u32) -> chrono::DateTime<Local> {
    Local
        .with_ymd_and_hms(2024, 5, day, hour, minute, 0)
        .single()
        .unwrap()
}

fn set_mtime(path: &Path, when: chrono::DateTime<Local>) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::from(when))
        .unwrap();
}

/// `CARD/DCIM/100CANON`: three JPEGs and a RAW dated 2024-05-17 and 2024-05-18, plus a video
/// and an XMP sidecar. `CARD/MISC` is outside DCIM, so it is never ingested.
fn fake_card(card: &Path) -> PathBuf {
    let dcim = card.join("DCIM").join("100CANON");
    write_file(&dcim.join("IMG_0001.JPG"), &jpeg(1));
    write_file(&dcim.join("IMG_0002.JPG"), &jpeg(2));
    write_file(&dcim.join("IMG_0003.JPG"), &jpeg(3));
    write_file(&dcim.join("IMG_0004.CR2"), &noise(96 * 1024, 4));
    write_file(&dcim.join("MVI_0005.MP4"), &noise(64 * 1024, 5));
    write_file(&dcim.join("IMG_0003.XMP"), b"<x:xmpmeta/>\n");
    write_file(&card.join("MISC").join("readme.txt"), b"not camera media\n");
    set_mtime(&dcim.join("IMG_0001.JPG"), local(17, 12, 0));
    set_mtime(&dcim.join("IMG_0002.JPG"), local(17, 12, 1));
    set_mtime(&dcim.join("IMG_0003.JPG"), local(18, 9, 0));
    set_mtime(&dcim.join("IMG_0004.CR2"), local(18, 9, 1));
    set_mtime(&dcim.join("IMG_0003.XMP"), local(18, 9, 0));
    set_mtime(&dcim.join("MVI_0005.MP4"), local(18, 9, 2));
    dcim
}

/// Every regular file under `root` (links are not followed), as `/`-separated relative paths.
fn files_under(root: &Path) -> Vec<String> {
    fn visit(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                visit(&path, root, out);
            } else if meta.is_file() {
                let rel = path.strip_prefix(root).unwrap();
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    visit(root, root, &mut out);
    out.sort();
    out
}

/// Bytes and modified time of every file under `root`, to prove a folder was not touched.
fn snapshot(root: &Path) -> Vec<(String, Vec<u8>, SystemTime)> {
    files_under(root)
        .into_iter()
        .map(|rel| {
            let path = root.join(&rel);
            let bytes = fs::read(&path).unwrap();
            let modified = fs::metadata(&path).unwrap().modified().unwrap();
            (rel, bytes, modified)
        })
        .collect()
}

/// Destination files other than the ingest manifests written at the library root.
fn payload_files(dst: &Path) -> Vec<String> {
    files_under(dst)
        .into_iter()
        .filter(|rel| !rel.starts_with("forge-ingest-"))
        .collect()
}

/// One JSON report per ingest run (`forge-ingest-*.json`), parsed.
fn reports(dst: &Path) -> Vec<serde_json::Value> {
    files_under(dst)
        .into_iter()
        .filter(|rel| rel.starts_with("forge-ingest-") && rel.ends_with(".json"))
        .map(|rel| {
            let text = fs::read_to_string(dst.join(rel)).unwrap();
            serde_json::from_str(&text).unwrap()
        })
        .collect()
}

fn count(report: &serde_json::Value, key: &str) -> usize {
    report[key].as_array().map_or(0, Vec::len)
}

fn manifest_lines(dst: &Path) -> Vec<usize> {
    files_under(dst)
        .into_iter()
        .filter(|rel| rel.starts_with("forge-ingest-") && rel.ends_with(".b3"))
        .map(|rel| {
            fs::read_to_string(dst.join(rel))
                .unwrap()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
        })
        .collect()
}

#[test]
fn verified_card_ingest_copies_every_photo_and_ends_safe_to_format() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let card = sandbox.path().join("CARD");
    let dst = sandbox.path().join("library");
    let dcim = fake_card(&card);
    let before = snapshot(&card);
    // Both folders come through the native-dialog seam, as they would from the Browse buttons.
    let text = script(&[
        "ui ingest open",
        &format!("dialog {}", shown(&card)),
        "ui ingest pick-source",
        &format!("dialog {}", shown(&dst)),
        "ui ingest pick-destination",
        "ui ingest template {YYYY}/{MM}-{DD} | {orig_name}",
        "ui ingest wait-preview 20",
        "ui ingest assert-plan 6 0",
        "ui ingest start 20",
        "ui ingest wait-done 60",
        "ui ingest assert-counts copied=6 verified=6 skipped=0 failed=0 missing=0",
        "ui ingest assert-safety safe",
        "pointer 960 500",
        "scroll 5000",
        "wait-frames 3",
        &format!("screenshot {}", shot("verified-safe.png")),
    ]);

    let outcome = run_ingest(sandbox.path(), &text);

    expect_ok(&outcome);
    for (source, rel) in CARD_FILES {
        assert_eq!(
            fs::read(dst.join(rel)).unwrap(),
            fs::read(dcim.join(source)).unwrap(),
            "{rel} must be byte-identical to its source"
        );
    }
    let mut expected: Vec<&str> = CARD_FILES.iter().map(|(_, rel)| *rel).collect();
    expected.sort();
    assert_eq!(
        payload_files(&dst),
        expected,
        "every card file, in the template folders"
    );
    assert_eq!(
        manifest_lines(&dst),
        [6],
        "the manifest lists every verified file"
    );
    let report = reports(&dst);
    assert_eq!(report.len(), 1);
    assert_eq!(count(&report[0], "verified"), 6);
    assert_eq!(count(&report[0], "failed"), 0);
    assert_eq!(count(&report[0], "missing"), 0);
    assert_eq!(
        snapshot(&card),
        before,
        "the card must be untouched, bytes and times"
    );
    assert_no_crash(&profile);
}

#[test]
fn ingest_keeps_an_existing_destination_file_and_writes_a_numbered_copy() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let card = sandbox.path().join("CARD");
    let dst = sandbox.path().join("library");
    let dcim = fake_card(&card);
    let other = b"a different photo with the same name".to_vec();
    write_file(&dst.join("2024/05-17/IMG_0001.JPG"), &other);
    let before = snapshot(&card);
    let text = script(&[
        &format!("ui ingest open {} | {}", shown(&card), shown(&dst)),
        "ui ingest template {YYYY}/{MM}-{DD} | {orig_name}",
        "ui ingest wait-preview 20",
        "ui ingest assert-plan 6 0",
        "ui ingest start 20",
        "ui ingest wait-done 60",
        "ui ingest assert-counts copied=6 verified=6 skipped=0 failed=0 missing=0",
        "ui ingest assert-safety safe",
        "pointer 960 500",
        "scroll 5000",
        "wait-frames 3",
        &format!("screenshot {}", shot("conflict-safe.png")),
    ]);

    let outcome = run_ingest(sandbox.path(), &text);

    expect_ok(&outcome);
    assert_eq!(
        fs::read(dst.join("2024/05-17/IMG_0001.JPG")).unwrap(),
        other,
        "the conflicting file must never be overwritten"
    );
    assert_eq!(
        fs::read(dst.join("2024/05-17/IMG_0001 (2).JPG")).unwrap(),
        fs::read(dcim.join("IMG_0001.JPG")).unwrap(),
        "the incoming photo goes to the next free name"
    );
    assert_eq!(
        payload_files(&dst).len(),
        7,
        "six card files plus the existing one"
    );
    assert_eq!(snapshot(&card), before);
    assert_no_crash(&profile);
}

#[test]
fn ingest_leaving_a_file_type_behind_is_not_safe_to_format() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let card = sandbox.path().join("CARD");
    let dst = sandbox.path().join("library");
    fake_card(&card);
    let text = script(&[
        &format!("ui ingest open {} | {}", shown(&card), shown(&dst)),
        "ui ingest template {YYYY}/{MM}-{DD} | {orig_name}",
        "ui ingest option filter photos",
        "ui ingest wait-preview 20",
        "ui ingest assert-plan 4 0",
        "ui ingest start 20",
        "ui ingest wait-done 60",
        "ui ingest assert-counts copied=4 verified=4 skipped=0 failed=0 missing=0",
        "ui ingest assert-safety not-safe",
        "ui ingest assert-issue MVI_0005.MP4",
        "ui ingest assert-issue IMG_0003.XMP",
        "pointer 960 500",
        "scroll 5000",
        "wait-frames 3",
        &format!("screenshot {}", shot("photos-only-not-safe.png")),
    ]);

    let outcome = run_ingest(sandbox.path(), &text);

    expect_ok(&outcome);
    assert_eq!(
        payload_files(&dst).len(),
        4,
        "only the photo filter's files are copied"
    );
    assert_no_crash(&profile);
}

#[test]
fn ingest_with_a_verification_mismatch_is_not_safe_to_format() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let card = sandbox.path().join("CARD");
    let dst = sandbox.path().join("library");
    let dcim = fake_card(&card);
    let victim = dcim.join("IMG_0001.JPG");
    let before = snapshot(&card);
    let text = script(&[
        &format!("ui ingest fault corrupt {}", shown(&victim)),
        &format!("ui ingest open {} | {}", shown(&card), shown(&dst)),
        "ui ingest template {YYYY}/{MM}-{DD} | {orig_name}",
        "ui ingest wait-preview 20",
        "ui ingest start 20",
        "ui ingest wait-done 60",
        "ui ingest fault off",
        "ui ingest assert-counts copied=5 verified=5 skipped=0 failed=1 missing=0",
        "ui ingest assert-safety not-safe",
        "ui ingest assert-issue IMG_0001.JPG",
        "pointer 960 500",
        "scroll 5000",
        "wait-frames 3",
        &format!("screenshot {}", shot("mismatch-not-safe.png")),
    ]);

    let outcome = run_ingest(sandbox.path(), &text);

    expect_ok(&outcome);
    assert!(
        !dst.join("2024/05-17/IMG_0001.JPG").exists(),
        "a file that failed verification must never appear under its final name"
    );
    assert_eq!(
        payload_files(&dst).len(),
        5,
        "no partial copy may be left behind"
    );
    for (source, rel) in &CARD_FILES[1..] {
        assert_eq!(
            fs::read(dst.join(rel)).unwrap(),
            fs::read(dcim.join(source)).unwrap()
        );
    }
    assert_eq!(manifest_lines(&dst), [5]);
    let report = reports(&dst);
    assert_eq!(report.len(), 1);
    assert_eq!(count(&report[0], "failed"), 1);
    let failed = report[0]["failed"][0]["path"].as_str().unwrap();
    assert!(
        failed.ends_with("IMG_0001.JPG"),
        "the mismatch is reported: {failed}"
    );
    assert_eq!(snapshot(&card), before, "the card must be untouched");
    assert_no_crash(&profile);
}

/// Source folder for the transfer scenarios: a small file, a large file the card pull
/// interrupts, and a file the copy never reaches.
fn write_transfer_source(src: &Path) {
    write_file(&src.join("a-first.bin"), &noise(64 * 1024, 11));
    write_file(&src.join("b-large.bin"), &noise(3 * 1024 * 1024, 12));
    write_file(&src.join("c-last.bin"), &noise(8 * 1024, 13));
}

/// Pulls the card (the source folder fails with ERROR_DEVICE_NOT_CONNECTED) after 128 KiB has
/// been read, which is in the middle of `b-large.bin`. Then checks the error, the partial
/// cleanup and the source rules, and retries the rest with the card back.
fn pulled_card_during_transfer(cmd: &str, moves: bool) {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let src = sandbox.path().join("src");
    let dst = sandbox.path().join("dst");
    write_transfer_source(&src);
    fs::create_dir_all(&dst).unwrap();
    let first = noise(64 * 1024, 11);
    let large = noise(3 * 1024 * 1024, 12);
    let last = noise(8 * 1024, 13);
    let text = dual_pane_script(
        &src,
        &dst,
        &[
            &format!("ui ingest fault source-gone {} 131072", shown(&src)),
            "select a-first.bin, b-large.bin, c-last.bin",
            &format!("cmd {cmd}"),
            "wait-jobs 30",
            &format!("screenshot {}", shot(&format!("{cmd}-card-pulled.png"))),
            "accept",
            &format!("assert-missing {}", shown(&dst.join("b-large.bin"))),
            &format!("assert-missing {}", shown(&dst.join("c-last.bin"))),
            &format!("assert-exists {}", shown(&src.join("b-large.bin"))),
            &format!("assert-exists {}", shown(&src.join("c-last.bin"))),
            "ui ingest fault off",
            "select b-large.bin, c-last.bin",
            &format!("cmd {cmd}"),
            "wait-jobs 30",
        ],
    );

    let outcome = run_ingest(sandbox.path(), &text);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(
        fs::read(dst.join("a-first.bin")).unwrap(),
        first,
        "a finished file stays intact"
    );
    assert_eq!(
        fs::read(dst.join("b-large.bin")).unwrap(),
        large,
        "the retry completes it"
    );
    assert_eq!(fs::read(dst.join("c-last.bin")).unwrap(), last);
    assert_eq!(
        files_under(&dst),
        ["a-first.bin", "b-large.bin", "c-last.bin"],
        "no truncated or temporary file may be left in the destination"
    );
    if moves {
        assert!(
            !src.join("a-first.bin").exists(),
            "a completed move removes its source"
        );
        assert!(!src.join("b-large.bin").exists() && !src.join("c-last.bin").exists());
    } else {
        assert_eq!(fs::read(src.join("a-first.bin")).unwrap(), first);
        assert_eq!(fs::read(src.join("b-large.bin")).unwrap(), large);
        assert_eq!(fs::read(src.join("c-last.bin")).unwrap(), last);
    }
}

#[test]
fn copy_survives_the_card_being_pulled_mid_file_and_retries_cleanly() {
    pulled_card_during_transfer("CopyToOtherPane", false);
}

#[test]
fn move_never_deletes_a_source_whose_copy_did_not_complete() {
    pulled_card_during_transfer("MoveToOtherPane", true);
}

#[test]
fn ingest_reports_not_safe_when_the_card_is_pulled_then_retries_after_the_card_returns() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let card = sandbox.path().join("CARD");
    let dst = sandbox.path().join("library");
    let dcim = card.join("DCIM").join("100CANON");
    write_file(&dcim.join("IMG_0001.JPG"), &jpeg(1));
    write_file(&dcim.join("IMG_0002.JPG"), &noise(300 * 1024, 2));
    write_file(&dcim.join("IMG_0003.JPG"), &jpeg(3));
    set_mtime(&dcim.join("IMG_0001.JPG"), local(17, 12, 0));
    set_mtime(&dcim.join("IMG_0002.JPG"), local(17, 12, 1));
    set_mtime(&dcim.join("IMG_0003.JPG"), local(17, 12, 2));
    let before = snapshot(&card);
    let text = script(&[
        &format!("ui ingest open {} | {}", shown(&card), shown(&dst)),
        "ui ingest template {YYYY}/{MM}-{DD} | {orig_name}",
        "ui ingest option filter photos",
        "ui ingest wait-preview 20",
        "ui ingest assert-plan 3 0",
        &format!("ui ingest fault source-gone {} 65536", shown(&dcim)),
        "ui ingest start 20",
        "ui ingest wait-done 60",
        "ui ingest assert-counts copied=1 verified=1 skipped=0 failed=1 missing=1",
        "ui ingest assert-safety not-safe",
        "ui ingest assert-issue IMG_0002.JPG",
        "pointer 960 500",
        "scroll 5000",
        "wait-frames 3",
        &format!("screenshot {}", shot("ingest-card-pulled.png")),
        &format!(
            "assert-missing {}",
            shown(&dst.join("2024/05-17/IMG_0002.JPG"))
        ),
        "ui ingest fault off",
        "ui ingest retry",
        "ui ingest wait-preview 20",
        "ui ingest assert-plan 3 0",
        "ui ingest start 20",
        "ui ingest wait-done 60",
        "ui ingest assert-counts copied=2 verified=3 skipped=1 failed=0 missing=0",
        "ui ingest assert-safety safe",
        "pointer 960 500",
        "scroll 5000",
        "wait-frames 3",
        &format!("screenshot {}", shot("ingest-retried-safe.png")),
    ]);

    let outcome = run_ingest(sandbox.path(), &text);

    expect_ok(&outcome);
    assert_eq!(
        payload_files(&dst),
        [
            "2024/05-17/IMG_0001.JPG",
            "2024/05-17/IMG_0002.JPG",
            "2024/05-17/IMG_0003.JPG"
        ],
        "the retry must not duplicate the file that already arrived"
    );
    for name in ["IMG_0001.JPG", "IMG_0002.JPG", "IMG_0003.JPG"] {
        assert_eq!(
            fs::read(dst.join("2024/05-17").join(name)).unwrap(),
            fs::read(dcim.join(name)).unwrap(),
            "{name} must be intact after the retry"
        );
    }
    assert_eq!(
        manifest_lines(&dst).len(),
        2,
        "each run writes one manifest and one report"
    );
    let mut counts: Vec<(usize, usize, usize)> = reports(&dst)
        .iter()
        .map(|r| {
            (
                count(r, "verified"),
                count(r, "already_present"),
                count(r, "missing"),
            )
        })
        .collect();
    counts.sort();
    assert_eq!(
        counts,
        [(1, 0, 1), (2, 1, 0)],
        "(verified, already present, missing) per run"
    );
    assert_eq!(
        snapshot(&card),
        before,
        "the card must be untouched, bytes and times"
    );
    assert_no_crash(&profile);
}
