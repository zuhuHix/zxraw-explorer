//! Drive enumeration.
//!
//! Each drive is queried on its own short-lived thread. A disconnected network share can
//! block `GetVolumeInformationW` or `GetDiskFreeSpaceExW` for a long time, so every drive
//! gets a deadline of [`QUERY_TIMEOUT`]; a drive that misses it is reported with
//! `available == false` and its query thread is left to finish (or hang) on its own.

use std::path::PathBuf;
use std::time::Duration;

/// Per-drive deadline for all of its queries.
pub const QUERY_TIMEOUT: Duration = Duration::from_millis(1500);

/// Drive type as reported by `GetDriveTypeW`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriveKind {
    Unknown,
    /// Path exists but no root directory is mounted (e.g. an empty card reader slot).
    NoRootDir,
    Removable,
    Fixed,
    Network,
    CdRom,
    RamDisk,
}

/// Information about one logical drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveInfo {
    /// Drive root, e.g. `C:\`.
    pub root: PathBuf,
    pub kind: DriveKind,
    /// Volume label; empty if none or unavailable.
    pub label: String,
    /// File system name such as `NTFS`, `exFAT`; empty if unavailable.
    pub fs_name: String,
    pub serial: Option<u32>,
    pub total_bytes: Option<u64>,
    /// Bytes available to the current user.
    pub free_bytes: Option<u64>,
    /// False if the drive timed out or was not ready. Such drives still appear in the list
    /// so the UI can show them as unavailable.
    pub available: bool,
}

/// Lists logical drives with their type, label, file system, serial and space.
///
/// Never blocks longer than roughly [`QUERY_TIMEOUT`] per drive, all drives in parallel.
/// Returns an empty list on non-Windows targets.
#[cfg(windows)]
pub fn list_drives() -> Vec<DriveInfo> {
    use std::sync::mpsc;
    use std::time::Instant;

    let roots = logical_drive_roots();
    let (tx, rx) = mpsc::channel::<(usize, DriveInfo)>();
    for (i, root) in roots.iter().enumerate() {
        let tx = tx.clone();
        let root = root.clone();
        std::thread::spawn(move || {
            let info = query_drive(&root);
            // The receiver may already have given up on us; that is fine.
            let _ = tx.send((i, info));
        });
    }
    drop(tx);

    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut results: Vec<Option<DriveInfo>> = vec![None; roots.len()];
    let mut received = 0;
    while received < roots.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok((i, info)) => {
                results[i] = Some(info);
                received += 1;
            }
            // Timed out, or every sender is gone without sending (cannot happen here).
            Err(_) => break,
        }
    }

    roots
        .into_iter()
        .zip(results)
        .map(|(root, res)| {
            res.unwrap_or_else(|| DriveInfo {
                root,
                kind: DriveKind::Unknown,
                label: String::new(),
                fs_name: String::new(),
                serial: None,
                total_bytes: None,
                free_bytes: None,
                available: false,
            })
        })
        .collect()
}

#[cfg(not(windows))]
pub fn list_drives() -> Vec<DriveInfo> {
    Vec::new()
}

/// Root paths of all logical drives (`C:\`, `D:\`, ...). Local, does not touch the drives.
#[cfg(windows)]
pub(crate) fn logical_drive_roots() -> Vec<PathBuf> {
    use windows::Win32::Storage::FileSystem::GetLogicalDriveStringsW;

    let mut buf = vec![0u16; 256];
    loop {
        // SAFETY: buf is valid for buf.len() UTF-16 units.
        let n = unsafe { GetLogicalDriveStringsW(Some(&mut buf)) } as usize;
        if n == 0 {
            return Vec::new();
        }
        if n < buf.len() {
            buf.truncate(n);
            break;
        }
        buf.resize(n + 1, 0);
    }
    buf.split(|c| *c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(String::from_utf16_lossy(s)))
        .collect()
}

/// Runs every query for one drive. Failures leave the corresponding field empty.
#[cfg(windows)]
fn query_drive(root: &PathBuf) -> DriveInfo {
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeInformationW,
    };
    use windows::core::PCWSTR;

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;
    const DRIVE_NO_ROOT_DIR: u32 = 1;

    let wide = crate::to_wide(root);
    let raw = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
    let kind = match raw {
        DRIVE_NO_ROOT_DIR => DriveKind::NoRootDir,
        DRIVE_REMOVABLE => DriveKind::Removable,
        DRIVE_FIXED => DriveKind::Fixed,
        DRIVE_REMOTE => DriveKind::Network,
        DRIVE_CDROM => DriveKind::CdRom,
        DRIVE_RAMDISK => DriveKind::RamDisk,
        _ => DriveKind::Unknown,
    };

    let mut label_buf = [0u16; 261];
    let mut fs_buf = [0u16; 261];
    let mut serial = 0u32;
    let vol = unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            Some(&mut label_buf),
            Some(&mut serial),
            None,
            None,
            Some(&mut fs_buf),
        )
    };
    let (label, fs_name, serial) = match vol {
        Ok(()) => (
            wide_to_string(&label_buf),
            wide_to_string(&fs_buf),
            Some(serial),
        ),
        // Empty optical drive, not-ready card reader, or a network share without info.
        Err(_) => (String::new(), String::new(), None),
    };

    let mut free_to_caller = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    let space = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free_to_caller),
            Some(&mut total),
            Some(&mut total_free),
        )
    };
    let (total_bytes, free_bytes) = match space {
        Ok(()) => (Some(total), Some(free_to_caller)),
        Err(_) => (None, None),
    };

    DriveInfo {
        root: root.clone(),
        kind,
        label,
        fs_name,
        serial,
        total_bytes,
        free_bytes,
        available: kind != DriveKind::NoRootDir,
    }
}

#[cfg(windows)]
fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn list_drives_contains_system_drive() {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let want = PathBuf::from(format!("{}\\", system.trim_end_matches('\\')));
        let drives = list_drives();
        let c = drives
            .iter()
            .find(|d| {
                d.root
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&want.to_string_lossy())
            })
            .unwrap_or_else(|| panic!("{} missing from {drives:?}", want.display()));
        assert!(c.available);
        assert!(c.total_bytes.is_some());
    }

    #[test]
    fn list_drives_returns_within_timeout_budget() {
        let start = std::time::Instant::now();
        let _ = list_drives();
        // All drives are queried in parallel; allow generous slack for slow machines.
        assert!(start.elapsed() < QUERY_TIMEOUT * 4);
    }
}
