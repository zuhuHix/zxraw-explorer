//! Linux mount table support: `/proc/self/mountinfo`, sysfs, udev names and `statvfs`.
//!
//! Only `statvfs` can block (a dead network mount), so it runs on worker threads with a
//! shared deadline in [`list_drives`]. Everything else reads local kernel interfaces. Paths
//! are matched lexically: symlinks in a path are not followed, so no call can stall on a
//! network mount.

use std::collections::{BTreeSet, HashSet};
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

use super::{DriveInfo, DriveKind, QUERY_TIMEOUT};
use crate::files::fold_dev;

const MOUNTINFO: &str = "/proc/self/mountinfo";
const SYS_DEV_BLOCK: &str = "/sys/dev/block";
const BY_LABEL: &str = "/dev/disk/by-label";
const BY_UUID: &str = "/dev/disk/by-uuid";

/// File systems shown to the user. Anything not listed (proc, sysfs, tmpfs, overlay,
/// squashfs snaps, fuse.gvfsd-fuse, ...) is hidden.
const VISIBLE_FS: &[&str] = &[
    "ext2",
    "ext3",
    "ext4",
    "btrfs",
    "xfs",
    "f2fs",
    "jfs",
    "reiserfs",
    "vfat",
    "msdos",
    "exfat",
    "ntfs",
    "ntfs3",
    "fuseblk",
    "hfs",
    "hfsplus",
    "iso9660",
    "udf",
    "zfs",
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "fuse.sshfs",
];

const NETWORK_FS: &[&str] = &["nfs", "nfs4", "cifs", "smb3", "fuse.sshfs"];

/// One mount, with the octal escapes of the mount table decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mount {
    pub(crate) mount_point: PathBuf,
    pub(crate) major: u32,
    pub(crate) minor: u32,
    pub(crate) fs_type: String,
    /// Mount source: a device such as `/dev/sda2`, or a share such as `//host/share`.
    pub(crate) source: String,
}

/// Parses `/proc/self/mountinfo` text. Lines that do not parse are skipped.
pub(crate) fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines().filter_map(parse_line).collect()
}

/// One line: `id parent maj:min root mount-point options [tags...] - fstype source superopts`.
fn parse_line(line: &str) -> Option<Mount> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    // The optional tags end with a lone `-`; searching from field 6 keeps a mount point
    // named `-` from being taken for it.
    let sep = 6 + fields.get(6..)?.iter().position(|f| *f == "-")?;
    let (major, minor) = fields[2].split_once(':')?;
    Some(Mount {
        mount_point: PathBuf::from(unescape(fields[4])),
        major: major.parse().ok()?,
        minor: minor.parse().ok()?,
        fs_type: fields.get(sep + 1)?.to_string(),
        source: unescape(fields.get(sep + 2)?),
    })
}

/// Decodes `\ooo` octal escapes (space `\040`, tab `\011`, newline `\012`, backslash `\134`).
pub(crate) fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 4 <= b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let value = (b[i + 1] - b'0') as u32 * 64
                + (b[i + 2] - b'0') as u32 * 8
                + (b[i + 3] - b'0') as u32;
            if value <= 255 {
                out.push(value as u8);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decodes udev's `\xHH` escapes used in `/dev/disk/by-*` names (`My\x20Disk` is `My Disk`).
pub(crate) fn decode_udev_name(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() && b[i + 1] == b'x' {
            if let Some(v) = std::str::from_utf8(&b[i + 2..i + 4])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// glibc `makedev`: the `st_dev` value of a device with these numbers.
pub(crate) fn makedev(major: u32, minor: u32) -> u64 {
    let (ma, mi) = (u64::from(major), u64::from(minor));
    ((ma & 0xffff_f000) << 32)
        | ((ma & 0x0000_0fff) << 8)
        | ((mi & 0xffff_ff00) << 12)
        | (mi & 0xff)
}

fn is_visible_fs(fs: &str) -> bool {
    VISIBLE_FS.contains(&fs)
}

/// Mount points that belong to the system rather than to the user: boot, snaps, runtime
/// state. `/run/media`, `/media` and `/mnt` are where the user's disks appear.
fn is_system_mount_point(mp: &Path) -> bool {
    if ["/run/media", "/media", "/mnt"]
        .iter()
        .any(|p| mp.starts_with(p))
    {
        return false;
    }
    ["/boot", "/snap", "/var/snap", "/run"]
        .iter()
        .any(|p| mp.starts_with(p))
}

/// The mount that holds `path`: the one with the longest matching mount point (the topmost
/// one when several share it). Lexical: no filesystem access.
pub(crate) fn mount_for_path<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    let mut best: Option<&Mount> = None;
    for m in mounts {
        if !path.starts_with(&m.mount_point) {
            continue;
        }
        let longer_or_equal =
            best.is_none_or(|b| m.mount_point.as_os_str().len() >= b.mount_point.as_os_str().len());
        if longer_or_equal {
            best = Some(m);
        }
    }
    best
}

/// The mounts to list: the root and the home directory's mount always; otherwise a real
/// user-visible file system that is not a system mount, and at most one mount per device
/// (btrfs subvolumes and bind mounts repeat a device).
pub(crate) fn visible_mounts(all: &[Mount], home: Option<&Path>) -> Vec<Mount> {
    let home_mount = home
        .and_then(|h| mount_for_path(all, h))
        .map(|m| m.mount_point.clone());
    let pinned = |m: &Mount| {
        m.mount_point == Path::new("/") || home_mount.as_deref() == Some(m.mount_point.as_path())
    };
    let mut devices = HashSet::new();
    let mut out = Vec::new();
    for m in all {
        if pinned(m) {
            devices.insert((m.major, m.minor));
            out.push(m.clone());
            continue;
        }
        if !is_visible_fs(&m.fs_type)
            || is_system_mount_point(&m.mount_point)
            || !devices.insert((m.major, m.minor))
        {
            continue;
        }
        out.push(m.clone());
    }
    out
}

/// True for file systems served over the network.
fn is_network_fs(fs: &str) -> bool {
    NETWORK_FS.contains(&fs)
}

/// Kind of a mount: network file systems, optical discs (`iso9660`/`udf` on `/dev/sr*`),
/// removable or USB devices (per sysfs), and fixed disks otherwise.
pub(crate) fn kind_of(m: &Mount, sys_dev_block: &Path) -> DriveKind {
    if is_network_fs(&m.fs_type) {
        return DriveKind::Network;
    }
    if matches!(m.fs_type.as_str(), "iso9660" | "udf") && m.source.starts_with("/dev/sr") {
        return DriveKind::CdRom;
    }
    if m.major != 0 && device_is_removable(sys_dev_block, m.major, m.minor) {
        return DriveKind::Removable;
    }
    DriveKind::Fixed
}

/// True if the block device `major:minor` is removable or sits on a USB bus. A partition
/// takes the flag of its disk. `sys_dev_block` is normally `/sys/dev/block`.
pub(crate) fn device_is_removable(sys_dev_block: &Path, major: u32, minor: u32) -> bool {
    let Ok(real) = std::fs::canonicalize(sys_dev_block.join(format!("{major}:{minor}"))) else {
        return false;
    };
    if real
        .components()
        .any(|c| c.as_os_str().to_str().is_some_and(|s| s.starts_with("usb")))
    {
        return true;
    }
    let disk = if real.join("partition").exists() {
        real.parent()
    } else {
        Some(real.as_path())
    };
    disk.is_some_and(|d| {
        std::fs::read_to_string(d.join("removable")).is_ok_and(|s| s.trim() == "1")
    })
}

/// Reads the mount table now.
fn read_mounts() -> io::Result<Vec<Mount>> {
    Ok(parse_mountinfo(&std::fs::read_to_string(MOUNTINFO)?))
}

/// `(decoded name, device node)` for each link in a udev `/dev/disk/by-*` directory.
fn udev_links(dir: &str) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = decode_udev_name(&e.file_name().to_string_lossy());
            let node = std::fs::canonicalize(e.path()).ok()?;
            Some((name, node))
        })
        .collect()
}

/// The udev name (label or UUID) of the device a mount source names, if it has one.
fn udev_name_of(links: &[(String, PathBuf)], source: &str) -> Option<String> {
    if !source.starts_with("/dev/") {
        return None;
    }
    let node = std::fs::canonicalize(source).ok()?;
    links
        .iter()
        .find(|(_, dev)| *dev == node)
        .map(|(name, _)| name.clone())
}

/// Label shown when the volume has none: the mount point's last name, or `File System`
/// for `/`.
pub(crate) fn label_fallback(mount_point: &Path) -> String {
    mount_point
        .file_name()
        .filter(|_| mount_point != Path::new("/"))
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "File System".into())
}

/// Total and available bytes of the file system at `path` (`statvfs`). Can block on a dead
/// network mount: call it on a worker thread.
pub(crate) fn statvfs_space(path: &Path) -> io::Result<(u64, u64)> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `st` is a plain C struct, all-zero is a valid value; `c` is NUL-terminated.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let unit = (if st.f_frsize > 0 {
        st.f_frsize
    } else {
        st.f_bsize
    }) as u64;
    Ok((unit * st.f_blocks as u64, unit * st.f_bavail as u64))
}

/// Every drive of the system, with the space of each (see [`super::list_drives`]).
pub(crate) fn list_drives() -> Vec<DriveInfo> {
    let all = match read_mounts() {
        Ok(all) => all,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the mount table");
            return Vec::new();
        }
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mounts = visible_mounts(&all, home.as_deref());
    let labels = udev_links(BY_LABEL);
    let sys = Path::new(SYS_DEV_BLOCK);
    let mut infos: Vec<DriveInfo> = mounts
        .iter()
        .map(|m| DriveInfo {
            root: m.mount_point.clone(),
            kind: kind_of(m, sys),
            label: udev_name_of(&labels, &m.source)
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| label_fallback(&m.mount_point)),
            fs_name: m.fs_type.clone(),
            serial: Some(fold_dev(makedev(m.major, m.minor))),
            total_bytes: None,
            free_bytes: None,
            available: true,
        })
        .collect();

    // One thread per mount, one shared deadline. A mount that misses it is reported as
    // unavailable with its space unknown; its thread is left to finish on its own.
    let (tx, rx) = mpsc::channel::<(usize, io::Result<(u64, u64)>)>();
    for (i, m) in mounts.iter().enumerate() {
        let tx = tx.clone();
        let path = m.mount_point.clone();
        let _ = std::thread::Builder::new()
            .name("forge-statvfs".into())
            .spawn(move || {
                // The receiver may have given up already; that is fine.
                let _ = tx.send((i, statvfs_space(&path)));
            });
    }
    drop(tx);
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut answered = vec![false; infos.len()];
    let mut received = 0;
    while received < infos.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok((i, space)) = rx.recv_timeout(remaining) else {
            break;
        };
        answered[i] = true;
        received += 1;
        if let Ok((total, free)) = space {
            infos[i].total_bytes = Some(total);
            infos[i].free_bytes = Some(free);
        }
    }
    for (info, done) in infos.iter_mut().zip(answered) {
        if !done {
            info.available = false;
        }
    }
    infos
}

/// Mount points of the visible mounts, for the drive watcher. `None` if the mount table
/// cannot be read (the caller keeps its previous state).
pub(crate) fn mount_roots() -> Option<BTreeSet<PathBuf>> {
    let all = read_mounts().ok()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Some(
        visible_mounts(&all, home.as_deref())
            .into_iter()
            .map(|m| m.mount_point)
            .collect(),
    )
}

/// True if the volume that holds `path` is a removable device (see [`kind_of`]).
pub(crate) fn path_is_removable(path: &Path) -> io::Result<bool> {
    let mounts = read_mounts()?;
    let abs = std::path::absolute(path)?;
    Ok(mount_for_path(&mounts, &abs)
        .is_some_and(|m| kind_of(m, Path::new(SYS_DEV_BLOCK)) == DriveKind::Removable))
}

/// `//host/share` (lowercased) if `path` lies on a CIFS/SMB mount, else `None`.
pub(crate) fn network_share_of(path: &Path) -> io::Result<Option<String>> {
    let mounts = read_mounts()?;
    let abs = std::path::absolute(path)?;
    Ok(mount_for_path(&mounts, &abs)
        .filter(|m| matches!(m.fs_type.as_str(), "cifs" | "smb3"))
        .map(|m| m.source.to_lowercase()))
}

/// The volume that holds `path`: `uuid:<UUID>` from `/dev/disk/by-uuid` when the device has
/// one, else `dev:<major>:<minor>` of its mount.
pub(crate) fn volume_identity(path: &Path) -> io::Result<String> {
    let mounts = read_mounts()?;
    let abs = std::path::absolute(path)?;
    let m = mount_for_path(&mounts, &abs)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no mount holds the path"))?;
    if let Some(uuid) = udev_name_of(&udev_links(BY_UUID), &m.source) {
        return Ok(format!("uuid:{uuid}"));
    }
    Ok(format!("dev:{}:{}", m.major, m.minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic `/proc/self/mountinfo` from an Ubuntu btrfs install with extra disks.
    const SAMPLE: &str = "\
20 1 0:19 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
21 1 0:20 / /sys rw,nosuid,nodev,noexec,relatime shared:7 - sysfs sysfs rw
22 1 0:5 / /dev rw,nosuid,relatime shared:2 - devtmpfs udev rw,size=8021276k,mode=755
23 22 0:6 / /dev/pts rw,nosuid,noexec,relatime shared:3 - devpts devpts rw,gid=5,mode=620
24 1 0:23 / /run rw,nosuid,nodev,noexec,relatime shared:24 - tmpfs tmpfs rw,size=1621692k,mode=755
25 1 8:2 /@ / rw,relatime,discard=async,subvolid=256,subvol=/@ shared:1 - btrfs /dev/sda2 rw,subvolid=256,subvol=/@
26 25 8:2 /@home /home rw,relatime,subvolid=257,subvol=/@home shared:2 - btrfs /dev/sda2 rw,subvolid=257,subvol=/@home
27 25 8:1 / /boot/efi rw,relatime shared:28 - vfat /dev/sda1 rw,fmask=0077,dmask=0077,codepage=437
28 25 8:2 /@snap /snap rw,relatime,subvolid=258,subvol=/@snap shared:29 - btrfs /dev/sda2 rw,subvolid=258,subvol=/@snap
29 25 7:0 / /snap/core22/1722 ro,nodev,relatime shared:30 - squashfs /dev/loop0 ro,errors=continue
30 25 0:31 / /run/snapd/ns rw,nosuid,nodev,noexec,relatime shared:31 - nsfs nsfs rw
31 25 0:52 / /run/user/1000/doc rw,nosuid,nodev,relatime - fuse.portal portal rw,user_id=1000
32 25 0:33 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime shared:9 - cgroup2 cgroup2 rw,nsdelegate
33 25 8:17 / /mnt/data rw,relatime shared:40 - xfs /dev/sdb1 rw,attr2,inode64
34 33 8:17 /backups /mnt/bind rw,relatime shared:40 - xfs /dev/sdb1 rw,attr2,inode64
35 25 8:33 / /run/media/zuhu/My\\040Disk rw,nosuid,nodev,relatime shared:41 - exfat /dev/sdc1 rw,uid=1000,gid=1000
36 25 8:34 / /media/zuhu/Ubuntu\\04022.04 ro,nosuid,nodev,relatime - iso9660 /dev/sr0 ro,iocharset=utf8
37 25 0:60 / /mnt/nas rw,relatime - nfs4 nas:/export rw,vers=4.2,proto=tcp
38 25 0:61 / /mnt/share rw,relatime - cifs //nas/Media rw,vers=3.1.1,uid=1000
39 25 0:62 / /home/zuhu/Remote rw,nosuid,nodev,relatime - fuse.sshfs user@box:/srv rw,user_id=1000
40 25 0:70 / /run/user/1000/gvfs rw,nosuid,nodev,relatime - fuse.gvfsd-fuse gvfsd-fuse rw,user_id=1000
41 25 0:71 / /var/snap/x/y rw,relatime - ext4 /dev/sdd2 rw
42 25 0:72 / /mnt/with\\040tab\\011x rw,relatime - ext4 /dev/sdd3 rw
";

    fn visible_points(home: Option<&str>) -> Vec<String> {
        let all = parse_mountinfo(SAMPLE);
        visible_mounts(&all, home.map(Path::new))
            .into_iter()
            .map(|m| m.mount_point.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn mountinfo_parses_every_line_with_decoded_escapes() {
        let all = parse_mountinfo(SAMPLE);
        assert_eq!(all.len(), 23);
        let disk = all.iter().find(|m| m.fs_type == "exfat").unwrap();
        assert_eq!(disk.mount_point, PathBuf::from("/run/media/zuhu/My Disk"));
        assert_eq!((disk.major, disk.minor), (8, 33));
        assert_eq!(disk.source, "/dev/sdc1");
        let tab = all
            .iter()
            .find(|m| m.fs_type == "ext4" && m.source == "/dev/sdd3")
            .unwrap();
        assert_eq!(tab.mount_point, PathBuf::from("/mnt/with tab\tx"));
        let share = all.iter().find(|m| m.fs_type == "cifs").unwrap();
        assert_eq!(share.source, "//nas/Media");
    }

    #[test]
    fn unescape_decodes_octal_escapes_only() {
        assert_eq!(unescape(r"a\040b\011c\134d"), "a b\tc\\d");
        assert_eq!(unescape(r"no\escape\04"), r"no\escape\04");
        assert_eq!(
            unescape(r"\777"),
            r"\777",
            "values above 255 are kept literally"
        );
    }

    #[test]
    fn udev_names_decode_hex_escapes() {
        assert_eq!(decode_udev_name(r"My\x20Disk"), "My Disk");
        assert_eq!(decode_udev_name(r"Data\x2d1"), "Data-1");
        assert_eq!(decode_udev_name(r"plain"), "plain");
        assert_eq!(decode_udev_name(r"bad\xZZ"), r"bad\xZZ");
    }

    #[test]
    fn label_fallback_uses_mount_name_or_file_system() {
        assert_eq!(label_fallback(Path::new("/")), "File System");
        assert_eq!(label_fallback(Path::new("/mnt/data")), "data");
        assert_eq!(
            label_fallback(Path::new("/run/media/zuhu/My Disk")),
            "My Disk"
        );
    }

    #[test]
    fn visible_mounts_keeps_user_file_systems_only() {
        let got = visible_points(Some("/home/zuhu"));
        assert_eq!(
            got,
            [
                "/",
                "/home",
                "/mnt/data",
                "/run/media/zuhu/My Disk",
                "/media/zuhu/Ubuntu 22.04",
                "/mnt/nas",
                "/mnt/share",
                "/home/zuhu/Remote",
                "/mnt/with tab\tx",
            ]
        );
    }

    #[test]
    fn visible_mounts_drops_bind_mount_of_the_same_device() {
        let got = visible_points(None);
        assert!(got.contains(&"/mnt/data".to_string()));
        assert!(!got.contains(&"/mnt/bind".to_string()));
    }

    #[test]
    fn visible_mounts_excludes_system_and_pseudo_mounts() {
        let got = visible_points(Some("/home/zuhu"));
        for bad in [
            "/boot/efi",
            "/snap/core22/1722",
            "/var/snap/x/y",
            "/run/user/1000/gvfs",
            "/proc",
            "/sys/fs/cgroup",
        ] {
            assert!(!got.iter().any(|p| p == bad), "{bad} listed");
        }
    }

    #[test]
    fn home_mount_is_listed_even_without_root_match() {
        // Home on its own disk, mounted under a path the system filter would hide.
        let text =
            "1 0 8:2 / / rw - ext4 /dev/sda2 rw\n2 1 8:9 / /run/home rw - ext4 /dev/sdb1 rw\n";
        let all = parse_mountinfo(text);
        let got: Vec<String> = visible_mounts(&all, Some(Path::new("/run/home/zuhu")))
            .into_iter()
            .map(|m| m.mount_point.to_string_lossy().into_owned())
            .collect();
        assert_eq!(got, ["/", "/run/home"]);
    }

    #[test]
    fn mount_for_path_takes_the_longest_mount_point() {
        let all = parse_mountinfo(SAMPLE);
        let at = |p: &str| mount_for_path(&all, Path::new(p)).map(|m| m.mount_point.clone());
        assert_eq!(at("/home/zuhu/doc.txt"), Some(PathBuf::from("/home")));
        assert_eq!(at("/mnt/data/x/y"), Some(PathBuf::from("/mnt/data")));
        assert_eq!(at("/etc/hosts"), Some(PathBuf::from("/")));
    }

    #[test]
    fn makedev_matches_glibc_encoding() {
        assert_eq!(makedev(8, 2), 0x802);
        // Minor bits above 0xff move to bit 20 and up.
        assert_eq!(makedev(8, 0x101), 0x10_0801);
        // Folding xors the high word into the low one.
        assert_eq!(fold_dev(0x1_0000_0802), 0x803);
    }

    fn fake_sysfs() -> tempfile::TempDir {
        // devices/disk (removable flag), devices/disk/part (a partition), and the
        // dev/MAJ:MIN links that sysfs keeps under /sys/dev/block.
        let root = tempfile::tempdir().unwrap();
        let disk = root.path().join("devices/disk");
        let part = disk.join("part");
        std::fs::create_dir_all(&part).unwrap();
        std::fs::write(disk.join("removable"), "1\n").unwrap();
        std::fs::write(part.join("partition"), "1\n").unwrap();
        let dev = root.path().join("dev");
        std::fs::create_dir_all(&dev).unwrap();
        std::os::unix::fs::symlink("../devices/disk", dev.join("8:0")).unwrap();
        std::os::unix::fs::symlink("../devices/disk/part", dev.join("8:1")).unwrap();
        root
    }

    #[test]
    fn removable_flag_is_read_for_disks_and_their_partitions() {
        let root = fake_sysfs();
        let dev = root.path().join("dev");
        assert!(device_is_removable(&dev, 8, 0), "whole removable disk");
        assert!(
            device_is_removable(&dev, 8, 1),
            "partition takes its disk's flag"
        );
        assert!(!device_is_removable(&dev, 8, 7), "no such device");
    }

    #[test]
    fn fixed_disk_is_not_removable() {
        let root = fake_sysfs();
        std::fs::write(root.path().join("devices/disk/removable"), "0\n").unwrap();
        assert!(!device_is_removable(&root.path().join("dev"), 8, 0));
        assert!(!device_is_removable(&root.path().join("dev"), 8, 1));
    }

    #[test]
    fn kinds_follow_fs_type_and_sysfs() {
        let root = fake_sysfs();
        let dev = root.path().join("dev");
        let m = |fs: &str, src: &str, major, minor| Mount {
            mount_point: PathBuf::from("/x"),
            major,
            minor,
            fs_type: fs.into(),
            source: src.into(),
        };
        assert_eq!(
            kind_of(&m("nfs4", "nas:/e", 0, 45), &dev),
            DriveKind::Network
        );
        assert_eq!(
            kind_of(&m("cifs", "//nas/m", 0, 46), &dev),
            DriveKind::Network
        );
        assert_eq!(
            kind_of(&m("iso9660", "/dev/sr0", 11, 0), &dev),
            DriveKind::CdRom
        );
        assert_eq!(
            kind_of(&m("iso9660", "/dev/loop3", 7, 3), &dev),
            DriveKind::Fixed
        );
        assert_eq!(
            kind_of(&m("vfat", "/dev/sdb1", 8, 1), &dev),
            DriveKind::Removable
        );
        assert_eq!(
            kind_of(&m("ext4", "/dev/sdz9", 8, 9), &dev),
            DriveKind::Fixed
        );
    }

    #[test]
    fn statvfs_reports_the_root_file_system() {
        let (total, free) = statvfs_space(Path::new("/")).unwrap();
        assert!(total > 0);
        assert!(free <= total);
    }

    #[test]
    fn statvfs_of_missing_path_is_an_error() {
        assert!(statvfs_space(Path::new("/no/such/forge/path")).is_err());
    }

    #[test]
    fn path_lookup_does_not_need_the_path_to_exist() {
        // Lexical match: a missing path still resolves to its mount.
        let all = parse_mountinfo(SAMPLE);
        assert!(mount_for_path(&all, Path::new("/mnt/data/not/there")).is_some());
    }
}
