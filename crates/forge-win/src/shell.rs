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

/// Sends each path to the Recycle Bin (or the sandbox set by [`set_recycle_sandbox`]).
pub fn recycle(paths: &[PathBuf]) -> Result<()> {
    match recycle_sandbox() {
        Some(dir) => recycle_to_sandbox(&dir, paths),
        None => recycle_shell(paths),
    }
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

/// Sends each path to the Recycle Bin with `IFileOperation`.
///
/// Runs on the calling thread, which gets its own apartment-threaded COM initialisation
/// (the call is safe from any background thread). No confirmation or error UI is shown,
/// except the shell's own warning when an item cannot be recycled and would be destroyed;
/// failures come back as errors. If the shell aborted any item, returns
/// [`WinError::Aborted`].
#[cfg(windows)]
fn recycle_shell(paths: &[PathBuf]) -> Result<()> {
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING,
        FOFX_RECYCLEONDELETE, FileOperation, IFileOperation, IShellItem,
        SHCreateItemFromParsingName,
    };
    use windows::core::PCWSTR;

    if paths.is_empty() {
        return Ok(());
    }
    let _com = com::ComGuard::apartment_threaded()?;

    unsafe {
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)
            .map_err(|e| crate::api_err("CoCreateInstance(IFileOperation)", e))?;
        op.SetOperationFlags(
            // FOF_WANTNUKEWARNING overrides FOF_NOCONFIRMATION for items that cannot be
            // recycled (network shares, most removable media, oversized files): the shell asks
            // before destroying them instead of silently deleting permanently.
            FOF_ALLOWUNDO
                | FOFX_RECYCLEONDELETE
                | FOF_NOCONFIRMATION
                | FOF_WANTNUKEWARNING
                | FOF_SILENT
                | FOF_NOERRORUI,
        )
        .map_err(|e| crate::api_err("IFileOperation::SetOperationFlags", e))?;

        for p in paths {
            let wide = crate::to_wide(p);
            // Creating every item first means a bad path aborts before anything is queued.
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)
                .map_err(|e| crate::api_err("SHCreateItemFromParsingName", e))?;
            op.DeleteItem(&item, None)
                .map_err(|e| crate::api_err("IFileOperation::DeleteItem", e))?;
        }

        op.PerformOperations()
            .map_err(|e| crate::api_err("IFileOperation::PerformOperations", e))?;
        let aborted = op
            .GetAnyOperationsAborted()
            .map_err(|e| crate::api_err("IFileOperation::GetAnyOperationsAborted", e))?;
        if aborted.as_bool() {
            return Err(WinError::Aborted { op: "recycle" });
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn recycle_shell(_paths: &[PathBuf]) -> Result<()> {
    Err(WinError::Unsupported)
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
pub fn open_folder_in_explorer(_path: &Path) -> Result<()> {
    Err(WinError::Unsupported)
}

#[cfg(windows)]
fn shell_execute(verb: &str, path: &Path) -> Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    let _com = com::ComGuard::apartment_threaded()?;
    let verb = crate::to_wide(verb);
    let file = crate::to_wide(path);
    // ShellExecuteW returns a value greater than 32 on success, otherwise an SE_ERR code.
    let rc = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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
