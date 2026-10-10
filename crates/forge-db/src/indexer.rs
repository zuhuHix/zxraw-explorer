//! Resumable, cancellable folder indexer.
//!
//! * Walks each enabled index root depth-first with `std::fs`, never descending into
//!   symlinks, junctions or other reparse points (they are recorded as `link` rows).
//! * Exclusions (folders, names, extensions) are honoured; excluded entries are not
//!   recorded, and previously indexed rows under them become offline at the end of a pass.
//! * File rows are upserted in batches (default 2,000) — each batch is one transaction
//!   that also stores the checkpoint (the stack of folders still to list), so after a
//!   crash or cancel the next run resumes where the last committed batch left off.
//! * When a pass completes, rows under the root that this pass did not see (and that
//!   were not refreshed by anyone else since the pass began) are marked `present = 0`.
//!   Folders that could not be read do not cause their contents to be marked missing.
//! * [`Indexer::reconcile_dir`] refreshes one folder (non-recursive) — use it after file
//!   operations or a filesystem-watcher event.
//!
//! Metadata extraction is not done here; see `Db::upsert_image_meta` /
//! `Db::upsert_video_meta` and `Db::files_needing_meta`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_core::model::EntryKind;
use forge_jobs::{CancelToken, Progress};
use rusqlite::{Transaction, params};
use serde::Serialize;

use crate::error::{DbError, Result};
use crate::files::{
    NewFile, VolumeCache, VolumeId, VolumeInfo, mark_subtree_missing_tx, normalize_dir,
    subtree_bounds, system_time_ms, upsert_files_tx, upsert_volume_tx,
};
use crate::settings::{IndexRoot, ScanStatus};
use crate::{Db, now_ms, path_str};

pub const DEFAULT_BATCH: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum IndexOutcome {
    Completed,
    Cancelled,
    /// The root folder could not be opened (e.g. drive disconnected). Its files were
    /// marked offline; annotations are kept.
    RootUnavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RootReport {
    pub root_id: i64,
    pub root: PathBuf,
    pub outcome: IndexOutcome,
    /// Continued from a checkpoint of an earlier, unfinished run.
    pub resumed: bool,
    /// Entries upserted during this run.
    pub items: u64,
    /// Folders listed during this run.
    pub dirs: u64,
    pub errors: u64,
    /// Rows marked not present at the end of the pass.
    pub marked_missing: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IndexReport {
    pub roots: Vec<RootReport>,
}

impl IndexReport {
    pub fn cancelled(&self) -> bool {
        self.roots
            .iter()
            .any(|r| r.outcome == IndexOutcome::Cancelled)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReconcileReport {
    pub upserted: u64,
    pub marked_missing: u64,
}

/// Information passed to the `on_batch` hook after each committed batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchCommitted {
    pub root_id: i64,
    pub rows: usize,
    pub items_total: u64,
    pub pending_dirs: usize,
}

type VolumeResolver<'a> = Box<dyn Fn(&Path) -> Option<VolumeInfo> + Send + 'a>;
type BatchHook<'a> = Box<dyn FnMut(&BatchCommitted) + Send + 'a>;

pub struct Indexer<'a> {
    db: &'a Db,
    batch_size: usize,
    cancel: CancelToken,
    progress: Arc<Progress>,
    volume_resolver: Option<VolumeResolver<'a>>,
    on_batch: Option<BatchHook<'a>>,
    #[cfg(test)]
    fail_read_dirs: HashSet<PathBuf>,
    #[cfg(test)]
    fail_metadata_paths: HashSet<PathBuf>,
}

impl<'a> Indexer<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self {
            db,
            batch_size: DEFAULT_BATCH,
            cancel: CancelToken::new(),
            progress: Progress::new(),
            volume_resolver: None,
            on_batch: None,
            #[cfg(test)]
            fail_read_dirs: HashSet::new(),
            #[cfg(test)]
            fail_metadata_paths: HashSet::new(),
        }
    }

    pub fn batch_size(mut self, n: usize) -> Self {
        self.batch_size = n.max(1);
        self
    }

    pub fn cancel_token(mut self, token: CancelToken) -> Self {
        self.cancel = token;
        self
    }

    pub fn progress(mut self, progress: Arc<Progress>) -> Self {
        self.progress = progress;
        self
    }

    /// Supply volume identity (serial/label) for a root path, e.g. from forge-win.
    /// Without it, volumes are identified by their root (`E:\`).
    pub fn volume_resolver(mut self, f: impl Fn(&Path) -> Option<VolumeInfo> + Send + 'a) -> Self {
        self.volume_resolver = Some(Box::new(f));
        self
    }

    /// Called after every committed batch (progress UIs, tests).
    pub fn on_batch(mut self, f: impl FnMut(&BatchCommitted) + Send + 'a) -> Self {
        self.on_batch = Some(Box::new(f));
        self
    }

    pub fn progress_handle(&self) -> Arc<Progress> {
        self.progress.clone()
    }

    #[cfg(test)]
    fn fail_read_dir_for_test(mut self, path: &Path) -> Self {
        self.fail_read_dirs.insert(path.to_path_buf());
        self
    }

    #[cfg(test)]
    fn fail_metadata_for_test(mut self, path: &Path) -> Self {
        self.fail_metadata_paths.insert(path.to_path_buf());
        self
    }

    fn read_dir(&self, dir: &Path) -> std::io::Result<fs::ReadDir> {
        #[cfg(test)]
        if self.fail_read_dirs.contains(dir) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected read_dir failure",
            ));
        }
        fs::read_dir(dir)
    }

    fn entry_metadata(&self, entry: &fs::DirEntry, _path: &Path) -> std::io::Result<fs::Metadata> {
        #[cfg(test)]
        if self.fail_metadata_paths.contains(_path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected metadata failure",
            ));
        }
        entry.metadata()
    }

    /// Mark rows under an entry as observed despite an enumeration error. When the entry's
    /// own metadata failed, include its row too because it may be an unreadable directory.
    fn mark_entries_seen(&self, path: &Path, scan_id: i64, include_self: bool) -> Result<()> {
        let path_s = path_str(path);
        let (lo, hi) = subtree_bounds(&path_s);
        self.db.with_conn(|c| {
            if include_self {
                c.execute(
                    "UPDATE files SET seen_scan = ?4
                     WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
                    params![path_s, lo, hi, scan_id],
                )?;
            } else {
                c.execute(
                    "UPDATE files SET seen_scan = ?3 WHERE path >= ?1 AND path < ?2",
                    params![lo, hi, scan_id],
                )?;
            }
            Ok(())
        })
    }

    fn volume_for(&self, path: &Path) -> Result<VolumeId> {
        let info = self
            .volume_resolver
            .as_ref()
            .and_then(|f| f(path))
            .unwrap_or_else(|| VolumeInfo::from_path_root(path));
        self.db.with_tx(|tx| upsert_volume_tx(tx, &info))
    }

    /// Index every enabled root. Stops at the first cancellation.
    pub fn run(&mut self) -> Result<IndexReport> {
        let mut report = IndexReport::default();
        for root in self.db.index_roots()?.into_iter().filter(|r| r.enabled) {
            let r = self.index_root_inner(&root)?;
            let cancelled = r.outcome == IndexOutcome::Cancelled;
            report.roots.push(r);
            if cancelled {
                break;
            }
        }
        Ok(report)
    }

    /// Index one root by id.
    pub fn index_root(&mut self, root_id: i64) -> Result<RootReport> {
        let root = self
            .db
            .index_roots()?
            .into_iter()
            .find(|r| r.id == root_id)
            .ok_or_else(|| DbError::NotFound(format!("index root {root_id}")))?;
        self.index_root_inner(&root)
    }

    fn index_root_inner(&mut self, root: &IndexRoot) -> Result<RootReport> {
        let root_s = normalize_dir(&path_str(&root.path));
        let root_path = PathBuf::from(&root_s);
        let mut report = RootReport {
            root_id: root.id,
            root: root_path.clone(),
            outcome: IndexOutcome::Completed,
            resumed: false,
            items: 0,
            dirs: 0,
            errors: 0,
            marked_missing: 0,
        };
        let exclusions = self.db.exclusions()?;

        // Root reachable?
        let root_md = match fs::symlink_metadata(&root_path) {
            Ok(md) if md.is_dir() => md,
            Ok(_) => {
                return Err(DbError::Invalid(format!(
                    "index root {root_s} is not a folder"
                )));
            }
            Err(e) => {
                let msg = e.to_string();
                let now = now_ms();
                let n = self.db.with_tx(|tx| {
                    let n = mark_subtree_missing_tx(tx, &root_s, true)?;
                    tx.execute(
                        "INSERT INTO index_state(root_id, scan_id, status, started_at, updated_at, last_error)
                         VALUES (?1, ?2, 'unavailable', ?2, ?2, ?3)
                         ON CONFLICT(root_id) DO UPDATE SET status = CASE WHEN status = 'complete'
                             THEN 'unavailable' ELSE status END, updated_at = ?2, last_error = ?3",
                        params![root.id, now, msg],
                    )?;
                    Ok(n)
                })?;
                report.marked_missing = n;
                report.outcome = IndexOutcome::RootUnavailable(msg);
                return Ok(report);
            }
        };

        let volume = self.volume_for(&root_path)?;

        // Resume or start a pass.
        let state = self.db.index_state(root.id)?;
        let (scan_id, mut stack, resumed) = match state {
            Some(s)
                if matches!(
                    s.status,
                    ScanStatus::Running | ScanStatus::Interrupted | ScanStatus::Unavailable
                ) && !s.pending.is_empty() =>
            {
                (s.scan_id, s.pending, true)
            }
            _ => (now_ms(), vec![root_path.clone()], false),
        };
        report.resumed = resumed;
        let mut batch: Vec<NewFile> = Vec::with_capacity(self.batch_size.min(65_536));
        if resumed {
            self.db.with_conn(|c| {
                c.execute(
                    "UPDATE index_state SET status = 'running', updated_at = ?2 WHERE root_id = ?1",
                    params![root.id, now_ms()],
                )?;
                Ok(())
            })?;
        } else {
            self.db.with_conn(|c| {
                c.execute(
                    "INSERT INTO index_state(root_id, scan_id, status, started_at, updated_at, pending,
                                             items_seen, errors, finished_at, last_error)
                     VALUES (?1, ?2, 'running', ?2, ?2, ?3, 0, 0, NULL, NULL)
                     ON CONFLICT(root_id) DO UPDATE SET scan_id = excluded.scan_id, status = 'running',
                         started_at = excluded.started_at, updated_at = excluded.updated_at,
                         pending = excluded.pending, items_seen = 0, errors = 0, finished_at = NULL,
                         last_error = NULL",
                    params![root.id, scan_id, pending_json(&stack)],
                )?;
                Ok(())
            })?;
            batch.push(new_file(&root_path, EntryKind::Dir, &root_md));
        }

        let mut last_error: Option<String> = None;
        let mut items_since_check = 0usize;

        while let Some(dir) = stack.pop() {
            let base_len = stack.len();
            if self.cancel.check().is_err() {
                stack.push(dir);
                self.flush(
                    root.id,
                    scan_id,
                    volume,
                    &mut batch,
                    &stack,
                    &mut report,
                    &last_error,
                    ScanStatus::Interrupted,
                )?;
                report.outcome = IndexOutcome::Cancelled;
                return Ok(report);
            }
            self.progress.set_current(path_str(&dir));
            let rd = match self.read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) => {
                    report.errors += 1;
                    self.progress.add_error();
                    last_error = Some(format!("{}: {e}", dir.display()));
                    tracing::debug!("index: cannot list {}: {e}", dir.display());
                    // Keep what we knew about this folder's contents: refresh their
                    // scan id so the end-of-pass sweep does not mark them missing.
                    self.mark_entries_seen(&dir, scan_id, false)?;
                    continue;
                }
            };
            report.dirs += 1;
            for de in rd {
                items_since_check += 1;
                if items_since_check >= 256 {
                    items_since_check = 0;
                    if self.cancel.check().is_err() {
                        // Re-list this folder on resume; its already-pushed subfolders are
                        // dropped from the checkpoint to avoid listing them twice.
                        let mut pending = stack[..base_len].to_vec();
                        pending.push(dir.clone());
                        self.flush(
                            root.id,
                            scan_id,
                            volume,
                            &mut batch,
                            &pending,
                            &mut report,
                            &last_error,
                            ScanStatus::Interrupted,
                        )?;
                        report.outcome = IndexOutcome::Cancelled;
                        return Ok(report);
                    }
                }
                let de = match de {
                    Ok(de) => de,
                    Err(e) => {
                        report.errors += 1;
                        self.progress.add_error();
                        last_error = Some(format!("{}: {e}", dir.display()));
                        // An iterator error has no usable entry path. Preserve this whole
                        // directory's known contents because enumeration was incomplete.
                        self.mark_entries_seen(&dir, scan_id, false)?;
                        continue;
                    }
                };
                let path = de.path();
                let name_lc = de.file_name().to_string_lossy().to_lowercase();
                if exclusions.excludes_name(&name_lc) {
                    continue;
                }
                let md = match self.entry_metadata(&de, &path) {
                    Ok(md) => md,
                    Err(e) => {
                        report.errors += 1;
                        self.progress.add_error();
                        last_error = Some(format!("{}: {e}", path.display()));
                        // The row may represent a file or an unreadable directory; preserve
                        // its previous metadata and any descendants during the sweep.
                        self.mark_entries_seen(&path, scan_id, true)?;
                        continue;
                    }
                };
                let kind = kind_of(&md);
                match kind {
                    EntryKind::Dir => {
                        if exclusions.excludes_folder(&path_str(&path).to_lowercase()) {
                            continue;
                        }
                        stack.push(path.clone());
                    }
                    EntryKind::File | EntryKind::Link => {
                        let ext = forge_core::model::ext_of(&path);
                        if exclusions.excludes_ext(&ext) {
                            continue;
                        }
                    }
                }
                batch.push(new_file(&path, kind, &md));
                self.progress.add_items(1);
                if batch.len() >= self.batch_size {
                    let mut pending = stack[..base_len].to_vec();
                    pending.push(dir.clone());
                    self.flush(
                        root.id,
                        scan_id,
                        volume,
                        &mut batch,
                        &pending,
                        &mut report,
                        &last_error,
                        ScanStatus::Running,
                    )?;
                }
            }
            if batch.len() >= self.batch_size {
                self.flush(
                    root.id,
                    scan_id,
                    volume,
                    &mut batch,
                    &stack,
                    &mut report,
                    &last_error,
                    ScanStatus::Running,
                )?;
            }
        }
        self.flush(
            root.id,
            scan_id,
            volume,
            &mut batch,
            &[],
            &mut report,
            &last_error,
            ScanStatus::Running,
        )?;

        // Complete: sweep rows not seen in this pass.
        let marked = self.db.with_tx(|tx| {
            let (lo, hi) = subtree_bounds(&root_s);
            let n = tx.execute(
                "UPDATE files SET present = 0
                 WHERE present = 1 AND (path = ?1 OR (path >= ?2 AND path < ?3))
                   AND seen_scan IS NOT ?4 AND indexed_at < ?4",
                params![root_s, lo, hi, scan_id],
            )? as u64;
            let now = now_ms();
            tx.execute(
                "UPDATE index_state SET status = 'complete', finished_at = ?2, updated_at = ?2,
                     pending = '[]', last_error = ?3 WHERE root_id = ?1",
                params![root.id, now, last_error],
            )?;
            tx.execute(
                "UPDATE index_roots SET last_complete_at = ?2 WHERE id = ?1",
                params![root.id, now],
            )?;
            Ok(n)
        })?;
        report.marked_missing = marked;
        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    fn flush(
        &mut self,
        root_id: i64,
        scan_id: i64,
        volume: VolumeId,
        batch: &mut Vec<NewFile>,
        pending: &[PathBuf],
        report: &mut RootReport,
        last_error: &Option<String>,
        status: ScanStatus,
    ) -> Result<()> {
        let rows = batch.len();
        let pending_s = pending_json(pending);
        let errors = report.errors as i64;
        self.db.with_tx(|tx| {
            if !batch.is_empty() {
                upsert_files_tx(
                    tx,
                    batch,
                    Some(volume),
                    Some(scan_id),
                    &mut VolumeCache::new(),
                )?;
            }
            write_checkpoint(
                tx,
                root_id,
                &pending_s,
                rows as i64,
                errors,
                last_error,
                status,
            )
        })?;
        batch.clear();
        report.items += rows as u64;
        if let Some(hook) = self.on_batch.as_mut() {
            hook(&BatchCommitted {
                root_id,
                rows,
                items_total: report.items,
                pending_dirs: pending.len(),
            });
        }
        Ok(())
    }

    /// Refresh one folder (non-recursive): upsert its current entries and mark rows for
    /// entries that disappeared (and everything below them) as not present. If the
    /// folder itself is gone, it and its subtree are marked missing.
    pub fn reconcile_dir(&self, dir: &Path) -> Result<ReconcileReport> {
        let dir_s = normalize_dir(&path_str(dir));
        let dir_path = PathBuf::from(&dir_s);
        let mut report = ReconcileReport::default();
        let md = match fs::symlink_metadata(&dir_path) {
            Ok(md) if md.is_dir() => md,
            _ => {
                report.marked_missing = self
                    .db
                    .with_tx(|tx| mark_subtree_missing_tx(tx, &dir_s, true))?;
                return Ok(report);
            }
        };
        let exclusions = self.db.exclusions()?;
        let mut rows = vec![new_file(&dir_path, EntryKind::Dir, &md)];
        let rd = self
            .read_dir(&dir_path)
            .map_err(|e| DbError::io(&dir_path, e))?;
        for de in rd {
            self.cancel.check()?;
            let de = de.map_err(|e| DbError::io(&dir_path, e))?;
            let path = de.path();
            let name_lc = de.file_name().to_string_lossy().to_lowercase();
            if exclusions.excludes_name(&name_lc) {
                continue;
            }
            let md = self
                .entry_metadata(&de, &path)
                .map_err(|e| DbError::io(&path, e))?;
            let kind = kind_of(&md);
            let excluded = match kind {
                EntryKind::Dir => exclusions.excludes_folder(&path_str(&path).to_lowercase()),
                _ => exclusions.excludes_ext(&forge_core::model::ext_of(&path)),
            };
            if !excluded {
                rows.push(new_file(&path, kind, &md));
            }
        }
        let volume = self.volume_for(&dir_path)?;
        let listed: HashSet<String> = rows
            .iter()
            .map(|r| path_str(&r.path).to_lowercase())
            .collect();
        self.db.with_tx(|tx| {
            upsert_files_tx(tx, &rows, Some(volume), None, &mut VolumeCache::new())?;
            report.upserted = rows.len() as u64;
            let gone: Vec<(String, String)> = {
                let mut st = tx.prepare(
                    "SELECT path, kind FROM files WHERE parent_path = ?1 AND present = 1",
                )?;
                st.query_map([&dir_s], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(p, _)| !listed.contains(&p.to_lowercase()))
                .collect()
            };
            for (p, kind) in gone {
                report.marked_missing += if kind == "dir" {
                    mark_subtree_missing_tx(tx, &p, true)?
                } else {
                    tx.execute(
                        "UPDATE files SET present = 0 WHERE path = ?1 AND present = 1",
                        [&p],
                    )? as u64
                };
            }
            Ok(())
        })?;
        Ok(report)
    }
}

fn write_checkpoint(
    tx: &Transaction<'_>,
    root_id: i64,
    pending: &str,
    rows: i64,
    errors: i64,
    last_error: &Option<String>,
    status: ScanStatus,
) -> Result<()> {
    tx.execute(
        "UPDATE index_state SET pending = ?2, items_seen = items_seen + ?3, errors = ?4,
             last_error = COALESCE(?5, last_error), status = ?6, updated_at = ?7
         WHERE root_id = ?1",
        params![
            root_id,
            pending,
            rows,
            errors,
            last_error,
            status.as_str(),
            now_ms()
        ],
    )?;
    Ok(())
}

fn pending_json(p: &[PathBuf]) -> String {
    let v: Vec<String> = p.iter().map(|x| path_str(x)).collect();
    serde_json::to_string(&v).unwrap_or_else(|_| "[]".into())
}

fn new_file(path: &Path, kind: EntryKind, md: &fs::Metadata) -> NewFile {
    NewFile {
        path: path.to_path_buf(),
        kind,
        size: if kind == EntryKind::File { md.len() } else { 0 },
        mtime: md.modified().ok().map(system_time_ms),
        ctime: md.created().ok().map(system_time_ms),
    }
}

/// Classify without following links. Any reparse point (symlink, junction, mount point,
/// cloud placeholder folder) counts as a link and is never descended into.
fn kind_of(md: &fs::Metadata) -> EntryKind {
    let ft = md.file_type();
    if ft.is_symlink() || is_reparse_point(md) {
        EntryKind::Link
    } else if ft.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
    }
}

#[cfg(windows)]
fn is_reparse_point(md: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_md: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexed_tree() -> (tempfile::TempDir, Db, i64, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let dir = root.join("sub");
        let file = dir.join("keep.jpg");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&file, b"present").unwrap();

        let db = Db::open_in_memory().unwrap();
        let root_id = db.add_index_root(&root).unwrap();
        let initial = Indexer::new(&db).index_root(root_id).unwrap();
        assert_eq!(initial.marked_missing, 0);
        (tmp, db, root_id, root, dir, file)
    }

    #[test]
    fn metadata_error_does_not_mark_existing_file_missing_in_full_scan() {
        let (_tmp, db, root_id, _root, _dir, file) = indexed_tree();

        let report = Indexer::new(&db)
            .fail_metadata_for_test(&file)
            .index_root(root_id)
            .unwrap();

        assert_eq!(report.errors, 1);
        assert_eq!(report.marked_missing, 0);
        assert!(db.file_by_path(&file).unwrap().unwrap().present);
    }

    #[test]
    fn metadata_error_does_not_mark_existing_file_missing_in_reconcile() {
        let (_tmp, db, _root_id, _root, dir, file) = indexed_tree();

        let result = Indexer::new(&db)
            .fail_metadata_for_test(&file)
            .reconcile_dir(&dir);

        assert!(result.is_err());
        assert!(db.file_by_path(&file).unwrap().unwrap().present);
    }

    #[test]
    fn unreadable_directory_preserves_its_existing_subtree() {
        let (_tmp, db, root_id, _root, dir, file) = indexed_tree();

        let report = Indexer::new(&db)
            .fail_read_dir_for_test(&dir)
            .index_root(root_id)
            .unwrap();

        assert_eq!(report.errors, 1);
        assert_eq!(report.marked_missing, 0);
        assert!(db.file_by_path(&dir).unwrap().unwrap().present);
        assert!(db.file_by_path(&file).unwrap().unwrap().present);
    }
}
