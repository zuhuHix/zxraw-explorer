//! The Unix half of [`super`]: `openat` with `O_NOFOLLOW` relative to held folders,
//! `fstatat(AT_SYMLINK_NOFOLLOW)` lookups, `st_dev` for mount points and `fstatfs` for network
//! file systems.

use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path};
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
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

/// Synthetic Win32-style attributes, size and write time (ns since 1970) from a `stat`.
fn info_of(stat: &Stat) -> Info {
    let attributes = match FileType::from_raw_mode(stat.st_mode as _) {
        FileType::Directory => ATTR_DIRECTORY,
        FileType::Symlink => ATTR_LINK,
        FileType::RegularFile => ATTR_NORMAL,
        _ => ATTR_SPECIAL,
    };
    Info {
        attributes,
        size: stat.st_size.max(0) as u64,
        mtime: (stat.st_mtime as i64)
            .saturating_mul(1_000_000_000)
            .saturating_add(stat.st_mtime_nsec as i64),
    }
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
    fn new(handle: Handle, info: Info, dev: u64) -> Dir {
        Dir {
            handle,
            info,
            serial: DIR_SERIAL.fetch_add(1, Ordering::Relaxed),
            dev,
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
        let stat = rustix::fs::fstat(&handle).map_err(io)?;
        let info = info_of(&stat);
        if !info.is_dir() {
            return Err(Error::WrongKind);
        }
        if is_remote_fs(&handle)? {
            return Err(Error::Link);
        }
        let fs = Fs {
            volume: stat.st_dev as u64,
            narrowed: false,
            root_final: String::new(),
            opened: record.then(|| Mutex::new(Vec::new())),
        };
        fs.note("/");
        Ok((fs, Dir::new(handle, info, stat.st_dev as u64)))
    }

    /// Makes `dir`, a folder opened through this `Fs`, the root nothing may leave from now on:
    /// no handle may be on another device (mount points become links) or outside its path.
    pub fn narrow(&mut self, dir: &Dir) -> Result<(), Error> {
        if let Some(path) = final_path(&dir.handle) {
            self.root_final = path.trim_end_matches('/').to_string();
        }
        self.volume = dir.dev;
        self.narrowed = true;
        Ok(())
    }

    /// Opens `entry`, listed in `parent`, without following links. Refused before opening:
    /// an entry from another folder's listing, a link, a placeholder, a FIFO or device, the
    /// wrong kind. Checked on the handle after: the expected kind, on the root's device (once
    /// narrowed) and not on a network file system, inside the root, one name (files).
    fn open(&self, parent: &Dir, entry: &Entry, dir: bool) -> Result<(OwnedFd, Info, u64), Error> {
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
        let stat = rustix::fs::fstat(&handle).map_err(io)?;
        let info = info_of(&stat);
        let dev = stat.st_dev as u64;
        if info.is_link() {
            return Err(Error::Link);
        }
        if info.is_dir() != dir || (!dir && info.attributes & ATTR_SPECIAL != 0) {
            return Err(Error::WrongKind);
        }
        if self.narrowed {
            if dev != self.volume {
                return Err(Error::Link);
            }
        } else if dev != parent.dev && is_remote_fs(&handle)? {
            return Err(Error::Link);
        }
        if !dir && stat.st_nlink > 1 {
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
        Ok((handle, info, dev))
    }

    /// The entry `name` in folder `parent`, from `fstatat` without following a link; nothing
    /// is opened. `name` must be one plain component.
    pub fn lookup(&self, parent: &Dir, name: &str) -> Result<Entry, Error> {
        if !plain_component(name) {
            return Err(Error::BadName);
        }
        let stat =
            rustix::fs::statat(&parent.handle, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
        let mut info = info_of(&stat);
        if self.narrowed && stat.st_dev as u64 != parent.dev {
            info.attributes |= ATTR_LINK;
        }
        Ok(entry(name.to_string(), info, parent.serial))
    }

    /// Opens folder `entry`, listed in `parent`.
    pub fn dir(&self, parent: &Dir, entry: &Entry) -> Result<Dir, Error> {
        let (handle, info, dev) = self.open(parent, entry, true)?;
        Ok(Dir::new(handle, info, dev))
    }

    /// Opens file `entry`, listed in `parent`.
    pub fn file(&self, parent: &Dir, entry: &Entry) -> Result<File, Error> {
        let (handle, info, _) = self.open(parent, entry, false)?;
        Ok(File {
            file: std::fs::File::from(handle),
            info,
        })
    }
}

impl Dir {
    /// Lists the folder, calling `each` per entry (`.` and `..` skipped) until it returns
    /// false. Symlinks are reported as links, and so are mount points (an entry on another
    /// device); nothing is opened.
    pub fn list(&self, mut each: impl FnMut(Entry) -> bool) -> Result<(), Error> {
        let mut dir = rustix::fs::Dir::read_from(&self.handle).map_err(io)?;
        while let Some(item) = dir.read() {
            let item = item.map_err(io)?;
            let raw = item.file_name().to_bytes();
            if raw == b"." || raw == b".." {
                continue;
            }
            let name = String::from_utf8_lossy(raw).into_owned();
            let stat =
                match rustix::fs::statat(&self.handle, item.file_name(), AtFlags::SYMLINK_NOFOLLOW)
                {
                    Ok(stat) => stat,
                    // Removed since the listing.
                    Err(Errno::NOENT) => continue,
                    Err(e) => return Err(io(e)),
                };
            let mut info = info_of(&stat);
            if stat.st_dev as u64 != self.dev {
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
