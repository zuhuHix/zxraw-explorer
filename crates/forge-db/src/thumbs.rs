//! Index of the on-disk thumbnail cache (the image files themselves are owned by
//! forge-media). Supports LRU eviction down to a byte budget.

use std::path::PathBuf;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::error::Result;
use crate::{Db, now_ms, path_str};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThumbRecord {
    /// Cache key (e.g. hash of path + size + mtime + target size).
    pub key: String,
    /// Source file.
    pub path: PathBuf,
    /// Source size in bytes.
    pub size: u64,
    /// Source mtime (unix ms).
    pub mtime: Option<i64>,
    pub width: u32,
    pub height: u32,
    /// Unix ms; set on put and on every get.
    pub last_access: i64,
    /// Size of the cached thumbnail file.
    pub bytes: u64,
}

fn read(r: &rusqlite::Row<'_>) -> rusqlite::Result<ThumbRecord> {
    Ok(ThumbRecord {
        key: r.get(0)?,
        path: PathBuf::from(r.get::<_, String>(1)?),
        size: r.get::<_, i64>(2)?.max(0) as u64,
        mtime: r.get(3)?,
        width: r.get(4)?,
        height: r.get(5)?,
        last_access: r.get(6)?,
        bytes: r.get::<_, i64>(7)?.max(0) as u64,
    })
}

const COLS: &str = "key, path, size, mtime, width, height, last_access, bytes";

impl Db {
    /// Insert or replace an entry; `last_access` is set to now.
    pub fn thumb_put(&self, t: &ThumbRecord) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT OR REPLACE INTO thumbs(key, path, size, mtime, width, height, last_access, bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    t.key,
                    path_str(&t.path),
                    t.size.min(i64::MAX as u64) as i64,
                    t.mtime,
                    t.width,
                    t.height,
                    now_ms(),
                    t.bytes.min(i64::MAX as u64) as i64
                ],
            )?;
            Ok(())
        })
    }

    /// Look up and mark as recently used.
    pub fn thumb_get(&self, key: &str) -> Result<Option<ThumbRecord>> {
        self.with_conn(|c| {
            let now = now_ms();
            c.execute(
                "UPDATE thumbs SET last_access = ?2 WHERE key = ?1",
                params![key, now],
            )?;
            Ok(c.query_row(
                &format!("SELECT {COLS} FROM thumbs WHERE key = ?1"),
                [key],
                read,
            )
            .optional()?)
        })
    }

    /// Mark many keys as used in one transaction (e.g. everything visible in a grid).
    pub fn thumb_touch(&self, keys: &[&str]) -> Result<()> {
        self.with_tx(|tx| {
            let now = now_ms();
            let mut st = tx.prepare_cached("UPDATE thumbs SET last_access = ?2 WHERE key = ?1")?;
            for k in keys {
                st.execute(params![k, now])?;
            }
            Ok(())
        })
    }

    pub fn thumb_remove(&self, key: &str) -> Result<bool> {
        self.with_conn(|c| Ok(c.execute("DELETE FROM thumbs WHERE key = ?1", [key])? > 0))
    }

    /// All cache entries for a source file (to invalidate after edits).
    pub fn thumbs_for_path(&self, path: &std::path::Path) -> Result<Vec<ThumbRecord>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("SELECT {COLS} FROM thumbs WHERE path = ?1"))?;
            let v = st
                .query_map([path_str(path)], read)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
    }

    pub fn thumb_total_bytes(&self) -> Result<u64> {
        self.with_conn(|c| {
            Ok(
                c.query_row("SELECT COALESCE(SUM(bytes), 0) FROM thumbs", [], |r| {
                    r.get::<_, i64>(0)
                })? as u64,
            )
        })
    }

    /// Remove least-recently-used entries until the total is at most `budget` bytes.
    /// Returns the removed entries; the caller deletes their files.
    pub fn thumb_evict_to_budget(&self, budget: u64) -> Result<Vec<ThumbRecord>> {
        self.with_tx(|tx| {
            let mut total: u64 =
                tx.query_row("SELECT COALESCE(SUM(bytes), 0) FROM thumbs", [], |r| {
                    r.get::<_, i64>(0)
                })? as u64;
            let mut evicted = Vec::new();
            if total <= budget {
                return Ok(evicted);
            }
            {
                let mut st = tx.prepare(&format!(
                    "SELECT {COLS} FROM thumbs ORDER BY last_access ASC, key"
                ))?;
                let mut rows = st.query([])?;
                while total > budget {
                    let Some(r) = rows.next()? else { break };
                    let t = read(r)?;
                    total = total.saturating_sub(t.bytes);
                    evicted.push(t);
                }
            }
            let mut del = tx.prepare_cached("DELETE FROM thumbs WHERE key = ?1")?;
            for t in &evicted {
                del.execute([&t.key])?;
            }
            Ok(evicted)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(key: &str, bytes: u64) -> ThumbRecord {
        ThumbRecord {
            key: key.into(),
            path: PathBuf::from(format!(r"C:\src\{key}.jpg")),
            size: 1000,
            mtime: Some(5),
            width: 256,
            height: 171,
            last_access: 0,
            bytes,
        }
    }

    #[test]
    fn lru_eviction() {
        let db = Db::open_in_memory().unwrap();
        for (i, k) in ["a", "b", "c", "d"].iter().enumerate() {
            db.thumb_put(&t(k, 100)).unwrap();
            // Distinct, increasing access times.
            db.with_conn(|c| {
                c.execute(
                    "UPDATE thumbs SET last_access = ?2 WHERE key = ?1",
                    params![k, i as i64],
                )?;
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(db.thumb_total_bytes().unwrap(), 400);
        // Using "a" makes it most recent.
        assert_eq!(db.thumb_get("a").unwrap().unwrap().width, 256);
        let ev = db.thumb_evict_to_budget(250).unwrap();
        assert_eq!(
            ev.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(),
            ["b", "c"]
        );
        assert_eq!(db.thumb_total_bytes().unwrap(), 200);
        assert!(db.thumb_evict_to_budget(1000).unwrap().is_empty());
        assert!(db.thumb_get("b").unwrap().is_none());
        assert_eq!(
            db.thumbs_for_path(&PathBuf::from(r"C:\src\d.jpg"))
                .unwrap()
                .len(),
            1
        );
        assert!(db.thumb_remove("d").unwrap());
        assert_eq!(db.thumb_evict_to_budget(0).unwrap().len(), 1);
    }
}
