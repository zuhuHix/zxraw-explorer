//! Thin adapter between the UI and the operating system.
//!
//! Opening, revealing and launching go to `forge-win` everywhere (on Linux: `xdg-open`, the
//! XDG portal and the `org.freedesktop.FileManager1` interface). Drives and known folders come
//! from `forge-win` on Windows; on Linux from the XDG user directories and the mounted file
//! systems under `/media`, `/run/media` and `/mnt` (removable drives).

use std::path::{Path, PathBuf};

use crate::commands::Cmd;
use crate::sidebar::Drive;


/// Whether a command exists on this system. Windows-only: the administrator terminal, and the
/// FFmpeg download (a pinned, checksummed Windows build; Linux has no comparably stable
/// static build to pin, so ffmpeg comes from the package manager there). Commands that aren't
/// available are left out of the menus, the command palette and the shortcut list, and their
/// shortcuts do nothing. Properties, Open with, network locations and the update check have
/// Linux backends in `forge-win`.
pub fn command_available(cmd: Cmd) -> bool {
    match cmd {
        Cmd::OpenTerminalAdmin | Cmd::DownloadFfmpeg => cfg!(windows),
        _ => true,
    }
}

/// Where deleted items go: the Recycle Bin on Windows, the Trash (freedesktop) elsewhere.
pub const TRASH: &str = if cfg!(windows) { "Recycle Bin" } else { "Trash" };

/// What tells an item from another one later created under the same name: on Windows the
/// volume serial and file index (`forge_win::file_id`); on Unix `st_dev`, `st_ino` and the
/// birth time, since an inode number is reused at once after a delete (`forge_win::file_id`
/// alone would take a re-created file for the old one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileKey {
    pub volume: u64,
    pub index: u64,
    /// Unix: birth time in nanoseconds since 1970; 0 on Windows.
    pub born: u64,
}

impl FileKey {
    /// A key from its volume and index alone (tests, and Windows file IDs).
    #[cfg(any(windows, test))]
    pub const fn new(volume: u64, index: u64) -> FileKey {
        FileKey {
            volume,
            index,
            born: 0,
        }
    }
}

/// The [`FileKey`] of the item at `path`, not following a link at the end. `None` when it
/// can't be read, or (Unix) the file system keeps no birth time.
#[cfg(windows)]
pub fn file_key(path: &Path) -> Option<FileKey> {
    forge_win::file_id(path)
        .ok()
        .map(|(volume, index)| FileKey::new(u64::from(volume), index))
}

/// See the Windows version.
#[cfg(unix)]
pub fn file_key(path: &Path) -> Option<FileKey> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::symlink_metadata(path).ok()?;
    let born = md.created().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(FileKey {
        volume: md.dev(),
        index: md.ino(),
        born: born.as_nanos() as u64,
    })
}

#[cfg(not(any(windows, unix)))]
pub fn file_key(_path: &Path) -> Option<FileKey> {
    None
}

/// The user's home folder.
pub fn home_dir() -> PathBuf {
    forge_core::paths::home_dir()
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" }))
}

#[cfg(windows)]
pub fn drives() -> Vec<Drive> {
    windows_drives()
}

#[cfg(not(windows))]
pub fn drives() -> Vec<Drive> {
    unix_drives()
}

#[cfg(windows)]
pub fn known_folders() -> Vec<(String, PathBuf)> {
    forge_win::known_folders()
}

#[cfg(not(windows))]
pub fn known_folders() -> Vec<(String, PathBuf)> {
    unix_known_folders()
}

/// Opens `path` with its default application (`xdg-open` on Linux).
pub fn open_default(path: &Path) -> Result<(), forge_win::WinError> {
    forge_win::open_default(path)
}

/// Opens the folder that holds `path` with `path` selected in it (Explorer's "Show in
/// folder"; on Linux the file manager's `ShowItems`, else the containing folder opens).
pub fn reveal(path: &Path) -> Result<(), forge_win::WinError> {
    forge_win::reveal_in_explorer(path)
}

/// Opens a folder in the file manager.
pub fn open_folder(path: &Path) -> Result<(), forge_win::WinError> {
    forge_win::open_folder_in_explorer(path)
}

/// Starts a program with separate arguments (no shell) in `cwd`, without waiting for it.
pub fn launch(
    exe: &Path,
    args: &[std::ffi::OsString],
    cwd: Option<&Path>,
) -> Result<(), forge_win::WinError> {
    forge_win::launch(exe, args, cwd)
}

#[cfg(windows)]
fn windows_drives() -> Vec<Drive> {
    use forge_win::DriveKind;
    forge_win::list_drives()
        .into_iter()
        .filter(|d| d.kind != DriveKind::NoRootDir)
        .map(|d| Drive {
            remote: (d.kind == DriveKind::Network)
                .then(|| d.root.to_string_lossy().chars().next())
                .flatten()
                .and_then(forge_win::network::mapped_share),
            kind: match d.kind {
                DriveKind::Removable => "Removable",
                DriveKind::Network => "Network",
                DriveKind::CdRom => "Optical",
                DriveKind::RamDisk => "RAM disk",
                _ => "Local Disk",
            },
            root: d.root,
            label: d.label,
            total: d.total_bytes.unwrap_or(0),
            free: d.free_bytes.unwrap_or(0),
            available: d.available,
        })
        .collect()
}

/// `/` plus the file systems mounted under the usual removable-media folders. Media mounted
/// under `/media` or `/run/media` count as removable (so an inserted card can be offered for
/// import); other mounts under `/mnt` are local disks.
#[cfg(not(windows))]
fn unix_drives() -> Vec<Drive> {
    let mut drives = vec![unix_drive(Path::new("/"), "", "Local Disk")];
    for mount in mounted_points() {
        let kind = if mount.starts_with("/media") || mount.starts_with("/run/media") {
            "Removable"
        } else {
            "Local Disk"
        };
        let label = mount
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        drives.push(unix_drive(&mount, &label, kind));
    }
    drives
}

#[cfg(not(windows))]
fn unix_drive(root: &Path, label: &str, kind: &'static str) -> Drive {
    let stats = volume_space(root);
    let (total, free) = stats.unwrap_or((0, 0));
    Drive {
        root: root.to_path_buf(),
        label: label.to_string(),
        kind,
        total,
        free,
        available: stats.is_some(),
        remote: None,
    }
}

/// Total and free bytes of the file system holding `root`, from `statvfs`.
#[cfg(unix)]
fn volume_space(root: &Path) -> Option<(u64, u64)> {
    rustix::fs::statvfs(root)
        .ok()
        .map(|s| (s.f_blocks * s.f_frsize, s.f_bavail * s.f_frsize))
}

#[cfg(not(any(unix, windows)))]
fn volume_space(_root: &Path) -> Option<(u64, u64)> {
    None
}

/// The mount points below `/media`, `/run/media` and `/mnt` (the folders themselves and
/// their per-user parents excluded), read from `/proc/self/mounts`.
#[cfg(not(windows))]
fn mounted_points() -> Vec<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    text.lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(unescape_mount)
        .filter(|mount| is_removable_mount_point(mount))
        .collect()
}

/// `/proc/mounts` writes space, tab, newline and backslash as octal escapes (`\040`).
#[cfg(not(windows))]
fn unescape_mount(field: &str) -> PathBuf {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4);
        match (bytes[i], octal) {
            (b'\\', Some(digits)) if digits.iter().all(|d| (b'0'..=b'7').contains(d)) => {
                let value = digits
                    .iter()
                    .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
                out.push(value as u8);
                i += 4;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

/// Whether `mount` is a folder strictly inside one of the removable-media or `/mnt` trees.
#[cfg(not(windows))]
fn is_removable_mount_point(mount: &Path) -> bool {
    ["/media", "/run/media", "/mnt"].iter().any(|tree| {
        mount
            .strip_prefix(tree)
            .is_ok_and(|rest| rest.components().next().is_some())
    })
}

/// The XDG user folders (Desktop, Downloads, …) plus the home folder, as `(label, path)`.
/// Folders that do not exist are left out, and so is a folder equal to home (Desktop often
/// falls back to home when it is not set up).
#[cfg(not(windows))]
fn unix_known_folders() -> Vec<(String, PathBuf)> {
    let home = home_dir();
    let Some(dirs) = directories::UserDirs::new() else {
        return vec![("Home".to_string(), home)];
    };
    let mut out = Vec::new();
    let named = [
        ("Desktop", dirs.desktop_dir()),
        ("Downloads", dirs.download_dir()),
        ("Documents", dirs.document_dir()),
        ("Pictures", dirs.picture_dir()),
        ("Videos", dirs.video_dir()),
        ("Music", dirs.audio_dir()),
    ];
    for (label, path) in named {
        if let Some(path) = path.filter(|p| p.is_dir() && *p != home.as_path()) {
            out.push((label.to_string(), path.to_path_buf()));
        }
    }
    out.push(("Home".to_string(), home));
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn windows_only_commands_are_hidden() {
        for cmd in [Cmd::OpenTerminalAdmin, Cmd::DownloadFfmpeg] {
            assert!(!command_available(cmd), "{cmd:?}");
        }
        for cmd in [
            Cmd::Properties,
            Cmd::OpenWith,
            Cmd::AddToNetwork,
            Cmd::CheckForUpdates,
            Cmd::OpenTerminal,
            Cmd::Recycle,
            Cmd::Copy,
            Cmd::Paste,
        ] {
            assert!(command_available(cmd), "{cmd:?}");
        }
        let shown = Cmd::ALL.iter().filter(|c| command_available(**c)).count();
        assert_eq!(shown, Cmd::ALL.len() - 2);
        assert_eq!(Cmd::RevealInExplorer.label(), "Show in file manager");
    }

    #[test]
    fn mount_escapes_are_decoded() {
        assert_eq!(
            unescape_mount(r"/run/media/zuhu/My\040Stick"),
            PathBuf::from("/run/media/zuhu/My Stick")
        );
        assert_eq!(unescape_mount(r"/mnt/a\134b"), PathBuf::from(r"/mnt/a\b"));
        assert_eq!(unescape_mount("/mnt/plain"), PathBuf::from("/mnt/plain"));
    }

    #[test]
    fn only_folders_below_the_media_trees_count_as_removable_mounts() {
        assert!(is_removable_mount_point(Path::new("/media/zuhu/CARD")));
        assert!(is_removable_mount_point(Path::new("/run/media/zuhu/CARD")));
        assert!(is_removable_mount_point(Path::new("/mnt/data")));
        assert!(!is_removable_mount_point(Path::new("/media")));
        assert!(!is_removable_mount_point(Path::new("/home/zuhu")));
        assert!(!is_removable_mount_point(Path::new("/mntx/data")));
    }

    #[test]
    fn root_drive_reports_its_size() {
        let drives = unix_drives();
        let root = drives
            .iter()
            .find(|d| d.root == Path::new("/"))
            .expect("the root file system is always listed");
        assert!(root.available);
        assert!(root.total > 0 && root.free <= root.total);
        assert_eq!(root.kind, "Local Disk");
        assert_eq!(root.label, "");
    }

    #[test]
    fn home_is_always_a_known_folder() {
        let folders = unix_known_folders();
        assert!(
            folders
                .iter()
                .any(|(label, path)| label == "Home" && *path == home_dir())
        );
    }

    #[test]
    fn installed_ffmpeg_is_found_on_path_without_an_exe_suffix() {
        // Only meaningful where ffmpeg is installed; skipped otherwise.
        let on_path = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .any(|dir| dir.join("ffmpeg").is_file());
        if !on_path {
            return;
        }
        let tools = forge_media::video::FfTools::discover();
        assert!(tools.has_ffmpeg(), "ffmpeg is on PATH but was not found");
        assert!(tools.has_ffprobe(), "ffprobe is on PATH but was not found");
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        let missing = launch(Path::new("/nonexistent/zxraw-no-such-program"), &[], None);
        assert!(missing.is_err());
    }
}
