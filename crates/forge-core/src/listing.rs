//! Directory enumeration.
//!
//! On Windows `std::fs::read_dir` is backed by `FindFirstFileExW`, and
//! `DirEntry::metadata()` returns the data already captured by the find call — no
//! extra syscall per entry. That makes a plain `read_dir` loop fast enough for folders
//! with tens of thousands of items, provided it runs off the UI thread and streams
//! results in batches (see [`list_dir_streaming`]).

use std::cmp::Ordering;
use std::fs;
use std::path::Path;

use forge_jobs::CancelToken;
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::model::{EntryKind, FileEntry, ext_of};

/// Build a [`FileEntry`] from a path using `symlink_metadata` (does not follow links).
pub fn entry_for_path(path: &Path) -> Result<FileEntry> {
    let md = fs::symlink_metadata(path).map_err(|e| CoreError::io(path, e))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    Ok(entry_from_metadata(path.to_path_buf(), name, &md))
}

fn entry_from_metadata(path: std::path::PathBuf, name: String, md: &fs::Metadata) -> FileEntry {
    let ft = md.file_type();
    let kind = if ft.is_symlink() || is_reparse_point(md) {
        // Junctions report as dirs with the reparse attribute; treat as links so recursive
        // operations never descend into them implicitly.
        EntryKind::Link
    } else if ft.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
    };
    let (hidden, system) = hidden_system(&name, md);
    let ext = if kind == EntryKind::Dir {
        String::new()
    } else {
        ext_of(&path)
    };
    FileEntry {
        name,
        kind,
        size: if kind == EntryKind::File { md.len() } else { 0 },
        modified: md.modified().ok(),
        created: md.created().ok(),
        hidden,
        system,
        readonly: md.permissions().readonly(),
        ext,
        path,
    }
}

#[cfg(windows)]
fn attrs(md: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes()
}

#[cfg(windows)]
fn is_reparse_point(md: &fs::Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    attrs(md) & FILE_ATTRIBUTE_REPARSE_POINT != 0
}
#[cfg(not(windows))]
fn is_reparse_point(_md: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn hidden_system(_name: &str, md: &fs::Metadata) -> (bool, bool) {
    let a = attrs(md);
    (a & 0x2 != 0, a & 0x4 != 0)
}
#[cfg(not(windows))]
fn hidden_system(name: &str, _md: &fs::Metadata) -> (bool, bool) {
    (name.starts_with('.'), false)
}

/// Is the link at `path` pointing to a directory? (Used to let users *explicitly* open
/// junctions/symlinked folders.)
pub fn link_targets_dir(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// Enumerate `dir`, delivering entries in batches of `batch` to `sink`. Returns the number
/// of entries that could not be read. Stops early (returning `Err(Cancelled)`) if `cancel`
/// fires.
pub fn list_dir_streaming(
    dir: &Path,
    batch: usize,
    cancel: &CancelToken,
    mut sink: impl FnMut(Vec<FileEntry>),
) -> Result<usize> {
    let rd = fs::read_dir(dir).map_err(|e| CoreError::io(dir, e))?;
    let mut buf = Vec::with_capacity(batch);
    let mut errors = 0;
    for (i, item) in rd.enumerate() {
        if i % 256 == 0 {
            cancel.check()?;
        }
        let Ok(de) = item else {
            errors += 1;
            continue;
        };
        let name = de.file_name().to_string_lossy().into_owned();
        match de.metadata() {
            Ok(md) => buf.push(entry_from_metadata(de.path(), name, &md)),
            Err(_) => {
                // Still show the entry (e.g. locked system files) with unknown metadata.
                errors += 1;
                let path = de.path();
                let ext = ext_of(&path);
                buf.push(FileEntry {
                    path,
                    name,
                    kind: EntryKind::File,
                    size: 0,
                    modified: None,
                    created: None,
                    hidden: false,
                    readonly: false,
                    system: false,
                    ext,
                });
            }
        }
        if buf.len() >= batch {
            sink(std::mem::replace(&mut buf, Vec::with_capacity(batch)));
        }
    }
    if !buf.is_empty() {
        sink(buf);
    }
    Ok(errors)
}

/// Convenience: list a whole directory into a vector.
pub fn list_dir(dir: &Path) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    list_dir_streaming(dir, 4096, &CancelToken::new(), |b| out.extend(b))?;
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
    Created,
    Type,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortSpec {
    pub key: SortKey,
    pub descending: bool,
    pub dirs_first: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            descending: false,
            dirs_first: true,
        }
    }
}

/// Explorer-style "natural" comparison: case-insensitive, digit runs compared numerically
/// ("IMG_9.jpg" < "IMG_10.jpg").
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    bi.next();
                }
                let ta = na.trim_start_matches('0');
                let tb = nb.trim_start_matches('0');
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                let o = x
                    .to_lowercase()
                    .cmp(y.to_lowercase())
                    .then(Ordering::Equal);
                if o != Ordering::Equal {
                    return o;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

pub fn sort_entries(entries: &mut [FileEntry], spec: SortSpec) {
    entries.sort_by(|a, b| {
        if spec.dirs_first {
            let (ad, bd) = (a.kind != EntryKind::File, b.kind != EntryKind::File);
            if ad != bd {
                return bd.cmp(&ad);
            }
        }
        let o = match spec.key {
            SortKey::Name => natural_cmp(&a.name, &b.name),
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Modified => a.modified.cmp(&b.modified),
            SortKey::Created => a.created.cmp(&b.created),
            SortKey::Type => a.ext.cmp(&b.ext),
        }
        .then_with(|| natural_cmp(&a.name, &b.name));
        if spec.descending { o.reverse() } else { o }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["IMG_10.jpg", "img_9.jpg", "IMG_100.jpg", "a", "B"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["a", "B", "img_9.jpg", "IMG_10.jpg", "IMG_100.jpg"]);
    }

    #[test]
    fn lists_unicode_and_streams_batches() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..1000 {
            fs::write(d.path().join(format!("файл_{i}_日本.txt")), b"x").unwrap();
        }
        fs::create_dir(d.path().join("sub")).unwrap();
        let mut batches = 0;
        let mut all = Vec::new();
        let errs = list_dir_streaming(d.path(), 100, &CancelToken::new(), |b| {
            batches += 1;
            all.extend(b)
        })
        .unwrap();
        assert_eq!(errs, 0);
        assert_eq!(all.len(), 1001);
        assert!(batches >= 10);
        sort_entries(&mut all, SortSpec::default());
        assert!(all[0].is_dir());
        assert_eq!(all[1].ext, "txt");
    }

    #[test]
    fn cancel_stops_listing() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..600 {
            fs::write(d.path().join(format!("{i}")), b"").unwrap();
        }
        let c = CancelToken::new();
        c.cancel();
        assert!(matches!(
            list_dir_streaming(d.path(), 10, &c, |_| {}),
            Err(CoreError::Cancelled)
        ));
    }
}
