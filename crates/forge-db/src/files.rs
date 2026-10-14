//! Volumes, file rows, presence tracking, relinking, metadata and hashes.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use forge_core::model::{EntryKind, FileEntry, MediaKind, ext_of, media_kind_for_ext};
use forge_core::query::{ColorLabel, PickState};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::error::{DbError, Result};
use crate::{Db, now_ms, path_str};

pub type FileId = i64;
pub type VolumeId = i64;

pub(crate) const SEP: char = std::path::MAIN_SEPARATOR;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// What the platform layer knows about a volume. With a `serial`, a drive keeps its
/// identity when its letter changes; without one, the root string identifies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub serial: Option<String>,
    pub label: Option<String>,
    pub root: String,
}

impl VolumeInfo {
    /// A volume identified only by the root of `path` (e.g. `E:\`).
    pub fn from_path_root(path: &Path) -> Self {
        Self {
            serial: None,
            label: None,
            root: volume_root_of(path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Volume {
    pub id: VolumeId,
    pub serial: Option<String>,
    pub label: Option<String>,
    pub root: String,
    pub last_seen: Option<i64>,
}

/// A file/folder to insert or refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFile {
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    /// Unix ms.
    pub mtime: Option<i64>,
    /// Unix ms.
    pub ctime: Option<i64>,
}

impl From<&FileEntry> for NewFile {
    fn from(e: &FileEntry) -> Self {
        Self {
            path: e.path.clone(),
            kind: e.kind,
            size: e.size,
            mtime: e.modified.map(system_time_ms),
            ctime: e.created.map(system_time_ms),
        }
    }
}

pub(crate) fn system_time_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// One indexed file with its basic annotations.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileRecord {
    pub id: FileId,
    pub volume_id: VolumeId,
    pub path: PathBuf,
    pub parent_path: PathBuf,
    pub name: String,
    pub ext: String,
    pub size: u64,
    pub mtime: Option<i64>,
    pub ctime: Option<i64>,
    pub kind: EntryKind,
    pub media_kind: MediaKind,
    pub indexed_at: i64,
    /// False when the file was not found on the last scan or its volume is offline.
    pub present: bool,
    pub partial_hash: Option<String>,
    pub full_hash: Option<String>,
    pub rating: u8,
    pub label: Option<ColorLabel>,
    pub pick: PickState,
    /// The file has a non-empty note in `annotations.notes`.
    pub has_notes: bool,
}

/// Columns matching [`FileRecord::from_row`]; aliases `f` = files, `a` = annotations.
pub(crate) const RECORD_COLS: &str = "f.id, f.volume_id, f.path, f.parent_path, f.name, f.ext, \
    f.size, f.mtime, f.ctime, f.kind, f.media_kind, f.indexed_at, f.present, f.partial_hash, \
    f.full_hash, COALESCE(a.rating, 0), a.label, COALESCE(a.pick, 0),     COALESCE(a.notes, '') <> ''";

impl FileRecord {
    pub(crate) fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            volume_id: r.get(1)?,
            path: PathBuf::from(r.get::<_, String>(2)?),
            parent_path: PathBuf::from(r.get::<_, String>(3)?),
            name: r.get(4)?,
            ext: r.get(5)?,
            size: r.get::<_, i64>(6)?.max(0) as u64,
            mtime: r.get(7)?,
            ctime: r.get(8)?,
            kind: entry_kind_from_str(&r.get::<_, String>(9)?),
            media_kind: media_kind_from_str(&r.get::<_, String>(10)?),
            indexed_at: r.get(11)?,
            present: r.get::<_, i64>(12)? != 0,
            partial_hash: r.get(13)?,
            full_hash: r.get(14)?,
            rating: r.get::<_, i64>(15)?.clamp(0, 5) as u8,
            label: r
                .get::<_, Option<String>>(16)?
                .and_then(|s| ColorLabel::parse(&s)),
            pick: PickState::from_i64(r.get(17)?),
            has_notes: r.get::<_, i64>(18)? != 0,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageMeta {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens: Option<String>,
    /// mm
    pub focal_length: Option<f64>,
    /// f-number
    pub aperture: Option<f64>,
    /// seconds
    pub shutter: Option<f64>,
    pub iso: Option<u32>,
    /// EV
    pub exposure_comp: Option<f64>,
    /// Unix ms.
    pub taken_at: Option<i64>,
    pub gps_lat: Option<f64>,
    pub gps_lon: Option<f64>,
    pub orientation: Option<u16>,
    pub color_space: Option<String>,
    pub bit_depth: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VideoMeta {
    /// seconds
    pub duration: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub vfr: bool,
    pub codec: Option<String>,
    pub bit_depth: Option<u16>,
    /// e.g. "4:2:2"
    pub chroma: Option<String>,
    /// bits/s
    pub bitrate: Option<u64>,
    pub audio_streams: Option<u32>,
    /// Transfer characteristic, e.g. "PQ" / "HLG"; `None` = SDR or unknown.
    pub hdr: Option<String>,
    pub rotation: Option<i32>,
    pub timecode: Option<String>,
    pub camera: Option<String>,
    /// Unix ms.
    pub recorded_at: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RelinkReport {
    /// Rows whose path was rewritten.
    pub moved: u64,
    /// Rows merged into an already-indexed row at the new path (annotations, tags and
    /// collection membership carried over; the old row removed).
    pub merged: u64,
    /// Conflicting non-default annotation values. The source value was kept.
    pub conflicts: Vec<RelinkConflict>,
    /// Rewritten destination paths that were not found on disk and remain offline.
    pub offline_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RelinkConflict {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub field: String,
}

// ---------------------------------------------------------------------------
// String encodings
// ---------------------------------------------------------------------------

pub fn media_kind_str(m: MediaKind) -> &'static str {
    match m {
        MediaKind::Image => "image",
        MediaKind::Raw => "raw",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Sidecar => "sidecar",
        MediaKind::Document => "document",
        MediaKind::Archive => "archive",
        MediaKind::Project => "project",
        MediaKind::Other => "other",
    }
}

pub fn media_kind_from_str(s: &str) -> MediaKind {
    match s {
        "image" => MediaKind::Image,
        "raw" => MediaKind::Raw,
        "video" => MediaKind::Video,
        "audio" => MediaKind::Audio,
        "sidecar" => MediaKind::Sidecar,
        "document" => MediaKind::Document,
        "archive" => MediaKind::Archive,
        "project" => MediaKind::Project,
        _ => MediaKind::Other,
    }
}

pub(crate) fn entry_kind_str(k: EntryKind) -> &'static str {
    match k {
        EntryKind::File => "file",
        EntryKind::Dir => "dir",
        EntryKind::Link => "link",
    }
}

pub(crate) fn entry_kind_from_str(s: &str) -> EntryKind {
    match s {
        "dir" => EntryKind::Dir,
        "link" => EntryKind::Link,
        _ => EntryKind::File,
    }
}

/// The volume root of a path: `C:\`, `\\server\share\`, or `/`.
pub fn volume_root_of(path: &Path) -> String {
    let mut root = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => root.push(c.as_os_str()),
            _ => break,
        }
    }
    let s = path_str(&root);
    if s.is_empty() { SEP.to_string() } else { s }
}

/// Strip a trailing separator unless the path is a root (`C:\`, `/`).
pub(crate) fn normalize_dir(p: &str) -> String {
    let mut s = p.to_string();
    while s.len() > 1 && s.ends_with(SEP) && !s.ends_with(&format!(":{SEP}")) {
        s.pop();
    }
    s
}

/// Bounds `[lo, hi)` such that `path >= lo AND path < hi` selects everything strictly
/// below `dir` (uses the `path` index).
pub(crate) fn subtree_bounds(dir: &str) -> (String, String) {
    let d = normalize_dir(dir);
    let base = if d.ends_with(SEP) {
        d
    } else {
        format!("{d}{SEP}")
    };
    let mut hi = base.clone();
    hi.pop();
    hi.push(char::from_u32(SEP as u32 + 1).expect("valid char"));
    (base, hi)
}

// ---------------------------------------------------------------------------
// Row-level helpers shared with the indexer
// ---------------------------------------------------------------------------

pub(crate) fn upsert_volume_tx(tx: &Transaction<'_>, v: &VolumeInfo) -> Result<VolumeId> {
    let now = now_ms();
    if let Some(serial) = &v.serial {
        let id = tx.query_row(
            "INSERT INTO volumes(serial, label, root, last_seen) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(serial) DO UPDATE SET label = excluded.label, root = excluded.root,
                 last_seen = excluded.last_seen
             RETURNING id",
            params![serial, v.label, v.root, now],
            |r| r.get(0),
        )?;
        return Ok(id);
    }
    if let Some(id) = tx
        .query_row(
            "SELECT id FROM volumes WHERE serial IS NULL AND root = ?1",
            [&v.root],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        tx.execute(
            "UPDATE volumes SET last_seen = ?2, label = COALESCE(?3, label) WHERE id = ?1",
            params![id, now, v.label],
        )?;
        return Ok(id);
    }
    tx.execute(
        "INSERT INTO volumes(serial, label, root, last_seen) VALUES (NULL, ?1, ?2, ?3)",
        params![v.label, v.root, now],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Resolves volume ids by path root within one transaction.
pub(crate) struct VolumeCache(HashMap<String, VolumeId>);

impl VolumeCache {
    pub(crate) fn new() -> Self {
        Self(HashMap::new())
    }
    pub(crate) fn get(&mut self, tx: &Transaction<'_>, path: &Path) -> Result<VolumeId> {
        let root = volume_root_of(path);
        if let Some(id) = self.0.get(&root) {
            return Ok(*id);
        }
        // Prefer a known serial-identified volume currently mounted at this root.
        let id = match tx
            .query_row(
                "SELECT id FROM volumes WHERE root = ?1 ORDER BY (serial IS NULL), last_seen DESC LIMIT 1",
                [&root],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            Some(id) => id,
            None => upsert_volume_tx(
                tx,
                &VolumeInfo {
                    serial: None,
                    label: None,
                    root: root.clone(),
                },
            )?,
        };
        self.0.insert(root, id);
        Ok(id)
    }
}

/// Insert or refresh rows. `volume` overrides per-path volume resolution; `scan_id` is set
/// by the indexer (others pass `None` and leave the column alone). Content hashes are
/// dropped when size or mtime changed.
pub(crate) fn upsert_files_tx(
    tx: &Transaction<'_>,
    files: &[NewFile],
    volume: Option<VolumeId>,
    scan_id: Option<i64>,
    vols: &mut VolumeCache,
) -> Result<Vec<FileId>> {
    let now = now_ms();
    // Fast path for re-scans: when nothing indexed changed, touch only the non-indexed
    // bookkeeping columns (avoids rewriting all eight index entries per row).
    let mut unchanged = tx.prepare_cached(
        "UPDATE files SET indexed_at = ?1, present = 1, seen_scan = COALESCE(?2, seen_scan)
         WHERE path = ?3 AND (path COLLATE BINARY) = ?3 AND volume_id = ?4 AND kind = ?5
           AND size = ?6 AND mtime IS ?7 AND ctime IS ?8
         RETURNING id",
    )?;
    let mut st = tx.prepare_cached(
        "INSERT INTO files(volume_id, path, path_lc, parent_path, name, name_lc, ext, size,
                           mtime, ctime, kind, media_kind, indexed_at, present, seen_scan)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 1, ?14)
         ON CONFLICT(path) DO UPDATE SET
             volume_id = excluded.volume_id,
             path = excluded.path,
             path_lc = excluded.path_lc,
             parent_path = excluded.parent_path,
             name = excluded.name,
             name_lc = excluded.name_lc,
             ext = excluded.ext,
             partial_hash = CASE WHEN files.size = excluded.size AND files.mtime IS excluded.mtime
                                 THEN files.partial_hash END,
             full_hash = CASE WHEN files.size = excluded.size AND files.mtime IS excluded.mtime
                              THEN files.full_hash END,
             size = excluded.size,
             mtime = excluded.mtime,
             ctime = excluded.ctime,
             kind = excluded.kind,
             media_kind = excluded.media_kind,
             indexed_at = excluded.indexed_at,
             present = 1,
             seen_scan = COALESCE(excluded.seen_scan, files.seen_scan)
         RETURNING id",
    )?;
    let mut ids = Vec::with_capacity(files.len());
    for f in files {
        let path = path_str(&f.path);
        let name = f
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        let parent = f.path.parent().map(path_str).unwrap_or_default();
        let (ext, media) = match f.kind {
            EntryKind::File => {
                let e = ext_of(&f.path);
                let m = media_kind_for_ext(&e);
                (e, m)
            }
            EntryKind::Link => (ext_of(&f.path), MediaKind::Other),
            EntryKind::Dir => (String::new(), MediaKind::Other),
        };
        let vol = match volume {
            Some(v) => v,
            None => vols.get(tx, &f.path)?,
        };
        let size = if f.kind == EntryKind::File {
            f.size as i64
        } else {
            0
        };
        if let Some(id) = unchanged
            .query_row(
                params![
                    now,
                    scan_id,
                    path,
                    vol,
                    entry_kind_str(f.kind),
                    size,
                    f.mtime,
                    f.ctime
                ],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            ids.push(id);
            continue;
        }
        let id: i64 = st.query_row(
            params![
                vol,
                path,
                path.to_lowercase(),
                parent,
                name,
                name.to_lowercase(),
                ext,
                size,
                f.mtime,
                f.ctime,
                entry_kind_str(f.kind),
                media_kind_str(media),
                now,
                scan_id,
            ],
            |r| r.get(0),
        )?;
        ids.push(id);
    }
    Ok(ids)
}

/// Mark `dir` itself (if `include_self`) and everything below it as not present.
pub(crate) fn mark_subtree_missing_tx(
    tx: &Transaction<'_>,
    dir: &str,
    include_self: bool,
) -> Result<u64> {
    let (lo, hi) = subtree_bounds(dir);
    let mut n = tx.execute(
        "UPDATE files SET present = 0 WHERE present = 1 AND path >= ?1 AND path < ?2",
        params![lo, hi],
    )? as u64;
    if include_self {
        n += tx.execute(
            "UPDATE files SET present = 0 WHERE present = 1 AND path = ?1",
            [normalize_dir(dir)],
        )? as u64;
    }
    Ok(n)
}

fn query_records(
    conn: &rusqlite::Connection,
    where_sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<FileRecord>> {
    let sql = format!(
        "SELECT {RECORD_COLS} FROM files f LEFT JOIN annotations a ON a.file_id = f.id WHERE {where_sql}"
    );
    let mut st = conn.prepare_cached(&sql)?;
    let rows = st
        .query_map(params, FileRecord::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl Db {
    /// Register or refresh a volume (updates `last_seen`).
    pub fn upsert_volume(&self, v: &VolumeInfo) -> Result<VolumeId> {
        self.with_tx(|tx| upsert_volume_tx(tx, v))
    }

    pub fn volumes(&self) -> Result<Vec<Volume>> {
        self.with_conn(|c| {
            let mut st =
                c.prepare("SELECT id, serial, label, root, last_seen FROM volumes ORDER BY root")?;
            let v = st
                .query_map([], |r| {
                    Ok(Volume {
                        id: r.get(0)?,
                        serial: r.get(1)?,
                        label: r.get(2)?,
                        root: r.get(3)?,
                        last_seen: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(v)
        })
    }

    /// A disconnected drive: everything on it becomes offline (rows and annotations kept).
    pub fn mark_volume_offline(&self, volume: VolumeId) -> Result<u64> {
        self.with_conn(|c| {
            Ok(c.execute(
                "UPDATE files SET present = 0 WHERE volume_id = ?1 AND present = 1",
                [volume],
            )? as u64)
        })
    }

    /// Insert or refresh many rows in one transaction; volumes are resolved from each
    /// path's root. Returns ids in input order.
    pub fn upsert_files(&self, files: &[NewFile]) -> Result<Vec<FileId>> {
        self.with_tx(|tx| upsert_files_tx(tx, files, None, None, &mut VolumeCache::new()))
    }

    /// Like [`Db::upsert_files`] with an explicit volume for all rows.
    pub fn upsert_files_in_volume(
        &self,
        volume: VolumeId,
        files: &[NewFile],
    ) -> Result<Vec<FileId>> {
        self.with_tx(|tx| upsert_files_tx(tx, files, Some(volume), None, &mut VolumeCache::new()))
    }

    /// Convenience for listing results.
    pub fn upsert_entries(&self, entries: &[FileEntry]) -> Result<Vec<FileId>> {
        let files: Vec<NewFile> = entries.iter().map(NewFile::from).collect();
        self.upsert_files(&files)
    }

    pub fn file(&self, id: FileId) -> Result<Option<FileRecord>> {
        self.with_conn(|c| Ok(query_records(c, "f.id = ?1", [id])?.pop()))
    }

    pub fn file_by_path(&self, path: &Path) -> Result<Option<FileRecord>> {
        self.with_conn(|c| Ok(query_records(c, "f.path = ?1", [path_str(path)])?.pop()))
    }

    pub fn file_id(&self, path: &Path) -> Result<Option<FileId>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT id FROM files WHERE path = ?1",
                [path_str(path)],
                |r| r.get(0),
            )
            .optional()?)
        })
    }

    /// Ids for many paths (missing paths are skipped).
    pub fn file_ids(&self, paths: &[PathBuf]) -> Result<Vec<(PathBuf, FileId)>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached("SELECT id FROM files WHERE path = ?1")?;
            let mut out = Vec::with_capacity(paths.len());
            for p in paths {
                if let Some(id) = st.query_row([path_str(p)], |r| r.get(0)).optional()? {
                    out.push((p.clone(), id));
                }
            }
            Ok(out)
        })
    }

    /// Direct children of a folder.
    pub fn children(&self, dir: &Path, include_offline: bool) -> Result<Vec<FileRecord>> {
        let d = normalize_dir(&path_str(dir));
        self.with_conn(|c| {
            query_records(
                c,
                "f.parent_path = ?1 AND (?2 OR f.present = 1) ORDER BY f.name_lc",
                params![d, include_offline],
            )
        })
    }

    /// Mark exact paths, and everything below them, as not present.
    pub fn mark_missing(&self, paths: &[PathBuf]) -> Result<u64> {
        self.with_tx(|tx| {
            let mut n = 0;
            for p in paths {
                n += mark_subtree_missing_tx(tx, &path_str(p), true)?;
            }
            Ok(n)
        })
    }

    /// Delete offline rows. With `keep_annotated`, rows that carry a rating/label/pick/
    /// notes, tags or collection membership are kept so they can still be relinked.
    pub fn purge_offline(&self, keep_annotated: bool) -> Result<u64> {
        self.with_conn(|c| {
            let sql = if keep_annotated {
                "DELETE FROM files WHERE present = 0
                   AND NOT EXISTS (SELECT 1 FROM annotations a WHERE a.file_id = files.id
                        AND (a.rating > 0 OR a.label IS NOT NULL OR a.pick <> 0 OR COALESCE(a.notes, '') <> ''))
                   AND NOT EXISTS (SELECT 1 FROM file_tags t WHERE t.file_id = files.id)
                   AND NOT EXISTS (SELECT 1 FROM collection_items ci WHERE ci.file_id = files.id)"
            } else {
                "DELETE FROM files WHERE present = 0"
            };
            Ok(c.execute(sql, [])? as u64)
        })
    }

    /// Rewrite the path prefix `old` → `new` for a folder (and everything below it) or a
    /// single file, e.g. after a drive letter changed or a folder was moved outside ZXraw Explorer.
    /// Restrict to one volume with `volume`. Where a row already exists at the new path
    /// (because the new location was indexed meanwhile), the old row's annotations, tags
    /// and collection memberships are merged into it and the old row is removed.
    /// Rewritten rows are marked present only when their destination exists on disk. Missing
    /// targets are listed in `offline_paths`.
    pub fn relink(&self, old: &Path, new: &Path, volume: Option<VolumeId>) -> Result<RelinkReport> {
        self.with_tx(|tx| relink_tx(tx, old, new, volume))
    }

    /// Several renames or moves made together (a batch rename), applied as one set in one
    /// transaction. Every source is first parked under a unique temporary path, then moved
    /// to its destination, so swaps (A→B, B→A) and cycles keep each row's annotations on the
    /// right file instead of merging them. A case-only rename (`a.jpg` → `A.jpg`) also
    /// updates the stored path this way. Pairs whose paths are identical are skipped.
    pub fn relink_many(&self, pairs: &[(PathBuf, PathBuf)]) -> Result<RelinkReport> {
        let pairs: Vec<&(PathBuf, PathBuf)> = pairs.iter().filter(|(o, n)| o != n).collect();
        for (old, new) in &pairs {
            relink_paths(old, new)?;
        }
        let nonce = now_ms();
        self.with_tx(|tx| {
            let mut parked = Vec::with_capacity(pairs.len());
            for (i, (old, new)) in pairs.iter().enumerate() {
                let mut root = volume_root_of(Path::new(&normalize_dir(&path_str(old))));
                if !root.ends_with(SEP) {
                    root.push(SEP);
                }
                // `<` and `>` cannot appear in a Windows file name, so no real file can ever
                // be indexed at this staging key.
                let temp_s = format!("{root}<zxraw-relink-{nonce}-{i}>");
                let (lo, hi) = subtree_bounds(&temp_s);
                let taken: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM files WHERE path = ?1 OR (path >= ?2 AND path < ?3))",
                    params![temp_s, lo, hi],
                    |r| r.get(0),
                )?;
                if taken {
                    return Err(DbError::Invalid(format!(
                        "relink staging key {temp_s} is already in use"
                    )));
                }
                let temp = PathBuf::from(temp_s);
                relink_tx(tx, old, &temp, None)?;
                parked.push((temp, new));
            }
            let mut report = RelinkReport::default();
            for (temp, new) in parked {
                let part = relink_tx(tx, &temp, new, None)?;
                report.moved += part.moved;
                report.merged += part.merged;
                report.conflicts.extend(part.conflicts);
                report.offline_paths.extend(part.offline_paths);
            }
            Ok(report)
        })
    }

    /// Folders to re-list after a relink: `root` itself and every folder that holds a row
    /// below it. Re-listing refreshes the `present` flags that [`Db::relink`] leaves alone.
    pub fn folders_under(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let root_s = normalize_dir(&path_str(root));
        let (lo, hi) = subtree_bounds(&root_s);
        self.with_conn(|c| {
            let mut st = c.prepare(
                "SELECT DISTINCT parent_path FROM files
                 WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
            )?;
            let mut dirs = st
                .query_map(params![root_s, lo, hi], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            dirs.push(PathBuf::from(&root_s));
            dirs.sort();
            dirs.dedup();
            Ok(dirs)
        })
    }

    /// Store content hashes computed elsewhere (hex strings).
    pub fn set_hashes(&self, id: FileId, partial: Option<&str>, full: Option<&str>) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE files SET partial_hash = COALESCE(?2, partial_hash), full_hash = COALESCE(?3, full_hash) WHERE id = ?1",
                params![id, partial, full],
            )?;
            Ok(())
        })
    }

    /// Present files sharing a full hash, grouped (largest groups first).
    pub fn duplicate_groups(&self, limit: u32) -> Result<Vec<Vec<FileRecord>>> {
        self.with_conn(|c| {
            let hashes: Vec<String> = {
                let mut st = c.prepare(
                    "SELECT full_hash FROM files WHERE present = 1 AND full_hash IS NOT NULL
                     GROUP BY full_hash HAVING COUNT(*) > 1 ORDER BY COUNT(*) DESC, MAX(size) DESC LIMIT ?1",
                )?;
                st.query_map([limit as i64], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            let mut out = Vec::with_capacity(hashes.len());
            for h in hashes {
                out.push(query_records(
                    c,
                    "f.full_hash = ?1 AND f.present = 1 ORDER BY f.path",
                    [h],
                )?);
            }
            Ok(out)
        })
    }

    pub fn upsert_image_meta(&self, id: FileId, m: &ImageMeta) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT OR REPLACE INTO image_meta(file_id, width, height, camera_make, camera_model, lens,
                    focal_length, aperture, shutter, iso, exposure_comp, taken_at, gps_lat, gps_lon,
                    orientation, color_space, bit_depth, source_mtime, extracted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                    (SELECT mtime FROM files WHERE id = ?1), ?18)",
                params![
                    id,
                    m.width,
                    m.height,
                    m.camera_make,
                    m.camera_model,
                    m.lens,
                    m.focal_length,
                    m.aperture,
                    m.shutter,
                    m.iso,
                    m.exposure_comp,
                    m.taken_at,
                    m.gps_lat,
                    m.gps_lon,
                    m.orientation,
                    m.color_space,
                    m.bit_depth,
                    now_ms()
                ],
            )?;
            Ok(())
        })
    }

    pub fn upsert_video_meta(&self, id: FileId, m: &VideoMeta) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT OR REPLACE INTO video_meta(file_id, duration, width, height, fps, vfr, codec,
                    bit_depth, chroma, bitrate, audio_streams, hdr, rotation, timecode, camera,
                    recorded_at, source_mtime, extracted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                    (SELECT mtime FROM files WHERE id = ?1), ?17)",
                params![
                    id,
                    m.duration,
                    m.width,
                    m.height,
                    m.fps,
                    m.vfr,
                    m.codec,
                    m.bit_depth,
                    m.chroma,
                    m.bitrate.map(|b| b.min(i64::MAX as u64) as i64),
                    m.audio_streams,
                    m.hdr,
                    m.rotation,
                    m.timecode,
                    m.camera,
                    m.recorded_at,
                    now_ms()
                ],
            )?;
            Ok(())
        })
    }

    pub fn image_meta(&self, id: FileId) -> Result<Option<ImageMeta>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT width, height, camera_make, camera_model, lens, focal_length, aperture, shutter,
                        iso, exposure_comp, taken_at, gps_lat, gps_lon, orientation, color_space, bit_depth
                 FROM image_meta WHERE file_id = ?1",
                [id],
                |r| {
                    Ok(ImageMeta {
                        width: r.get(0)?,
                        height: r.get(1)?,
                        camera_make: r.get(2)?,
                        camera_model: r.get(3)?,
                        lens: r.get(4)?,
                        focal_length: r.get(5)?,
                        aperture: r.get(6)?,
                        shutter: r.get(7)?,
                        iso: r.get(8)?,
                        exposure_comp: r.get(9)?,
                        taken_at: r.get(10)?,
                        gps_lat: r.get(11)?,
                        gps_lon: r.get(12)?,
                        orientation: r.get(13)?,
                        color_space: r.get(14)?,
                        bit_depth: r.get(15)?,
                    })
                },
            )
            .optional()?)
        })
    }

    pub fn video_meta(&self, id: FileId) -> Result<Option<VideoMeta>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT duration, width, height, fps, vfr, codec, bit_depth, chroma, bitrate,
                        audio_streams, hdr, rotation, timecode, camera, recorded_at
                 FROM video_meta WHERE file_id = ?1",
                [id],
                |r| {
                    Ok(VideoMeta {
                        duration: r.get(0)?,
                        width: r.get(1)?,
                        height: r.get(2)?,
                        fps: r.get(3)?,
                        vfr: r.get(4)?,
                        codec: r.get(5)?,
                        bit_depth: r.get(6)?,
                        chroma: r.get(7)?,
                        bitrate: r.get::<_, Option<i64>>(8)?.map(|b| b.max(0) as u64),
                        audio_streams: r.get(9)?,
                        hdr: r.get(10)?,
                        rotation: r.get(11)?,
                        timecode: r.get(12)?,
                        camera: r.get(13)?,
                        recorded_at: r.get(14)?,
                    })
                },
            )
            .optional()?)
        })
    }

    /// Present image/RAW/video files whose metadata is missing or older than the file
    /// (work queue for the media pipeline), newest first.
    pub fn files_needing_meta(&self, limit: u32) -> Result<Vec<FileRecord>> {
        self.with_conn(|c| {
            query_records(
                c,
                "f.present = 1 AND f.kind = 'file' AND (
                    (f.media_kind IN ('image', 'raw') AND NOT EXISTS (
                        SELECT 1 FROM image_meta m WHERE m.file_id = f.id AND m.source_mtime IS f.mtime))
                 OR (f.media_kind = 'video' AND NOT EXISTS (
                        SELECT 1 FROM video_meta m WHERE m.file_id = f.id AND m.source_mtime IS f.mtime)))
                 ORDER BY f.mtime DESC LIMIT ?1",
                [limit as i64],
            )
        })
    }

    /// Present image/RAW/video files with missing or stale metadata, after `after_id`.
    /// Results are ordered by id so callers can walk the queue without refetching a batch.
    /// When video metadata tools are unavailable, `include_video` can be false to leave
    /// video rows eligible for a later pass while still progressing through images.
    pub fn files_needing_meta_after(
        &self,
        after_id: i64,
        limit: u32,
        include_video: bool,
    ) -> Result<Vec<FileRecord>> {
        self.with_conn(|c| {
            query_records(
                c,
                "f.present = 1 AND f.kind = 'file' AND f.id > ?1 AND (?2 OR f.media_kind <> 'video') AND (
                    (f.media_kind IN ('image', 'raw') AND NOT EXISTS (
                        SELECT 1 FROM image_meta m WHERE m.file_id = f.id AND m.source_mtime IS f.mtime))
                 OR (f.media_kind = 'video' AND NOT EXISTS (
                        SELECT 1 FROM video_meta m WHERE m.file_id = f.id AND m.source_mtime IS f.mtime)))
                 ORDER BY f.id LIMIT ?3",
                params![after_id, include_video, limit as i64],
            )
        })
    }
}

/// Move user data from row `from` to row `to`, then delete `from`.
fn merge_rows_tx(
    tx: &Transaction<'_>,
    from: FileId,
    to: FileId,
    source_path: &Path,
    destination_path: &Path,
    conflicts: &mut Vec<RelinkConflict>,
) -> Result<()> {
    let source: Option<(i64, Option<String>, i64, Option<String>)> = tx
        .query_row(
            "SELECT rating, label, pick, notes FROM annotations WHERE file_id = ?1",
            [from],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if let Some((source_rating, source_label, source_pick, source_notes)) = source {
        let destination: (i64, Option<String>, i64, Option<String>) = tx
            .query_row(
                "SELECT rating, label, pick, notes FROM annotations WHERE file_id = ?1",
                [to],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .unwrap_or((0, None, 0, None));

        let (destination_rating, destination_label, destination_pick, destination_notes) =
            destination;
        let rating = if source_rating > 0 {
            if destination_rating > 0 && source_rating != destination_rating {
                conflicts.push(relink_conflict(source_path, destination_path, "rating"));
            }
            source_rating
        } else {
            destination_rating
        };
        let label = merge_annotation_field(
            source_label,
            destination_label,
            "label",
            source_path,
            destination_path,
            conflicts,
        );
        let pick = if source_pick != 0 {
            if destination_pick != 0 && source_pick != destination_pick {
                conflicts.push(relink_conflict(source_path, destination_path, "pick"));
            }
            source_pick
        } else {
            destination_pick
        };
        let notes = match (
            nonempty_note(source_notes),
            nonempty_note(destination_notes),
        ) {
            (Some(source), Some(destination)) if source != destination => {
                conflicts.push(relink_conflict(source_path, destination_path, "notes"));
                Some(format!("{source}\n\n--- merged note ---\n\n{destination}"))
            }
            (Some(source), _) => Some(source),
            (_, Some(destination)) => Some(destination),
            (None, None) => None,
        };
        tx.execute(
            "INSERT INTO annotations(file_id, rating, label, pick, notes, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(file_id) DO UPDATE SET rating = excluded.rating, label = excluded.label,
                 pick = excluded.pick, notes = excluded.notes, updated_at = excluded.updated_at",
            params![to, rating, label, pick, notes, now_ms()],
        )?;
    }
    tx.execute(
        "INSERT OR IGNORE INTO file_tags(file_id, tag_id) SELECT ?2, tag_id FROM file_tags WHERE file_id = ?1",
        params![from, to],
    )?;
    tx.execute(
        "UPDATE OR IGNORE collection_items SET file_id = ?2 WHERE file_id = ?1",
        params![from, to],
    )?;
    tx.execute("DELETE FROM files WHERE id = ?1", [from])?;
    Ok(())
}

fn merge_annotation_field(
    source: Option<String>,
    destination: Option<String>,
    field: &str,
    source_path: &Path,
    destination_path: &Path,
    conflicts: &mut Vec<RelinkConflict>,
) -> Option<String> {
    match (source, destination) {
        (Some(source), Some(destination)) if source != destination => {
            conflicts.push(relink_conflict(source_path, destination_path, field));
            Some(source)
        }
        (Some(source), _) => Some(source),
        (_, Some(destination)) => Some(destination),
        (None, None) => None,
    }
}

fn nonempty_note(note: Option<String>) -> Option<String> {
    note.filter(|text| !text.trim().is_empty())
}

fn relink_conflict(source: &Path, destination: &Path, field: &str) -> RelinkConflict {
    RelinkConflict {
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        field: field.to_string(),
    }
}

/// Validated, normalized `(old, new)` for a relink, or `None` when they name the same path.
fn relink_paths(old: &Path, new: &Path) -> Result<Option<(String, String)>> {
    let old_s = normalize_dir(&path_str(old));
    let new_s = normalize_dir(&path_str(new));
    if old_s.is_empty() || new_s.is_empty() {
        return Err(DbError::Invalid("relink paths must not be empty".into()));
    }
    if old_s.eq_ignore_ascii_case(&new_s) {
        return Ok(None);
    }
    let (lo, _) = subtree_bounds(&old_s);
    if new_s.len() > old_s.len()
        && new_s
            .to_ascii_lowercase()
            .starts_with(&lo.to_ascii_lowercase())
    {
        return Err(DbError::Invalid(
            "cannot relink a folder into itself".into(),
        ));
    }
    Ok(Some((old_s, new_s)))
}

/// [`Db::relink`] inside a caller's transaction.
fn relink_tx(
    tx: &Transaction<'_>,
    old: &Path,
    new: &Path,
    volume: Option<VolumeId>,
) -> Result<RelinkReport> {
    let Some((old_s, new_s)) = relink_paths(old, new)? else {
        return Ok(RelinkReport::default());
    };
    let (lo, hi) = subtree_bounds(&old_s);
    let old_root = volume_root_of(Path::new(&old_s));
    let new_root = volume_root_of(Path::new(&new_s));
    let mut rows: Vec<(FileId, String, VolumeId)> = {
        let mut st = tx.prepare(
            "SELECT id, path, volume_id FROM files
             WHERE (path = ?1 OR (path >= ?2 AND path < ?3)) AND (?4 IS NULL OR volume_id = ?4)",
        )?;
        st.query_map(params![old_s, lo, hi, volume], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    // Parents before children keeps parent_path consistent at every step.
    rows.sort_by_key(|(_, p, _)| p.len());
    let new_volume = if old_root.eq_ignore_ascii_case(&new_root) {
        None
    } else {
        Some(VolumeCache::new().get(tx, Path::new(&new_s))?)
    };
    let mut report = RelinkReport::default();
    let mut find = tx.prepare_cached("SELECT id FROM files WHERE path = ?1")?;
    let mut update = tx.prepare_cached(
        "UPDATE files SET path = ?2, path_lc = ?3, parent_path = ?4, name = ?5, name_lc = ?6,
             volume_id = ?7 WHERE id = ?1",
    )?;
    for (id, path, vol) in rows {
        // NOCASE matched only ASCII case differences, so byte lengths agree.
        let rest = &path[old_s.len()..];
        let np = format!("{new_s}{rest}");
        let pb = PathBuf::from(&np);
        let present = std::fs::symlink_metadata(&pb).is_ok();
        if let Some(target) = find.query_row([&np], |r| r.get::<_, i64>(0)).optional()?
            && target != id
        {
            merge_rows_tx(
                tx,
                id,
                target,
                Path::new(&path),
                &pb,
                &mut report.conflicts,
            )?;
            tx.execute(
                "UPDATE files SET present = ?2 WHERE id = ?1",
                params![target, present],
            )?;
            if !present {
                report.offline_paths.push(pb);
            }
            report.merged += 1;
            continue;
        }
        let name = pb
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| np.clone());
        let parent = pb.parent().map(path_str).unwrap_or_default();
        update.execute(params![
            id,
            np,
            np.to_lowercase(),
            parent,
            name,
            name.to_lowercase(),
            new_volume.unwrap_or(vol)
        ])?;
        tx.execute(
            "UPDATE files SET present = ?2 WHERE id = ?1",
            params![id, present],
        )?;
        if !present {
            report.offline_paths.push(pb);
        }
        report.moved += 1;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nf(p: &str, size: u64, mtime: i64) -> NewFile {
        NewFile {
            path: PathBuf::from(p),
            kind: EntryKind::File,
            size,
            mtime: Some(mtime),
            ctime: None,
        }
    }

    #[test]
    fn folders_under_lists_each_folder_below_the_root_once() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_files(&[
            nf(r"C:\shoot\a.jpg", 1, 1),
            nf(r"C:\shoot\day1\b.jpg", 1, 1),
            nf(r"C:\shoot\day2\deep\c.jpg", 1, 1),
            nf(r"C:\shoot\day2\deep\d.jpg", 1, 1),
            nf(r"C:\other\e.jpg", 1, 1),
        ])
        .unwrap();
        assert_eq!(
            db.folders_under(Path::new(r"C:\shoot")).unwrap(),
            [
                PathBuf::from(r"C:\shoot"),
                PathBuf::from(r"C:\shoot\day1"),
                PathBuf::from(r"C:\shoot\day2\deep"),
            ]
        );
    }

    fn plan(db: &Db, sql: &str) -> String {
        db.with_conn(|c| {
            let mut st = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
            let rows: Vec<String> = st
                .query_map([], |r| r.get::<_, String>(3))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows.join(" | "))
        })
        .unwrap()
    }

    #[test]
    fn hot_statements_use_indexes() {
        let db = Db::open_in_memory().unwrap();
        let p = plan(
            &db,
            "UPDATE files SET indexed_at = 1 WHERE path = 'x' AND (path COLLATE BINARY) = 'x'
             AND volume_id = 1 AND kind = 'file' AND size = 1 AND mtime IS 1 AND ctime IS 1",
        );
        assert!(p.contains("INDEX") && p.contains("path=?"), "{p}");
        let p = plan(
            &db,
            "SELECT id FROM files WHERE path >= 'a\\' AND path < 'a]'",
        );
        assert!(p.contains("INDEX") && p.contains("path>?"), "{p}");
        let p = plan(&db, "SELECT id FROM files WHERE parent_path = 'a'");
        assert!(p.contains("idx_files_parent"), "{p}");
    }

    #[test]
    fn roots_and_bounds() {
        assert_eq!(volume_root_of(Path::new(r"C:\a\b.txt")), r"C:\");
        assert_eq!(volume_root_of(Path::new(r"\\srv\share\x")), r"\\srv\share\");
        assert_eq!(normalize_dir(r"C:\a\"), r"C:\a");
        assert_eq!(normalize_dir(r"C:\"), r"C:\");
        assert_eq!(
            subtree_bounds(r"C:\a"),
            (r"C:\a\".to_string(), r"C:\a]".to_string())
        );
        assert_eq!(
            subtree_bounds(r"C:\"),
            (r"C:\".to_string(), r"C:]".to_string())
        );
    }

    #[test]
    fn upsert_is_idempotent_and_invalidates_hashes() {
        let db = Db::open_in_memory().unwrap();
        let ids = db
            .upsert_files(&[nf(r"C:\p\a.JPG", 10, 1), nf(r"C:\p\b.mov", 20, 1)])
            .unwrap();
        let ids2 = db.upsert_files(&[nf(r"C:\p\a.JPG", 10, 1)]).unwrap();
        assert_eq!(ids[0], ids2[0]);
        let r = db.file(ids[0]).unwrap().unwrap();
        assert_eq!(r.ext, "jpg");
        assert_eq!(r.media_kind, MediaKind::Image);
        assert_eq!(r.parent_path, PathBuf::from(r"C:\p"));
        // Same path, different ASCII case → same row (Windows semantics).
        assert_eq!(db.file_id(Path::new(r"c:\P\A.jpg")).unwrap(), Some(ids[0]));

        db.set_hashes(ids[0], Some("pp"), Some("ff")).unwrap();
        db.upsert_files(&[nf(r"C:\p\a.JPG", 10, 1)]).unwrap();
        assert_eq!(
            db.file(ids[0]).unwrap().unwrap().full_hash.as_deref(),
            Some("ff")
        );
        db.upsert_files(&[nf(r"C:\p\a.JPG", 11, 2)]).unwrap();
        let r = db.file(ids[0]).unwrap().unwrap();
        assert_eq!(r.full_hash, None);
        assert_eq!(r.partial_hash, None);
        assert_eq!(r.size, 11);
        assert_eq!(db.volumes().unwrap().len(), 1);
    }

    #[test]
    fn metadata_roundtrip_and_staleness() {
        let db = Db::open_in_memory().unwrap();
        let ids = db
            .upsert_files(&[
                nf(r"C:\m\a.cr3", 1, 5),
                nf(r"C:\m\b.mp4", 1, 5),
                nf(r"C:\m\c.txt", 1, 5),
            ])
            .unwrap();
        assert_eq!(db.files_needing_meta(10).unwrap().len(), 2);
        let im = ImageMeta {
            width: Some(6000),
            camera_make: Some("Canon".into()),
            iso: Some(1600),
            gps_lat: Some(1.5),
            ..Default::default()
        };
        db.upsert_image_meta(ids[0], &im).unwrap();
        let vm = VideoMeta {
            duration: Some(12.5),
            fps: Some(59.94),
            vfr: true,
            bitrate: Some(100_000_000),
            ..Default::default()
        };
        db.upsert_video_meta(ids[1], &vm).unwrap();
        assert_eq!(db.image_meta(ids[0]).unwrap(), Some(im));
        assert_eq!(db.video_meta(ids[1]).unwrap(), Some(vm));
        assert!(db.files_needing_meta(10).unwrap().is_empty());
        db.upsert_files(&[nf(r"C:\m\a.cr3", 1, 6)]).unwrap();
        let need = db.files_needing_meta(10).unwrap();
        assert_eq!(need.len(), 1);
        assert_eq!(need[0].id, ids[0]);
    }

    #[test]
    fn metadata_cursor_skips_unavailable_videos_without_hiding_later_images() {
        let db = Db::open_in_memory().unwrap();
        let mut files = Vec::with_capacity(610);
        for i in 0..600 {
            files.push(nf(&format!(r"C:\queue\video-{i:03}.mp4"), 1, i));
        }
        for i in 0..10 {
            files.push(nf(&format!(r"C:\queue\image-{i:02}.jpg"), 1, 700 + i));
        }
        let ids = db.upsert_files(&files).unwrap();

        // This is the queue configuration used when ffprobe is absent. The cursor
        // skips videos entirely and still reaches the images after them.
        let images = db.files_needing_meta_after(0, 500, false).unwrap();
        assert_eq!(images.len(), 10);
        assert!(images.iter().all(|r| r.media_kind == MediaKind::Image));
        assert_eq!(images.first().unwrap().id, ids[600]);

        for image in &images {
            db.upsert_image_meta(image.id, &ImageMeta::default())
                .unwrap();
        }
        let still_needing_meta = db.files_needing_meta(1000).unwrap();
        assert_eq!(still_needing_meta.len(), 600);
        assert!(
            still_needing_meta
                .iter()
                .all(|r| r.media_kind == MediaKind::Video)
        );

        // With video support, id-cursor batches cover the complete queue once.
        let mut after_id = 0;
        let mut found = Vec::new();
        loop {
            let batch = db.files_needing_meta_after(after_id, 500, true).unwrap();
            if batch.is_empty() {
                break;
            }
            after_id = batch.last().unwrap().id;
            found.extend(batch.into_iter().map(|r| r.id));
        }
        assert_eq!(found.len(), 600);
        assert!(found.windows(2).all(|ids| ids[0] < ids[1]));
    }

    #[test]
    fn volumes_by_serial_survive_letter_change() {
        let db = Db::open_in_memory().unwrap();
        let a = db
            .upsert_volume(&VolumeInfo {
                serial: Some("ABCD".into()),
                label: Some("Card".into()),
                root: r"E:\".into(),
            })
            .unwrap();
        let b = db
            .upsert_volume(&VolumeInfo {
                serial: Some("ABCD".into()),
                label: Some("Card".into()),
                root: r"F:\".into(),
            })
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(db.volumes().unwrap()[0].root, r"F:\");
        // Files on F:\ resolve to the serial volume.
        let ids = db.upsert_files(&[nf(r"F:\DCIM\x.jpg", 1, 1)]).unwrap();
        assert_eq!(db.file(ids[0]).unwrap().unwrap().volume_id, a);
        assert_eq!(db.mark_volume_offline(a).unwrap(), 1);
        assert!(!db.file(ids[0]).unwrap().unwrap().present);
    }

    #[test]
    fn duplicate_groups_and_purge() {
        let db = Db::open_in_memory().unwrap();
        let ids = db
            .upsert_files(&[
                nf(r"C:\d\a", 5, 1),
                nf(r"C:\d\b", 5, 1),
                nf(r"C:\d\c", 5, 1),
            ])
            .unwrap();
        db.set_hashes(ids[0], None, Some("h")).unwrap();
        db.set_hashes(ids[1], None, Some("h")).unwrap();
        let g = db.duplicate_groups(10).unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].len(), 2);
        db.mark_missing(&[PathBuf::from(r"C:\d")]).unwrap();
        db.set_rating(&[ids[0]], 3).unwrap();
        assert_eq!(db.purge_offline(true).unwrap(), 2);
        assert!(db.file(ids[0]).unwrap().is_some());
        assert_eq!(db.purge_offline(false).unwrap(), 1);
        assert!(db.file(ids[0]).unwrap().is_none());
    }

    #[test]
    fn relink_collision_merges_annotations_tags_and_memberships() {
        use crate::annotations::AnnotationChange;
        use forge_core::query::{ColorLabel, PickState};

        let db = Db::open_in_memory().unwrap();
        let source_path = PathBuf::from(r"C:\old\photo.jpg");
        let destination_path = PathBuf::from(r"D:\new\photo.jpg");
        let ids = db
            .upsert_files(&[
                nf(source_path.to_str().unwrap(), 10, 1),
                nf(destination_path.to_str().unwrap(), 20, 2),
            ])
            .unwrap();
        db.annotate(
            &[ids[0]],
            &AnnotationChange {
                rating: Some(5),
                label: Some(Some(ColorLabel::Red)),
                pick: Some(PickState::Pick),
                notes: Some(Some("note from moved original".into())),
                add_tags: vec!["source-tag".into()],
                ..Default::default()
            },
        )
        .unwrap();
        db.annotate(
            &[ids[1]],
            &AnnotationChange {
                rating: Some(3),
                label: Some(Some(ColorLabel::Blue)),
                pick: Some(PickState::Reject),
                add_tags: vec!["destination-tag".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let source_collection = db.create_manual_collection("Moved picks").unwrap();
        let destination_collection = db.create_manual_collection("Existing picks").unwrap();
        db.add_to_collection(source_collection, &[ids[0]]).unwrap();
        db.add_to_collection(destination_collection, &[ids[1]])
            .unwrap();

        let report = db.relink(&source_path, &destination_path, None).unwrap();

        let merged = db.annotation(ids[1]).unwrap();
        assert_eq!(merged.notes.as_deref(), Some("note from moved original"));
        assert_eq!(merged.rating, 5);
        assert_eq!(merged.label, Some(ColorLabel::Red));
        assert_eq!(merged.pick, PickState::Pick);
        assert_eq!(merged.tags, ["destination-tag", "source-tag"]);
        assert_eq!(report.merged, 1);
        assert_eq!(report.conflicts.len(), 3);
        assert_eq!(report.conflicts[0].field, "rating");
        assert_eq!(report.conflicts[1].field, "label");
        assert_eq!(report.conflicts[2].field, "pick");
        assert_eq!(report.conflicts[0].source, source_path);
        assert_eq!(report.conflicts[0].destination, destination_path);
        assert_eq!(
            db.collection(source_collection)
                .unwrap()
                .unwrap()
                .item_count,
            1
        );
        assert_eq!(
            db.collection(destination_collection)
                .unwrap()
                .unwrap()
                .item_count,
            1
        );
    }

    #[test]
    fn relink_marks_existing_destination_present() {
        let db = Db::open_in_memory().unwrap();
        let root = std::env::temp_dir().join(format!(
            "zxraw-relink-present-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let destination = root.join("photo.jpg");
        std::fs::write(&destination, b"photo").unwrap();
        let source = root.join("offline.jpg");
        let source_id = db
            .upsert_files(&[NewFile {
                path: source.clone(),
                kind: EntryKind::File,
                size: 5,
                mtime: Some(1),
                ctime: None,
            }])
            .unwrap()[0];
        db.mark_missing(std::slice::from_ref(&source)).unwrap();

        let report = db.relink(&source, &destination, None).unwrap();

        let moved = db.file(source_id).unwrap().unwrap();
        assert_eq!(moved.path, destination);
        assert!(moved.present);
        assert_eq!(report.moved, 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relink_reports_missing_destination_offline() {
        let db = Db::open_in_memory().unwrap();
        let source = PathBuf::from(r"C:\offline\photo.jpg");
        let destination = PathBuf::from(r"D:\recovered\photo.jpg");
        let id = db
            .upsert_files(&[nf(source.to_str().unwrap(), 1, 1)])
            .unwrap()[0];
        db.mark_missing(std::slice::from_ref(&source)).unwrap();

        let report = db.relink(&source, &destination, None).unwrap();

        assert!(!db.file(id).unwrap().unwrap().present);
        assert_eq!(report.offline_paths, [destination]);
    }
}
