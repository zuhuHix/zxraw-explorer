//! Shell operations: Recycle Bin, open, Open-with, Properties, reveal, known folders.

use std::path::{Path, PathBuf};

use crate::{Result, WinError};

/// Sends each path to the Recycle Bin with `IFileOperation`.
///
/// Runs on the calling thread, which gets its own apartment-threaded COM initialisation
/// (the call is safe from any background thread). No confirmation or error UI is shown,
/// except the shell's own warning when an item cannot be recycled and would be destroyed;
/// failures come back as errors. If the shell aborted any item, returns
/// [`WinError::Aborted`].
#[cfg(windows)]
pub fn recycle(paths: &[PathBuf]) -> Result<()> {
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING,
        FOFX_RECYCLEONDELETE,
        FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName,
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
pub fn recycle(_paths: &[PathBuf]) -> Result<()> {
    Err(WinError::Unsupported)
}

/// Opens `path` with its registered default handler (ShellExecuteW verb "open").
/// The path is passed as a single argument; no command string is built.
#[cfg(windows)]
pub fn open_default(path: &Path) -> Result<()> {
    shell_execute("open", path)
}

#[cfg(not(windows))]
pub fn open_default(_path: &Path) -> Result<()> {
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
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music,
        FOLDERID_Pictures, FOLDERID_Profile, FOLDERID_Videos, SHGetKnownFolderPath,
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

    let mut out = Vec::new();
    for (label, id) in LIST {
        let res = unsafe { SHGetKnownFolderPath(*id, Default::default(), None) };
        let Ok(pwstr) = res else { continue };
        let path = unsafe { pwstr.to_string() };
        unsafe { CoTaskMemFree(Some(pwstr.0 as *const _)) };
        if let Ok(path) = path {
            out.push(((*label).to_string(), PathBuf::from(path)));
        }
    }
    out
}

#[cfg(not(windows))]
pub fn known_folders() -> Vec<(String, PathBuf)> {
    Vec::new()
}

/// Apartment-threaded COM initialisation for the current thread, undone on drop.
#[cfg(windows)]
mod com {
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
    fn recycle_empty_list_is_ok() {
        recycle(&[]).unwrap();
    }
}
