//! "Download FFmpeg": fetches one pinned FFmpeg build, checks its size and SHA-256, and
//! installs only `ffmpeg.exe`, `ffprobe.exe` and the licence into the profile's data
//! folder, then points Settings › Video at it.
//!
//! This is the app's only network access, and it happens only after the user confirms.
//! The build is pinned (URL, size and checksum are compiled in), so a changed or
//! tampered file on the server is refused rather than installed. Nothing is written
//! outside `<data>\tools`, and a failed or cancelled download leaves nothing behind.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use forge_jobs::{JobHandle, JobState};
use sha2::{Digest, Sha256};

use crate::actions::Event;
use crate::app::ZxrawApp;
use crate::modal::{Confirm, ConfirmKind, Modal};

/// A downloadable FFmpeg build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub version: String,
    pub url: String,
    /// Exact size of the zip in bytes.
    pub size: u64,
    /// Lowercase hex SHA-256 of the zip.
    pub sha256: String,
    /// Folder inside the zip holding `bin\` and `LICENSE`.
    pub root: String,
}

/// Gyan Doshi's "essentials" build (the Windows build linked from ffmpeg.org), as
/// published on GitHub. It includes libx264, which proxy generation needs.
fn pinned() -> Package {
    Package {
        version: "9.0.2".into(),
        url: "https://github.com/GyanD/codexffmpeg/releases/download/9.0.2/ffmpeg-9.0.2-essentials_build.zip".into(),
        size: 114_768_076,
        sha256: "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba".into(),
        root: "ffmpeg-9.0.2-essentials_build".into(),
    }
}

/// The package to install. Debug and automation builds accept
/// `ZXRAW_FFMPEG_PACKAGE=url|size|sha256|root|version` so tests can serve a small zip from
/// this PC; release builds always use the pinned build.
pub fn package() -> Package {
    #[cfg(any(debug_assertions, feature = "automation"))]
    if let Some(p) = std::env::var("ZXRAW_FFMPEG_PACKAGE")
        .ok()
        .and_then(|v| parse_override(&v))
    {
        return p;
    }
    pinned()
}

#[cfg(any(debug_assertions, feature = "automation"))]
fn parse_override(value: &str) -> Option<Package> {
    let parts: Vec<&str> = value.split('|').collect();
    let [url, size, sha256, root, version] = parts.as_slice() else {
        tracing::warn!("ignoring malformed ZXRAW_FFMPEG_PACKAGE");
        return None;
    };
    Some(Package {
        version: version.to_string(),
        url: url.to_string(),
        size: size.parse().ok()?,
        sha256: sha256.to_ascii_lowercase(),
        root: root.to_string(),
    })
}

/// Where a package is installed: `<data>\tools\ffmpeg-<version>`.
pub fn install_dir(tools_dir: &Path, package: &Package) -> PathBuf {
    tools_dir.join(format!("ffmpeg-{}", package.version))
}

fn is_installed(dir: &Path) -> bool {
    dir.join("ffmpeg.exe").is_file() && dir.join("ffprobe.exe").is_file()
}

/// The running download, if any.
#[derive(Default)]
pub struct FfmpegSetup {
    job: Option<JobHandle>,
}

impl FfmpegSetup {
    pub fn is_running(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(|job| !job.state().is_finished())
    }

    pub fn status(&self) -> Status {
        match &self.job {
            Some(job) if !job.state().is_finished() => Status {
                running: true,
                fraction: (job.state() == JobState::Running)
                    .then(|| job.progress.snapshot().fraction())
                    .flatten(),
            },
            _ => Status::default(),
        }
    }
}

/// What the download offer shows: whether a download runs and how far along it is.
#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    pub running: bool,
    pub fraction: Option<f32>,
}

/// The "Download FFmpeg…" offer shared by Settings, the welcome window and the video
/// panel: a progress line while downloading, otherwise a short note and the button.
pub fn offer(ui: &mut egui::Ui, status: Status, on_click: impl FnOnce()) {
    if status.running {
        ui.horizontal(|ui| {
            match status.fraction {
                Some(f) => {
                    ui.add(egui::ProgressBar::new(f).desired_width(160.0).show_percentage());
                }
                None => {
                    ui.spinner();
                }
            }
            ui.label("Downloading FFmpeg… (cancel it under Operations)");
        });
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    } else {
        let package = package();
        ui.label(
            egui::RichText::new(format!(
                "ZXraw Explorer can download FFmpeg {} for you ({}, checksum-verified).",
                package.version,
                megabytes(package.size)
            ))
            .weak(),
        );
        if ui.button("Download FFmpeg…").clicked() {
            on_click();
        }
    }
}

/// Size as shown to the user, e.g. "115 MB".
fn megabytes(bytes: u64) -> String {
    format!("{} MB", bytes.div_ceil(1_000_000))
}

/// Asks before downloading (the `DownloadFfmpeg` command).
pub fn confirm(app: &mut ZxrawApp) {
    if app.ops.ffmpeg_setup.is_running() {
        app.toast("FFmpeg is already downloading — see Operations");
        return;
    }
    let package = package();
    let dir = install_dir(&app.paths.data.join("tools"), &package);
    let host = package
        .url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("the internet");
    app.ops.modal = Some(Modal::Confirm(Confirm {
        kind: ConfirmKind::DownloadFfmpeg,
        title: "Download FFmpeg?".into(),
        body: format!(
            "ZXraw Explorer will download FFmpeg {version} ({size}) from {host} and install \
             ffmpeg.exe and ffprobe.exe into:\n{dir}\n\n\
             The file's SHA-256 checksum is checked before anything is installed. This is the \
             only time the app connects to the internet, and only because you asked.\n\n\
             FFmpeg is free software (GPL v3) from the FFmpeg project, built by Gyan Doshi; \
             its licence is installed alongside it.",
            version = package.version,
            size = megabytes(package.size),
            dir = dir.display(),
        ),
        confirm_label: "Download".into(),
        danger: false,
    }));
}

/// Starts the download job after the user confirmed.
pub fn start(app: &mut ZxrawApp) {
    if app.ops.ffmpeg_setup.is_running() {
        return;
    }
    let package = package();
    let tools_dir = app.paths.data.join("tools");
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    let title = format!("Downloading FFmpeg {}", package.version);
    let job = app.ops.jobs.spawn("ffmpeg", title, move |job| {
        let result = install(&package, &tools_dir, job);
        let event = match &result {
            Ok(dir) => Event::FfmpegInstalled(dir.clone()),
            Err(_) if job.cancel.is_cancelled() => Event::Toast("FFmpeg download cancelled".into()),
            Err(e) => Event::Error(format!("Could not install FFmpeg: {e}")),
        };
        let _ = tx.send(event);
        ctx.request_repaint();
        result.map(|_| ())
    });
    app.ops.ffmpeg_setup.job = Some(job);
    app.toast("Downloading FFmpeg — see Operations");
}

/// Downloads, verifies and installs `package` under `tools_dir`; returns the folder that
/// holds the executables. An existing complete install is reused without downloading.
pub fn install(package: &Package, tools_dir: &Path, job: &JobHandle) -> Result<PathBuf, String> {
    let final_dir = install_dir(tools_dir, package);
    if is_installed(&final_dir) {
        job.log("already installed; nothing downloaded");
        return Ok(final_dir);
    }
    fs::create_dir_all(tools_dir)
        .map_err(|e| format!("cannot create {}: {e}", tools_dir.display()))?;
    sweep_stale(tools_dir);
    let tag = format!("{TEMP_PREFIX}{}-{}", package.version, std::process::id());
    let part = tools_dir.join(format!("{tag}.zip.part"));
    let staging = tools_dir.join(format!("{tag}.staging"));
    let _cleanup = Cleanup(vec![part.clone(), staging.clone()]);

    job.progress.add_total_bytes(package.size);
    job.progress
        .set_current(format!("Downloading {}", megabytes(package.size)));
    download_verified(package, &part, job)?;

    job.progress.set_current("Unpacking ffmpeg.exe and ffprobe.exe");
    extract(package, &part, &staging, job)?;
    if !is_installed(&staging) {
        return Err("the download did not contain ffmpeg.exe and ffprobe.exe".into());
    }
    // Our own folder from an earlier, incomplete attempt; a complete one returned above.
    if final_dir.exists() {
        fs::remove_dir_all(&final_dir)
            .map_err(|e| format!("cannot replace {}: {e}", final_dir.display()))?;
    }
    fs::rename(&staging, &final_dir)
        .map_err(|e| format!("cannot finish installing into {}: {e}", final_dir.display()))?;
    job.log(format!("installed into {}", final_dir.display()));
    Ok(final_dir)
}

/// Name prefix of the temporary download and staging entries in the tools folder.
const TEMP_PREFIX: &str = ".ffmpeg-";

/// Temporary entries older than this are leftovers from an app that exited mid-download.
/// Younger ones may belong to another running instance and are left alone.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Deletes stale temporary downloads (only our own `.ffmpeg-*` entries).
fn sweep_stale(tools_dir: &Path) {
    let Ok(entries) = fs::read_dir(tools_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(TEMP_PREFIX))
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > STALE_AFTER);
        if stale {
            let path = entry.path();
            let _ = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
        }
    }
}

/// Removes leftover temporary files and folders when the install ends, however it ends.
struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = if path.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
        }
    }
}

enum Stop {
    Cancelled,
    TooLarge,
    Write(std::io::Error),
}

fn download_verified(package: &Package, part: &Path, job: &JobHandle) -> Result<(), String> {
    let file = File::create(part).map_err(|e| format!("cannot create {}: {e}", part.display()))?;
    let mut out = BufWriter::with_capacity(1 << 20, file);
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut stop = None;
    let result = forge_win::http::download(&package.url, |chunk| {
        if job.cancel.is_cancelled() {
            stop = Some(Stop::Cancelled);
            return ControlFlow::Break(());
        }
        received += chunk.len() as u64;
        if received > package.size {
            stop = Some(Stop::TooLarge);
            return ControlFlow::Break(());
        }
        if let Err(e) = out.write_all(chunk) {
            stop = Some(Stop::Write(e));
            return ControlFlow::Break(());
        }
        hasher.update(chunk);
        job.progress.add_bytes(chunk.len() as u64);
        ControlFlow::Continue(())
    });
    match (stop, result) {
        (Some(Stop::Cancelled), _) => return Err("cancelled".into()),
        (Some(Stop::TooLarge), _) => {
            return Err(format!(
                "the server sent more than the expected {} bytes; nothing was installed",
                package.size
            ));
        }
        (Some(Stop::Write(e)), _) => return Err(format!("cannot save the download: {e}")),
        (None, Err(e)) => return Err(e.to_string()),
        (None, Ok(_)) => {}
    }
    if received != package.size {
        return Err(format!(
            "the download stopped early ({received} of {} bytes); try again",
            package.size
        ));
    }
    let digest = hex(&hasher.finalize());
    if digest != package.sha256 {
        return Err(format!(
            "the downloaded file does not match the expected SHA-256 checksum, so it was \
             not installed (got {digest})"
        ));
    }
    let file = out.into_inner().map_err(|e| format!("cannot save the download: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("cannot save the download: {e}"))?;
    job.log(format!("downloaded {} bytes, SHA-256 {digest}", package.size));
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Files taken from the zip: `(path inside the package root, installed name, required)`.
const WANTED: &[(&str, &str, bool)] = &[
    ("bin/ffmpeg.exe", "ffmpeg.exe", true),
    ("bin/ffprobe.exe", "ffprobe.exe", true),
    ("LICENSE", "LICENSE.txt", false),
    ("README.txt", "README.txt", false),
];

/// Largest single file accepted from the zip (the real executables are ~100 MB).
const MAX_ENTRY: u64 = 1 << 30;

fn extract(package: &Package, zip_path: &Path, staging: &Path, job: &JobHandle) -> Result<(), String> {
    let file = File::open(zip_path).map_err(|e| format!("cannot reopen the download: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("the download is not a valid zip: {e}"))?;
    fs::create_dir_all(staging).map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
    for (inner, name, required) in WANTED {
        let entry_name = format!("{}/{inner}", package.root);
        let mut entry = match archive.by_name(&entry_name) {
            Ok(entry) => entry,
            Err(zip::result::ZipError::FileNotFound) if !required => continue,
            Err(e) => return Err(format!("{entry_name} is missing from the download: {e}")),
        };
        if entry.size() > MAX_ENTRY {
            return Err(format!("{entry_name} is unexpectedly large"));
        }
        let target = staging.join(name);
        let mut out = File::create(&target)
            .map_err(|e| format!("cannot create {}: {e}", target.display()))?;
        let mut buf = vec![0u8; 1 << 20];
        let mut written = 0u64;
        loop {
            if job.cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            let n = entry
                .read(&mut buf)
                .map_err(|e| format!("cannot unpack {entry_name}: {e}"))?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > MAX_ENTRY {
                return Err(format!("{entry_name} is unexpectedly large"));
            }
            out.write_all(&buf[..n])
                .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
        }
        out.sync_all()
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    }
    let source = format!(
        "FFmpeg {} downloaded by ZXraw Explorer\r\nfrom {}\r\nSHA-256 {}\r\n",
        package.version, package.url, package.sha256
    );
    fs::write(staging.join("SOURCE.txt"), source)
        .map_err(|e| format!("cannot write {}: {e}", staging.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;

    fn zip_with(root: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in files {
            w.start_file(
                format!("{root}/{name}"),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// Serves `body` to every request; returns the URL.
    fn serve(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
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

    fn package_for(body: &[u8], url: String) -> Package {
        Package {
            version: "test".into(),
            url,
            size: body.len() as u64,
            sha256: hex(&Sha256::digest(body)),
            root: "ffmpeg-test".into(),
        }
    }

    fn job() -> JobHandle {
        forge_jobs::JobRegistry::new().spawn("test", "test", |_| Ok(()))
    }

    fn leftovers(tools: &Path) -> Vec<String> {
        fs::read_dir(tools)
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn installs_only_the_tools_and_licence() {
        let body = zip_with(
            "ffmpeg-test",
            &[
                ("bin/ffmpeg.exe", b"ffmpeg"),
                ("bin/ffprobe.exe", b"ffprobe"),
                ("bin/ffplay.exe", b"ffplay"),
                ("LICENSE", b"GPL"),
                ("doc/ffmpeg.html", b"docs"),
            ],
        );
        let package = package_for(&body, serve(body.clone()));
        let tmp = tempfile::tempdir().unwrap();
        let tools = tmp.path().join("tools");

        let dir = install(&package, &tools, &job()).unwrap();
        assert_eq!(dir, tools.join("ffmpeg-test"));
        assert_eq!(fs::read(dir.join("ffmpeg.exe")).unwrap(), b"ffmpeg");
        assert_eq!(fs::read(dir.join("ffprobe.exe")).unwrap(), b"ffprobe");
        assert_eq!(fs::read(dir.join("LICENSE.txt")).unwrap(), b"GPL");
        assert!(!dir.join("ffplay.exe").exists());
        assert!(fs::read_to_string(dir.join("SOURCE.txt")).unwrap().contains(&package.sha256));
        assert_eq!(leftovers(&tools), ["ffmpeg-test"], "no temporary files remain");
        let tools_found = forge_media::FfTools::discover_in(std::slice::from_ref(&dir));
        assert!(tools_found.has_ffmpeg() && tools_found.has_ffprobe());

        // A second run reuses the install without downloading (the URL is now dead).
        let offline = Package {
            url: "http://127.0.0.1:1/gone.zip".into(),
            ..package
        };
        assert_eq!(install(&offline, &tools, &job()).unwrap(), dir);
    }

    #[test]
    fn a_checksum_mismatch_installs_nothing() {
        let body = zip_with(
            "ffmpeg-test",
            &[("bin/ffmpeg.exe", b"x"), ("bin/ffprobe.exe", b"y")],
        );
        let mut package = package_for(&body, serve(body.clone()));
        package.sha256 = "0".repeat(64);
        let tmp = tempfile::tempdir().unwrap();
        let tools = tmp.path().join("tools");

        let err = install(&package, &tools, &job()).unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert!(leftovers(&tools).is_empty(), "{:?}", leftovers(&tools));
    }

    #[test]
    fn a_wrong_size_installs_nothing() {
        let body = zip_with(
            "ffmpeg-test",
            &[("bin/ffmpeg.exe", b"x"), ("bin/ffprobe.exe", b"y")],
        );
        let url = serve(body.clone());
        let tmp = tempfile::tempdir().unwrap();
        let tools = tmp.path().join("tools");

        let mut smaller = package_for(&body, url.clone());
        smaller.size -= 1;
        let err = install(&smaller, &tools, &job()).unwrap_err();
        assert!(err.contains("more than the expected"), "{err}");

        let mut larger = package_for(&body, url);
        larger.size += 1;
        let err = install(&larger, &tools, &job()).unwrap_err();
        assert!(err.contains("stopped early"), "{err}");
        assert!(leftovers(&tools).is_empty(), "{:?}", leftovers(&tools));
    }

    #[test]
    fn a_zip_without_the_tools_installs_nothing() {
        let body = zip_with("ffmpeg-test", &[("bin/ffmpeg.exe", b"x")]);
        let package = package_for(&body, serve(body.clone()));
        let tmp = tempfile::tempdir().unwrap();
        let tools = tmp.path().join("tools");

        let err = install(&package, &tools, &job()).unwrap_err();
        assert!(err.contains("ffprobe.exe"), "{err}");
        assert!(leftovers(&tools).is_empty(), "{:?}", leftovers(&tools));
    }

    #[test]
    fn cancelling_stops_and_cleans_up() {
        let body = zip_with(
            "ffmpeg-test",
            &[("bin/ffmpeg.exe", b"x"), ("bin/ffprobe.exe", b"y")],
        );
        let package = package_for(&body, serve(body.clone()));
        let tmp = tempfile::tempdir().unwrap();
        let tools = tmp.path().join("tools");
        let job = job();
        job.cancel.cancel();

        assert_eq!(install(&package, &tools, &job).unwrap_err(), "cancelled");
        assert!(leftovers(&tools).is_empty(), "{:?}", leftovers(&tools));
    }

    #[test]
    fn the_pinned_build_is_https_and_well_formed() {
        let p = pinned();
        assert!(p.url.starts_with("https://github.com/"));
        assert!(p.url.contains(&p.version) && p.root.contains(&p.version));
        assert_eq!(p.sha256.len(), 64);
        assert!(p.sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    /// Backdates a file's or folder's modified time.
    #[cfg(windows)]
    fn backdate(path: &Path, to: std::time::SystemTime) {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap()
            .set_modified(to)
            .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn stale_temporary_downloads_are_swept_but_fresh_ones_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let old_part = tmp.path().join(".ffmpeg-9.0.2-1.zip.part");
        let old_staging = tmp.path().join(".ffmpeg-9.0.2-1.staging");
        let fresh = tmp.path().join(".ffmpeg-9.0.2-2.zip.part");
        let other = tmp.path().join("notes.txt");
        fs::write(&old_part, b"x").unwrap();
        fs::create_dir(&old_staging).unwrap();
        fs::write(old_staging.join("ffmpeg.exe"), b"x").unwrap();
        fs::write(&fresh, b"x").unwrap();
        fs::write(&other, b"x").unwrap();
        let long_ago = std::time::SystemTime::now() - STALE_AFTER * 2;
        for path in [&old_part, &old_staging, &other] {
            backdate(path, long_ago);
        }

        sweep_stale(tmp.path());
        assert!(!old_part.exists() && !old_staging.exists());
        assert!(fresh.exists(), "a younger download may belong to another instance");
        assert!(other.exists(), "only our own temporary entries are touched");
    }

    #[cfg(any(debug_assertions, feature = "automation"))]
    #[test]
    fn the_override_needs_all_five_fields() {
        let p = parse_override("http://127.0.0.1:9/f.zip|12|ABCD|root|1.0").unwrap();
        assert_eq!((p.size, p.sha256.as_str(), p.version.as_str()), (12, "abcd", "1.0"));
        assert!(parse_override("http://127.0.0.1:9/f.zip|12|abcd").is_none());
        assert!(parse_override("u|not-a-number|abcd|root|1.0").is_none());
    }
}
