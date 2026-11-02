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

/// Create `base/rel` one component at a time, refusing to pass through links or files.
pub(crate) fn ensure_dirs_no_links(base: &Path, rel: &Path) -> Result<()> {
    verify_real_directory(base)?;
    let mut cur = base.to_path_buf();
    for c in rel.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(_) => verify_real_directory(&cur)?,
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
                // `AlreadyExists` may mean that a link or junction won the race.
                verify_real_directory(&cur)?;
            }
            Err(error) => return Err(CoreError::io(&cur, error)),
        }
    }
    Ok(())
}

pub(crate) fn verify_real_directory(path: &Path) -> Result<()> {
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
            assert!(validate_entry_name(evil).is_err(), "{evil:?} must be rejected");
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

    #[test]
    fn decoder_memory_headers_parse() {
        // version 5.2, props size 5, lc/lp/pb byte, 64 MiB dictionary.
        let lzma = [5, 2, 5, 0, 0x5D, 0, 0, 0, 4];
        assert_eq!(zip_lzma_dictionary(&lzma), Some(64 << 20));
        // 8 MiB PPMd model, order 6.
        assert_eq!(zip_ppmd_memory(&((7u16 << 4) | 5).to_le_bytes()), Some(8 << 20));
        let mut xz = vec![0xFD, b'7', b'z', b'X', b'Z', 0, 0, 1, 0, 0, 0, 0];
        // Block header: size byte 2 (12 bytes), one filter, LZMA2 (0x21) with 1 prop byte.
        xz.extend([2, 0, 0x21, 1, 22, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(xz_dictionary(&xz), Some(8 << 20));
        assert_eq!(xz_dictionary(b"not xz"), None);
    }
}
