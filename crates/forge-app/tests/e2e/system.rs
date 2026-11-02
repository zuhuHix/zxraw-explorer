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
