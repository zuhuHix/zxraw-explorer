use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Dir,
    /// Symlink / junction / other reparse point. Never followed implicitly by recursive
    /// operations.
    Link,
}

/// Coarse media classification by extension (cheap, no file reads).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaKind {
    Image,
    Raw,
    Video,
    Audio,
    Sidecar,
    Document,
    Archive,
    Project,
    Other,
}

/// One directory entry. Kept small: tens of thousands are held in memory per tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub hidden: bool,
    pub readonly: bool,
    pub system: bool,
    /// Lowercased extension without the dot ("" if none).
    pub ext: String,
}

impl FileEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
    pub fn media_kind(&self) -> MediaKind {
        if self.is_dir() {
            MediaKind::Other
        } else {
            media_kind_for_ext(&self.ext)
        }
    }
}

pub const RAW_EXTS: &[&str] = &[
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "dng", "pef",
    "srw", "x3f", "3fr", "iiq", "erf", "kdc", "mef", "mos", "mrw", "rwl",
];
pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "jpe", "png", "tif", "tiff", "webp", "gif", "bmp", "heic", "heif", "avif",
    "jxl", "psd", "ico", "jfif",
];
pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "mxf", "avi", "webm", "mts", "m2ts", "ts", "wmv", "flv", "mpg",
    "mpeg", "3gp", "braw", "r3d", "insv", "lrv",
];
pub const AUDIO_EXTS: &[&str] = &[
    "wav", "mp3", "flac", "aac", "m4a", "ogg", "opus", "aif", "aiff", "wma", "bwf",
];
pub const SIDECAR_EXTS: &[&str] = &["xmp", "thm", "pp3", "dop", "cos", "on1", "aae"];
pub const ARCHIVE_EXTS: &[&str] = &["zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst"];
pub const PROJECT_EXTS: &[&str] = &[
    "drp",
    "prproj",
    "aep",
    "veg",
    "blend",
    "aup3",
    "fcpxml",
    "lrcat",
    "cocatalog",
    "psb",
    "afphoto",
    "kdenlive",
    "otio",
    "edl",
    "xml",
];
pub const DOCUMENT_EXTS: &[&str] = &[
    "pdf", "txt", "md", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "csv", "rtf", "odt", "json",
];

pub fn media_kind_for_ext(ext: &str) -> MediaKind {
    let e = ext;
    if RAW_EXTS.contains(&e) {
        MediaKind::Raw
    } else if IMAGE_EXTS.contains(&e) {
        MediaKind::Image
    } else if VIDEO_EXTS.contains(&e) {
        MediaKind::Video
    } else if AUDIO_EXTS.contains(&e) {
        MediaKind::Audio
    } else if SIDECAR_EXTS.contains(&e) {
        MediaKind::Sidecar
    } else if ARCHIVE_EXTS.contains(&e) {
        MediaKind::Archive
    } else if PROJECT_EXTS.contains(&e) {
        MediaKind::Project
    } else if DOCUMENT_EXTS.contains(&e) {
        MediaKind::Document
    } else {
        MediaKind::Other
    }
}

pub fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Human-readable byte size (binary units, as Explorer does).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_extensions() {
        assert_eq!(media_kind_for_ext("cr3"), MediaKind::Raw);
        assert_eq!(media_kind_for_ext("jpg"), MediaKind::Image);
        assert_eq!(media_kind_for_ext("mxf"), MediaKind::Video);
        assert_eq!(media_kind_for_ext("xmp"), MediaKind::Sidecar);
        assert_eq!(media_kind_for_ext("weird"), MediaKind::Other);
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1536 * 1024), "1.50 MB");
    }
}
