//! Installing a verified update on Linux. The download and the SHA-256 check are shared with
//! Windows (`updater::download_verified`); this module decides how the running copy was
//! installed and replaces it.
//!
//! * **AppImage**: this process runs from an AppImage mount (`/proc/self/exe` is inside
//!   `$APPDIR`), and `$APPIMAGE` is an absolute path to a regular file (not a symlink) of
//!   ours that starts with the ELF magic and the type 2 AppImage marker. The verified file is
//!   copied to a temporary name in the AppImage's own folder, hashed again, given the old
//!   file's mode (at least 0755), synced, and renamed over the old path. The rename is
//!   atomic, so the path always names either the whole old or the whole new AppImage; the
//!   running copy keeps its own (now unlinked) file through its mount. No `.old` backup is
//!   kept: the new file was verified before the rename and nothing is ever half written, so
//!   a backup would only be a 60 MB file to clean up. A folder ZXraw may not write to, or an
//!   AppImage owned by another user or group, gets the file in Downloads instead.
//! * **deb** (`/usr/bin/zxraw`, owned by the package `zxraw` according to `dpkg-query`): the
//!   file is checked unprivileged first (our private 0700 folder, one link, the checksum, and
//!   `dpkg-deb -f`: Package zxraw, the offered Version, Architecture amd64), so a wrong file
//!   never asks for a password. Then `pkexec /usr/lib/zxraw/install-update <file> <sha256>
//!   <version>` runs the helper the .deb ships (`packaging/linux/zxraw-install-update`): as
//!   root it copies the file once into a root-only folder and checks and installs only that
//!   copy, so a program running as the user cannot swap the file after the check. Without
//!   pkexec, a polkit agent or the helper (a package from before the helper was added), the file is opened
//!   in the desktop's package installer instead.
//! * **Other** (a build folder, a copied binary): no install; the release page is opened.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Where the .deb puts the app.
pub const DEB_EXE: &str = "/usr/bin/zxraw";
/// The .deb's package name (`packaging/linux/build-linux.sh`).
pub const DEB_PACKAGE: &str = "zxraw";
pub const DPKG_QUERY: &str = "/usr/bin/dpkg-query";
/// pkexec needs an absolute program path; so does the program it runs.
pub const PKEXEC: &str = "/usr/bin/pkexec";
/// The root helper the .deb installs (`packaging/linux/zxraw-install-update`).
pub const INSTALL_HELPER: &str = "/usr/lib/zxraw/install-update";
pub const DPKG_DEB: &str = "/usr/bin/dpkg-deb";
const DPKG_TIMEOUT: Duration = Duration::from_secs(5);
/// apt's output kept for an error message (the end of it).
const MAX_OUTPUT: usize = 256 << 10;

/// How this copy of ZXraw Explorer was installed, which decides how it is updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// Started from this AppImage file (`$APPIMAGE`: a regular file of ours, not a symlink).
    AppImage(PathBuf),
    /// The `zxraw` package's `/usr/bin/zxraw`.
    Deb,
    /// Anything else: only the release page.
    Other,
}

/// The install kind of this process. Runs `dpkg-query` (with a timeout) for a copy in
/// `/usr/bin`, so call it off the UI thread. Only amd64 packages are published.
pub fn detect() -> InstallKind {
    if !cfg!(target_arch = "x86_64") {
        return InstallKind::Other;
    }
    detect_from(
        std::env::var_os("APPIMAGE"),
        std::env::var_os("APPDIR"),
        std::env::current_exe().ok(),
        Path::new(DEB_EXE),
        |path| dpkg_owner(Path::new(DPKG_QUERY), path, DPKG_TIMEOUT),
    )
}

/// [`detect`] with its inputs given: `$APPIMAGE`, `$APPDIR`, the current exe (resolved, as
/// `/proc/self/exe` is), the path the .deb installs, and the package that owns a path.
///
/// A set `$APPIMAGE` that fails [`is_our_appimage`] is [`InstallKind::Other`]: it may name
/// any file, and replacing that file is not ours to do.
pub fn detect_from(
    appimage: Option<OsString>,
    appdir: Option<OsString>,
    exe: Option<PathBuf>,
    deb_exe: &Path,
    owner: impl FnOnce(&Path) -> Option<String>,
) -> InstallKind {
    if let Some(value) = appimage.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        return if is_our_appimage(&path, appdir.as_deref(), exe.as_deref()) {
            InstallKind::AppImage(path)
        } else {
            InstallKind::Other
        };
    }
    let Some(exe) = exe else {
        return InstallKind::Other;
    };
    let resolved = fs::canonicalize(&exe).unwrap_or(exe);
    let deb_resolved = fs::canonicalize(deb_exe).unwrap_or_else(|_| deb_exe.to_path_buf());
    if resolved != deb_resolved {
        return InstallKind::Other;
    }
    match owner(deb_exe) {
        Some(package) if package == DEB_PACKAGE => InstallKind::Deb,
        _ => InstallKind::Other,
    }
}

/// The first bytes of a type 2 AppImage: an ELF file with `AI\x02` at offset 8.
const ELF_MAGIC: &[u8] = b"\x7fELF";
const APPIMAGE_TYPE2: &[u8] = b"AI\x02";

/// `path` is the AppImage this process runs from: we run inside its mount (`exe` is under
/// `appdir`), and it is an absolute path to a regular file (not a symlink) owned by us, with
/// the ELF magic and the type 2 AppImage marker.
pub fn is_our_appimage(path: &Path, appdir: Option<&OsStr>, exe: Option<&Path>) -> bool {
    let (Some(appdir), Some(exe)) = (appdir.filter(|d| !d.is_empty()), exe) else {
        return false;
    };
    let (Ok(appdir), Ok(exe)) = (fs::canonicalize(appdir), fs::canonicalize(exe)) else {
        return false;
    };
    if !path.is_absolute() || !exe.starts_with(&appdir) {
        return false;
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.uid() != euid() {
        return false;
    }
    let mut head = [0u8; 11];
    open_nofollow(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok_and(|()| head.starts_with(ELF_MAGIC) && &head[8..11] == APPIMAGE_TYPE2)
}

/// The one package `dpkg-query -S <path>` names, run without a shell and stopped after
/// `timeout`.
pub fn dpkg_owner(program: &Path, path: &Path, timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .arg("-S")
        .arg(path)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.take(64 << 10).read_to_end(&mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::info!("{} did not answer in time", program.display());
                return None;
            }
        }
    }
    let text = reader.join().ok()?;
    parse_dpkg_owner(&String::from_utf8_lossy(&text), path)
}

/// The package in `dpkg-query -S` output (`zxraw: /usr/bin/zxraw`, or `zxraw:amd64: …`),
/// when exactly one package owns `path`. Diversion lines are skipped.
pub fn parse_dpkg_owner(text: &str, path: &Path) -> Option<String> {
    let suffix = format!(": {}", path.display());
    let mut packages: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.starts_with("diversion by ") {
            continue;
        }
        let Some(names) = line.trim_end().strip_suffix(suffix.as_str()) else {
            continue;
        };
        for name in names.split(", ") {
            let name = name.split(':').next().unwrap_or("").trim();
            if name.is_empty() {
                return None;
            }
            if !packages.iter().any(|p| p == name) {
                packages.push(name.to_string());
            }
        }
    }
    match packages.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn egid() -> u32 {
    rustix::process::getegid().as_raw()
}

/// Creates `dir` readable only by us (0700). Fails if it exists.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(dir)
}

/// `dir` is a real folder (not a symlink), ours, and closed to everyone else.
pub fn check_private_dir(dir: &Path) -> Result<(), String> {
    let meta =
        fs::symlink_metadata(dir).map_err(|e| format!("cannot check {}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != euid() || meta.mode() & 0o077 != 0 {
        return Err(format!("{} is not a private folder", dir.display()));
    }
    Ok(())
}

fn open_nofollow(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Copies `from` into `to` and returns the SHA-256 of what was copied.
fn copy_hashed(from: &mut File, to: &mut File) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hash_file(file: &mut File) -> io::Result<String> {
    copy_hashed(file, &mut File::options().write(true).open("/dev/null")?)
}

/// Is this error "you may not write here" (rather than a disk or I/O failure)?
fn is_not_writable(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(code) if code == rustix::io::Errno::ACCESS.raw_os_error()
        || code == rustix::io::Errno::PERM.raw_os_error()
        || code == rustix::io::Errno::ROFS.raw_os_error())
}

/// Removes a temporary file unless disarmed.
struct RemoveFile(Option<PathBuf>);

impl Drop for RemoveFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

// ---------------------------------------------------------------------------
// AppImage
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum ReplaceError {
    /// The AppImage's folder (or the file) may not be changed by us: save it elsewhere.
    NotWritable(String),
    Failed(String),
}

/// Replaces the AppImage at `appimage` (a regular file, never a symlink) with the verified
/// file `verified`, whose SHA-256 must still be `expected`. Returns the path that was
/// replaced. An AppImage owned by another user or group is left alone
/// ([`ReplaceError::NotWritable`]): the new file would be ours. On any failure the old
/// AppImage is untouched and no temporary file is left behind.
pub fn replace_appimage(
    verified: &Path,
    expected: &str,
    appimage: &Path,
) -> Result<PathBuf, ReplaceError> {
    let target = appimage.to_path_buf();
    if !target.is_absolute() {
        return Err(ReplaceError::Failed(format!(
            "unexpected AppImage path {}",
            target.display()
        )));
    }
    let old = fs::symlink_metadata(&target)
        .map_err(|e| ReplaceError::Failed(format!("cannot read {}: {e}", target.display())))?;
    if !old.is_file() {
        return Err(ReplaceError::Failed(format!(
            "{} is not a regular file",
            target.display()
        )));
    }
    if old.uid() != euid() || old.gid() != egid() {
        return Err(ReplaceError::NotWritable(format!(
            "{} belongs to another user or group",
            target.display()
        )));
    }
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(ReplaceError::Failed(format!(
            "unexpected AppImage path {}",
            target.display()
        )));
    };
    let mut temp_name = OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".zxraw-update-{}", unique_suffix()));
    let temp = dir.join(temp_name);

    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| {
            let why = format!("cannot write to {}: {e}", dir.display());
            if is_not_writable(&e) {
                ReplaceError::NotWritable(why)
            } else {
                ReplaceError::Failed(why)
            }
        })?;
    let mut cleanup = RemoveFile(Some(temp.clone()));
    let failed = |e: io::Error| ReplaceError::Failed(format!("cannot write the new AppImage: {e}"));
    let mut source = open_nofollow(verified).map_err(failed)?;
    let copied = copy_hashed(&mut source, &mut out).map_err(failed)?;
    if copied != expected {
        return Err(ReplaceError::Failed(
            "the downloaded AppImage changed after it was verified; nothing was replaced".into(),
        ));
    }
    let mode = (old.mode() & 0o777) | 0o755;
    out.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(failed)?;
    out.sync_all().map_err(failed)?;
    drop(out);
    fs::rename(&temp, &target).map_err(|e| {
        let why = format!("cannot replace {}: {e}", target.display());
        if is_not_writable(&e) {
            ReplaceError::NotWritable(why)
        } else {
            ReplaceError::Failed(why)
        }
    })?;
    cleanup.0 = None;
    // The rename itself is durable once the folder is synced.
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(target)
}

/// A name in `dir` that does not exist yet: `name`, else `stem (2).ext` and so on.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if fs::symlink_metadata(&first).is_err() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (2..1000)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| fs::symlink_metadata(p).is_err())
        .unwrap_or_else(|| dir.join(format!("{stem} ({}){ext}", unique_suffix())))
}

/// Copies the verified file into `dir` as `name` (or a free variant), executable when
/// `executable`, and returns where it went.
pub fn save_copy(
    verified: &Path,
    expected: &str,
    dir: &Path,
    name: &str,
    executable: bool,
) -> Result<PathBuf, String> {
    let target = free_name(dir, name);
    let mode = if executable { 0o755 } else { 0o644 };
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&target)
        .map_err(|e| format!("cannot save {}: {e}", target.display()))?;
    let mut cleanup = RemoveFile(Some(target.clone()));
    let mut source =
        open_nofollow(verified).map_err(|e| format!("cannot read the download: {e}"))?;
    let copied = copy_hashed(&mut source, &mut out)
        .map_err(|e| format!("cannot save {}: {e}", target.display()))?;
    if copied != expected {
        return Err("the download changed after it was verified".into());
    }
    out.sync_all()
        .map_err(|e| format!("cannot save {}: {e}", target.display()))?;
    cleanup.0 = None;
    Ok(target)
}

/// Is `path` inside the e2e sandbox? Automated runs replace only an AppImage there.
pub fn inside_sandbox(path: &Path, sandbox: Option<OsString>) -> bool {
    let Some(sandbox) = sandbox.and_then(|s| fs::canonicalize(PathBuf::from(s)).ok()) else {
        return false;
    };
    fs::canonicalize(path).is_ok_and(|real| real.starts_with(&sandbox))
}

// ---------------------------------------------------------------------------
// deb
// ---------------------------------------------------------------------------

/// The `.deb` at `deb` may be handed to apt: in our own private folder, a regular file of
/// ours with one link and no group or other write bit, and still matching `expected`.
/// Called right before pkexec, so a file changed since the download is refused.
pub fn check_deb(deb: &Path, expected: &str) -> Result<(), String> {
    let dir = deb.parent().ok_or("the package has no folder")?;
    check_private_dir(dir)?;
    let mut file = open_nofollow(deb).map_err(|e| format!("cannot open {}: {e}", deb.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot check {}: {e}", deb.display()))?;
    if !meta.is_file() || meta.uid() != euid() || meta.nlink() != 1 || meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is not the file ZXraw Explorer downloaded",
            deb.display()
        ));
    }
    let digest = hash_file(&mut file).map_err(|e| format!("cannot read {}: {e}", deb.display()))?;
    if digest != expected {
        return Err(
            "the downloaded package changed after it was verified; it was not installed".into(),
        );
    }
    Ok(())
}

/// The control fields of `deb` must be `Package: zxraw`, `Version: <version>` and
/// `Architecture: amd64` (`dpkg-deb -f`, unprivileged). Checked before pkexec, so a wrong
/// file never asks for the password; the root helper checks its own copy again.
pub fn check_deb_metadata(dpkg_deb: &Path, deb: &Path, version: &str) -> Result<(), String> {
    let field = |name: &str| -> Result<String, String> {
        let output = Command::new(dpkg_deb)
            .arg("-f")
            .arg(deb)
            .arg(name)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run {}: {e}", dpkg_deb.display()))?;
        if !output.status.success() {
            return Err(format!("{} is not a Debian package", deb.display()));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let found = (field("Package")?, field("Version")?, field("Architecture")?);
    if (found.0.as_str(), found.1.as_str(), found.2.as_str()) != (DEB_PACKAGE, version, "amd64") {
        return Err(format!(
            "the download is {} {} ({}), not {DEB_PACKAGE} {version} (amd64); it was not installed",
            found.0, found.1, found.2
        ));
    }
    Ok(())
}

/// How a pkexec install-helper run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebOutcome {
    Installed,
    /// The polkit dialog was dismissed, or the password was not accepted.
    Cancelled,
    /// No pkexec, no polkit agent to ask for the password, or no install helper (a package
    /// from before it was added): open the file in the package installer.
    PackageInstaller,
    /// The helper refused the file or apt-get failed; the reason or apt's output (its end).
    Failed(String),
}

/// The install helper's exit codes (`packaging/linux/zxraw-install-update`).
pub const HELPER_BAD_ARGS: i32 = 64;
pub const HELPER_CHECKSUM: i32 = 65;
pub const HELPER_NOT_OURS: i32 = 66;
pub const HELPER_COPY: i32 = 67;
pub const HELPER_APT: i32 = 68;

/// The arguments after `pkexec`.
pub fn pkexec_args(helper: &Path, deb: &Path, sha256: &str, version: &str) -> Vec<OsString> {
    vec![
        helper.as_os_str().to_owned(),
        deb.as_os_str().to_owned(),
        sha256.into(),
        version.into(),
    ]
}

/// Runs `pkexec <helper> <deb> <sha256> <version>` without a shell and waits for it (the
/// user may take a while to type the password). Call off the UI thread.
pub fn run_pkexec(
    pkexec: &Path,
    helper: &Path,
    deb: &Path,
    sha256: &str,
    version: &str,
) -> DebOutcome {
    if !pkexec.is_file() || !helper.is_file() {
        return DebOutcome::PackageInstaller;
    }
    let child = Command::new(pkexec)
        .args(pkexec_args(helper, deb, sha256, version))
        // pkexec's own messages, which tell "no agent" apart, in English.
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => return DebOutcome::Failed(format!("cannot start {}: {e}", pkexec.display())),
    };
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut stream| {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buf = [0u8; 8192];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
                kept.extend_from_slice(&buf[..n]);
                if kept.len() > MAX_OUTPUT {
                    kept.drain(..kept.len() - MAX_OUTPUT);
                }
            }
            kept
        })
    })
    .collect();
    let status = child.wait();
    let output: String = readers
        .into_iter()
        .filter_map(|r| r.join().ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    match status {
        Ok(status) => map_pkexec(status.code(), &output),
        Err(e) => DebOutcome::Failed(format!("{} did not finish: {e}", pkexec.display())),
    }
}

/// pkexec exits 126 when the user dismisses the dialog or authentication fails, and 127
/// when it could not ask (no agent: "No authentication agent found") or was dismissed on
/// older polkit versions. Anything else is the helper's exit code.
pub fn map_pkexec(code: Option<i32>, output: &str) -> DebOutcome {
    match code {
        Some(0) => DebOutcome::Installed,
        Some(126) => DebOutcome::Cancelled,
        Some(127) if output.to_ascii_lowercase().contains("authentication agent") => {
            DebOutcome::PackageInstaller
        }
        Some(127) => DebOutcome::Cancelled,
        Some(HELPER_CHECKSUM) => DebOutcome::Failed(
            "the package changed before it could be installed (its SHA-256 no longer matches); nothing was installed".into(),
        ),
        Some(HELPER_NOT_OURS) => DebOutcome::Failed(
            "the package is not the expected ZXraw Explorer version; nothing was installed".into(),
        ),
        Some(HELPER_COPY) => DebOutcome::Failed(
            "the downloaded package could not be read for installing; nothing was installed".into(),
        ),
        Some(HELPER_BAD_ARGS) => {
            DebOutcome::Failed(format!("the install helper refused the request: {}", output_tail(output, HELPER_BAD_ARGS)))
        }
        Some(HELPER_APT) => DebOutcome::Failed(output_tail(output, HELPER_APT)),
        Some(code) => DebOutcome::Failed(output_tail(output, code)),
        None => DebOutcome::Failed("the install was stopped before it finished".into()),
    }
}

/// The last lines of apt's output, for the notice.
fn output_tail(output: &str, code: i32) -> String {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = &lines[lines.len().saturating_sub(12)..];
    if tail.is_empty() {
        format!("the install failed (exit code {code})")
    } else {
        tail.join("\n")
    }
}

/// The version of the installed `zxraw` package, from `dpkg-query -W`.
pub fn installed_deb_version() -> Option<String> {
    let output = Command::new(DPKG_QUERY)
        .args(["-W", "-f=${Version}", DEB_PACKAGE])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Restart
// ---------------------------------------------------------------------------

/// The AppImage runtime's variables, which describe the old mount.
const APPIMAGE_VARS: &[&str] = &["APPDIR", "APPIMAGE", "ARGV0", "OWD"];

/// `list` (a `PATH`-style value) without the entries under `appdir`; `None` when nothing is
/// left.
pub fn without_entries_under(list: &OsStr, appdir: &Path) -> Option<OsString> {
    let kept: Vec<PathBuf> = std::env::split_paths(list)
        .filter(|p| !p.starts_with(appdir))
        .collect();
    if kept.is_empty() {
        return None;
    }
    std::env::join_paths(kept).ok()
}

/// Starts `program` in its own process group with no arguments (the session restores the
/// tabs), without the old AppImage mount's variables and search paths.
pub fn relaunch(program: &Path) -> io::Result<()> {
    let mut cmd = Command::new(program);
    if let Some(appdir) = std::env::var_os("APPDIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
    {
        for var in ["PATH", "LD_LIBRARY_PATH"] {
            if let Some(value) = std::env::var_os(var) {
                match without_entries_under(&value, &appdir) {
                    Some(kept) => cmd.env(var, kept),
                    None => cmd.env_remove(var),
                };
            }
        }
    }
    for var in APPIMAGE_VARS {
        cmd.env_remove(var);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    /// The first bytes of a type 2 AppImage.
    const APPIMAGE_HEAD: &[u8] = b"\x7fELF\x02\x01\x01\x00AI\x02";

    #[test]
    fn an_appimage_is_detected_only_from_inside_its_mount_with_its_own_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mount = tmp.path().join(".mount_ZXrawAB");
        fs::create_dir_all(mount.join("usr/bin")).unwrap();
        let exe = mount.join("usr/bin/zxraw");
        fs::write(&exe, b"elf").unwrap();
        let elsewhere = tmp.path().join("zxraw");
        fs::write(&elsewhere, b"elf").unwrap();
        let image = tmp.path().join("ZXraw.AppImage");
        fs::write(&image, [APPIMAGE_HEAD, b"payload"].concat()).unwrap();
        let deb = tmp.path().join("usr-bin-zxraw");
        let never = |_: &Path| -> Option<String> { panic!("no dpkg-query for an AppImage") };
        let detect = |image: &Path, appdir: Option<&Path>, exe: &Path| {
            detect_from(
                Some(image.into()),
                appdir.map(|d| d.into()),
                Some(exe.into()),
                &deb,
                never,
            )
        };
        assert_eq!(
            detect(&image, Some(&mount), &exe),
            InstallKind::AppImage(image.clone())
        );
        // Not running from an AppImage mount: $APPIMAGE could name any file.
        assert_eq!(detect(&image, None, &exe), InstallKind::Other);
        assert_eq!(detect(&image, Some(&mount), &elsewhere), InstallKind::Other);
        // The file itself: gone, relative, a folder, a symlink, not an AppImage.
        assert_eq!(
            detect(&tmp.path().join("moved.AppImage"), Some(&mount), &exe),
            InstallKind::Other
        );
        assert_eq!(
            detect(Path::new("ZXraw.AppImage"), Some(&mount), &exe),
            InstallKind::Other
        );
        assert_eq!(detect(tmp.path(), Some(&mount), &exe), InstallKind::Other);
        let link = tmp.path().join("link.AppImage");
        std::os::unix::fs::symlink(&image, &link).unwrap();
        assert_eq!(detect(&link, Some(&mount), &exe), InstallKind::Other);
        let photo = tmp.path().join("photo.jpg");
        fs::write(&photo, b"\xff\xd8\xff\xe0 not an AppImage").unwrap();
        assert_eq!(detect(&photo, Some(&mount), &exe), InstallKind::Other);
        let type1 = tmp.path().join("type1.AppImage");
        fs::write(&type1, b"\x7fELF\x02\x01\x01\x00AI\x01rest").unwrap();
        assert_eq!(detect(&type1, Some(&mount), &exe), InstallKind::Other);
        let short = tmp.path().join("short.AppImage");
        fs::write(&short, b"\x7fELF").unwrap();
        assert_eq!(detect(&short, Some(&mount), &exe), InstallKind::Other);
        // Empty means unset.
        assert_eq!(
            detect_from(Some("".into()), Some(mount.into()), Some(exe), &deb, |_| {
                None
            }),
            InstallKind::Other
        );
    }

    #[test]
    fn the_deb_needs_its_path_and_the_package_that_owns_it() {
        let tmp = tempfile::tempdir().unwrap();
        let deb = tmp.path().join("zxraw");
        fs::write(&deb, b"elf").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&deb, &link).unwrap();
        let other = tmp.path().join("target-debug-zxraw");
        fs::write(&other, b"elf").unwrap();
        let owned = |p: &Path| {
            assert_eq!(p, deb.as_path());
            Some("zxraw".to_string())
        };
        assert_eq!(
            detect_from(None, None, Some(deb.clone()), &deb, owned),
            InstallKind::Deb
        );
        assert_eq!(
            detect_from(None, None, Some(link), &deb, owned),
            InstallKind::Deb,
            "resolved"
        );
        assert_eq!(
            detect_from(None, None, Some(other), &deb, owned),
            InstallKind::Other
        );
        assert_eq!(
            detect_from(None, None, Some(deb.clone()), &deb, |_| Some(
                "zxraw-git".into()
            )),
            InstallKind::Other
        );
        assert_eq!(
            detect_from(None, None, Some(deb.clone()), &deb, |_| None),
            InstallKind::Other
        );
        assert_eq!(
            detect_from(None, None, None, &deb, owned),
            InstallKind::Other
        );
    }

    #[test]
    fn dpkg_query_output_names_one_package() {
        let path = Path::new("/usr/bin/zxraw");
        assert_eq!(
            parse_dpkg_owner("zxraw: /usr/bin/zxraw\n", path).as_deref(),
            Some("zxraw")
        );
        assert_eq!(
            parse_dpkg_owner("zxraw:amd64: /usr/bin/zxraw\n", path).as_deref(),
            Some("zxraw")
        );
        assert_eq!(
            parse_dpkg_owner("diversion by x from: /usr/bin/zxraw\ndiversion by x to: /usr/bin/zxraw.real\nzxraw: /usr/bin/zxraw\n", path).as_deref(),
            Some("zxraw")
        );
        assert_eq!(
            parse_dpkg_owner("a, b: /usr/bin/zxraw\n", path),
            None,
            "two owners"
        );
        assert_eq!(parse_dpkg_owner("zxraw: /usr/bin/zxraw-cli\n", path), None);
        assert_eq!(parse_dpkg_owner("", path), None);
    }

    #[test]
    fn dpkg_query_runs_without_a_shell_and_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let stub = tmp.path().join("dpkg-query");
        fs::write(&stub, "#!/bin/sh\n[ \"$1\" = -S ] && echo \"zxraw: $2\"\n").unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        let path = Path::new("/usr/bin/zxraw");
        assert_eq!(
            dpkg_owner(&stub, path, Duration::from_secs(5)).as_deref(),
            Some("zxraw")
        );
        let slow = tmp.path().join("slow");
        fs::write(&slow, "#!/bin/sh\nexec sleep 30\n").unwrap();
        fs::set_permissions(&slow, fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        assert_eq!(dpkg_owner(&slow, path, Duration::from_millis(200)), None);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            dpkg_owner(&tmp.path().join("missing"), path, Duration::from_secs(1)),
            None
        );
    }

    fn fake_appimage(dir: &Path, mode: u32) -> PathBuf {
        let old = dir.join("ZXraw-Explorer-1.0.0-x86_64.AppImage");
        fs::write(&old, b"old AppImage").unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(mode)).unwrap();
        old
    }

    fn verified(dir: &Path, body: &[u8]) -> (PathBuf, String) {
        let private = dir.join("private");
        create_private_dir(&private).unwrap();
        let file = private.join("ZXraw-Explorer-2.0.0-x86_64.AppImage");
        fs::write(&file, body).unwrap();
        (file, sha(body))
    }

    #[test]
    fn an_appimage_is_replaced_atomically_keeping_its_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let apps = tmp.path().join("Applications");
        fs::create_dir(&apps).unwrap();
        let old = fake_appimage(&apps, 0o750);
        // A file that keeps the old one open sees the old contents: the inode is not reused.
        let mut held = File::open(&old).unwrap();
        let (new, hash) = verified(tmp.path(), b"new AppImage");
        assert_eq!(
            replace_appimage(&new, &hash, &old).unwrap(),
            fs::canonicalize(&old).unwrap()
        );
        assert_eq!(fs::read(&old).unwrap(), b"new AppImage");
        assert_eq!(
            fs::metadata(&old).unwrap().mode() & 0o7777,
            0o755,
            "old mode, at least 0755"
        );
        let mut before = Vec::new();
        held.read_to_end(&mut before).unwrap();
        assert_eq!(before, b"old AppImage");
        assert_eq!(
            fs::read_dir(&apps).unwrap().count(),
            1,
            "no temporary or backup file"
        );

        // A wider old mode is kept.
        fs::set_permissions(&old, fs::Permissions::from_mode(0o775)).unwrap();
        let (new, hash) = (new.with_file_name("again"), sha(b"newer"));
        fs::write(&new, b"newer").unwrap();
        replace_appimage(&new, &hash, &old).unwrap();
        assert_eq!(fs::metadata(&old).unwrap().mode() & 0o7777, 0o775);
    }

    #[test]
    fn a_symlink_or_an_appimage_of_another_group_is_not_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let real = fake_appimage(tmp.path(), 0o755);
        let link = tmp.path().join("zxraw.AppImage");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (new, hash) = verified(tmp.path(), b"new");
        let err = replace_appimage(&new, &hash, &link).unwrap_err();
        assert!(
            matches!(&err, ReplaceError::Failed(m) if m.contains("not a regular file")),
            "{err:?}"
        );
        assert_eq!(fs::read(&real).unwrap(), b"old AppImage");

        // A group of ours other than the current one, if we have one.
        let other_gid = rustix::process::getgroups()
            .unwrap_or_default()
            .into_iter()
            .map(|g| g.as_raw())
            .find(|&g| g != egid());
        if let Some(gid) = other_gid {
            std::os::unix::fs::chown(&real, None, Some(gid)).unwrap();
            let err = replace_appimage(&new, &hash, &real).unwrap_err();
            assert!(
                matches!(&err, ReplaceError::NotWritable(m) if m.contains("another user or group")),
                "{err:?}"
            );
            assert_eq!(fs::read(&real).unwrap(), b"old AppImage");
        }
    }

    #[test]
    fn a_changed_download_or_an_unwritable_folder_leaves_the_old_appimage() {
        let tmp = tempfile::tempdir().unwrap();
        let apps = tmp.path().join("apps");
        fs::create_dir(&apps).unwrap();
        let old = fake_appimage(&apps, 0o755);
        let (new, _) = verified(tmp.path(), b"new");
        let err = replace_appimage(&new, &sha(b"something else"), &old).unwrap_err();
        assert!(
            matches!(&err, ReplaceError::Failed(m) if m.contains("changed after")),
            "{err:?}"
        );
        assert_eq!(fs::read(&old).unwrap(), b"old AppImage");
        assert_eq!(fs::read_dir(&apps).unwrap().count(), 1);

        fs::set_permissions(&apps, fs::Permissions::from_mode(0o555)).unwrap();
        let writable_anyway = File::create(apps.join("probe")).is_ok(); // root ignores modes
        if !writable_anyway {
            let err = replace_appimage(&new, &sha(b"new"), &old).unwrap_err();
            assert!(matches!(err, ReplaceError::NotWritable(_)), "{err:?}");
            assert_eq!(fs::read(&old).unwrap(), b"old AppImage");
        }
        fs::set_permissions(&apps, fs::Permissions::from_mode(0o755)).unwrap();

        // The fallback keeps the verified file, executable, under a free name.
        let downloads = tmp.path().join("Downloads");
        fs::create_dir(&downloads).unwrap();
        fs::write(
            downloads.join("ZXraw-Explorer-2.0.0-x86_64.AppImage"),
            b"earlier",
        )
        .unwrap();
        let saved = save_copy(
            &new,
            &sha(b"new"),
            &downloads,
            "ZXraw-Explorer-2.0.0-x86_64.AppImage",
            true,
        )
        .unwrap();
        assert_eq!(
            saved,
            downloads.join("ZXraw-Explorer-2.0.0-x86_64 (2).AppImage")
        );
        assert_eq!(fs::read(&saved).unwrap(), b"new");
        assert_eq!(fs::metadata(&saved).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn only_a_private_unchanged_deb_is_handed_to_apt() {
        let tmp = tempfile::tempdir().unwrap();
        let private = tmp.path().join("zxraw-update-1");
        create_private_dir(&private).unwrap();
        let deb = private.join("zxraw_2.0.0_amd64.deb");
        fs::write(&deb, b"deb").unwrap();
        fs::set_permissions(&deb, fs::Permissions::from_mode(0o644)).unwrap();
        check_deb(&deb, &sha(b"deb")).unwrap();
        let err = check_deb(&deb, &sha(b"other")).unwrap_err();
        assert!(err.contains("changed after"), "{err}");

        // A symlink in our folder is refused, even to the same contents.
        let link = private.join("link.deb");
        std::os::unix::fs::symlink(&deb, &link).unwrap();
        assert!(check_deb(&link, &sha(b"deb")).is_err());
        fs::remove_file(&link).unwrap();
        // So is a second hard link (the file could be changed through it).
        let hard = private.join("hard.deb");
        fs::hard_link(&deb, &hard).unwrap();
        assert!(check_deb(&deb, &sha(b"deb")).is_err());
        fs::remove_file(&hard).unwrap();
        // And a folder others can open.
        fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            check_deb(&deb, &sha(b"deb"))
                .unwrap_err()
                .contains("not a private folder")
        );
    }

    #[test]
    fn pkexec_outcomes_are_mapped() {
        assert_eq!(map_pkexec(Some(0), ""), DebOutcome::Installed);
        assert_eq!(
            map_pkexec(
                Some(126),
                "Error executing command as another user: Not authorized"
            ),
            DebOutcome::Cancelled
        );
        assert_eq!(
            map_pkexec(
                Some(127),
                "Error executing command as another user: Request dismissed"
            ),
            DebOutcome::Cancelled
        );
        assert_eq!(
            map_pkexec(
                Some(127),
                "Error executing command as another user: No authentication agent found."
            ),
            DebOutcome::PackageInstaller
        );
        match map_pkexec(
            Some(HELPER_APT),
            "Reading package lists...\nE: Unable to locate package\n\n",
        ) {
            DebOutcome::Failed(m) => assert!(m.ends_with("E: Unable to locate package"), "{m}"),
            other => panic!("{other:?}"),
        }
        for (code, words) in [
            (HELPER_CHECKSUM, "SHA-256"),
            (HELPER_NOT_OURS, "not the expected"),
            (HELPER_COPY, "could not be read"),
            (HELPER_BAD_ARGS, "refused the request"),
        ] {
            match map_pkexec(Some(code), "install-update: details") {
                DebOutcome::Failed(m) => assert!(m.contains(words), "{code}: {m}"),
                other => panic!("{code}: {other:?}"),
            }
        }
        assert_eq!(
            map_pkexec(Some(1), ""),
            DebOutcome::Failed("the install failed (exit code 1)".into())
        );
        assert!(matches!(map_pkexec(None, ""), DebOutcome::Failed(_)));
    }

    #[test]
    fn pkexec_runs_the_helper_with_absolute_paths_and_no_shell() {
        let tmp = tempfile::tempdir().unwrap();
        let record = tmp.path().join("args");
        let helper = tmp.path().join("install-update");
        fs::write(&helper, "").unwrap();
        let deb = tmp.path().join("my file; rm -rf ~.deb");
        let sha = "a".repeat(64);
        let stub = |name: &str, body: &str| {
            let path = tmp.path().join(name);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n{body}\n",
                    record.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let ok = stub("ok", "exit 0");
        assert_eq!(
            run_pkexec(&ok, &helper, &deb, &sha, "2.0.0"),
            DebOutcome::Installed
        );
        let args = fs::read_to_string(&record).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                helper.to_str().unwrap(),
                deb.to_str().unwrap(),
                sha.as_str(),
                "2.0.0"
            ]
        );
        let dismissed = stub("dismissed", "exit 126");
        assert_eq!(
            run_pkexec(&dismissed, &helper, &deb, &sha, "2.0.0"),
            DebOutcome::Cancelled
        );
        let no_agent = stub(
            "noagent",
            "echo 'Error executing command as another user: No authentication agent found.' >&2; exit 127",
        );
        assert_eq!(
            run_pkexec(&no_agent, &helper, &deb, &sha, "2.0.0"),
            DebOutcome::PackageInstaller
        );
        let broken = stub(
            "broken",
            "echo 'E: Sub-process /usr/bin/dpkg returned an error code (1)' >&2; exit 68",
        );
        assert_eq!(
            run_pkexec(&broken, &helper, &deb, &sha, "2.0.0"),
            DebOutcome::Failed("E: Sub-process /usr/bin/dpkg returned an error code (1)".into())
        );
        // No pkexec, or a package from before the helper: the package installer.
        assert_eq!(
            run_pkexec(&tmp.path().join("no-pkexec"), &helper, &deb, &sha, "2.0.0"),
            DebOutcome::PackageInstaller
        );
        assert_eq!(
            run_pkexec(&ok, &tmp.path().join("no-helper"), &deb, &sha, "2.0.0"),
            DebOutcome::PackageInstaller
        );
    }

    // -- The root install helper, run as this user with a stand-in apt-get. --

    fn helper_script() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux/zxraw-install-update")
    }

    /// The helper tests need dpkg-deb and must not run as root (the helper then ignores the
    /// test apt-get and would run the real one).
    fn helper_testable() -> bool {
        euid() != 0 && Path::new(DPKG_DEB).is_file()
    }

    fn build_deb(dir: &Path, package: &str, version: &str, arch: &str) -> PathBuf {
        let root = dir.join(format!("{package}-{version}-{arch}"));
        fs::create_dir_all(root.join("DEBIAN")).unwrap();
        fs::write(
            root.join("DEBIAN/control"),
            format!(
                "Package: {package}\nVersion: {version}\nArchitecture: {arch}\n\
                 Maintainer: Test <test@example.com>\nDescription: test package\n"
            ),
        )
        .unwrap();
        let out = dir.join(format!("{package}_{version}_{arch}.deb"));
        let status = Command::new(DPKG_DEB)
            .args(["--build", "--root-owner-group"])
            .arg(&root)
            .arg(&out)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        out
    }

    fn file_sha(path: &Path) -> String {
        sha(&fs::read(path).unwrap())
    }

    /// Runs the helper with `args`, with `apt` as its apt-get when given.
    fn run_helper(args: &[&OsStr], apt: Option<&Path>) -> (i32, String) {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg(helper_script())
            .args(args)
            .env_remove("ZXRAW_INSTALL_UPDATE_TEST_APT");
        if let Some(apt) = apt {
            cmd.env("ZXRAW_INSTALL_UPDATE_TEST_APT", apt);
        }
        let out = cmd.stdin(Stdio::null()).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    #[test]
    fn the_install_helper_checks_its_arguments_and_its_own_copy() {
        if !helper_testable() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let record = tmp.path().join("apt-args");
        let apt = tmp.path().join("apt-get");
        // Records its arguments and what the file it was given holds at install time.
        fs::write(
            &apt,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{0}'\nsha256sum \"$4\" | cut -d ' ' -f 1 >> '{0}'\n",
                record.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&apt, fs::Permissions::from_mode(0o755)).unwrap();
        let failing = tmp.path().join("apt-fails");
        fs::write(
            &failing,
            "#!/bin/sh\necho 'E: broken packages' >&2\nexit 100\n",
        )
        .unwrap();
        fs::set_permissions(&failing, fs::Permissions::from_mode(0o755)).unwrap();

        let good = build_deb(tmp.path(), "zxraw", "2.0.0", "amd64");
        let hash = file_sha(&good);
        let os = |s: &str| OsString::from(s);
        let args = |deb: &Path, sha: &str, version: &str| {
            vec![deb.as_os_str().to_owned(), os(sha), os(version)]
        };
        let run = |a: Vec<OsString>, apt: Option<&Path>| {
            let refs: Vec<&OsStr> = a.iter().map(OsString::as_os_str).collect();
            run_helper(&refs, apt)
        };

        // Installs a copy, not the user's file, and removes the copy afterwards.
        assert_eq!(run(args(&good, &hash, "2.0.0"), Some(&apt)).0, 0);
        let recorded = fs::read_to_string(&record).unwrap();
        let lines: Vec<&str> = recorded.lines().collect();
        assert_eq!(&lines[..3], ["install", "-y", "--no-remove"]);
        let copy = Path::new(lines[3]);
        assert_ne!(copy, good.as_path());
        assert!(copy.is_absolute());
        assert_eq!(lines[4], hash, "the copy is the checked file");
        assert!(
            !copy.exists() && !copy.parent().unwrap().exists(),
            "copy removed"
        );
        fs::remove_file(&record).unwrap();

        // The copy is checked: checksum, then the package fields.
        assert_eq!(
            run(args(&good, &"0".repeat(64), "2.0.0"), Some(&apt)).0,
            HELPER_CHECKSUM
        );
        assert_eq!(
            run(args(&good, &hash, "2.0.1"), Some(&apt)).0,
            HELPER_NOT_OURS
        );
        for (package, arch) in [("zxraw-git", "amd64"), ("zxraw", "i386")] {
            let deb = build_deb(tmp.path(), package, "2.0.0", arch);
            let (code, err) = run(args(&deb, &file_sha(&deb), "2.0.0"), Some(&apt));
            assert_eq!(code, HELPER_NOT_OURS, "{package} {arch}: {err}");
        }
        // Not a regular file: missing, a folder, a symlink.
        let link = tmp.path().join("link.deb");
        std::os::unix::fs::symlink(&good, &link).unwrap();
        for path in [
            tmp.path().join("missing.deb"),
            tmp.path().to_path_buf(),
            link,
        ] {
            assert_eq!(
                run(args(&path, &hash, "2.0.0"), Some(&apt)).0,
                HELPER_COPY,
                "{path:?}"
            );
        }
        assert!(!record.exists(), "apt-get never ran for a refused file");

        // Arguments.
        let bad: Vec<Vec<OsString>> = vec![
            vec![good.as_os_str().to_owned(), os(&hash)],
            args(Path::new("zxraw_2.0.0_amd64.deb"), &hash, "2.0.0"),
            args(&good, &hash.to_uppercase(), "2.0.0"),
            args(&good, &hash[..63], "2.0.0"),
            args(&good, &format!("{hash}\n"), "2.0.0"),
            args(&good, &hash, "2.0"),
            args(&good, &hash, "2.0.0-1"),
            args(&good, &hash, "2.0.0\n2.0.0"),
            args(&tmp.path().join("a\nb.deb"), &hash, "2.0.0"),
        ];
        for a in bad {
            let shown = format!("{a:?}");
            assert_eq!(run(a, Some(&apt)).0, HELPER_BAD_ARGS, "{shown}");
        }
        assert!(!record.exists());

        // apt-get's failure and output come through.
        let (code, err) = run(args(&good, &hash, "2.0.0"), Some(&failing));
        assert_eq!(code, HELPER_APT);
        assert!(err.contains("E: broken packages"), "{err}");
        // Without the test apt-get, a normal user is refused.
        assert_eq!(run(args(&good, &hash, "2.0.0"), None).0, 69);
    }

    #[test]
    fn the_install_helper_passes_shellcheck() {
        let Ok(out) = Command::new("shellcheck")
            .args(["-s", "sh"])
            .arg(helper_script())
            .output()
        else {
            return; // shellcheck is not installed here (CI has it)
        };
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    #[test]
    fn a_wrong_package_is_refused_before_any_password_prompt() {
        if !helper_testable() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dpkg_deb = Path::new(DPKG_DEB);
        let good = build_deb(tmp.path(), "zxraw", "2.0.0", "amd64");
        check_deb_metadata(dpkg_deb, &good, "2.0.0").unwrap();
        let err = check_deb_metadata(dpkg_deb, &good, "2.0.1").unwrap_err();
        assert!(err.contains("not zxraw 2.0.1"), "{err}");
        let other = build_deb(tmp.path(), "zxraw-git", "2.0.0", "amd64");
        assert!(check_deb_metadata(dpkg_deb, &other, "2.0.0").is_err());
        let arch = build_deb(tmp.path(), "zxraw", "2.0.0", "arm64");
        assert!(check_deb_metadata(dpkg_deb, &arch, "2.0.0").is_err());
        let junk = tmp.path().join("junk.deb");
        fs::write(&junk, b"not a deb").unwrap();
        let err = check_deb_metadata(dpkg_deb, &junk, "2.0.0").unwrap_err();
        assert!(err.contains("not a Debian package"), "{err}");

        // End to end through a stand-in pkexec that runs the real helper as this user.
        let apt = tmp.path().join("apt-get");
        fs::write(&apt, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&apt, fs::Permissions::from_mode(0o755)).unwrap();
        let pkexec = tmp.path().join("pkexec");
        fs::write(
            &pkexec,
            format!(
                "#!/bin/sh\nZXRAW_INSTALL_UPDATE_TEST_APT='{}' exec /bin/sh \"$@\"\n",
                apt.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&pkexec, fs::Permissions::from_mode(0o755)).unwrap();
        let helper = helper_script();
        let hash = file_sha(&good);
        assert_eq!(
            run_pkexec(&pkexec, &helper, &good, &hash, "2.0.0"),
            DebOutcome::Installed
        );
        match run_pkexec(&pkexec, &helper, &good, &"0".repeat(64), "2.0.0") {
            DebOutcome::Failed(m) => assert!(m.contains("SHA-256"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_relaunch_drops_the_old_mounts_search_paths() {
        let appdir = Path::new("/tmp/.mount_ZXrawAB12");
        let path = OsString::from("/tmp/.mount_ZXrawAB12/usr/bin:/usr/local/bin:/usr/bin");
        assert_eq!(
            without_entries_under(&path, appdir),
            Some(OsString::from("/usr/local/bin:/usr/bin"))
        );
        assert_eq!(
            without_entries_under(OsStr::new("/tmp/.mount_ZXrawAB12/usr/lib"), appdir),
            None
        );
    }

    #[test]
    fn automated_runs_replace_only_inside_their_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let inside = tmp.path().join("a.AppImage");
        fs::write(&inside, b"x").unwrap();
        assert!(inside_sandbox(&inside, Some(tmp.path().into())));
        assert!(!inside_sandbox(&inside, None));
        let other = tempfile::tempdir().unwrap();
        assert!(!inside_sandbox(&inside, Some(other.path().into())));
    }
}
