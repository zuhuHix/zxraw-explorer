//! Key/value settings, index roots, exclusions and the indexing checkpoint table.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{DbError, Result};
use crate::files::{SEP, normalize_dir};
use crate::{Db, now_ms, path_str};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexRoot {
    pub id: i64,
    pub path: PathBuf,
    pub enabled: bool,
    pub added_at: i64,
    pub last_complete_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExclusionKind {
    /// A folder path; it and everything below it are skipped.
    Folder,
    /// A file extension (without dot).
    Ext,
    /// A file or folder name anywhere (e.g. `node_modules`, `.git`, `$RECYCLE.BIN`).
    Name,
}

impl ExclusionKind {
    fn as_str(self) -> &'static str {
        match self {
            ExclusionKind::Folder => "folder",
            ExclusionKind::Ext => "ext",
            ExclusionKind::Name => "name",
        }
    }
}

/// All exclusions, lowercased, ready for matching.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Exclusions {
    pub folders: Vec<String>,
    pub exts: Vec<String>,
    pub names: Vec<String>,
}

impl Exclusions {
    /// Is the folder at `path_lc` (lowercased) excluded?
    pub fn excludes_folder(&self, path_lc: &str) -> bool {
        self.folders.iter().any(|f| {
            path_lc == f
                || (path_lc.len() > f.len()
                    && path_lc.starts_with(f.as_str())
                    && (f.ends_with(SEP) || path_lc[f.len()..].starts_with(SEP)))
        })
    }
    pub fn excludes_name(&self, name_lc: &str) -> bool {
        self.names.iter().any(|n| n == name_lc)
    }
    pub fn excludes_ext(&self, ext_lc: &str) -> bool {
        !ext_lc.is_empty() && self.exts.iter().any(|e| e == ext_lc)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ScanStatus {
    Running,
    Interrupted,
    Complete,
    /// Root folder was not accessible (e.g. drive disconnected).
    Unavailable,
}

impl ScanStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ScanStatus::Running => "running",
            ScanStatus::Interrupted => "interrupted",
            ScanStatus::Complete => "complete",
            ScanStatus::Unavailable => "unavailable",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "running" => ScanStatus::Running,
            "interrupted" => ScanStatus::Interrupted,
            "unavailable" => ScanStatus::Unavailable,
            _ => ScanStatus::Complete,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexState {
    pub root_id: i64,
    pub scan_id: i64,
    pub status: ScanStatus,
    pub started_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
    /// Folders still to be listed (resume point).
    pub pending: Vec<PathBuf>,
    pub items_seen: u64,
    pub errors: u64,
    pub last_error: Option<String>,
}

impl Db {
    // -- settings -----------------------------------------------------------------------

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.with_conn(|c| {
            Ok(
                c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                    r.get(0)
                })
                .optional()?,
            )
        })
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO settings(key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![key, value, now_ms()],
            )?;
            Ok(())
        })
    }

    pub fn delete_setting(&self, key: &str) -> Result<bool> {
        self.with_conn(|c| Ok(c.execute("DELETE FROM settings WHERE key = ?1", [key])? > 0))
    }

    pub fn setting_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.setting(key)? {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    pub fn set_setting_json<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.set_setting(key, &serde_json::to_string(value)?)
    }

    // -- index roots --------------------------------------------------------------------

    /// Add (or re-enable) a root folder. Returns its id.
    pub fn add_index_root(&self, path: &Path) -> Result<i64> {
        let p = normalize_dir(&path_str(path));
        if p.is_empty() {
            return Err(DbError::Invalid("index root path is empty".into()));
        }
        self.with_conn(|c| {
            Ok(c.query_row(
                "INSERT INTO index_roots(path, enabled, added_at) VALUES (?1, 1, ?2)
                 ON CONFLICT(path) DO UPDATE SET enabled = 1 RETURNING id",
                params![p, now_ms()],
                |r| r.get(0),
            )?)
        })
    }

    /// Remove a root (its checkpoint goes too; indexed rows and annotations are kept).
    pub fn remove_index_root(&self, id: i64) -> Result<bool> {
        self.with_conn(|c| Ok(c.execute("DELETE FROM index_roots WHERE id = ?1", [id])? > 0))
    }

    pub fn set_index_root_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE index_roots SET enabled = ?2 WHERE id = ?1",
                params![id, enabled],
            )?;
            Ok(())
        })
    }

    pub fn index_roots(&self) -> Result<Vec<IndexRoot>> {
        self.with_conn(|c| {
            let mut st = c.prepare(
                "SELECT id, path, enabled, added_at, last_complete_at FROM index_roots ORDER BY path",
            )?;
            let v = st
                .query_map([], |r| {
                    Ok(IndexRoot {
                        id: r.get(0)?,
                        path: PathBuf::from(r.get::<_, String>(1)?),
                        enabled: r.get(2)?,
                        added_at: r.get(3)?,
                        last_complete_at: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
    }

    // -- exclusions ---------------------------------------------------------------------

    pub fn add_exclusion(&self, kind: ExclusionKind, value: &str) -> Result<()> {
        let mut v = value.trim().to_lowercase();
        match kind {
            ExclusionKind::Folder => v = normalize_dir(&v),
            ExclusionKind::Ext => {
                v = v
                    .trim_start_matches('*')
                    .trim_start_matches('.')
                    .to_string()
            }
            ExclusionKind::Name => {}
        }
        if v.is_empty() {
            return Err(DbError::Invalid("exclusion value is empty".into()));
        }
        self.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO index_exclusions(kind, value) VALUES (?1, ?2)",
                params![kind.as_str(), v],
            )?;
            Ok(())
        })
    }

    pub fn remove_exclusion(&self, kind: ExclusionKind, value: &str) -> Result<bool> {
        let mut v = value.trim().to_lowercase();
        if kind == ExclusionKind::Folder {
            v = normalize_dir(&v);
        }
        if kind == ExclusionKind::Ext {
            v = v
                .trim_start_matches('*')
                .trim_start_matches('.')
                .to_string();
        }
        self.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM index_exclusions WHERE kind = ?1 AND value = ?2",
                params![kind.as_str(), v],
            )? > 0)
        })
    }

    pub fn exclusions(&self) -> Result<Exclusions> {
        self.with_conn(|c| {
            let mut st =
                c.prepare("SELECT kind, value FROM index_exclusions ORDER BY kind, value")?;
            let mut ex = Exclusions::default();
            let rows =
                st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (k, v) = row?;
                match k.as_str() {
                    "folder" => ex.folders.push(v),
                    "ext" => ex.exts.push(v),
                    _ => ex.names.push(v),
                }
            }
            Ok(ex)
        })
    }

    // -- checkpoint ---------------------------------------------------------------------

    pub fn index_state(&self, root_id: i64) -> Result<Option<IndexState>> {
        self.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT root_id, scan_id, status, started_at, updated_at, finished_at, pending,
                            items_seen, errors, last_error
                     FROM index_state WHERE root_id = ?1",
                    [root_id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, Option<i64>>(5)?,
                            r.get::<_, String>(6)?,
                            r.get::<_, i64>(7)?,
                            r.get::<_, i64>(8)?,
                            r.get::<_, Option<String>>(9)?,
                        ))
                    },
                )
                .optional()?;
            let Some((
                root_id,
                scan_id,
                status,
                started_at,
                updated_at,
                finished_at,
                pending,
                items,
                errors,
                last_error,
            )) = row
            else {
                return Ok(None);
            };
            let pending: Vec<String> = serde_json::from_str(&pending).unwrap_or_default();
            Ok(Some(IndexState {
                root_id,
                scan_id,
                status: ScanStatus::parse(&status),
                started_at,
                updated_at,
                finished_at,
                pending: pending.into_iter().map(PathBuf::from).collect(),
                items_seen: items.max(0) as u64,
                errors: errors.max(0) as u64,
                last_error,
            }))
        })
    }

    /// Forget a root's checkpoint so the next run starts a fresh pass.
    pub fn reset_index_state(&self, root_id: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM index_state WHERE root_id = ?1", [root_id])?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_settings() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.setting("theme").unwrap(), None);
        db.set_setting("theme", "dark").unwrap();
        db.set_setting("theme", "light").unwrap();
        assert_eq!(db.setting("theme").unwrap().as_deref(), Some("light"));
        db.set_setting_json("layout", &vec![1, 2, 3]).unwrap();
        assert_eq!(
            db.setting_json::<Vec<i32>>("layout").unwrap(),
            Some(vec![1, 2, 3])
        );
        assert!(db.delete_setting("theme").unwrap());
        assert!(!db.delete_setting("theme").unwrap());
    }

    #[test]
    fn roots_and_exclusions() {
        let db = Db::open_in_memory().unwrap();
        let a = db.add_index_root(Path::new(r"D:\Photos\")).unwrap();
        assert_eq!(db.add_index_root(Path::new(r"d:\photos")).unwrap(), a);
        db.set_index_root_enabled(a, false).unwrap();
        assert!(!db.index_roots().unwrap()[0].enabled);
        assert_eq!(
            db.index_roots().unwrap()[0].path,
            PathBuf::from(r"D:\Photos")
        );

        db.add_exclusion(ExclusionKind::Folder, r"D:\Photos\Cache\")
            .unwrap();
        db.add_exclusion(ExclusionKind::Ext, "*.TMP").unwrap();
        db.add_exclusion(ExclusionKind::Name, "node_modules")
            .unwrap();
        let ex = db.exclusions().unwrap();
        assert!(ex.excludes_folder(r"d:\photos\cache"));
        assert!(ex.excludes_folder(r"d:\photos\cache\x"));
        assert!(!ex.excludes_folder(r"d:\photos\cache2"));
        assert!(ex.excludes_ext("tmp"));
        assert!(!ex.excludes_ext(""));
        assert!(ex.excludes_name("node_modules"));
        assert!(db.remove_exclusion(ExclusionKind::Ext, "tmp").unwrap());
        assert!(db.remove_index_root(a).unwrap());
    }
}
