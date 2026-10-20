//! Which processes hold a file open, via the Restart Manager API.
//!
//! Intended for "access denied / file in use" diagnostics. Restart Manager reports
//! applications that have registered or locked the resource; it may miss some holders
//! (e.g. kernel-level or services without a registered name).

use std::path::Path;

use crate::{Result, WinError};

/// Returns `(pid, application name)` for each process currently using `path`.
#[cfg(windows)]
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
    #[cfg(windows)]
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

#[cfg(not(windows))]
pub fn file_locking_processes(_path: &Path) -> Result<Vec<(u32, String)>> {
    Err(WinError::Unsupported)
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
