//! Manual collections (ordered file references), smart collections (stored query),
//! saved searches, and portable JSON export/import (path based).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use forge_core::model::EntryKind;
use forge_core::query::Query;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::error::{DbError, Result};
use crate::files::{FileId, FileRecord, NewFile, RECORD_COLS, VolumeCache, upsert_files_tx};
use crate::search::SearchOptions;
use crate::{Db, now_ms, path_str};

pub type CollectionId = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectionKind {
    Manual,
    Smart,
}

impl CollectionKind {
    fn as_str(self) -> &'static str {
        match self {
            CollectionKind::Manual => "manual",
            CollectionKind::Smart => "smart",
        }
    }
    fn parse(s: &str) -> Self {
        if s == "smart" {
            CollectionKind::Smart
        } else {
            CollectionKind::Manual
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Collection {
    pub id: CollectionId,
    pub name: String,
    pub kind: CollectionKind,
    /// Smart collections only.
    pub query: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Manual collections: number of items (including offline files). Smart: 0.
    pub item_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SavedSearch {
    pub id: i64,
    pub name: String,
    pub query: String,
    /// Opaque (e.g. JSON-encoded sort settings), owned by the UI.
    pub sort: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Portable export format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionsExport {
    /// Always `"forge-collections"`.
    pub format: String,
    pub version: u32,
    pub exported_at: i64,
    pub collections: Vec<CollectionExport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionExport {
    pub name: String,
    pub kind: CollectionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Full paths, in collection order (manual collections).
    #[serde(default)]
    pub items: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    pub collections_created: u64,
    /// Existing collections with the same name that were merged or resolved by an explicit choice.
    pub collections_merged: u64,
    /// Items whose path was already indexed.
    pub items_linked: u64,
    /// Items not in the index yet: recorded as offline placeholders that become live when
    /// the path is indexed.
    pub items_placeholder: u64,
    /// Same-named smart collections with different queries. No import is applied until every
    /// reported conflict has an explicit resolution.
    pub smart_conflicts: Vec<SmartCollectionConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SmartCollectionConflict {
    pub name: String,
    pub existing_query: String,
    pub imported_query: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartCollectionConflictChoice {
    Replace,
    KeepExisting,
    ImportAsCopy,
}

const EXPORT_FORMAT: &str = "forge-collections";
/// Maximum JSON payload accepted by the library import path.
pub const MAX_COLLECTION_IMPORT_BYTES: usize = 16 * 1024 * 1024;
const MAX_IMPORTED_COLLECTIONS: usize = 10_000;
const MAX_IMPORTED_ITEMS: usize = 250_000;
const MAX_IMPORTED_TEXT_BYTES: usize = 4_096;
const MAX_IMPORTED_PATH_BYTES: usize = 32 * 1024;

fn valid_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(DbError::Invalid("collection name is empty".into()));
    }
    if n.len() > MAX_IMPORTED_TEXT_BYTES {
        return Err(DbError::Invalid("collection name is too long".into()));
    }
    Ok(n.to_string())
}

fn validate_import_path(path: &Path) -> Result<()> {
    let text = path.to_string_lossy();
    let normalized = text.replace('/', "\\").to_lowercase();
    if text.len() > MAX_IMPORTED_PATH_BYTES
        || !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || normalized.starts_with("\\\\?\\")
        || normalized.starts_with("\\\\.\\")
    {
        return Err(DbError::Invalid(format!(
            "unsafe or invalid collection item path: {text}"
        )));
    }
    Ok(())
}

fn insert_collection_tx(
    tx: &Transaction<'_>,
    name: &str,
    kind: CollectionKind,
    query: Option<&str>,
) -> Result<CollectionId> {
    let now = now_ms();
    let r = tx.execute(
        "INSERT INTO collections(name, name_lc, kind, query, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![name, name.to_lowercase(), kind.as_str(), query, now],
    );
    match r {
        Ok(_) => Ok(tx.last_insert_rowid()),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(DbError::Invalid(format!(
                "a collection named '{name}' already exists"
            )))
        }
        Err(e) => Err(e.into()),
    }
}

fn append_items_tx(tx: &Transaction<'_>, id: CollectionId, files: &[FileId]) -> Result<usize> {
    let mut next: i64 = tx.query_row(
        "SELECT COALESCE(MAX(position), -1) + 1 FROM collection_items WHERE collection_id = ?1",
        [id],
        |r| r.get(0),
    )?;
    let mut st = tx.prepare_cached(
        "INSERT OR IGNORE INTO collection_items(collection_id, file_id, position) VALUES (?1, ?2, ?3)",
    )?;
    let mut added = 0;
    for f in files {
        if st.execute(params![id, f, next])? > 0 {
            next += 1;
            added += 1;
        }
    }
    tx.execute(
        "UPDATE collections SET updated_at = ?2 WHERE id = ?1",
        params![id, now_ms()],
    )?;
    Ok(added)
}

fn read_collection(r: &rusqlite::Row<'_>) -> rusqlite::Result<Collection> {
    Ok(Collection {
        id: r.get(0)?,
        name: r.get(1)?,
        kind: CollectionKind::parse(&r.get::<_, String>(2)?),
        query: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
        item_count: r.get::<_, i64>(6)? as u64,
    })
}

const COLLECTION_SELECT: &str = "SELECT c.id, c.name, c.kind, c.query, c.created_at, c.updated_at,
    (SELECT COUNT(*) FROM collection_items ci WHERE ci.collection_id = c.id) FROM collections c";

impl Db {
    pub fn create_manual_collection(&self, name: &str) -> Result<CollectionId> {
        let name = valid_name(name)?;
        self.with_tx(|tx| insert_collection_tx(tx, &name, CollectionKind::Manual, None))
    }

    /// The query is validated (parsed) before storing.
    pub fn create_smart_collection(&self, name: &str, query: &str) -> Result<CollectionId> {
        let name = valid_name(name)?;
        Query::parse(query)?;
        self.with_tx(|tx| insert_collection_tx(tx, &name, CollectionKind::Smart, Some(query)))
    }

    pub fn collections(&self) -> Result<Vec<Collection>> {
        self.with_conn(|c| {
            let mut st = c.prepare(&format!("{COLLECTION_SELECT} ORDER BY c.name_lc"))?;
            let v = st
                .query_map([], read_collection)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
    }

    pub fn collection(&self, id: CollectionId) -> Result<Option<Collection>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                &format!("{COLLECTION_SELECT} WHERE c.id = ?1"),
                [id],
                read_collection,
            )
            .optional()?)
        })
    }

    pub fn collection_by_name(&self, name: &str) -> Result<Option<Collection>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                &format!("{COLLECTION_SELECT} WHERE c.name_lc = ?1"),
                [name.trim().to_lowercase()],
                read_collection,
            )
            .optional()?)
        })
    }

    pub fn rename_collection(&self, id: CollectionId, name: &str) -> Result<()> {
        let name = valid_name(name)?;
        self.with_conn(|c| {
            let n = c
                .execute(
                    "UPDATE collections SET name = ?2, name_lc = ?3, updated_at = ?4 WHERE id = ?1",
                    params![id, name, name.to_lowercase(), now_ms()],
                )
                .map_err(|e| match e {
                    rusqlite::Error::SqliteFailure(f, _)
                        if f.code == rusqlite::ErrorCode::ConstraintViolation =>
                    {
                        DbError::Invalid(format!("a collection named '{name}' already exists"))
                    }
                    e => e.into(),
                })?;
            if n == 0 {
                return Err(DbError::NotFound(format!("collection {id}")));
            }
            Ok(())
        })
    }

    /// Replace a smart collection's query (validated).
    pub fn set_collection_query(&self, id: CollectionId, query: &str) -> Result<()> {
        Query::parse(query)?;
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE collections SET query = ?2, updated_at = ?3 WHERE id = ?1 AND kind = 'smart'",
                params![id, query, now_ms()],
            )?;
            if n == 0 {
                return Err(DbError::NotFound(format!("smart collection {id}")));
            }
            Ok(())
        })
    }

    pub fn delete_collection(&self, id: CollectionId) -> Result<bool> {
        self.with_conn(|c| Ok(c.execute("DELETE FROM collections WHERE id = ?1", [id])? > 0))
    }

    /// Append files (already-present members are ignored). Returns how many were added.
    pub fn add_to_collection(&self, id: CollectionId, files: &[FileId]) -> Result<usize> {
        self.with_tx(|tx| {
            let kind: Option<String> = tx
                .query_row("SELECT kind FROM collections WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()?;
            match kind.as_deref() {
                Some("manual") => append_items_tx(tx, id, files),
                Some(_) => Err(DbError::Invalid(
                    "cannot add files to a smart collection".into(),
                )),
                None => Err(DbError::NotFound(format!("collection {id}"))),
            }
        })
    }

    pub fn remove_from_collection(&self, id: CollectionId, files: &[FileId]) -> Result<usize> {
        self.with_tx(|tx| {
            let mut st = tx.prepare_cached(
                "DELETE FROM collection_items WHERE collection_id = ?1 AND file_id = ?2",
            )?;
            let mut n = 0;
            for f in files {
                n += st.execute(params![id, f])?;
            }
            Ok(n)
        })
    }

    /// Reorder: `order` items come first in that order; the rest keep their relative order.
    pub fn reorder_collection(&self, id: CollectionId, order: &[FileId]) -> Result<()> {
        self.with_tx(|tx| {
            let current: Vec<FileId> = {
                let mut st = tx.prepare(
                    "SELECT file_id FROM collection_items WHERE collection_id = ?1 ORDER BY position",
                )?;
                st.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            let mut seen = std::collections::HashSet::new();
            let mut final_order = Vec::with_capacity(current.len());
            for f in order.iter().chain(current.iter()) {
                if current.contains(f) && seen.insert(*f) {
                    final_order.push(*f);
                }
            }
            let mut st = tx.prepare_cached(
                "UPDATE collection_items SET position = ?3 WHERE collection_id = ?1 AND file_id = ?2",
            )?;
            for (i, f) in final_order.iter().enumerate() {
                st.execute(params![id, f, i as i64])?;
            }
            Ok(())
        })
    }

    /// Members of a collection. Manual: in collection order, offline files included (check
    /// `present`). Smart: the stored query run with `opts`.
    pub fn collection_files(
        &self,
        id: CollectionId,
        opts: &SearchOptions,
    ) -> Result<Vec<FileRecord>> {
        let col = self
            .collection(id)?
            .ok_or_else(|| DbError::NotFound(format!("collection {id}")))?;
        match col.kind {
            CollectionKind::Smart => {
                let q = Query::parse(col.query.as_deref().unwrap_or(""))?;
                self.search(&q, opts)
            }
            CollectionKind::Manual => self.with_conn(|c| {
                let mut st = c.prepare_cached(&format!(
                    "SELECT {RECORD_COLS} FROM collection_items ci
                     JOIN files f ON f.id = ci.file_id
                     LEFT JOIN annotations a ON a.file_id = f.id
                     WHERE ci.collection_id = ?1 ORDER BY ci.position LIMIT ?2 OFFSET ?3"
                ))?;
                let rows = st
                    .query_map(
                        params![id, opts.limit.map_or(-1, i64::from), i64::from(opts.offset)],
                        FileRecord::from_row,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            }),
        }
    }

    // -- saved searches ---------------------------------------------------------------

    /// Create or replace (by name, case-insensitive) a saved search. Query is validated.
    pub fn save_search(&self, name: &str, query: &str, sort: Option<&str>) -> Result<i64> {
        let name = valid_name(name)?;
        Query::parse(query)?;
        self.with_conn(|c| {
            let now = now_ms();
            Ok(c.query_row(
                "INSERT INTO saved_searches(name, name_lc, query, sort, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(name_lc) DO UPDATE SET name = excluded.name, query = excluded.query,
                     sort = excluded.sort, updated_at = excluded.updated_at
                 RETURNING id",
                params![name, name.to_lowercase(), query, sort, now],
                |r| r.get(0),
            )?)
        })
    }

    pub fn saved_searches(&self) -> Result<Vec<SavedSearch>> {
        self.with_conn(|c| {
            let mut st = c.prepare(
                "SELECT id, name, query, sort, created_at, updated_at FROM saved_searches ORDER BY name_lc",
            )?;
            let v = st
                .query_map([], |r| {
                    Ok(SavedSearch {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        query: r.get(2)?,
                        sort: r.get(3)?,
                        created_at: r.get(4)?,
                        updated_at: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
    }

    pub fn delete_saved_search(&self, name: &str) -> Result<bool> {
        self.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM saved_searches WHERE name_lc = ?1",
                [name.trim().to_lowercase()],
            )? > 0)
        })
    }

    // -- export / import ----------------------------------------------------------------

    /// Export the given collections (all if `ids` is empty).
    pub fn export_collections(&self, ids: &[CollectionId]) -> Result<CollectionsExport> {
        let cols: Vec<Collection> = self
            .collections()?
            .into_iter()
            .filter(|c| ids.is_empty() || ids.contains(&c.id))
            .collect();
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT f.path FROM collection_items ci JOIN files f ON f.id = ci.file_id
                 WHERE ci.collection_id = ?1 ORDER BY ci.position",
            )?;
            let mut out = Vec::with_capacity(cols.len());
            for col in cols {
                let items = if col.kind == CollectionKind::Manual {
                    st.query_map([col.id], |r| Ok(PathBuf::from(r.get::<_, String>(0)?)))?
                        .collect::<rusqlite::Result<_>>()?
                } else {
                    Vec::new()
                };
                out.push(CollectionExport {
                    name: col.name,
                    kind: col.kind,
                    query: col.query,
                    items,
                });
            }
            Ok(CollectionsExport {
                format: EXPORT_FORMAT.into(),
                version: 1,
                exported_at: now_ms(),
                collections: out,
            })
        })
    }

    pub fn export_collections_json(&self, ids: &[CollectionId]) -> Result<String> {
        Ok(serde_json::to_string_pretty(
            &self.export_collections(ids)?,
        )?)
    }

    /// Import in one transaction. Same-named manual collections are merged. A smart collection
    /// with a different query is reported and the whole import is left unapplied until resolved.
    pub fn import_collections(&self, data: &CollectionsExport) -> Result<ImportReport> {
        self.import_collections_with_choices(data, &HashMap::new())
    }

    /// Import with explicit choices for conflicting smart collections, keyed by lower-cased name.
    pub fn import_collections_with_choices(
        &self,
        data: &CollectionsExport,
        choices: &HashMap<String, SmartCollectionConflictChoice>,
    ) -> Result<ImportReport> {
        if data.format != EXPORT_FORMAT {
            return Err(DbError::Invalid(format!(
                "not a ZXraw Explorer collections file (format '{}')",
                data.format
            )));
        }
        if data.version != 1 {
            return Err(DbError::Invalid(format!(
                "unsupported collections file version {}",
                data.version
            )));
        }
        if data.collections.len() > MAX_IMPORTED_COLLECTIONS {
            return Err(DbError::Invalid("too many collections in import".into()));
        }
        let mut total_items = 0usize;
        for c in &data.collections {
            valid_name(&c.name)?;
            if c.query
                .as_ref()
                .is_some_and(|query| query.len() > MAX_IMPORTED_TEXT_BYTES)
            {
                return Err(DbError::Invalid("collection query is too long".into()));
            }
            if c.kind == CollectionKind::Smart {
                Query::parse(c.query.as_deref().unwrap_or(""))?;
            }
            total_items = total_items
                .checked_add(c.items.len())
                .ok_or_else(|| DbError::Invalid("too many collection items in import".into()))?;
            if total_items > MAX_IMPORTED_ITEMS {
                return Err(DbError::Invalid(
                    "too many collection items in import".into(),
                ));
            }
            for path in &c.items {
                validate_import_path(path)?;
            }
        }
        self.with_tx(|tx| {
            let mut unresolved = ImportReport::default();
            for c in &data.collections {
                if c.kind != CollectionKind::Smart {
                    continue;
                }
                let name = c.name.trim();
                let existing: Option<(String, Option<String>)> = tx
                    .query_row(
                        "SELECT kind, query FROM collections WHERE name_lc = ?1",
                        [name.to_lowercase()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some((kind, existing_query)) = existing
                    && kind == CollectionKind::Smart.as_str()
                    && existing_query.as_deref() != c.query.as_deref()
                    && !choices.contains_key(&name.to_lowercase())
                {
                    unresolved.smart_conflicts.push(SmartCollectionConflict {
                        name: name.to_string(),
                        existing_query: existing_query.unwrap_or_default(),
                        imported_query: c.query.clone().unwrap_or_default(),
                    });
                }
            }
            if !unresolved.smart_conflicts.is_empty() {
                return Ok(unresolved);
            }

            let mut report = ImportReport::default();
            let mut vols = VolumeCache::new();
            let mut find = tx.prepare_cached("SELECT id FROM files WHERE path = ?1")?;
            for c in &data.collections {
                let name = c.name.trim();
                let existing: Option<(i64, String, Option<String>)> = tx
                    .query_row(
                        "SELECT id, kind, query FROM collections WHERE name_lc = ?1",
                        [name.to_lowercase()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                let id = match existing {
                    Some((id, kind, _))
                        if kind == c.kind.as_str() && c.kind == CollectionKind::Manual =>
                    {
                        report.collections_merged += 1;
                        id
                    }
                    Some((id, kind, existing_query))
                        if kind == CollectionKind::Smart.as_str()
                            && c.kind == CollectionKind::Smart
                            && existing_query.as_deref() == c.query.as_deref() =>
                    {
                        report.collections_merged += 1;
                        id
                    }
                    Some((id, kind, _))
                        if kind == CollectionKind::Smart.as_str()
                            && c.kind == CollectionKind::Smart =>
                    {
                        match choices.get(&name.to_lowercase()) {
                            Some(SmartCollectionConflictChoice::Replace) => {
                                tx.execute(
                                    "UPDATE collections SET query = ?2, updated_at = ?3 WHERE id = ?1",
                                    params![id, c.query, now_ms()],
                                )?;
                                report.collections_merged += 1;
                                id
                            }
                            Some(SmartCollectionConflictChoice::KeepExisting) => {
                                report.collections_merged += 1;
                                id
                            }
                            Some(SmartCollectionConflictChoice::ImportAsCopy) => {
                                let copy_name = copy_collection_name_tx(tx, name)?;
                                report.collections_created += 1;
                                insert_collection_tx(
                                    tx,
                                    &copy_name,
                                    CollectionKind::Smart,
                                    c.query.as_deref(),
                                )?
                            }
                            None => unreachable!("unresolved conflicts returned before import"),
                        }
                    }
                    Some(_) => {
                        return Err(DbError::Invalid(format!(
                            "collection '{name}' exists with a different kind"
                        )));
                    }
                    None => {
                        report.collections_created += 1;
                        insert_collection_tx(tx, name, c.kind, c.query.as_deref())?
                    }
                };
                if c.kind != CollectionKind::Manual {
                    continue;
                }
                let mut file_ids = Vec::with_capacity(c.items.len());
                for p in &c.items {
                    if let Some(fid) = find
                        .query_row([path_str(p)], |r| r.get::<_, i64>(0))
                        .optional()?
                    {
                        report.items_linked += 1;
                        file_ids.push(fid);
                    } else {
                        let nf = NewFile {
                            path: p.clone(),
                            kind: EntryKind::File,
                            size: 0,
                            mtime: None,
                            ctime: None,
                        };
                        let fid = upsert_files_tx(tx, &[nf], None, None, &mut vols)?[0];
                        tx.execute(
                            "UPDATE files SET present = 0, indexed_at = 0 WHERE id = ?1",
                            [fid],
                        )?;
                        report.items_placeholder += 1;
                        file_ids.push(fid);
                    }
                }
                append_items_tx(tx, id, &file_ids)?;
            }
            Ok(report)
        })
    }

    pub fn import_collections_json(&self, json: &str) -> Result<ImportReport> {
        if json.len() > MAX_COLLECTION_IMPORT_BYTES {
            return Err(DbError::Invalid(
                "collections import exceeds the size limit".into(),
            ));
        }
        let data: CollectionsExport = serde_json::from_str(json)?;
        self.import_collections(&data)
    }

    pub fn import_collections_json_with_choices(
        &self,
        json: &str,
        choices: &HashMap<String, SmartCollectionConflictChoice>,
    ) -> Result<ImportReport> {
        let data: CollectionsExport = serde_json::from_str(json)?;
        self.import_collections_with_choices(&data, choices)
    }
}

fn copy_collection_name_tx(tx: &Transaction<'_>, name: &str) -> Result<String> {
    for copy_number in 2..=u64::MAX {
        let candidate = format!("{name} ({copy_number})");
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM collections WHERE name_lc = ?1)",
            [candidate.to_lowercase()],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(candidate);
        }
    }
    Err(DbError::Invalid(
        "could not find an available collection copy name".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn nf(p: &str) -> NewFile {
        NewFile {
            path: PathBuf::from(p),
            kind: EntryKind::File,
            size: 1,
            mtime: Some(1),
            ctime: None,
        }
    }

    #[test]
    fn manual_collection_crud_and_order() {
        let db = Db::open_in_memory().unwrap();
        let ids = db
            .upsert_files(&[nf(r"C:\c\1.jpg"), nf(r"C:\c\2.jpg"), nf(r"C:\c\3.jpg")])
            .unwrap();
        let c = db.create_manual_collection("Selects").unwrap();
        assert!(db.create_manual_collection(" selects ").is_err());
        assert_eq!(
            db.add_to_collection(c, &[ids[2], ids[0], ids[2]]).unwrap(),
            2
        );
        assert_eq!(db.add_to_collection(c, &[ids[1]]).unwrap(), 1);
        let order = |db: &Db| -> Vec<FileId> {
            db.collection_files(c, &SearchOptions::default())
                .unwrap()
                .iter()
                .map(|r| r.id)
                .collect()
        };
        assert_eq!(order(&db), vec![ids[2], ids[0], ids[1]]);
        db.reorder_collection(c, &[ids[1]]).unwrap();
        assert_eq!(order(&db), vec![ids[1], ids[2], ids[0]]);
        db.remove_from_collection(c, &[ids[2]]).unwrap();
        assert_eq!(order(&db), vec![ids[1], ids[0]]);
        db.rename_collection(c, "Final").unwrap();
        let col = db.collection_by_name("FINAL").unwrap().unwrap();
        assert_eq!((col.id, col.item_count), (c, 2));
        // Offline members stay in the collection.
        db.mark_missing(&[PathBuf::from(r"C:\c")]).unwrap();
        let files = db.collection_files(c, &SearchOptions::default()).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| !f.present));
        assert!(db.delete_collection(c).unwrap());
        assert!(db.collections().unwrap().is_empty());
    }

    #[test]
    fn import_rejects_malformed_oversized_and_traversal_without_partial_writes() {
        let db = Db::open_in_memory().unwrap();
        db.create_manual_collection("Before").unwrap();

        assert!(db.import_collections_json("{broken").is_err());
        assert!(
            db.import_collections_json(&" ".repeat(MAX_COLLECTION_IMPORT_BYTES + 1))
                .is_err()
        );

        let hostile = CollectionsExport {
            format: EXPORT_FORMAT.into(),
            version: 1,
            exported_at: 0,
            collections: vec![CollectionExport {
                name: "Would be partial".into(),
                kind: CollectionKind::Manual,
                query: None,
                items: vec![
                    PathBuf::from(r"C:\photos\safe.jpg"),
                    PathBuf::from(r"C:\photos\..\escape.jpg"),
                ],
            }],
        };
        let json = serde_json::to_string(&hostile).unwrap();
        assert!(db.import_collections_json(&json).is_err());
        assert_eq!(
            db.collections()
                .unwrap()
                .iter()
                .map(|collection| collection.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Before"]
        );
        assert_eq!(db.stats().unwrap().files, 0);
    }

    #[test]
    fn export_import_round_trip_keeps_manual_paths() {
        let source = Db::open_in_memory().unwrap();
        let id = source.create_manual_collection("Favorites").unwrap();
        let files = source
            .upsert_files(&[nf(r"C:\photos\one.jpg"), nf(r"C:\photos\two.jpg")])
            .unwrap();
        source.add_to_collection(id, &files).unwrap();
        let json = source.export_collections_json(&[id]).unwrap();

        let target = Db::open_in_memory().unwrap();
        let report = target.import_collections_json(&json).unwrap();
        assert_eq!(report.collections_created, 1);
        assert_eq!(report.items_placeholder, 2);
        let collection = target.collection_by_name("favorites").unwrap().unwrap();
        let members = target
            .collection_files(
                collection.id,
                &SearchOptions {
                    include_offline: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            members
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            vec!["one.jpg", "two.jpg"]
        );
        assert!(members.iter().all(|row| !row.present));
    }

    #[test]
    fn smart_collections_and_saved_searches() {
        let db = Db::open_in_memory().unwrap();
        let ids = db
            .upsert_files(&[nf(r"C:\s\a.jpg"), nf(r"C:\s\b.png")])
            .unwrap();
        db.set_rating(&ids[..1], 5).unwrap();
        assert!(matches!(
            db.create_smart_collection("Bad", "rating:>>"),
            Err(DbError::Query(_))
        ));
        let s = db
            .create_smart_collection("Five stars", "rating:5")
            .unwrap();
        assert!(db.add_to_collection(s, &ids).is_err());
        let r = db.collection_files(s, &SearchOptions::default()).unwrap();
        assert_eq!(r.len(), 1);
        db.set_collection_query(s, "ext:png").unwrap();
        assert_eq!(
            db.collection_files(s, &SearchOptions::default()).unwrap()[0].id,
            ids[1]
        );

        let a = db
            .save_search("Pngs", "ext:png", Some("{\"sort\":\"Size\"}"))
            .unwrap();
        let b = db.save_search("PNGS", "ext:png size:>1", None).unwrap();
        assert_eq!(a, b);
        let ss = db.saved_searches().unwrap();
        assert_eq!(ss.len(), 1);
        assert_eq!(ss[0].query, "ext:png size:>1");
        assert!(db.save_search("x", "(", None).is_err());
        assert!(db.delete_saved_search("pngs").unwrap());
    }

    #[test]
    fn export_import_roundtrip_with_placeholders() {
        let src = Db::open_in_memory().unwrap();
        let ids = src
            .upsert_files(&[
                nf(r"E:\shoot\a.cr3"),
                nf(r"E:\shoot\b.cr3"),
                nf(r"E:\shoot\日本.cr3"),
            ])
            .unwrap();
        let m = src.create_manual_collection("Client picks").unwrap();
        src.add_to_collection(m, &[ids[2], ids[0]]).unwrap();
        src.create_smart_collection("Raws", "kind:raw").unwrap();
        let json = src.export_collections_json(&[]).unwrap();
        assert!(json.contains("forge-collections"));

        let dst = Db::open_in_memory().unwrap();
        dst.upsert_files(&[nf(r"E:\shoot\a.cr3")]).unwrap();
        let rep = dst.import_collections_json(&json).unwrap();
        assert_eq!(rep.collections_created, 2);
        assert_eq!(rep.items_linked, 1);
        assert_eq!(rep.items_placeholder, 1);
        let c = dst.collection_by_name("client picks").unwrap().unwrap();
        let files = dst
            .collection_files(c.id, &SearchOptions::default())
            .unwrap();
        assert_eq!(
            files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            ["日本.cr3", "a.cr3"]
        );
        assert!(!files[0].present);
        // The placeholder goes live once the path is indexed.
        dst.upsert_files(&[nf(r"E:\shoot\日本.cr3")]).unwrap();
        assert!(
            dst.file_by_path(Path::new(r"E:\shoot\日本.cr3"))
                .unwrap()
                .unwrap()
                .present
        );
        // Importing again merges instead of duplicating.
        let rep2 = dst.import_collections_json(&json).unwrap();
        assert_eq!(rep2.collections_merged, 2);
        assert_eq!(dst.collection(c.id).unwrap().unwrap().item_count, 2);
        assert!(
            dst.import_collections_json(
                "{\"format\":\"x\",\"version\":1,\"exported_at\":0,\"collections\":[]}"
            )
            .is_err()
        );
    }

    #[test]
    fn import_reports_smart_collection_query_conflict_without_replacing() {
        let db = Db::open_in_memory().unwrap();
        db.create_smart_collection("Raws", "kind:raw").unwrap();
        let incoming = CollectionsExport {
            format: EXPORT_FORMAT.into(),
            version: 1,
            exported_at: 0,
            collections: vec![CollectionExport {
                name: "Raws".into(),
                kind: CollectionKind::Smart,
                query: Some("kind:image".into()),
                items: Vec::new(),
            }],
        };

        let report = db.import_collections(&incoming).unwrap();

        assert_eq!(
            db.collection_by_name("Raws")
                .unwrap()
                .unwrap()
                .query
                .as_deref(),
            Some("kind:raw")
        );
        assert_eq!(report.smart_conflicts.len(), 1);
        assert_eq!(report.smart_conflicts[0].name, "Raws");
        assert_eq!(report.smart_conflicts[0].existing_query, "kind:raw");
        assert_eq!(report.smart_conflicts[0].imported_query, "kind:image");

        let choice = HashMap::from([(
            "raws".to_string(),
            SmartCollectionConflictChoice::ImportAsCopy,
        )]);
        let resolved = db
            .import_collections_with_choices(&incoming, &choice)
            .unwrap();
        assert_eq!(resolved.collections_created, 1);
        assert_eq!(
            db.collection_by_name("Raws")
                .unwrap()
                .unwrap()
                .query
                .as_deref(),
            Some("kind:raw")
        );
        assert_eq!(
            db.collection_by_name("Raws (2)")
                .unwrap()
                .unwrap()
                .query
                .as_deref(),
            Some("kind:image")
        );

        let keep_choice = HashMap::from([(
            "raws".to_string(),
            SmartCollectionConflictChoice::KeepExisting,
        )]);
        let mut later_import = incoming.clone();
        later_import.collections[0].query = Some("kind:video".into());
        db.import_collections_with_choices(&later_import, &keep_choice)
            .unwrap();
        assert_eq!(
            db.collection_by_name("Raws")
                .unwrap()
                .unwrap()
                .query
                .as_deref(),
            Some("kind:raw")
        );

        let replace_choice =
            HashMap::from([("raws".to_string(), SmartCollectionConflictChoice::Replace)]);
        db.import_collections_with_choices(&later_import, &replace_choice)
            .unwrap();
        assert_eq!(
            db.collection_by_name("Raws")
                .unwrap()
                .unwrap()
                .query
                .as_deref(),
            Some("kind:video")
        );
    }
}
