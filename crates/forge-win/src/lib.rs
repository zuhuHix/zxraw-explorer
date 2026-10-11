//! Windows shell integration for ZXraw Explorer.
//!
//! Every public function compiles on every platform. On non-Windows targets the
//! fallible functions return [`WinError::Unsupported`] and the infallible ones return an
//! empty result, so the rest of the workspace can call them unconditionally.
//!
//! * [`shell`] — Recycle Bin, ShellExecute open / Open-with / Properties, Explorer reveal,
//!   known folders.
//! * [`drives`] — drive enumeration with per-drive timeouts, [`DriveWatcher`] polling.
//! * [`files`] — attributes, hard-link count, volume serial, removable check.
//! * [`restart`] — which processes hold a file open (Restart Manager).
//! * [`process`] — launching external tools without a shell.

use std::path::PathBuf;

use thiserror::Error;

mod clipboard;
mod integration;
pub mod drives;
pub mod files;
pub mod process;
pub mod restart;
pub mod shell;
pub mod watch;

pub use clipboard::{
    ClipboardEffect, clear_if_owned_files, clipboard_sequence, get_files, set_files,
};
pub use drives::{DriveInfo, DriveKind, list_drives};
pub use files::{hard_link_count, is_removable, set_attributes, volume_serial};
pub use integration::{
    is_registered, refresh_if_registered, register_context_menu, unregister_context_menu,
};
pub use process::launch;
pub use restart::file_locking_processes;
pub use shell::{
    known_folders, open_default, open_with_dialog, recycle, reveal_in_explorer, show_properties,
};
pub use watch::{DriveEvent, DriveWatcher, drive_change_watcher};

/// Errors from Windows shell calls.
#[derive(Debug, Error)]
pub enum WinError {
    /// The operation only exists on Windows.
    #[error("not supported on this platform")]
    Unsupported,
    /// A Win32 / COM call failed. `code` is the HRESULT or Win32 error code.
    #[error("{op} failed (0x{code:08X}): {message}")]
    Api {
        op: &'static str,
        code: u32,
        message: String,
    },
    /// A filesystem or process operation failed for a specific path.
    #[error("{op} failed for {}: {source}", path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The shell reported that the user or system aborted part of the operation.
    #[error("{op}: the operation was aborted or only partly completed")]
    Aborted { op: &'static str },
    /// The argument is not usable for this operation.
    #[error("invalid argument: {0}")]
    Invalid(String),
}

pub type Result<T, E = WinError> = std::result::Result<T, E>;

/// Encodes a path as NUL-terminated UTF-16 for Win32 calls.
#[cfg(windows)]
pub(crate) fn to_wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// Converts a Win32 error into [`WinError::Api`].
#[cfg(windows)]
pub(crate) fn api_err(op: &'static str, e: windows::core::Error) -> WinError {
    WinError::Api {
        op,
        code: e.code().0 as u32,
        message: e.message(),
    }
}

/// Last OS error for `path`, as [`WinError::Io`].
#[cfg(windows)]
pub(crate) fn last_io_err(op: &'static str, path: &std::path::Path) -> WinError {
    WinError::Io {
        op,
        path: path.to_path_buf(),
        source: std::io::Error::last_os_error(),
    }
}
