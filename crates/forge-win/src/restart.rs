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
        let wide = crate::to_wide(path);
        let files = [PCWSTR(wide.as_ptr())];
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
