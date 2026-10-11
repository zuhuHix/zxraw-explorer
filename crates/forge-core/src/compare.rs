//! Folder comparison and one-way sync planning.
//!
//! `compare_dirs` is read-only. `plan_sync` turns a comparison into a list of actions that
//! the UI shows *before* anything happens; `execute_sync` then performs exactly those actions
//! using the atomic copy from [`crate::ops`]. Deletions only ever appear in
//! [`SyncMode::Mirror`] plans.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::hashing;
use crate::ops::{self, CopyFileOptions, JobCtx, NodeKind, OpIssue, OpReport, WalkItem, walk_tree};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompareOptions {
    /// Compare file contents (BLAKE3) when sizes match, instead of trusting timestamps.
    pub use_hash: bool,
    /// Timestamps closer than this count as equal (FAT/exFAT store 2-second resolution).
    pub mtime_tolerance: Duration,
}

impl Default for CompareOptions {
    fn default() -> Self {
        Self {
            use_hash: false,
            mtime_tolerance: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideInfo {
    pub kind: NodeKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffReason {
    Size,
    LeftNewer,
    RightNewer,
    /// Same size and (with `use_hash`) different content.
    Content,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryState {
    LeftOnly,
    RightOnly,
    Same,
    Different(DiffReason),
    /// e.g. a file on the left and a folder on the right.
    TypeMismatch,
    /// Could not be compared (hash failed).
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompareEntry {
    /// Relative path (left spelling when both exist).
    pub rel: PathBuf,
    pub left: Option<SideInfo>,
    pub right: Option<SideInfo>,
    pub state: EntryState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompareReport {
    pub entries: Vec<CompareEntry>,
    pub errors: Vec<OpIssue>,
}

impl CompareReport {
    pub fn count(&self, pred: impl Fn(&EntryState) -> bool) -> usize {
        self.entries.iter().filter(|e| pred(&e.state)).count()
    }
}

fn key(rel: &Path) -> String {
    let s = crate::paths::to_slash(rel);
    if cfg!(windows) { s.to_lowercase() } else { s }
}

fn scan_side(
    root: &Path,
    ctx: &JobCtx<'_>,
    errors: &mut Vec<OpIssue>,
) -> Result<BTreeMap<String, (PathBuf, SideInfo)>> {
    let md = fs::metadata(root).map_err(|e| CoreError::io(root, e))?;
    if !md.is_dir() {
        return Err(CoreError::Invalid(format!(
            "{} is not a folder",
            root.display()
        )));
    }
    let mut map = BTreeMap::new();
    walk_tree(root, ctx.cancel, |item| match item {
        WalkItem::Entry(e) => {
            ctx.progress.add_items(1);
            map.insert(
                key(&e.rel),
                (
                    e.rel.clone(),
                    SideInfo {
                        kind: e.kind,
                        size: e.size,
                        modified: e.modified,
                    },
                ),
            );
        }
        WalkItem::Error { path, error } => errors.push(OpIssue::new(path, error.to_string())),
    })?;
    Ok(map)
}

fn time_cmp(a: Option<SystemTime>, b: Option<SystemTime>, tol: Duration) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (Some(a), Some(b)) => match a.duration_since(b) {
            Ok(d) if d > tol => Greater,
            Ok(_) => Equal,
            Err(e) if e.duration() > tol => Less,
            Err(_) => Equal,
        },
        _ => Equal,
    }
}

/// Compare two folder trees (recursive, links not followed; links compare by target).
/// Name matching is case-insensitive on Windows.
pub fn compare_dirs(
    left: &Path,
    right: &Path,
    opts: &CompareOptions,
    ctx: &JobCtx<'_>,
) -> Result<CompareReport> {
    let mut report = CompareReport::default();
    let l = scan_side(left, ctx, &mut report.errors)?;
    let mut r = scan_side(right, ctx, &mut report.errors)?;
    for (k, (rel, li)) in l {
        ctx.cancel.check()?;
        let Some((_, ri)) = r.remove(&k) else {
            report.entries.push(CompareEntry {
                rel,
                left: Some(li),
                right: None,
                state: EntryState::LeftOnly,
            });
            continue;
        };
        let state = if li.kind != ri.kind {
            EntryState::TypeMismatch
        } else {
            match li.kind {
                NodeKind::Dir => EntryState::Same,
                NodeKind::Link => {
                    let lt = fs::read_link(left.join(&rel)).ok();
                    let rt = fs::read_link(right.join(&rel)).ok();
                    if lt.is_some() && lt == rt {
                        EntryState::Same
                    } else {
                        EntryState::Different(DiffReason::Content)
                    }
                }
                NodeKind::File => compare_files(left, right, &rel, &li, &ri, opts, ctx)?,
            }
        };
        report.entries.push(CompareEntry {
            rel,
            left: Some(li),
            right: Some(ri),
            state,
        });
    }
    for (_, (rel, ri)) in r {
        report.entries.push(CompareEntry {
            rel,
            left: None,
            right: Some(ri),
            state: EntryState::RightOnly,
        });
    }
    report.entries.sort_by_key(|e| key(&e.rel));
    Ok(report)
}

fn compare_files(
    left: &Path,
    right: &Path,
    rel: &Path,
    li: &SideInfo,
    ri: &SideInfo,
    opts: &CompareOptions,
    ctx: &JobCtx<'_>,
) -> Result<EntryState> {
    if li.size != ri.size {
        return Ok(EntryState::Different(DiffReason::Size));
    }
    if opts.use_hash {
        let h = |p: PathBuf| hashing::hash_file(&p, ctx.cancel, Some(ctx.progress));
        return Ok(match (h(left.join(rel)), h(right.join(rel))) {
            (Ok(a), Ok(b)) if a == b => EntryState::Same,
            (Ok(_), Ok(_)) => EntryState::Different(DiffReason::Content),
            (Err(CoreError::Cancelled), _) | (_, Err(CoreError::Cancelled)) => {
                return Err(CoreError::Cancelled);
            }
            (Err(e), _) | (_, Err(e)) => EntryState::Error(e.to_string()),
        });
    }
    Ok(
        match time_cmp(li.modified, ri.modified, opts.mtime_tolerance) {
            std::cmp::Ordering::Equal => EntryState::Same,
            std::cmp::Ordering::Greater => EntryState::Different(DiffReason::LeftNewer),
            std::cmp::Ordering::Less => EntryState::Different(DiffReason::RightNewer),
        },
    )
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMode {
    /// Copy new files and files that are newer on the left. Never deletes or overwrites a
    /// newer right-side file.
    Update,
    /// Make the right side identical to the left: copies, overwrites any difference and
    /// **deletes** right-only items.
    Mirror,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncAction {
    CreateDir {
        rel: PathBuf,
    },
    Copy {
        rel: PathBuf,
        overwrite: bool,
        bytes: u64,
    },
    /// Only produced in Mirror mode. Removes `right/rel` (recursively for folders).
    Delete {
        rel: PathBuf,
        is_dir: bool,
        /// Snapshot of the whole item tree, including every descendant of a folder.
        expected: Vec<SyncSnapshot>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPlan {
    pub left: PathBuf,
    pub right: PathBuf,
    pub mode: SyncMode,
    /// Order: deletions, then folder creation (parents first), then copies.
    pub actions: Vec<SyncAction>,
    pub bytes_to_copy: u64,
    /// Entries deliberately left alone, with the reason (shown in the preview).
    pub skipped: Vec<OpIssue>,
    /// File metadata and fingerprints observed while building each copy action. Execution
    /// checks these snapshots again so a stale preview cannot silently overwrite changed files.
    pub expected: Vec<SyncExpectation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentFingerprint {
    pub digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSnapshot {
    pub rel: PathBuf,
    pub side: SideInfo,
    pub fingerprint: Option<ContentFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncExpectation {
    pub rel: PathBuf,
    pub left: SideInfo,
    pub right: Option<SideInfo>,
    pub left_fingerprint: Option<ContentFingerprint>,
    pub right_fingerprint: Option<ContentFingerprint>,
}

impl SyncPlan {
    pub fn deletes(&self) -> usize {
        self.actions
            .iter()
            .filter(|a| matches!(a, SyncAction::Delete { .. }))
            .count()
    }
}

/// Build a sync plan (left → right) from a comparison.
pub fn plan_sync(report: &CompareReport, left: &Path, right: &Path, mode: SyncMode) -> SyncPlan {
    plan_sync_inner(report, left, right, mode, None)
}

/// Build a sync plan while updating a cancellable job's progress on large snapshots.
pub fn plan_sync_with_ctx(
    report: &CompareReport,
    left: &Path,
    right: &Path,
    mode: SyncMode,
    ctx: &JobCtx<'_>,
) -> SyncPlan {
    plan_sync_inner(report, left, right, mode, Some(ctx))
}

fn plan_sync_inner(
    report: &CompareReport,
    left: &Path,
    right: &Path,
    mode: SyncMode,
    ctx: Option<&JobCtx<'_>>,
) -> SyncPlan {
    let mirror = mode == SyncMode::Mirror;
    let mut deletes: Vec<SyncAction> = Vec::new();
    let mut mkdirs = Vec::new();
    let mut copies = Vec::new();
    let mut skipped = Vec::new();
    let mut expected = Vec::new();
    let mut deleted_dirs: Vec<&Path> = Vec::new();
    let mut bytes = 0;
    let mut copy =
        |entry: &CompareEntry, overwrite: bool, size: u64, copies: &mut Vec<SyncAction>| {
            if let Some(left_side) = entry.left {
                let left_fingerprint = fingerprint_for_side(&left.join(&entry.rel), left_side, ctx);
                let right_side = entry
                    .right
                    .filter(|side| overwrite && side.kind == NodeKind::File);
                let right_fingerprint = right_side
                    .and_then(|side| fingerprint_for_side(&right.join(&entry.rel), side, ctx));
                expected.push(SyncExpectation {
                    rel: entry.rel.clone(),
                    left: left_side,
                    right: right_side,
                    left_fingerprint,
                    right_fingerprint,
                });
            }
            bytes += size;
            copies.push(SyncAction::Copy {
                rel: entry.rel.clone(),
                overwrite,
                bytes: size,
            });
        };
    for e in &report.entries {
        let rel = e.rel.as_path();
        let lk = e.left.map(|i| i.kind);
        match &e.state {
            EntryState::Same => {}
            EntryState::Error(msg) => skipped.push(OpIssue::new(rel, msg.clone())),
            EntryState::LeftOnly => match lk {
                Some(NodeKind::Dir) => mkdirs.push(SyncAction::CreateDir { rel: rel.into() }),
                Some(NodeKind::File) => copy(e, false, e.left.unwrap().size, &mut copies),
                _ => skipped.push(OpIssue::new(rel, "links are not synced")),
            },
            EntryState::RightOnly => {
                if mirror {
                    if deleted_dirs.iter().any(|d| rel.starts_with(d)) {
                        continue; // removed with its parent
                    }
                    let is_dir = e.right.is_some_and(|i| i.kind == NodeKind::Dir);
                    if is_dir {
                        deleted_dirs.push(rel);
                    }
                    deletes.push(SyncAction::Delete {
                        rel: rel.into(),
                        is_dir,
                        expected: deletion_snapshot(report, right, rel, is_dir, ctx),
                    });
                }
            }
            EntryState::TypeMismatch => {
                if mirror && matches!(lk, Some(NodeKind::Dir | NodeKind::File)) {
                    let is_dir = e.right.is_some_and(|i| i.kind == NodeKind::Dir);
                    if is_dir {
                        deleted_dirs.push(rel);
                    }
                    deletes.push(SyncAction::Delete {
                        rel: rel.into(),
                        is_dir,
                        expected: deletion_snapshot(report, right, rel, is_dir, ctx),
                    });
                    if lk == Some(NodeKind::Dir) {
                        mkdirs.push(SyncAction::CreateDir { rel: rel.into() });
                    } else {
                        copy(e, false, e.left.unwrap().size, &mut copies);
                    }
                } else {
                    skipped.push(OpIssue::new(rel, "different item types on each side"));
                }
            }
            EntryState::Different(reason) => {
                if lk != Some(NodeKind::File) {
                    skipped.push(OpIssue::new(rel, "links are not synced"));
                    continue;
                }
                let size = e.left.unwrap().size;
                let left_newer = time_cmp(
                    e.left.and_then(|i| i.modified),
                    e.right.and_then(|i| i.modified),
                    Duration::from_secs(2),
                ) == std::cmp::Ordering::Greater;
                if mirror || *reason == DiffReason::LeftNewer || left_newer {
                    copy(e, true, size, &mut copies);
                } else {
                    skipped.push(OpIssue::new(
                        rel,
                        "right side is newer or the same age (Update never overwrites it)",
                    ));
                }
            }
        }
    }
    let mut actions = deletes;
    actions.extend(mkdirs);
    actions.extend(copies);
    SyncPlan {
        left: left.to_path_buf(),
        right: right.to_path_buf(),
        mode,
        actions,
        bytes_to_copy: bytes,
        skipped,
        expected,
    }
}

fn fingerprint_for_side(
    path: &Path,
    side: SideInfo,
    ctx: Option<&JobCtx<'_>>,
) -> Option<ContentFingerprint> {
    (side.kind == NodeKind::File)
        .then(|| content_fingerprint(path, side, ctx))
        .flatten()
}

fn deletion_snapshot(
    report: &CompareReport,
    right: &Path,
    rel: &Path,
    is_dir: bool,
    ctx: Option<&JobCtx<'_>>,
) -> Vec<SyncSnapshot> {
    let mut snapshots = report
        .entries
        .iter()
        .filter_map(|entry| {
            let side = entry.right?;
            let belongs = entry.rel == rel || (is_dir && entry.rel.starts_with(rel));
            belongs.then(|| SyncSnapshot {
                rel: entry.rel.clone(),
                side: SideInfo {
                    modified: snapshot_modified(side.kind, side.modified),
                    ..side
                },
                fingerprint: fingerprint_for_side(&right.join(&entry.rel), side, ctx),
            })
        })
        .collect::<Vec<_>>();
    snapshots.sort_by(|a, b| a.rel.cmp(&b.rel));
    snapshots
}

/// Deleter used for Mirror deletions; `None` means permanent deletion. The app can pass a
/// Recycle Bin implementation from `forge-win`.
pub type Deleter<'a> = &'a mut dyn FnMut(&Path) -> std::result::Result<(), String>;

const FULL_FINGERPRINT_LIMIT: u64 = 1024 * 1024;
const FINGERPRINT_BLOCK: usize = 64 * 1024;

/// Snapshot content without following a symlink or junction. Small files get a full hash;
/// larger files use a bounded sample so preview and revalidation cost stays predictable.
fn content_fingerprint(
    path: &Path,
    expected: SideInfo,
    ctx: Option<&JobCtx<'_>>,
) -> Option<ContentFingerprint> {
    if expected.kind != NodeKind::File || ctx.is_some_and(|ctx| ctx.cancel.check().is_err()) {
        return None;
    }
    let before = fs::symlink_metadata(path).ok()?;
    if sync_node_kind(&before) != NodeKind::File
        || before.len() != expected.size
        || before.modified().ok() != expected.modified
    {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"ZXraw sync snapshot v1\0");
    hasher.update(&expected.size.to_le_bytes());
    match expected.modified {
        Some(modified) => match modified.duration_since(std::time::UNIX_EPOCH) {
            Ok(time) => {
                hasher.update(&[1]);
                hasher.update(&time.as_secs().to_le_bytes());
                hasher.update(&time.subsec_nanos().to_le_bytes());
            }
            Err(error) => {
                hasher.update(&[2]);
                hasher.update(&error.duration().as_secs().to_le_bytes());
                hasher.update(&error.duration().subsec_nanos().to_le_bytes());
            }
        },
        None => {
            hasher.update(&[0]);
        }
    }

    if expected.size <= FULL_FINGERPRINT_LIMIT {
        let mut remaining = expected.size;
        let mut buffer = [0u8; FINGERPRINT_BLOCK];
        while remaining > 0 {
            if ctx.is_some_and(|ctx| ctx.cancel.check().is_err()) {
                return None;
            }
            let want = remaining.min(buffer.len() as u64) as usize;
            let read = file.read(&mut buffer[..want]).ok()?;
            if read == 0 {
                return None;
            }
            hasher.update(&buffer[..read]);
            remaining -= read as u64;
        }
        let mut trailing = [0u8; 1];
        if file.read(&mut trailing).ok()? != 0 {
            return None;
        }
    } else {
        // Unsampled middle bytes can change while size and mtime are preserved. The fixed
        // head, tail, and interior samples keep this guard bounded, but are not a full-file
        // proof for large files.
        let last = expected.size - FINGERPRINT_BLOCK as u64;
        let mut offsets = vec![
            0,
            expected.size / 4,
            expected.size / 2,
            expected.size * 3 / 4,
            last,
        ];
        offsets.sort_unstable();
        offsets.dedup();
        for offset in offsets {
            if ctx.is_some_and(|ctx| ctx.cancel.check().is_err()) {
                return None;
            }
            file.seek(SeekFrom::Start(offset)).ok()?;
            let mut block = [0u8; FINGERPRINT_BLOCK];
            file.read_exact(&mut block).ok()?;
            hasher.update(&offset.to_le_bytes());
            hasher.update(&block);
        }
    }

    let after = fs::symlink_metadata(path).ok()?;
    if sync_node_kind(&after) != NodeKind::File
        || after.len() != expected.size
        || after.modified().ok() != expected.modified
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return None;
    }
    Some(ContentFingerprint {
        digest: *hasher.finalize().as_bytes(),
    })
}

fn sync_node_kind(metadata: &fs::Metadata) -> NodeKind {
    if metadata.file_type().is_symlink() || is_reparse_point(metadata) {
        NodeKind::Link
    } else if metadata.is_dir() {
        NodeKind::Dir
    } else {
        NodeKind::File
    }
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn metadata_matches(actual: &fs::Metadata, expected: SideInfo) -> bool {
    sync_node_kind(actual) == expected.kind
        && (expected.kind != NodeKind::File || actual.len() == expected.size)
        && actual.modified().ok() == expected.modified
}

fn matches_snapshot(
    path: &Path,
    expected: Option<SideInfo>,
    fingerprint: Option<ContentFingerprint>,
    ctx: &JobCtx<'_>,
) -> bool {
    match (expected, fs::symlink_metadata(path)) {
        (Some(expected), Ok(actual)) if metadata_matches(&actual, expected) => {
            if expected.kind == NodeKind::File {
                fingerprint.is_some_and(|fingerprint| {
                    content_fingerprint(path, expected, Some(ctx)) == Some(fingerprint)
                })
            } else {
                fingerprint.is_none()
            }
        }
        (None, Err(error)) => error.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

fn indexed_expectations(expected: &[SyncExpectation]) -> HashMap<&Path, &SyncExpectation> {
    expected
        .iter()
        .map(|snapshot| (snapshot.rel.as_path(), snapshot))
        .collect()
}

fn matches_delete_snapshot(
    right: &Path,
    rel: &Path,
    expected: &[SyncSnapshot],
    ctx: &JobCtx<'_>,
) -> bool {
    let root = right.join(rel);
    let Some(actual) = current_tree_snapshot(&root, right, ctx) else {
        return false;
    };
    if actual.len() != expected.len() {
        return false;
    }
    let by_path: HashMap<String, &SyncSnapshot> = expected
        .iter()
        .map(|snapshot| (key(&snapshot.rel), snapshot))
        .collect();
    for (path, actual_side) in actual {
        let Some(snapshot) = by_path.get(&key(&path)) else {
            return false;
        };
        if !side_matches(actual_side, snapshot.side) {
            return false;
        }
        if snapshot.side.kind == NodeKind::File {
            let Some(expected_fingerprint) = snapshot.fingerprint else {
                return false;
            };
            if content_fingerprint(&right.join(&path), actual_side, Some(ctx))
                != Some(expected_fingerprint)
            {
                return false;
            }
        }
    }
    true
}

fn current_tree_snapshot(
    root: &Path,
    relative_to: &Path,
    ctx: &JobCtx<'_>,
) -> Option<Vec<(PathBuf, SideInfo)>> {
    let root_metadata = fs::symlink_metadata(root).ok()?;
    let mut stack = vec![(root.to_path_buf(), root_metadata)];
    let mut entries = Vec::new();
    while let Some((path, metadata)) = stack.pop() {
        if ctx.cancel.check().is_err() {
            return None;
        }
        let kind = sync_node_kind(&metadata);
        let relative = path.strip_prefix(relative_to).ok()?.to_path_buf();
        let side = SideInfo {
            kind,
            size: if kind == NodeKind::File {
                metadata.len()
            } else {
                0
            },
            modified: snapshot_modified(kind, metadata.modified().ok()),
        };
        entries.push((relative, side));
        if kind == NodeKind::Dir {
            for item in fs::read_dir(&path).ok()? {
                if ctx.cancel.check().is_err() {
                    return None;
                }
                let item = item.ok()?;
                let child = item.path();
                let child_metadata = fs::symlink_metadata(&child).ok()?;
                stack.push((child, child_metadata));
            }
        }
    }
    Some(entries)
}

/// Folder timestamps are not part of a delete snapshot. NTFS refreshes a child folder's time
/// in its parent's listing lazily, so a listing can disagree with the folder's own metadata,
/// and every change inside a folder already alters the item list or a file's own snapshot.
fn snapshot_modified(kind: NodeKind, modified: Option<SystemTime>) -> Option<SystemTime> {
    if kind == NodeKind::Dir {
        None
    } else {
        modified
    }
}

fn side_matches(actual: SideInfo, expected: SideInfo) -> bool {
    actual.kind == expected.kind
        && (expected.kind != NodeKind::File || actual.size == expected.size)
        && actual.modified == expected.modified
}

/// Execute a previewed sync plan.
pub fn execute_sync(
    plan: &SyncPlan,
    verify: bool,
    mut deleter: Option<Deleter<'_>>,
    ctx: &JobCtx<'_>,
) -> OpReport {
    let mut report = OpReport::default();
    ctx.progress.add_total_items(plan.actions.len() as u64);
    ctx.progress.add_total_bytes(plan.bytes_to_copy);
    if plan.mode != SyncMode::Mirror && plan.deletes() > 0 {
        report.failed.push(OpIssue::new(
            &plan.right,
            "refusing to delete: plan is not a Mirror plan",
        ));
        return report;
    }
    let fail = |report: &mut OpReport, p: &Path, why: String| {
        ctx.log(format!("failed: {}: {why}", p.display()));
        ctx.progress.add_error();
        report.failed.push(OpIssue::new(p, why));
    };
    let expected_by_rel = indexed_expectations(&plan.expected);
    for action in &plan.actions {
        if ctx.cancel.check().is_err() {
            report.cancelled = true;
            break;
        }
        match action {
            SyncAction::Delete { rel, expected, .. } => {
                let p = plan.right.join(rel);
                if !matches_delete_snapshot(&plan.right, rel, expected, ctx) {
                    if ctx.cancel.check().is_err() {
                        report.cancelled = true;
                        break;
                    }
                    fail(
                        &mut report,
                        &p,
                        "destination changed since preview; item was not deleted; rerun compare"
                            .into(),
                    );
                    ctx.progress.add_items(1);
                    continue;
                }
                match deleter.as_mut() {
                    Some(d) => match d(&p) {
                        Ok(()) => report.done.push(p),
                        Err(e) => fail(&mut report, &p, e),
                    },
                    None => {
                        let r = ops::delete_permanently(std::slice::from_ref(&p), ctx);
                        report.failed.extend(r.failed);
                        report.done.extend(r.done);
                    }
                }
            }
            SyncAction::CreateDir { rel } => {
                let p = plan.right.join(rel);
                if let Err(e) = fs::create_dir_all(&p) {
                    fail(&mut report, &p, e.to_string());
                }
            }
            SyncAction::Copy {
                rel,
                overwrite,
                bytes,
            } => {
                let src = plan.left.join(rel);
                let dest = plan.right.join(rel);
                let expected = expected_by_rel.get(rel.as_path());
                if expected.is_none_or(|snapshot| {
                    !matches_snapshot(&src, Some(snapshot.left), snapshot.left_fingerprint, ctx)
                        || !matches_snapshot(&dest, snapshot.right, snapshot.right_fingerprint, ctx)
                }) {
                    if ctx.cancel.check().is_err() {
                        report.cancelled = true;
                        break;
                    }
                    ctx.progress.add_bytes(*bytes);
                    fail(
                        &mut report,
                        &dest,
                        "source or destination changed since comparison; rerun compare".into(),
                    );
                    ctx.progress.add_items(1);
                    continue;
                }
                ctx.progress.set_current(src.display().to_string());
                let opts = CopyFileOptions {
                    overwrite: *overwrite,
                    verify,
                    want_hash: false,
                };
                match ops::copy_file_atomic(&src, &dest, opts, ctx) {
                    Ok(out) => {
                        report.bytes += out.bytes;
                        report.done.push(dest);
                    }
                    Err(CoreError::Cancelled) => {
                        report.cancelled = true;
                        break;
                    }
                    Err(e) => {
                        ctx.progress.add_bytes(*bytes);
                        fail(&mut report, &src, e.to_string());
                    }
                }
            }
        }
        ctx.progress.add_items(1);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_jobs::{CancelToken, Progress};
    use std::fs::File;

    fn set_mtime(p: &Path, secs: u64) {
        File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let l = d.path().join("L");
        let r = d.path().join("R");
        fs::create_dir_all(l.join("sub/newdir")).unwrap();
        fs::create_dir_all(r.join("sub")).unwrap();
        fs::create_dir_all(r.join("extra/inner")).unwrap();
        fs::write(l.join("same.txt"), b"same").unwrap();
        fs::write(r.join("same.txt"), b"same").unwrap();
        set_mtime(&l.join("same.txt"), 1_000_000_000);
        set_mtime(&r.join("same.txt"), 1_000_000_001);
        fs::write(l.join("sub/newer.txt"), b"left").unwrap();
        fs::write(r.join("sub/newer.txt"), b"rght").unwrap();
        set_mtime(&l.join("sub/newer.txt"), 1_600_000_000);
        set_mtime(&r.join("sub/newer.txt"), 1_500_000_000);
        fs::write(l.join("older.txt"), b"left").unwrap();
        fs::write(r.join("older.txt"), b"rght").unwrap();
        set_mtime(&l.join("older.txt"), 1_500_000_000);
        set_mtime(&r.join("older.txt"), 1_600_000_000);
        fs::write(l.join("size.txt"), b"longer").unwrap();
        fs::write(r.join("size.txt"), b"short").unwrap();
        fs::write(l.join("sub/newdir/only-left.txt"), b"L").unwrap();
        fs::write(r.join("extra/inner/only-right.txt"), b"R").unwrap();
        fs::write(r.join("right.txt"), b"R").unwrap();
        (d, l, r)
    }

    #[test]
    fn compare_and_update_never_deletes() {
        let (_d, l, r) = setup();
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let ctx = JobCtx::new(&cancel, &progress);
        let rep = compare_dirs(&l, &r, &CompareOptions::default(), &ctx).unwrap();
        let state = |p: &str| {
            rep.entries
                .iter()
                .find(|e| crate::paths::to_slash(&e.rel) == p)
                .map(|e| e.state.clone())
                .unwrap()
        };
        assert_eq!(state("same.txt"), EntryState::Same);
        assert_eq!(
            state("sub/newer.txt"),
            EntryState::Different(DiffReason::LeftNewer)
        );
        assert_eq!(
            state("older.txt"),
            EntryState::Different(DiffReason::RightNewer)
        );
        assert_eq!(state("size.txt"), EntryState::Different(DiffReason::Size));
        assert_eq!(state("sub/newdir"), EntryState::LeftOnly);
        assert_eq!(state("extra/inner/only-right.txt"), EntryState::RightOnly);

        let plan = plan_sync(&rep, &l, &r, SyncMode::Update);
        assert_eq!(plan.deletes(), 0);
        let rep2 = execute_sync(&plan, true, None, &ctx);
        assert!(rep2.failed.is_empty(), "{rep2:?}");
        assert_eq!(fs::read(r.join("sub/newer.txt")).unwrap(), b"left");
        assert_eq!(
            fs::read(r.join("older.txt")).unwrap(),
            b"rght",
            "newer right kept"
        );
        assert_eq!(fs::read(r.join("sub/newdir/only-left.txt")).unwrap(), b"L");
        assert!(r.join("right.txt").exists());
        assert!(r.join("extra/inner/only-right.txt").exists());
    }

    #[test]
    fn update_refuses_files_changed_after_the_preview() {
        for change_source in [true, false] {
            let (_d, l, r) = setup();
            let cancel = CancelToken::new();
            let progress = Progress::default();
            let ctx = JobCtx::new(&cancel, &progress);
            let report = compare_dirs(&l, &r, &CompareOptions::default(), &ctx).unwrap();
            let plan = plan_sync(&report, &l, &r, SyncMode::Update);
            let left = l.join("sub/newer.txt");
            let right = r.join("sub/newer.txt");
            let changed = if change_source { &left } else { &right };
            let original_mtime = fs::metadata(changed).unwrap().modified().unwrap();

            // Preserve both size and timestamp: only the content fingerprint detects this.
            fs::write(changed, b"edit").unwrap();
            File::options()
                .write(true)
                .open(changed)
                .unwrap()
                .set_modified(original_mtime)
                .unwrap();
            let expected_right = fs::read(&right).unwrap();
            let result = execute_sync(&plan, true, None, &ctx);

            assert_eq!(fs::read(&right).unwrap(), expected_right);
            assert!(
                result.failed.iter().any(|issue| {
                    issue.path == right && issue.reason.contains("changed since comparison")
                }),
                "{result:?}"
            );
        }
    }

    #[test]
    fn mirror_skips_a_changed_file_and_changed_file_inside_a_deleted_folder() {
        for nested in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let left = temp.path().join("left");
            let right = temp.path().join("right");
            fs::create_dir_all(&left).unwrap();
            let changed = if nested {
                right.join("outer/inner/photo.bin")
            } else {
                right.join("photo.bin")
            };
            fs::create_dir_all(changed.parent().unwrap()).unwrap();
            fs::write(&changed, b"original").unwrap();
            let cancel = CancelToken::new();
            let progress = Progress::default();
            let ctx = JobCtx::new(&cancel, &progress);
            let report = compare_dirs(&left, &right, &CompareOptions::default(), &ctx).unwrap();
            let plan = plan_sync(&report, &left, &right, SyncMode::Mirror);
            let original_mtime = fs::metadata(&changed).unwrap().modified().unwrap();
            fs::write(&changed, b"revised!").unwrap();
            File::options()
                .write(true)
                .open(&changed)
                .unwrap()
                .set_modified(original_mtime)
                .unwrap();
            let mut deleted = Vec::new();
            let mut deleter = |path: &Path| {
                deleted.push(path.to_path_buf());
                Ok(())
            };

            let result = execute_sync(&plan, false, Some(&mut deleter), &ctx);

            assert!(
                deleted.is_empty(),
                "stale delete must not reach the deleter"
            );
            assert!(changed.exists(), "changed bytes must remain on disk");
            assert!(result.failed.iter().any(|issue| {
                issue.reason.contains("changed since preview")
                    && issue.reason.contains("not deleted")
            }));
        }
    }

    #[test]
    fn mirror_makes_identical() {
        let (_d, l, r) = setup();
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let ctx = JobCtx::new(&cancel, &progress);
        let opts = CompareOptions {
            use_hash: true,
            ..Default::default()
        };
        let rep = compare_dirs(&l, &r, &opts, &ctx).unwrap();
        let plan = plan_sync(&rep, &l, &r, SyncMode::Mirror);
        // "extra" folder deleted as one action (its contents are covered).
        assert_eq!(plan.deletes(), 2);
        let mut deleted = Vec::new();
        let mut deleter = |p: &Path| {
            deleted.push(p.to_path_buf());
            ops::delete_permanently(
                &[p.to_path_buf()],
                &JobCtx::new(&CancelToken::new(), &Progress::default()),
            );
            Ok(())
        };
        let res = execute_sync(&plan, false, Some(&mut deleter), &ctx);
        assert!(res.failed.is_empty(), "{res:?}");
        assert_eq!(deleted.len(), 2);
        let after = compare_dirs(&l, &r, &opts, &ctx).unwrap();
        assert!(
            after.entries.iter().all(|e| e.state == EntryState::Same),
            "{:?}",
            after.entries
        );
    }

    #[test]
    fn non_mirror_plan_with_deletes_is_refused() {
        let (_d, l, r) = setup();
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let ctx = JobCtx::new(&cancel, &progress);
        let report = compare_dirs(&l, &r, &CompareOptions::default(), &ctx).unwrap();
        let mut plan = plan_sync(&report, &l, &r, SyncMode::Mirror);
        plan.mode = SyncMode::Update;
        let res = execute_sync(&plan, false, None, &ctx);
        assert_eq!(res.failed.len(), 1);
        assert!(r.join("right.txt").exists());
        plan.mode = SyncMode::Mirror;
        let res = execute_sync(&plan, false, None, &ctx);
        assert!(res.failed.is_empty());
        assert!(!r.join("right.txt").exists());
    }

    #[test]
    fn expectation_lookup_scales_to_fifty_thousand_actions() {
        // Sources do not exist, so every action fails its snapshot check without copying.
        // The run therefore measures only the per-action expectation lookup.
        const COUNT: usize = 50_000;
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        let side = SideInfo {
            kind: NodeKind::File,
            size: 1,
            modified: None,
        };
        let fingerprint = Some(ContentFingerprint { digest: [7; 32] });
        let mut expected = Vec::with_capacity(COUNT);
        let mut actions = Vec::with_capacity(COUNT);
        for index in 0..COUNT {
            let rel = PathBuf::from(format!("file-{index:05}.bin"));
            expected.push(SyncExpectation {
                rel: rel.clone(),
                left: side,
                right: None,
                left_fingerprint: fingerprint,
                right_fingerprint: None,
            });
            actions.push(SyncAction::Copy {
                rel,
                overwrite: false,
                bytes: 1,
            });
        }
        let plan = SyncPlan {
            left,
            right,
            mode: SyncMode::Update,
            actions,
            bytes_to_copy: COUNT as u64,
            skipped: Vec::new(),
            expected,
        };

        let cancel = CancelToken::new();
        let progress = Progress::default();
        let ctx = JobCtx::new(&cancel, &progress);
        let started = std::time::Instant::now();
        let report = execute_sync(&plan, false, None, &ctx);
        let elapsed = started.elapsed();
        assert_eq!(report.failed.len(), COUNT);
        assert!(
            elapsed < Duration::from_secs(30),
            "50,000 indexed lookups took {elapsed:?}"
        );
    }
}
