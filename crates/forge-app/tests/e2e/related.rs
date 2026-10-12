//! End-to-end scenarios: related clips, exports, and settings.

use crate::*;
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};

fn shot(name: &str) -> PathBuf {
    let directory = shots_dir().join("related");
    fs::create_dir_all(&directory).unwrap();
    directory.join(name)
}

fn run_script(root: &Path, profile: &Path, steps: &[String]) -> Outcome {
    static NEXT_WORK: AtomicUsize = AtomicUsize::new(0);
    let work = root.join(format!(
        "related-work-{}",
        NEXT_WORK.fetch_add(1, Ordering::Relaxed)
    ));
    run_app(&work, profile, &steps.join("\n"))
}

fn noise_file(path: &Path, seed: u32) {
    write_file(path, &noise(16_384, seed));
}

fn jpeg_file(path: &Path, color: [u8; 3]) {
    let image = image::RgbImage::from_pixel(64, 48, image::Rgb(color));
    let mut cursor = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut cursor, image::ImageFormat::Jpeg)
        .unwrap();
    write_file(path, &cursor.into_inner());
}

fn path_step(verb: &str, path: &Path) -> String {
    format!("{verb} {}", shown(path))
}

#[test]
fn video_proxy_conventions_and_external_root_scan_are_visible() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    let videos = root.join("videos");
    let external = root.join("external-card");
    let profile = root.join("profile");

    noise_file(&videos.join("GX010001.MP4"), 11);
    noise_file(&videos.join("Proxy").join("GX010001_proxy.mp4"), 12);
    noise_file(&videos.join("Proxies").join("GX010001_Proxy.MOV"), 13);
    noise_file(&videos.join("_Proxies").join("GX010001.proxy.720.mp4"), 14);
    noise_file(&videos.join("GL010001.LRV"), 15);
    noise_file(&videos.join("DJI_0001.MP4"), 16);
    noise_file(&videos.join("DJI_0001.LRF"), 17);
    noise_file(&videos.join("C0001.MP4"), 18);
    noise_file(&videos.join("SUB").join("C0001S03.MP4"), 19);
    // Named so that only the external-root scan can find it; the local Proxy folder has `GX010001_proxy.mp4`.
    noise_file(
        &external
            .join("trip")
            .join("camera")
            .join("GX010001.proxy.mp4"),
        20,
    );
    noise_file(
        &external
            .join("trip")
            .join("camera")
            .join("GX010002_proxy.mp4"),
        21,
    );

    let shots_local = shot("video-related-local.png");
    let shots_dji = shot("video-related-dji.png");
    let shots_external = shot("video-related-located.png");
    let steps = vec![
        path_step("navigate", &videos),
        "cmd TogglePreview".into(),
        "select GX010001.MP4".into(),
        "wait-frames 2".into(),
        "pointer 1400 700".into(),
        "scroll -600".into(),
        "wait-frames 2".into(),
        "ui related assert-proxies GX010001_Proxy.MOV,GX010001.proxy.720.mp4,GL010001.LRV".into(),
        "ui related assert-no-proxies GX010001.proxy.mp4".into(),
        path_step("screenshot", &shots_local),
        "pointer 1400 700".into(),
        "scroll 800".into(),
        "wait-frames 2".into(),
        "ui related assert-proxies GX010001_proxy.mp4".into(),
        "select DJI_0001.MP4".into(),
        "wait-frames 2".into(),
        "ui related assert-proxies DJI_0001.LRF".into(),
        path_step("screenshot", &shots_dji),
        "select C0001.MP4".into(),
        "wait-frames 2".into(),
        "ui related assert-proxies C0001S03.MP4".into(),
        "select GX010001.MP4".into(),
        "wait-frames 2".into(),
        path_step("dialog", &external),
        "ui related proxies-locate".into(),
        // Wait for the asynchronous scan before scrolling; its results extend the preview.
        "ui related assert-no-proxies GX010002_proxy.mp4".into(),
        // Scroll just far enough to bring the located row into the visible Related section.
        "pointer 1250 700".into(),
        "scroll 120".into(),
        "wait-frames 2".into(),
        "ui related assert-proxies GX010001.proxy.mp4".into(),
        path_step("screenshot", &shots_external),
        // A dismissed folder picker must leave the completed results available.
        "dialog cancel".into(),
        "ui related proxies-locate".into(),
        "ui related assert-proxies GX010001.proxy.mp4".into(),
        "assert-no-error-toast".into(),
        "exit".into(),
    ];
    let outcome = run_script(root, &profile, &steps);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn same_folder_selection_and_nearby_export_folders_work() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    let same = root.join("same-folder");
    let raw_folder = root.join("collection").join("RAW");
    let exports_folder = root.join("collection").join("Exports");
    let jpeg_folder = root.join("collection").join("JPEG");
    let profile = root.join("profile");

    jpeg_file(&same.join("IMG_1234.jpg"), [48, 82, 116]);
    jpeg_file(&same.join("IMG_1234-Edit.jpg"), [132, 64, 44]);
    jpeg_file(&same.join("IMG_1234-Edit-2.jpg"), [188, 128, 48]);
    jpeg_file(&same.join("IMG_12345-Edit.jpg"), [80, 92, 104]);
    jpeg_file(&raw_folder.join("IMG_5678.jpg"), [70, 90, 110]);
    jpeg_file(&exports_folder.join("IMG_5678-HDR.jpg"), [160, 92, 30]);
    jpeg_file(&raw_folder.join("IMG_9000.jpg"), [48, 60, 72]);
    jpeg_file(&jpeg_folder.join("IMG_9000-Pano.jpg"), [180, 120, 70]);

    let shots_same = shot("photo-related-same-folder.png");
    let shots_sibling = shot("photo-related-show-in-folder.png");
    let steps = vec![
        path_step("navigate", &same),
        "cmd TogglePreview".into(),
        "select IMG_1234-Edit.jpg".into(),
        "ui related select-originals".into(),
        "ui related assert-selection IMG_1234.jpg".into(),
        "assert-toast Selected 1 counterpart(s)".into(),
        "select IMG_1234.jpg".into(),
        "ui related assert-exports IMG_1234-Edit.jpg,IMG_1234-Edit-2.jpg".into(),
        "ui related select-exports".into(),
        "ui related assert-selection IMG_1234-Edit.jpg,IMG_1234-Edit-2.jpg".into(),
        "assert-toast Selected 2 counterpart(s)".into(),
        path_step("screenshot", &shots_same),
        path_step("navigate", &raw_folder),
        "select IMG_5678.jpg".into(),
        "ui related assert-exports IMG_5678-HDR.jpg".into(),
        path_step("screenshot", &shots_sibling),
        path_step("navigate", &exports_folder),
        "select IMG_5678-HDR.jpg".into(),
        "ui related assert-originals IMG_5678.jpg".into(),
        path_step("navigate", &raw_folder),
        "select IMG_9000.jpg".into(),
        "ui related assert-exports IMG_9000-Pano.jpg".into(),
        "assert-no-error-toast".into(),
        "exit".into(),
    ];
    let outcome = run_script(root, &profile, &steps);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn media_link_settings_rule_is_validated_and_survives_restart() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    let photos = root.join("photos");
    let profile = root.join("profile");
    jpeg_file(&photos.join("IMG_7777.jpg"), [35, 65, 95]);
    jpeg_file(&photos.join("IMG_7777-Retouched.jpg"), [145, 95, 45]);
    let settings_shot = shot("settings-media-links.png");
    let related_shot = shot("settings-rule-related-after-restart.png");

    let first = run_script(
        root,
        &profile,
        &[
            path_step("navigate", &photos),
            "cmd TogglePreview".into(),
            "select IMG_7777.jpg".into(),
            "ui related settings-edit-rule 1 {base}-Retouched".into(),
            path_step("screenshot", &settings_shot),
            "exit".into(),
        ],
    );
    expect_ok(&first);

    // The preview state is saved in the session, so the restarted run already shows it.
    let second = run_script(
        root,
        &profile,
        &[
            path_step("navigate", &photos),
            "select IMG_7777-Retouched.jpg".into(),
            "ui related assert-originals IMG_7777.jpg".into(),
            path_step("screenshot", &related_shot),
            "exit".into(),
        ],
    );
    expect_ok(&second);
    assert_no_crash(&profile);
}

#[test]
fn invalid_media_link_rules_are_rejected_and_defaults_keep_working() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    let photos = root.join("photos");
    let profile = root.join("profile");
    jpeg_file(&photos.join("IMG_6100.jpg"), [40, 70, 100]);
    jpeg_file(&photos.join("IMG_6100-Edit.jpg"), [140, 90, 40]);

    // A path separator and an unknown placeholder are both refused by the Settings validator.
    let separator = run_script(
        root,
        &profile,
        &[
            path_step("navigate", &photos),
            "cmd TogglePreview".into(),
            "select IMG_6100.jpg".into(),
            "ui related settings-edit-rule 1 {base}/Retouched".into(),
            "exit".into(),
        ],
    );
    expect_script_error(&separator, "without path separators");

    let unknown = run_script(
        root,
        &profile,
        &[
            path_step("navigate", &photos),
            "cmd TogglePreview".into(),
            "select IMG_6100.jpg".into(),
            "ui related settings-edit-rule 1 {base}-{unknown}".into(),
            "exit".into(),
        ],
    );
    expect_script_error(&unknown, "placeholders are supported");

    // Row 1 is still `{base}-Edit` because neither edit was applied.
    let after = run_script(
        root,
        &profile,
        &[
            path_step("navigate", &photos),
            "cmd TogglePreview".into(),
            "select IMG_6100.jpg".into(),
            "ui related assert-exports IMG_6100-Edit.jpg".into(),
            "exit".into(),
        ],
    );
    expect_ok(&after);
    assert_no_crash(&profile);
}

#[test]
fn select_commands_report_when_no_counterpart_is_listed() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    let folder = root.join("solo");
    let profile = root.join("profile");
    jpeg_file(&folder.join("IMG_3000.jpg"), [70, 70, 70]);
    jpeg_file(&folder.join("IMG_3001.jpg"), [90, 60, 60]);
    jpeg_file(&folder.join("IMG_3001-Edit.jpg"), [120, 60, 60]);
    let shots_none = shot("select-no-counterpart.png");

    let steps = vec![
        path_step("navigate", &folder),
        "cmd TogglePreview".into(),
        // IMG_3000 has no export in this folder.
        "select IMG_3000.jpg".into(),
        "ui related select-exports".into(),
        "ui related wait-selection".into(),
        "assert-toast No matching exports are in this folder".into(),
        // IMG_3001 is itself an original, and its export is not an original of IMG_3001.
        "select IMG_3001.jpg".into(),
        "ui related select-originals".into(),
        "ui related wait-selection".into(),
        "assert-toast No matching originals are in this folder".into(),
        "assert-no-error-toast".into(),
        path_step("screenshot", &shots_none),
        "exit".into(),
    ];
    let outcome = run_script(root, &profile, &steps);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn an_export_made_while_the_original_is_shown_appears_after_the_folder_refreshes() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    let folder = root.join("shoot");
    let profile = root.join("profile");
    jpeg_file(&folder.join("IMG_4242.jpg"), [60, 80, 100]);
    jpeg_file(&folder.join("IMG_4242-Edit.jpg"), [140, 70, 40]);
    let late_export = folder.join("IMG_4242-Edit-2.jpg");

    let steps = vec![
        path_step("navigate", &folder),
        "select IMG_4242.jpg".into(),
        "ui related assert-exports IMG_4242-Edit.jpg".into(),
        // Another app saves a second export; the watcher refreshes the folder.
        path_step("ui tools external create", &late_export),
        "wait-entry IMG_4242-Edit-2.jpg".into(),
        "select IMG_4242.jpg".into(),
        "ui related assert-exports IMG_4242-Edit.jpg,IMG_4242-Edit-2.jpg".into(),
        "exit".into(),
    ];
    let outcome = run_script(root, &profile, &steps);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}
