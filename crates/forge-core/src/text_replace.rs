//! Find and replace text in files, with a preview and an undoable, atomic replace.
//!
//! * [`scan`] walks the roots and returns a [`ScanReport`] of matching files, each with its
//!   matching lines and a fingerprint of the bytes it saw.
//! * [`apply`] replaces in those files, but only where the bytes still match the fingerprint,
//!   keeps the original bytes in a backup file and writes the result with [`write_atomic`].
//! * [`undo`] restores a file from its backup, unless it was edited since.
//!
//! Files are decoded (UTF-8, UTF-8 with BOM, UTF-16 with BOM) and searched as whole text, so
//! line endings and the encoding survive the replace byte-for-byte. Online-only cloud files
//! are never read, and binary files are skipped.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::ops::{JobCtx, WalkItem, walk_tree};
use crate::cloud::background_read_allowed;
use crate::query::glob_match;

/// Files larger than this are skipped by [`scan`] unless the caller raises the limit.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;

/// Longest line shown in a [`LineMatch`]; longer lines are cut to a window around the match.
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
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            recursive: true,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
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

/// One line of a file that contains at least one match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineMatch {
    /// 1-based line where the match starts.
    pub line_number: usize,
    /// That line without its line ending. Cut to a window of about 400 bytes for long lines.
    pub line: String,
    /// Byte ranges of the matches inside `line`. A match continuing past the line end is
    /// clipped to the line end.
    pub ranges: Vec<Range<usize>>,
    /// The line after replacement (single-line view; line breaks in the replacement show as
    /// `\n`).
    pub replaced: String,
    /// Byte ranges of the inserted replacement text inside `replaced`.
    pub replaced_ranges: Vec<Range<usize>>,
}

/// A file with at least one match, as found by [`scan`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePreview {
    pub path: PathBuf,
    pub encoding: Encoding,
    pub line_ending: LineEnding,
    /// One entry per line that has at least one match.
    pub matches: Vec<LineMatch>,
    pub match_count: usize,
    /// Identity of the bytes that were searched. [`apply`] refuses files that no longer match.
    pub fingerprint: Fingerprint,
}

/// Why a file was not searched or not changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    /// Known binary extension; never read.
    BinaryExtension,
    /// Contains NUL bytes or is not decodable text.
    Binary,
    /// Larger than the size limit (bytes).
    TooLarge(u64),
    /// Online-only cloud file; reading it would download it.
    OnlineOnly,
    /// Read-only; matches are not changed.
    ReadOnly,
    /// Changed after the preview was made.
    Changed,
    /// Could not be read; the message says why.
    Unreadable(String),
}

impl SkipReason {
    pub fn describe(&self) -> String {
        match self {
            Self::BinaryExtension => "Binary file type, not searched".into(),
            Self::Binary => "Not a text file".into(),
            Self::TooLarge(n) => format!("Too large to search ({n} bytes)"),
            Self::OnlineOnly => "Online-only cloud file, not downloaded".into(),
            Self::ReadOnly => "Read-only file, not changed".into(),
            Self::Changed => "Changed since the preview, not changed".into(),
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
}

impl ScanReport {
    pub fn total_matches(&self) -> usize {
        self.files.iter().map(|f| f.match_count).sum()
    }
}

/// Record of one replaced file, enough to undo it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceUndo {
    pub path: PathBuf,
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
    for cand in &col.out {
        ctx.cancel.check()?;
        ctx.progress.set_current(cand.path.display().to_string());
        report.files_scanned += 1;
        scan_file(cand, &re, spec, opts, ctx, &mut report);
        ctx.progress.add_items(1);
    }
    Ok(report)
}

struct Candidate {
    path: PathBuf,
    size: u64,
    readonly: bool,
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

    fn push(&mut self, path: PathBuf, size: u64, readonly: bool) {
        let key = if cfg!(windows) {
            path.to_string_lossy().to_lowercase()
        } else {
            path.to_string_lossy().into_owned()
        };
        if self.seen.insert(key) {
            self.out.push(Candidate {
                path,
                size,
                readonly,
            });
        }
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
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
        col.push(root.to_path_buf(), md.len(), md.permissions().readonly());
        return Ok(());
    }
    if !md.is_dir() {
        return Ok(());
    }
    if opts.recursive {
        walk_tree(root, ctx.cancel, |item| match item {
            WalkItem::Entry(e) => {
                if e.is_file() && col.name_ok(&file_name_of(&e.path)) {
                    col.push(e.path.clone(), e.size, e.readonly);
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
                col.push(path, meta.len(), meta.permissions().readonly());
            }
        }
    }
    Ok(())
}

fn skip(report: &mut ScanReport, path: &Path, reason: SkipReason) {
    report.skipped.push((path.to_path_buf(), reason));
}

/// Checks run per file, in this order: placeholder, extension, size, read, decode, search.
fn scan_file(
    cand: &Candidate,
    re: &Regex,
    spec: &SearchSpec,
    opts: &ScanOptions,
    ctx: &JobCtx<'_>,
    report: &mut ScanReport,
) {
    let path = cand.path.as_path();
    if !background_read_allowed(path) {
        return skip(report, path, SkipReason::OnlineOnly);
    }
    if is_binary_extension(path) {
        return skip(report, path, SkipReason::BinaryExtension);
    }
    if cand.size > opts.max_file_size {
        return skip(report, path, SkipReason::TooLarge(cand.size));
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            ctx.progress.add_error();
            let msg = CoreError::io(path, e).to_string();
            return skip(report, path, SkipReason::Unreadable(msg));
        }
    };
    let Some(decoded) = decode(&bytes) else {
        return skip(report, path, SkipReason::Binary);
    };
    let (matches, match_count) = find_matches(&decoded.text, re, spec);
    if match_count == 0 {
        return;
    }
    if cand.readonly {
        return skip(report, path, SkipReason::ReadOnly);
    }
    report.files.push(FilePreview {
        path: cand.path.clone(),
        encoding: decoded.encoding,
        line_ending: decoded.line_ending,
        matches,
        match_count,
        fingerprint: fingerprint(&bytes),
    });
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

/// The matches on one line, before they are turned into a [`LineMatch`].
struct LineGroup {
    line_no: usize,
    line_start: usize,
    content_end: usize,
    /// Byte range inside the line, and the replacement text for that match.
    hits: Vec<(Range<usize>, String)>,
}

/// Group the matches of `text` by line. Returns the lines and the total match count.
fn find_matches(text: &str, re: &Regex, spec: &SearchSpec) -> (Vec<LineMatch>, usize) {
    let mut groups: Vec<LineGroup> = Vec::new();
    let mut lines = LineCursor::new(text);
    let mut count = 0;
    for caps in real_matches(text, re) {
        let Some(m) = caps.get(0) else { continue };
        count += 1;
        let (line_no, line_start, content_end) = lines.locate(m.start());
        // The match may run past the line end (multi-line pattern); clip it to the line.
        let start = m.start() - line_start;
        let end = m.end().min(content_end) - line_start;
        if end <= start {
            continue;
        }
        let rep = replacement_text(&caps, spec);
        match groups.last_mut() {
            Some(g) if g.line_start == line_start => g.hits.push((start..end, rep)),
            _ => groups.push(LineGroup {
                line_no,
                line_start,
                content_end,
                hits: vec![(start..end, rep)],
            }),
        }
    }
    let lines = groups.iter().map(|g| finish_line(text, g)).collect();
    (lines, count)
}

fn finish_line(text: &str, g: &LineGroup) -> LineMatch {
    let line = &text[g.line_start..g.content_end];
    let mut replaced = String::with_capacity(line.len());
    let mut replaced_ranges = Vec::new();
    let mut ranges = Vec::with_capacity(g.hits.len());
    let mut prev = 0;
    for (range, rep) in &g.hits {
        replaced.push_str(&line[prev..range.start]);
        let shown = one_line(rep);
        let begin = replaced.len();
        replaced.push_str(&shown);
        if !shown.is_empty() {
            replaced_ranges.push(begin..replaced.len());
        }
        ranges.push(range.clone());
        prev = range.end;
    }
    replaced.push_str(&line[prev..]);

    let (line, ranges) = window(line, &ranges);
    let (replaced, replaced_ranges) = window(&replaced, &replaced_ranges);
    LineMatch {
        line_number: g.line_no,
        line,
        ranges,
        replaced,
        replaced_ranges,
    }
}

/// Single-line display form of a replacement: line breaks become a visible `\n`.
fn one_line(rep: &str) -> String {
    rep.replace("\r\n", "\\n").replace('\n', "\\n")
}

/// Cut `text` to a window of about [`DISPLAY_MAX`] bytes around the first range, and shift
/// the ranges into the window. Short text is returned unchanged.
fn window(text: &str, ranges: &[Range<usize>]) -> (String, Vec<Range<usize>>) {
    if text.len() <= DISPLAY_MAX {
        return (text.to_string(), ranges.to_vec());
    }
    let first = ranges.first().map_or(0, |r| r.start);
    let start = floor_boundary(text, first.saturating_sub(DISPLAY_CONTEXT));
    let end = floor_boundary(text, start + DISPLAY_MAX);
    let shifted = ranges
        .iter()
        .filter_map(|r| {
            let s = r.start.max(start);
            let e = r.end.min(end);
            (s < e).then(|| s - start..e - start)
        })
        .collect();
    (text[start..end].to_string(), shifted)
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
}

/// Replace in each previewed file. A file is changed only if its bytes still match the preview
/// fingerprint; the original bytes go to a backup in `backup_dir` first. A failure on one file
/// is recorded and the rest continue. Errors only for a bad spec or an uncreatable backup dir.
pub fn apply(
    files: &[FilePreview],
    spec: &SearchSpec,
    backup_dir: &Path,
    ctx: &JobCtx<'_>,
) -> Result<ApplyReport> {
    let re = spec.compile()?;
    fs::create_dir_all(backup_dir).map_err(|e| CoreError::io(backup_dir, e))?;
    ctx.progress.add_total_items(files.len() as u64);

    let mut report = ApplyReport::default();
    for (index, file) in files.iter().enumerate() {
        if ctx.cancel.check().is_err() {
            report.cancelled = true;
            break;
        }
        ctx.progress.set_current(file.path.display().to_string());
        match replace_one(file, index, &re, spec, backup_dir) {
            Ok(Outcome::Replaced(undo)) => report.undo.push(undo),
            Ok(Outcome::Skipped(reason)) => report.skipped.push((file.path.clone(), reason)),
            Err(e) => {
                ctx.progress.add_error();
                report.failed.push((file.path.clone(), e.to_string()));
            }
        }
        ctx.progress.add_items(1);
    }
    Ok(report)
}

fn replace_one(
    file: &FilePreview,
    index: usize,
    re: &Regex,
    spec: &SearchSpec,
    backup_dir: &Path,
) -> Result<Outcome> {
    let path = file.path.as_path();
    if !background_read_allowed(path) {
        return Ok(Outcome::Skipped(SkipReason::OnlineOnly));
    }
    let md = fs::metadata(path).map_err(|e| CoreError::io(path, e))?;
    if md.permissions().readonly() {
        return Ok(Outcome::Skipped(SkipReason::ReadOnly));
    }
    let original = fs::read(path).map_err(|e| CoreError::io(path, e))?;
    if fingerprint(&original) != file.fingerprint {
        return Ok(Outcome::Skipped(SkipReason::Changed));
    }
    let Some(decoded) = decode(&original) else {
        return Ok(Outcome::Skipped(SkipReason::Binary));
    };
    let (text, count) = replace_text(&decoded.text, re, spec, decoded.line_ending);
    if count == 0 {
        return Ok(Outcome::Skipped(SkipReason::Changed));
    }
    let new_bytes = encode(&text, decoded.encoding);

    let backup = backup_dir.join(format!("{index:05}-{}", file_name_of(path)));
    write_new_file(&backup, &original).map_err(|e| CoreError::io(&backup, e))?;
    if let Err(e) = write_atomic(path, &new_bytes) {
        let _ = fs::remove_file(&backup);
        return Err(e);
    }
    Ok(Outcome::Replaced(ReplaceUndo {
        path: file.path.clone(),
        backup,
        written: fingerprint(&new_bytes),
        replacements: count,
    }))
}

fn write_new_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = File::create_new(path)?;
    f.write_all(bytes)?;
    f.sync_all()
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

/// Atomically replace the contents of the existing file `path` with `bytes`. The data goes to
/// a temp file in the same folder first, then replaces the target, so a crash leaves either
/// the old or the new content.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let perms = fs::metadata(path)
        .map_err(|e| CoreError::io(path, e))?
        .permissions();
    let (tmp, file) = create_temp(path)?;
    if let Err(e) = write_and_replace(file, &tmp, path, bytes, perms) {
        let _ = fs::remove_file(&tmp);
        return Err(CoreError::io(path, e));
    }
    Ok(())
}

fn write_and_replace(
    mut file: File,
    tmp: &Path,
    path: &Path,
    bytes: &[u8],
    perms: fs::Permissions,
) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    // Close the handle before the file is moved over the target.
    drop(file);
    replace_into_place(tmp, path, perms)
}

#[cfg(windows)]
fn replace_into_place(tmp: &Path, path: &Path, perms: fs::Permissions) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{REPLACEFILE_IGNORE_MERGE_ERRORS, ReplaceFileW};
    use windows::core::PCWSTR;

    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let target = wide(path);
    let source = wide(tmp);
    // SAFETY: both strings are NUL-terminated and outlive the call. The backup, exclude and
    // reserved arguments are null, which the API accepts.
    let done = unsafe {
        ReplaceFileW(
            PCWSTR(target.as_ptr()),
            PCWSTR(source.as_ptr()),
            PCWSTR::null(),
            REPLACEFILE_IGNORE_MERGE_ERRORS,
            None,
            None,
        )
    };
    match done {
        Ok(()) => Ok(()),
        // ReplaceFileW keeps creation time, attributes, ACLs and streams; if it fails (for
        // example on a filesystem without support), a plain rename still replaces the data.
        Err(_) => rename_over(tmp, path, perms),
    }
}

#[cfg(not(windows))]
fn replace_into_place(tmp: &Path, path: &Path, perms: fs::Permissions) -> io::Result<()> {
    rename_over(tmp, path, perms)
}

fn rename_over(tmp: &Path, path: &Path, perms: fs::Permissions) -> io::Result<()> {
    fs::set_permissions(tmp, perms)?;
    fs::rename(tmp, path)
}

/// Undo one replace. Refused if the file changed since the replace; otherwise the backup
/// bytes are written back and the backup is deleted.
pub fn undo(rec: &ReplaceUndo) -> Result<()> {
    if !background_read_allowed(&rec.path) {
        return Err(CoreError::Invalid(format!(
            "{}: is an online-only file",
            rec.path.display()
        )));
    }
    let current = fs::read(&rec.path).map_err(|e| CoreError::io(&rec.path, e))?;
    if fingerprint(&current) != rec.written {
        return Err(CoreError::Invalid(format!(
            "{} has been modified since the replace",
            rec.path.display()
        )));
    }
    let original = fs::read(&rec.backup).map_err(|e| CoreError::io(&rec.backup, e))?;
    write_atomic(&rec.path, &original)?;
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
}
