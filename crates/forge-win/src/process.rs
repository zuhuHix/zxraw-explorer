//! Launching external tools.

use std::ffi::OsString;
use std::path::Path;
#[cfg(windows)]
use std::process::{Command, Stdio};

use crate::{Result, WinError};

/// Starts `exe` with `args` as an argument vector (no shell is involved), optionally in
/// working directory `cwd`. The process is detached: its handle is dropped and ZXraw Explorer does
/// not wait for it or kill it. Standard streams are connected to nothing.
///
/// Batch scripts (`.bat`, `.cmd`) are refused: Windows runs them through `cmd.exe`, where
/// the argument quoting rules would reintroduce shell injection.
#[cfg(windows)]
pub fn launch(exe: &Path, args: &[OsString], cwd: Option<&Path>) -> Result<()> {
    if let Some(ext) = exe.extension().and_then(|e| e.to_str())
        && (ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd"))
    {
        return Err(WinError::Invalid(format!(
            "refusing to run batch script {}",
            exe.display()
        )));
    }

    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // The child is not created with CREATE_NO_WINDOW: GUI tools must show their windows.
    let child = cmd.spawn().map_err(|source| WinError::Io {
        op: "launch",
        path: exe.to_path_buf(),
        source,
    })?;
    // Dropping a std Child does not terminate the process on Windows; that is the detach.
    drop(child);
    Ok(())
}

#[cfg(not(windows))]
pub fn launch(_exe: &Path, _args: &[OsString], _cwd: Option<&Path>) -> Result<()> {
    Err(WinError::Unsupported)
}

/// Creates the named mutex `name` and keeps it until the process exits, so an installer
/// (Inno Setup's `AppMutex`) can tell that the app is running. Never fails the caller's
/// startup: an error only means the installer cannot see this instance.
#[cfg(windows)]
pub fn hold_named_mutex(name: &str) -> Result<()> {
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    let wide = crate::to_wide(name);
    // The handle is deliberately leaked: Windows closes it when the process ends, which is
    // exactly when the installer may go ahead.
    unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }
        .map(|_| ())
        .map_err(|e| crate::api_err("CreateMutexW", e))
}

#[cfg(not(windows))]
pub fn hold_named_mutex(_name: &str) -> Result<()> {
    Err(WinError::Unsupported)
}

/// Starts a console program in its own new console window through ShellExecuteW, so it gets a
/// real console (std::process with null stdio would make a shell exit at once). `params` is the
/// raw parameter string (already quoted by the caller). `elevated` uses the "runas" verb (UAC
/// prompt). The process is detached.
#[cfg(windows)]
pub fn launch_console(exe: &Path, params: &str, cwd: Option<&Path>, elevated: bool) -> Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    let _com = crate::shell::com::ComGuard::apartment_threaded()?;
    let verb = crate::to_wide(if elevated { "runas" } else { "open" });
    let file = crate::to_wide(exe);
    let params = (!params.is_empty()).then(|| crate::to_wide(params));
    let dir = cwd.map(crate::to_wide);
    let params_ptr = params
        .as_ref()
        .map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr()));
    let dir_ptr = dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr()));
    // ShellExecuteW returns a value greater than 32 on success, otherwise an SE_ERR code.
    let rc = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            params_ptr,
            dir_ptr,
            SW_SHOWNORMAL,
        )
    };
    let code = rc.0 as isize;
    if code > 32 {
        Ok(())
    } else if elevated {
        Err(WinError::Api {
            op: "ShellExecuteW",
            code: code as u32,
            message: format!(
                "the administrator prompt was declined or failed ({})",
                exe.display()
            ),
        })
    } else {
        Err(WinError::Api {
            op: "ShellExecuteW",
            code: code as u32,
            message: format!("could not start {}", exe.display()),
        })
    }
}

#[cfg(not(windows))]
pub fn launch_console(
    _exe: &Path,
    _params: &str,
    _cwd: Option<&Path>,
    _elevated: bool,
) -> Result<()> {
    Err(WinError::Unsupported)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn batch_scripts_are_refused() {
        let err = launch(Path::new(r"C:\tools\run.BAT"), &[], None).unwrap_err();
        assert!(matches!(err, WinError::Invalid(_)));
    }

    #[test]
    fn missing_executable_reports_io_error() {
        let err = launch(Path::new(r"C:\definitely\missing\tool.exe"), &[], None).unwrap_err();
        assert!(matches!(err, WinError::Io { .. }));
    }
}
