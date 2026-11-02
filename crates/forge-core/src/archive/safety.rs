//! Format-independent defences used by every archive backend: entry name rules, collision
//! keys, link-free folder creation in the destination and decoder memory estimates.

#[cfg(test)]
use std::cell::RefCell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};
use crate::paths;

#[cfg(test)]
pub(crate) type BeforeParentCreateHook = Box<dyn Fn(&Path)>;

#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_PARENT_CREATE: RefCell<Option<BeforeParentCreateHook>> =
        const { RefCell::new(None) };
    /// Tests: runs with the destination's final path as soon as an extraction pinned it,
    /// before the archive is listed and planned.
    pub(crate) static AFTER_DEST_PINNED: RefCell<Option<BeforeParentCreateHook>> =
        const { RefCell::new(None) };
    /// Tests: runs with the folder's final path before an extraction that made its folder
    /// takes back what it wrote (cancelled, or nothing extracted).
    pub(crate) static BEFORE_CREATED_CLEANUP: RefCell<Option<BeforeParentCreateHook>> =
        const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn run_hook(
    hook: &'static std::thread::LocalKey<RefCell<Option<BeforeParentCreateHook>>>,
    path: &Path,
) {
    hook.with(|h| {
        if let Some(f) = &*h.borrow() {
            f(path);
        }
    });
}

/// Normalise an entry name as stored: `\` becomes `/`. Directory names keep their trailing `/`.
pub fn normalize_entry_name(name: &str) -> String {
    name.replace('\\', "/")
}

/// Validate an (untrusted) entry name and return the relative path it extracts to.
///
/// Rejects rooted names (`/x`, `\x`), drive and UNC prefixes (`C:`, `\\server`), device
/// paths (`\\?\`, `\\.\`), `..`, NUL and other control characters, `:` (alternate data
/// streams), names ending in a dot or space and reserved device names (`CON`, `nul.txt`,
/// `COM1 .log`, ...). See [`paths::validate_relative`].
pub fn validate_entry_name(name: &str) -> Result<PathBuf> {
    let trimmed = name.trim_end_matches(['/', '\\']);
    paths::validate_relative(trimmed)
}

/// Key under which two entry names collide on Windows (case-insensitive, `/`-separated,
/// without a trailing `/`).
pub fn collision_key(rel: &Path) -> String {
    paths::to_slash(rel).to_lowercase()
}

/// Folders held open so they can't change under a write: each one was opened without
/// following reparse points and checked by handle to be a real folder, and the handles deny
/// `FILE_SHARE_DELETE`, so while they live none of them can be renamed, deleted or replaced
/// by a junction, and a path through them resolves to the folders that were checked.
pub(crate) struct PinnedDirs {
    #[allow(dead_code)] // held for their effect
    handles: Vec<fs::File>,
}

/// An extraction destination pinned for a whole job: resolved once to its final path
/// (through any junction, subst or mapped drive the user's path legitimately goes through),
/// then every folder of that real path, from the volume root down, pinned (see
/// [`PinnedDirs`]). Entries are written beneath [`PinnedBase::root`]; per entry only the
/// folders below it are pinned ([`pin_dirs`]).
pub(crate) struct PinnedBase {
    root: PathBuf,
    _chain: PinnedDirs,
}

impl PinnedBase {
    pub(crate) fn open(dest: &Path) -> Result<Self> {
        let root = fs::canonicalize(dest).map_err(|e| CoreError::io(dest, e))?;
        let mut handles = Vec::new();
        let mut cur = PathBuf::new();
        for c in root.components() {
            cur.push(c);
            // The prefix alone (`\\?\C:`) is not a folder; the root after it is.
            if matches!(c, std::path::Component::Prefix(_)) {
                continue;
            }
            handles.push(open_pinned(&cur)?);
        }
        Ok(Self {
            root,
            _chain: PinnedDirs { handles },
        })
    }

    /// The destination's final path, which entries are written beneath.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The folder `name` directly inside this one, pinned (refused when it is a link or
    /// not a folder). Only that folder is pinned by the result: keep `self` alive with it
    /// for its ancestors to stay pinned.
    pub(crate) fn child(&self, name: &std::ffi::OsStr) -> Result<PinnedBase> {
        let mut parts = Path::new(name).components();
        if !matches!(
            (parts.next(), parts.next()),
            (Some(std::path::Component::Normal(_)), None)
        ) {
            return Err(CoreError::Invalid(format!(
                "{} is not a folder name",
                Path::new(name).display()
            )));
        }
        let root = self.root.join(name);
        let handle = open_pinned(&root)?;
        Ok(Self {
            root,
            _chain: PinnedDirs {
                handles: vec![handle],
            },
        })
    }
}

/// Pin the existing folders of `rel` below `base` without creating any, refusing links
/// (see [`pin_dirs`]): what is then removed beneath them is what the job wrote there.
pub(crate) fn pin_existing(base: &PinnedBase, rel: &Path) -> Result<PinnedDirs> {
    let mut handles = Vec::new();
    let mut cur = base.root().to_path_buf();
    for c in rel.components() {
        cur.push(c);
        handles.push(open_pinned(&cur)?);
    }
    Ok(PinnedDirs { handles })
}

/// Create `base/rel` one component at a time, refusing to pass through links, junctions
/// or files, and return the folders below `base` pinned (see [`PinnedDirs`]); `base` itself
/// is pinned by its [`PinnedBase`]. Keep it alive while anything is written beneath.
pub(crate) fn pin_dirs(base: &PinnedBase, rel: &Path) -> Result<PinnedDirs> {
    let mut handles = Vec::new();
    let mut cur = base.root().to_path_buf();
    for c in rel.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                #[cfg(test)]
                BEFORE_PARENT_CREATE.with(|hook| {
                    if let Some(f) = &*hook.borrow() {
                        f(&cur);
                    }
                });
                match fs::create_dir(&cur) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(CoreError::io(&cur, error)),
                }
            }
            Err(error) => return Err(CoreError::io(&cur, error)),
        }
        // Whatever is there now (a link or junction may have won a race) is checked through
        // the handle that then pins it.
        handles.push(open_pinned(&cur)?);
    }
    Ok(PinnedDirs { handles })
}

/// Open a folder itself (not what a reparse point leads to), deny delete sharing, and
/// check through the handle that it is a real folder.
#[cfg(windows)]
fn open_pinned(path: &Path) -> Result<fs::File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    // List access, not just attributes: opens asking only for attributes are left out of
    // the sharing check, so their share mode would not keep anyone from renaming the folder.
    const FILE_LIST_DIRECTORY: u32 = 0x1;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let file = fs::OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|e| CoreError::io(path, e))?;
    // GetFileInformationByHandle: the attributes of the opened object itself.
    let attrs = file
        .metadata()
        .map_err(|e| CoreError::io(path, e))?
        .file_attributes();
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(CoreError::UnsafePath(format!(
            "{} is a link; refusing to extract through it",
            path.display()
        )));
    }
    if attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(CoreError::Invalid(format!(
            "{} exists and is not a folder",
            path.display()
        )));
    }
    Ok(file)
}

#[cfg(not(windows))]
fn open_pinned(path: &Path) -> Result<fs::File> {
    verify_real_directory(path)?;
    fs::File::open(path).map_err(|e| CoreError::io(path, e))
}

#[cfg(not(windows))]
fn verify_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| CoreError::io(path, error))?;
    if metadata.is_dir() && !is_link_or_reparse(&metadata) {
        Ok(())
    } else if is_link_or_reparse(&metadata) {
        Err(CoreError::UnsafePath(format!(
            "{} is a link; refusing to extract through it",
            path.display()
        )))
    } else {
        Err(CoreError::Invalid(format!(
            "{} exists and is not a folder",
            path.display()
        )))
    }
}

pub(crate) fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

// ---------------------------------------------------------------------------
// Decoder memory
// ---------------------------------------------------------------------------

/// Dictionary size declared by an LZMA stream as stored in a ZIP entry (method 14): a 2-byte
/// version, a 2-byte properties size, then the 5-byte LZMA properties.
pub(crate) fn zip_lzma_dictionary(head: &[u8]) -> Option<u64> {
    let props_len = u16::from_le_bytes([*head.get(2)?, *head.get(3)?]) as usize;
    if props_len < 5 {
        return None;
    }
    let d = head.get(5..9)?;
    Some(u32::from_le_bytes([d[0], d[1], d[2], d[3]]) as u64)
}

/// Largest LZMA2 dictionary declared by the first block of an `.xz` stream.
pub(crate) fn xz_dictionary(head: &[u8]) -> Option<u64> {
    const MAGIC: &[u8] = &[0xFD, b'7', b'z', b'X', b'Z', 0];
    if head.get(..6)? != MAGIC {
        return None;
    }
    let block = head.get(12..)?;
    let header_len = (*block.first()? as usize + 1) * 4;
    let block = block.get(..header_len.min(block.len()))?;
    let flags = *block.get(1)?;
    let filters = (flags & 0x03) as usize + 1;
    let mut pos = 2;
    let varint = |pos: &mut usize| -> Option<u64> {
        let mut value = 0u64;
        for shift in 0..9 {
            let byte = *block.get(*pos)?;
            *pos += 1;
            value |= u64::from(byte & 0x7F) << (shift * 7);
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    };
    if flags & 0x40 != 0 {
        varint(&mut pos)?;
    }
    if flags & 0x80 != 0 {
        varint(&mut pos)?;
    }
    let mut largest = None;
    for _ in 0..filters {
        let id = varint(&mut pos)?;
        let size = varint(&mut pos)? as usize;
        let props = block.get(pos..pos + size)?;
        pos += size;
        if id == 0x21 {
            let bits = u32::from(*props.first()? & 0x3F);
            let dict = if bits >= 40 {
                u64::from(u32::MAX)
            } else {
                u64::from(2 | (bits & 1)) << (bits / 2 + 11)
            };
            largest = Some(largest.map_or(dict, |l: u64| l.max(dict)));
        }
    }
    largest
}

/// Model memory declared by a PPMd (method 98) ZIP entry: the 2-byte header holds the
/// order, the memory size in MiB minus one, and the restore method.
pub(crate) fn zip_ppmd_memory(head: &[u8]) -> Option<u64> {
    let v = u16::from_le_bytes([*head.first()?, *head.get(1)?]);
    Some((u64::from((v >> 4) & 0xFF) + 1) << 20)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malicious_names_are_rejected() {
        for evil in [
            "../evil.txt",
            "ok/../../evil.txt",
            "..\\evil.txt",
            "a\\..\\..\\evil.txt",
            "/abs.txt",
            "\\abs.txt",
            "C:/Windows/evil.txt",
            "C:evil.txt",
            "c:\\evil.txt",
            "\\\\server\\share\\evil.txt",
            "//server/share/evil.txt",
            "\\\\?\\C:\\evil.txt",
            "\\\\.\\PhysicalDrive0",
            "file.txt:ads",
            "dir/file.txt:Zone.Identifier",
            "nul\0byte.txt",
            "tab\there.txt",
            "CON",
            "con",
            "PRN.txt",
            "aux.tar.gz",
            "NUL",
            "dir/COM1",
            "COM9.log",
            "LPT1",
            "lpt9.txt",
            "COM1 .txt",
            "trailing.",
            "trailing ",
            "dir./x.txt",
            "dir /x.txt",
            "",
            "/",
            "..",
            "./..",
        ] {
            assert!(
                validate_entry_name(evil).is_err(),
                "{evil:?} must be rejected"
            );
        }
    }

    #[test]
    fn ordinary_names_are_accepted() {
        for (ok, expect) in [
            ("a.txt", "a.txt"),
            ("dir/", "dir"),
            ("dir\\sub\\x.txt", "dir/sub/x.txt"),
            ("./a/./b.txt", "a/b.txt"),
            ("Проект 日本/файл.txt", "Проект 日本/файл.txt"),
            ("COM10.txt", "COM10.txt"),
            ("console.log", "console.log"),
            (".hidden", ".hidden"),
        ] {
            let rel = validate_entry_name(ok).unwrap();
            assert_eq!(paths::to_slash(&rel), expect);
        }
    }

    #[test]
    fn collisions_ignore_case_and_separators() {
        let a = validate_entry_name("Dir/File.TXT").unwrap();
        let b = validate_entry_name("dir\\file.txt").unwrap();
        assert_eq!(collision_key(&a), collision_key(&b));
        assert_eq!(normalize_entry_name("a\\b\\"), "a/b/");
    }

    #[cfg(windows)]
    #[test]
    fn pinned_folders_cannot_be_swapped_while_held() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("dest");
        fs::create_dir(&base).unwrap();
        let pinned_base = PinnedBase::open(&base).unwrap();
        let pinned = pin_dirs(&pinned_base, Path::new("a/b")).unwrap();
        // Neither a folder of the chain nor the base can be renamed or removed, so none can
        // be replaced by a junction while an entry is written beneath them.
        assert!(fs::rename(base.join("a"), d.path().join("moved")).is_err());
        assert!(fs::rename(base.join("a/b"), base.join("a/c")).is_err());
        assert!(fs::remove_dir(base.join("a/b")).is_err());
        assert!(fs::rename(&base, d.path().join("dest2")).is_err());
        // Nor can any folder above the destination, up to the volume root.
        assert!(fs::rename(d.path(), d.path().with_extension("moved")).is_err());
        // Writing beneath them still works.
        fs::write(base.join("a/b/x.txt"), b"x").unwrap();
        fs::rename(base.join("a/b/x.txt"), base.join("a/b/y.txt")).unwrap();
        drop(pinned);
        drop(pinned_base);
        fs::remove_file(base.join("a/b/y.txt")).unwrap();
        fs::rename(base.join("a"), d.path().join("moved")).unwrap();

        // A junction planted in the destination is refused, and nothing is created through it.
        let outside = d.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(base.join("j"))
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
        if !made {
            eprintln!("mklink unavailable; skipping the junction assertion");
            return;
        }
        let pinned_base = PinnedBase::open(&base).unwrap();
        let err = pin_dirs(&pinned_base, Path::new("j/sub"))
            .err()
            .expect("refused");
        assert!(matches!(err, CoreError::UnsafePath(_)), "{err:?}");
        assert!(!outside.join("sub").exists());
    }

    #[test]
    fn decoder_memory_headers_parse() {
        // version 5.2, props size 5, lc/lp/pb byte, 64 MiB dictionary.
        let lzma = [5, 2, 5, 0, 0x5D, 0, 0, 0, 4];
        assert_eq!(zip_lzma_dictionary(&lzma), Some(64 << 20));
        // 8 MiB PPMd model, order 6.
        assert_eq!(
            zip_ppmd_memory(&((7u16 << 4) | 5).to_le_bytes()),
            Some(8 << 20)
        );
        let mut xz = vec![0xFD, b'7', b'z', b'X', b'Z', 0, 0, 1, 0, 0, 0, 0];
        // Block header: size byte 2 (12 bytes), one filter, LZMA2 (0x21) with 1 prop byte.
        xz.extend([2, 0, 0x21, 1, 22, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(xz_dictionary(&xz), Some(8 << 20));
        assert_eq!(xz_dictionary(b"not xz"), None);
    }
}
