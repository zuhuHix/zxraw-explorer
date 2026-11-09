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

/// Unix: the program gets its own process group, so a Ctrl-C in the terminal ZXraw Explorer was
/// started from does not reach it. No shell: `exe` and `args` go to `execve` as given. Scripts
/// are not refused here (a `#!` script runs as its interpreter decides).
#[cfg(unix)]
pub fn launch(exe: &Path, args: &[OsString], cwd: Option<&Path>) -> Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    spawn_detached(cmd).map_err(|source| WinError::Io {
        op: "launch",
        path: exe.to_path_buf(),
        source,
    })
}

#[cfg(not(any(windows, unix)))]
pub fn launch(_exe: &Path, _args: &[OsString], _cwd: Option<&Path>) -> Result<()> {
    Err(WinError::Unsupported)
}

/// Starts `cmd` with null standard streams, in its own process group, and leaves it running.
/// A background thread waits for it, so an exited child never stays a zombie.
#[cfg(unix)]
fn spawn_detached(mut cmd: std::process::Command) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = cmd.spawn()?;
    let reaper = std::thread::Builder::new()
        .name("reap-child".into())
        .spawn(move || {
            let _ = child.wait();
        });
    if let Err(e) = reaper {
        tracing::warn!("could not start a thread to reap a child process: {e}");
    }
    Ok(())
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

/// Unix: an exclusive `flock` on `<name>.lock` in `$XDG_RUNTIME_DIR` (`/tmp` without it), kept
/// until the process exits. As on Windows, a name that another process already holds does not
/// fail: the lock is then simply not taken, and the installer sees the other holder.
#[cfg(unix)]
pub fn hold_named_mutex(name: &str) -> Result<()> {
    if let Some(lock) = acquire_named_lock(&runtime_dir(), name)? {
        // Held for the life of the process, like the Windows handle.
        std::mem::forget(lock);
    }
    Ok(())
}

/// The lock file of `name` in `dir`, locked: `Ok(None)` when another process holds it.
#[cfg(unix)]
fn acquire_named_lock(dir: &Path, name: &str) -> Result<Option<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let path = dir.join(format!("{}.lock", sanitize_name(name)));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        // Never follow a symlink planted at the lock path.
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|source| WinError::Io {
            op: "open the lock file",
            path: path.clone(),
            source,
        })?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let source = std::io::Error::last_os_error();
    if source.kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(WinError::Io {
            op: "lock",
            path,
            source,
        })
    }
}

/// `name` with every character but ASCII letters, digits, `-`, `_` and `.` replaced by `_`.
#[cfg(unix)]
fn sanitize_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "mutex".into()
    } else {
        clean
    }
}

/// `$XDG_RUNTIME_DIR` when it is set to an absolute path, else `/tmp`.
#[cfg(unix)]
fn runtime_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
}

#[cfg(not(any(windows, unix)))]
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

/// Unix: starts a console program for the user. `params` is split into words the way a POSIX
/// shell quotes them (see [`split_params`]); no shell runs.
///
/// * `elevated`: `pkexec exe args...` (a polkit password prompt). pkexec has no terminal, so a
///   console program's output is not shown, and a declined prompt is not reported (the process
///   is detached).
/// * otherwise: `x-terminal-emulator -e exe args...` in a new terminal window. Some terminal
///   emulators take `-e` as one command string rather than an argument vector; this follows the
///   Debian alternatives convention. Without any `x-terminal-emulator`, `exe` runs directly with
///   no console at all.
///
/// Everything is detached as in [`launch`]: a failure to start is reported, a later failure is not.
#[cfg(unix)]
pub fn launch_console(
    exe: &Path,
    params: &str,
    cwd: Option<&Path>,
    elevated: bool,
) -> Result<()> {
    let words = split_params(params)
        .map_err(|why| WinError::Invalid(format!("parameters of {}: {why}", exe.display())))?;
    let args: Vec<OsString> = words.into_iter().map(OsString::from).collect();
    let io = |source| WinError::Io {
        op: "launch_console",
        path: exe.to_path_buf(),
        source,
    };
    let with_dir = |mut cmd: std::process::Command| {
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        cmd
    };

    if elevated {
        let mut cmd = std::process::Command::new("pkexec");
        cmd.arg(exe).args(&args);
        return spawn_detached(with_dir(cmd)).map_err(io);
    }
    let mut terminal = std::process::Command::new("x-terminal-emulator");
    terminal.arg("-e").arg(exe).args(&args);
    match spawn_detached(with_dir(terminal)) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    let mut direct = std::process::Command::new(exe);
    direct.args(&args);
    spawn_detached(with_dir(direct)).map_err(io)
}

/// Splits `text` into words the way a POSIX shell quotes them: whitespace separates words;
/// single quotes keep everything literal; double quotes keep everything except a backslash
/// before `"`, `\`, `$` or a backtick; a backslash outside quotes escapes the next character.
/// Nothing is expanded. An unclosed quote or a trailing backslash is an error.
#[cfg(unix)]
fn split_params(text: &str) -> std::result::Result<Vec<String>, String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quoting {
        Plain,
        Single,
        Double,
    }

    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quoting = Quoting::Plain;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quoting, c) {
            (Quoting::Plain, c) if c.is_whitespace() => {
                if let Some(done) = word.take() {
                    words.push(done);
                }
            }
            (Quoting::Plain, '\'') => {
                word.get_or_insert_with(String::new);
                quoting = Quoting::Single;
            }
            (Quoting::Plain, '"') => {
                word.get_or_insert_with(String::new);
                quoting = Quoting::Double;
            }
            (Quoting::Plain, '\\') => {
                let next = chars.next().ok_or("a backslash at the end")?;
                word.get_or_insert_with(String::new).push(next);
            }
            (Quoting::Plain, c) => word.get_or_insert_with(String::new).push(c),
            (Quoting::Single, '\'') => quoting = Quoting::Plain,
            (Quoting::Single, c) => word.get_or_insert_with(String::new).push(c),
            (Quoting::Double, '"') => quoting = Quoting::Plain,
            (Quoting::Double, '\\') => {
                let escaped = match chars.peek() {
                    Some(&n @ ('"' | '\\' | '$' | '`')) => {
                        chars.next();
                        n
                    }
                    _ => '\\',
                };
                word.get_or_insert_with(String::new).push(escaped);
            }
            (Quoting::Double, c) => word.get_or_insert_with(String::new).push(c),
        }
    }
    if quoting != Quoting::Plain {
        return Err("a quote is not closed".into());
    }
    if let Some(done) = word {
        words.push(done);
    }
    Ok(words)
}

#[cfg(not(any(windows, unix)))]
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

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    #[test]
    fn a_missing_program_reports_an_io_error() {
        let err = launch(Path::new("/definitely/missing/tool"), &[], None).unwrap_err();
        assert!(matches!(err, WinError::Io { .. }));
    }

    #[test]
    fn launch_runs_the_program_with_its_arguments_in_the_directory() {
        // `touch` is harmless; it proves the argument vector and the working directory.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        launch(Path::new("touch"), &[OsString::from("marker")], Some(dir.path())).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(marker.exists(), "the program did not run in {}", dir.path().display());
    }

    #[test]
    fn parameters_split_like_a_posix_shell_quotes_them() {
        assert_eq!(
            split_params(r#"a "b c" 'd e' f\ g "h\"i" 'j\k'"#).unwrap(),
            ["a", "b c", "d e", "f g", "h\"i", "j\\k"]
        );
        assert_eq!(split_params("   ").unwrap(), Vec::<String>::new());
        assert_eq!(split_params(r#""" ''"#).unwrap(), ["", ""]);
        assert_eq!(split_params(r#""a\b\\c""#).unwrap(), [r"a\b\c"]);
        assert_eq!(split_params("$HOME `x`").unwrap(), ["$HOME", "`x`"], "nothing expands");
    }

    #[test]
    fn unclosed_quotes_and_trailing_backslashes_are_refused() {
        assert!(split_params("'open").is_err());
        assert!(split_params(r#""open"#).is_err());
        assert!(split_params("trailing\\").is_err());
        // Refused before anything is started.
        let err = launch_console(Path::new("/bin/true"), "'open", None, false).unwrap_err();
        assert!(matches!(err, WinError::Invalid(_)));
    }

    #[test]
    fn a_held_name_is_taken_once_and_a_second_claim_does_not_fail() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_named_lock(dir.path(), "ZXrawTest").unwrap();
        assert!(first.is_some());
        assert!(
            acquire_named_lock(dir.path(), "ZXrawTest").unwrap().is_none(),
            "a second holder got the lock"
        );
        drop(first);
        assert!(
            acquire_named_lock(dir.path(), "ZXrawTest").unwrap().is_some(),
            "the name is free again once its holder ends"
        );
    }

    #[test]
    fn names_are_sanitized_into_file_names() {
        assert_eq!(sanitize_name("ZX Raw/../x"), "ZX_Raw_.._x");
        assert_eq!(sanitize_name(""), "mutex");
    }
}
