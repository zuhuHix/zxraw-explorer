//! Find and replace text in files, with a preview and an undoable, atomic replace.
//!
//! * [`scan`] walks the roots and returns a [`ScanReport`] of matching files, each with its
//!   matching lines and a fingerprint of the bytes it saw.
//! * [`apply`] replaces in those files, but only where the bytes still match the fingerprint,
//!   keeps the original bytes in a backup file and publishes the result with a replace.
//! * [`undo`] restores a file from its backup, unless it was edited since.
//!
//! Files are decoded (UTF-8, UTF-8 with BOM, UTF-16 with BOM) and searched as whole text, so
//! line endings and the encoding survive the replace byte-for-byte. Online-only cloud files
//! are never read, binary files are skipped, and only files of a known text type are searched
//! unless the user names them in the include list.
//!
//! Writes never follow a link: each previewed file remembers the canonical root it was found
//! under and its path relative to that root, and apply checks that path again before it reads
//! and before it publishes.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};

use crate::cloud::{
    AttributeSource, FsAttributes, background_read_allowed, background_read_allowed_with,
};
use crate::error::{CoreError, Result};
use crate::ops::{JobCtx, WalkItem, walk_tree};
use crate::paths;
use crate::query::glob_match;

/// Files larger than this are skipped by [`scan`] unless the caller raises the limit.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;

/// Preview lines stored per file in a [`FilePreview`]. The counts stay exact past this.
pub const MAX_PREVIEW_LINES: usize = 200;
/// Preview lines stored across all files of one scan. Later files still count their matches,
/// but store no preview lines once this is reached.
pub const MAX_PREVIEW_TOTAL_LINES: usize = 20_000;
/// Matches stored per line; later matches on the line are counted, not stored.
pub const MAX_PREVIEW_HITS_PER_LINE: usize = 100;
/// Matches stored per file.
pub const MAX_PREVIEW_HITS_PER_FILE: usize = 2_000;
/// Matches stored across all files of one scan.
pub const MAX_PREVIEW_TOTAL_HITS: usize = 50_000;
/// Longest match (bytes) shown in the preview. A longer match is counted as not shown.
pub const MAX_SHOWN_MATCH: usize = 4_096;
/// Files a scan examines before it stops, unless the caller changes the limit.
pub const DEFAULT_MAX_TOTAL_FILES: usize = 50_000;
/// Bytes a scan reads before it stops, unless the caller changes the limit.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

/// Longest text shown in a [`LineMatch`]; longer lines are cut to a window around the matches.
const DISPLAY_MAX: usize = 400;
/// Bytes of context kept before the first match when a line is cut.
const DISPLAY_CONTEXT: usize = 100;

/// What to search for and how to replace it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SearchSpec {
    pub pattern: String,
    pub replacement: String,
    /// Treat `pattern` as a regular expression; `replacement` may then use `$1` / `${name}`.
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

impl SearchSpec {
    /// Build the regex for this spec. Literal patterns are escaped, `whole_word` wraps the
    /// pattern in `\b(?:…)\b`, and case-insensitive search adds `(?i)`.
    pub fn compile(&self) -> Result<Regex> {
        if self.pattern.is_empty() {
            return Err(CoreError::Invalid("the search text is empty".into()));
        }
        let body = if self.regex {
            self.pattern.clone()
        } else {
            regex::escape(&self.pattern)
        };
        let body = if self.whole_word {
            format!(r"\b(?:{body})\b")
        } else {
            body
        };
        let pattern = if self.case_sensitive {
            body
        } else {
            format!("(?i){body}")
        };
        Regex::new(&pattern).map_err(|e| CoreError::Invalid(e.to_string()))
    }
}

/// Which files a scan visits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanOptions {
    /// File-name globs (case-insensitive). Empty means every file.
    pub include: Vec<String>,
    /// File-name globs (case-insensitive) that are never scanned.
    pub exclude: Vec<String>,
    /// Descend into sub-folders.
    pub recursive: bool,
    /// Files larger than this (bytes) are skipped as [`SkipReason::TooLarge`].
    pub max_file_size: u64,
    /// The scan stops after examining this many files.
    #[serde(default = "default_max_total_files")]
    pub max_total_files: usize,
    /// The scan stops before a file that would take the bytes read past this.
    #[serde(default = "default_max_total_bytes")]
    pub max_total_bytes: u64,
}

fn default_max_total_files() -> usize {
    DEFAULT_MAX_TOTAL_FILES
}

fn default_max_total_bytes() -> u64 {
    DEFAULT_MAX_TOTAL_BYTES
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            recursive: true,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            max_total_files: DEFAULT_MAX_TOTAL_FILES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

impl ScanOptions {
    /// Split a user-typed list such as `"*.txt; *.md, *.xmp"` into patterns. Separators are
    /// `;`, `,` and whitespace.
    pub fn parse_patterns(text: &str) -> Vec<String> {
        text.split(|c: char| c == ';' || c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// Text encoding of a file, detected from its byte order mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Encoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf8Bom => "UTF-8 BOM",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
        }
    }
}

/// Line endings found in a text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineEnding {
    Lf,
    CrLf,
    /// Both `\r\n` and bare `\n`.
    Mixed,
    /// No line breaks at all.
    None,
}

impl LineEnding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::CrLf => "CRLF",
            Self::Mixed => "Mixed",
            Self::None => "None",
        }
    }
}

/// A decoded file. `text` has the byte order mark removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedText {
    pub encoding: Encoding,
    pub line_ending: LineEnding,
    pub text: String,
}

const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Decode file bytes as text. Returns `None` for data that is not text: a BOM-less file with
/// a NUL byte, or bytes that are not valid UTF-8 / UTF-16. UTF-16 is recognised only by its
/// byte order mark.
pub fn decode(bytes: &[u8]) -> Option<DecodedText> {
    let (encoding, text) = if let Some(rest) = bytes.strip_prefix(&UTF8_BOM) {
        (
            Encoding::Utf8Bom,
            std::str::from_utf8(rest).ok()?.to_string(),
        )
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        (Encoding::Utf16Le, decode_utf16(rest, u16::from_le_bytes)?)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        (Encoding::Utf16Be, decode_utf16(rest, u16::from_be_bytes)?)
    } else {
        if bytes.contains(&0) {
            return None;
        }
        (Encoding::Utf8, std::str::from_utf8(bytes).ok()?.to_string())
    };
    Some(DecodedText {
        encoding,
        line_ending: detect_line_ending(&text),
        text,
    })
}

fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| unit(*c)).collect();
    String::from_utf16(&units).ok()
}

/// Encode text with the given encoding, adding the byte order mark back. The exact inverse of
/// [`decode`] for the same encoding.
pub fn encode(text: &str, encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Utf8 => text.as_bytes().to_vec(),
        Encoding::Utf8Bom => {
            let mut out = UTF8_BOM.to_vec();
            out.extend_from_slice(text.as_bytes());
            out
        }
        Encoding::Utf16Le => {
            let mut out = vec![0xFF, 0xFE];
            for unit in text.encode_utf16() {
                out.extend_from_slice(&unit.to_le_bytes());
            }
            out
        }
        Encoding::Utf16Be => {
            let mut out = vec![0xFE, 0xFF];
            for unit in text.encode_utf16() {
                out.extend_from_slice(&unit.to_be_bytes());
            }
            out
        }
    }
}

fn detect_line_ending(text: &str) -> LineEnding {
    let lf = text.matches('\n').count();
    let crlf = text.matches("\r\n").count();
    match (crlf, lf - crlf) {
        (0, 0) => LineEnding::None,
        (_, 0) => LineEnding::CrLf,
        (0, _) => LineEnding::Lf,
        _ => LineEnding::Mixed,
    }
}

const BINARY_EXTENSIONS: &[&str] = &[
    // raw camera formats
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "pef", "dng",
    "srw", "x3f", "3fr", "fff", "iiq", "rwl", "mos", "erf", "kdc", "dcr", "mef", "mrw",
    // images
    "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif", "avif", "psd", "psb",
    "ico", "jxl", // video and audio
    "mp4", "mov", "avi", "mkv", "mts", "m2ts", "wmv", "webm", "mp3", "wav", "flac", "aac", "m4a",
    "ogg", // archives and binaries
    "zip", "7z", "rar", "gz", "tgz", "bz2", "xz", "zst", "tar", "iso", "dmg", "exe", "dll", "sys",
    "msi", "pdb", "obj", "lib", "bin", "dat", "db", "sqlite", "pdf", "doc", "docx", "xls", "xlsx",
    "ppt", "pptx",
];

/// Is the file extension one of the known binary formats? Checked before the file is read.
pub fn is_binary_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    BINARY_EXTENSIONS.contains(&ext.as_str())
}

/// Extensions (and whole file names, for names like `Dockerfile`) of text formats that are
/// searched by default. Anything else is skipped as [`SkipReason::UnknownType`] unless the
/// user names it in the include list.
const TEXT_EXTENSIONS: &[&str] = &[
    "txt",
    "text",
    "md",
    "markdown",
    "rst",
    "log",
    "csv",
    "tsv",
    "json",
    "jsonc",
    "xml",
    "xmp",
    "xsl",
    "svg",
    "html",
    "htm",
    "xhtml",
    "css",
    "scss",
    "js",
    "mjs",
    "cjs",
    "ts",
    "tsx",
    "jsx",
    "vue",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "properties",
    "env",
    "sql",
    "sh",
    "bash",
    "zsh",
    "bat",
    "cmd",
    "ps1",
    "psm1",
    "py",
    "rb",
    "pl",
    "php",
    "lua",
    "r",
    "rs",
    "go",
    "java",
    "kt",
    "kts",
    "c",
    "h",
    "cpp",
    "hpp",
    "cc",
    "cs",
    "swift",
    "m",
    "mm",
    "gradle",
    "cmake",
    "make",
    "mk",
    "tex",
    "bib",
    "srt",
    "vtt",
    "ass",
    "ssa",
    "sub",
    "lrc",
    "cue",
    "m3u",
    "m3u8",
    "pls",
    "nfo",
    "gpx",
    "kml",
    "geojson",
    "pp3",
    "dop",
    "on1",
    "cube",
    "plist",
    "reg",
    "gitignore",
    "gitattributes",
    "editorconfig",
    "dockerfile",
];

/// Is this a known text format (by extension, or by file name for names such as `Dockerfile`)?
/// The check is by name only; the file is not opened.
pub fn is_known_text_extension(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // `.gitignore` has no extension; its name without the leading dot is the list entry.
    let name = name.trim_start_matches('.').to_ascii_lowercase();
    if TEXT_EXTENSIONS.contains(&name.as_str()) {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| TEXT_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Content identity of a file: byte length and BLAKE3 of the raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub size: u64,
    pub hash: [u8; 32],
}

fn fingerprint(bytes: &[u8]) -> Fingerprint {
    Fingerprint {
        size: bytes.len() as u64,
        hash: *blake3::hash(bytes).as_bytes(),
    }
}

/// Matches shown together: one line of a file, or a window of a long line. A match that runs
/// over several lines brings them all in, so every stored match is shown in full.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineMatch {
    /// 1-based line where the first match starts.
    pub line_number: usize,
    /// The text around the matches, single-line view: line breaks show as `\n` (a lone
    /// carriage return as `\r`). Long lines are cut to a window of about 400 bytes.
    pub line: String,
    /// Byte ranges of the matches inside `line`, each covering the whole match.
    pub ranges: Vec<Range<usize>>,
    /// `line` after replacement, in the same single-line view.
    pub replaced: String,
    /// Byte ranges of the inserted replacement text inside `replaced`.
    pub replaced_ranges: Vec<Range<usize>>,
}

/// A file with at least one match, as found by [`scan`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePreview {
    /// The path as it was found (for display and for matching the user's ticks).
    pub path: PathBuf,
    /// Canonical form of the root this file was found under. For a file given directly as a
    /// root, its canonical parent folder.
    pub root: PathBuf,
    /// The file's path relative to `root`. [`apply`] writes to `root.join(rel)`, never to
    /// `path`.
    pub rel: PathBuf,
    pub encoding: Encoding,
    pub line_ending: LineEnding,
    /// The matches stored for the preview, at most [`MAX_PREVIEW_LINES`] entries.
    pub matches: Vec<LineMatch>,
    /// Every match in the file, including those not stored in `matches`.
    pub match_count: usize,
    /// Matches counted in `match_count` but not shown in `matches`: past a preview limit, or
    /// longer than [`MAX_SHOWN_MATCH`]. [`apply`] replaces them too.
    #[serde(default)]
    pub hidden_matches: usize,
    /// Every line with a match, including those not stored in `matches`.
    pub lines_with_matches: usize,
    /// Identity of the bytes that were searched. [`apply`] refuses files that no longer match.
    pub fingerprint: Fingerprint,
    /// The size limit in force when the file was scanned. [`apply`] reads at most this much.
    pub max_file_size: u64,
}

/// Why a file was not searched or not changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    /// Known binary extension; never read.
    BinaryExtension,
    /// Not a known text type and not named in the include list; never read.
    UnknownType,
    /// Contains NUL bytes or is not decodable text.
    Binary,
    /// Larger than the size limit (bytes).
    TooLarge(u64),
    /// Online-only cloud file, or its attributes could not be read; reading it could download
    /// it, so it is never read.
    OnlineOnly,
    /// Read-only; matches are not changed.
    ReadOnly,
    /// Changed after the preview was made.
    Changed,
    /// A folder in its path was replaced by a link, or the file moved.
    PathChanged,
    /// The file has more than one hard link; replacing it would split the links.
    HardLinked,
    /// Could not be read; the message says why.
    Unreadable(String),
}

impl SkipReason {
    pub fn describe(&self) -> String {
        match self {
            Self::BinaryExtension => "Binary file type, not searched".into(),
            Self::UnknownType => {
                "Not a known text file type; add it to Include files to search it".into()
            }
            Self::Binary => "Not a text file".into(),
            Self::TooLarge(n) => format!("Too large to search ({n} bytes)"),
            Self::OnlineOnly => {
                "Online-only cloud file, or its attributes could not be read; not read".into()
            }
            Self::ReadOnly => "Read-only file, not changed".into(),
            Self::Changed => "Changed since the preview, not changed".into(),
            Self::PathChanged => {
                "A folder in its path was replaced by a link, or the file moved; not changed".into()
            }
            Self::HardLinked => "Hard-linked file, not changed".into(),
            Self::Unreadable(msg) => format!("Could not be read: {msg}"),
        }
    }
}

/// Result of [`scan`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanReport {
    /// Only files with at least one match.
    pub files: Vec<FilePreview>,
    pub skipped: Vec<(PathBuf, SkipReason)>,
    /// Files examined, including skipped ones; files without a match are counted here only.
    pub files_scanned: usize,
    /// Files not examined because the scan reached its file or byte limit.
    #[serde(default)]
    pub files_not_scanned: usize,
}

impl ScanReport {
    pub fn total_matches(&self) -> usize {
        self.files.iter().map(|f| f.match_count).sum()
    }

    /// Matches that will be replaced but are not shown in the preview.
    pub fn hidden_matches(&self) -> usize {
        self.files.iter().map(|f| f.hidden_matches).sum()
    }

    /// What the preview leaves out, for the user to read before replacing. `None` when the
    /// preview shows every match and every file was searched.
    pub fn truncation_notice(&self) -> Option<String> {
        let mut parts = Vec::new();
        let hidden = self.hidden_matches();
        if hidden > 0 {
            let word = if hidden == 1 { "match" } else { "matches" };
            parts.push(format!(
                "Preview truncated: {hidden} more {word} not shown, but Replace changes them too"
            ));
        }
        if self.files_not_scanned > 0 {
            let word = if self.files_not_scanned == 1 {
                "file"
            } else {
                "files"
            };
            parts.push(format!(
                "Search stopped at its file or size limit: {} {word} not searched or changed",
                self.files_not_scanned
            ));
        }
        (!parts.is_empty()).then(|| parts.join(". "))
    }
}

/// Record of one replaced file, enough to undo it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceUndo {
    pub path: PathBuf,
    /// Canonical root and the path under it, re-validated before the file is restored.
    pub root: PathBuf,
    pub rel: PathBuf,
    /// Copy of the original bytes.
    pub backup: PathBuf,
    /// Identity of the bytes written by the replace.
    pub written: Fingerprint,
    pub replacements: usize,
}

/// Result of [`apply`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApplyReport {
    pub undo: Vec<ReplaceUndo>,
    pub skipped: Vec<(PathBuf, SkipReason)>,
    pub failed: Vec<(PathBuf, String)>,
    /// Set when the job was cancelled; files already done stay done.
    pub cancelled: bool,
}

impl ApplyReport {
    pub fn replacements(&self) -> usize {
        self.undo.iter().map(|u| u.replacements).sum()
    }
}

// ---------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------

/// Find matches under `roots`. A folder is walked (one level if not recursive) and its files
/// are filtered by the include/exclude globs; a file root is taken as is. Links are never
/// followed. Errors only for cancellation or an unreadable root.
pub fn scan(
    roots: &[PathBuf],
    spec: &SearchSpec,
    opts: &ScanOptions,
    ctx: &JobCtx<'_>,
) -> Result<ScanReport> {
    scan_with(roots, spec, opts, ctx, &FsAttributes)
}

/// [`scan`] with the attribute source injected; the read gate asks `source` for each file.
fn scan_with(
    roots: &[PathBuf],
    spec: &SearchSpec,
    opts: &ScanOptions,
    ctx: &JobCtx<'_>,
    source: &dyn AttributeSource,
) -> Result<ScanReport> {
    let re = spec.compile()?;
    let mut col = Collector::new(opts);
    for root in roots {
        ctx.cancel.check()?;
        collect_root(root, opts, ctx, &mut col)?;
    }
    ctx.progress.add_total_items(col.out.len() as u64);

    let mut report = ScanReport {
        skipped: col.skipped,
        ..Default::default()
    };
    let mut budget = Budget {
        lines: MAX_PREVIEW_TOTAL_LINES,
        hits: MAX_PREVIEW_TOTAL_HITS,
    };
    let mut bytes_read: u64 = 0;
    for (index, cand) in col.out.iter().enumerate() {
        ctx.cancel.check()?;
        if report.files_scanned >= opts.max_total_files
            || bytes_read.saturating_add(cand.size) > opts.max_total_bytes
        {
            report.files_not_scanned = col.out.len() - index;
            break;
        }
        ctx.progress.set_current(cand.path.display().to_string());
        report.files_scanned += 1;
        bytes_read += scan_file(cand, &re, spec, opts, ctx, source, &mut budget, &mut report);
        ctx.progress.add_items(1);
    }
    Ok(report)
}

struct Candidate {
    path: PathBuf,
    root: PathBuf,
    rel: PathBuf,
    size: u64,
}

/// Collects the files to scan: applies name globs, drops duplicate paths across roots.
struct Collector {
    include: Vec<String>,
    exclude: Vec<String>,
    seen: HashSet<String>,
    out: Vec<Candidate>,
    skipped: Vec<(PathBuf, SkipReason)>,
}

impl Collector {
    fn new(opts: &ScanOptions) -> Self {
        Self {
            include: opts.include.iter().map(|p| p.to_lowercase()).collect(),
            exclude: opts.exclude.iter().map(|p| p.to_lowercase()).collect(),
            seen: HashSet::new(),
            out: Vec::new(),
            skipped: Vec::new(),
        }
    }

    fn name_ok(&self, name: &str) -> bool {
        let name = name.to_lowercase();
        let included = self.include.is_empty() || self.include.iter().any(|p| glob_match(p, &name));
        included && !self.exclude.iter().any(|p| glob_match(p, &name))
    }

    /// Adds a file. `root` is the canonical root and `rel` the path under it.
    fn push(&mut self, path: PathBuf, root: PathBuf, rel: PathBuf, size: u64) {
        let key = if cfg!(windows) {
            path.to_string_lossy().to_lowercase()
        } else {
            path.to_string_lossy().into_owned()
        };
        if self.seen.insert(key) {
            self.out.push(Candidate {
                path,
                root,
                rel,
                size,
            });
        }
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Canonical form of `path` with the `\\?\` prefix removed, the form [`paths`] displays.
fn canonical_path(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path).map(|p| PathBuf::from(paths::display_path(&p)))
}

/// The canonical root and the path of `path` under the root it was walked from (`given`).
fn root_and_rel(given: &Path, canonical: &Path, path: &Path) -> (PathBuf, PathBuf) {
    let rel = path
        .strip_prefix(given)
        .map_or_else(|_| PathBuf::from(file_name_of(path)), Path::to_path_buf);
    (canonical.to_path_buf(), rel)
}

fn collect_root(
    root: &Path,
    opts: &ScanOptions,
    ctx: &JobCtx<'_>,
    col: &mut Collector,
) -> Result<()> {
    let md = fs::symlink_metadata(root).map_err(|e| CoreError::io(root, e))?;
    if md.file_type().is_symlink() {
        col.skipped.push((
            root.to_path_buf(),
            SkipReason::Unreadable("a link, not followed".into()),
        ));
        return Ok(());
    }
    if md.is_file() {
        // A file root: its canonical parent is the root, the file name the relative path.
        let parent = root
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let canonical = canonical_path(parent).map_err(|e| CoreError::io(parent, e))?;
        let rel = PathBuf::from(file_name_of(root));
        col.push(root.to_path_buf(), canonical, rel, md.len());
        return Ok(());
    }
    if !md.is_dir() {
        return Ok(());
    }
    let canonical = canonical_path(root).map_err(|e| CoreError::io(root, e))?;
    if opts.recursive {
        walk_tree(root, ctx.cancel, |item| match item {
            WalkItem::Entry(e) => {
                if e.is_file() && col.name_ok(&file_name_of(&e.path)) {
                    let (canon, rel) = root_and_rel(root, &canonical, &e.path);
                    col.push(e.path.clone(), canon, rel, e.size);
                }
            }
            WalkItem::Error { path, error } => col
                .skipped
                .push((path.clone(), SkipReason::Unreadable(error.to_string()))),
        })?;
    } else {
        let entries = fs::read_dir(root).map_err(|e| CoreError::io(root, e))?;
        for entry in entries {
            ctx.cancel.check()?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    col.skipped
                        .push((root.to_path_buf(), SkipReason::Unreadable(e.to_string())));
                    continue;
                }
            };
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) => {
                    col.skipped
                        .push((path, SkipReason::Unreadable(e.to_string())));
                    continue;
                }
            };
            if meta.is_file() && col.name_ok(&file_name_of(&path)) {
                let (canon, rel) = root_and_rel(root, &canonical, &path);
                col.push(path, canon, rel, meta.len());
            }
        }
    }
    Ok(())
}

/// Records a skipped file read before any bytes were read; returns 0, the bytes read.
fn skip(report: &mut ScanReport, path: &Path, reason: SkipReason) -> u64 {
    report.skipped.push((path.to_path_buf(), reason));
    0
}

/// Is `name` (a file name, not a path) explicitly named by the include list? True when the list
/// is non-empty and one of its patterns matches the name, so the user asked for this type.
fn named_by_include(include: &[String], name: &str) -> bool {
    let name = name.to_lowercase();
    include.iter().any(|p| glob_match(&p.to_lowercase(), &name))
}

/// Checks run per file, in this order: cloud read gate, binary extension, known text type,
/// listed size, bounded read, decode, search, read-only. Returns the bytes read.
fn scan_file(
    cand: &Candidate,
    re: &Regex,
    spec: &SearchSpec,
    opts: &ScanOptions,
    ctx: &JobCtx<'_>,
    source: &dyn AttributeSource,
    budget: &mut Budget,
    report: &mut ScanReport,
) -> u64 {
    let path = cand.path.as_path();
    if !background_read_allowed_with(path, source) {
        return skip(report, path, SkipReason::OnlineOnly);
    }
    if is_binary_extension(path) {
        return skip(report, path, SkipReason::BinaryExtension);
    }
    let name = file_name_of(path);
    if !is_known_text_extension(path) && !named_by_include(&opts.include, &name) {
        return skip(report, path, SkipReason::UnknownType);
    }
    if cand.size > opts.max_file_size {
        return skip(report, path, SkipReason::TooLarge(cand.size));
    }
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) => {
            ctx.progress.add_error();
            let msg = CoreError::io(path, e).to_string();
            return skip(report, path, SkipReason::Unreadable(msg));
        }
    };
    // The listing size can be stale: check the open file, then never read past the limit.
    let md = match file.metadata() {
        Ok(md) => md,
        Err(e) => {
            ctx.progress.add_error();
            let msg = CoreError::io(path, e).to_string();
            return skip(report, path, SkipReason::Unreadable(msg));
        }
    };
    if md.len() > opts.max_file_size {
        return skip(report, path, SkipReason::TooLarge(md.len()));
    }
    match handle_stamp(&file) {
        Ok(st) if st.links > 1 => return skip(report, path, SkipReason::HardLinked),
        Ok(_) => {}
        Err(e) => {
            ctx.progress.add_error();
            let msg = CoreError::io(path, e).to_string();
            return skip(report, path, SkipReason::Unreadable(msg));
        }
    }
    let bytes = match read_bounded(&mut file, opts.max_file_size) {
        Ok(Some(bytes)) => bytes,
        // The file grew after the metadata was read: at least one byte over the limit.
        Ok(None) => {
            skip(report, path, SkipReason::TooLarge(opts.max_file_size + 1));
            return opts.max_file_size + 1;
        }
        Err(e) => {
            ctx.progress.add_error();
            let msg = CoreError::io(path, e).to_string();
            return skip(report, path, SkipReason::Unreadable(msg));
        }
    };
    let read = bytes.len() as u64;
    let Some(decoded) = decode(&bytes) else {
        skip(report, path, SkipReason::Binary);
        return read;
    };
    let found = find_matches(&decoded.text, re, spec, budget);
    if found.count == 0 {
        return read;
    }
    budget.lines -= found.matches.len();
    budget.hits -= found.shown;
    if md.permissions().readonly() {
        skip(report, path, SkipReason::ReadOnly);
        return read;
    }
    report.files.push(FilePreview {
        path: cand.path.clone(),
        root: cand.root.clone(),
        rel: cand.rel.clone(),
        encoding: decoded.encoding,
        line_ending: decoded.line_ending,
        hidden_matches: found.count - found.shown,
        matches: found.matches,
        match_count: found.count,
        lines_with_matches: found.lines_with_matches,
        fingerprint: fingerprint(&bytes),
        max_file_size: opts.max_file_size,
    });
    read
}

/// Reads at most `limit` bytes from `file`. `Ok(None)` when the file holds more than `limit`
/// bytes, so the caller can refuse it without reading the rest.
fn read_bounded(file: &mut File, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Ok(None);
    }
    Ok(Some(bytes))
}

/// Text used for one match: the expanded template in regex mode, verbatim otherwise.
fn replacement_text(caps: &Captures<'_>, spec: &SearchSpec) -> String {
    if spec.regex {
        let mut out = String::new();
        caps.expand(&spec.replacement, &mut out);
        out
    } else {
        spec.replacement.clone()
    }
}

/// Keep only the non-empty matches; empty matches would insert text between every character.
fn real_matches<'t>(text: &'t str, re: &'t Regex) -> impl Iterator<Item = Captures<'t>> + 't {
    re.captures_iter(text)
        .filter(|caps| caps.get(0).is_some_and(|m| !m.is_empty()))
}

/// Tracks the line containing a byte offset. Offsets must be passed in increasing order.
struct LineCursor<'a> {
    text: &'a str,
    started: bool,
    line_start: usize,
    /// Index of the `\n` that ends the current line (or the text length).
    line_end: usize,
    counted: usize,
    line_no: usize,
}

impl<'a> LineCursor<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            started: false,
            line_start: 0,
            line_end: 0,
            counted: 0,
            line_no: 1,
        }
    }

    /// Returns (1-based line number, line start, content end without a trailing `\r`).
    fn locate(&mut self, pos: usize) -> (usize, usize, usize) {
        if !self.started || pos > self.line_end {
            self.started = true;
            let start = self.text[..pos].rfind('\n').map_or(0, |i| i + 1);
            self.line_no += self.text[self.counted..start].matches('\n').count();
            self.counted = start;
            self.line_start = start;
            self.line_end = self.text[pos..]
                .find('\n')
                .map_or(self.text.len(), |i| pos + i);
        }
        let bytes = self.text.as_bytes();
        let content_end = if self.line_end > self.line_start && bytes[self.line_end - 1] == b'\r' {
            self.line_end - 1
        } else {
            self.line_end
        };
        (self.line_no, self.line_start, content_end)
    }
}

/// Preview storage left for the rest of a scan.
struct Budget {
    lines: usize,
    hits: usize,
}

/// What [`find_matches`] found in one file.
struct Found {
    matches: Vec<LineMatch>,
    count: usize,
    lines_with_matches: usize,
    /// Matches stored in `matches`; every other match is counted only.
    shown: usize,
}

/// Matches shown together in one [`LineMatch`]: they start on the same line (or on the last
/// line of a multi-line match before them) and fit in one display window.
struct Group {
    line_no: usize,
    /// Start of the line holding the last byte of the last match.
    last_line_start: usize,
    /// Start of the display window.
    from: usize,
    /// End of the last covered line, or later when a match takes the line break.
    cover_end: usize,
    /// Absolute byte range of each match and its replacement text.
    hits: Vec<(Range<usize>, String)>,
}

/// Find the matches of `text`. Counts every match, but stores at most
/// [`MAX_PREVIEW_HITS_PER_LINE`] per line, [`MAX_PREVIEW_HITS_PER_FILE`] per file and what is
/// left of `budget`. Each stored match is shown in full, line breaks included; a match longer
/// than [`MAX_SHOWN_MATCH`] is counted but not stored. Replacement text is only built for
/// stored matches.
fn find_matches(text: &str, re: &Regex, spec: &SearchSpec, budget: &Budget) -> Found {
    let line_limit = MAX_PREVIEW_LINES.min(budget.lines);
    let hit_limit = MAX_PREVIEW_HITS_PER_FILE.min(budget.hits);
    let mut lines = LineCursor::new(text);
    let mut out: Vec<LineMatch> = Vec::new();
    let mut group: Option<Group> = None;
    let mut count = 0;
    let mut lines_total = 0;
    let mut shown = 0;
    // Line start of the last line where a match started, and the matches stored on it.
    let mut last_line: Option<usize> = None;
    let mut on_line = 0;
    for caps in real_matches(text, re) {
        let Some(m) = caps.get(0) else { continue };
        count += 1;
        let (line_no, line_start, content_end) = lines.locate(m.start());
        if last_line != Some(line_start) {
            last_line = Some(line_start);
            lines_total += 1;
            on_line = 0;
        }
        if m.len() > MAX_SHOWN_MATCH || on_line >= MAX_PREVIEW_HITS_PER_LINE || shown >= hit_limit {
            continue;
        }
        // The line holding the last byte of the match. A match that takes the line break
        // covers it, so the break is shown.
        let (_, end_line_start, end_content_end) = lines.locate(m.end() - 1);
        let cover_end = end_content_end.max(m.end());
        let joins = group
            .as_ref()
            .is_some_and(|g| g.last_line_start == line_start && m.end() - g.from <= DISPLAY_MAX);
        if !joins {
            if let Some(g) = group.take() {
                out.push(finish_group(text, &g));
            }
            if out.len() >= line_limit {
                continue;
            }
            // Short lines are shown whole; long ones from a little before the first match.
            let from = if content_end.max(cover_end) - line_start <= DISPLAY_MAX
                || m.start() - line_start <= DISPLAY_CONTEXT
            {
                line_start
            } else {
                floor_boundary(text, m.start() - DISPLAY_CONTEXT)
            };
            group = Some(Group {
                line_no,
                last_line_start: line_start,
                from,
                cover_end: content_end,
                hits: Vec::new(),
            });
        }
        let Some(g) = group.as_mut() else { continue };
        g.last_line_start = end_line_start;
        g.cover_end = g.cover_end.max(cover_end);
        g.hits.push((m.range(), replacement_text(&caps, spec)));
        on_line += 1;
        shown += 1;
    }
    if let Some(g) = group.take() {
        out.push(finish_group(text, &g));
    }
    Found {
        matches: out,
        count,
        lines_with_matches: lines_total,
        shown,
    }
}

/// Render a group: the window from `g.from`, cut after [`DISPLAY_MAX`] bytes unless a match
/// runs further, with line breaks shown as `\n`.
fn finish_group(text: &str, g: &Group) -> LineMatch {
    let last_end = g.hits.last().map_or(g.from, |(r, _)| r.end);
    let to = if g.cover_end - g.from <= DISPLAY_MAX {
        g.cover_end
    } else {
        floor_boundary(text, g.from + DISPLAY_MAX).max(last_end)
    };
    let mut line = String::new();
    let mut ranges = Vec::with_capacity(g.hits.len());
    let mut replaced = String::new();
    let mut replaced_ranges = Vec::with_capacity(g.hits.len());
    let mut prev = g.from;
    for (range, rep) in &g.hits {
        let between = one_line(&text[prev..range.start]);
        line.push_str(&between);
        replaced.push_str(&between);
        let begin = line.len();
        line.push_str(&one_line(&text[range.clone()]));
        ranges.push(begin..line.len());
        let begin = replaced.len();
        replaced.push_str(&one_line(rep));
        if replaced.len() > begin {
            replaced_ranges.push(begin..replaced.len());
        }
        prev = range.end;
    }
    let rest = one_line(&text[prev..to]);
    line.push_str(&rest);
    replaced.push_str(&rest);
    LineMatch {
        line_number: g.line_no,
        line,
        ranges,
        replaced,
        replaced_ranges,
    }
}

/// Single-line display form of text: a line break (`\r\n` or `\n`) becomes a visible `\n`,
/// and a lone `\r` a visible `\r`.
fn one_line(text: &str) -> String {
    text.replace("\r\n", "\\n")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn floor_boundary(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Replace every match in `text`. Regex mode expands `$1`/`${name}`; literal mode inserts the
/// replacement verbatim. With `CrLf` line endings, each bare `\n` in the inserted text becomes
/// `\r\n`. Returns the new text and the number of replacements.
pub fn replace_text(
    text: &str,
    re: &Regex,
    spec: &SearchSpec,
    line_ending: LineEnding,
) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut count = 0;
    for caps in real_matches(text, re) {
        let Some(m) = caps.get(0) else { continue };
        out.push_str(&text[last..m.start()]);
        let rep = replacement_text(&caps, spec);
        if line_ending == LineEnding::CrLf {
            out.push_str(&rep.replace("\r\n", "\n").replace('\n', "\r\n"));
        } else {
            out.push_str(&rep);
        }
        last = m.end();
        count += 1;
    }
    out.push_str(&text[last..]);
    (out, count)
}

// ---------------------------------------------------------------------------
// Apply and undo
// ---------------------------------------------------------------------------

enum Outcome {
    Replaced(ReplaceUndo),
    Skipped(SkipReason),
    /// The job was cancelled inside this file; the file was not changed.
    Cancelled,
}

/// Replace in each previewed file. A file is changed only if its bytes still match the preview
/// fingerprint; the original bytes go to a backup in `backup_dir` first. A failure on one file
/// is recorded and the rest continue. Errors only for a bad spec or an uncreatable backup dir.
///
/// The file is written to `root.join(rel)`, after checking that no folder on that path is a link
/// (see [`FilePreview`]).
pub fn apply(
    files: &[FilePreview],
    spec: &SearchSpec,
    backup_dir: &Path,
    ctx: &JobCtx<'_>,
) -> Result<ApplyReport> {
    apply_with_hook(files, spec, backup_dir, ctx, &|_| {})
}

/// [`apply`] with a hook that runs for each file at the last moment before it is published.
/// Tests use it to change the job or the file exactly there.
fn apply_with_hook(
    files: &[FilePreview],
    spec: &SearchSpec,
    backup_dir: &Path,
    ctx: &JobCtx<'_>,
    before_publish: &dyn Fn(&Path),
) -> Result<ApplyReport> {
    let re = spec.compile()?;
    fs::create_dir_all(backup_dir).map_err(|e| CoreError::io(backup_dir, e))?;
    ctx.progress.add_total_items(files.len() as u64);

    let mut report = ApplyReport::default();
    for (index, file) in files.iter().enumerate() {
        if ctx.cancel.is_cancelled() {
            report.cancelled = true;
            break;
        }
        ctx.progress.set_current(file.path.display().to_string());
        let step = Step {
            re: &re,
            spec,
            backup_dir,
            ctx,
            before_publish,
        };
        match replace_one(file, index, &step) {
            Ok(Outcome::Replaced(undo)) => report.undo.push(undo),
            Ok(Outcome::Skipped(reason)) => report.skipped.push((file.path.clone(), reason)),
            Ok(Outcome::Cancelled) => {
                report.cancelled = true;
                break;
            }
            Err(e) => {
                ctx.progress.add_error();
                report.failed.push((file.path.clone(), e.to_string()));
            }
        }
        ctx.progress.add_items(1);
    }
    Ok(report)
}

/// What one file's replace needs besides the file itself.
struct Step<'a> {
    re: &'a Regex,
    spec: &'a SearchSpec,
    backup_dir: &'a Path,
    ctx: &'a JobCtx<'a>,
    before_publish: &'a dyn Fn(&Path),
}

/// What a concurrent writer or a swapped file changes: size, last-write time and file identity
/// (volume serial and file index), plus the hard-link count.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: u64,
    volume: u32,
    index: u64,
    links: u32,
}

/// The [`Stamp`] of an open file, from `GetFileInformationByHandle`.
#[cfg(windows)]
fn handle_stamp(file: &File) -> io::Result<Stamp> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is owned by `file`, which outlives the call; `info` is a valid out
    // pointer.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(io::Error::other)?;
    let join = |hi: u32, lo: u32| (u64::from(hi) << 32) | u64::from(lo);
    Ok(Stamp {
        len: join(info.nFileSizeHigh, info.nFileSizeLow),
        modified: join(
            info.ftLastWriteTime.dwHighDateTime,
            info.ftLastWriteTime.dwLowDateTime,
        ),
        volume: info.dwVolumeSerialNumber,
        index: join(info.nFileIndexHigh, info.nFileIndexLow),
        links: info.nNumberOfLinks,
    })
}

#[cfg(not(windows))]
fn handle_stamp(file: &File) -> io::Result<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let md = file.metadata()?;
    Ok(Stamp {
        len: md.len(),
        modified: (md.mtime() as u64) << 32 ^ md.mtime_nsec() as u64,
        volume: md.dev() as u32,
        index: md.ino(),
        links: md.nlink() as u32,
    })
}

/// The [`Stamp`] of `path`, opened for attributes only and sharing everything, so no other
/// program is disturbed. A link at `path` is not followed.
fn path_stamp(path: &Path) -> io::Result<Stamp> {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(path)?
    };
    #[cfg(not(windows))]
    let file = File::open(path)?;
    handle_stamp(&file)
}

/// True when `file.root.join(&file.rel)` is still the previewed file: every folder on the way is
/// a real folder (not a symlink or junction), the file is a regular file, and the root and the
/// file still canonicalize to where they did at scan time. Cloud placeholders are not links, so
/// they pass.
fn location_intact(file: &FilePreview) -> bool {
    location_intact_at(&file.root, &file.rel)
}

/// [`location_intact`] for a canonical `root` and the path `rel` under it.
fn location_intact_at(root: &Path, rel: &Path) -> bool {
    let mut names = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(name) => names.push(name),
            _ => return false,
        }
    }
    let Some(last) = names.len().checked_sub(1) else {
        return false;
    };
    let mut cur = root.to_path_buf();
    for (i, name) in names.iter().enumerate() {
        cur.push(name);
        let Ok(md) = fs::symlink_metadata(&cur) else {
            return false;
        };
        if md.file_type().is_symlink() {
            return false;
        }
        let expected = if i == last { md.is_file() } else { md.is_dir() };
        if !expected {
            return false;
        }
    }
    match canonical_path(root) {
        Ok(now) if now == root => {}
        _ => return false,
    }
    canonical_path(&cur).is_ok_and(|now| now.starts_with(root))
}

/// Opens `path` for reading and replacing. On Windows the handle shares read and delete only,
/// so no other process can write to the file while it is open: the bytes read through it cannot
/// change under us. Elsewhere this is a plain open.
fn open_locked(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_DELETE.0)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

/// Replace in one file. Order: cloud gate, location, locked open, hard-link check, bounded
/// read, fingerprint, replacement in memory, backup written from the bytes read, then
/// [`publish`]. A stop (changed, moved, cancelled, failed) after the backup was written
/// removes the backup.
fn replace_one(file: &FilePreview, index: usize, step: &Step<'_>) -> Result<Outcome> {
    let target = file.root.join(&file.rel);
    if !background_read_allowed(&target) {
        return Ok(Outcome::Skipped(SkipReason::OnlineOnly));
    }
    if !location_intact(file) {
        return Ok(Outcome::Skipped(SkipReason::PathChanged));
    }
    let mut handle = open_locked(&target).map_err(|e| CoreError::io(&target, e))?;
    let md = handle.metadata().map_err(|e| CoreError::io(&target, e))?;
    if md.permissions().readonly() {
        return Ok(Outcome::Skipped(SkipReason::ReadOnly));
    }
    let before = handle_stamp(&handle).map_err(|e| CoreError::io(&target, e))?;
    if before.links > 1 {
        return Ok(Outcome::Skipped(SkipReason::HardLinked));
    }
    let original = match read_bounded(&mut handle, file.max_file_size) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(Outcome::Skipped(SkipReason::Changed)),
        Err(e) => return Err(CoreError::io(&target, e)),
    };
    if original.len() as u64 != file.fingerprint.size || fingerprint(&original) != file.fingerprint
    {
        return Ok(Outcome::Skipped(SkipReason::Changed));
    }
    let Some(decoded) = decode(&original) else {
        return Ok(Outcome::Skipped(SkipReason::Binary));
    };
    let (text, count) = replace_text(&decoded.text, step.re, step.spec, decoded.line_ending);
    if count == 0 {
        return Ok(Outcome::Skipped(SkipReason::Changed));
    }
    let new_bytes = encode(&text, decoded.encoding);
    if step.ctx.cancel.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }

    let backup = step
        .backup_dir
        .join(format!("{index:05}-{}", file_name_of(&target)));
    write_new_file(&backup, &original).map_err(|e| CoreError::io(&backup, e))?;
    let checks = || {
        if !location_intact(file) {
            return Some(Outcome::Skipped(SkipReason::PathChanged));
        }
        (step.before_publish)(&target);
        if step.ctx.cancel.is_cancelled() {
            return Some(Outcome::Cancelled);
        }
        None
    };
    match publish(&target, handle, &before, &new_bytes, checks) {
        Ok(Published::Done) => Ok(Outcome::Replaced(ReplaceUndo {
            path: target,
            root: file.root.clone(),
            rel: file.rel.clone(),
            backup,
            written: fingerprint(&new_bytes),
            replacements: count,
        })),
        Ok(Published::Stopped(stop)) => {
            let _ = fs::remove_file(&backup);
            Ok(stop)
        }
        Ok(Published::Changed) => {
            let _ = fs::remove_file(&backup);
            Ok(Outcome::Skipped(SkipReason::Changed))
        }
        Err(e) => {
            let _ = fs::remove_file(&backup);
            Err(e)
        }
    }
}

enum Published {
    Done,
    /// `checks` stopped it.
    Stopped(Outcome),
    /// The file's stamp changed after it was read.
    Changed,
}

/// Write `new_bytes` to a temp file in the target's folder and publish it over `target` with
/// `ReplaceFileW`. `handle` is the deny-write handle the current bytes were read through and
/// `before` its stamp. The temp file is removed whenever nothing is published.
///
/// Order: write and flush the temp file while `handle` still blocks other writers, close
/// `handle` (ReplaceFileW cannot run while it is open), run `checks` (location and junction
/// validation, cancellation), then compare the size, last-write time and file ID with `before`
/// as the very last step before ReplaceFileW. Protection against a concurrent edit is
/// best-effort: a writer that gets in during the microseconds between that comparison and
/// ReplaceFileW is not detected (its edit is replaced; the backup holds the bytes we read).
fn publish(
    target: &Path,
    handle: File,
    before: &Stamp,
    new_bytes: &[u8],
    checks: impl FnOnce() -> Option<Outcome>,
) -> Result<Published> {
    let (temp, mut temp_file) = create_temp(target)?;
    let written = temp_file
        .write_all(new_bytes)
        .and_then(|()| temp_file.sync_all());
    drop(temp_file);
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(CoreError::io(target, e));
    }
    drop(handle);
    if let Some(stop) = checks() {
        let _ = fs::remove_file(&temp);
        return Ok(Published::Stopped(stop));
    }
    if !path_stamp(target).is_ok_and(|now| now == *before) {
        let _ = fs::remove_file(&temp);
        return Ok(Published::Changed);
    }
    match replace_into_place(&temp, target) {
        Ok(()) => Ok(Published::Done),
        Err(e) => {
            let _ = fs::remove_file(&temp);
            Err(CoreError::io(target, e))
        }
    }
}

/// Create `path` and write `bytes` to it; a partly written file is removed on failure.
fn write_new_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = File::create_new(path)?;
    let written = f.write_all(bytes).and_then(|()| f.sync_all());
    drop(f);
    if written.is_err() {
        let _ = fs::remove_file(path);
    }
    written
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Create a new temp file next to `path`, named `.{name}.zxraw-replace-{pid}-{seq}.tmp`.
fn create_temp(path: &Path) -> Result<(PathBuf, File)> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = file_name_of(path);
    let pid = std::process::id();
    for _ in 0..1000 {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.zxraw-replace-{pid}-{seq}.tmp"));
        match File::create_new(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(CoreError::io(path, e)),
        }
    }
    Err(CoreError::Invalid(format!(
        "{}: could not create a temporary file",
        path.display()
    )))
}

/// Swap `tmp` in for `path` with `ReplaceFileW`, which keeps the original's creation time,
/// attributes, ACLs and alternate streams. No merge errors are ignored and there is no
/// fallback: if they cannot be kept, the call fails and the original stays untouched.
#[cfg(windows)]
fn replace_into_place(tmp: &Path, path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{REPLACE_FILE_FLAGS, ReplaceFileW};
    use windows::core::PCWSTR;

    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let target = wide(path);
    let source = wide(tmp);
    // SAFETY: both strings are NUL-terminated and outlive the call. The backup, exclude and
    // reserved arguments are null, which the API accepts.
    unsafe {
        ReplaceFileW(
            PCWSTR(target.as_ptr()),
            PCWSTR(source.as_ptr()),
            PCWSTR::null(),
            REPLACE_FILE_FLAGS(0),
            None,
            None,
        )
    }
    .map_err(io::Error::other)
}

#[cfg(not(windows))]
fn replace_into_place(tmp: &Path, path: &Path) -> io::Result<()> {
    fs::set_permissions(tmp, fs::metadata(path)?.permissions())?;
    fs::rename(tmp, path)
}

/// Undo one replace. Refused if the file moved, a folder on its path became a link, or it
/// changed since the replace; otherwise the backup bytes are published over it the same way
/// as a replace (validation, locked read, fingerprint, temp file, stamp check last) and the
/// backup is deleted.
pub fn undo(rec: &ReplaceUndo) -> Result<()> {
    undo_with_hook(rec, &|_| {})
}

/// [`undo`] with a hook that runs just before the final checks. For tests.
fn undo_with_hook(rec: &ReplaceUndo, before_publish: &dyn Fn(&Path)) -> Result<()> {
    let target = rec.root.join(&rec.rel);
    let refused = |why: &str| CoreError::Invalid(format!("{}: {why}", target.display()));
    if !background_read_allowed(&target) {
        return Err(refused("is an online-only file"));
    }
    if !location_intact_at(&rec.root, &rec.rel) {
        return Err(refused("moved, or a folder on its path is now a link"));
    }
    let modified = || refused("has been modified since the replace");
    let mut handle = open_locked(&target).map_err(|e| CoreError::io(&target, e))?;
    let before = handle_stamp(&handle).map_err(|e| CoreError::io(&target, e))?;
    if before.links > 1 {
        return Err(refused("is hard-linked"));
    }
    let current = match read_bounded(&mut handle, rec.written.size) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Err(modified()),
        Err(e) => return Err(CoreError::io(&target, e)),
    };
    if fingerprint(&current) != rec.written {
        return Err(modified());
    }
    let original = fs::read(&rec.backup).map_err(|e| CoreError::io(&rec.backup, e))?;
    let checks = || {
        if !location_intact_at(&rec.root, &rec.rel) {
            return Some(Outcome::Skipped(SkipReason::PathChanged));
        }
        before_publish(&target);
        None
    };
    match publish(&target, handle, &before, &original, checks)? {
        Published::Done => {}
        Published::Stopped(_) => {
            return Err(refused("moved, or a folder on its path is now a link"));
        }
        Published::Changed => return Err(modified()),
    }
    fs::remove_file(&rec.backup).map_err(|e| CoreError::io(&rec.backup, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_jobs::{CancelToken, Progress};
    use std::fs;

    fn spec(pattern: &str, replacement: &str) -> SearchSpec {
        SearchSpec {
            pattern: pattern.into(),
            replacement: replacement.into(),
            ..Default::default()
        }
    }

    fn run_scan(root: &Path, spec: &SearchSpec, opts: &ScanOptions) -> ScanReport {
        let cancel = CancelToken::new();
        let progress = Progress::new();
        let ctx = JobCtx::new(&cancel, &progress);
        scan(&[root.to_path_buf()], spec, opts, &ctx).unwrap()
    }

    fn run_apply(files: &[FilePreview], spec: &SearchSpec, backup: &Path) -> ApplyReport {
        let cancel = CancelToken::new();
        let progress = Progress::new();
        let ctx = JobCtx::new(&cancel, &progress);
        apply(files, spec, backup, &ctx).unwrap()
    }

    fn skip_reason<'a>(report: &'a ScanReport, name: &str) -> Option<&'a SkipReason> {
        report
            .skipped
            .iter()
            .find(|(p, _)| p.file_name().and_then(|n| n.to_str()) == Some(name))
            .map(|(_, r)| r)
    }

    fn names(report: &ScanReport) -> Vec<String> {
        let mut v: Vec<String> = report.files.iter().map(|f| file_name_of(&f.path)).collect();
        v.sort();
        v
    }

    fn with_prefix(prefix: &[u8], body: &str) -> Vec<u8> {
        let mut v = prefix.to_vec();
        v.extend_from_slice(body.as_bytes());
        v
    }

    fn utf16_with_bom(text: &str, big_endian: bool) -> Vec<u8> {
        let mut out = if big_endian {
            vec![0xFE, 0xFF]
        } else {
            vec![0xFF, 0xFE]
        };
        for unit in text.encode_utf16() {
            let bytes = if big_endian {
                unit.to_be_bytes()
            } else {
                unit.to_le_bytes()
            };
            out.extend_from_slice(&bytes);
        }
        out
    }

    #[test]
    fn round_trip_keeps_encoding_and_line_endings() {
        let bom = [0xEF, 0xBB, 0xBF];
        let cases = vec![
            (
                "utf8 lf",
                b"alpha foo\nbeta foo\n".to_vec(),
                b"alpha bar\nbeta bar\n".to_vec(),
                Encoding::Utf8,
                LineEnding::Lf,
            ),
            (
                "utf8 bom crlf",
                with_prefix(&bom, "alpha foo\r\nbeta foo\r\n"),
                with_prefix(&bom, "alpha bar\r\nbeta bar\r\n"),
                Encoding::Utf8Bom,
                LineEnding::CrLf,
            ),
            (
                "utf16 le crlf",
                utf16_with_bom("alpha foo\r\nbeta foo\r\n", false),
                utf16_with_bom("alpha bar\r\nbeta bar\r\n", false),
                Encoding::Utf16Le,
                LineEnding::CrLf,
            ),
            (
                "utf16 be lf",
                utf16_with_bom("alpha foo\nbeta foo\n", true),
                utf16_with_bom("alpha bar\nbeta bar\n", true),
                Encoding::Utf16Be,
                LineEnding::Lf,
            ),
        ];
        for (name, original, expected, encoding, eol) in cases {
            let dir = tempfile::tempdir().unwrap();
            let backups = tempfile::tempdir().unwrap();
            let path = dir.path().join("doc.txt");
            fs::write(&path, &original).unwrap();

            let decoded = decode(&original).expect(name);
            assert_eq!(decoded.encoding, encoding, "{name}");
            assert_eq!(decoded.line_ending, eol, "{name}");
            assert_eq!(encode(&decoded.text, decoded.encoding), original, "{name}");

            let s = spec("foo", "bar");
            let report = run_scan(dir.path(), &s, &ScanOptions::default());
            assert_eq!(report.files.len(), 1, "{name}");
            assert_eq!(report.total_matches(), 2, "{name}");
            let applied = run_apply(&report.files, &s, backups.path());
            assert_eq!(applied.replacements(), 2, "{name}");
            assert!(applied.failed.is_empty(), "{name}");
            assert_eq!(fs::read(&path).unwrap(), expected, "{name}");
        }
    }

    #[test]
    fn mixed_line_endings_are_preserved_and_inserted_newlines_follow_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();

        let mixed = dir.path().join("mixed.txt");
        fs::write(&mixed, b"a1\r\nb1\nc\r\n").unwrap();
        let s = spec("1", "2");
        let report = run_scan(&mixed, &s, &ScanOptions::default());
        assert_eq!(report.files[0].line_ending, LineEnding::Mixed);
        run_apply(&report.files, &s, backups.path());
        assert_eq!(fs::read(&mixed).unwrap(), b"a2\r\nb2\nc\r\n");

        let crlf = dir.path().join("crlf.txt");
        fs::write(&crlf, b"x,y\r\nz\r\n").unwrap();
        let s = spec(",", "\n");
        let report = run_scan(&crlf, &s, &ScanOptions::default());
        run_apply(&report.files, &s, backups.path());
        assert_eq!(fs::read(&crlf).unwrap(), b"x\r\ny\r\nz\r\n");
    }

    #[test]
    fn binary_files_and_large_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"hello\0world hello").unwrap();
        fs::write(dir.path().join("photo.CR3"), b"hello hello").unwrap();
        fs::write(dir.path().join("big.txt"), "hello ".repeat(100)).unwrap();

        let opts = ScanOptions {
            max_file_size: 100,
            ..Default::default()
        };
        let report = run_scan(dir.path(), &spec("hello", "bye"), &opts);
        assert!(report.files.is_empty());
        assert_eq!(skip_reason(&report, "a.txt"), Some(&SkipReason::Binary));
        assert_eq!(
            skip_reason(&report, "photo.CR3"),
            Some(&SkipReason::BinaryExtension)
        );
        assert_eq!(
            skip_reason(&report, "big.txt"),
            Some(&SkipReason::TooLarge(600))
        );
    }

    #[test]
    fn regex_captures_case_and_whole_word() {
        let replaced = |text: &str, s: &SearchSpec| {
            let re = s.compile().unwrap();
            replace_text(text, &re, s, LineEnding::Lf)
        };

        let captures = SearchSpec {
            pattern: r"(\d+)-(\d+)".into(),
            replacement: "$2-$1".into(),
            regex: true,
            ..Default::default()
        };
        assert_eq!(
            replaced("1-2 and 33-44", &captures),
            ("2-1 and 44-33".to_string(), 2)
        );

        let cat = spec("cat", "dog");
        assert_eq!(
            replaced("Cat cat CAT", &cat),
            ("dog dog dog".to_string(), 3)
        );
        let cs = SearchSpec {
            case_sensitive: true,
            ..cat.clone()
        };
        assert_eq!(replaced("Cat dog CAT", &cs).1, 0);
        assert_eq!(replaced("Cat cat CAT", &cs), ("Cat dog CAT".to_string(), 1));

        let whole = SearchSpec {
            whole_word: true,
            ..cat.clone()
        };
        assert_eq!(
            replaced("concatenate cat. Cat", &whole),
            ("concatenate dog. dog".to_string(), 2)
        );

        let literal = spec("a.b", "$1");
        assert_eq!(replaced("a.b axb", &literal), ("$1 axb".to_string(), 1));

        let bad = SearchSpec {
            pattern: "(".into(),
            regex: true,
            ..Default::default()
        };
        assert!(matches!(bad.compile(), Err(CoreError::Invalid(_))));
        assert!(matches!(
            spec("", "x").compile(),
            Err(CoreError::Invalid(_))
        ));
    }

    #[test]
    fn include_exclude_and_non_recursive() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("a.txt"), "hit").unwrap();
        fs::write(dir.path().join("b.md"), "hit").unwrap();
        fs::write(dir.path().join("sub/c.txt"), "hit").unwrap();
        fs::write(dir.path().join("sub/d.log"), "hit").unwrap();
        let s = spec("hit", "x");

        let opts = ScanOptions {
            include: ScanOptions::parse_patterns("*.TXT; *.md"),
            ..Default::default()
        };
        assert_eq!(
            names(&run_scan(dir.path(), &s, &opts)),
            ["a.txt", "b.md", "c.txt"]
        );

        let opts = ScanOptions {
            include: ScanOptions::parse_patterns("*.txt"),
            exclude: ScanOptions::parse_patterns("c.*"),
            ..Default::default()
        };
        assert_eq!(names(&run_scan(dir.path(), &s, &opts)), ["a.txt"]);

        let opts = ScanOptions {
            recursive: false,
            ..Default::default()
        };
        assert_eq!(names(&run_scan(dir.path(), &s, &opts)), ["a.txt", "b.md"]);
    }

    #[test]
    fn preview_lines_and_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.txt");
        fs::write(&path, "foo bar foo\nxx\nfoo").unwrap();
        let report = run_scan(dir.path(), &spec("foo", "bazz"), &ScanOptions::default());
        let file = &report.files[0];
        assert_eq!(file.match_count, 3);
        assert_eq!(file.matches.len(), 2);

        let first = &file.matches[0];
        assert_eq!(first.line_number, 1);
        assert_eq!(first.line, "foo bar foo");
        assert_eq!(first.ranges, vec![0..3, 8..11]);
        assert_eq!(first.replaced, "bazz bar bazz");
        assert_eq!(first.replaced_ranges, vec![0..4, 9..13]);

        let second = &file.matches[1];
        assert_eq!(second.line_number, 3);
        assert_eq!(second.line, "foo");
        assert_eq!(second.ranges, vec![0..3]);
        assert_eq!(second.replaced, "bazz");
        assert_eq!(second.replaced_ranges, vec![0..4]);
    }

    #[test]
    fn atomic_write_keeps_creation_time_and_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.txt");
        fs::write(&path, b"foo one").unwrap();
        #[cfg(windows)]
        let created_before = {
            let status = std::process::Command::new("attrib")
                .arg("+h")
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success());
            fs::metadata(&path).unwrap().created().unwrap()
        };

        let s = spec("foo", "bar");
        let report = run_scan(&path, &s, &ScanOptions::default());
        let applied = run_apply(&report.files, &s, backups.path());
        assert_eq!(applied.replacements(), 1);
        assert_eq!(fs::read(&path).unwrap(), b"bar one");

        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);

        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
            let md = fs::metadata(&path).unwrap();
            assert_eq!(md.created().unwrap(), created_before);
            assert_ne!(md.file_attributes() & FILE_ATTRIBUTE_HIDDEN, 0);
        }
    }

    #[test]
    fn file_changed_after_preview_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.txt");
        fs::write(&path, b"foo one").unwrap();
        let s = spec("foo", "bar");
        let report = run_scan(&path, &s, &ScanOptions::default());

        fs::write(&path, b"foo two, edited").unwrap();
        let applied = run_apply(&report.files, &s, backups.path());
        assert!(matches!(
            applied.skipped.as_slice(),
            [(_, SkipReason::Changed)]
        ));
        assert_eq!(applied.replacements(), 0);
        assert_eq!(fs::read(&path).unwrap(), b"foo two, edited");
    }

    #[test]
    fn undo_restores_original_and_refuses_after_edit() {
        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.txt");
        let s = spec("foo", "bar");

        fs::write(&path, b"orig foo").unwrap();
        let report = run_scan(&path, &s, &ScanOptions::default());
        let applied = run_apply(&report.files, &s, backups.path());
        assert_eq!(applied.undo.len(), 1);
        let rec = &applied.undo[0];
        assert!(rec.backup.exists());
        assert_eq!(fs::read(&path).unwrap(), b"orig bar");

        undo(rec).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"orig foo");
        assert!(!rec.backup.exists());

        let backups2 = tempfile::tempdir().unwrap();
        let report = run_scan(&path, &s, &ScanOptions::default());
        let applied = run_apply(&report.files, &s, backups2.path());
        let rec = &applied.undo[0];
        fs::write(&path, b"edited by user").unwrap();
        assert!(undo(rec).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"edited by user");
        assert!(rec.backup.exists());
    }

    #[test]
    fn online_only_file_is_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud.txt");
        fs::write(&path, b"foo").unwrap();
        crate::cloud::simulate_online_only(path.clone());

        let report = run_scan(dir.path(), &spec("foo", "bar"), &ScanOptions::default());
        crate::cloud::clear_simulated_online_only(&path);

        assert!(report.files.is_empty());
        assert_eq!(
            skip_reason(&report, "cloud.txt"),
            Some(&SkipReason::OnlineOnly)
        );
    }

    // The read-only flag is Windows-only in effect; clearing it lets the tempdir be removed.
    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn read_only_match_is_skipped_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.txt");
        fs::write(&path, b"foo").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&path, perms.clone()).unwrap();

        let report = run_scan(dir.path(), &spec("foo", "bar"), &ScanOptions::default());
        assert!(report.files.is_empty());
        assert_eq!(skip_reason(&report, "ro.txt"), Some(&SkipReason::ReadOnly));
        assert_eq!(fs::read(&path).unwrap(), b"foo");

        perms.set_readonly(false);
        fs::set_permissions(&path, perms).unwrap();
    }

    #[test]
    fn cancelled_scan_returns_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"foo").unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        let progress = Progress::new();
        let ctx = JobCtx::new(&cancel, &progress);
        let r = scan(
            &[dir.path().to_path_buf()],
            &spec("foo", "bar"),
            &ScanOptions::default(),
            &ctx,
        );
        assert!(matches!(r, Err(CoreError::Cancelled)));
    }

    /// Attributes that can never be read: every file must be treated as online-only.
    struct NoAttributes;

    impl AttributeSource for NoAttributes {
        fn attributes(&self, _path: &Path) -> Option<u32> {
            None
        }
    }

    #[test]
    fn unreadable_attributes_fail_closed_in_scan() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"foo").unwrap();
        let cancel = CancelToken::new();
        let progress = Progress::new();
        let ctx = JobCtx::new(&cancel, &progress);
        let s = spec("foo", "bar");
        let report = scan_with(
            &[dir.path().to_path_buf()],
            &s,
            &ScanOptions::default(),
            &ctx,
            &NoAttributes,
        )
        .unwrap();
        assert!(report.files.is_empty());
        // The gate returns before the file is opened, so no read error is counted either.
        assert_eq!(skip_reason(&report, "a.txt"), Some(&SkipReason::OnlineOnly));
        assert_eq!(progress.snapshot().errors, 0);

        // The same file is searched when the attributes are readable.
        let report = run_scan(dir.path(), &s, &ScanOptions::default());
        assert_eq!(report.files.len(), 1);
    }

    #[test]
    fn only_known_text_types_are_searched_unless_named_in_include() {
        assert!(is_known_text_extension(Path::new("A.TXT")));
        assert!(is_known_text_extension(Path::new("Dockerfile")));
        assert!(is_known_text_extension(Path::new(".gitignore")));
        assert!(!is_known_text_extension(Path::new("photo.cr3")));
        assert!(!is_known_text_extension(Path::new("Makefile")));
        assert!(!is_known_text_extension(Path::new("notes")));

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "hit").unwrap();
        fs::write(dir.path().join("sample.xyz"), "hit").unwrap();
        fs::write(dir.path().join("Makefile"), "hit").unwrap();
        fs::write(dir.path().join("Dockerfile"), "hit").unwrap();
        let s = spec("hit", "x");

        let report = run_scan(dir.path(), &s, &ScanOptions::default());
        assert_eq!(names(&report), ["Dockerfile", "notes.txt"]);
        assert_eq!(
            skip_reason(&report, "sample.xyz"),
            Some(&SkipReason::UnknownType)
        );
        assert_eq!(
            skip_reason(&report, "Makefile"),
            Some(&SkipReason::UnknownType)
        );

        // Naming the type in the include list searches it, if it decodes as text.
        let opts = ScanOptions {
            include: ScanOptions::parse_patterns("*.xyz"),
            ..Default::default()
        };
        let report = run_scan(dir.path(), &s, &opts);
        assert_eq!(names(&report), ["sample.xyz"]);
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn preview_keeps_counts_past_the_stored_line_cap() {
        let dir = tempfile::tempdir().unwrap();
        let lines = MAX_PREVIEW_LINES + 100;
        fs::write(dir.path().join("many.txt"), "foo foo\n".repeat(lines)).unwrap();
        let report = run_scan(dir.path(), &spec("foo", "bar"), &ScanOptions::default());
        let file = &report.files[0];
        assert_eq!(file.match_count, 2 * lines);
        assert_eq!(file.lines_with_matches, lines);
        assert_eq!(file.matches.len(), MAX_PREVIEW_LINES);
        assert_eq!(file.matches[0].ranges.len(), 2);
        assert_eq!(file.matches.last().unwrap().line_number, MAX_PREVIEW_LINES);
    }

    #[test]
    fn cancel_inside_a_file_leaves_it_untouched_and_stops_the_job() {
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.txt");
        let second = dir.path().join("b.txt");
        fs::write(&first, b"foo one").unwrap();
        fs::write(&second, b"foo two").unwrap();
        let s = spec("foo", "bar");
        let mut files = run_scan(dir.path(), &s, &ScanOptions::default()).files;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(files.len(), 2);

        let cancel = CancelToken::new();
        let progress = Progress::new();
        let ctx = JobCtx::new(&cancel, &progress);
        let hook_calls = AtomicUsize::new(0);
        let hook = |_: &Path| {
            hook_calls.fetch_add(1, Ordering::Relaxed);
            cancel.cancel();
        };
        let report = apply_with_hook(&files, &s, backups.path(), &ctx, &hook).unwrap();

        assert!(report.cancelled);
        assert!(report.undo.is_empty());
        assert_eq!(
            hook_calls.load(Ordering::Relaxed),
            1,
            "the second file was not reached"
        );
        assert_eq!(fs::read(&first).unwrap(), b"foo one");
        assert_eq!(fs::read(&second).unwrap(), b"foo two");
        assert!(
            fs::read_dir(backups.path()).unwrap().next().is_none(),
            "no backup left behind"
        );
        let temps = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temps, 0, "no temp file left behind");
    }

    /// A folder on the previewed path replaced by a junction must not redirect the write.
    #[cfg(windows)]
    #[test]
    fn folder_replaced_by_a_junction_after_preview_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("doc.txt"), b"foo inside").unwrap();
        let victim = outside.path().join("doc.txt");
        fs::write(&victim, b"foo outside").unwrap();
        let s = spec("foo", "bar");
        let report = run_scan(dir.path(), &s, &ScanOptions::default());
        assert_eq!(report.files.len(), 1);

        fs::remove_dir_all(&sub).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&sub)
            .arg(outside.path())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed");

        let applied = run_apply(&report.files, &s, backups.path());
        assert!(
            matches!(applied.skipped.as_slice(), [(_, SkipReason::PathChanged)]),
            "{:?}",
            applied.skipped
        );
        assert_eq!(applied.replacements(), 0);
        assert!(applied.failed.is_empty());
        assert_eq!(fs::read(&victim).unwrap(), b"foo outside");

        // Remove the junction itself; its target is untouched.
        fs::remove_dir(&sub).unwrap();
    }

    fn temp_files_in(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count()
    }

    #[test]
    fn hard_linked_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let linked = dir.path().join("linked.txt");
        fs::write(&linked, b"foo linked").unwrap();
        fs::hard_link(&linked, dir.path().join("twin.txt")).unwrap();
        let s = spec("foo", "bar");
        let report = run_scan(dir.path(), &s, &ScanOptions::default());
        assert_eq!(
            skip_reason(&report, "linked.txt"),
            Some(&SkipReason::HardLinked)
        );
        assert!(report.files.is_empty());

        // A link made after the preview is caught by apply.
        let single = dir.path().join("single.txt");
        fs::write(&single, b"foo single").unwrap();
        let report = run_scan(&single, &s, &ScanOptions::default());
        assert_eq!(report.files.len(), 1);
        fs::hard_link(&single, dir.path().join("late-twin.txt")).unwrap();
        let applied = run_apply(&report.files, &s, backups.path());
        assert!(matches!(
            applied.skipped.as_slice(),
            [(_, SkipReason::HardLinked)]
        ));
        assert_eq!(fs::read(&single).unwrap(), b"foo single");
        assert_eq!(fs::read(&linked).unwrap(), b"foo linked");
    }

    #[test]
    fn an_edit_at_the_last_moment_is_caught_by_the_final_stamp_check() {
        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let file = dir.path().join("doc.txt");
        fs::write(&file, b"foo text").unwrap();
        let s = spec("foo", "bar");
        let files = run_scan(dir.path(), &s, &ScanOptions::default()).files;
        let cancel = CancelToken::new();
        let progress = Progress::new();
        let ctx = JobCtx::new(&cancel, &progress);
        // The hook runs after the path checks and before the stamp check.
        let hook = |path: &Path| {
            let mut f = OpenOptions::new().append(true).open(path).unwrap();
            f.write_all(b" edited").unwrap();
        };
        let report = apply_with_hook(&files, &s, backups.path(), &ctx, &hook).unwrap();
        assert!(matches!(
            report.skipped.as_slice(),
            [(_, SkipReason::Changed)]
        ));
        assert_eq!(fs::read(&file).unwrap(), b"foo text edited");
        assert_eq!(temp_files_in(dir.path()), 0);
        assert!(fs::read_dir(backups.path()).unwrap().next().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn a_failed_replace_reports_the_file_and_leaves_it_untouched() {
        use std::cell::RefCell;
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let file = dir.path().join("doc.txt");
        fs::write(&file, b"foo text").unwrap();
        let s = spec("foo", "bar");
        let files = run_scan(dir.path(), &s, &ScanOptions::default()).files;
        let cancel = CancelToken::new();
        let progress = Progress::new();
        let ctx = JobCtx::new(&cancel, &progress);
        // A handle without delete sharing makes ReplaceFileW fail; there is no rename fallback.
        let held = RefCell::new(None);
        let hook = |path: &Path| {
            let f = OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
                .open(path)
                .unwrap();
            *held.borrow_mut() = Some(f);
        };
        let report = apply_with_hook(&files, &s, backups.path(), &ctx, &hook).unwrap();
        drop(held.borrow_mut().take());
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(report.undo.is_empty());
        assert_eq!(fs::read(&file).unwrap(), b"foo text");
        assert_eq!(temp_files_in(dir.path()), 0);
        assert!(fs::read_dir(backups.path()).unwrap().next().is_none());
    }

    #[test]
    fn undo_refuses_an_edit_at_the_last_moment() {
        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let file = dir.path().join("doc.txt");
        fs::write(&file, b"foo text").unwrap();
        let s = spec("foo", "bar");
        let files = run_scan(dir.path(), &s, &ScanOptions::default()).files;
        let applied = run_apply(&files, &s, backups.path());
        let rec = &applied.undo[0];
        let hook = |path: &Path| {
            let mut f = OpenOptions::new().append(true).open(path).unwrap();
            f.write_all(b" edited").unwrap();
        };
        assert!(undo_with_hook(rec, &hook).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"bar text edited");
        assert!(
            rec.backup.exists(),
            "the backup is kept when undo is refused"
        );
        assert_eq!(temp_files_in(dir.path()), 0);
    }

    #[cfg(windows)]
    #[test]
    fn undo_refuses_when_a_folder_became_a_junction() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("doc.txt"), b"foo inside").unwrap();
        let s = spec("foo", "bar");
        let files = run_scan(dir.path(), &s, &ScanOptions::default()).files;
        let applied = run_apply(&files, &s, backups.path());
        assert_eq!(applied.undo.len(), 1);

        // Put the replaced bytes outside and point `sub` at them through a junction.
        let victim = outside.path().join("doc.txt");
        fs::write(&victim, b"bar inside").unwrap();
        fs::remove_dir_all(&sub).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&sub)
            .arg(outside.path())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed");

        assert!(undo(&applied.undo[0]).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"bar inside");
        fs::remove_dir(&sub).unwrap();
    }

    #[test]
    fn preview_lines_share_one_budget_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let per_file = MAX_PREVIEW_LINES;
        let files = MAX_PREVIEW_TOTAL_LINES / per_file + 3;
        let body = "foo\n".repeat(per_file);
        for i in 0..files {
            fs::write(dir.path().join(format!("f{i:04}.txt")), &body).unwrap();
        }
        let report = run_scan(dir.path(), &spec("foo", "bar"), &ScanOptions::default());
        assert_eq!(report.files.len(), files);
        let stored: usize = report.files.iter().map(|f| f.matches.len()).sum();
        assert_eq!(stored, MAX_PREVIEW_TOTAL_LINES);
        assert!(report.files.iter().all(|f| f.match_count == per_file));
        assert!(
            report
                .files
                .iter()
                .all(|f| f.lines_with_matches == per_file)
        );
    }

    fn regex_spec(pattern: &str, replacement: &str) -> SearchSpec {
        SearchSpec {
            regex: true,
            case_sensitive: true,
            ..spec(pattern, replacement)
        }
    }

    fn shown(file: &FilePreview) -> usize {
        file.matches.iter().map(|m| m.ranges.len()).sum()
    }

    #[test]
    fn a_huge_single_line_stores_capped_matches_and_reports_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let len = 1024 * 1024;
        fs::write(dir.path().join("huge.txt"), "a".repeat(len)).unwrap();
        let report = run_scan(dir.path(), &spec("a", "b"), &ScanOptions::default());
        let file = &report.files[0];
        assert_eq!(file.match_count, len);
        assert_eq!(file.lines_with_matches, 1);
        assert_eq!(shown(file), MAX_PREVIEW_HITS_PER_LINE);
        assert_eq!(file.hidden_matches, len - MAX_PREVIEW_HITS_PER_LINE);
        for m in &file.matches {
            assert!(m.line.len() <= DISPLAY_MAX && m.replaced.len() <= DISPLAY_MAX);
            assert_eq!(m.line_number, 1);
        }
        let notice = report.truncation_notice().unwrap();
        assert!(
            notice.contains(&format!("{} more matches not shown", file.hidden_matches)),
            "{notice}"
        );
    }

    #[test]
    fn matches_stored_are_capped_per_file_and_per_scan() {
        let dir = tempfile::tempdir().unwrap();
        // 200 lines of 50 matches: 10,000 matches per file.
        let body = format!("{}\n", "foo ".repeat(50)).repeat(200);
        let files = MAX_PREVIEW_TOTAL_HITS / MAX_PREVIEW_HITS_PER_FILE + 2;
        for i in 0..files {
            fs::write(dir.path().join(format!("f{i:03}.txt")), &body).unwrap();
        }
        let report = run_scan(dir.path(), &spec("foo", "bar"), &ScanOptions::default());
        assert_eq!(report.files.len(), files);
        let first = &report.files[0];
        assert_eq!(first.match_count, 10_000);
        assert_eq!(shown(first), MAX_PREVIEW_HITS_PER_FILE);
        assert_eq!(first.matches.len(), MAX_PREVIEW_HITS_PER_FILE / 50);
        let stored: usize = report.files.iter().map(shown).sum();
        assert_eq!(stored, MAX_PREVIEW_TOTAL_HITS);
        assert!(
            report
                .files
                .iter()
                .all(|f| shown(f) + f.hidden_matches == f.match_count)
        );
        assert_eq!(
            report.hidden_matches(),
            files * 10_000 - MAX_PREVIEW_TOTAL_HITS
        );
    }

    #[test]
    fn a_scan_stops_at_its_file_and_byte_limits_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            fs::write(dir.path().join(format!("f{i}.txt")), "foo\n").unwrap();
        }
        let opts = ScanOptions {
            max_total_files: 2,
            ..ScanOptions::default()
        };
        let report = run_scan(dir.path(), &spec("foo", "bar"), &opts);
        assert_eq!(report.files_scanned, 2);
        assert_eq!(report.files.len(), 2);
        assert_eq!(report.files_not_scanned, 3);
        assert!(
            report
                .truncation_notice()
                .unwrap()
                .contains("3 files not searched")
        );

        let opts = ScanOptions {
            max_total_bytes: 10,
            ..ScanOptions::default()
        };
        let report = run_scan(dir.path(), &spec("foo", "bar"), &opts);
        assert_eq!(report.files_scanned, 2);
        assert_eq!(report.files_not_scanned, 3);

        let report = run_scan(dir.path(), &spec("foo", "bar"), &ScanOptions::default());
        assert_eq!(report.files_not_scanned, 0);
        assert_eq!(report.truncation_notice(), None);
    }

    #[test]
    fn a_multi_line_match_is_shown_in_full() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("m.txt"),
            "x BEGIN one\ntwo END y BEGIN z END\nnext\r\nBEGIN crlf\r\nEND.\n",
        )
        .unwrap();
        let report = run_scan(
            dir.path(),
            &regex_spec(r"(?s)BEGIN.*?END", "R"),
            &ScanOptions::default(),
        );
        let file = &report.files[0];
        assert_eq!(file.match_count, 3);
        assert_eq!(file.hidden_matches, 0);
        assert_eq!(file.matches.len(), 2);

        // The second match starts on the line where the first ends: both are shown together.
        let first = &file.matches[0];
        assert_eq!(first.line_number, 1);
        assert_eq!(first.line, r"x BEGIN one\ntwo END y BEGIN z END");
        let spans: Vec<&str> = first
            .ranges
            .iter()
            .map(|r| &first.line[r.clone()])
            .collect();
        assert_eq!(spans, [r"BEGIN one\ntwo END", "BEGIN z END"]);
        assert_eq!(first.replaced, "x R y R");
        assert_eq!(first.replaced_ranges, vec![2..3, 6..7]);

        let second = &file.matches[1];
        assert_eq!(second.line_number, 4);
        assert_eq!(second.line, r"BEGIN crlf\nEND.");
        assert_eq!(&second.line[second.ranges[0].clone()], r"BEGIN crlf\nEND");
        assert_eq!(second.replaced, "R.");
        assert_eq!(report.truncation_notice(), None);
    }

    #[test]
    fn a_newline_only_match_is_shown() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("n.txt"), "ab\ncd\r\nef").unwrap();
        let report = run_scan(dir.path(), &regex_spec(r"\n", " "), &ScanOptions::default());
        let file = &report.files[0];
        assert_eq!(file.match_count, 2);
        assert_eq!(file.hidden_matches, 0);
        assert_eq!(file.matches.len(), 2);

        let first = &file.matches[0];
        assert_eq!(first.line_number, 1);
        assert_eq!(first.line, r"ab\n");
        assert_eq!(first.ranges, vec![2..4]);
        assert_eq!(first.replaced, "ab ");

        // Only the `\n` of a CRLF break matches; the `\r` that stays is shown too.
        let second = &file.matches[1];
        assert_eq!(second.line_number, 2);
        assert_eq!(second.line, r"cd\r\n");
        assert_eq!(&second.line[second.ranges[0].clone()], r"\n");
        assert_eq!(second.replaced, r"cd\r ");
    }

    #[test]
    fn a_match_too_long_to_show_is_reported_as_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "BEGIN\n{}\nEND\nshort BEGIN END",
            "x\n".repeat(MAX_SHOWN_MATCH)
        );
        fs::write(dir.path().join("l.txt"), body).unwrap();
        let report = run_scan(
            dir.path(),
            &regex_spec(r"(?s)BEGIN.*?END", "R"),
            &ScanOptions::default(),
        );
        let file = &report.files[0];
        assert_eq!(file.match_count, 2);
        assert_eq!(file.hidden_matches, 1);
        assert_eq!(shown(file), 1);
        assert_eq!(file.matches[0].line, "short BEGIN END");
        assert!(
            report
                .truncation_notice()
                .unwrap()
                .contains("1 more match not shown")
        );
    }

    #[test]
    fn matches_far_apart_on_a_long_line_get_their_own_windows() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("foo{}foo{}", "x".repeat(1000), "y".repeat(1000));
        fs::write(dir.path().join("w.txt"), body).unwrap();
        let report = run_scan(dir.path(), &spec("foo", "bar"), &ScanOptions::default());
        let file = &report.files[0];
        assert_eq!(file.hidden_matches, 0);
        assert_eq!(file.matches.len(), 2);
        assert_eq!(file.lines_with_matches, 1);
        for m in &file.matches {
            assert!(m.line.len() <= DISPLAY_MAX);
            assert_eq!(&m.line[m.ranges[0].clone()], "foo");
            assert_eq!(&m.replaced[m.replaced_ranges[0].clone()], "bar");
        }
        assert!(
            file.matches[1]
                .line
                .starts_with(&"x".repeat(DISPLAY_CONTEXT))
        );
    }
}
