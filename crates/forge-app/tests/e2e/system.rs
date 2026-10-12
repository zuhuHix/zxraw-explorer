//! End-to-end scenarios: system.

#[allow(unused_imports)]
use crate::*;

#[cfg(windows)]
#[test]
fn heic_thumbnail_is_visible_in_grid_and_viewer() {
    let sandbox = tempfile::tempdir().unwrap();
    let media = sandbox.path().join("media");
    let profile = sandbox.path().join("profile");
    std::fs::create_dir_all(&media).unwrap();
    let fixture = media.join("generated.heic");
    match forge_media::wic::encode_test_fixture(&fixture, forge_media::wic::TestContainer::Heif) {
        Ok(()) => {}
        Err(forge_media::MediaError::Unsupported(reason)) => {
            eprintln!("skipped HEIC grid/viewer scenario: {reason}");
            return;
        }
        Err(error) => panic!("could not encode a WIC HEIC fixture: {error}"),
    }

    let shots = shots_dir().join("system");
    std::fs::create_dir_all(&shots).unwrap();
    let grid_shot = shots.join("heic-grid.png");
    let viewer_shot = shots.join("heic-viewer.png");
    let script = format!(
        "limit 120\ndismiss-notices\nnavigate {}\ncmd ViewGrid\n\
         ui system wait-thumbnail generated.heic | 45\n\
         ui system assert-thumbnail-ready generated.heic\n\
         screenshot {}\nselect generated.heic\ncmd Open\n\
         ui viewer assert-open single\nui viewer assert-current generated.heic\n\
         ui viewer wait-decoded 45\nscreenshot {}\nexit\n",
        shown(&media),
        shown(&grid_shot),
        shown(&viewer_shot),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(
        grid_shot.is_file(),
        "missing grid screenshot: {}",
        grid_shot.display()
    );
    assert!(
        viewer_shot.is_file(),
        "missing viewer screenshot: {}",
        viewer_shot.display()
    );
}

#[test]
fn corrupt_heic_shows_thumbnail_and_viewer_errors_without_crashing() {
    let sandbox = tempfile::tempdir().unwrap();
    let media = sandbox.path().join("media");
    let profile = sandbox.path().join("profile");
    std::fs::create_dir_all(&media).unwrap();
    write_file(&media.join("corrupt.heic"), &noise(4096, 0x41C0));

    let shots = shots_dir().join("system");
    std::fs::create_dir_all(&shots).unwrap();
    let grid_shot = shots.join("corrupt-heic-grid.png");
    let viewer_shot = shots.join("corrupt-heic-viewer.png");
    let script = format!(
        "limit 120\ndismiss-notices\nnavigate {}\ncmd ViewGrid\n\
         ui system wait-thumbnail corrupt.heic | 45\n\
         ui system assert-thumbnail-error corrupt.heic\nscreenshot {}\n\
         select corrupt.heic\ncmd Open\nui viewer assert-open single\n\
         ui viewer assert-current corrupt.heic\nui viewer wait-decoded 45\n\
         ui viewer assert-error-shown\nscreenshot {}\nexit\n",
        shown(&media),
        shown(&grid_shot),
        shown(&viewer_shot),
    );
    let outcome = run_app(sandbox.path(), &profile, &script);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(
        grid_shot.is_file(),
        "missing grid screenshot: {}",
        grid_shot.display()
    );
    assert!(
        viewer_shot.is_file(),
        "missing viewer screenshot: {}",
        viewer_shot.display()
    );
}

/// Serves `body` over plain HTTP on the loopback address to every request; returns the URL.
fn serve_once_per_request(body: Vec<u8>) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    format!("http://127.0.0.1:{port}/ffmpeg.zip")
}

fn fake_ffmpeg_zip() -> Vec<u8> {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in [
        ("bin/ffmpeg.exe", &b"not really ffmpeg"[..]),
        ("bin/ffprobe.exe", b"not really ffprobe"),
        ("LICENSE", b"GPL"),
    ] {
        w.start_file(
            format!("ffmpeg-e2e/{name}"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn download_ffmpeg_installs_the_tools_and_points_settings_at_them() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let zip = fake_ffmpeg_zip();
    let package = format!(
        "{}|{}|{}|ffmpeg-e2e|e2e",
        serve_once_per_request(zip.clone()),
        zip.len(),
        sha256_hex(&zip)
    );
    let installed = profile.join("data").join("tools").join("ffmpeg-e2e");
    let script = format!(
        "dismiss-notices\ncmd DownloadFfmpeg\naccept\nwait-jobs 30\n\
         wait-exists {}\nwait-frames 5\nassert-toast FFmpeg installed\n\
         assert-no-error-toast\nexit\n",
        shown(&installed.join("ffprobe.exe")),
    );
    let outcome = run_app_with(
        sandbox.path(),
        &profile,
        &script,
        &RunOpts {
            env: vec![("ZXRAW_FFMPEG_PACKAGE".into(), package)],
            ..Default::default()
        },
    );
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(
        std::fs::read(installed.join("ffmpeg.exe")).unwrap(),
        b"not really ffmpeg"
    );
    assert!(installed.join("LICENSE.txt").is_file());
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(profile.join("config").join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        settings["ffmpeg_dir"].as_str().map(std::path::PathBuf::from),
        Some(installed.clone()),
        "Settings › Video points at the downloaded tools"
    );
}

#[test]
fn download_ffmpeg_with_a_bad_checksum_installs_nothing_and_says_why() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let zip = fake_ffmpeg_zip();
    let package = format!(
        "{}|{}|{}|ffmpeg-e2e|e2e",
        serve_once_per_request(zip.clone()),
        zip.len(),
        "0".repeat(64)
    );
    let script = "dismiss-notices\ncmd DownloadFfmpeg\naccept\nwait-jobs 30\nwait-frames 5\n\
                  assert-toast does not match the expected SHA-256\nexit\n";
    let outcome = run_app_with(
        sandbox.path(),
        &profile,
        script,
        &RunOpts {
            env: vec![("ZXRAW_FFMPEG_PACKAGE".into(), package)],
            ..Default::default()
        },
    );
    expect_ok(&outcome);
    assert_no_crash(&profile);
    let tools = profile.join("data").join("tools");
    let left: Vec<_> = std::fs::read_dir(&tools)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "nothing installed or left behind: {left:?}");
    let settings = std::fs::read_to_string(profile.join("config").join("settings.json"))
        .unwrap_or_default();
    assert!(
        !settings.contains("ffmpeg-e2e"),
        "settings are unchanged: {settings}"
    );
}
