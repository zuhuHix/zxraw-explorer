//! Manual collections (ordered file references), smart collections (stored query),
//! saved searches, and portable JSON export/import (path based).

use std::path::PathBuf;

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
    /// Existing collections with the same name that items were appended to (manual) or
    /// whose query was replaced (smart).
    pub collections_merged: u64,
    /// Items whose path was already indexed.
    pub items_linked: u64,
    /// Items not in the index yet: recorded as offline placeholders that become live when
    /// the path is indexed.
    pub items_placeholder: u64,
}

const EXPORT_FORMAT: &str = "forge-collections";

fn valid_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(DbError::Invalid("collection name is empty".into()));
    }
    Ok(n.to_string())
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

    /// Import in one transaction. Same-named collections are merged.
    pub fn import_collections(&self, data: &CollectionsExport) -> Result<ImportReport> {
        if data.format != EXPORT_FORMAT {
            return Err(DbError::Invalid(format!(
                "not a Forge collections file (format '{}')",
                data.format
            )));
        }
        if data.version != 1 {
            return Err(DbError::Invalid(format!(
                "unsupported collections file version {}",
                data.version
            )));
        }
        for c in &data.collections {
            valid_name(&c.name)?;
            if c.kind == CollectionKind::Smart {
                Query::parse(c.query.as_deref().unwrap_or(""))?;
            }
        }
        self.with_tx(|tx| {
            let mut report = ImportReport::default();
            let mut vols = VolumeCache::new();
            let mut find = tx.prepare_cached("SELECT id FROM files WHERE path = ?1")?;
            for c in &data.collections {
                let name = c.name.trim();
                let existing: Option<(i64, String)> = tx
                    .query_row(
                        "SELECT id, kind FROM collections WHERE name_lc = ?1",
                        [name.to_lowercase()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let id = match existing {
                    Some((id, kind)) if kind == c.kind.as_str() => {
                        report.collections_merged += 1;
                        if c.kind == CollectionKind::Smart {
                            tx.execute(
                                "UPDATE collections SET query = ?2, updated_at = ?3 WHERE id = ?1",
                                params![id, c.query, now_ms()],
                            )?;
                        }
                        id
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
        let data: CollectionsExport = serde_json::from_str(json)?;
        self.import_collections(&data)
    }
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
}
