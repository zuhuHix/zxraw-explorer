//! Shell operations: Recycle Bin, open, Open-with, Properties, reveal, known folders.

use std::path::{Path, PathBuf};

use crate::{Result, WinError};

static RECYCLE_SANDBOX: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Test seam: while set, [`recycle`] moves items into `dir` instead of the real Recycle
/// Bin. Automated UI runs set this so they can never touch the user's Recycle Bin.
pub fn set_recycle_sandbox(dir: Option<PathBuf>) {
    *RECYCLE_SANDBOX.lock().unwrap_or_else(|p| p.into_inner()) = dir;
}

/// Whether [`recycle`] currently goes to a sandbox instead of the real Recycle Bin.
pub fn recycle_sandbox_active() -> bool {
    recycle_sandbox().is_some()
}

fn recycle_sandbox() -> Option<PathBuf> {
    RECYCLE_SANDBOX
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// Sends each path to the Recycle Bin (or the sandbox set by [`set_recycle_sandbox`]),
/// without progress. Never deletes permanently. See [`crate::recycle_items`] for a per-item
/// report, and [`recycle_result`] for the errors.
pub fn recycle(paths: &[PathBuf]) -> Result<()> {
    recycle_result(&crate::recycle::recycle_items(paths, Box::new(|_| true)))
}

/// A recycle's report as one result. [`WinError::NotDeleted`] when any item stays, naming
/// the first and saying whether the rest was cancelled; [`WinError::Aborted`] when it was
/// only cancelled.
pub fn recycle_result(report: &crate::DeleteReport) -> Result<()> {
    let mut parts = Vec::new();
    if let Some((path, why)) = report.failed.first() {
        let more = report.failed.len() - 1;
        let more = if more > 0 {
            format!(" (and {more} more)")
        } else {
            String::new()
        };
        parts.push(format!("{}: {why}{more}", path.display()));
    }
    if let Some(path) = report.partly_deleted.first() {
        parts.push(format!(
            "{}: partly deleted, some of its contents may already be gone",
            path.display()
        ));
    }
    if report.cancelled {
        if parts.is_empty() {
            return Err(WinError::Aborted { op: "recycle" });
        }
        let left = report.not_started.len();
        parts.push(format!("cancelled; {left} more item(s) left as they were"));
    }
    if parts.is_empty() {
        Ok(())
    } else {
        Err(WinError::NotDeleted(parts.join("; ")))
    }
}

pub(crate) fn recycle_sandbox_dir() -> Option<PathBuf> {
    recycle_sandbox()
}

/// Sandbox "Recycle Bin": every item is renamed into a fresh, newly created numbered
/// folder under `dir` (`dir/<n>/<name>`), so nothing already there can be replaced. Like
/// the shell, a missing item fails the whole call before anything is moved. Items on
/// another volume than `dir` fail with an error instead of being copied.
pub fn recycle_to_sandbox(dir: &Path, paths: &[PathBuf]) -> Result<()> {
    let io = |op: &'static str, path: &Path| {
        let path = path.to_path_buf();
        move |source| WinError::Io { op, path, source }
    };
    for p in paths {
        std::fs::symlink_metadata(p).map_err(io("recycle (sandbox)", p))?;
    }
    std::fs::create_dir_all(dir).map_err(io("create recycle sandbox", dir))?;
    for p in paths {
        let name = p
            .file_name()
            .ok_or_else(|| WinError::Invalid(format!("cannot recycle {}", p.display())))?;
        let slot = (0u32..)
            .map(|n| dir.join(n.to_string()))
            .find_map(|slot| match std::fs::create_dir(&slot) {
                Ok(()) => Some(Ok(slot)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(e) => Some(Err(e)),
            })
            .expect("unbounded range")
            .map_err(io("create recycle sandbox slot", dir))?;
        std::fs::rename(p, slot.join(name)).map_err(io("recycle (sandbox)", p))?;
    }
    Ok(())
}

/// Opens files with their registered handler. Existing directories are always sent directly to
/// Windows Explorer so making ZXraw the Directory default cannot route its own reveal/open flows
/// back into itself.
#[cfg(windows)]
pub fn open_default(path: &Path) -> Result<()> {
    if path.is_dir() {
        open_folder_in_explorer(path)
    } else {
        shell_execute("open", path)
    }
}

/// On Linux, opens `path` with `gio open` (or `xdg-open`) after checking that it exists.
#[cfg(target_os = "linux")]
pub fn open_default(path: &Path) -> Result<()> {
    linux::open(path)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn open_default(_path: &Path) -> Result<()> {
    Err(WinError::Unsupported)
}

/// Opens a filesystem folder or drive through `explorer.exe` explicitly.
#[cfg(windows)]
pub fn open_folder_in_explorer(path: &Path) -> Result<()> {
    let windows_dir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let explorer = windows_dir.join("explorer.exe");
    std::process::Command::new(&explorer)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|source| WinError::Io {
            op: "open folder in Explorer",
            path: path.to_path_buf(),
            source,
        })
}

/// On Linux there is no separate folder opener: `gio open` opens a folder in the file manager.
#[cfg(target_os = "linux")]
pub fn open_folder_in_explorer(path: &Path) -> Result<()> {
    linux::open(path)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn open_folder_in_explorer(_path: &Path) -> Result<()> {
    Err(WinError::Unsupported)
}

/// The working directory a file opened from ZXraw gets: its own folder, as Explorer does, so
/// scripts and programs that use relative paths find their neighbours. `None` (inherit) for
/// URLs, shell URIs and anything else that is not an absolute path in an existing folder.
/// `cmd.exe` cannot run in a UNC folder and falls back to the Windows folder, as it does when
/// a script is started from Explorer.
pub(crate) fn working_dir_for(path: &Path) -> Option<&Path> {
    if !path.is_absolute() {
        return None;
    }
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty() && dir.is_dir())
}

#[cfg(windows)]
fn shell_execute(verb: &str, path: &Path) -> Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    shell_execute_with(verb, path, SW_SHOWNORMAL)
}

#[cfg(windows)]
fn shell_execute_with(
    verb: &str,
    path: &Path,
    show: windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD,
) -> Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::core::PCWSTR;

    let _com = com::ComGuard::apartment_threaded()?;
    let verb = crate::to_wide(verb);
    let file = crate::to_wide(path);
    let dir = working_dir_for(path).map(crate::to_wide);
    let dir_ptr = dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr()));
    // ShellExecuteW returns a value greater than 32 on success, otherwise an SE_ERR code.
    let rc = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            dir_ptr,
            show,
        )
    };
    let code = rc.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(WinError::Api {
            op: "ShellExecuteW",
            code: code as u32,
            message: format!("could not open {}", path.display()),
        })
    }
}

/// Shows the "Open with" dialog for `path`.
#[cfg(windows)]
pub fn open_with_dialog(path: &Path) -> Result<()> {
    use windows::Win32::UI::Shell::{
        OAIF_ALLOW_REGISTRATION, OAIF_EXEC, OPENASINFO, SHOpenWithDialog,
    };
    use windows::core::PCWSTR;

    let _com = com::ComGuard::apartment_threaded()?;
    let file = crate::to_wide(path);
    let info = OPENASINFO {
        pcszFile: PCWSTR(file.as_ptr()),
        pcszClass: PCWSTR::null(),
        oaifInFlags: OAIF_EXEC | OAIF_ALLOW_REGISTRATION,
    };
    unsafe { SHOpenWithDialog(None, &info) }.map_err(|e| crate::api_err("SHOpenWithDialog", e))
}

/// On Linux, asks the XDG desktop portal for its app chooser (approximates the Windows dialog);
/// without a portal it opens `path` with its default application instead.
#[cfg(target_os = "linux")]
pub fn open_with_dialog(path: &Path) -> Result<()> {
    linux::open_with(path)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn open_with_dialog(_path: &Path) -> Result<()> {
    Err(WinError::Unsupported)
}

/// Shows the Explorer Properties dialog for `path`.
#[cfg(windows)]
pub fn show_properties(path: &Path) -> Result<()> {
    use windows::Win32::UI::Shell::{SHOP_FILEPATH, SHObjectProperties};
    use windows::core::PCWSTR;

    let file = crate::to_wide(path);
    let ok =
        unsafe { SHObjectProperties(None, SHOP_FILEPATH, PCWSTR(file.as_ptr()), PCWSTR::null()) };
    if ok.as_bool() {
        Ok(())
    } else {
        Err(crate::last_io_err("SHObjectProperties", path))
    }
}

/// On Linux, asks the file manager (FileManager1) for its properties dialog. No fallback: if the
/// file manager cannot show it, the error is returned.
#[cfg(target_os = "linux")]
pub fn show_properties(path: &Path) -> Result<()> {
    linux::show_properties(path)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn show_properties(_path: &Path) -> Result<()> {
    Err(WinError::Unsupported)
}

/// Opens the containing folder of `path` in Explorer with `path` selected.
#[cfg(windows)]
pub fn reveal_in_explorer(path: &Path) -> Result<()> {
    use windows::Win32::UI::Shell::{ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems};
    use windows::core::PCWSTR;

    let _com = com::ComGuard::apartment_threaded()?;
    let wide = crate::to_wide(path);
    unsafe {
        let pidl = ILCreateFromPathW(PCWSTR(wide.as_ptr()));
        if pidl.is_null() {
            return Err(crate::last_io_err("ILCreateFromPathW", path));
        }
        // With no child list, the PIDL of a file makes Explorer open its folder and
        // select that file.
        let res = SHOpenFolderAndSelectItems(pidl, None, 0);
        ILFree(Some(pidl as *const _));
        res.map_err(|e| crate::api_err("SHOpenFolderAndSelectItems", e))
    }
}

/// On Linux, asks the file manager (FileManager1) to select `path`. If that fails, the parent
/// folder is opened instead.
#[cfg(target_os = "linux")]
pub fn reveal_in_explorer(path: &Path) -> Result<()> {
    linux::reveal(path)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn reveal_in_explorer(_path: &Path) -> Result<()> {
    Err(WinError::Unsupported)
}

/// Well-known user folders as `(label, path)`. Folders that Windows cannot resolve are
/// omitted. Labels: Desktop, Downloads, Documents, Pictures, Videos, Music, Home.
#[cfg(windows)]
pub fn known_folders() -> Vec<(String, PathBuf)> {
    use windows::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music,
        FOLDERID_Pictures, FOLDERID_Profile, FOLDERID_Videos,
    };

    const LIST: &[(&str, &windows::core::GUID)] = &[
        ("Desktop", &FOLDERID_Desktop),
        ("Downloads", &FOLDERID_Downloads),
        ("Documents", &FOLDERID_Documents),
        ("Pictures", &FOLDERID_Pictures),
        ("Videos", &FOLDERID_Videos),
        ("Music", &FOLDERID_Music),
        ("Home", &FOLDERID_Profile),
    ];

    LIST.iter()
        .filter_map(|(label, id)| Some(((*label).to_string(), known_folder(id)?)))
        .collect()
}

#[cfg(target_os = "linux")]
pub fn known_folders() -> Vec<(String, PathBuf)> {
    linux::known_folders()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn known_folders() -> Vec<(String, PathBuf)> {
    Vec::new()
}

/// Points this user's shortcuts that launch `exe` (Start menu, desktop and pinned taskbar
/// items) at the icon resource `index` inside `exe`, so they match the app icon setting.
/// Shortcuts to anything else are never modified, and shortcuts that cannot be read are
/// skipped. Returns how many shortcuts changed. Pinned taskbar items may keep showing the
/// old icon until Explorer restarts.
pub fn set_shortcut_icon(exe: &Path, index: i32) -> Result<usize> {
    #[cfg(windows)]
    {
        set_shortcut_icon_in(&shortcut_dirs(), exe, index)
    }
    #[cfg(not(windows))]
    {
        let _ = (exe, index);
        Err(WinError::Unsupported)
    }
}

/// [`set_shortcut_icon`] over the `.lnk` files directly inside `dirs`.
#[cfg(windows)]
pub fn set_shortcut_icon_in(dirs: &[PathBuf], exe: &Path, index: i32) -> Result<usize> {
    use windows::Win32::UI::Shell::{SHCNE_UPDATEITEM, SHCNF_PATHW, SHChangeNotify};

    let _com = com::ComGuard::apartment_threaded()?;
    let app = same_file_key(exe);
    let mut changed = 0;
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()) {
            if !path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
            {
                continue;
            }
            let updated = Link::open(&path).and_then(|link| {
                if same_file_key(&link.target()?) != app {
                    return Ok(false);
                }
                let (icon, current) = link.icon()?;
                // No icon location means "the target's first icon".
                let icon = if icon.as_os_str().is_empty() {
                    exe.to_path_buf()
                } else {
                    icon
                };
                if current == index && same_file_key(&icon) == app {
                    return Ok(false);
                }
                link.set_icon(exe, index)?;
                Ok(true)
            });
            match updated {
                Ok(true) => {
                    changed += 1;
                    let wide = crate::to_wide(&path);
                    unsafe {
                        SHChangeNotify(
                            SHCNE_UPDATEITEM,
                            SHCNF_PATHW,
                            Some(wide.as_ptr().cast()),
                            None,
                        )
                    };
                }
                Ok(false) => {}
                Err(e) => tracing::debug!("skipping shortcut {}: {e}", path.display()),
            }
        }
    }
    Ok(changed)
}

/// Folders holding this user's shortcuts that may launch the app.
#[cfg(windows)]
fn shortcut_dirs() -> Vec<PathBuf> {
    use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Programs, FOLDERID_RoamingAppData};

    let mut dirs: Vec<PathBuf> = [&FOLDERID_Programs, &FOLDERID_Desktop]
        .into_iter()
        .filter_map(known_folder)
        .collect();
    if let Some(roaming) = known_folder(&FOLDERID_RoamingAppData) {
        dirs.push(roaming.join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar"));
    }
    dirs
}

#[cfg(windows)]
fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::SHGetKnownFolderPath;

    let pwstr = unsafe { SHGetKnownFolderPath(id, Default::default(), None) }.ok()?;
    let path = unsafe { pwstr.to_string() };
    unsafe { CoTaskMemFree(Some(pwstr.0 as *const _)) };
    path.ok().map(PathBuf::from)
}

/// Comparable form of a path: resolved when it exists, lowercase either way.
#[cfg(windows)]
fn same_file_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_lowercase()
}

/// A `.lnk` shortcut opened for reading and updating. Needs COM on the calling thread.
#[cfg(windows)]
struct Link {
    link: windows::Win32::UI::Shell::IShellLinkW,
    file: windows::Win32::System::Com::IPersistFile,
}

#[cfg(windows)]
impl Link {
    fn open(path: &Path) -> Result<Self> {
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, STGM_READWRITE};
        use windows::Win32::UI::Shell::ShellLink;
        use windows::core::{Interface, PCWSTR};

        let wide = crate::to_wide(path);
        unsafe {
            let link: windows::Win32::UI::Shell::IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| crate::api_err("CoCreateInstance(ShellLink)", e))?;
            let file: windows::Win32::System::Com::IPersistFile =
                link.cast().map_err(|e| crate::api_err("IPersistFile", e))?;
            file.Load(PCWSTR(wide.as_ptr()), STGM_READWRITE)
                .map_err(|e| crate::api_err("IPersistFile::Load", e))?;
            Ok(Self { link, file })
        }
    }

    fn target(&self) -> Result<PathBuf> {
        let mut buf = vec![0u16; 32_768];
        unsafe { self.link.GetPath(&mut buf, std::ptr::null_mut(), 0) }
            .map_err(|e| crate::api_err("IShellLink::GetPath", e))?;
        Ok(PathBuf::from(from_wide(&buf)))
    }

    fn icon(&self) -> Result<(PathBuf, i32)> {
        let mut buf = vec![0u16; 32_768];
        let mut index = 0;
        unsafe { self.link.GetIconLocation(&mut buf, &mut index) }
            .map_err(|e| crate::api_err("IShellLink::GetIconLocation", e))?;
        Ok((PathBuf::from(from_wide(&buf)), index))
    }

    fn set_icon(&self, exe: &Path, index: i32) -> Result<()> {
        use windows::core::PCWSTR;

        let wide = crate::to_wide(exe);
        unsafe {
            self.link
                .SetIconLocation(PCWSTR(wide.as_ptr()), index)
                .map_err(|e| crate::api_err("IShellLink::SetIconLocation", e))?;
            self.file
                .Save(PCWSTR::null(), true)
                .map_err(|e| crate::api_err("IPersistFile::Save", e))
        }
    }
}

#[cfg(windows)]
fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Apartment-threaded COM initialisation for the current thread, undone on drop.
#[cfg(windows)]
pub(crate) mod com {
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
    use windows::core::HRESULT;

    use crate::{Result, WinError};

    /// RPC_E_CHANGED_MODE: the thread is already initialised as MTA. Usable as is.
    const RPC_E_CHANGED_MODE: HRESULT = HRESULT(0x8001_0106_u32 as i32);

    pub(crate) struct ComGuard {
        uninit: bool,
    }

    impl ComGuard {
        pub(crate) fn apartment_threaded() -> Result<Self> {
            let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            if hr.is_ok() {
                Ok(Self { uninit: true })
            } else if hr == RPC_E_CHANGED_MODE {
                Ok(Self { uninit: false })
            } else {
                Err(WinError::Api {
                    op: "CoInitializeEx",
                    code: hr.0 as u32,
                    message: "COM initialisation failed".to_string(),
                })
            }
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.uninit {
                unsafe { CoUninitialize() };
            }
        }
    }
}

/// Linux desktop integration. Opening goes through `gio open` (or `xdg-open`); reveal and
/// properties through the FileManager1 D-Bus interface, called with `gdbus` and falling back to
/// `dbus-send`; the app chooser through the XDG desktop portal; known folders from
/// `user-dirs.dirs`. Every external program is run directly, never through a shell, with its
/// stdin closed.
#[cfg(target_os = "linux")]
pub(crate) mod linux {
    use std::ffi::OsStr;
    use std::fmt::Write as _;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    use crate::{Result, WinError};

    const FILE_MANAGER: &str = "org.freedesktop.FileManager1";
    const FILE_MANAGER_PATH: &str = "/org/freedesktop/FileManager1";
    const PORTAL: &str = "org.freedesktop.portal.Desktop";
    const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
    /// How long the file manager may take to answer a D-Bus call.
    const FILE_MANAGER_TIMEOUT: Duration = Duration::from_secs(3);
    /// The portal may first have to start its backend, so it gets longer.
    const PORTAL_TIMEOUT: Duration = Duration::from_secs(5);

    fn io_error(op: &'static str, path: &Path, source: io::Error) -> WinError {
        WinError::Io {
            op,
            path: path.to_path_buf(),
            source,
        }
    }

    /// `path` made absolute against the current folder, without resolving links.
    fn absolute(path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map_or_else(|_| path.to_path_buf(), |dir| dir.join(path))
        }
    }

    /// The path must exist, as ShellExecute requires. A missing path is `NotFound`.
    fn ensure_exists(op: &'static str, path: &Path) -> Result<()> {
        std::fs::metadata(path)
            .map(drop)
            .map_err(|source| io_error(op, path, source))
    }

    /// Starts `program` detached: no stdin and no output. Returns once the program has been
    /// started, not when it exits.
    fn spawn_detached(
        program: &'static str,
        args: &[&OsStr],
        dir: Option<&Path>,
    ) -> io::Result<()> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        let child = command.spawn()?;
        reap(child, program);
        Ok(())
    }

    /// Waits for `child` on a background thread so it never stays a zombie.
    fn reap(mut child: Child, program: &'static str) {
        let thread = std::thread::Builder::new()
            .name(format!("reap {program}"))
            .spawn(move || match child.wait() {
                Ok(status) if !status.success() => {
                    tracing::debug!("{program} exited with {status}")
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("could not wait for {program}: {e}"),
            });
        if let Err(e) = thread {
            tracing::debug!("no thread to reap {program}: {e}");
        }
    }

    /// Opens `path` with its default application: `gio open`, or `xdg-open` when gio cannot be
    /// started. Returns `Io` when neither can be started.
    pub(crate) fn open(path: &Path) -> Result<()> {
        ensure_exists("open", path)?;
        let dir = super::working_dir_for(path);
        match spawn_detached("gio", &[OsStr::new("open"), path.as_os_str()], dir) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::debug!("gio cannot open {}: {e}; trying xdg-open", path.display());
                spawn_detached("xdg-open", &[path.as_os_str()], dir)
                    .map_err(|source| io_error("open", path, source))
            }
        }
    }

    /// Runs `command` to the end with stdout discarded. The error is the first line of stderr,
    /// or why the program could not run.
    fn run(mut command: Command) -> std::result::Result<(), String> {
        let program = command.get_program().to_string_lossy().into_owned();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match command.output() {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stderr);
                let line = text.lines().map(str::trim).find(|line| !line.is_empty());
                Err(line.map_or_else(
                    || format!("{program} failed ({})", out.status),
                    str::to_string,
                ))
            }
            Err(e) => Err(format!("{program} could not run: {e}")),
        }
    }

    /// Calls FileManager1 `method`, which takes one file URI and a startup id, through `gdbus`
    /// (with its own timeout), then `dbus-send` if that fails.
    fn file_manager_call(method: &str, uri: &str) -> std::result::Result<(), String> {
        let mut gdbus = Command::new("gdbus");
        gdbus
            .args(["call", "--session", "--timeout"])
            .arg(FILE_MANAGER_TIMEOUT.as_secs().to_string())
            .args([
                "--dest",
                FILE_MANAGER,
                "--object-path",
                FILE_MANAGER_PATH,
                "--method",
            ])
            .arg(format!("{FILE_MANAGER}.{method}"))
            // GVariant text: an array of one string, then the empty startup id.
            .arg(format!("[{}]", gvariant_string(uri)))
            .arg("\"\"");
        let gdbus_err = match run(gdbus) {
            Ok(()) => return Ok(()),
            Err(why) => why,
        };

        let mut dbus = Command::new("dbus-send");
        dbus.args(["--session", "--print-reply"])
            .arg(format!(
                "--reply-timeout={}",
                FILE_MANAGER_TIMEOUT.as_millis()
            ))
            .arg(format!("--dest={FILE_MANAGER}"))
            .arg(FILE_MANAGER_PATH)
            .arg(format!("{FILE_MANAGER}.{method}"))
            // dbus-send splits array elements at commas; the URI is percent-encoded, so it has none.
            .arg(format!("array:string:{uri}"))
            .arg("string:");
        run(dbus).map_err(|dbus_err| format!("{gdbus_err}; {dbus_err}"))
    }

    /// Selects `path` in the file manager. If the file manager cannot do that, its parent
    /// folder is opened instead.
    pub(crate) fn reveal(path: &Path) -> Result<()> {
        let path = absolute(path);
        ensure_exists("reveal in file manager", &path)?;
        match file_manager_call("ShowItems", &file_uri(&path)) {
            Ok(()) => Ok(()),
            Err(why) => {
                tracing::debug!(
                    "no file manager selection for {}: {why}; opening its folder",
                    path.display()
                );
                open(path.parent().unwrap_or(path.as_path()))
            }
        }
    }

    /// Asks the file manager to show the properties of `path`. No fallback.
    pub(crate) fn show_properties(path: &Path) -> Result<()> {
        let path = absolute(path);
        ensure_exists("show properties", &path)?;
        file_manager_call("ShowItemProperties", &file_uri(&path))
            .map_err(|why| io_error("show properties", &path, io::Error::other(why)))
    }

    /// Shows the portal's app chooser for `path` (`ask` = true). Without a working portal,
    /// `path` opens with its default application.
    pub(crate) fn open_with(path: &Path) -> Result<()> {
        let path = absolute(path);
        ensure_exists("open with", &path)?;
        let mut portal = Command::new("gdbus");
        portal
            .args(["call", "--session", "--timeout"])
            .arg(PORTAL_TIMEOUT.as_secs().to_string())
            .args([
                "--dest",
                PORTAL,
                "--object-path",
                PORTAL_PATH,
                "--method",
                "org.freedesktop.portal.OpenURI.OpenURI",
            ])
            // parent window (none), the URI, and the options.
            .arg("\"\"")
            .arg(gvariant_string(&file_uri(&path)))
            .arg("{'ask': <true>}");
        match run(portal) {
            Ok(()) => Ok(()),
            Err(why) => {
                tracing::debug!(
                    "the portal could not show an app chooser: {why}; opening {}",
                    path.display()
                );
                open(&path)
            }
        }
    }

    /// The XDG user folders that exist, in the Windows list's order: Desktop, Downloads,
    /// Documents, Pictures, Videos, Music, then Home.
    pub(crate) fn known_folders() -> Vec<(String, PathBuf)> {
        let Some(home) = absolute_env("HOME") else {
            return Vec::new();
        };
        let config = absolute_env("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let contents = std::fs::read_to_string(config.join("user-dirs.dirs")).unwrap_or_default();
        user_dir_candidates(&contents, &home)
            .into_iter()
            .filter(|(_, path)| path.is_dir())
            .map(|(label, path)| (label.to_string(), path))
            .collect()
    }

    fn absolute_env(name: &str) -> Option<PathBuf> {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    }

    /// The XDG user directories the Windows list shows, with the XDG key each one comes from
    /// and the folder name XDG uses when the file does not set it.
    const USER_DIRS: &[(&str, &str, &str)] = &[
        ("Desktop", "DESKTOP", "Desktop"),
        ("Downloads", "DOWNLOAD", "Downloads"),
        ("Documents", "DOCUMENTS", "Documents"),
        ("Pictures", "PICTURES", "Pictures"),
        ("Videos", "VIDEOS", "Videos"),
        ("Music", "MUSIC", "Music"),
    ];

    /// The Windows-order list of `(label, path)` from the contents of `user-dirs.dirs`, with
    /// `$HOME` taken from `home`. Missing keys get the XDG default under `home`. Nothing is
    /// checked on disk; `Home` is always last.
    pub(crate) fn user_dir_candidates(contents: &str, home: &Path) -> Vec<(&'static str, PathBuf)> {
        let parsed = parse_user_dirs(contents, home);
        let mut out: Vec<(&'static str, PathBuf)> = USER_DIRS
            .iter()
            .map(|(label, key, default)| {
                let path = parsed
                    .iter()
                    .find(|(name, _)| name == key)
                    .map_or_else(|| home.join(default), |(_, path)| path.clone());
                (*label, path)
            })
            .collect();
        out.push(("Home", home.to_path_buf()));
        out
    }

    /// The `XDG_<KEY>_DIR="value"` lines of `user-dirs.dirs` as `(KEY, path)`. `$HOME` is
    /// expanded with `home`. Comments, other lines, and values that are not quoted or not
    /// absolute (after `$HOME` expansion) are skipped.
    pub(crate) fn parse_user_dirs(contents: &str, home: &Path) -> Vec<(String, PathBuf)> {
        contents
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.starts_with('#') {
                    return None;
                }
                let (key, value) = line.split_once('=')?;
                let key = key.trim().strip_prefix("XDG_")?.strip_suffix("_DIR")?;
                let value = unquote(value.trim())?;
                let path = if value == "$HOME" {
                    home.to_path_buf()
                } else if let Some(rest) = value.strip_prefix("$HOME/") {
                    home.join(rest)
                } else if value.starts_with('/') {
                    PathBuf::from(value)
                } else {
                    return None;
                };
                Some((key.to_string(), path))
            })
            .collect()
    }

    /// The inside of a double-quoted value, with `\"` and `\\` unescaped. `None` when the value
    /// is not quoted.
    fn unquote(value: &str) -> Option<String> {
        let inner = value.strip_prefix('"')?.strip_suffix('"')?;
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                out.push(chars.next()?);
            } else {
                out.push(c);
            }
        }
        Some(out)
    }

    /// `file://` URI of the absolute `path`. Every byte except the unreserved characters and
    /// `/` is percent-encoded, so spaces, `%`, `#`, commas and non-ASCII names all survive
    /// GVariant text and dbus-send's `array:string:` list.
    pub(crate) fn file_uri(path: &Path) -> String {
        format!(
            "file://{}",
            percent_encode(path.as_os_str().as_bytes(), true)
        )
    }

    /// Percent-encodes `bytes`, leaving only the unreserved characters (and `/` when
    /// `keep_slash`). Hex digits are upper case.
    pub(crate) fn percent_encode(bytes: &[u8], keep_slash: bool) -> String {
        let mut out = String::with_capacity(bytes.len());
        for &byte in bytes {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    out.push(char::from(byte))
                }
                b'/' if keep_slash => out.push('/'),
                _ => {
                    let _ = write!(out, "%{byte:02X}");
                }
            }
        }
        out
    }

    /// `text` as a GVariant string literal, the syntax `gdbus call` reads its arguments in.
    /// Backslashes and single quotes are escaped, and control characters too.
    pub(crate) fn gvariant_string(text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 2);
        out.push('\'');
        for c in text.chars() {
            match c {
                '\\' => out.push_str(r"\\"),
                '\'' => out.push_str(r"\'"),
                '\n' => out.push_str(r"\n"),
                '\r' => out.push_str(r"\r"),
                '\t' => out.push_str(r"\t"),
                c if c.is_control() => {
                    let _ = write!(out, r"\u{:04x}", c as u32);
                }
                c => out.push(c),
            }
        }
        out.push('\'');
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn file_uri_encodes_spaces_percent_and_unicode() {
            assert_eq!(
                file_uri(Path::new("/home/u/My File.txt")),
                "file:///home/u/My%20File.txt"
            );
            assert_eq!(
                file_uri(Path::new("/tmp/100%.txt")),
                "file:///tmp/100%25.txt"
            );
            assert_eq!(
                file_uri(Path::new("/tmp/Café 1.txt")),
                "file:///tmp/Caf%C3%A9%201.txt"
            );
            assert_eq!(
                file_uri(Path::new("/a/b,c#d?e+f[g]'h")),
                "file:///a/b%2Cc%23d%3Fe%2Bf%5Bg%5D%27h"
            );
            assert_eq!(file_uri(Path::new("/a/keep-._~Z9")), "file:///a/keep-._~Z9");
            let raw = OsStr::from_bytes(b"/x/\xff");
            assert_eq!(file_uri(Path::new(raw)), "file:///x/%FF");
        }

        #[test]
        fn gvariant_strings_escape_quotes_backslashes_and_controls() {
            assert_eq!(gvariant_string("plain"), "'plain'");
            assert_eq!(gvariant_string(r"it's \ here"), r"'it\'s \\ here'");
            assert_eq!(gvariant_string("a\nb"), r"'a\nb'");
            assert_eq!(gvariant_string("\u{1}"), r"'\u0001'");
            assert_eq!(gvariant_string(""), "''");
        }

        #[test]
        fn user_dirs_expand_home_and_skip_junk() {
            let sample = "# comment\n\
                XDG_DESKTOP_DIR=\"$HOME/Escritorio\"\n\
                XDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\n\
                XDG_DOCUMENTS_DIR=\"/data/docs\"\n\
                XDG_MUSIC_DIR=\"$HOME\"\n\
                XDG_TEMPLATES_DIR=\"$HOME/Templates\"\n\
                XDG_PUBLICSHARE_DIR=\"relative/no\"\n\
                XDG_PICTURES_DIR=unquoted\n\
                not a line\n";
            let parsed = parse_user_dirs(sample, Path::new("/home/u"));
            assert_eq!(
                parsed,
                vec![
                    ("DESKTOP".to_string(), PathBuf::from("/home/u/Escritorio")),
                    ("DOWNLOAD".to_string(), PathBuf::from("/home/u/Downloads")),
                    ("DOCUMENTS".to_string(), PathBuf::from("/data/docs")),
                    ("MUSIC".to_string(), PathBuf::from("/home/u")),
                    ("TEMPLATES".to_string(), PathBuf::from("/home/u/Templates")),
                ]
            );
        }

        #[test]
        fn user_dirs_unescape_quotes_and_backslashes() {
            let parsed = parse_user_dirs(r#"XDG_PICTURES_DIR="/x/a \"b\" \\c""#, Path::new("/h"));
            assert_eq!(
                parsed,
                vec![("PICTURES".to_string(), PathBuf::from(r#"/x/a "b" \c"#))]
            );
        }

        #[test]
        fn candidates_follow_the_windows_order_with_xdg_defaults() {
            let sample = "XDG_DESKTOP_DIR=\"$HOME/Escritorio\"\nXDG_MUSIC_DIR=\"$HOME\"\n";
            let got = user_dir_candidates(sample, Path::new("/home/u"));
            let labels: Vec<&str> = got.iter().map(|(label, _)| *label).collect();
            assert_eq!(
                labels,
                [
                    "Desktop",
                    "Downloads",
                    "Documents",
                    "Pictures",
                    "Videos",
                    "Music",
                    "Home"
                ]
            );
            assert_eq!(got[0].1, PathBuf::from("/home/u/Escritorio"));
            // Not in the file: the XDG defaults under home.
            assert_eq!(got[1].1, PathBuf::from("/home/u/Downloads"));
            assert_eq!(got[3].1, PathBuf::from("/home/u/Pictures"));
            assert_eq!(got[5].1, PathBuf::from("/home/u"));
            assert_eq!(got[6].1, PathBuf::from("/home/u"));
        }

        #[test]
        fn candidates_without_a_file_are_the_defaults() {
            let got = user_dir_candidates("", Path::new("/h"));
            assert_eq!(got.len(), 7);
            assert_eq!(got[2].1, PathBuf::from("/h/Documents"));
            assert_eq!(got[6], ("Home", PathBuf::from("/h")));
        }

        /// The existence check runs before anything is spawned, so no program starts here.
        #[test]
        fn missing_paths_fail_with_not_found_before_anything_starts() {
            let d = tempfile::tempdir().unwrap();
            let missing = d.path().join("nope.txt");
            for result in [
                crate::shell::open_default(&missing),
                crate::shell::reveal_in_explorer(&missing),
                crate::shell::show_properties(&missing),
                crate::shell::open_with_dialog(&missing),
            ] {
                match result {
                    Err(WinError::Io { source, .. }) => {
                        assert_eq!(source.kind(), io::ErrorKind::NotFound)
                    }
                    other => panic!("expected NotFound, got {other:?}"),
                }
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn known_folders_contains_documents() {
        let folders = known_folders();
        let docs = folders
            .iter()
            .find(|(label, _)| label == "Documents")
            .expect("Documents known folder");
        assert!(docs.1.is_dir(), "{}", docs.1.display());
    }

    #[test]
    #[ignore = "sends a file to the real Recycle Bin; run with --ignored"]
    fn recycle_removes_file_from_disk() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("to-recycle.txt");
        std::fs::write(&f, b"bin me").unwrap();
        recycle(std::slice::from_ref(&f)).unwrap();
        assert!(!f.exists(), "file still present after recycle");
    }

    #[test]
    fn a_recycle_result_keeps_failures_and_the_cancel() {
        let report = crate::DeleteReport {
            failed: vec![(PathBuf::from("a"), crate::DeleteFailure::InUse)],
            not_started: vec![PathBuf::from("b")],
            cancelled: true,
            ..Default::default()
        };
        match recycle_result(&report) {
            Err(WinError::NotDeleted(m)) => {
                assert!(m.contains("in use") && m.contains("cancelled"), "{m}")
            }
            other => panic!("{other:?}"),
        }
        let only_cancelled = crate::DeleteReport {
            cancelled: true,
            ..Default::default()
        };
        assert!(matches!(
            recycle_result(&only_cancelled),
            Err(WinError::Aborted { .. })
        ));
        assert!(recycle_result(&crate::DeleteReport::default()).is_ok());
    }

    #[test]
    fn recycle_missing_path_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("nope.txt");
        assert!(recycle(&[missing]).is_err());
    }

    #[test]
    fn sandbox_recycle_moves_items_without_replacing() {
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("bin");
        let a = d.path().join("same.txt");
        std::fs::write(&a, b"first").unwrap();
        recycle_to_sandbox(&bin, std::slice::from_ref(&a)).unwrap();
        std::fs::write(&a, b"second").unwrap();
        let folder = d.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("inner.txt"), b"x").unwrap();
        recycle_to_sandbox(&bin, &[a.clone(), folder.clone()]).unwrap();
        assert!(!a.exists() && !folder.exists());
        assert_eq!(
            std::fs::read(bin.join("0").join("same.txt")).unwrap(),
            b"first"
        );
        assert_eq!(
            std::fs::read(bin.join("1").join("same.txt")).unwrap(),
            b"second"
        );
        assert!(bin.join("2").join("folder").join("inner.txt").is_file());
    }

    #[test]
    fn sandbox_recycle_with_a_missing_item_moves_nothing() {
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("bin");
        let present = d.path().join("present.txt");
        std::fs::write(&present, b"keep").unwrap();
        let missing = d.path().join("missing.txt");
        assert!(recycle_to_sandbox(&bin, &[present.clone(), missing]).is_err());
        assert!(
            present.exists(),
            "nothing may move when one item is missing"
        );
    }

    #[test]
    fn working_dir_is_the_files_folder() {
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("run.bat");
        assert_eq!(working_dir_for(&script), Some(d.path()));
        assert_eq!(
            working_dir_for(Path::new(r"C:\run.bat")),
            Some(Path::new(r"C:\"))
        );
        // Not absolute paths or folders that do not exist: inherit.
        assert_eq!(working_dir_for(Path::new("run.bat")), None);
        assert_eq!(
            working_dir_for(&d.path().join("gone").join("run.bat")),
            None
        );
        assert_eq!(working_dir_for(Path::new(r"C:\")), None);
        // URLs and shell URIs also go through `open_default`.
        assert_eq!(working_dir_for(Path::new("https://example.com/a/b")), None);
        assert_eq!(working_dir_for(Path::new("ms-settings:defaultapps")), None);
    }

    #[test]
    fn opened_batch_script_runs_in_its_own_folder() {
        use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

        let d = tempfile::tempdir().unwrap();
        let folder = d.path().join("script folder");
        std::fs::create_dir(&folder).unwrap();
        let script = folder.join("where.bat");
        std::fs::write(&script, "@echo %CD%> \"%~dp0cwd.txt\"\r\n").unwrap();
        let out = folder.join("cwd.txt");
        shell_execute_with("open", &script, SW_HIDE).unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let recorded = loop {
            if let Ok(text) = std::fs::read_to_string(&out)
                && text.ends_with('\n')
            {
                break text;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the script did not write {}",
                out.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(
            recorded
                .trim()
                .eq_ignore_ascii_case(&folder.to_string_lossy()),
            "script ran in {recorded:?}, expected {}",
            folder.display()
        );
    }

    #[test]
    fn recycle_empty_list_is_ok() {
        recycle(&[]).unwrap();
    }

    #[test]
    fn shortcut_icons_change_only_for_shortcuts_to_the_app() {
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
        use windows::core::{Interface, PCWSTR};

        let d = tempfile::tempdir().unwrap();
        let app = d.path().join("zxraw.exe");
        let other = d.path().join("other.exe");
        std::fs::write(&app, b"").unwrap();
        std::fs::write(&other, b"").unwrap();
        let _com = com::ComGuard::apartment_threaded().unwrap();
        let make = |name: &str, target: &Path| {
            let lnk = d.path().join(name);
            let target = crate::to_wide(target);
            let path = crate::to_wide(&lnk);
            unsafe {
                let link: IShellLinkW =
                    CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
                link.SetPath(PCWSTR(target.as_ptr())).unwrap();
                let file: IPersistFile = link.cast().unwrap();
                file.Save(PCWSTR(path.as_ptr()), true).unwrap();
            }
            lnk
        };
        let app_lnk = make("ZXraw Explorer.lnk", &app);
        let other_lnk = make("Other.lnk", &other);
        let icon = |lnk: &Path| Link::open(lnk).unwrap().icon().unwrap();
        let dirs = [d.path().to_path_buf()];

        // A fresh shortcut already shows the first icon, so asking for it changes nothing.
        assert_eq!(set_shortcut_icon_in(&dirs, &app, 0).unwrap(), 0);
        assert_eq!(set_shortcut_icon_in(&dirs, &app, 1).unwrap(), 1);
        let (path, index) = icon(&app_lnk);
        assert_eq!((same_file_key(&path), index), (same_file_key(&app), 1));
        assert_eq!(
            icon(&other_lnk),
            (PathBuf::new(), 0),
            "other shortcuts are untouched"
        );
        assert_eq!(set_shortcut_icon_in(&dirs, &app, 1).unwrap(), 0);
        assert_eq!(set_shortcut_icon_in(&dirs, &app, 0).unwrap(), 1);
        assert_eq!(icon(&app_lnk).1, 0);
    }
}
