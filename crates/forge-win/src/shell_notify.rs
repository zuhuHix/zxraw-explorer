//! Tells Explorer (and the Desktop) about file system changes this app made, the way
//! `IFileOperation` does, so their windows update at once instead of whenever their own
//! file system watchers get round to it. Without these calls the Desktop and Explorer
//! windows only learn of our renames through their interrupt-level file system watchers,
//! which is late.

use std::path::Path;

/// `from` was renamed or moved to `to` (`SHCNE_RENAMEITEM` / `SHCNE_RENAMEFOLDER`).
#[cfg(windows)]
pub fn notify_renamed(from: &Path, to: &Path, is_dir: bool) {
    use windows::Win32::UI::Shell::{SHCNE_RENAMEFOLDER, SHCNE_RENAMEITEM};
    let event = if is_dir {
        SHCNE_RENAMEFOLDER
    } else {
        SHCNE_RENAMEITEM
    };
    send(event, from, Some(to));
}

/// `from` was renamed or moved to `to` (`SHCNE_RENAMEITEM` / `SHCNE_RENAMEFOLDER`).
#[cfg(not(windows))]
pub fn notify_renamed(_from: &Path, _to: &Path, _is_dir: bool) {}

/// `path` was created (`SHCNE_CREATE` / `SHCNE_MKDIR`).
#[cfg(windows)]
pub fn notify_created(path: &Path, is_dir: bool) {
    use windows::Win32::UI::Shell::{SHCNE_CREATE, SHCNE_MKDIR};
    let event = if is_dir { SHCNE_MKDIR } else { SHCNE_CREATE };
    send(event, path, None);
}

/// `path` was created (`SHCNE_CREATE` / `SHCNE_MKDIR`).
#[cfg(not(windows))]
pub fn notify_created(_path: &Path, _is_dir: bool) {}

/// `path` was deleted (`SHCNE_DELETE` / `SHCNE_RMDIR`).
#[cfg(windows)]
pub fn notify_deleted(path: &Path, is_dir: bool) {
    use windows::Win32::UI::Shell::{SHCNE_DELETE, SHCNE_RMDIR};
    let event = if is_dir { SHCNE_RMDIR } else { SHCNE_DELETE };
    send(event, path, None);
}

/// `path` was deleted (`SHCNE_DELETE` / `SHCNE_RMDIR`).
#[cfg(not(windows))]
pub fn notify_deleted(_path: &Path, _is_dir: bool) {}

/// The contents of folder `dir` changed (`SHCNE_UPDATEDIR`).
#[cfg(windows)]
pub fn notify_dir_updated(dir: &Path) {
    use windows::Win32::UI::Shell::SHCNE_UPDATEDIR;
    send(SHCNE_UPDATEDIR, dir, None);
}

/// The contents of folder `dir` changed (`SHCNE_UPDATEDIR`).
#[cfg(not(windows))]
pub fn notify_dir_updated(_dir: &Path) {}

/// Sends one `SHChangeNotify` event with the given paths, without waiting for delivery.
#[cfg(windows)]
fn send(event: windows::Win32::UI::Shell::SHCNE_ID, first: &Path, second: Option<&Path>) {
    use windows::Win32::UI::Shell::{SHCNF_FLUSHNOWAIT, SHCNF_PATHW, SHChangeNotify};
    let first = crate::to_wide(shell_path(first));
    let second = second.map(|p| crate::to_wide(shell_path(p)));
    unsafe {
        SHChangeNotify(
            event,
            SHCNF_PATHW | SHCNF_FLUSHNOWAIT,
            Some(first.as_ptr().cast::<std::ffi::c_void>()),
            second
                .as_ref()
                .map(|w| w.as_ptr().cast::<std::ffi::c_void>()),
        );
    }
}

/// The path as the shell wants it: a `\\?\` verbatim prefix is removed, because the shell
/// does not parse verbatim paths (`\\?\UNC\server\share` becomes `\\server\share`).
#[cfg(windows)]
fn shell_path(path: &Path) -> std::path::PathBuf {
    use std::path::PathBuf;
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    match text.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(unc) => PathBuf::from(format!(r"\\{unc}")),
            None => PathBuf::from(rest),
        },
        None => path.to_path_buf(),
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn verbatim_prefix_is_removed_for_the_shell() {
        assert_eq!(
            shell_path(Path::new(r"\\?\C:\x\a.txt")),
            Path::new(r"C:\x\a.txt")
        );
        assert_eq!(
            shell_path(Path::new(r"\\?\UNC\server\share\a.txt")),
            Path::new(r"\\server\share\a.txt")
        );
        assert_eq!(
            shell_path(Path::new(r"C:\plain\a.txt")),
            Path::new(r"C:\plain\a.txt")
        );
        assert_eq!(
            shell_path(Path::new(r"\\server\share\a.txt")),
            Path::new(r"\\server\share\a.txt")
        );
    }
}
