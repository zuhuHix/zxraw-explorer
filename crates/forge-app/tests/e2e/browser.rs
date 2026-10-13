//! End-to-end scenarios: browser grouping.

#[allow(unused_imports)]
use crate::*;

use image::RgbImage;
use std::fs::{self, File};
use std::io::Cursor;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};
use tempfile::TempDir;

const BURST_HASH: u64 = 0x1234_5678_9abc_def0;
const DUPLICATE_HASH: u64 = 0x0f0f_f0f0_aaaa_5555;

fn jpeg_for_hash(hash: u64, seed: u32) -> Vec<u8> {
    let image = pixels_for_hash(hash, seed);
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 94)
        .encode_image(&image)
        .unwrap();
    bytes
}

fn png_for_hash(hash: u64, seed: u32) -> Vec<u8> {
    let image = pixels_for_hash(hash, seed);
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

fn pixels_for_hash(hash: u64, seed: u32) -> RgbImage {
    const WIDTH: u32 = 144;
    const HEIGHT: u32 = 128;
    let mut state = seed.max(1);
    RgbImage::from_fn(WIDTH, HEIGHT, |x, y| {
        let cell_x = (x * 9 / WIDTH).min(8) as usize;
        let cell_y = (y * 8 / HEIGHT).min(7) as usize;
        let mut level = 112i16;
        for bit in cell_y * 8..cell_y * 8 + cell_x {
            level += if (hash >> bit) & 1 == 1 { -12 } else { 12 };
        }
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let noise = ((state >> 16) % 5) as i16 - 2;
        let value = (level + noise).clamp(0, 255) as u8;
        image::Rgb([value, value, value])
    })
}

fn write_jpeg(dir: &Path, name: &str, hash: u64, seed: u32, mtime: u64) {
    let path = dir.join(name);
    write_file(&path, &jpeg_for_hash(hash, seed));
    set_mtime(&path, mtime);
}

fn set_mtime(path: &Path, seconds: u64) {
    let file = File::options().write(true).open(path).unwrap();
    file.set_times(
        std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)),
    )
    .unwrap();
}

fn fixture() -> (
    TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let work = root.join("work");
    let profile = root.join("profile");
    (temp, work, profile, root)
}

#[test]
fn bursts_and_far_apart_visual_duplicates_group_select_copy_and_screenshot() {
    let (_temp, work, profile, root) = fixture();
    let source = root.join("source");
    let destination = root.join("destination");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&destination).unwrap();
    let base = 1_700_000_000;

    for shot in 0..5 {
        write_jpeg(
            &source,
            &format!("burst-{shot}.jpg"),
            BURST_HASH,
            shot + 1,
            base + shot as u64,
        );
    }
    write_jpeg(&source, "unrelated-a.jpg", 0, 31, base + 3_600);
    write_jpeg(&source, "duplicate-a.jpg", DUPLICATE_HASH, 41, base + 4_800);
    write_jpeg(&source, "unrelated-b.jpg", u64::MAX, 51, base + 5_400);
    write_jpeg(
        &source,
        "unrelated-c.jpg",
        0xaaaa_aaaa_aaaa_aaaa,
        61,
        base + 7_200,
    );
    write_jpeg(&source, "duplicate-b.jpg", DUPLICATE_HASH, 71, base + 8_500);

    let screenshot_dir = shots_dir().join("browser");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let shot_stamp = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let shot_prefix = format!("similar-{}-{shot_stamp}", std::process::id());
    let collapsed = screenshot_dir.join(format!("{shot_prefix}-collapsed.png"));
    let expanded = screenshot_dir.join(format!("{shot_prefix}-expanded.png"));
    let copied: Vec<String> = (0..5)
        .map(|shot| shown(&destination.join(format!("burst-{shot}.jpg"))))
        .collect();
    let mut script = format!(
        "limit 180\nnavigate {}\ncmd ViewGrid\nui browser toggle\nui browser wait 120\n\
         ui browser assert-groups 2\n\
         ui browser assert-members burst-0.jpg | burst-0.jpg, burst-1.jpg, burst-2.jpg, burst-3.jpg, burst-4.jpg\n\
         ui browser assert-members duplicate-a.jpg | duplicate-a.jpg, duplicate-b.jpg\n\
         ui viewer thumbs-idle 60\nui browser collapse burst-0.jpg\nwait-frames 3\n\
         ui browser assert-collapsed burst-0.jpg\nscreenshot {}\n\
         ui browser expand burst-0.jpg\nwait-frames 3\nui browser assert-expanded burst-0.jpg\nscreenshot {}\n\
         ui browser collapse burst-0.jpg\nwait-frames 2\n\
         ui browser select-group burst-0.jpg\ncmd ToggleDualPane\nfocus right\nnavigate {}\nfocus left\ncmd CopyToOtherPane\n",
        shown(&source),
        shown(&collapsed),
        shown(&expanded),
        shown(&destination),
    );
    for path in &copied {
        script.push_str(&format!("wait-exists {path} | 60\n"));
    }
    for shot in 0..5 {
        script.push_str(&format!(
            "assert-same {} | {}\n",
            shown(&source.join(format!("burst-{shot}.jpg"))),
            shown(&destination.join(format!("burst-{shot}.jpg")))
        ));
    }
    script.push_str("assert-no-error-toast\nexit\n");

    let outcome = run_app_with(
        &work,
        &profile,
        &script,
        &RunOpts {
            limit: Some(Duration::from_secs(180)),
            ..Default::default()
        },
    );
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(
        collapsed.is_file(),
        "missing collapsed screenshot: {}",
        collapsed.display()
    );
    assert!(
        expanded.is_file(),
        "missing expanded screenshot: {}",
        expanded.display()
    );
    println!(
        "browser grouping screenshots:\n collapsed: {}\n expanded: {}",
        collapsed.display(),
        expanded.display()
    );
}

#[test]
fn corrupt_jpeg_in_burst_does_not_break_grouping_or_crash() {
    let (_temp, work, profile, root) = fixture();
    let source = root.join("corrupt-source");
    fs::create_dir_all(&source).unwrap();
    let base = 1_700_100_000;
    write_jpeg(&source, "valid-before.jpg", BURST_HASH, 1, base);
    let corrupt = source.join("corrupt.jpg");
    write_file(&corrupt, &noise(1_024, 0xface));
    set_mtime(&corrupt, base + 1);
    write_jpeg(&source, "valid-after.jpg", BURST_HASH, 2, base + 2);

    let script = format!(
        "limit 90\nnavigate {}\ncmd ViewGrid\nui browser toggle\nui browser wait 60\n\
         ui browser assert-groups 1\n\
         ui browser assert-members valid-before.jpg | valid-before.jpg, corrupt.jpg, valid-after.jpg\n\
         assert-no-error-toast\nexit\n",
        shown(&source)
    );
    let outcome = run_app_with(&work, &profile, &script, &RunOpts::default());
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn two_thousand_small_images_record_grouping_frame_times() {
    // Frame timings: runs alone, with no other app window running.
    let _alone = exclusive();
    let (_temp, work, profile, root) = fixture();
    let source = root.join("many-images");
    fs::create_dir_all(&source).unwrap();
    let base = 1_701_000_000;
    let mut state = 0x1234_5678u32;
    let variants: Vec<Vec<u8>> = (0..64)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            png_for_hash((state as u64) << 32 | state.rotate_left(13) as u64, state)
        })
        .collect();
    for index in 0..2_000 {
        let path = source.join(format!("image-{index:04}.png"));
        write_file(&path, &variants[index % variants.len()]);
        set_mtime(&path, base + index as u64 * 60);
    }

    let report = root.join("grouping-perf.json");
    let script = format!(
        "limit 360\nnavigate {}\ncmd ViewGrid\nui perf begin\nui browser toggle\n\
         ui browser wait 300\nui browser assert-groups 0\nui perf end {}\nexit\n",
        shown(&source),
        shown(&report)
    );
    let outcome = run_app_with(
        &work,
        &profile,
        &script,
        &RunOpts {
            limit: Some(Duration::from_secs(360)),
            ..Default::default()
        },
    );
    expect_ok(&outcome);
    assert_no_crash(&profile);
    let perf = fs::read_to_string(&report).unwrap();
    println!("2,000-image grouping frame performance:\n{perf}");
}

/// Marks a file hidden the way Explorer does (`attrib +h`), so the listing leaves it out.
fn hide(path: &Path) {
    let status = std::process::Command::new("attrib")
        .arg("+h")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "attrib +h failed for {}", path.display());
}

#[test]
fn hidden_burst_member_is_not_selected_or_copied_with_its_stack() {
    let (_temp, work, profile, root) = fixture();
    let source = root.join("hidden-source");
    let destination = root.join("hidden-destination");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&destination).unwrap();
    let base = 1_702_000_000;
    for shot in 0..3u32 {
        write_jpeg(
            &source,
            &format!("shot-{}.jpg", shot + 1),
            BURST_HASH,
            shot + 1,
            base + shot as u64,
        );
    }
    hide(&source.join("shot-2.jpg"));

    let screenshot_dir = shots_dir().join("browser");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let stack_shot = screenshot_dir.join("similar-hidden-stack.png");
    let copied = destination.join("shot-1.jpg");
    let copied_last = destination.join("shot-3.jpg");
    let hidden_copy = destination.join("shot-2.jpg");
    let script = format!(
        "limit 120\nnavigate {}\ncmd ViewGrid\nui browser toggle\nui browser wait 60\n\
         ui browser assert-groups 1\n\
         ui browser assert-members shot-1.jpg | shot-1.jpg, shot-3.jpg\n\
         wait-frames 3\nscreenshot {}\n\
         ui browser select-group shot-1.jpg\n\
         cmd ToggleDualPane\nfocus right\nnavigate {}\nfocus left\ncmd CopyToOtherPane\n\
         wait-exists {} | 60\nwait-exists {} | 60\nwait-jobs\n\
         assert-missing {}\n\
         assert-same {} | {}\n\
         assert-same {} | {}\n\
         assert-no-error-toast\nexit\n",
        shown(&source),
        shown(&stack_shot),
        shown(&destination),
        shown(&copied),
        shown(&copied_last),
        shown(&hidden_copy),
        shown(&source.join("shot-1.jpg")),
        shown(&copied),
        shown(&source.join("shot-3.jpg")),
        shown(&copied_last),
    );
    let outcome = run_app_with(&work, &profile, &script, &RunOpts::default());
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(
        stack_shot.is_file(),
        "missing stack screenshot: {}",
        stack_shot.display()
    );
    println!("hidden-member stack screenshot: {}", stack_shot.display());
}
