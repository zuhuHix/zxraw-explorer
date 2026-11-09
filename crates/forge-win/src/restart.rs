//! Which processes hold a file open, via the Restart Manager API.
//!
//! Intended for "access denied / file in use" diagnostics. Restart Manager reports
//! applications that have registered or locked the resource; it may miss some holders
//! (e.g. kernel-level or services without a registered name).

use std::path::Path;

use crate::{Result, WinError};

/// Returns `(pid, application name)` for each process currently using `path`.
#[cfg(any(windows, target_os = "linux"))]
pub fn file_locking_processes(path: &Path) -> Result<Vec<(u32, String)>> {
    locking_processes(std::slice::from_ref(&path))
}

/// The names of the programs using `path` or, for a folder, any of the first `max_files`
/// files inside it. Sorted, without duplicates; empty when none are found or the lookup
/// fails.
pub fn processes_using(path: &Path, max_files: usize) -> Vec<String> {
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let mut dirs = vec![path.to_path_buf()];
    if !path.is_dir() {
        files.push(path.to_path_buf());
        dirs.clear();
    }
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            if files.len() >= max_files {
                break;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => dirs.push(e.path()),
                Ok(_) => files.push(e.path()),
                Err(_) => {}
            }
        }
    }
    let mut names: Vec<String> = Vec::new();
    #[cfg(any(windows, target_os = "linux"))]
    for chunk in files.chunks(500) {
        let refs: Vec<&Path> = chunk.iter().map(|p| p.as_path()).collect();
        if let Ok(found) = locking_processes(&refs) {
            names.extend(found.into_iter().map(|(_, name)| name));
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(windows)]
fn locking_processes(paths: &[&Path]) -> Result<Vec<(u32, String)>> {
    use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR};
    use windows::Win32::System::RestartManager::{
        CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
        RmStartSession,
    };
    use windows::core::{PCWSTR, PWSTR};

    fn rm_err(op: &'static str, code: WIN32_ERROR) -> WinError {
        WinError::Api {
            op,
            code: code.0,
            message: std::io::Error::from_raw_os_error(code.0 as i32).to_string(),
        }
    }

    let mut session: u32 = 0;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    let rc = unsafe { RmStartSession(&mut session, None, PWSTR(key.as_mut_ptr())) };
    if rc != ERROR_SUCCESS {
        return Err(rm_err("RmStartSession", rc));
    }

    // Always end the session, whatever happens below.
    let result = (|| {
        let wide: Vec<Vec<u16>> = paths.iter().map(crate::to_wide).collect();
        let files: Vec<PCWSTR> = wide.iter().map(|w| PCWSTR(w.as_ptr())).collect();
        let rc = unsafe { RmRegisterResources(session, Some(&files), None, None) };
        if rc != ERROR_SUCCESS {
            return Err(rm_err("RmRegisterResources", rc));
        }

        let mut needed: u32 = 0;
        let mut count: u32 = 0;
        let mut reasons: u32 = 0;
        let mut infos: Vec<RM_PROCESS_INFO> = Vec::new();
        loop {
            let rc = unsafe {
                RmGetList(
                    session,
                    &mut needed,
                    &mut count,
                    if infos.is_empty() {
                        None
                    } else {
                        Some(infos.as_mut_ptr())
                    },
                    &mut reasons,
                )
            };
            if rc == ERROR_SUCCESS {
                infos.truncate(count as usize);
                break;
            }
            if rc != ERROR_MORE_DATA {
                return Err(rm_err("RmGetList", rc));
            }
            // Buffer too small: `needed` holds the required element count. Retry.
            infos = (0..needed).map(|_| RM_PROCESS_INFO::default()).collect();
            count = needed;
        }

        Ok(infos
            .iter()
            .map(|info| {
                let name = info.strAppName;
                let end = name.iter().position(|c| *c == 0).unwrap_or(name.len());
                let app = String::from_utf16_lossy(&name[..end]);
                (info.Process.dwProcessId, app)
            })
            .collect())
    })();

    // The session is ending either way; the status carries nothing actionable.
    let _ = unsafe { RmEndSession(session) };
    result
}

/// Linux: the processes with `paths` open, found by scanning `/proc/<pid>/fd` once.
///
/// Unlike Restart Manager this lists the holders of a file, not of a lock: Linux has no
/// file locks that block a delete, so "in use" here means "open". A descriptor matches when its
/// device and inode are those of a path (so a renamed file is still found), or when the kernel's
/// path for it is the path given (also for a deleted file, shown with a " (deleted)" suffix).
/// Processes whose descriptors cannot be read (another user's, or already gone) are skipped.
/// Each process appears once, by pid. Our own process counts, as it does on Windows.
#[cfg(target_os = "linux")]
fn locking_processes(paths: &[&Path]) -> Result<Vec<(u32, String)>> {
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    let mut ids: Vec<(u64, u64)> = Vec::new();
    let mut targets: Vec<PathBuf> = Vec::new();
    for path in paths {
        if let Ok(meta) = std::fs::metadata(path) {
            ids.push((meta.dev(), meta.ino()));
        }
        targets.push(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    }

    let procs = std::fs::read_dir("/proc").map_err(|source| WinError::Io {
        op: "read the process list",
        path: PathBuf::from("/proc"),
        source,
    })?;
    let mut found = Vec::new();
    for entry in procs.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let proc_dir = entry.path();
        // Unreadable for other users' processes; gone for exited ones.
        let Ok(fds) = std::fs::read_dir(proc_dir.join("fd")) else {
            continue;
        };
        let holds = fds.flatten().any(|fd| {
            let fd = fd.path();
            let Ok(link) = std::fs::read_link(&fd) else {
                return false;
            };
            // Pipes, sockets and anonymous inodes show as `pipe:[..]` and the like.
            if !link.is_absolute() {
                return false;
            }
            if targets.contains(&strip_deleted(&link)) {
                return true;
            }
            std::fs::metadata(&fd)
                .is_ok_and(|meta| ids.contains(&(meta.dev(), meta.ino())))
        });
        if holds {
            found.push((pid, process_name(&proc_dir, pid)));
        }
    }
    found.sort();
    Ok(found)
}

/// `link` without the " (deleted)" suffix the kernel adds to the path of a deleted file.
#[cfg(target_os = "linux")]
fn strip_deleted(link: &Path) -> std::path::PathBuf {
    match link.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(path) => std::path::PathBuf::from(path),
        None => link.to_path_buf(),
    }
}

/// The program name of process `pid` (`/proc/<pid>/comm`), or `pid N` when it cannot be read.
#[cfg(target_os = "linux")]
fn process_name(proc_dir: &Path, pid: u32) -> String {
    std::fs::read_to_string(proc_dir.join("comm"))
        .map(|name| name.trim_end().to_string())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("pid {pid}"))
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn file_locking_processes(_path: &Path) -> Result<Vec<(u32, String)>> {
    Err(WinError::Unsupported)
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use std::fs::File;

    /// The test directory with its path resolved, so kernel-reported paths compare equal.
    fn base(dir: &tempfile::TempDir) -> std::path::PathBuf {
        std::fs::canonicalize(dir.path()).unwrap()
    }

    fn held_by_us(holders: &[(u32, String)]) -> bool {
        holders.iter().any(|(pid, _)| *pid == std::process::id())
    }

    #[test]
    fn an_unopened_file_has_no_holders() {
        let d = tempfile::tempdir().unwrap();
        let f = base(&d).join("free.txt");
        std::fs::write(&f, b"x").unwrap();
        let holders = file_locking_processes(&f).unwrap();
        assert!(holders.is_empty(), "{holders:?}");
    }

    #[test]
    fn an_open_file_is_held_by_this_process() {
        let d = tempfile::tempdir().unwrap();
        let f = base(&d).join("held.txt");
        let _held = File::create(&f).unwrap();
        let holders = file_locking_processes(&f).unwrap();
        assert!(held_by_us(&holders), "{holders:?}");
        assert!(holders.iter().all(|(pid, _)| *pid != 0));
    }

    #[test]
    fn a_renamed_open_file_is_found_by_its_device_and_inode() {
        let d = tempfile::tempdir().unwrap();
        let dir = base(&d);
        let held = File::create(dir.join("before.txt")).unwrap();
        std::fs::rename(dir.join("before.txt"), dir.join("after.txt")).unwrap();
        // The old path no longer exists; the new one has the same inode.
        let holders = file_locking_processes(&dir.join("after.txt")).unwrap();
        assert!(held_by_us(&holders), "{holders:?}");
        drop(held);
    }

    #[test]
    fn a_deleted_but_open_file_is_still_found() {
        let d = tempfile::tempdir().unwrap();
        let f = base(&d).join("gone.txt");
        let _held = File::create(&f).unwrap();
        std::fs::remove_file(&f).unwrap();
        let holders = file_locking_processes(&f).unwrap();
        assert!(held_by_us(&holders), "{holders:?}");
    }

    #[test]
    fn processes_using_names_the_holders_inside_a_folder() {
        let d = tempfile::tempdir().unwrap();
        let dir = base(&d);
        let _held = File::create(dir.join("inside.txt")).unwrap();
        let names = processes_using(&dir, 10);
        let ours = std::fs::read_to_string("/proc/self/comm").unwrap();
        assert!(names.contains(&ours.trim_end().to_string()), "{names:?}");
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn unlocked_file_has_no_holders() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("free.txt");
        std::fs::write(&f, b"x").unwrap();
        let holders = file_locking_processes(&f).unwrap();
        assert!(holders.is_empty(), "{holders:?}");
    }

    #[test]
    fn open_handle_query_succeeds() {
        // Restart Manager reports processes, not handles, and the result depends on the
        // environment. This test checks the call completes and never reports pid 0.
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("held.txt");
        let _held = std::fs::File::create(&f).unwrap();
        let holders = file_locking_processes(&f).unwrap();
        assert!(holders.iter().all(|(pid, _)| *pid != 0));
    }
}
