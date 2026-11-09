//! Connecting to network shares. Windows asks for the user name and password in its own
//! dialog: this module never sees, asks for or stores a credential.

use std::path::{Path, PathBuf};

use crate::Result;
#[cfg(not(target_os = "linux"))]
use crate::WinError;

/// What happened to a connection request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connect {
    /// The share is connected (or already was) and can be opened now.
    Connected,
    /// The user closed Windows' sign-in dialog.
    Cancelled,
}

/// ERROR_CANCELLED: the user dismissed the credential dialog.
#[cfg_attr(not(windows), allow(dead_code))]
const ERROR_CANCELLED: u32 = 1223;
/// ERROR_SESSION_CREDENTIAL_CONFLICT: a connection to this server under another user exists.
#[cfg_attr(not(windows), allow(dead_code))]
const ERROR_SESSION_CREDENTIAL_CONFLICT: u32 = 1219;

/// Connects to the share `remote` (`\\server\share`, deviceless, no drive letter), letting
/// Windows prompt for credentials with `owner` (an HWND, 0 for none) as the dialog's owner.
///
/// Blocks until the user answers and the server responds, which can take tens of seconds on
/// a slow network: call it from a worker thread.
#[cfg(windows)]
pub fn connect_share(owner: isize, remote: &Path) -> Result<Connect> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::NetworkManagement::WNet::{
        CONNECT_INTERACTIVE, CONNECT_PROMPT, NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection3W,
    };
    use windows::core::{PCWSTR, PWSTR};

    let mut name: Vec<u16> = remote
        .to_string_lossy()
        .trim_end_matches('\\')
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let resource = NETRESOURCEW {
        dwType: RESOURCETYPE_DISK,
        lpRemoteName: PWSTR(name.as_mut_ptr()),
        ..Default::default()
    };
    let owner = (owner != 0).then_some(HWND(owner as *mut c_void));
    // SAFETY: `resource` points at `name`, which is NUL-terminated and outlives the call.
    // Null user name and password let Windows use saved credentials or prompt.
    let status = unsafe {
        WNetAddConnection3W(
            owner,
            &resource,
            PCWSTR::null(),
            PCWSTR::null(),
            CONNECT_INTERACTIVE | CONNECT_PROMPT,
        )
    };
    match status.0 {
        0 => Ok(Connect::Connected),
        ERROR_CANCELLED => Ok(Connect::Cancelled),
        code => Err(WinError::Api {
            op: "WNetAddConnection3W",
            code,
            message: connect_message(code),
        }),
    }
}

/// On Linux the share is mounted with `gio mount` (GVfs), with no owner window and no prompt:
/// `remote` may be `smb://host/share`, `//host/share` or `\\host\share`. The server must accept
/// the session without a password, since nothing here can ask for one; otherwise the error says
/// so. The share then appears under `$XDG_RUNTIME_DIR/gvfs/`, but this function does not return
/// that path.
#[cfg(target_os = "linux")]
pub fn connect_share(_owner: isize, remote: &Path) -> Result<Connect> {
    linux::connect(remote)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn connect_share(_owner: isize, _remote: &Path) -> Result<Connect> {
    Err(WinError::Unsupported)
}

/// A user-facing reason for a failed connection.
#[cfg_attr(not(windows), allow(dead_code))]
fn connect_message(code: u32) -> String {
    match code {
        ERROR_SESSION_CREDENTIAL_CONFLICT => "Windows is already connected to this server as \
            another user. Disconnect the other connection (for example in File Explorer) and \
            try again."
            .into(),
        53 | 67 | 1231 | 1232 => "The server or share could not be reached.".into(),
        1326 | 86 => "The user name or password was not accepted.".into(),
        _ => std::io::Error::from_raw_os_error(code as i32).to_string(),
    }
}

/// The share a mapped drive letter points at (`Z` → `\\nas\photos`), from Windows' own table
/// of connections. Does not contact the server; `None` when `letter` is not a network drive.
#[cfg(windows)]
pub fn mapped_share(letter: char) -> Option<PathBuf> {
    use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    use windows::core::{PCWSTR, PWSTR};

    let local: Vec<u16> = format!("{letter}:").encode_utf16().chain(Some(0)).collect();
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: both buffers are valid for the given lengths; `local` is NUL-terminated.
    let status = unsafe {
        WNetGetConnectionW(PCWSTR(local.as_ptr()), Some(PWSTR(buf.as_mut_ptr())), &mut len)
    };
    // ERROR_CONNECTION_UNAVAIL (1201): a remembered drive that is not connected right now;
    // the name is still filled in.
    if status.0 != 0 && status.0 != 1201 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    (end > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..end])))
}

#[cfg(not(windows))]
pub fn mapped_share(_letter: char) -> Option<PathBuf> {
    None
}

/// Linux: shares are mounted with `gio mount` (GVfs). There are no drive letters, so
/// [`mapped_share`] is always `None` here.
#[cfg(target_os = "linux")]
pub(crate) mod linux {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::Connect;
    use crate::{Result, WinError};

    /// How long `gio mount` may take before it is killed.
    const MOUNT_TIMEOUT: Duration = Duration::from_secs(60);
    const NEEDS_LOGIN: &str = "The share needs a user name and password, which ZXraw cannot ask \
        for on Linux. Connect to it once from the file manager (Network) and try again.";

    /// A share on a server: `smb://host/name`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Share {
        pub(crate) host: String,
        pub(crate) name: String,
    }

    /// The server and share of `text`, which is `smb://host/share[/…]`, `//host/share[/…]` or
    /// `\\host\share[\…]`. Anything after the share is ignored. A user name, a port or an empty
    /// part is rejected.
    pub(crate) fn parse_share(text: &str) -> std::result::Result<Share, String> {
        let rest = if let Some(rest) = text
            .get(..6)
            .filter(|scheme| scheme.eq_ignore_ascii_case("smb://"))
            .map(|_| &text[6..])
        {
            rest
        } else if let Some(rest) = text.strip_prefix("//").or_else(|| text.strip_prefix(r"\\")) {
            rest
        } else {
            return Err(format!(
                "not a network share: {text} (use smb://host/share or \\\\host\\share)"
            ));
        };
        let mut parts = rest.split(['/', '\\']);
        let host = parts.next().unwrap_or_default();
        let name = parts.next().unwrap_or_default();
        if host.is_empty() || name.is_empty() {
            return Err(format!("no server and share in {text}"));
        }
        if host
            .chars()
            .any(|c| matches!(c, '@' | ':') || c.is_whitespace())
        {
            return Err(format!(
                "{host} has a user name or port, which is not supported here"
            ));
        }
        if name.chars().any(char::is_control) {
            return Err(format!("the share name in {text} has control characters"));
        }
        Ok(Share {
            host: host.to_string(),
            name: name.to_string(),
        })
    }

    /// Where GVfs shows a mounted share: `$XDG_RUNTIME_DIR/gvfs/smb-share:server=HOST,share=NAME`.
    pub(crate) fn gvfs_share_path(runtime_dir: &Path, share: &Share) -> PathBuf {
        runtime_dir.join("gvfs").join(format!(
            "smb-share:server={},share={}",
            share.host, share.name
        ))
    }

    /// Mounts the share `remote` with `gio mount`, waiting at most [`MOUNT_TIMEOUT`].
    pub(crate) fn connect(remote: &Path) -> Result<Connect> {
        let share = parse_share(&remote.to_string_lossy()).map_err(WinError::Invalid)?;
        let name = crate::shell::linux::percent_encode(share.name.as_bytes(), false);
        let url = format!("smb://{}/{name}", share.host);
        let mut child = Command::new("gio")
            .args(["mount", url.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| WinError::Io {
                op: "gio mount",
                path: remote.to_path_buf(),
                source,
            })?;

        // Read stderr on its own thread: a full pipe would block gio.
        let (tx, rx) = mpsc::channel();
        if let Some(mut stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text);
                let _ = tx.send(text);
            });
        }

        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < MOUNT_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(WinError::Api {
                        op: "gio mount",
                        code: 0,
                        message: format!(
                            "the server did not answer within {} s",
                            MOUNT_TIMEOUT.as_secs()
                        ),
                    });
                }
                Err(source) => {
                    return Err(WinError::Io {
                        op: "gio mount",
                        path: remote.to_path_buf(),
                        source,
                    });
                }
            }
        };
        // A grandchild may keep the pipe open, so do not wait for it for long.
        let stderr = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
        if status.success() {
            if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
                tracing::debug!(
                    "mounted {url} at {}",
                    gvfs_share_path(Path::new(&runtime), &share).display()
                );
            }
        }
        classify(status.success(), &stderr)
    }

    /// The outcome of a finished `gio mount`. A share that is already mounted counts as
    /// connected. Something that looks like a missing login (gio cannot prompt, since stdin is
    /// closed) becomes an error asking the user to connect from the file manager.
    pub(crate) fn classify(success: bool, stderr: &str) -> Result<Connect> {
        if success {
            return Ok(Connect::Connected);
        }
        let lower = stderr.to_lowercase();
        if lower.contains("already mounted") {
            return Ok(Connect::Connected);
        }
        const LOGIN_MARKERS: &[&str] = &[
            "anonymous",
            "password",
            "permission denied",
            "access denied",
            "logon failure",
            "authentication",
            "not authorized",
            "cancelled",
        ];
        let message = if LOGIN_MARKERS.iter().any(|marker| lower.contains(marker)) {
            NEEDS_LOGIN.to_string()
        } else {
            stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("gio could not mount the share")
                .to_string()
        };
        Err(WinError::Api {
            op: "gio mount",
            code: 0,
            message,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn share_urls_and_unc_paths_are_parsed() {
            let share = |host: &str, name: &str| Share {
                host: host.into(),
                name: name.into(),
            };
            assert_eq!(parse_share("smb://nas/photos"), Ok(share("nas", "photos")));
            assert_eq!(
                parse_share("SMB://nas/photos/2024/x"),
                Ok(share("nas", "photos"))
            );
            assert_eq!(parse_share("//nas/photos"), Ok(share("nas", "photos")));
            assert_eq!(parse_share(r"\\nas\photos\sub"), Ok(share("nas", "photos")));
            assert_eq!(
                parse_share("smb://nas/My Share/"),
                Ok(share("nas", "My Share"))
            );
            assert_eq!(
                parse_share("smb://192.168.1.5/C$"),
                Ok(share("192.168.1.5", "C$"))
            );
        }

        #[test]
        fn incomplete_or_foreign_paths_are_rejected() {
            for bad in [
                "smb://nas",
                "smb://",
                "//nas/",
                "smb:///photos",
                "http://nas/photos",
                "C:\\Users",
                "smb://user@nas/photos",
                "smb://nas:445/photos",
                "",
            ] {
                assert!(parse_share(bad).is_err(), "{bad} should be rejected");
            }
        }

        #[test]
        fn gvfs_path_names_server_and_share() {
            let share = Share {
                host: "nas".into(),
                name: "photos".into(),
            };
            assert_eq!(
                gvfs_share_path(Path::new("/run/user/1000"), &share),
                PathBuf::from("/run/user/1000/gvfs/smb-share:server=nas,share=photos")
            );
        }

        /// Runs `gio mount` against a name that cannot resolve. Needs gio on the machine.
        #[test]
        #[ignore = "needs gio and a desktop session"]
        fn connect_to_an_unknown_server_is_an_error() {
            let remote = Path::new("smb://zx-test-host.invalid/none");
            assert!(connect(remote).is_err());
        }

        #[test]
        fn mount_outcomes_are_classified() {
            assert!(matches!(classify(true, ""), Ok(Connect::Connected)));
            assert!(matches!(
                classify(false, "Error: Location is already mounted"),
                Ok(Connect::Connected)
            ));
            for login in [
                "Error mounting: Anonymous access denied",
                "Failed to mount: Operation was cancelled",
                "Logon failure: unknown user name or bad password",
            ] {
                match classify(false, login) {
                    Err(WinError::Api { message, .. }) => assert_eq!(message, NEEDS_LOGIN),
                    other => panic!("{login}: {other:?}"),
                }
            }
            match classify(false, "\n  Failed to resolve host nas\n") {
                Err(WinError::Api { message, .. }) => {
                    assert_eq!(message, "Failed to resolve host nas")
                }
                other => panic!("{other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_drive_letters_are_not_mapped_shares() {
        // A drive letter that cannot be mapped (no network drive is ever 0:).
        assert_eq!(mapped_share('0'), None);
    }

    #[test]
    fn failure_messages_are_plain() {
        assert!(connect_message(1219).contains("another user"));
        assert!(connect_message(1326).contains("not accepted"));
        assert!(!connect_message(5).is_empty());
    }
}
