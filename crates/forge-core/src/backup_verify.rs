//! Backup check: compare an original folder with its backup by content (BLAKE3).
//!
//! Files are paired by relative path (case-insensitive on Windows, as in `compare`). Quick
//! mode trusts a matching size and modification time and hashes only the rest; Full mode
//! hashes every pair with equal sizes. Online-only cloud placeholders are never read, so they
//! are reported as "not checked" instead of being downloaded.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::cloud::background_read_allowed;
use crate::error::{CoreError, Result};
use crate::hashing::{Digest, hash_file};
use crate::ops::{JobCtx, NodeKind, WalkItem, walk_tree};
use crate::paths;

const LINK_REASON: &str = "link not followed";
const PLACEHOLDER_REASON: &str = "online-only file, not downloaded";
const CLASH_REASON: &str = "file-vs-folder clash";
const AMBIGUOUS_REASON: &str = "ambiguous: names that differ only in letter case";

/// Result of hashing one file: the digest, or the reason it could not be read.
type DigestResult = std::result::Result<Digest, String>;

/// How file pairs are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VerifyMode {
    /// Matching size and modification time (within the tolerance) count as a match without
    /// reading the data. Everything else is hashed.
    #[default]
    Quick,
    /// Every pair with equal sizes is hashed, whatever the timestamps say.
    Full,
}

/// Settings for [`verify_backup`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyOptions {
    pub mode: VerifyMode,
    /// Hash missing originals against same-size extra backup files, to find moved or renamed
    /// files. Slower: every candidate is read once.
    pub detect_moves: bool,
    /// Modification times closer than this count as equal (copy tools and FAT round times).
    pub mtime_tolerance: Duration,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            mode: VerifyMode::Quick,
            detect_moves: false,
            mtime_tolerance: Duration::from_secs(2),
        }
    }
}

/// Outcome for one relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifyStatus {
    /// Same content (hashed), or in Quick mode the same size and mtime.
    Match,
    /// In the original, not in the backup.
    Missing,
    /// Both exist and the content differs (size or hash).
    Differs,
    /// Only in the backup.
    Extra,
    /// The original is missing at its path, but the same content is in the backup at
    /// `backup_rel` (found by hash, with `detect_moves`).
    Moved { backup_rel: PathBuf },
    /// Not compared: an online-only placeholder, a link, or a file-vs-folder clash.
    NotChecked(String),
    /// The file could not be read or hashed.
    Error(String),
}

impl VerifyStatus {
    /// Short lowercase word for lists and CSV.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Missing => "missing",
            Self::Differs => "differs",
            Self::Extra => "extra",
            Self::Moved { .. } => "moved",
            Self::NotChecked(_) => "not checked",
            Self::Error(_) => "error",
        }
    }

    /// Extra text for CSV: the new location for moves, the reason otherwise.
    fn detail(&self) -> String {
        match self {
            Self::Moved { backup_rel } => paths::to_slash(backup_rel),
            Self::NotChecked(reason) | Self::Error(reason) => reason.clone(),
            _ => String::new(),
        }
    }
}

/// One compared path. Sizes are `None` for folders, links and missing sides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyEntry {
    /// Path relative to the original root (the backup root for extras).
    pub rel: PathBuf,
    pub original_size: Option<u64>,
    pub backup_size: Option<u64>,
    pub status: VerifyStatus,
    /// Whether file data was read for this entry.
    pub hashed: bool,
}

/// Number of entries per status.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifyCounts {
    pub matched: usize,
    pub missing: usize,
    pub differs: usize,
    pub extra: usize,
    pub moved: usize,
    pub not_checked: usize,
    pub errors: usize,
}

/// Full result of [`verify_backup`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupReport {
    pub original: PathBuf,
    pub backup: PathBuf,
    pub mode: VerifyMode,
    /// Sorted by relative path, case-insensitively.
    pub entries: Vec<VerifyEntry>,
    /// Bytes actually read for hashing.
    pub bytes_hashed: u64,
}

impl BackupReport {
    pub fn counts(&self) -> VerifyCounts {
        let mut c = VerifyCounts::default();
        for e in &self.entries {
            let slot = match &e.status {
                VerifyStatus::Match => &mut c.matched,
                VerifyStatus::Missing => &mut c.missing,
                VerifyStatus::Differs => &mut c.differs,
                VerifyStatus::Extra => &mut c.extra,
                VerifyStatus::Moved { .. } => &mut c.moved,
                VerifyStatus::NotChecked(_) => &mut c.not_checked,
                VerifyStatus::Error(_) => &mut c.errors,
            };
            *slot += 1;
        }
        c
    }

    /// True when nothing is missing, changed, moved or unreadable. Extra files and
    /// not-checked entries do not fail the check.
    pub fn is_ok(&self) -> bool {
        let c = self.counts();
        c.missing == 0 && c.differs == 0 && c.errors == 0 && c.moved == 0
    }

    /// CSV with header `status,path,original_size,backup_size,detail` (RFC 4180 quoting).
    pub fn to_csv(&self) -> String {
        let mut out = String::from("status,path,original_size,backup_size,detail\r\n");
        let size = |s: Option<u64>| s.map(|v| v.to_string()).unwrap_or_default();
        for e in &self.entries {
            let fields = [
                e.status.label().to_string(),
                csv_field(&paths::to_slash(&e.rel)),
                size(e.original_size),
                size(e.backup_size),
                csv_field(&e.status.detail()),
            ];
            out.push_str(&fields.join(","));
            out.push_str("\r\n");
        }
        out
    }

    /// Human-readable summary with one section per problem kind.
    pub fn to_text(&self) -> String {
        let c = self.counts();
        let mode = match self.mode {
            VerifyMode::Quick => "Quick",
            VerifyMode::Full => "Full",
        };
        let mut out = format!(
            "Backup verification\nOriginal: {}\nBackup: {}\nMode: {mode}\n\
             Counts: {} matched, {} missing, {} differ, {} only in the backup, {} moved, \
             {} not checked, {} errors\n",
            self.original.display(),
            self.backup.display(),
            c.matched,
            c.missing,
            c.differs,
            c.extra,
            c.moved,
            c.not_checked,
            c.errors,
        );
        let of = |want: fn(&VerifyStatus) -> bool, fmt: &dyn Fn(&VerifyEntry) -> String| {
            self.entries
                .iter()
                .filter(|e| want(&e.status))
                .map(fmt)
                .collect::<Vec<String>>()
        };
        let plain = |e: &VerifyEntry| paths::to_slash(&e.rel);
        let sections = [
            (
                "Missing from the backup",
                of(|s| matches!(s, VerifyStatus::Missing), &plain),
            ),
            (
                "Differs",
                of(|s| matches!(s, VerifyStatus::Differs), &plain),
            ),
            (
                "Moved or renamed",
                of(|s| matches!(s, VerifyStatus::Moved { .. }), &|e| {
                    format!("{} -> {}", plain(e), e.status.detail())
                }),
            ),
            (
                "Only in the backup",
                of(|s| matches!(s, VerifyStatus::Extra), &plain),
            ),
            (
                "Not checked",
                of(|s| matches!(s, VerifyStatus::NotChecked(_)), &|e| {
                    format!("{} ({})", plain(e), e.status.detail())
                }),
            ),
            (
                "Errors",
                of(|s| matches!(s, VerifyStatus::Error(_)), &|e| {
                    format!("{}: {}", plain(e), e.status.detail())
                }),
            ),
        ];
        for (title, lines) in sections {
            if lines.is_empty() {
                continue;
            }
            out.push_str(&format!("\n{title} ({}):\n", lines.len()));
            for line in lines {
                out.push_str("  ");
                out.push_str(&line);
                out.push('\n');
            }
        }
        if self.is_ok() {
            out.push_str("\nBackup verified: no problems found.\n");
        }
        out
    }
}

/// Quote a CSV field when it contains a comma, quote or line break.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// One file or folder seen while scanning one side.
#[derive(Debug, Clone)]
struct Side {
    path: PathBuf,
    rel: PathBuf,
    kind: NodeKind,
    size: u64,
    modified: Option<SystemTime>,
}

/// A report row before sorting. Keeps both sides so move detection can hash them.
#[derive(Debug, Clone)]
struct Row {
    entry: VerifyEntry,
    orig: Option<Side>,
    back: Option<Side>,
}

/// Everything seen while scanning one side, keyed later by [`index_sides`].
struct Scan {
    /// Every entry, with its real relative path.
    items: Vec<(PathBuf, Side)>,
    /// Unreadable items, as error rows.
    errors: Vec<Row>,
    /// Keys of unreadable items. Nothing under them is known, so nothing there is reported
    /// as missing or extra.
    unreadable: Vec<String>,
}

/// The two sides keyed by matching key, with ambiguous names set aside.
#[derive(Default)]
struct Index {
    orig: BTreeMap<String, Side>,
    back: BTreeMap<String, Side>,
    /// One error row per real path whose key is shared with another spelling, on either side.
    ambiguous: Vec<Row>,
}

/// Matching key for a relative path: `/`-separated, lowercase on Windows (as in `compare`).
fn key(rel: &Path) -> String {
    let s = paths::to_slash(rel);
    if cfg!(windows) { s.to_lowercase() } else { s }
}

/// Is `key` the item `prefix` or something inside it?
fn under(key: &str, prefix: &str) -> bool {
    key == prefix
        || key
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Group one side's entries by key.
fn group_by_key(
    items: impl IntoIterator<Item = (PathBuf, Side)>,
    key_of: &impl Fn(&Path) -> String,
) -> BTreeMap<String, Vec<Side>> {
    let mut map: BTreeMap<String, Vec<Side>> = BTreeMap::new();
    for (rel, side) in items {
        map.entry(key_of(&rel)).or_default().push(side);
    }
    map
}

/// Pair the two sides by key. A key that more than one real spelling maps to on either side
/// (for example `A.jpg` and `a.jpg` in one case-sensitive folder) is ambiguous: it is left out
/// of both maps, so nothing is compared or hashed under it, and every file involved becomes an
/// error row with its real spelling. Folders are never reported, so they are not listed.
fn index_sides(
    orig: impl IntoIterator<Item = (PathBuf, Side)>,
    back: impl IntoIterator<Item = (PathBuf, Side)>,
    key_of: impl Fn(&Path) -> String,
) -> Index {
    let og = group_by_key(orig, &key_of);
    let bg = group_by_key(back, &key_of);
    let ambiguous: HashSet<String> = og
        .iter()
        .chain(bg.iter())
        .filter(|(_, sides)| sides.len() > 1)
        .map(|(k, _)| k.clone())
        .collect();

    let mut idx = Index::default();
    let set_aside = |sides: Vec<Side>, original: bool, out: &mut Index| {
        for side in sides.into_iter().filter(|s| s.kind != NodeKind::Dir) {
            let size = file_size(Some(&side));
            let (original_size, backup_size) = if original { (size, None) } else { (None, size) };
            out.ambiguous.push(Row {
                entry: VerifyEntry {
                    rel: side.rel.clone(),
                    original_size,
                    backup_size,
                    status: VerifyStatus::Error(AMBIGUOUS_REASON.into()),
                    hashed: false,
                },
                orig: None,
                back: None,
            });
        }
    };
    for (k, sides) in og {
        if ambiguous.contains(&k) {
            set_aside(sides, true, &mut idx);
        } else if let Some(side) = sides.into_iter().next() {
            idx.orig.insert(k, side);
        }
    }
    for (k, sides) in bg {
        if ambiguous.contains(&k) {
            set_aside(sides, false, &mut idx);
        } else if let Some(side) = sides.into_iter().next() {
            idx.back.insert(k, side);
        }
    }
    idx
}

fn scan(root: &Path, ctx: &JobCtx<'_>) -> Result<Scan> {
    let mut sc = Scan {
        items: Vec::new(),
        errors: Vec::new(),
        unreadable: Vec::new(),
    };
    walk_tree(root, ctx.cancel, |item| match item {
        WalkItem::Entry(e) => {
            let side = Side {
                path: e.path.clone(),
                rel: e.rel.clone(),
                kind: e.kind,
                size: e.size,
                modified: e.modified,
            };
            sc.items.push((e.rel.clone(), side));
        }
        WalkItem::Error { path, error } => {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path.as_path())
                .to_path_buf();
            let k = key(&rel);
            sc.unreadable.push(k.clone());
            sc.errors.push(Row {
                entry: VerifyEntry {
                    rel,
                    original_size: None,
                    backup_size: None,
                    status: VerifyStatus::Error(error.to_string()),
                    hashed: false,
                },
                orig: None,
                back: None,
            });
        }
    })?;
    Ok(sc)
}

fn times_match(a: Option<SystemTime>, b: Option<SystemTime>, tol: Duration) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let d = match a.duration_since(b) {
                Ok(d) => d,
                Err(e) => e.duration(),
            };
            d <= tol
        }
        _ => false,
    }
}

/// BLAKE3 of one side. The inner `Err` is a read failure; cancellation is the outer `Err`.
fn hash_side(side: &Side, ctx: &JobCtx<'_>, bytes_hashed: &mut u64) -> Result<DigestResult> {
    ctx.progress.set_current(side.path.display().to_string());
    match hash_file(&side.path, ctx.cancel, Some(ctx.progress)) {
        Ok(d) => {
            *bytes_hashed += side.size;
            Ok(Ok(d))
        }
        Err(CoreError::Cancelled) => Err(CoreError::Cancelled),
        Err(e) => Ok(Err(e.to_string())),
    }
}

/// Compare two files that exist on both sides. Returns the status and whether both were
/// hashed.
fn compare_files(
    o: &Side,
    b: &Side,
    opts: &VerifyOptions,
    ctx: &JobCtx<'_>,
    bytes_hashed: &mut u64,
) -> Result<(VerifyStatus, bool)> {
    if o.size != b.size {
        return Ok((VerifyStatus::Differs, false));
    }
    if opts.mode == VerifyMode::Quick && times_match(o.modified, b.modified, opts.mtime_tolerance) {
        return Ok((VerifyStatus::Match, false));
    }
    ctx.progress.add_total_bytes(o.size + b.size);
    let od = match hash_side(o, ctx, bytes_hashed)? {
        Ok(d) => d,
        Err(msg) => return Ok((VerifyStatus::Error(msg), false)),
    };
    let bd = match hash_side(b, ctx, bytes_hashed)? {
        Ok(d) => d,
        Err(msg) => return Ok((VerifyStatus::Error(msg), false)),
    };
    let status = if od == bd {
        VerifyStatus::Match
    } else {
        VerifyStatus::Differs
    };
    Ok((status, true))
}

fn file_size(s: Option<&Side>) -> Option<u64> {
    s.filter(|s| s.kind == NodeKind::File).map(|s| s.size)
}

/// Decide the row for one key. `None` for folders (not reported) and for suppressed items.
fn classify(
    key: &str,
    o: Option<&Side>,
    b: Option<&Side>,
    opts: &VerifyOptions,
    unreadable: &[String],
    ctx: &JobCtx<'_>,
    bytes_hashed: &mut u64,
) -> Result<Option<Row>> {
    let (status, hashed) = match (o, b) {
        (None, None) => return Ok(None),
        (Some(o), None) => match o.kind {
            NodeKind::Dir => return Ok(None),
            NodeKind::Link => (VerifyStatus::NotChecked(LINK_REASON.into()), false),
            NodeKind::File if unreadable.iter().any(|u| under(key, u)) => return Ok(None),
            NodeKind::File => (VerifyStatus::Missing, false),
        },
        (None, Some(b)) => match b.kind {
            NodeKind::Dir => return Ok(None),
            NodeKind::Link => (VerifyStatus::NotChecked(LINK_REASON.into()), false),
            NodeKind::File if unreadable.iter().any(|u| under(key, u)) => return Ok(None),
            NodeKind::File => (VerifyStatus::Extra, false),
        },
        (Some(o), Some(b)) => {
            if o.kind == NodeKind::Dir && b.kind == NodeKind::Dir {
                return Ok(None);
            }
            if o.kind == NodeKind::Link || b.kind == NodeKind::Link {
                (VerifyStatus::NotChecked(LINK_REASON.into()), false)
            } else if o.kind != b.kind {
                (VerifyStatus::NotChecked(CLASH_REASON.into()), false)
            } else if !(background_read_allowed(&o.path) && background_read_allowed(&b.path)) {
                (VerifyStatus::NotChecked(PLACEHOLDER_REASON.into()), false)
            } else {
                compare_files(o, b, opts, ctx, bytes_hashed)?
            }
        }
    };
    let rel = o.or(b).map(|s| s.rel.clone()).unwrap_or_default();
    Ok(Some(Row {
        entry: VerifyEntry {
            rel,
            original_size: file_size(o),
            backup_size: file_size(b),
            status,
            hashed,
        },
        orig: o.cloned(),
        back: b.cloned(),
    }))
}

/// A file we may read in the background (not a placeholder, not a link or folder).
fn hashable(s: Option<&Side>) -> bool {
    s.is_some_and(|s| s.kind == NodeKind::File && background_read_allowed(&s.path))
}

/// Turn Missing originals into Moved when an Extra backup file of the same size has the same
/// hash. Each Extra satisfies at most one original, and the matched Extra row is dropped.
fn detect_moves(rows: &mut Vec<Row>, ctx: &JobCtx<'_>, bytes_hashed: &mut u64) -> Result<()> {
    detect_moves_with(rows, ctx, &mut |side: &Side| {
        hash_side(side, ctx, bytes_hashed)
    })
}

/// [`detect_moves`] with the digest step passed in, so tests can make reads fail or cancel.
///
/// Only sizes present on both sides are read: the Extras of such a size are hashed and indexed
/// by (size, digest), then each Missing of an indexed size is hashed once and paired with one
/// unused Extra of the same (size, digest). A file whose read fails becomes an `Error` row.
fn detect_moves_with(
    rows: &mut Vec<Row>,
    ctx: &JobCtx<'_>,
    digest: &mut dyn FnMut(&Side) -> Result<DigestResult>,
) -> Result<()> {
    let missing: Vec<usize> = (0..rows.len())
        .filter(|&i| {
            rows[i].entry.status == VerifyStatus::Missing && hashable(rows[i].orig.as_ref())
        })
        .collect();
    let extra: Vec<usize> = (0..rows.len())
        .filter(|&i| rows[i].entry.status == VerifyStatus::Extra && hashable(rows[i].back.as_ref()))
        .collect();
    if missing.is_empty() || extra.is_empty() {
        return Ok(());
    }
    let missing_sizes: HashSet<u64> = missing
        .iter()
        .filter_map(|&i| rows[i].orig.as_ref())
        .map(|s| s.size)
        .collect();

    // Read and index the extras that could pair with a missing file.
    let mut index: HashMap<(u64, Digest), Vec<usize>> = HashMap::new();
    let mut failed: Vec<(usize, String)> = Vec::new();
    for &ei in &extra {
        ctx.cancel.check()?;
        let Some(b) = rows[ei].back.as_ref() else {
            continue;
        };
        if !missing_sizes.contains(&b.size) {
            continue;
        }
        ctx.progress.add_total_bytes(b.size);
        match digest(b)? {
            Ok(d) => index.entry((b.size, d)).or_default().push(ei),
            Err(msg) => failed.push((ei, msg)),
        }
        ctx.cancel.check()?;
    }

    // Read the missing files whose size some indexed extra has, and pair each one.
    let indexed_sizes: HashSet<u64> = index.keys().map(|&(size, _)| size).collect();
    let mut consumed = vec![false; rows.len()];
    let mut pairs = Vec::new();
    for &mi in &missing {
        ctx.cancel.check()?;
        let Some(o) = rows[mi].orig.as_ref() else {
            continue;
        };
        if !indexed_sizes.contains(&o.size) {
            continue;
        }
        ctx.progress.add_total_bytes(o.size);
        let od = match digest(o)? {
            Ok(d) => d,
            Err(msg) => {
                failed.push((mi, msg));
                continue;
            }
        };
        ctx.cancel.check()?;
        if let Some(ei) = index.get_mut(&(o.size, od)).and_then(Vec::pop) {
            consumed[ei] = true;
            pairs.push((mi, ei));
        }
    }

    for (mi, ei) in pairs {
        let backup_rel = rows[ei].entry.rel.clone();
        let backup_size = rows[ei].entry.backup_size;
        let row = &mut rows[mi];
        row.entry.status = VerifyStatus::Moved { backup_rel };
        row.entry.backup_size = backup_size;
        row.entry.hashed = true;
    }
    for (i, msg) in failed {
        rows[i].entry.status = VerifyStatus::Error(msg);
    }
    let mut idx = 0;
    rows.retain(|_| {
        let keep = !consumed[idx];
        idx += 1;
        keep
    });
    Ok(())
}

/// Refuse roots that are the same folder or contain each other: a backup inside the original
/// would be scanned into itself, and an original inside the backup would be reported as extra.
fn check_roots(original: &Path, backup: &Path) -> Result<()> {
    for root in [original, backup] {
        if !fs::metadata(root).is_ok_and(|m| m.is_dir()) {
            return Err(CoreError::Invalid(format!(
                "{} is not a folder",
                root.display()
            )));
        }
    }
    let o = fs::canonicalize(original).map_err(|e| CoreError::io(original, e))?;
    let b = fs::canonicalize(backup).map_err(|e| CoreError::io(backup, e))?;
    if o == b {
        Err(CoreError::Invalid(
            "the original and the backup are the same folder".into(),
        ))
    } else if b.starts_with(&o) {
        Err(CoreError::Invalid(
            "the backup folder is inside the original folder".into(),
        ))
    } else if o.starts_with(&b) {
        Err(CoreError::Invalid(
            "the original folder is inside the backup folder".into(),
        ))
    } else {
        Ok(())
    }
}

/// Compare `original` with `backup` by content.
///
/// Only regular files are compared. Folders are traversed but not reported, links are
/// reported as not checked, and online-only placeholders are never read. Unreadable items
/// become `Error` entries and the walk goes on. Names that differ only in letter case inside
/// one folder are reported as errors rather than compared. Returns `Err(Invalid)` if either
/// root is not a folder, or if the roots are the same folder or contain each other, and
/// `Err(Cancelled)` when cancelled.
pub fn verify_backup(
    original: &Path,
    backup: &Path,
    opts: &VerifyOptions,
    ctx: &JobCtx<'_>,
) -> Result<BackupReport> {
    ctx.cancel.check()?;
    check_roots(original, backup)?;
    let left = scan(original, ctx)?;
    let right = scan(backup, ctx)?;
    let unreadable: Vec<String> = left
        .unreadable
        .into_iter()
        .chain(right.unreadable)
        .collect();

    let mut rows = left.errors;
    rows.extend(right.errors);
    let Index {
        orig: lsides,
        back: rsides,
        ambiguous,
    } = index_sides(left.items, right.items, key);
    rows.extend(ambiguous);

    let mut bytes_hashed = 0u64;
    let keys: BTreeSet<&String> = lsides.keys().chain(rsides.keys()).collect();
    ctx.progress.add_total_items(keys.len() as u64);
    for k in keys {
        ctx.cancel.check()?;
        let row = classify(
            k,
            lsides.get(k),
            rsides.get(k),
            opts,
            &unreadable,
            ctx,
            &mut bytes_hashed,
        )?;
        rows.extend(row);
        ctx.progress.add_items(1);
    }

    if opts.detect_moves {
        detect_moves(&mut rows, ctx, &mut bytes_hashed)?;
    }
    rows.sort_by_cached_key(|r| paths::to_slash(&r.entry.rel).to_lowercase());
    Ok(BackupReport {
        original: original.to_path_buf(),
        backup: backup.to_path_buf(),
        mode: opts.mode,
        entries: rows.into_iter().map(|r| r.entry).collect(),
        bytes_hashed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{clear_simulated_online_only, simulate_online_only};
    use forge_jobs::{CancelToken, Progress};

    fn put(path: &Path, data: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    fn set_mtime(path: &Path, t: SystemTime) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn run(orig: &Path, back: &Path, opts: &VerifyOptions) -> Result<BackupReport> {
        let cancel = CancelToken::new();
        let progress = Progress::default();
        verify_backup(orig, back, opts, &JobCtx::new(&cancel, &progress))
    }

    fn entry<'a>(r: &'a BackupReport, rel: &str) -> &'a VerifyEntry {
        r.entries
            .iter()
            .find(|e| paths::to_slash(&e.rel) == rel)
            .unwrap_or_else(|| panic!("no entry for {rel}"))
    }

    #[test]
    fn classifies_every_case_in_full_mode() {
        let d = tempfile::tempdir().unwrap();
        let (o, b) = (d.path().join("orig"), d.path().join("backup"));
        put(&o.join("same.txt"), b"same");
        put(&o.join("changed.txt"), b"aaaa");
        put(&o.join("longer.txt"), b"short");
        put(&o.join("missing.txt"), b"gone");
        put(&o.join("sub/deep.txt"), b"deep");
        put(&b.join("same.txt"), b"same");
        put(&b.join("changed.txt"), b"bbbb");
        put(&b.join("longer.txt"), b"much longer now");
        put(&b.join("sub/deep.txt"), b"deep");
        put(&b.join("extra.txt"), b"new");
        // Same size and same mtime: Quick would trust this pair, Full must hash it.
        let t = SystemTime::now() - Duration::from_secs(86_400);
        set_mtime(&o.join("changed.txt"), t);
        set_mtime(&b.join("changed.txt"), t);

        let opts = VerifyOptions {
            mode: VerifyMode::Full,
            ..Default::default()
        };
        let r = run(&o, &b, &opts).unwrap();
        assert_eq!(entry(&r, "same.txt").status, VerifyStatus::Match);
        assert_eq!(entry(&r, "sub/deep.txt").status, VerifyStatus::Match);
        assert_eq!(entry(&r, "changed.txt").status, VerifyStatus::Differs);
        assert_eq!(entry(&r, "longer.txt").status, VerifyStatus::Differs);
        assert_eq!(entry(&r, "missing.txt").status, VerifyStatus::Missing);
        assert_eq!(entry(&r, "extra.txt").status, VerifyStatus::Extra);
        assert_eq!(r.entries.len(), 6);
        let c = r.counts();
        assert_eq!(
            (
                c.matched,
                c.missing,
                c.differs,
                c.extra,
                c.moved,
                c.not_checked,
                c.errors
            ),
            (2, 1, 2, 1, 0, 0, 0)
        );
        assert!(!r.is_ok());
    }

    #[test]
    fn quick_trusts_size_and_mtime_full_hashes() {
        let d = tempfile::tempdir().unwrap();
        let (o, b) = (d.path().join("orig"), d.path().join("backup"));
        let t = SystemTime::now() - Duration::from_secs(3600);
        // Different bytes, same size and mtime.
        put(&o.join("a.bin"), b"aaaa");
        put(&b.join("a.bin"), b"bbbb");
        set_mtime(&o.join("a.bin"), t);
        set_mtime(&b.join("a.bin"), t);
        // Same bytes, mtime one hour apart.
        put(&o.join("c.bin"), b"same");
        put(&b.join("c.bin"), b"same");
        set_mtime(&o.join("c.bin"), t);
        set_mtime(&b.join("c.bin"), t + Duration::from_secs(3600));

        let quick = run(&o, &b, &VerifyOptions::default()).unwrap();
        let a = entry(&quick, "a.bin");
        assert_eq!(a.status, VerifyStatus::Match);
        assert!(!a.hashed);
        let c = entry(&quick, "c.bin");
        assert_eq!(c.status, VerifyStatus::Match);
        assert!(c.hashed);

        let full = run(
            &o,
            &b,
            &VerifyOptions {
                mode: VerifyMode::Full,
                ..Default::default()
            },
        )
        .unwrap();
        let a = entry(&full, "a.bin");
        assert_eq!(a.status, VerifyStatus::Differs);
        assert!(a.hashed);
    }

    #[test]
    fn detects_moves_by_hash_only_when_asked() {
        let d = tempfile::tempdir().unwrap();
        let (o, b) = (d.path().join("orig"), d.path().join("backup"));
        put(&o.join("a/photo.bin"), b"PHOTO-BYTES-0001");
        put(&b.join("b/renamed.bin"), b"PHOTO-BYTES-0001");
        // Same size as the photo, different content: must not be paired.
        put(&b.join("b/decoy.bin"), b"PHOTO-BYTES-0002");

        let opts = VerifyOptions {
            detect_moves: true,
            ..Default::default()
        };
        let r = run(&o, &b, &opts).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(
            entry(&r, "a/photo.bin").status,
            VerifyStatus::Moved {
                backup_rel: PathBuf::from("b/renamed.bin")
            }
        );
        assert_eq!(entry(&r, "b/decoy.bin").status, VerifyStatus::Extra);
        assert!(
            r.entries
                .iter()
                .all(|e| paths::to_slash(&e.rel) != "b/renamed.bin")
        );
        let c = r.counts();
        assert_eq!((c.moved, c.extra, c.missing), (1, 1, 0));

        let plain = run(&o, &b, &VerifyOptions::default()).unwrap();
        assert_eq!(entry(&plain, "a/photo.bin").status, VerifyStatus::Missing);
        let c = plain.counts();
        assert_eq!((c.missing, c.extra, c.moved), (1, 2, 0));
    }

    #[test]
    fn online_only_files_are_never_read() {
        let d = tempfile::tempdir().unwrap();
        let (o, b) = (d.path().join("orig"), d.path().join("backup"));
        put(&o.join("a.txt"), b"content");
        put(&b.join("a.txt"), b"content");
        let file = o.join("a.txt");
        simulate_online_only(file.clone());
        let opts = VerifyOptions {
            mode: VerifyMode::Full,
            ..Default::default()
        };
        let res = run(&o, &b, &opts);
        clear_simulated_online_only(&file);
        let r = res.unwrap();
        let e = entry(&r, "a.txt");
        assert_eq!(
            e.status,
            VerifyStatus::NotChecked(PLACEHOLDER_REASON.to_string())
        );
        assert!(!e.hashed);
        assert_eq!(r.bytes_hashed, 0);
    }

    #[test]
    fn cancelled_before_start_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let (o, b) = (d.path().join("orig"), d.path().join("backup"));
        fs::create_dir_all(&o).unwrap();
        fs::create_dir_all(&b).unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        let progress = Progress::default();
        let res = verify_backup(
            &o,
            &b,
            &VerifyOptions::default(),
            &JobCtx::new(&cancel, &progress),
        );
        assert!(matches!(res, Err(CoreError::Cancelled)));
    }

    #[cfg(windows)]
    #[test]
    fn names_match_case_insensitively_on_windows() {
        let d = tempfile::tempdir().unwrap();
        let (o, b) = (d.path().join("orig"), d.path().join("backup"));
        put(&o.join("IMG.JPG"), b"pixels");
        put(&b.join("img.jpg"), b"pixels");
        let r = run(&o, &b, &VerifyOptions::default()).unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].status, VerifyStatus::Match);
    }

    #[test]
    fn csv_quotes_fields_and_text_lists_sections() {
        let report = BackupReport {
            original: "C:/a".into(),
            backup: "D:/b".into(),
            mode: VerifyMode::Quick,
            entries: vec![
                VerifyEntry {
                    rel: PathBuf::from("docs/a,b \"q\".txt"),
                    original_size: Some(10),
                    backup_size: None,
                    status: VerifyStatus::Missing,
                    hashed: false,
                },
                VerifyEntry {
                    rel: PathBuf::from("p.bin"),
                    original_size: Some(4),
                    backup_size: Some(4),
                    status: VerifyStatus::Moved {
                        backup_rel: PathBuf::from("q/p.bin"),
                    },
                    hashed: true,
                },
            ],
            bytes_hashed: 8,
        };
        let csv = report.to_csv();
        assert_eq!(
            csv.lines().next(),
            Some("status,path,original_size,backup_size,detail")
        );
        assert!(csv.contains("\r\nmissing,\"docs/a,b \"\"q\"\".txt\",10,,\r\n"));
        assert!(csv.contains("moved,p.bin,4,4,q/p.bin\r\n"));

        let text = report.to_text();
        assert!(text.contains("Missing from the backup (1):\n  docs/a,b \"q\".txt\n"));
        assert!(text.contains("Moved or renamed (1):\n  p.bin -> q/p.bin\n"));
        assert!(!text.contains("Differs"));
        assert!(!text.contains("no problems found"));

        let ok = BackupReport {
            entries: vec![VerifyEntry {
                rel: PathBuf::from("new.txt"),
                original_size: None,
                backup_size: Some(1),
                status: VerifyStatus::Extra,
                hashed: false,
            }],
            ..Default::default()
        };
        assert!(ok.is_ok());
        assert!(
            ok.to_text()
                .ends_with("Backup verified: no problems found.\n")
        );
    }

    #[test]
    fn file_as_root_is_invalid() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("file.txt");
        put(&file, b"x");
        let backup = d.path().join("backup");
        fs::create_dir_all(&backup).unwrap();
        let res = run(&file, &backup, &VerifyOptions::default());
        assert!(matches!(res, Err(CoreError::Invalid(_))));
    }

    fn item(rel: &str, size: u64) -> (PathBuf, Side) {
        let rel = PathBuf::from(rel);
        let side = Side {
            path: rel.clone(),
            rel: rel.clone(),
            kind: NodeKind::File,
            size,
            modified: None,
        };
        (rel, side)
    }

    /// A row for a real file under `root` that is only on one side (Missing for the original,
    /// Extra otherwise). The file must exist: move detection only reads readable files.
    fn one_sided_row(root: &Path, rel: &str, size: u64, original: bool) -> Row {
        let (rel, mut side) = item(rel, size);
        side.path = root.join(&rel);
        put(&side.path, &vec![0u8; size as usize]);
        let (status, orig, back) = if original {
            (VerifyStatus::Missing, Some(side), None)
        } else {
            (VerifyStatus::Extra, None, Some(side))
        };
        Row {
            entry: VerifyEntry {
                rel,
                original_size: orig.as_ref().map(|s| s.size),
                backup_size: back.as_ref().map(|s| s.size),
                status,
                hashed: false,
            },
            orig,
            back,
        }
    }

    /// Digest that depends only on the file name: `a/photo.bin` and `b/renamed.bin` share
    /// content, every other name has different content.
    fn name_digest(s: &Side) -> DigestResult {
        let id = if matches!(
            paths::to_slash(&s.rel).as_str(),
            "a/photo.bin" | "b/renamed.bin"
        ) {
            1
        } else {
            2
        };
        Ok([id; 32])
    }

    #[test]
    fn names_differing_only_in_case_are_ambiguous() {
        let fold = |p: &Path| paths::to_slash(p).to_lowercase();
        let idx = index_sides(
            vec![item("A.jpg", 1), item("a.jpg", 1), item("Solo.txt", 2)],
            vec![item("a.jpg", 1), item("solo.txt", 2), item("other.txt", 3)],
            fold,
        );
        // Every file under the shared key is reported, on the side it is on.
        let mut amb: Vec<String> = idx
            .ambiguous
            .iter()
            .map(|r| paths::to_slash(&r.entry.rel))
            .collect();
        amb.sort();
        assert_eq!(amb, ["A.jpg", "a.jpg", "a.jpg"]);
        assert!(
            idx.ambiguous
                .iter()
                .all(|r| r.entry.status == VerifyStatus::Error(AMBIGUOUS_REASON.to_string()))
        );
        // Ambiguous keys are not compared; unambiguous ones still pair up.
        let orig_keys: Vec<&str> = idx.orig.keys().map(String::as_str).collect();
        let back_keys: Vec<&str> = idx.back.keys().map(String::as_str).collect();
        assert_eq!(orig_keys, ["solo.txt"]);
        assert_eq!(back_keys, ["other.txt", "solo.txt"]);
        assert_eq!(idx.orig["solo.txt"].rel, PathBuf::from("Solo.txt"));
    }

    #[test]
    fn digest_failure_during_move_detection_is_an_error_row() {
        let d = tempfile::tempdir().unwrap();
        let mut rows = vec![
            one_sided_row(d.path(), "a/photo.bin", 16, true),
            one_sided_row(d.path(), "b/renamed.bin", 16, false),
            one_sided_row(d.path(), "b/decoy.bin", 16, false),
        ];
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let ctx = JobCtx::new(&cancel, &progress);
        detect_moves_with(&mut rows, &ctx, &mut |s: &Side| {
            if paths::to_slash(&s.rel) == "b/renamed.bin" {
                Ok(Err("disk on fire".to_string()))
            } else {
                Ok(name_digest(s))
            }
        })
        .unwrap();
        let status = |rel: &str| {
            rows.iter()
                .find(|r| paths::to_slash(&r.entry.rel) == rel)
                .unwrap_or_else(|| panic!("no row for {rel}"))
                .entry
                .status
                .clone()
        };
        assert_eq!(
            status("b/renamed.bin"),
            VerifyStatus::Error("disk on fire".to_string())
        );
        assert_eq!(status("a/photo.bin"), VerifyStatus::Missing);
        assert_eq!(status("b/decoy.bin"), VerifyStatus::Extra);
    }

    #[test]
    fn move_detection_reads_only_files_of_a_shared_size() {
        let d = tempfile::tempdir().unwrap();
        let mut rows = vec![
            one_sided_row(d.path(), "a.bin", 4, true),
            one_sided_row(d.path(), "x.bin", 9, false),
            one_sided_row(d.path(), "y.bin", 4, false),
        ];
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let ctx = JobCtx::new(&cancel, &progress);
        let mut read = Vec::new();
        detect_moves_with(&mut rows, &ctx, &mut |s: &Side| {
            read.push(paths::to_slash(&s.rel));
            Ok(name_digest(s))
        })
        .unwrap();
        // x.bin has no same-size original, so it is never read.
        assert_eq!(read, ["y.bin", "a.bin"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].entry.status,
            VerifyStatus::Moved {
                backup_rel: PathBuf::from("y.bin")
            }
        );
    }

    #[test]
    fn move_detection_stops_when_cancelled_in_the_digest_step() {
        let d = tempfile::tempdir().unwrap();
        let mut rows = vec![
            one_sided_row(d.path(), "a.bin", 4, true),
            one_sided_row(d.path(), "b.bin", 4, false),
            one_sided_row(d.path(), "c.bin", 4, false),
        ];
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let ctx = JobCtx::new(&cancel, &progress);
        let mut calls = 0;
        let res = detect_moves_with(&mut rows, &ctx, &mut |_: &Side| {
            calls += 1;
            cancel.cancel();
            Ok(Ok([0; 32]))
        });
        assert!(matches!(res, Err(CoreError::Cancelled)));
        assert_eq!(calls, 1);
    }

    #[test]
    fn same_or_nested_roots_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let o = d.path().join("orig");
        let inner = o.join("backup");
        fs::create_dir_all(&inner).unwrap();
        let cases = [
            (o.clone(), o.clone(), "identical"),
            (o.clone(), inner.clone(), "backup inside original"),
            (inner.clone(), o.clone(), "original inside backup"),
        ];
        for (a, b, what) in cases {
            let res = run(&a, &b, &VerifyOptions::default());
            assert!(
                matches!(res, Err(CoreError::Invalid(_))),
                "{what}: expected Invalid, got {res:?}"
            );
        }
        // Siblings are fine, and the report keeps the roots as the caller passed them.
        let sibling = d.path().join("sibling");
        fs::create_dir_all(&sibling).unwrap();
        let r = run(&o, &sibling, &VerifyOptions::default()).unwrap();
        assert_eq!(r.original, o);
        assert_eq!(r.backup, sibling);
    }
}
