//! The Unix half of [`super`]: `openat` with `O_NOFOLLOW` relative to held folders,
//! `statx`/`fstatat(AT_SYMLINK_NOFOLLOW)` lookups, mount points told by `st_dev`, the mount id
//! and `STATX_ATTR_MOUNT_ROOT` (so a bind mount of the same device counts too), and `fstatfs`
//! for network file systems.

use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path};
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

#[cfg(test)]
use super::PATH_OPENS;
use super::{
    ATTR_DIRECTORY, ATTR_LINK, DIR_SERIAL, Dir, Entry, Error, File, Fs, Handle, Info, PLACEHOLDER,
    entry, plain_component,
};

/// `FILE_ATTRIBUTE_DEVICE`: on Unix, a FIFO, socket or device node. Never opened (opening a
/// FIFO blocks, a device can do anything).
pub const ATTR_SPECIAL: u32 = 0x40;
/// `FILE_ATTRIBUTE_NORMAL`: a regular file.
const ATTR_NORMAL: u32 = 0x80;

/// The stand-in for a drive letter: Unix has one tree, rooted at `/`.
pub const ROOT: char = '/';

/// `Some(ROOT)` for an absolute path. A path on a network file system is refused later, when
/// the folder on it is opened (see [`is_remote_fs`]); nothing is opened here.
pub fn local_drive(path: &Path) -> Option<char> {
    matches!(path.components().next(), Some(Component::RootDir)).then_some(ROOT)
}

/// The root folder [`local_drive`] stands for.
pub fn volume_root(_letter: char) -> std::path::PathBuf {
    std::path::PathBuf::from("/")
}

/// `statfs` magic numbers of network and FUSE file systems: reading them can be slow, hang on
/// a dead server, or (FUSE) run arbitrary code behind every open.
const REMOTE_FS: &[u32] = &[
    0x6969,      // NFS
    0x517B,      // SMB
    0xFF53_4D42, // CIFS
    0xFE53_4D42, // SMB2
    0x0102_1997, // 9P
    0x6573_5546, // FUSE (sshfs, rclone, gvfs, ...)
    0x5346_414F, // AFS
    0x7375_7245, // CODA
    0x00C3_6400, // CEPH
    0x564C,      // NCP
    0x0BD0_0BD0, // Lustre
];

/// Is the file system of `fd` a network (or FUSE) one?
fn is_remote_fs(fd: &OwnedFd) -> Result<bool, Error> {
    let stat = rustix::fs::fstatfs(fd).map_err(io)?;
    Ok(is_remote_magic(stat.f_type as i64 as u64))
}

/// Is `magic`, a `statfs` `f_type`, one of [`REMOTE_FS`]?
pub fn is_remote_magic(magic: u64) -> bool {
    REMOTE_FS.contains(&((magic & 0xFFFF_FFFF) as u32))
}

fn io(e: Errno) -> Error {
    match e {
        Errno::NOENT => Error::NotFound,
        Errno::NOTDIR | Errno::ISDIR => Error::WrongKind,
        // O_NOFOLLOW on a symlink.
        Errno::LOOP => Error::Link,
        Errno::NAMETOOLONG | Errno::INVAL | Errno::ILSEQ => Error::BadName,
        e => Error::Io(std::io::Error::from(e).to_string()),
    }
}

/// What the scan needs to know of an entry or open handle.
struct Meta {
    info: Info,
    dev: u64,
    nlink: u64,
    /// `stx_mnt_id`, where the kernel reports it.
    mount: Option<u64>,
    /// `STATX_ATTR_MOUNT_ROOT`: something is mounted here (a bind mount included).
    mount_root: bool,
    /// Did the kernel report both the mount id and `STATX_ATTR_MOUNT_ROOT`? Without them a
    /// bind mount of the same device can't be told from a plain folder.
    mount_known: bool,
}

impl Meta {
    /// Is this entry of folder `dev`/`mount` the root of another mount? A folder whose mount
    /// can't be established (no `statx`, or one that reports neither the mount id nor
    /// `STATX_ATTR_MOUNT_ROOT`) counts as one: fail closed, so the scan inside a repository
    /// enters no folder it can't vouch for. On such old kernels badges are then missing, never
    /// read through a bind mount.
    fn crosses(&self, dev: u64, mount: Option<u64>) -> bool {
        self.dev != dev
            || self.mount_root
            || matches!((self.mount, mount), (Some(a), Some(b)) if a != b)
            || (!self.mount_known && self.info.is_dir())
    }
}

#[cfg(test)]
thread_local! {
    /// Tests: [`meta`] behaves as without `statx` (the `fstatat` fallback) on this thread.
    pub static NO_STATX: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Synthetic Win32-style attributes, size and write time (ns since 1970).
fn info_from(mode: u32, size: u64, secs: i64, nanos: i64) -> Info {
    let attributes = match FileType::from_raw_mode(mode as _) {
        FileType::Directory => ATTR_DIRECTORY,
        FileType::Symlink => ATTR_LINK,
        FileType::RegularFile => ATTR_NORMAL,
        _ => ATTR_SPECIAL,
    };
    Info {
        attributes,
        size,
        mtime: secs.saturating_mul(1_000_000_000).saturating_add(nanos),
    }
}

/// `name` in `dirfd` (or `dirfd` itself, with `AtFlags::EMPTY_PATH` and an empty name), never
/// following a link: `statx` where there is one, else `fstatat` without mount information.
fn meta<P: rustix::path::Arg + Copy>(
    dirfd: impl AsFd,
    name: P,
    flags: AtFlags,
) -> Result<Meta, Errno> {
    let flags = flags | AtFlags::SYMLINK_NOFOLLOW;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use rustix::fs::{StatxAttributes, StatxFlags};
        // Tests can force the fallback below (see `NO_STATX`).
        #[cfg(test)]
        let skip = NO_STATX.with(std::cell::Cell::get);
        #[cfg(not(test))]
        let skip = false;
        let got = if skip {
            Err(Errno::NOSYS)
        } else {
            rustix::fs::statx(
                &dirfd,
                name,
                flags,
                StatxFlags::BASIC_STATS | StatxFlags::MNT_ID,
            )
        };
        match got {
            Ok(x) => {
                let root = StatxAttributes::MOUNT_ROOT;
                let has_mnt_id = x.stx_mask & StatxFlags::MNT_ID.bits() != 0;
                return Ok(Meta {
                    info: info_from(
                        u32::from(x.stx_mode),
                        x.stx_size,
                        x.stx_mtime.tv_sec,
                        i64::from(x.stx_mtime.tv_nsec),
                    ),
                    dev: rustix::fs::makedev(x.stx_dev_major, x.stx_dev_minor),
                    nlink: u64::from(x.stx_nlink),
                    mount: has_mnt_id.then_some(x.stx_mnt_id),
                    mount_root: x.stx_attributes_mask.contains(root)
                        && x.stx_attributes.contains(root),
                    mount_known: has_mnt_id && x.stx_attributes_mask.contains(root),
                });
            }
            Err(Errno::NOSYS) => {}
            Err(e) => return Err(e),
        }
    }
    let stat = rustix::fs::statat(&dirfd, name, flags)?;
    Ok(Meta {
        info: info_from(
            stat.st_mode as u32,
            stat.st_size.max(0) as u64,
            stat.st_mtime as i64,
            stat.st_mtime_nsec as i64,
        ),
        dev: stat.st_dev as u64,
        nlink: stat.st_nlink as u64,
        mount: None,
        mount_root: false,
        mount_known: false,
    })
}

/// [`meta`] of an open handle.
fn meta_fd(fd: &OwnedFd) -> Result<Meta, Errno> {
    meta(fd, "", AtFlags::EMPTY_PATH)
}

/// Opens folder `name` in `dirfd`, refusing a symlink. Reads need `r` on the folder; one with
/// only `x` (allowed on the way down to a repository) is opened as `O_PATH`, which can still
/// look names up and open them, but not be listed.
fn open_dir(dirfd: impl AsFd, name: &str) -> Result<OwnedFd, Error> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match rustix::fs::openat(&dirfd, name, flags, Mode::empty()) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Err(Errno::ACCESS) => rustix::fs::openat(
            &dirfd,
            name,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io),
        other => other.map_err(io),
    }
}

impl Dir {
    fn new(handle: Handle, meta: &Meta) -> Dir {
        Dir {
            handle,
            info: meta.info,
            serial: DIR_SERIAL.fetch_add(1, Ordering::Relaxed),
            dev: meta.dev,
            mount: meta.mount,
        }
    }
}

impl Fs {
    /// Opens `/` (the only open by path). With `record`, every opened handle's final path is
    /// kept (see [`Fs::opened`]).
    pub fn open_volume(_letter: char, record: bool) -> Result<(Fs, Dir), Error> {
        #[cfg(test)]
        PATH_OPENS.with(|n| n.set(n.get() + 1));
        let handle = open_dir(rustix::fs::CWD, "/")?;
        let meta = meta_fd(&handle).map_err(io)?;
        if !meta.info.is_dir() {
            return Err(Error::WrongKind);
        }
        if is_remote_fs(&handle)? {
            return Err(Error::Link);
        }
        let fs = Fs {
            volume: meta.dev,
            narrowed: false,
            mount: None,
            root_final: String::new(),
            opened: record.then(|| Mutex::new(Vec::new())),
        };
        fs.note("/");
        Ok((fs, Dir::new(handle, &meta)))
    }

    /// Makes `dir`, a folder opened through this `Fs`, the root nothing may leave from now on:
    /// no handle may be on another device or mount (mount points, bind mounts included,
    /// become links) or outside its path.
    pub fn narrow(&mut self, dir: &Dir) -> Result<(), Error> {
        if let Some(path) = final_path(&dir.handle) {
            self.root_final = path.trim_end_matches('/').to_string();
        }
        self.volume = dir.dev;
        self.mount = dir.mount;
        self.narrowed = true;
        Ok(())
    }

    /// Opens `entry`, listed in `parent`, without following links. Refused before opening:
    /// an entry from another folder's listing, a link, a placeholder, a FIFO or device, the
    /// wrong kind. Checked on the handle after: the expected kind, on the root's device and
    /// mount and not a mount point (once narrowed), not on a network file system, inside the
    /// root, one name (files).
    fn open(&self, parent: &Dir, entry: &Entry, dir: bool) -> Result<(OwnedFd, Meta), Error> {
        let name = entry.name.as_str();
        if entry.parent != parent.serial || !plain_component(name) {
            return Err(Error::BadName);
        }
        if entry.info.is_link() {
            return Err(Error::Link);
        }
        if entry.is_placeholder() {
            return Err(Error::OnlineOnly);
        }
        if entry.info.is_dir() != dir || entry.info.attributes & ATTR_SPECIAL != 0 {
            return Err(Error::WrongKind);
        }
        let handle = if dir {
            open_dir(&parent.handle, name)?
        } else {
            // NONBLOCK: should the name have become a FIFO since the listing, the open does
            // not wait for a writer (and the handle is then refused below).
            rustix::fs::openat(
                &parent.handle,
                name,
                OFlags::RDONLY
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::NOCTTY
                    | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io)?
        };
        let path = final_path(&handle);
        if let Some(path) = &path {
            self.note(path);
        }
        let meta = meta_fd(&handle).map_err(io)?;
        let info = meta.info;
        if info.is_link() {
            return Err(Error::Link);
        }
        if info.is_dir() != dir || (!dir && info.attributes & ATTR_SPECIAL != 0) {
            return Err(Error::WrongKind);
        }
        if self.narrowed {
            if meta.crosses(self.volume, self.mount) {
                return Err(Error::Link);
            }
        } else if meta.crosses(parent.dev, parent.mount) && is_remote_fs(&handle)? {
            return Err(Error::Link);
        }
        if !dir && meta.nlink > 1 {
            return Err(Error::Link);
        }
        if info.attributes & PLACEHOLDER != 0 {
            return Err(Error::OnlineOnly);
        }
        // Without /proc there is no final path; the walk opens only single names relative to
        // held folders, so it cannot leave the root anyway.
        if let Some(path) = path
            && !path
                .strip_prefix(&self.root_final)
                .is_some_and(|rest| rest.starts_with('/'))
        {
            return Err(Error::Link);
        }
        if !dir {
            // Reads should block normally.
            let _ = rustix::fs::fcntl_setfl(&handle, OFlags::empty());
        }
        Ok((handle, meta))
    }

    /// The entry `name` in folder `parent`, from `fstatat` without following a link; nothing
    /// is opened. `name` must be one plain component.
    pub fn lookup(&self, parent: &Dir, name: &str) -> Result<Entry, Error> {
        if !plain_component(name) {
            return Err(Error::BadName);
        }
        let found = meta(&parent.handle, name, AtFlags::empty()).map_err(io)?;
        let mut info = found.info;
        if self.narrowed && found.crosses(parent.dev, parent.mount) {
            info.attributes |= ATTR_LINK;
        }
        Ok(entry(name.to_string(), info, parent.serial))
    }

    /// Opens folder `entry`, listed in `parent`.
    pub fn dir(&self, parent: &Dir, entry: &Entry) -> Result<Dir, Error> {
        let (handle, meta) = self.open(parent, entry, true)?;
        Ok(Dir::new(handle, &meta))
    }

    /// Opens file `entry`, listed in `parent`.
    pub fn file(&self, parent: &Dir, entry: &Entry) -> Result<File, Error> {
        let (handle, meta) = self.open(parent, entry, false)?;
        Ok(File {
            file: std::fs::File::from(handle),
            info: meta.info,
        })
    }
}

impl Dir {
    /// Lists the folder, calling `each` per entry (`.` and `..` skipped) until it returns
    /// false. Symlinks are reported as links, and so are mount points (an entry on another
    /// device or mount, or a mount root: bind mounts too); nothing is opened. A name that is
    /// not UTF-8 is skipped: the scan works with `str` names, and a lossy one (U+FFFD) could
    /// alias another entry.
    pub fn list(&self, mut each: impl FnMut(Entry) -> bool) -> Result<(), Error> {
        let mut dir = rustix::fs::Dir::read_from(&self.handle).map_err(io)?;
        while let Some(item) = dir.read() {
            let item = item.map_err(io)?;
            let raw = item.file_name().to_bytes();
            if raw == b"." || raw == b".." {
                continue;
            }
            let Ok(name) = std::str::from_utf8(raw) else {
                continue;
            };
            let name = name.to_string();
            let found = match meta(&self.handle, item.file_name(), AtFlags::empty()) {
                Ok(found) => found,
                // Removed since the listing.
                Err(Errno::NOENT) => continue,
                Err(e) => return Err(io(e)),
            };
            let mut info = found.info;
            if found.crosses(self.dev, self.mount) {
                info.attributes |= ATTR_LINK;
            }
            if !each(entry(name, info, self.serial)) {
                return Ok(());
            }
        }
        Ok(())
    }
}

impl File {
    /// Up to `buf.len()` bytes at `offset`.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(&self.file, buf, offset)
    }
}

/// The path an open handle is at, from `/proc/self/fd`. `None` where there is no `/proc`.
fn final_path(handle: &OwnedFd) -> Option<String> {
    use std::os::fd::AsRawFd;
    let link = std::fs::read_link(format!("/proc/self/fd/{}", handle.as_raw_fd())).ok()?;
    let path = link.to_string_lossy().into_owned();
    path.starts_with('/').then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mount point (here `/proc`, below `/`) crosses the boundary however it is told: its
    /// device, its mount id or `STATX_ATTR_MOUNT_ROOT`; a plain folder does not. A bind mount
    /// of the same device is caught by the last two.
    #[test]
    fn mount_points_cross_the_boundary() {
        let root = open_dir(rustix::fs::CWD, "/").unwrap();
        let top = meta_fd(&root).unwrap();
        let proc_ = meta(&root, "proc", AtFlags::empty()).unwrap();
        assert!(proc_.crosses(top.dev, top.mount));
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("plain")).unwrap();
        let dir = open_dir(rustix::fs::CWD, tmp.path().to_str().unwrap()).unwrap();
        let here = meta_fd(&dir).unwrap();
        let plain = meta(&dir, "plain", AtFlags::empty()).unwrap();
        assert!(!plain.crosses(here.dev, here.mount));
        // Same device, but another mount, or the root of one: both cross.
        let bind = Meta {
            mount: here.mount.map(|m| m + 1),
            ..meta(&dir, "plain", AtFlags::empty()).unwrap()
        };
        assert_eq!(bind.crosses(here.dev, here.mount), here.mount.is_some());
        let root_of = Meta {
            mount_root: true,
            ..meta(&dir, "plain", AtFlags::empty()).unwrap()
        };
        assert!(root_of.crosses(here.dev, here.mount));
    }

    /// Without mount information (no `statx`: forced here), every folder counts as a possible
    /// mount point, so a same-device bind mount can't be entered unseen; files don't.
    #[test]
    fn without_mount_information_folders_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("plain")).unwrap();
        std::fs::write(tmp.path().join("f"), "x").unwrap();
        NO_STATX.with(|n| n.set(true));
        let dir = open_dir(rustix::fs::CWD, tmp.path().to_str().unwrap()).unwrap();
        let here = meta_fd(&dir).unwrap();
        let plain = meta(&dir, "plain", AtFlags::empty()).unwrap();
        let file = meta(&dir, "f", AtFlags::empty()).unwrap();
        NO_STATX.with(|n| n.set(false));
        assert!(!plain.mount_known);
        assert!(plain.crosses(here.dev, here.mount));
        assert!(!file.crosses(here.dev, here.mount));
    }
}
