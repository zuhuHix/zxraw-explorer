//! SQLite persistence for ZXraw Explorer: file index, media metadata, ratings/labels/tags,
//! collections, saved searches, thumbnail-cache index, indexing state and settings.
//!
//! * One [`Db`] wraps one SQLite connection behind a mutex (`Db: Send + Sync`), so it can
//!   be shared as `Arc<Db>`. Open a second `Db` on the same file for a concurrent reader;
//!   WAL mode lets readers proceed while the indexer writes.
//! * All user-provided values reach SQL as bound parameters — never interpolated.
//! * Timestamps are Unix epoch **milliseconds** (`i64`) everywhere.
//! * Paths are stored as full strings; `path` is unique (ASCII case-insensitive, as on
//!   Windows). Rows are never deleted when files disappear (`present = 0`), so annotations
//!   survive offline drives and can be moved with [`Db::relink`].

mod annotations;
mod collections;
mod error;
mod files;
pub mod indexer;
mod schema;
mod search;
mod settings;
mod thumbs;

use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;
use serde::Serialize;

pub use annotations::{Annotation, AnnotationChange, TagCount};
pub use collections::{
    Collection, CollectionExport, CollectionId, CollectionKind, CollectionsExport, ImportReport,
    SavedSearch, SmartCollectionConflict, SmartCollectionConflictChoice,
};
pub use error::{DbError, Result};
pub use files::{
    FileId, FileRecord, ImageMeta, NewFile, RelinkConflict, RelinkReport, VideoMeta, Volume,
    VolumeId, VolumeInfo, media_kind_from_str, media_kind_str, volume_root_of,
};
pub use indexer::{IndexOutcome, IndexReport, Indexer, ReconcileReport, RootReport};
pub use schema::latest_version;
pub use search::{CompiledQuery, SearchOptions, SearchScope, SortField, compile_query};
pub use settings::{ExclusionKind, Exclusions, IndexRoot, IndexState, ScanStatus};
pub use thumbs::ThumbRecord;

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Canonical stored form of a path. On Windows `/` is normalized to `\` (both are valid
/// separators there) so lookups match regardless of how a caller joined the path.
pub(crate) fn path_str(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) && s.contains('/') {
        s.replace('/', "\\")
    } else {
        s.into_owned()
    }
}

/// Handle to the ZXraw Explorer database.
pub struct Db {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
    recovered_from: Option<PathBuf>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.path)
            .field("recovered_from", &self.recovered_from)
            .finish()
    }
}

impl Db {
    /// Open (creating if needed) the database at `path` and apply pending migrations.
    ///
    /// If the file is corrupt or not a SQLite database, it (and its `-wal`/`-shm`
    /// companions) is renamed aside to `<name>.corrupt-<timestamp>` and a fresh database
    /// is created; [`Db::recovered_corrupt_file`] then returns the renamed path so the UI
    /// can tell the user.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| DbError::io(parent, e))?;
        }
        match open_conn(&path) {
            Ok(conn) => Ok(Self {
                conn: Mutex::new(conn),
                path: Some(path),
                recovered_from: None,
            }),
            Err(e) if e.is_corruption() => {
                let aside = move_aside(&path)?;
                tracing::error!(
                    "database {} is corrupt ({e}); moved to {} and starting fresh",
                    path.display(),
                    aside.display()
                );
                let conn = open_conn(&path)?;
                Ok(Self {
                    conn: Mutex::new(conn),
                    path: Some(path),
                    recovered_from: Some(aside),
                })
            }
            Err(e) => Err(e),
        }
    }

    /// A private in-memory database (tests, previews).
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        configure(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: None,
            recovered_from: None,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// If opening found a corrupt database, where the bad file was moved.
    pub fn recovered_corrupt_file(&self) -> Option<&Path> {
        self.recovered_from.as_deref()
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.with_conn(|c| schema::current_version(c))
    }

    /// Full `PRAGMA integrity_check`. Returns an empty vector when the database is fine.
    /// Expensive on large databases; run it from a maintenance job.
    pub fn integrity_check(&self) -> Result<Vec<String>> {
        self.with_conn(|c| {
            let mut st = c.prepare("PRAGMA integrity_check")?;
            let rows: Vec<String> = st
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows.into_iter().filter(|r| r != "ok").collect())
        })
    }

    /// Reclaim space and refresh planner statistics.
    pub fn optimize(&self) -> Result<()> {
        self.with_conn(|c| {
            c.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);")?;
            Ok(())
        })
    }

    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut c = self.conn.lock();
        f(&mut c)
    }

    /// Run `f` inside one transaction (committed if `f` succeeds).
    pub(crate) fn with_tx<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.with_conn(|c| {
            let tx = c.transaction()?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })
    }

    /// Database-wide counters for a status/maintenance page.
    pub fn stats(&self) -> Result<DbStats> {
        self.with_conn(|c| {
            let one = |sql: &str| -> rusqlite::Result<i64> { c.query_row(sql, [], |r| r.get(0)) };
            let mut st = c.prepare(
                "SELECT media_kind, COUNT(*), COALESCE(SUM(size), 0) FROM files
                 WHERE present = 1 AND kind = 'file' GROUP BY media_kind ORDER BY 2 DESC",
            )?;
            let by_media_kind = st
                .query_map([], |r| {
                    Ok(MediaKindStats {
                        media_kind: r.get(0)?,
                        files: r.get::<_, i64>(1)? as u64,
                        bytes: r.get::<_, i64>(2)? as u64,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let page_count = one("PRAGMA page_count")?;
            let page_size = one("PRAGMA page_size")?;
            Ok(DbStats {
                files: one("SELECT COUNT(*) FROM files WHERE kind <> 'dir'")? as u64,
                dirs: one("SELECT COUNT(*) FROM files WHERE kind = 'dir'")? as u64,
                present: one("SELECT COUNT(*) FROM files WHERE present = 1")? as u64,
                offline: one("SELECT COUNT(*) FROM files WHERE present = 0")? as u64,
                total_bytes: one(
                    "SELECT COALESCE(SUM(size), 0) FROM files WHERE present = 1 AND kind = 'file'",
                )? as u64,
                by_media_kind,
                volumes: one("SELECT COUNT(*) FROM volumes")? as u64,
                image_meta: one("SELECT COUNT(*) FROM image_meta")? as u64,
                video_meta: one("SELECT COUNT(*) FROM video_meta")? as u64,
                rated: one("SELECT COUNT(*) FROM annotations WHERE rating > 0")? as u64,
                labeled: one("SELECT COUNT(*) FROM annotations WHERE label IS NOT NULL")? as u64,
                tags: one("SELECT COUNT(*) FROM tags")? as u64,
                tagged_files: one("SELECT COUNT(DISTINCT file_id) FROM file_tags")? as u64,
                collections: one("SELECT COUNT(*) FROM collections")? as u64,
                saved_searches: one("SELECT COUNT(*) FROM saved_searches")? as u64,
                thumbs: one("SELECT COUNT(*) FROM thumbs")? as u64,
                thumb_bytes: one("SELECT COALESCE(SUM(bytes), 0) FROM thumbs")? as u64,
                db_bytes: (page_count * page_size) as u64,
            })
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MediaKindStats {
    pub media_kind: String,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DbStats {
    /// Files and links (not folders), present or not.
    pub files: u64,
    pub dirs: u64,
    pub present: u64,
    pub offline: u64,
    /// Bytes of present files.
    pub total_bytes: u64,
    pub by_media_kind: Vec<MediaKindStats>,
    pub volumes: u64,
    pub image_meta: u64,
    pub video_meta: u64,
    pub rated: u64,
    pub labeled: u64,
    pub tags: u64,
    pub tagged_files: u64,
    pub collections: u64,
    pub saved_searches: u64,
    pub thumbs: u64,
    pub thumb_bytes: u64,
    pub db_bytes: u64,
}

fn open_conn(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    configure(&mut conn)?;
    Ok(conn)
}

fn configure(conn: &mut Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    // Reading the journal mode touches the file header: a non-database file fails here.
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -16000;",
    )?;
    register_functions(conn)?;
    schema::migrate(conn)?;
    Ok(())
}

/// `regexp(pattern, text)` (backs the `REGEXP` operator) and `ulower(text)` (Unicode
/// lowercase; SQLite's built-in `lower()` only folds ASCII).
fn register_functions(conn: &Connection) -> Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    conn.create_scalar_function("regexp", 2, flags, |ctx| {
        let re = ctx.get_or_create_aux(
            0,
            |v| -> std::result::Result<regex::Regex, Box<dyn std::error::Error + Send + Sync>> {
                let p = v.as_str()?;
                Ok(forge_core::query::build_name_regex(p)?)
            },
        )?;
        let text = match ctx.get_raw(1) {
            rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
            _ => return Ok(false),
        };
        Ok(re.is_match(&text))
    })?;
    conn.create_scalar_function("ulower", 1, flags, |ctx| {
        Ok(match ctx.get_raw(0) {
            rusqlite::types::ValueRef::Text(t) => Some(String::from_utf8_lossy(t).to_lowercase()),
            _ => None,
        })
    })?;
    Ok(())
}

/// Rename a corrupt database (and its WAL/SHM files) aside. Returns the new main path.
fn move_aside(path: &Path) -> Result<PathBuf> {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "forge.db".into());
    let mut aside = path.with_file_name(format!("{file_name}.corrupt-{ts}"));
    let mut n = 1;
    while aside.exists() {
        aside = path.with_file_name(format!("{file_name}.corrupt-{ts}-{n}"));
        n += 1;
    }
    std::fs::rename(path, &aside).map_err(|e| DbError::io(path, e))?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", path.display()));
        if side.exists() {
            let to = PathBuf::from(format!("{}{suffix}", aside.display()));
            if std::fs::rename(&side, &to).is_err() {
                // A stale WAL next to a fresh database would be replayed into it.
                let _ = std::fs::remove_file(&side);
            }
        }
    }
    Ok(aside)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Migration, migrate_with};

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("forge.db");
        {
            let db = Db::open(&p).unwrap();
            assert_eq!(db.schema_version().unwrap(), latest_version());
            assert!(db.recovered_corrupt_file().is_none());
        }
        let db = Db::open(&p).unwrap();
        assert_eq!(db.schema_version().unwrap(), latest_version());
        let n: i64 = db
            .with_conn(|c| {
                Ok(c.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(n, schema::MIGRATIONS.len() as i64);
        // WAL + foreign keys are on.
        let (mode, fk): (String, i64) = db
            .with_conn(|c| {
                Ok((
                    c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?,
                    c.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?,
                ))
            })
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        assert_eq!(fk, 1);
        assert!(db.integrity_check().unwrap().is_empty());
    }

    #[test]
    fn forward_only_migrations_apply_only_new_versions() {
        let mut c = Connection::open_in_memory().unwrap();
        let v1 = [Migration {
            version: 1,
            description: "a",
            sql: "CREATE TABLE a(x);",
        }];
        assert_eq!(migrate_with(&mut c, &v1).unwrap(), vec![1]);
        assert!(migrate_with(&mut c, &v1).unwrap().is_empty());
        let v2 = [
            Migration {
                version: 1,
                description: "a",
                sql: "CREATE TABLE a(x);",
            },
            Migration {
                version: 2,
                description: "b",
                sql: "CREATE TABLE b(y);",
            },
        ];
        assert_eq!(migrate_with(&mut c, &v2).unwrap(), vec![2]);
        // An older build refuses a newer database instead of corrupting it.
        assert!(matches!(
            migrate_with(&mut c, &v1),
            Err(DbError::NewerSchema {
                found: 2,
                supported: 1
            })
        ));
        // A failing migration rolls back entirely.
        let bad = [
            Migration {
                version: 1,
                description: "a",
                sql: "",
            },
            Migration {
                version: 2,
                description: "b",
                sql: "",
            },
            Migration {
                version: 3,
                description: "c",
                sql: "CREATE TABLE c(z); SYNTAX ERROR;",
            },
        ];
        assert!(migrate_with(&mut c, &bad).is_err());
        assert_eq!(schema::current_version(&c).unwrap(), 2);
        let has_c: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'c'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_c, 0);
    }

    #[test]
    fn corrupt_file_is_moved_aside_and_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("forge.db");
        std::fs::write(&p, b"this is definitely not a sqlite database ".repeat(200)).unwrap();
        std::fs::write(dir.path().join("forge.db-wal"), b"junk").unwrap();
        let db = Db::open(&p).unwrap();
        let aside = db
            .recovered_corrupt_file()
            .expect("recovered")
            .to_path_buf();
        assert!(
            aside
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("forge.db.corrupt-")
        );
        assert!(aside.exists());
        assert_eq!(db.schema_version().unwrap(), latest_version());
        assert!(db.stats().unwrap().files == 0);
        drop(db);
        // Re-opening the fresh database is clean.
        let db = Db::open(&p).unwrap();
        assert!(db.recovered_corrupt_file().is_none());
    }

    #[test]
    fn damaged_header_of_real_database_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("forge.db");
        {
            let db = Db::open(&p).unwrap();
            db.set_setting("k", "v").unwrap();
            db.optimize().unwrap();
        }
        let mut bytes = std::fs::read(&p).unwrap();
        for b in bytes.iter_mut().take(100) {
            *b = 0xAB;
        }
        std::fs::write(&p, bytes).unwrap();
        let db = Db::open(&p).unwrap();
        assert!(db.recovered_corrupt_file().is_some());
        assert_eq!(db.setting("k").unwrap(), None);
    }

    #[test]
    fn db_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Db>();
    }
}
