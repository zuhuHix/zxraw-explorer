//! Update check and one-click update.
//!
//! Once a day (Settings › Updates › "Check for updates automatically", on by default), a
//! few seconds after startup, the app asks the GitHub API for the latest release of
//! `zuhuHix/zxraw-explorer`. The request is a plain GET with the app version in the
//! User-Agent; nothing about the PC, its files or its use is sent. A newer stable release
//! shows a notice (see `notices.rs`) offering "Update now".
//!
//! The API allows 60 anonymous requests an hour per IP address, which a shared connection
//! (an office, a VPN) can use up. When it answers "rate limited", the check falls back to
//! the public `releases/latest` page: GitHub redirects it to the latest stable release's
//! tag page, and the tag gives the installer and checksum URLs. Release notes and sizes
//! are not known that way; the SHA-256 check below still applies.
//!
//! "Update now" downloads `SHA256SUMS.txt` and the release's
//! `ZXraw-Explorer-<version>-Setup-x64.exe` into a fresh temporary folder, only from the
//! project's GitHub release URLs (and GitHub's download CDN after redirects), and refuses
//! the installer unless its SHA-256 (and size, when the API gave one) match. It then starts the installer silently
//! with `/RELAUNCH` and closes the app; the installer waits for the app to exit (the
//! `AppMutex`), upgrades in place keeping the user's tasks, and starts the new version.
//! A portable copy (no `unins000.exe` next to the exe) opens the release page instead.
//!
//! Automated runs never touch the network: the check runs there only when
//! `ZXRAW_UPDATE_API` points it at a local test server (debug or `automation` builds),
//! and a verified installer is never started.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use forge_jobs::{JobHandle, JobState};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::actions::Event;
use crate::app::ZxrawApp;

/// The running version, as built.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Named mutex the app holds while running; `packaging/zxraw.iss` names it in `AppMutex`.
pub const APP_MUTEX: &str = "ZXrawExplorerRunning";

const API_URL: &str = "https://api.github.com/repos/zuhuHix/zxraw-explorer/releases/latest";
const RELEASES_URL: &str = "https://github.com/zuhuHix/zxraw-explorer/releases/";
const DOWNLOAD_URL: &str = "https://github.com/zuhuHix/zxraw-explorer/releases/download/";
/// Redirects to the latest stable release's tag page; not subject to the API's rate limit.
const LATEST_PAGE_URL: &str = "https://github.com/zuhuHix/zxraw-explorer/releases/latest";
const TAG_URL: &str = "https://github.com/zuhuHix/zxraw-explorer/releases/tag/";
/// Hosts GitHub redirects release downloads to.
const DOWNLOAD_CDN_HOSTS: &[&str] = &[
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
const SUMS_NAME: &str = "SHA256SUMS.txt";

/// The automatic check runs at most this often.
const CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;
/// Delay after startup so the check never competes with the first folder listing.
const STARTUP_DELAY: Duration = Duration::from_secs(4);
/// Limits for the small documents fetched before the installer.
const MAX_API_BYTES: u64 = 2 << 20;
const MAX_SUMS_BYTES: u64 = 64 << 10;
/// Largest installer accepted, whatever the API claims (0.1.0's is ~10 MB).
const MAX_INSTALLER_BYTES: u64 = 512 << 20;
/// Name prefix of the temporary download folders in `%TEMP%`.
const TEMP_PREFIX: &str = "zxraw-update-";

// ---------------------------------------------------------------------------
// Versions, release JSON, checksums, URLs (pure; unit-tested)
// ---------------------------------------------------------------------------

/// `major.minor.patch` of a stable version: `0.2.0` or `v0.2.0`. Pre-releases
/// (`0.3.0-beta.1`) and anything else are `None`; build metadata (`+…`) is ignored.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim();
    let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
    let text = text.split_once('+').map_or(text, |(core, _)| core);
    let mut parts = text.split('.');
    let mut next = || -> Option<u64> {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) || part.len() > 9 {
            return None;
        }
        part.parse().ok()
    };
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// True when release tag `tag` is a stable version newer than `current`.
pub fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(tag), Some(current)) => tag > current,
        _ => false,
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    /// Bytes, as the API reports them; 0 when unknown (the release-page fallback).
    pub size: u64,
}

pub fn parse_release(json: &[u8]) -> Result<Release, String> {
    serde_json::from_slice(json).map_err(|e| format!("GitHub sent an unexpected answer ({e})"))
}

/// A newer release the user can install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// Without the `v`, e.g. `0.2.1`.
    pub version: String,
    /// The release page (checked to be this project's; otherwise the releases list).
    pub page_url: String,
    pub notes: String,
    pub installer: Option<Asset>,
    pub sums: Option<Asset>,
}

/// The release a `releases/latest` redirect landed on: `url` is `<tag_prefix><tag>` for a
/// stable version tag. Its assets are the installer and checksum list under
/// `<download_prefix><tag>/`, with unknown sizes; it has no notes. `None` for anything
/// else (a sign-in page, an error page, a pre-release tag).
pub fn release_from_tag_url(url: &str, tag_prefix: &str, download_prefix: &str) -> Option<Release> {
    let tag = url.strip_prefix(tag_prefix)?;
    let (major, minor, patch) = parse_version(tag)?;
    if !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') {
        return None;
    }
    let asset = |name: String| Asset {
        browser_download_url: format!("{download_prefix}{tag}/{name}"),
        name,
        size: 0,
    };
    Some(Release {
        tag_name: tag.to_string(),
        html_url: url.to_string(),
        body: None,
        draft: false,
        prerelease: false,
        assets: vec![
            asset(installer_name(&format!("{major}.{minor}.{patch}"))),
            asset(SUMS_NAME.to_string()),
        ],
    })
}

pub fn installer_name(version: &str) -> String {
    format!("ZXraw-Explorer-{version}-Setup-x64.exe")
}

/// The update `release` offers over `current`, if any: drafts, pre-releases and tags that
/// are not newer stable versions offer nothing.
pub fn offered_update(release: &Release, current: &str, test_server: bool) -> Option<UpdateInfo> {
    if release.draft || release.prerelease || !is_newer(&release.tag_name, current) {
        return None;
    }
    let (major, minor, patch) = parse_version(&release.tag_name)?;
    let version = format!("{major}.{minor}.{patch}");
    let wanted = installer_name(&version);
    let find = |name: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == name && allowed_download_url(&a.browser_download_url, test_server))
            .cloned()
    };
    let page_url = if release.html_url.starts_with(RELEASES_URL)
        || (test_server && is_test_url(&release.html_url))
    {
        release.html_url.clone()
    } else {
        format!("{RELEASES_URL}latest")
    };
    Some(UpdateInfo {
        installer: find(&wanted),
        sums: find(SUMS_NAME),
        version,
        page_url,
        notes: release.body.clone().unwrap_or_default().trim().to_string(),
    })
}

/// The lowercase SHA-256 listed for `file` in a `sha256sum`-style list
/// (`<hex>  <name>` or `<hex> *<name>`). `None` when missing or listed twice differently.
pub fn checksum_for(sums: &str, file: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for line in sums.lines() {
        let line = line.trim_start_matches('\u{feff}').trim_end();
        let Some((hash, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        if name != file || hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let hash = hash.to_ascii_lowercase();
        match &found {
            Some(earlier) if *earlier != hash => return None,
            _ => found = Some(hash),
        }
    }
    found
}

/// Host of an `https://` URL without user info or a port; `None` otherwise.
fn https_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let host = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    (!host.is_empty() && !host.contains(['@', ':', '\\'])).then_some(host)
}

/// Plain HTTP to this PC: only the e2e tests' local server, and only when it was chosen
/// through the test override.
fn is_test_url(url: &str) -> bool {
    url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:")
}

/// Where an update may be downloaded from: this project's release downloads.
pub fn allowed_download_url(url: &str, test_server: bool) -> bool {
    (url.starts_with(DOWNLOAD_URL) && https_host(url) == Some("github.com") && !url.contains(".."))
        || (test_server && is_test_url(url))
}

/// Where a download may end up after redirects: the release URL itself or GitHub's
/// download CDN.
pub fn allowed_final_url(url: &str, test_server: bool) -> bool {
    allowed_download_url(url, test_server)
        || https_host(url).is_some_and(|host| DOWNLOAD_CDN_HOSTS.contains(&host))
}

/// Whether the automatic check is due: never checked, a day has passed, or the clock was
/// turned back since the last check.
pub fn check_due(last_check: Option<i64>, now: i64) -> bool {
    match last_check {
        None => true,
        Some(last) => now - last >= CHECK_INTERVAL_SECS || last > now + 60 * 60,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

// ---------------------------------------------------------------------------
// Configuration for this run
// ---------------------------------------------------------------------------

/// The API URL to ask, or `None` when this run must not check (automated runs without a
/// local test server). Debug and automation builds accept `ZXRAW_UPDATE_API` so tests can
/// point the check at a local listener; release builds always ask GitHub.
fn api_url(automated: bool) -> Option<String> {
    #[cfg(any(debug_assertions, feature = "automation"))]
    if let Ok(url) = std::env::var("ZXRAW_UPDATE_API") {
        return Some(url);
    }
    (!automated).then(|| API_URL.to_string())
}

/// True when the check talks to a local test server (plain-HTTP loopback URLs allowed).
fn test_server() -> bool {
    #[cfg(any(debug_assertions, feature = "automation"))]
    if std::env::var("ZXRAW_UPDATE_API").is_ok_and(|url| is_test_url(&url)) {
        return true;
    }
    false
}

/// True when this exe was installed by the setup program (an uninstaller sits next to
/// it); a portable copy from the zip gets the release page instead of the installer.
pub fn is_installed_copy() -> bool {
    #[cfg(any(debug_assertions, feature = "automation"))]
    if std::env::var_os("ZXRAW_UPDATE_INSTALLED").is_some_and(|v| v == "1") {
        return true;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("unins000.exe").is_file()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Outcome of the last check, for Settings › Updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    UpToDate,
    Available(String),
    Failed(String),
}

/// What a background check reports to the UI thread.
pub struct Checked {
    manual: bool,
    result: Result<Release, String>,
}

pub struct Updater {
    started: Instant,
    auto_considered: bool,
    /// A check is in flight (`true` = started by the user).
    checking: Option<bool>,
    pub last_result: Option<CheckResult>,
    /// The newer release found by the last check.
    pub available: Option<UpdateInfo>,
    download: Option<JobHandle>,
    /// A verified installer waiting for running operations to finish.
    pub ready: Option<PathBuf>,
    /// When the download finished: the installer starts by itself if every operation
    /// (including the download job winding down) has ended shortly after; otherwise the
    /// notice offers "Restart and update".
    install_after: Option<Instant>,
}

impl Default for Updater {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            auto_considered: false,
            checking: None,
            last_result: None,
            available: None,
            download: None,
            ready: None,
            install_after: None,
        }
    }
}

/// What the update notice and Settings show about a download.
#[derive(Debug, Clone, Copy, Default)]
pub struct DownloadStatus {
    pub running: bool,
    pub fraction: Option<f32>,
}

impl Updater {
    pub fn checking(&self) -> bool {
        self.checking.is_some()
    }

    pub fn download_status(&self) -> DownloadStatus {
        match &self.download {
            Some(job) if !job.state().is_finished() => DownloadStatus {
                running: true,
                fraction: (job.state() == JobState::Running)
                    .then(|| job.progress.snapshot().fraction())
                    .flatten(),
            },
            _ => DownloadStatus::default(),
        }
    }

    pub fn cancel_download(&self) {
        if let Some(job) = &self.download {
            job.cancel.cancel();
        }
    }
}

// ---------------------------------------------------------------------------
// Checking
// ---------------------------------------------------------------------------

/// Starts the automatic check once per session when it is enabled, due and the startup
/// delay has passed. Call every frame; cheap until then.
pub fn tick(app: &mut ZxrawApp) {
    if let Some(since) = app.ops.updater.install_after {
        if app.ops.jobs.running_count() == 0 {
            app.ops.updater.install_after = None;
            let ctx = app.ops.ctx.clone();
            install_when_idle(app, &ctx);
        } else if since.elapsed() > AUTO_INSTALL_GRACE {
            app.ops.updater.install_after = None;
        } else {
            app.ops
                .ctx
                .request_repaint_after(Duration::from_millis(100));
        }
    }
    let updater = &mut app.ops.updater;
    if updater.auto_considered {
        return;
    }
    let wait = STARTUP_DELAY.saturating_sub(updater.started.elapsed());
    if !wait.is_zero() {
        app.ops.ctx.request_repaint_after(wait);
        return;
    }
    updater.auto_considered = true;
    if !app.settings.check_updates || !check_due(app.settings.last_update_check, unix_now()) {
        return;
    }
    if api_url(app.testhook.active()).is_none() {
        return;
    }
    start_check(app, false);
}

/// "Check for updates" (Settings button and the command palette).
pub fn check_now(app: &mut ZxrawApp) {
    if app.ops.updater.checking() {
        return;
    }
    if api_url(app.testhook.active()).is_none() {
        app.toast("Update checks are off in automated runs");
        return;
    }
    start_check(app, true);
}

fn start_check(app: &mut ZxrawApp, manual: bool) {
    let Some(url) = api_url(app.testhook.active()) else {
        return;
    };
    app.ops.updater.checking = Some(manual);
    app.settings.last_update_check = Some(unix_now());
    app.save_all();
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            sweep_stale(&std::env::temp_dir());
            let result = fetch_release(&url, test_server());
            let _ = tx.send(Event::UpdateChecked(Checked { manual, result }));
            ctx.request_repaint();
        });
    if let Err(e) = spawned {
        app.ops.updater.checking = None;
        tracing::warn!("cannot start the update check: {e}");
    }
}

/// The latest release from the API, or from the release page when the API is rate limited.
fn fetch_release(url: &str, test: bool) -> Result<Release, String> {
    match fetch_release_api(url, test) {
        Err(Rejected::RateLimited) => {
            let (latest, tag_prefix, download_prefix) = release_page_urls(url, test);
            fetch_release_page(&latest, &tag_prefix, &download_prefix).map_err(|e| {
                tracing::info!("update check: the release page fallback failed: {e}");
                "GitHub is limiting requests right now; try again later".to_string()
            })
        }
        Err(Rejected::Other(e)) => Err(e),
        Ok(release) => Ok(release),
    }
}

enum Rejected {
    RateLimited,
    Other(String),
}

/// The release page, tag prefix and download prefix that go with the API URL: GitHub's,
/// or the same paths on the local test server.
fn release_page_urls(api: &str, test: bool) -> (String, String, String) {
    if test && is_test_url(api) {
        let rest = &api["http://".len()..];
        let origin = &api[.."http://".len() + rest.find('/').unwrap_or(rest.len())];
        return (
            format!("{origin}/releases/latest"),
            format!("{origin}/releases/tag/"),
            format!("{origin}/releases/download/"),
        );
    }
    (LATEST_PAGE_URL.into(), TAG_URL.into(), DOWNLOAD_URL.into())
}

/// Follows the `releases/latest` redirect and reads the tag from where it lands. Only the
/// final URL matters, so the transfer stops at the first chunk of the page.
fn fetch_release_page(
    latest: &str,
    tag_prefix: &str,
    download_prefix: &str,
) -> Result<Release, String> {
    let landed = std::cell::RefCell::new(None::<String>);
    let final_ok = |u: &str| {
        *landed.borrow_mut() = Some(u.to_string());
        u.starts_with(tag_prefix)
    };
    let options = forge_win::http::Options {
        headers: &[("Accept", "text/html")],
        final_url: Some(&final_ok),
    };
    match forge_win::http::download_with(latest, &options, |_| ControlFlow::Break(())) {
        Ok(_) | Err(forge_win::WinError::Aborted { .. }) => {}
        Err(e) => return Err(e.to_string()),
    }
    let landed = landed.into_inner().unwrap_or_default();
    release_from_tag_url(&landed, tag_prefix, download_prefix)
        .ok_or_else(|| format!("the release page led to an unexpected address: {landed}"))
}

fn fetch_release_api(url: &str, test: bool) -> Result<Release, Rejected> {
    let final_ok = |u: &str| u.starts_with("https://api.github.com/") || (test && is_test_url(u));
    let options = forge_win::http::Options {
        headers: &[
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
        ],
        final_url: Some(&final_ok),
    };
    let mut body = Vec::new();
    let mut too_large = false;
    let result = forge_win::http::download_with(url, &options, |chunk| {
        if body.len() as u64 + chunk.len() as u64 > MAX_API_BYTES {
            too_large = true;
            return ControlFlow::Break(());
        }
        body.extend_from_slice(chunk);
        ControlFlow::Continue(())
    });
    if too_large {
        return Err(Rejected::Other(
            "GitHub's answer was unexpectedly large".into(),
        ));
    }
    result.map_err(|e| match e.to_string() {
        m if m.contains("HTTP 403") || m.contains("HTTP 429") => Rejected::RateLimited,
        m if m.contains("HTTP 404") => {
            Rejected::Other("no release has been published yet".to_string())
        }
        m => Rejected::Other(m),
    })?;
    parse_release(&body).map_err(Rejected::Other)
}

/// A check finished (called from `actions::poll`).
pub fn checked(app: &mut ZxrawApp, checked: Checked) {
    let Checked { manual, result } = checked;
    app.ops.updater.checking = None;
    let release = match result {
        Ok(release) => release,
        Err(error) => {
            tracing::info!("update check failed: {error}");
            app.ops.updater.last_result = Some(CheckResult::Failed(error.clone()));
            if manual {
                app.toast_err(format!("Could not check for updates: {error}"));
            }
            return;
        }
    };
    match offered_update(&release, CURRENT, test_server()) {
        Some(update) => {
            tracing::info!("update available: {} (running {CURRENT})", update.version);
            app.ops.updater.last_result = Some(CheckResult::Available(update.version.clone()));
            let skipped = app.settings.skipped_update.as_deref() == Some(update.version.as_str());
            if manual || !skipped {
                app.notices.update = Some(update.clone());
            }
            if manual {
                app.toast(format!("ZXraw Explorer {} is available", update.version));
            }
            app.ops.updater.available = Some(update);
        }
        None => {
            tracing::info!(
                "no newer release than {CURRENT} (latest: {})",
                release.tag_name
            );
            app.ops.updater.last_result = Some(CheckResult::UpToDate);
            app.ops.updater.available = None;
            if manual {
                app.toast(format!("You're up to date (ZXraw Explorer {CURRENT})"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Notice actions
// ---------------------------------------------------------------------------

/// "What's new": the release page in the default browser.
pub fn open_release_page(app: &mut ZxrawApp, update: &UpdateInfo) {
    if app.testhook.active() {
        app.toast(format!("Would open {}", update.page_url));
        return;
    }
    let target = PathBuf::from(&update.page_url);
    app.ops.spawn_shell("Open the release page", move || {
        forge_win::open_default(&target)
    });
}

pub fn skip(app: &mut ZxrawApp, update: &UpdateInfo) {
    app.settings.skipped_update = Some(update.version.clone());
    app.notices.update = None;
    app.save_all();
    app.toast(format!(
        "ZXraw Explorer {} skipped; Settings › Updates can still install it",
        update.version
    ));
}

/// "Update now": download and verify the installer, or open the release page for a
/// portable copy or a release without an installer.
pub fn update_now(app: &mut ZxrawApp, update: &UpdateInfo) {
    if app.ops.updater.download_status().running {
        return;
    }
    if !is_installed_copy() {
        open_release_page(app, update);
        return;
    }
    let (Some(installer), Some(sums)) = (update.installer.clone(), update.sums.clone()) else {
        app.toast_err(format!(
            "ZXraw Explorer {} has no verifiable installer on GitHub; opening the release page",
            update.version
        ));
        open_release_page(app, update);
        return;
    };
    let test = test_server();
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    let title = format!("Downloading ZXraw Explorer {}", update.version);
    let job = app.ops.jobs.spawn("update", title, move |job| {
        let result = download_verified(&installer, &sums, &std::env::temp_dir(), test, job);
        let event = match &result {
            Err(_) if job.cancel.is_cancelled() => Event::Toast("Update download cancelled".into()),
            _ => Event::UpdateDownloaded(result.clone()),
        };
        let _ = tx.send(event);
        ctx.request_repaint();
        result.map(|_| ())
    });
    app.ops.updater.download = Some(job);
}

/// How long after the download the installer may still start by itself.
const AUTO_INSTALL_GRACE: Duration = Duration::from_secs(3);

/// The verified installer is ready (called from `actions::poll`).
pub fn downloaded(app: &mut ZxrawApp, result: Result<PathBuf, String>) {
    match result {
        Ok(installer) => {
            app.ops.updater.ready = Some(installer);
            app.ops.updater.install_after = Some(Instant::now());
            app.ops.ctx.request_repaint();
        }
        Err(error) => app.toast_err(format!("Could not update ZXraw Explorer: {error}")),
    }
}

/// Starts the waiting installer unless file operations are still running (closing the app
/// would cancel them); the notice then offers "Restart and update" once they finish.
pub fn install_when_idle(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(installer) = app.ops.updater.ready.clone() else {
        return;
    };
    if app.ops.jobs.running_count() > 0 {
        return;
    }
    if app.testhook.active() {
        app.toast(format!(
            "Update verified; would run {}",
            installer.display()
        ));
        if let Some(dir) = installer.parent() {
            let _ = fs::remove_dir_all(dir);
        }
        app.ops.updater.ready = None;
        app.notices.update = None;
        return;
    }
    let log = installer.with_file_name("install.log");
    let args: Vec<std::ffi::OsString> = [
        "/SILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/CLOSEAPPLICATIONS",
        "/NORESTARTAPPLICATIONS",
        "/RELAUNCH",
    ]
    .into_iter()
    .map(Into::into)
    .chain(std::iter::once(format!("/LOG={}", log.display()).into()))
    .collect();
    match forge_win::launch(&installer, &args, installer.parent()) {
        Ok(()) => {
            tracing::info!("started the installer {}; closing", installer.display());
            app.ops.updater.ready = None;
            app.save_all();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        Err(e) => {
            app.ops.updater.ready = None;
            app.toast_err(format!("Could not start the installer: {e}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Download and verification
// ---------------------------------------------------------------------------

/// Downloads `sums` and `installer` into a new folder under `temp`, and returns the
/// installer's path once its size and SHA-256 match the release's checksum list. On any
/// failure the folder is removed.
pub fn download_verified(
    installer: &Asset,
    sums: &Asset,
    temp: &Path,
    test_server: bool,
    job: &JobHandle,
) -> Result<PathBuf, String> {
    for asset in [installer, sums] {
        if !allowed_download_url(&asset.browser_download_url, test_server) {
            return Err(format!(
                "{} is not hosted on the project's GitHub releases; not downloading it",
                asset.name
            ));
        }
    }
    if installer.name.contains(['/', '\\', ':']) || !installer.name.ends_with(".exe") {
        return Err(format!("unexpected installer name {:?}", installer.name));
    }
    if installer.size > MAX_INSTALLER_BYTES {
        return Err(format!(
            "unexpected installer size ({} bytes)",
            installer.size
        ));
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = temp.join(format!("{TEMP_PREFIX}{}-{nanos}", std::process::id()));
    fs::create_dir(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut cleanup = RemoveOnDrop(Some(dir.clone()));

    job.progress.set_current("Downloading the checksum list");
    let sums_text = fetch_small(&sums.browser_download_url, MAX_SUMS_BYTES, test_server, job)?;
    let expected =
        checksum_for(&String::from_utf8_lossy(&sums_text), &installer.name).ok_or_else(|| {
            format!(
                "{SUMS_NAME} has no checksum for {}; nothing was installed",
                installer.name
            )
        })?;

    if installer.size > 0 {
        job.progress.add_total_bytes(installer.size);
    }
    job.progress
        .set_current(format!("Downloading {}", installer.name));
    let target = dir.join(&installer.name);
    let part = dir.join(format!("{}.part", installer.name));
    let digest = fetch_to_file(installer, &part, test_server, job)?;
    if digest != expected {
        return Err(format!(
            "the downloaded installer does not match its SHA-256 checksum, so it was not run \
             (expected {expected}, got {digest})"
        ));
    }
    fs::rename(&part, &target).map_err(|e| format!("cannot save the installer: {e}"))?;
    job.log(format!(
        "downloaded {}, SHA-256 {digest} verified",
        installer.name
    ));
    cleanup.0 = None;
    Ok(target)
}

fn fetch_options<'a>(final_ok: &'a dyn Fn(&str) -> bool) -> forge_win::http::Options<'a> {
    forge_win::http::Options {
        headers: &[],
        final_url: Some(final_ok),
    }
}

fn fetch_small(url: &str, limit: u64, test: bool, job: &JobHandle) -> Result<Vec<u8>, String> {
    let final_ok = |u: &str| allowed_final_url(u, test);
    let mut body = Vec::new();
    let mut stop = None;
    let result = forge_win::http::download_with(url, &fetch_options(&final_ok), |chunk| {
        if job.cancel.is_cancelled() {
            stop = Some("cancelled".to_string());
            return ControlFlow::Break(());
        }
        if body.len() as u64 + chunk.len() as u64 > limit {
            stop = Some(format!("{SUMS_NAME} is unexpectedly large"));
            return ControlFlow::Break(());
        }
        body.extend_from_slice(chunk);
        ControlFlow::Continue(())
    });
    match (stop, result) {
        (Some(why), _) => Err(why),
        (None, Err(e)) => Err(e.to_string()),
        (None, Ok(_)) => Ok(body),
    }
}

/// Streams `asset` into `part`; returns its SHA-256 once exactly `asset.size` bytes came
/// (any amount up to [`MAX_INSTALLER_BYTES`] when the size is unknown).
fn fetch_to_file(
    asset: &Asset,
    part: &Path,
    test: bool,
    job: &JobHandle,
) -> Result<String, String> {
    let file = File::create(part).map_err(|e| format!("cannot create {}: {e}", part.display()))?;
    let mut out = BufWriter::with_capacity(1 << 20, file);
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut stop = None;
    let limit = if asset.size == 0 {
        MAX_INSTALLER_BYTES
    } else {
        asset.size
    };
    let final_ok = |u: &str| allowed_final_url(u, test);
    let result = forge_win::http::download_with(
        &asset.browser_download_url,
        &fetch_options(&final_ok),
        |chunk| {
            if job.cancel.is_cancelled() {
                stop = Some("cancelled".to_string());
                return ControlFlow::Break(());
            }
            received += chunk.len() as u64;
            if received > limit {
                stop = Some(format!(
                    "the server sent more than the expected {limit} bytes; nothing was installed"
                ));
                return ControlFlow::Break(());
            }
            if let Err(e) = out.write_all(chunk) {
                stop = Some(format!("cannot save the download: {e}"));
                return ControlFlow::Break(());
            }
            hasher.update(chunk);
            job.progress.add_bytes(chunk.len() as u64);
            ControlFlow::Continue(())
        },
    );
    match (stop, result) {
        (Some(why), _) => return Err(why),
        (None, Err(e)) => return Err(e.to_string()),
        (None, Ok(_)) => {}
    }
    if asset.size > 0 && received != asset.size {
        return Err(format!(
            "the download stopped early ({received} of {} bytes); try again",
            asset.size
        ));
    }
    let file = out
        .into_inner()
        .map_err(|e| format!("cannot save the download: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("cannot save the download: {e}"))?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

struct RemoveOnDrop(Option<PathBuf>);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(dir) = &self.0 {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

/// Removes our own update folders in `temp` older than a day (an installer that already
/// ran, or an app that exited mid-download).
fn sweep_stale(temp: &Path) {
    let Ok(entries) = fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(TEMP_PREFIX))
            && entry.file_type().is_ok_and(|t| t.is_dir())
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(24 * 60 * 60));
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    const FIXTURE: &str = r##"{
        "url": "https://api.github.com/repos/zuhuHix/zxraw-explorer/releases/1",
        "html_url": "https://github.com/zuhuHix/zxraw-explorer/releases/tag/v0.2.1",
        "tag_name": "v0.2.1",
        "name": "ZXraw Explorer 0.2.1",
        "draft": false,
        "prerelease": false,
        "body": "What is new\r\n- Faster thumbnails\r\n",
        "assets": [
            {"name": "SHA256SUMS.txt", "size": 200, "content_type": "text/plain",
             "browser_download_url": "https://github.com/zuhuHix/zxraw-explorer/releases/download/v0.2.1/SHA256SUMS.txt"},
            {"name": "ZXraw-Explorer-0.2.1-Setup-x64.exe", "size": 10485760,
             "browser_download_url": "https://github.com/zuhuHix/zxraw-explorer/releases/download/v0.2.1/ZXraw-Explorer-0.2.1-Setup-x64.exe"},
            {"name": "ZXraw-Explorer-0.2.1-x64.zip", "size": 9000000,
             "browser_download_url": "https://github.com/zuhuHix/zxraw-explorer/releases/download/v0.2.1/ZXraw-Explorer-0.2.1-x64.zip"}
        ]
    }"##;

    #[test]
    fn versions_compare_numerically_and_ignore_prereleases() {
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("0.10.3+build.7"), Some((0, 10, 3)));
        for bad in [
            "0.2",
            "0.2.0.1",
            "v0.3.0-beta.1",
            "x1.2.3",
            "1..2",
            "",
            "v",
            "1.-2.3",
        ] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
        assert!(is_newer("v0.2.1", "0.2.0"));
        assert!(is_newer("v0.10.0", "0.9.9"), "numeric, not text, order");
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("v0.1.9", "0.2.0"));
        assert!(
            !is_newer("v0.3.0-rc.1", "0.2.0"),
            "pre-releases are never offered"
        );
        assert!(!is_newer("nightly", "0.2.0"));
    }

    #[test]
    fn parses_a_github_release_and_selects_its_assets() {
        let release = parse_release(FIXTURE.as_bytes()).unwrap();
        assert_eq!(release.tag_name, "v0.2.1");
        assert_eq!(release.assets.len(), 3);
        let update = offered_update(&release, "0.2.0", false).unwrap();
        assert_eq!(update.version, "0.2.1");
        assert_eq!(
            update.page_url,
            "https://github.com/zuhuHix/zxraw-explorer/releases/tag/v0.2.1"
        );
        assert!(update.notes.starts_with("What is new"));
        let installer = update.installer.unwrap();
        assert_eq!(installer.name, "ZXraw-Explorer-0.2.1-Setup-x64.exe");
        assert_eq!(installer.size, 10_485_760);
        assert_eq!(update.sums.unwrap().name, "SHA256SUMS.txt");

        assert!(
            offered_update(&release, "0.2.1", false).is_none(),
            "same version"
        );
        let mut pre = release.clone();
        pre.prerelease = true;
        assert!(offered_update(&pre, "0.2.0", false).is_none());
        let mut draft = release.clone();
        draft.draft = true;
        assert!(offered_update(&draft, "0.2.0", false).is_none());
        assert!(parse_release(b"{\"message\": \"Not Found\"}").is_err());
        assert!(parse_release(b"<html>").is_err());
    }

    #[test]
    fn assets_from_elsewhere_are_not_offered() {
        let mut release = parse_release(FIXTURE.as_bytes()).unwrap();
        release.assets[1].browser_download_url =
            "https://evil.example/ZXraw-Explorer-0.2.1-Setup-x64.exe".into();
        release.html_url = "https://evil.example/release".into();
        let update = offered_update(&release, "0.2.0", false).unwrap();
        assert!(update.installer.is_none());
        assert!(update.sums.is_some());
        assert_eq!(
            update.page_url,
            "https://github.com/zuhuHix/zxraw-explorer/releases/latest"
        );
    }

    #[test]
    fn download_urls_are_limited_to_the_projects_releases() {
        let ok = "https://github.com/zuhuHix/zxraw-explorer/releases/download/v0.2.1/x.exe";
        assert!(allowed_download_url(ok, false));
        for bad in [
            "http://github.com/zuhuHix/zxraw-explorer/releases/download/v0.2.1/x.exe",
            "https://github.com/someone/zxraw-explorer/releases/download/v0.2.1/x.exe",
            "https://github.com.evil.example/zuhuHix/zxraw-explorer/releases/download/x.exe",
            "https://github.com/zuhuHix/zxraw-explorer/releases/download/../../other/x.exe",
            "https://objects.githubusercontent.com/x.exe",
            "http://127.0.0.1:8080/x.exe",
        ] {
            assert!(!allowed_download_url(bad, false), "{bad}");
        }
        assert!(allowed_download_url("http://127.0.0.1:8080/x.exe", true));

        // Redirect targets: the release URL itself or GitHub's download CDN only.
        assert!(allowed_final_url(ok, false));
        assert!(allowed_final_url(
            "https://objects.githubusercontent.com/github-production-release-asset/1?x=y",
            false
        ));
        assert!(allowed_final_url(
            "https://release-assets.githubusercontent.com/github-production-release-asset/1",
            false
        ));
        for bad in [
            "http://objects.githubusercontent.com/x",
            "https://objects.githubusercontent.com.evil.example/x",
            "https://user@objects.githubusercontent.com/x",
            "https://objects.githubusercontent.com:8443/x",
            "https://raw.githubusercontent.com/x",
        ] {
            assert!(!allowed_final_url(bad, false), "{bad}");
        }
    }

    #[test]
    fn reads_checksums_in_sha256sum_format() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!(
            "\u{feff}{a}  ZXraw-Explorer-0.2.1-Setup-x64.exe\r\n{b} *ZXraw-Explorer-0.2.1-x64.zip\n\nnot a line\n"
        );
        assert_eq!(
            checksum_for(&sums, "ZXraw-Explorer-0.2.1-Setup-x64.exe"),
            Some(a.clone())
        );
        assert_eq!(
            checksum_for(&sums, "ZXraw-Explorer-0.2.1-x64.zip"),
            Some("b".repeat(64))
        );
        assert_eq!(checksum_for(&sums, "other.exe"), None);
        assert_eq!(
            checksum_for(&format!("{}  x.exe", "a".repeat(63)), "x.exe"),
            None
        );
        let conflicting = format!("{a}  x.exe\n{}  x.exe\n", "c".repeat(64));
        assert_eq!(
            checksum_for(&conflicting, "x.exe"),
            None,
            "ambiguous lists are refused"
        );
        assert_eq!(
            checksum_for(&format!("{a}  x.exe\n{a}  x.exe"), "x.exe"),
            Some(a)
        );
    }

    #[test]
    fn the_automatic_check_runs_at_most_daily() {
        let now = 1_800_000_000;
        assert!(check_due(None, now));
        assert!(!check_due(Some(now - 60), now));
        assert!(!check_due(Some(now - CHECK_INTERVAL_SECS + 1), now));
        assert!(check_due(Some(now - CHECK_INTERVAL_SECS), now));
        assert!(!check_due(Some(now + 60), now), "small clock skew");
        assert!(check_due(Some(now + 2 * 60 * 60), now), "clock turned back");
    }

    /// Serves each path's body (a body starting with `HTTP/1.1 ` is sent as the whole
    /// response); unknown paths get 404.
    fn serve(routes: Vec<(&'static str, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = [0u8; 4096];
                let n = stream.read(&mut request).unwrap_or(0);
                let line = String::from_utf8_lossy(&request[..n]).into_owned();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let body = routes
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, b)| b.clone());
                let response = match body {
                    Some(body) if body.starts_with(b"HTTP/1.1 ") => body,
                    Some(body) => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(&body);
                        r
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&response);
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn job() -> JobHandle {
        forge_jobs::JobRegistry::new().spawn("test", "test", |_| Ok(()))
    }

    fn assets(base: &str, installer: &[u8]) -> (Asset, Asset) {
        (
            Asset {
                name: "ZXraw-Explorer-9.0.0-Setup-x64.exe".into(),
                browser_download_url: format!("{base}/setup.exe"),
                size: installer.len() as u64,
            },
            Asset {
                name: SUMS_NAME.into(),
                browser_download_url: format!("{base}/sums"),
                size: 0,
            },
        )
    }

    fn sums_for(body: &[u8]) -> Vec<u8> {
        let hash: String = Sha256::digest(body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("{hash}  ZXraw-Explorer-9.0.0-Setup-x64.exe\n").into_bytes()
    }

    fn leftovers(dir: &Path) -> usize {
        fs::read_dir(dir).unwrap().count()
    }

    #[test]
    fn a_verified_installer_is_kept_and_a_tampered_one_refused() {
        let good = b"MZ pretend installer".to_vec();
        let mut tampered = good.clone();
        tampered[3] ^= 1;
        let base = serve(vec![
            ("/setup.exe", good.clone()),
            ("/sums", sums_for(&good)),
            ("/bad/setup.exe", tampered),
            ("/bad/sums", sums_for(&good)),
            ("/nosum/setup.exe", good.clone()),
            ("/nosum/sums", b"0000  other.exe\n".to_vec()),
        ]);
        let tmp = tempfile::tempdir().unwrap();

        let (installer, sums) = assets(&base, &good);
        let path = download_verified(&installer, &sums, tmp.path(), true, &job()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), good);
        assert_eq!(
            path.file_name().unwrap(),
            "ZXraw-Explorer-9.0.0-Setup-x64.exe"
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();

        let (installer, sums) = assets(&format!("{base}/bad"), &good);
        let err = download_verified(&installer, &sums, tmp.path(), true, &job()).unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");

        let (installer, sums) = assets(&format!("{base}/nosum"), &good);
        let err = download_verified(&installer, &sums, tmp.path(), true, &job()).unwrap_err();
        assert!(err.contains("no checksum"), "{err}");

        let (mut installer, sums) = assets(&base, &good);
        installer.size -= 1;
        let err = download_verified(&installer, &sums, tmp.path(), true, &job()).unwrap_err();
        assert!(err.contains("more than the expected"), "{err}");

        assert_eq!(
            leftovers(tmp.path()),
            0,
            "failed downloads leave nothing behind"
        );
    }

    #[test]
    fn the_release_page_redirect_gives_the_tag_and_asset_urls() {
        let tag = "https://github.com/zuhuHix/zxraw-explorer/releases/tag/";
        let release = release_from_tag_url(&format!("{tag}v0.5.2"), TAG_URL, DOWNLOAD_URL).unwrap();
        assert_eq!(release.tag_name, "v0.5.2");
        assert_eq!(release.html_url, format!("{tag}v0.5.2"));
        let update = offered_update(&release, "0.5.1", false).unwrap();
        assert_eq!(update.version, "0.5.2");
        assert_eq!(update.page_url, format!("{tag}v0.5.2"));
        assert!(update.notes.is_empty());
        let installer = update.installer.unwrap();
        assert_eq!(
            installer.browser_download_url,
            "https://github.com/zuhuHix/zxraw-explorer/releases/download/v0.5.2/\
             ZXraw-Explorer-0.5.2-Setup-x64.exe"
        );
        assert_eq!(installer.size, 0, "unknown");
        assert_eq!(
            update.sums.unwrap().browser_download_url,
            "https://github.com/zuhuHix/zxraw-explorer/releases/download/v0.5.2/SHA256SUMS.txt"
        );
        assert!(offered_update(&release, "0.5.2", false).is_none());

        for landed in [
            "https://github.com/login?return_to=x",
            "https://github.com/zuhuHix/zxraw-explorer/releases",
            "https://github.com/zuhuHix/zxraw-explorer/releases/tag/v0.6.0-beta.1",
            "https://github.com/zuhuHix/zxraw-explorer/releases/tag/v0.6.0/../x",
            "https://github.com/zuhuHix/zxraw-explorer/releases/tag/nightly",
            "https://github.com/someone/zxraw-explorer/releases/tag/v9.0.0",
            "",
        ] {
            assert!(
                release_from_tag_url(landed, TAG_URL, DOWNLOAD_URL).is_none(),
                "{landed}"
            );
        }
    }

    #[test]
    fn the_release_page_urls_follow_the_test_server() {
        assert_eq!(
            release_page_urls("http://127.0.0.1:4567/api", true),
            (
                "http://127.0.0.1:4567/releases/latest".to_string(),
                "http://127.0.0.1:4567/releases/tag/".to_string(),
                "http://127.0.0.1:4567/releases/download/".to_string(),
            )
        );
        let github = (LATEST_PAGE_URL.into(), TAG_URL.into(), DOWNLOAD_URL.into());
        assert_eq!(
            release_page_urls("http://127.0.0.1:4567/api", false),
            github
        );
        assert_eq!(release_page_urls(API_URL, true), github);
    }

    #[test]
    fn a_rate_limited_check_falls_back_to_the_release_page() {
        let good = b"MZ pretend installer".to_vec();
        let base = serve(vec![
            (
                "/api",
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
            (
                "/releases/latest",
                b"HTTP/1.1 302 Found\r\nLocation: /releases/tag/v9.0.0\r\n\
                  Content-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
            (
                "/releases/tag/v9.0.0",
                b"<html>release page</html>".to_vec(),
            ),
            (
                "/releases/download/v9.0.0/ZXraw-Explorer-9.0.0-Setup-x64.exe",
                good.clone(),
            ),
            ("/releases/download/v9.0.0/SHA256SUMS.txt", sums_for(&good)),
        ]);
        let (latest, tag_prefix, download_prefix) = release_page_urls(&format!("{base}/api"), true);
        let release = fetch_release_page(&latest, &tag_prefix, &download_prefix).unwrap();
        assert_eq!(release.tag_name, "v9.0.0");
        let update = offered_update(&release, "0.5.2", true).unwrap();

        // The size is unknown, so only the checksum decides.
        let tmp = tempfile::tempdir().unwrap();
        let (installer, sums) = (update.installer.unwrap(), update.sums.unwrap());
        let path = download_verified(&installer, &sums, tmp.path(), true, &job()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), good);

        let err =
            fetch_release_page(&format!("{base}/api"), &tag_prefix, &download_prefix).unwrap_err();
        assert!(
            err.contains("unexpected address"),
            "no redirect to a tag: {err}"
        );
        let err = fetch_release_page(&format!("{base}/nowhere"), &tag_prefix, &download_prefix)
            .unwrap_err();
        assert!(err.contains("unexpected address"), "{err}");
    }

    #[test]
    fn only_a_rate_limited_api_uses_the_release_page() {
        let answer = |status: &str| {
            format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .into_bytes()
        };
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: /releases/tag/v9.0.0\r\n\
              Content-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec();
        for status in ["403 Forbidden", "429 Too Many Requests"] {
            let base = serve(vec![
                ("/api", answer(status)),
                ("/releases/latest", redirect.clone()),
                ("/releases/tag/v9.0.0", b"<html></html>".to_vec()),
            ]);
            let release = fetch_release(&format!("{base}/api"), true).unwrap();
            assert_eq!(release.tag_name, "v9.0.0", "{status}");
            assert!(release.body.is_none(), "{status}: came from the page");
        }

        // A failed fallback reports the rate limit.
        let base = serve(vec![("/api", answer("403 Forbidden"))]);
        let err = fetch_release(&format!("{base}/api"), true).unwrap_err();
        assert!(err.contains("limiting requests"), "{err}");

        // Other errors don't fall back, even with a release page available.
        for (status, expected) in [
            ("404 Not Found", "no release has been published yet"),
            ("500 Internal Server Error", "HTTP 500"),
        ] {
            let base = serve(vec![
                ("/api", answer(status)),
                ("/releases/latest", redirect.clone()),
                ("/releases/tag/v9.0.0", b"<html></html>".to_vec()),
            ]);
            let err = fetch_release(&format!("{base}/api"), true).unwrap_err();
            assert!(err.contains(expected), "{status}: {err}");
        }

        // The API's own answer wins when it works.
        let base = serve(vec![("/api", FIXTURE.as_bytes().to_vec())]);
        let release = fetch_release(&format!("{base}/api"), true).unwrap();
        assert_eq!(release.tag_name, "v0.2.1");
        assert!(release.body.is_some());
    }

    #[test]
    fn an_installer_of_unknown_size_still_needs_its_checksum() {
        let good = b"MZ pretend installer".to_vec();
        let mut tampered = good.clone();
        tampered[3] ^= 1;
        let base = serve(vec![("/setup.exe", tampered), ("/sums", sums_for(&good))]);
        let (mut installer, sums) = assets(&base, &good);
        installer.size = 0;
        let tmp = tempfile::tempdir().unwrap();
        let err = download_verified(&installer, &sums, tmp.path(), true, &job()).unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert_eq!(leftovers(tmp.path()), 0);
    }

    #[test]
    fn loopback_downloads_need_the_test_server_override() {
        let base = serve(vec![]);
        let (installer, sums) = assets(&base, b"x");
        let tmp = tempfile::tempdir().unwrap();
        let err = download_verified(&installer, &sums, tmp.path(), false, &job()).unwrap_err();
        assert!(
            err.contains("not hosted on the project's GitHub releases"),
            "{err}"
        );
        assert_eq!(leftovers(tmp.path()), 0);
    }
}
