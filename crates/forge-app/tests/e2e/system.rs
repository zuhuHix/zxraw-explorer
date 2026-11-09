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

// Windows-only: the FFmpeg download (Windows builds) is hidden on Linux.
#[cfg(windows)]
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

// Windows-only: the FFmpeg download (Windows builds) is hidden on Linux.
#[cfg(windows)]
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

// Windows-only: the in-app updater installs the Windows setup and is hidden on Linux.
#[cfg(windows)]
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

// Windows-only: the in-app updater installs the Windows setup and is hidden on Linux.
#[cfg(windows)]
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

/// A fake GitHub "latest release" for 99.0.0 with Linux packages: the AppImage download
/// serves `served` while SHA256SUMS-linux.txt lists the checksum of `genuine`.
#[cfg(target_os = "linux")]
fn fake_linux_release(genuine: &[u8], served: Vec<u8>) -> String {
    let appimage = "ZXraw-Explorer-99.0.0-x86_64.AppImage";
    let deb = "zxraw_99.0.0_amd64.deb";
    let sums = format!(
        "{}  {appimage}\n{}  {deb}\n",
        sha256_hex(genuine),
        sha256_hex(b"deb")
    )
    .into_bytes();
    let size = genuine.len();
    serve_routes(|base| {
        let release = serde_json::json!({
            "tag_name": "v99.0.0",
            "html_url": format!("{base}/release"),
            "draft": false,
            "prerelease": false,
            "body": "Fixture release",
            "assets": [
                {"name": appimage, "size": size,
                 "browser_download_url": format!("{base}/download/{appimage}")},
                {"name": deb, "size": 3,
                 "browser_download_url": format!("{base}/download/{deb}")},
                {"name": "SHA256SUMS-linux.txt", "size": sums.len(),
                 "browser_download_url": format!("{base}/download/SHA256SUMS-linux.txt")},
            ],
        });
        vec![
            ("/api".into(), release.to_string().into_bytes()),
            (format!("/download/{appimage}"), served),
            (format!("/download/{deb}"), b"deb".to_vec()),
            ("/download/SHA256SUMS-linux.txt".into(), sums),
        ]
    })
}

/// What the fake AppImage holds: the type 2 AppImage header, so the app accepts it.
#[cfg(target_os = "linux")]
const OLD_APPIMAGE: &[u8] = b"\x7fELF\x02\x01\x01\x00AI\x02 old AppImage";

/// A fake AppImage in the sandbox (never the user's), and the environment that makes the
/// app run as that AppImage against the fake release at `base`. `$APPDIR` is the test
/// binary's folder, so the app sees itself as running from the AppImage's mount.
#[cfg(target_os = "linux")]
fn fake_appimage_run(root: &Path, base: &str) -> (PathBuf, RunOpts) {
    use std::os::unix::fs::PermissionsExt;
    let apps = root.join("Apps");
    std::fs::create_dir_all(&apps).unwrap();
    let appimage = apps.join("ZXraw-Explorer-1.0.0-x86_64.AppImage");
    std::fs::write(&appimage, OLD_APPIMAGE).unwrap();
    std::fs::set_permissions(&appimage, std::fs::Permissions::from_mode(0o755)).unwrap();
    let home = root.join("home");
    std::fs::create_dir_all(home.join("Downloads")).unwrap();
    let opts = RunOpts {
        env: vec![
            ("ZXRAW_UPDATE_API".into(), format!("{base}/api")),
            ("APPIMAGE".into(), shown(&appimage)),
            (
                "APPDIR".into(),
                shown(Path::new(env!("CARGO_BIN_EXE_zxraw")).parent().unwrap()),
            ),
            ("HOME".into(), shown(&home)),
            ("XDG_CONFIG_HOME".into(), shown(&home.join(".config"))),
        ],
        ..Default::default()
    };
    (appimage, opts)
}

#[cfg(target_os = "linux")]
#[test]
fn linux_appimage_update_replaces_the_appimage_and_offers_a_restart() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let new = noise(300_000, 9);
    let base = fake_linux_release(&new, new.clone());
    let (appimage, opts) = fake_appimage_run(sandbox.path(), &base);
    let shots = shots_dir().join("system");
    std::fs::create_dir_all(&shots).unwrap();
    let script = format!(
        "limit 90\ndismiss-notices\nui system wait-update 99.0.0\nwait-frames 10\nscreenshot {}\n\
         ui system update-now\nwait-jobs 30\nwait-frames 5\nassert-no-error-toast\n\
         assert-toast Update installed; restart ZXraw Explorer to use it\n\
         ui system assert-update-status installed\nwait-frames 10\nscreenshot {}\n\
         ui system update-restart\nassert-toast Update installed; would restart\n\
         ui system assert-no-update\nexit\n",
        shown(&shots.join("linux-update-notice.png")),
        shown(&shots.join("linux-update-installed.png")),
    );
    let outcome = run_app_with(sandbox.path(), &profile, &script, &opts);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(std::fs::read(&appimage).unwrap(), new, "the AppImage was replaced");
    let mode = std::fs::metadata(&appimage).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    let left: Vec<_> = std::fs::read_dir(appimage.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left.len(), 1, "no temporary or backup file: {left:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_appimage_update_with_a_tampered_download_is_refused() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let genuine = noise(300_000, 9);
    let mut tampered = genuine.clone();
    tampered[1000] ^= 0x55;
    let base = fake_linux_release(&genuine, tampered);
    let (appimage, opts) = fake_appimage_run(sandbox.path(), &base);
    let script = "limit 90\ndismiss-notices\nui system wait-update 99.0.0\nui system update-now\n\
                  wait-jobs 30\nwait-frames 5\n\
                  assert-toast does not match its SHA-256 checksum, so it was not installed\n\
                  ui system assert-update-status none\nexit\n";
    let outcome = run_app_with(sandbox.path(), &profile, script, &opts);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(std::fs::read(&appimage).unwrap(), OLD_APPIMAGE);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_appimage_in_a_read_only_folder_is_saved_to_downloads() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let new = noise(200_000, 3);
    let base = fake_linux_release(&new, new.clone());
    let (appimage, opts) = fake_appimage_run(sandbox.path(), &base);
    let apps = appimage.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&apps, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::File::create(apps.join("probe")).is_ok() {
        // Running as root: modes do not stop writes, so there is nothing to test.
        std::fs::set_permissions(&apps, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let script = "limit 90\ndismiss-notices\nui system wait-update 99.0.0\nui system update-now\n\
                  wait-jobs 30\nwait-frames 5\nassert-no-error-toast\n\
                  ui system assert-update-status saved-instead\nexit\n";
    let outcome = run_app_with(sandbox.path(), &profile, script, &opts);
    std::fs::set_permissions(&apps, std::fs::Permissions::from_mode(0o755)).unwrap();
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(std::fs::read(&appimage).unwrap(), OLD_APPIMAGE, "left alone");
    let saved = sandbox
        .path()
        .join("home/Downloads/ZXraw-Explorer-99.0.0-x86_64.AppImage");
    assert_eq!(std::fs::read(&saved).unwrap(), new);
    let mode = std::fs::metadata(&saved).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "ready to run");
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
    #[allow(unused_mut)]
    let mut opts = RunOpts {
        limit: Some(std::time::Duration::from_secs(150)),
        ..Default::default()
    };
    // Unix: the emulator comes from $TERMINAL or PATH, and a CI runner has none installed.
    // A stub stands in; the automated run records the launch and never starts it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let stub = sandbox.path().join("fake-terminal");
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        opts.env.push(("TERMINAL".into(), shown(&stub)));
    }
    let outcome = run_app_with(sandbox.path(), &profile, &script, &opts);
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

/// A second `zxraw.exe <folder>` hands the folder to the running window, which opens it in a
/// new tab, and exits 0 without a window; a launch without a path only brings the running
/// window forward. Both use a pipe name of their own, never the installed copy's.
#[cfg(windows)]
#[test]
fn a_second_launch_opens_its_folder_in_a_new_tab_of_the_running_window() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let target = sandbox.path().join("Forwarded folder");
    std::fs::create_dir_all(&target).unwrap();
    write_file(&target.join("marker.txt"), b"forwarded");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let tag = format!("e2e-{}-{nanos}", std::process::id());
    let channel = forge_win::single_instance::Channel::current(&tag).unwrap();
    assert!(!channel.is_owned());

    let script = format!(
        "dismiss-notices\nui system wait-tabs 2 | 30\nassert-path {}\n\
         wait-entry marker.txt\nsleep 500\nassert-tabs 2\nexit\n",
        shown(&target)
    );
    let opts = RunOpts {
        env: vec![("ZXRAW_INSTANCE_NAME".into(), tag.clone())],
        ..Default::default()
    };
    let work = sandbox.path().to_path_buf();
    let first = {
        let (work, profile) = (work.clone(), profile.clone());
        std::thread::spawn(move || run_app_with(&work, &profile, &script, &opts))
    };

    let started = Instant::now();
    while !channel.is_owned() {
        assert!(
            started.elapsed() < Duration::from_secs(60) && !first.is_finished(),
            "the first window never took the instance pipe"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // A launch must hand over or fail (ZXRAW_FORWARD_ONLY): it never opens a window here.
    let launch = |args: &[&std::ffi::OsStr]| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zxraw"))
            .args(args)
            .current_dir(sandbox.path())
            .env("ZXRAW_INSTANCE_NAME", &tag)
            .env("ZXRAW_FORWARD_ONLY", "1")
            .env("ZXRAW_PROFILE_DIR", sandbox.path().join("second-profile"))
            .env_remove("ZXRAW_SCRIPT")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the second zxraw");
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return (status, started.elapsed());
            }
            if started.elapsed() > Duration::from_secs(20) {
                let _ = child.kill();
                panic!("the second launch did not exit");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let (status, _) = launch(&[]);
    assert_eq!(status.code(), Some(0), "a launch without a path is handed over too");
    // Relative to the second process's working directory, as from a terminal.
    let (status, took) = launch(&["Forwarded folder".as_ref()]);
    assert_eq!(status.code(), Some(0), "the folder launch was not handed over");
    assert!(took < Duration::from_secs(10), "handing over took {took:?}");

    let outcome = first.join().unwrap();
    expect_ok(&outcome);
    assert_no_crash(&profile);
}

/// A ZIP with `entries` (name, contents), written with the app's own zip crate.
fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
    use std::io::Write as _;
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, data) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
}

/// Archive files directly in `dir` with `extension`, sorted.
fn archives_in(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(extension)))
        .collect();
    found.sort();
    found
}

/// `zxraw.exe --extract-here <archive>` on a cold start: the new window extracts with the
/// in-app smart rule (two top-level files go into a folder named after the archive).
#[test]
fn a_cold_start_with_extract_here_extracts_with_the_smart_rule() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let media = sandbox.path().join("media");
    std::fs::create_dir_all(&media).unwrap();
    let archive = media.join("pack.zip");
    write_zip(&archive, &[("a.txt", b"alpha"), ("b.txt", b"beta")]);
    let script = format!(
        "dismiss-notices\nwait-exists {} | 60\nwait-exists {} | 60\nassert-no-error-toast\nexit\n",
        shown(&media.join("pack").join("a.txt")),
        shown(&media.join("pack").join("b.txt")),
    );
    let opts = RunOpts {
        args: vec!["--extract-here".into(), archive.clone().into()],
        ..Default::default()
    };
    let outcome = run_app_with(sandbox.path(), &profile, &script, &opts);
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(std::fs::read(media.join("pack").join("b.txt")).unwrap(), b"beta");
    assert!(!media.join("a.txt").exists(), "the smart rule never spills two items next to the archive");
}

/// Starts a window that owns the instance pipe `tag`, runs `script`, and returns once it owns
/// the pipe (with the thread that runs it).
#[cfg(windows)]
fn owner_window(
    sandbox: &Path,
    profile: &Path,
    tag: &str,
    script: String,
) -> std::thread::JoinHandle<Outcome> {
    let channel = forge_win::single_instance::Channel::current(tag).unwrap();
    assert!(!channel.is_owned());
    let opts = RunOpts {
        env: vec![("ZXRAW_INSTANCE_NAME".into(), tag.to_string())],
        ..Default::default()
    };
    let (work, profile) = (sandbox.to_path_buf(), profile.to_path_buf());
    let first = std::thread::spawn(move || run_app_with(&work, &profile, &script, &opts));
    let started = Instant::now();
    while !channel.is_owned() {
        assert!(
            started.elapsed() < Duration::from_secs(60) && !first.is_finished(),
            "the first window never took the instance pipe"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    first
}

#[cfg(windows)]
fn unique_tag(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("e2e-{name}-{}-{nanos}", std::process::id())
}

/// A second process that must hand its launch over (`ZXRAW_FORWARD_ONLY`) and never opens a
/// window of its own.
#[cfg(windows)]
fn forwarding_launch(sandbox: &Path, tag: &str, args: &[&std::ffi::OsStr]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_zxraw"))
        .args(args)
        .current_dir(sandbox)
        .env("ZXRAW_INSTANCE_NAME", tag)
        .env("ZXRAW_FORWARD_ONLY", "1")
        .env("ZXRAW_PROFILE_DIR", sandbox.join("second-profile"))
        .env_remove("ZXRAW_SCRIPT")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the second zxraw")
}

#[cfg(windows)]
fn wait_exit(mut child: std::process::Child, limit: Duration) -> ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            panic!("the second launch did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `zxraw.exe --add-zip a b c` while a window runs: the window makes one ZIP of all three,
/// named after their folder, and the launch exits without a window.
#[cfg(windows)]
#[test]
fn a_forwarded_add_zip_makes_one_archive_of_every_item() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let shoot = sandbox.path().join("Shoot");
    std::fs::create_dir_all(shoot.join("raw")).unwrap();
    for name in ["a.txt", "b.txt", "raw\\c.txt"] {
        write_file(&shoot.join(name), name.as_bytes());
    }
    let zip = shoot.join("Shoot.zip");
    let tag = unique_tag("addzip");
    let script = format!(
        "dismiss-notices\nwait-exists {} | 60\nwait-jobs 60\nsleep 1500\nassert-no-error-toast\nexit\n",
        shown(&zip)
    );
    let first = owner_window(sandbox.path(), &profile, &tag, script);
    let status = wait_exit(
        forwarding_launch(
            &shoot,
            &tag,
            &["--add-zip".as_ref(), "a.txt".as_ref(), "b.txt".as_ref(), "raw".as_ref()],
        ),
        Duration::from_secs(20),
    );
    assert_eq!(status.code(), Some(0), "the command was not handed over");
    let outcome = first.join().unwrap();
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(archives_in(&shoot, "zip"), [zip.clone()], "exactly one archive");
    let mut names: Vec<String> = forge_core::archive::list(&zip)
        .unwrap()
        .entries
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.name.clone())
        .collect();
    names.sort();
    assert_eq!(names, ["a.txt", "b.txt", "raw/c.txt"]);
}

/// What Explorer does for a ZXraw menu command on a multi-selection: COM starts
/// `zxraw.exe --shell-verb add-zip -Embedding`, and Explorer drops the whole selection on
/// the class it registers. Here the test plays Explorer (no registry entries are written:
/// COM finds the class the process registered). The command reaches the running window
/// once: three files make one ZIP.
#[cfg(windows)]
#[test]
fn a_menu_command_on_a_multi_selection_makes_one_archive() {
    use windows::Win32::Foundation::POINTL;
    use windows::Win32::System::Com::{
        CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoTaskMemFree, CoUninitialize, IDataObject,
    };
    use windows::Win32::System::Ole::{DROPEFFECT_COPY, IDropTarget};
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        BHID_DataObject, IShellItemArray, SHCreateShellItemArrayFromIDLists, SHParseDisplayName,
    };
    use windows::core::{GUID, HSTRING};

    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let folder = sandbox.path().join("Selection");
    std::fs::create_dir_all(&folder).unwrap();
    let files: Vec<PathBuf> = (1..=3)
        .map(|i| {
            let path = folder.join(format!("photo {i}.jpg"));
            write_file(&path, format!("photo {i}").as_bytes());
            path
        })
        .collect();
    let zip = folder.join("Selection.zip");
    let tag = unique_tag("menu");
    let script = format!(
        "dismiss-notices\nwait-exists {} | 60\nwait-jobs 60\nsleep 1500\nassert-no-error-toast\nexit\n",
        shown(&zip)
    );
    let first = owner_window(sandbox.path(), &profile, &tag, script);

    let verb = forge_win::MenuVerb::AddZip;
    let server = forwarding_launch(
        sandbox.path(),
        &tag,
        &["--shell-verb".as_ref(), verb.name().as_ref(), "-Embedding".as_ref()],
    );
    let clsid = GUID::try_from(verb.clsid().trim_matches(['{', '}'])).unwrap();
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok().unwrap();
    let target: IDropTarget = {
        let started = Instant::now();
        loop {
            match unsafe { CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER) } {
                Ok(target) => break target,
                Err(_) if started.elapsed() < Duration::from_secs(30) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => panic!("the menu process never registered its class: {e}"),
            }
        }
    };
    let mut pidls = Vec::new();
    for file in &files {
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        unsafe { SHParseDisplayName(&HSTRING::from(file.as_os_str()), None, &mut pidl, 0, None) }
            .unwrap();
        pidls.push(pidl as *const ITEMIDLIST);
    }
    {
        let items: IShellItemArray = unsafe { SHCreateShellItemArrayFromIDLists(&pidls) }.unwrap();
        let data: IDataObject = unsafe { items.BindToHandler(None, &BHID_DataObject) }.unwrap();
        let mut effect = DROPEFFECT_COPY;
        unsafe {
            target
                .DragEnter(&data, MODIFIERKEYS_FLAGS(0), POINTL::default(), &mut effect)
                .unwrap();
            target
                .Drop(&data, MODIFIERKEYS_FLAGS(0), POINTL::default(), &mut effect)
                .unwrap();
        }
    }
    drop(target);
    for pidl in pidls {
        unsafe { CoTaskMemFree(Some(pidl as *const std::ffi::c_void)) };
    }
    unsafe { CoUninitialize() };

    let status = wait_exit(server, Duration::from_secs(30));
    assert_eq!(status.code(), Some(0), "the menu command was not handed to the window");
    let outcome = first.join().unwrap();
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(archives_in(&folder, "zip"), [zip.clone()], "one archive for the whole selection");
    let entries = forge_core::archive::list(&zip).unwrap().entries;
    assert_eq!(entries.iter().filter(|e| !e.is_dir).count(), 3);
}

/// A forwarded `--add-zip` whose selection holds an item that vanished makes no archive of
/// the rest: the window refuses the whole command and names the item.
#[cfg(windows)]
#[test]
fn a_forwarded_add_zip_with_a_vanished_item_makes_no_archive() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let shoot = sandbox.path().join("Shoot");
    std::fs::create_dir_all(&shoot).unwrap();
    write_file(&shoot.join("a.txt"), b"a");
    let tag = unique_tag("addzip-gone");
    let script = "dismiss-notices\nui system wait-toast \"gone.txt\" no longer exists\nsleep 1000\nexit\n";
    let first = owner_window(sandbox.path(), &profile, &tag, script.to_string());
    let status = wait_exit(
        forwarding_launch(&shoot, &tag, &["--add-zip".as_ref(), "a.txt".as_ref(), "gone.txt".as_ref()]),
        Duration::from_secs(20),
    );
    assert_eq!(status.code(), Some(0), "the command was not handed over");
    let outcome = first.join().unwrap();
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(archives_in(&shoot, "zip").is_empty(), "no archive of only part of the selection");
}

/// Settings > Default app on Linux, against temp XDG folders only: the AppImage adds itself to
/// the applications menu, the three switches take their types over (also from a desktop-specific
/// `mimeapps.list` that overrides them), turning a switch off puts the previous apps back, a
/// moved AppImage is noticed, and removing the menu entry leaves nothing pointing at it.
#[cfg(target_os = "linux")]
#[test]
fn linux_default_apps_set_restore_and_menu_entry() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    let profile = root.join("profile");
    let config = root.join("xdg-config");
    let data = root.join("xdg-data");
    let system = root.join("usr-share");
    let entry = |dir: &Path, id: &str, name: &str| {
        write_file(
            &dir.join("applications").join(id),
            format!("[Desktop Entry]\nType=Application\nName={name}\nExec={name}\n").as_bytes(),
        );
    };
    entry(&system, "nemo.desktop", "Files");
    entry(&system, "xviewer.desktop", "Image Viewer");
    write_file(
        &system.join("applications/mimeapps.list"),
        b"[Default Applications]\ninode/directory=nemo.desktop\nimage/png=xviewer.desktop\n",
    );
    let user_list = config.join("mimeapps.list");
    let user_original = "# kept\n[Default Applications]\ntext/plain=gedit.desktop\n\n[Added Associations]\nimage/png=xviewer.desktop;\n";
    write_file(&user_list, user_original.as_bytes());
    let cinnamon = config.join("x-cinnamon-mimeapps.list");
    let cinnamon_original = "[Default Applications]\nimage/jpeg=xviewer.desktop\n";
    write_file(&cinnamon, cinnamon_original.as_bytes());
    let appimage = root.join("Apps/ZXraw Explorer.AppImage");
    write_file(&appimage, b"not really an AppImage");

    let env = |appimage: &Path| RunOpts {
        env: vec![
            ("XDG_CONFIG_HOME".into(), shown(&config)),
            ("XDG_CONFIG_DIRS".into(), shown(&root.join("etc-xdg"))),
            ("XDG_DATA_HOME".into(), shown(&data)),
            ("XDG_DATA_DIRS".into(), shown(&system)),
            ("XDG_CURRENT_DESKTOP".into(), "X-Cinnamon".into()),
            ("APPIMAGE".into(), shown(appimage)),
        ],
        ..RunOpts::default()
    };
    let shots = shots_dir().join("system");
    std::fs::create_dir_all(&shots).unwrap();
    let shot = shots.join("linux-default-apps.png");
    let script = format!(
        "limit 90\ndismiss-notices\nui system default-app-page\nwait-frames 5\n\
         ui system assert-menu-entry missing\n\
         ui system default-app folders on\nui system assert-default-app folders off\n\
         ui system default-app add-menu\nui system assert-menu-entry ours\n\
         ui system default-app folders on\nui system default-app photos on\nui system default-app archives on\n\
         ui system assert-default-app folders on\nui system assert-default-app photos on\n\
         ui system assert-default-app archives on\nwait-frames 30\nscreenshot {}\n\
         ui system default-app photos off\nui system assert-default-app photos off\n\
         ui system default-app folders off\nui system assert-default-app folders off\n\
         ui system assert-default-app archives on\nexit\n",
        shown(&shot)
    );
    let outcome = run_app_with(&root.join("run1"), &profile, &script, &env(&appimage));
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(shot.is_file(), "missing screenshot {}", shot.display());

    let desktop = data.join("applications/zxraw.desktop");
    let text = std::fs::read_to_string(&desktop).unwrap();
    assert!(
        text.contains(&format!("Exec=\"{}\" %f\n", appimage.display())),
        "Exec is the AppImage path: {text}"
    );
    assert!(data.join("icons/hicolor/256x256/apps/zxraw-appimage.png").is_file());
    assert!(text.contains("Icon=zxraw-appimage\n"), "{text}");
    // Photos and folders are back as they were; archives are still ours.
    assert_eq!(std::fs::read_to_string(&cinnamon).unwrap(), cinnamon_original);
    let list = std::fs::read_to_string(&user_list).unwrap();
    assert!(list.starts_with("# kept\n[Default Applications]\ntext/plain=gedit.desktop\n"), "{list}");
    assert!(list.contains("application/zip=zxraw.desktop;\n"), "{list}");
    assert!(!list.contains("inode/directory=") && !list.contains("image/jpeg="), "{list}");
    assert!(list.contains("[Added Associations]\nimage/png=xviewer.desktop;\n"), "{list}");

    // The AppImage moves while running ($APPIMAGE is gone): the entry is not rewritten to the
    // temporary mount. A damaged record refuses the restore and says so.
    let moved = root.join("Elsewhere/ZXraw.AppImage");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(&appimage, &moved).unwrap();
    let record = profile.join("config/linux-default-apps.tsv");
    let good_record = std::fs::read_to_string(&record).expect("archives are recorded");
    std::fs::write(&record, format!("{good_record}not a record line\n")).unwrap();
    let script = "limit 60\ndismiss-notices\nui system default-app-page\nwait-frames 5\n\
                  ui system assert-menu-entry moved\nui system default-app add-menu\n\
                  assert-toast ZXraw was moved while running\nui system assert-menu-entry moved\n\
                  ui system default-app archives off\n\
                  assert-toast Can't restore previous defaults; set them in System Settings\n\
                  ui system assert-default-app archives on\nexit\n";
    let outcome = run_app_with(&root.join("run-stale"), &profile, script, &env(&appimage));
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert_eq!(std::fs::read_to_string(&desktop).unwrap(), text, "entry untouched");
    std::fs::write(&record, &good_record).unwrap();

    // Started again from its new place: the page rewrites the entry, then removes it.
    let script = "limit 60\ndismiss-notices\nui system default-app-page\nwait-frames 5\n\
                  ui system assert-menu-entry moved\nui system default-app add-menu\n\
                  ui system assert-menu-entry ours\nui system default-app remove-menu\n\
                  ui system assert-menu-entry missing\nui system assert-default-app archives off\nexit\n";
    let outcome = run_app_with(&root.join("run2"), &profile, script, &env(&moved));
    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(!desktop.exists(), "menu entry removed");
    assert!(!data.join("icons/hicolor/48x48/apps/zxraw.png").exists(), "icons removed");
    let list = std::fs::read_to_string(&user_list).unwrap();
    assert!(!list.contains("zxraw"), "nothing points at the removed entry: {list}");
}
