//! Viewer, culling, preview and thumbnail scenarios: culling keys persist across a restart,
//! keys typed into a text field do not rate, corrupt media shows an error placeholder while
//! the app keeps running, a missing FFmpeg path is reported, and thumbnail work for folders
//! the user has left is cancelled.
//!
//! Screenshots go to `%TEMP%\zxfr-shots\viewer\`; each was looked at when the scenario was
//! written.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::*;

/// Where scenario screenshots are kept for review.
fn shot(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("zxfr-shots").join("viewer");
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// A valid JPEG of a gradient; `seed` changes the bytes so every fixture is distinct.
fn jpeg(w: u32, h: u32, seed: u8) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
    for y in 0..h {
        for x in 0..w {
            rgb.extend([(x as u8) ^ seed, y as u8, seed]);
        }
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
        .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
        .unwrap();
    out
}

/// A PNG whose header claims `w`×`h` with a stub pixel stream. Only the declared size matters
/// for the decode limit, so nothing close to that many pixels is ever allocated.
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

fn script(lines: Vec<String>) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Runs a viewer scenario. Scripts with waits longer than the defaults raise both the
/// script limit (`limit`, default 60 s) and the process limit.
fn run(work: &Path, profile: &Path, text: &str) -> Outcome {
    let text = format!("limit 240\n{text}");
    let opts = RunOpts {
        limit: Some(Duration::from_secs(270)),
        ..Default::default()
    };
    run_app_with(work, profile, &text, &opts)
}

#[test]
fn culling_keys_persist_across_a_restart_and_leave_files_untouched() {
    let sandbox = tempfile::tempdir().unwrap();
    let photos = sandbox.path().join("photos");
    let reference = sandbox.path().join("reference");
    let profile = sandbox.path().join("profile");
    for (i, name) in ["a.jpg", "b.jpg", "c.jpg", "d.jpg"].iter().enumerate() {
        write_file(&photos.join(name), &jpeg(640, 427, i as u8 + 1));
    }
    write_file(&reference.join("a.jpg"), &jpeg(640, 427, 1));
    let photo = |name: &str| shown(&photos.join(name));

    let first = script(vec![
        format!("navigate {}", shown(&photos)),
        "cmd CullMode".into(),
        "key A".into(), // auto-advance off: the arrow keys alone move between photos
        "ui viewer assert-open cull".into(),
        "ui viewer assert-current a.jpg".into(),
        "ui viewer wait-decoded".into(),
        // a: a digit key rates it
        "key 3".into(),
        "ui viewer assert-rating a.jpg 3".into(),
        "key ArrowRight".into(),
        "ui viewer assert-current b.jpg".into(),
        // b: rating, pick and a yellow label
        "key 5".into(),
        "key P".into(),
        "key 7".into(),
        "ui viewer assert-rating b.jpg 5".into(),
        "ui viewer assert-pick b.jpg pick".into(),
        "ui viewer assert-label b.jpg yellow".into(),
        "key ArrowRight".into(),
        // c: reject and a red label
        "key X".into(),
        "key 6".into(),
        "ui viewer assert-pick c.jpg reject".into(),
        "ui viewer assert-label c.jpg red".into(),
        "key ArrowRight".into(),
        // d: rated 2, then 0 clears the rating; purple label through Shift+6
        "key 2".into(),
        "key 0".into(),
        "key shift+6".into(),
        "ui viewer assert-rating d.jpg 0".into(),
        "ui viewer assert-label d.jpg purple".into(),
        // arrows move back and forth without changing anything
        "key ArrowLeft".into(),
        "ui viewer assert-current c.jpg".into(),
        "key ArrowRight".into(),
        "ui viewer assert-current d.jpg".into(),
        "key Escape".into(),
        "ui viewer assert-open none".into(),
        // XMP sync is opt-in and off by default: no sidecar may appear
        format!(
            "assert-same {} | {}",
            photo("a.jpg"),
            shown(&reference.join("a.jpg"))
        ),
        format!("assert-missing {}", photo("a.jpg.xmp")),
        format!("assert-missing {}", photo("b.jpg.xmp")),
        format!("assert-missing {}", photo("c.jpg.xmp")),
        format!("assert-missing {}", photo("d.jpg.xmp")),
        format!("assert-missing {}", photo("a.xmp")),
        "exit".into(),
    ]);
    let outcome = run(sandbox.path(), &profile, &first);
    expect_ok(&outcome);

    // A fresh process with the same profile must read the values back from the library DB.
    let second = script(vec![
        format!("navigate {}", shown(&photos)),
        "wait-entry a.jpg".into(),
        "ui viewer assert-rating a.jpg 3".into(),
        "ui viewer assert-label a.jpg none".into(),
        "ui viewer assert-pick a.jpg none".into(),
        "ui viewer assert-rating b.jpg 5".into(),
        "ui viewer assert-pick b.jpg pick".into(),
        "ui viewer assert-label b.jpg yellow".into(),
        "ui viewer assert-rating c.jpg 0".into(),
        "ui viewer assert-pick c.jpg reject".into(),
        "ui viewer assert-label c.jpg red".into(),
        "ui viewer assert-rating d.jpg 0".into(),
        "ui viewer assert-pick d.jpg none".into(),
        "ui viewer assert-label d.jpg purple".into(),
        "cmd CullMode".into(),
        "key A".into(), // auto-advance off: the arrow keys alone move between photos
        "ui viewer assert-open cull".into(),
        "ui viewer wait-decoded".into(),
        format!("screenshot {}", shown(&shot("cull-after-restart.png"))),
        "key Escape".into(),
        "ui viewer assert-open none".into(),
        "cmd ViewGrid".into(),
        "ui viewer thumbs-idle 60".into(),
        format!("screenshot {}", shown(&shot("grid-after-restart.png"))),
        "exit".into(),
    ]);
    let outcome = run(sandbox.path(), &profile, &second);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn keys_typed_into_a_rename_prompt_do_not_rate_the_photo() {
    let sandbox = tempfile::tempdir().unwrap();
    let photos = sandbox.path().join("photos");
    let profile = sandbox.path().join("profile");
    write_file(&photos.join("a.jpg"), &jpeg(640, 427, 1));

    // Rating keys act only inside the viewer, and the viewer cannot be open while a prompt
    // has focus. Typing digits into the rename prompt must therefore neither rate the photo
    // nor open the viewer; the rename is cancelled so the file keeps its name.
    let text = script(vec![
        format!("navigate {}", shown(&photos)),
        "select a.jpg".into(),
        "cmd Rename".into(),
        "text 5".into(),
        "key 5".into(),
        "key P".into(),
        "key 2".into(),
        "cancel".into(),
        "ui viewer assert-open none".into(),
        "ui viewer assert-rating a.jpg 0".into(),
        "ui viewer assert-pick a.jpg none".into(),
        format!("assert-exists {}", shown(&photos.join("a.jpg"))),
        "exit".into(),
    ]);
    let outcome = run(sandbox.path(), &profile, &text);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

/// A command palette opened over the viewer must be drawn there too: an open palette blocks
/// every viewer key, so an undrawn one could never be closed (regression test).
#[test]
fn command_palette_does_not_lock_an_open_viewer() {
    let sandbox = tempfile::tempdir().unwrap();
    let photos = sandbox.path().join("photos");
    let profile = sandbox.path().join("profile");
    write_file(&photos.join("a.jpg"), &jpeg(640, 427, 1));
    write_file(&photos.join("b.jpg"), &jpeg(640, 427, 2));

    let text = script(vec![
        format!("navigate {}", shown(&photos)),
        "cmd CullMode".into(),
        "key A".into(),
        "ui viewer wait-decoded".into(),
        "cmd CommandPalette".into(),
        "sleep 300".into(),
        "key Escape".into(),
        "sleep 300".into(),
        "ui viewer assert-keys-free".into(),
        "key ArrowRight".into(),
        "ui viewer assert-current b.jpg".into(),
        "key Escape".into(),
        "ui viewer assert-open none".into(),
        "exit".into(),
    ]);
    let outcome = run(sandbox.path(), &profile, &text);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn corrupt_media_shows_an_error_placeholder_and_the_app_keeps_running() {
    let sandbox = tempfile::tempdir().unwrap();
    let bad = sandbox.path().join("bad");
    let vids = sandbox.path().join("vids");
    let profile = sandbox.path().join("profile");
    let good = jpeg(640, 427, 9);
    write_file(&bad.join("a-random.jpg"), &noise(4096, 3));
    write_file(&bad.join("b-truncated.jpg"), &good[..good.len() * 2 / 5]);
    write_file(&bad.join("c-zero.jpg"), &[]);
    write_file(&bad.join("d-garbage.CR2"), &noise(8192, 4));
    write_file(&bad.join("e-garbage.NEF"), &noise(8192, 5));
    write_file(&bad.join("f-huge.png"), &png_claiming(60_000, 60_000));
    write_file(&bad.join("g-good.jpg"), &jpeg(320, 213, 1));
    write_file(&vids.join("garbage.mp4"), &noise(16384, 6));
    write_file(&vids.join("garbage.mov"), &noise(16384, 7));

    let mut lines = vec![
        format!("navigate {}", shown(&bad)),
        "select a-random.jpg".into(),
        "key Enter".into(),
        "ui viewer assert-open single".into(),
    ];
    // The viewer walks the folder in name order. Undecodable files must show the placeholder.
    // A truncated JPEG is not an error to the decoder: its readable part is drawn, so it is
    // only required to finish decoding and be checked by screenshot.
    let broken = [
        ("a-random.jpg", true),
        ("b-truncated.jpg", false),
        ("c-zero.jpg", true),
        ("d-garbage.CR2", true),
        ("e-garbage.NEF", true),
        ("f-huge.png", true),
    ];
    for (i, (name, is_error)) in broken.iter().enumerate() {
        if i > 0 {
            lines.push("key ArrowRight".into());
        }
        lines.push(format!("ui viewer assert-current {name}"));
        lines.push("ui viewer wait-decoded 20".into());
        if *is_error {
            lines.push("ui viewer assert-error-shown".into());
        }
        if *name == "f-huge.png" {
            // Refused by its declared size before any pixel buffer is allocated.
            lines.push("ui viewer assert-error-contains decode limit".into());
        }
        if let Some(stem) = name.strip_suffix(".jpg").or(name.strip_suffix(".png")) {
            lines.push(format!(
                "screenshot {}",
                shown(&shot(&format!("viewer-{stem}.png")))
            ));
        }
    }
    // After the broken files the viewer still moves and decodes the valid one.
    lines.push("key ArrowRight".into());
    lines.push("ui viewer assert-current g-good.jpg".into());
    lines.push("ui viewer wait-decoded 20".into());
    lines.push(format!(
        "screenshot {}",
        shown(&shot("viewer-good-after-errors.png"))
    ));
    lines.push("key Escape".into());
    lines.push("ui viewer assert-open none".into());

    // Preview panel: the same files as thumbnails, and the video paths with FFmpeg absent.
    lines.push(format!("navigate {}", shown(&bad)));
    lines.push("cmd TogglePreview".into());
    for name in [
        "d-garbage.CR2",
        "e-garbage.NEF",
        "f-huge.png",
        "a-random.jpg",
        "c-zero.jpg",
    ] {
        lines.push(format!("select {name}"));
        lines.push(format!("ui viewer wait-preview-error {name} 30"));
        if name == "f-huge.png" {
            lines.push(format!(
                "screenshot {}",
                shown(&shot("preview-huge-png.png"))
            ));
        }
    }
    lines.push(format!("navigate {}", shown(&vids)));
    for name in ["garbage.mp4", "garbage.mov"] {
        lines.push(format!("select {name}"));
        lines.push(format!("ui viewer wait-video-unavailable {name} 20"));
    }
    lines.push(format!(
        "screenshot {}",
        shown(&shot("preview-garbage-video.png"))
    ));
    lines.push(format!("navigate {}", shown(&bad)));
    lines.push("select g-good.jpg".into());
    lines.push("assert-entry g-good.jpg".into());
    lines.push("exit".into());

    let outcome = run(sandbox.path(), &profile, &script(lines));
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn missing_ffmpeg_setting_is_reported_for_videos_without_failing() {
    let sandbox = tempfile::tempdir().unwrap();
    let vids = sandbox.path().join("vids");
    let profile = sandbox.path().join("profile");
    write_file(&vids.join("garbage.mp4"), &noise(16384, 6));
    write_file(&vids.join("still.jpg"), &jpeg(320, 213, 4));
    // Point the FFmpeg setting at a file that does not exist, as a user with a moved install would.
    let missing = sandbox.path().join("no-such-ffmpeg");
    let settings = serde_json::json!({ "ffmpeg_dir": missing });
    write_file(
        &profile.join("config").join("settings.json"),
        settings.to_string().as_bytes(),
    );

    let text = script(vec![
        format!("navigate {}", shown(&vids)),
        "cmd TogglePreview".into(),
        "select garbage.mp4".into(),
        "ui viewer wait-video-unavailable garbage.mp4 20".into(),
        format!("screenshot {}", shown(&shot("preview-missing-ffmpeg.png"))),
        // Viewer: the video is one of the items in the folder; it reports an error, not a crash.
        "select still.jpg".into(),
        "key Enter".into(),
        "ui viewer assert-open single".into(),
        "key ArrowLeft".into(),
        "ui viewer assert-current garbage.mp4".into(),
        "ui viewer wait-decoded 20".into(),
        "ui viewer assert-error-shown".into(),
        "key Escape".into(),
        "ui viewer assert-open none".into(),
        "exit".into(),
    ]);
    let outcome = run(sandbox.path(), &profile, &text);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn thumbnail_work_is_cancelled_when_the_user_leaves_and_the_cache_stays_valid() {
    let sandbox = tempfile::tempdir().unwrap();
    let big = sandbox.path().join("big");
    let other = sandbox.path().join("other");
    let profile = sandbox.path().join("profile");
    generate_jpegs(&big, 300, 1600, 1066);
    for i in 0..20u8 {
        write_file(&other.join(format!("o-{i:02}.jpg")), &jpeg(320, 213, i));
    }

    let text = script(vec![
        format!("navigate {}", shown(&big)),
        "cmd ViewGrid".into(),
        "wait-entry img-000.jpg".into(),
        "sleep 300".into(),
        "cmd ThumbsBigger".into(),
        "pointer 500 400".into(),
        "scroll 4000".into(),
        // Leave the folder while its thumbnails are still being generated.
        format!("navigate {}", shown(&other)),
        "wait-entry o-00.jpg".into(),
        format!("assert-path {}", shown(&other)),
        // Come back, then open and close a tab with work still in flight.
        format!("navigate {}", shown(&big)),
        "cmd NewTab".into(),
        format!("navigate {}", shown(&other)),
        "cmd CloseTab".into(),
        "assert-tabs 1".into(),
        "cmd ThumbsSmaller".into(),
        "pointer 500 400".into(),
        "scroll -2000".into(),
        "sleep 200".into(),
        "ui viewer thumbs-idle 90".into(),
        "ui viewer assert-thumbs-cancelled 1".into(),
        format!("assert-path {}", shown(&big)),
        format!(
            "screenshot {}",
            shown(&shot("grid-cancelled-navigation.png"))
        ),
        "exit".into(),
    ]);
    let outcome = run(sandbox.path(), &profile, &text);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_cache_is_whole(&profile.join("cache").join("thumbs"));
}

/// Writes `count` gradient JPEGs named `img-000.jpg`… across a few threads.
fn generate_jpegs(dir: &Path, count: usize, w: u32, h: u32) {
    fs::create_dir_all(dir).unwrap();
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8);
    std::thread::scope(|scope| {
        for t in 0..threads {
            scope.spawn(move || {
                for i in (t..count).step_by(threads) {
                    let bytes = jpeg(w, h, (i % 251) as u8);
                    write_file(&dir.join(format!("img-{i:03}.jpg")), &bytes);
                }
            });
        }
    });
}

/// After the app exits, the disk cache holds only whole thumbnails: no temp files, and every
/// published entry decodes.
fn assert_cache_is_whole(cache: &Path) {
    let mut checked = 0;
    let mut stack = vec![cache.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.map(|e| e.unwrap()) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
                continue;
            }
            assert!(
                !name.contains(".tmp"),
                "partial cache file left behind: {}",
                path.display()
            );
            image::open(&path)
                .unwrap_or_else(|e| panic!("cache entry {} does not decode: {e}", path.display()));
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "no thumbnails were cached under {}",
        cache.display()
    );
}
