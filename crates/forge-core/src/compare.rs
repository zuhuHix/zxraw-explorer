//! Folder comparison and one-way sync planning.
//!
//! `compare_dirs` is read-only. `plan_sync` turns a comparison into a list of actions that
//! the UI shows *before* anything happens; `execute_sync` then performs exactly those actions
//! using the atomic copy from [`crate::ops`]. Deletions only ever appear in
//! [`SyncMode::Mirror`] plans.

use std::collections::BTreeMap;
use std::fs;
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
    /// File metadata observed while building each copy action. Execution checks these
    /// snapshots again so a stale preview cannot silently overwrite changed files.
    pub expected: Vec<SyncExpectation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncExpectation {
    pub rel: PathBuf,
    pub left: SideInfo,
    pub right: Option<SideInfo>,
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
            if let Some(left) = entry.left {
                expected.push(SyncExpectation {
                    rel: entry.rel.clone(),
                    left,
                    right: entry.right,
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

/// Deleter used for Mirror deletions; `None` means permanent deletion. The app can pass a
/// Recycle Bin implementation from `forge-win`.
pub type Deleter<'a> = &'a mut dyn FnMut(&Path) -> std::result::Result<(), String>;

fn matches_snapshot(path: &Path, expected: Option<SideInfo>) -> bool {
    match (expected, fs::symlink_metadata(path)) {
        (Some(expected), Ok(actual)) => {
            crate::ops::node_kind(&actual) == expected.kind
                && actual.len() == expected.size
                && actual.modified().ok() == expected.modified
        }
        (None, Err(error)) => error.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
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
    for action in &plan.actions {
        if ctx.cancel.check().is_err() {
            report.cancelled = true;
            break;
        }
        match action {
            SyncAction::Delete { rel, .. } => {
                let p = plan.right.join(rel);
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
                let expected = plan.expected.iter().find(|snapshot| snapshot.rel == *rel);
                if expected.is_none_or(|snapshot| {
                    !matches_snapshot(&src, Some(snapshot.left))
                        || !matches_snapshot(&dest, snapshot.right)
                }) {
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

            if change_source {
                fs::write(&left, b"changed after compare").unwrap();
                set_mtime(&left, 1_700_000_000);
            } else {
                fs::write(&right, b"changed after compare").unwrap();
                set_mtime(&right, 1_700_000_000);
            }
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
        let mut plan = SyncPlan {
            left: l,
            right: r.clone(),
            mode: SyncMode::Update,
            actions: vec![SyncAction::Delete {
                rel: "right.txt".into(),
                is_dir: false,
            }],
            bytes_to_copy: 0,
            skipped: vec![],
            expected: vec![],
        };
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let res = execute_sync(&plan, false, None, &JobCtx::new(&cancel, &progress));
        assert_eq!(res.failed.len(), 1);
        assert!(r.join("right.txt").exists());
        plan.mode = SyncMode::Mirror;
        let res = execute_sync(&plan, false, None, &JobCtx::new(&cancel, &progress));
        assert!(res.failed.is_empty());
        assert!(!r.join("right.txt").exists());
    }
}
