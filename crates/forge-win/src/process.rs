//! Launching external tools.

use std::ffi::OsString;
use std::path::Path;
#[cfg(windows)]
use std::process::{Command, Stdio};

use crate::{Result, WinError};

/// Starts `exe` with `args` as an argument vector (no shell is involved), optionally in
/// working directory `cwd`. The process is detached: its handle is dropped and Forge does
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
