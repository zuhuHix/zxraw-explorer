//! Connecting to network shares. Windows asks for the user name and password in its own
//! dialog: this module never sees, asks for or stores a credential.

use std::path::{Path, PathBuf};

use crate::{Result, WinError};

/// What happened to a connection request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connect {
    /// The share is connected (or already was) and can be opened now.
    Connected,
    /// The user closed Windows' sign-in dialog.
    Cancelled,
}

/// ERROR_CANCELLED: the user dismissed the credential dialog.
const ERROR_CANCELLED: u32 = 1223;
/// ERROR_SESSION_CREDENTIAL_CONFLICT: a connection to this server under another user exists.
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

#[cfg(not(windows))]
pub fn connect_share(_owner: isize, _remote: &Path) -> Result<Connect> {
    Err(WinError::Unsupported)
}

/// A user-facing reason for a failed connection.
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
