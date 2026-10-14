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

/// Serves fixed bodies by path over plain HTTP on the loopback address; unknown paths get
/// 404. `routes` receives the server's base URL so bodies can link back to it.
fn serve_routes(routes: impl FnOnce(&str) -> Vec<(String, Vec<u8>)>) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let routes = routes(&base);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut request = [0u8; 4096];
            let n = stream.read(&mut request).unwrap_or(0);
            let head = String::from_utf8_lossy(&request[..n]).into_owned();
            let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
            let response = match routes.iter().find(|(p, _)| *p == path) {
                Some((_, body)) => {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(body);
                    r
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = stream.write_all(&response);
        }
    });
    base
}

/// A fake GitHub "latest release" for 99.0.0 whose installer download serves `served`
/// while SHA256SUMS.txt lists the checksum of `genuine`.
fn fake_release(genuine: &[u8], served: Vec<u8>) -> String {
    let name = "ZXraw-Explorer-99.0.0-Setup-x64.exe";
    let sums = format!("{}  {name}\n", sha256_hex(genuine)).into_bytes();
    let size = genuine.len();
    serve_routes(|base| {
        let release = serde_json::json!({
            "tag_name": "v99.0.0",
            "html_url": format!("{base}/release"),
            "draft": false,
            "prerelease": false,
            "body": "Fixture release",
            "assets": [
                {"name": name, "size": size,
                 "browser_download_url": format!("{base}/download/{name}")},
                {"name": "SHA256SUMS.txt", "size": sums.len(),
                 "browser_download_url": format!("{base}/download/SHA256SUMS.txt")},
            ],
        });
        vec![
            ("/api".into(), release.to_string().into_bytes()),
            (format!("/download/{name}"), served),
            ("/download/SHA256SUMS.txt".into(), sums),
        ]
    })
}

fn update_opts(base: &str) -> RunOpts {
    RunOpts {
        env: vec![
            ("ZXRAW_UPDATE_API".into(), format!("{base}/api")),
            // Behave like the installed copy so "Update now" downloads the installer.
            ("ZXRAW_UPDATE_INSTALLED".into(), "1".into()),
        ],
        ..Default::default()
    }
}

#[test]
fn update_check_offers_a_newer_release_and_verifies_the_installer() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let installer = noise(300_000, 7);
    let base = fake_release(&installer, installer.clone());
    let shot = shots_dir().join("update-notice.png");
    let script = format!(
        "dismiss-notices\nui system wait-update 99.0.0\nwait-frames 10\nscreenshot {}\n\
         ui system update-now\nwait-jobs 30\nwait-frames 5\n\
         assert-toast Update verified; would run\nassert-no-error-toast\n\
         ui system assert-no-update\nexit\n",
        shown(&shot)
    );
    let outcome = run_app_with(sandbox.path(), &profile, &script, &update_opts(&base));
    expect_ok(&outcome);
    assert_no_crash(&profile);
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(profile.join("config").join("settings.json")).unwrap(),
    )
    .unwrap();
    assert!(
        settings["last_update_check"].as_i64().is_some(),
        "the check time is remembered for the daily throttle: {settings}"
    );
}

#[test]
fn update_with_a_tampered_installer_is_refused() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let installer = noise(300_000, 7);
    let mut tampered = installer.clone();
    tampered[1000] ^= 0x55;
    let base = fake_release(&installer, tampered);
    let script = "dismiss-notices\nui system wait-update 99.0.0\nui system update-now\n\
                  wait-jobs 30\nwait-frames 5\n\
                  assert-toast does not match its SHA-256 checksum, so it was not run\nexit\n";
    let outcome = run_app_with(sandbox.path(), &profile, script, &update_opts(&base));
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

#[test]
fn folder_context_menu_opens_a_terminal_in_that_folder() {
    let sandbox = tempfile::tempdir().unwrap();
    let media = sandbox.path().join("media");
    let profile = sandbox.path().join("profile");
    // Characters cmd, PowerShell and Windows Terminal each treat specially.
    let folder = media.join("Shoot & 100% ^ it's ;x");
    std::fs::create_dir_all(&folder).unwrap();

    // Automated runs only record the launch, so no terminal window opens.
    let script = format!(
        "limit 120\ndismiss-notices\nnavigate {}\nwait-entry Shoot & 100% ^ it's ;x\n\
         right-click-entry Shoot & 100% ^ it's ;x\n\
         ui system click-menu Open terminal here\n\
         ui system wait-terminal {} | 60\n\
         assert-no-error-toast\nexit\n",
        shown(&media),
        shown(&folder),
    );
    // Generous limits: the whole e2e suite runs several app windows at once.
    let opts = RunOpts {
        limit: Some(std::time::Duration::from_secs(150)),
        ..Default::default()
    };
    let outcome = run_app_with(sandbox.path(), &profile, &script, &opts);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}
