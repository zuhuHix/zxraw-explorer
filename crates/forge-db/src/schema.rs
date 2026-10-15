//! Versioned, forward-only schema migrations.
//!
//! Each migration runs in its own transaction together with the row recording it in
//! `schema_migrations`, so a crash mid-migration leaves the previous version intact.
//! Never edit a released migration; append a new one.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{DbError, Result};
use crate::now_ms;

pub struct Migration {
    pub version: i64,
    pub description: &'static str,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "initial schema",
        sql: V1,
    },
    Migration {
        version: 2,
        description: "notes: clear blank notes, index files with notes",
        sql: V2,
    },
    Migration {
        version: 3,
        description: "index clock: monotonic pass ids and indexed_at stamps",
        sql: V3,
    },
];

/// Highest schema version this build understands.
pub fn latest_version() -> i64 {
    MIGRATIONS.last().map_or(0, |m| m.version)
}

const V1: &str = r#"
CREATE TABLE volumes (
    id          INTEGER PRIMARY KEY,
    serial      TEXT UNIQUE,              -- volume serial / GUID when known
    label       TEXT,
    root        TEXT NOT NULL,            -- last known mount root, e.g. 'E:\'
    last_seen   INTEGER                   -- unix ms
);
CREATE INDEX idx_volumes_root ON volumes(root);

-- Files and folders. Rows are never deleted when a file disappears: `present` is cleared
-- instead, so ratings/tags/collections survive offline drives and can be relinked.
CREATE TABLE files (
    id            INTEGER PRIMARY KEY,
    volume_id     INTEGER NOT NULL REFERENCES volumes(id),
    path          TEXT NOT NULL COLLATE NOCASE UNIQUE,
    path_lc       TEXT NOT NULL,          -- Unicode-lowercased path for substring search
    parent_path   TEXT NOT NULL COLLATE NOCASE,
    name          TEXT NOT NULL,
    name_lc       TEXT NOT NULL,
    ext           TEXT NOT NULL DEFAULT '',
    size          INTEGER NOT NULL DEFAULT 0,
    mtime         INTEGER,                -- unix ms
    ctime         INTEGER,                -- unix ms
    kind          TEXT NOT NULL,          -- file | dir | link
    media_kind    TEXT NOT NULL,          -- image | raw | video | audio | ... | other
    indexed_at    INTEGER NOT NULL,       -- unix ms of last upsert
    present       INTEGER NOT NULL DEFAULT 1,
    seen_scan     INTEGER,                -- id of the last indexer pass that saw this row
    partial_hash  TEXT,                   -- quick prefilter hash (hex)
    full_hash     TEXT                    -- BLAKE3 of full contents (hex)
);
CREATE INDEX idx_files_name   ON files(name_lc);
CREATE INDEX idx_files_ext    ON files(ext);
CREATE INDEX idx_files_parent ON files(parent_path);
CREATE INDEX idx_files_size   ON files(size);
CREATE INDEX idx_files_mtime  ON files(mtime);
CREATE INDEX idx_files_volume ON files(volume_id);
CREATE INDEX idx_files_full_hash ON files(full_hash) WHERE full_hash IS NOT NULL;

CREATE TABLE image_meta (
    file_id        INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    width          INTEGER,
    height         INTEGER,
    camera_make    TEXT,
    camera_model   TEXT,
    lens           TEXT,
    focal_length   REAL,                  -- mm
    aperture       REAL,                  -- f-number
    shutter        REAL,                  -- seconds
    iso            INTEGER,
    exposure_comp  REAL,                  -- EV
    taken_at       INTEGER,               -- unix ms
    gps_lat        REAL,
    gps_lon        REAL,
    orientation    INTEGER,
    color_space    TEXT,
    bit_depth      INTEGER,
    source_mtime   INTEGER,               -- files.mtime when extracted (staleness check)
    extracted_at   INTEGER NOT NULL
);
CREATE INDEX idx_image_meta_taken ON image_meta(taken_at);

CREATE TABLE video_meta (
    file_id        INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    duration       REAL,                  -- seconds
    width          INTEGER,
    height         INTEGER,
    fps            REAL,
    vfr            INTEGER NOT NULL DEFAULT 0,
    codec          TEXT,
    bit_depth      INTEGER,
    chroma         TEXT,                  -- e.g. 4:2:2
    bitrate        INTEGER,               -- bits/s
    audio_streams  INTEGER,
    hdr            TEXT,                  -- e.g. PQ, HLG; NULL = SDR/unknown
    rotation       INTEGER,
    timecode       TEXT,
    camera         TEXT,
    recorded_at    INTEGER,               -- unix ms
    source_mtime   INTEGER,
    extracted_at   INTEGER NOT NULL
);
CREATE INDEX idx_video_meta_recorded ON video_meta(recorded_at);

CREATE TABLE annotations (
    file_id     INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    rating      INTEGER NOT NULL DEFAULT 0 CHECK (rating BETWEEN 0 AND 5),
    label       TEXT CHECK (label IN ('red','yellow','green','blue','purple')),
    pick        INTEGER NOT NULL DEFAULT 0 CHECK (pick IN (-1, 0, 1)),
    notes       TEXT,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX idx_annotations_rating ON annotations(rating);

CREATE TABLE tags (
    id       INTEGER PRIMARY KEY,
    name     TEXT NOT NULL,
    name_lc  TEXT NOT NULL UNIQUE
);
CREATE TABLE file_tags (
    file_id  INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    tag_id   INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (file_id, tag_id)
) WITHOUT ROWID;
CREATE INDEX idx_file_tags_tag ON file_tags(tag_id);

CREATE TABLE collections (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    name_lc     TEXT NOT NULL UNIQUE,
    kind        TEXT NOT NULL CHECK (kind IN ('manual', 'smart')),
    query       TEXT,                     -- smart collections: query string
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE TABLE collection_items (
    collection_id  INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    file_id        INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    position       INTEGER NOT NULL,
    PRIMARY KEY (collection_id, file_id)
) WITHOUT ROWID;
CREATE INDEX idx_collection_items_pos  ON collection_items(collection_id, position);
CREATE INDEX idx_collection_items_file ON collection_items(file_id);

CREATE TABLE saved_searches (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    name_lc     TEXT NOT NULL UNIQUE,
    query       TEXT NOT NULL,
    sort        TEXT,                     -- JSON-encoded SearchOptions sort, optional
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE thumbs (
    key          TEXT PRIMARY KEY,
    path         TEXT NOT NULL,           -- source file
    size         INTEGER NOT NULL,        -- source size
    mtime        INTEGER,                 -- source mtime (unix ms)
    width        INTEGER NOT NULL,
    height       INTEGER NOT NULL,
    last_access  INTEGER NOT NULL,
    bytes        INTEGER NOT NULL         -- size of the cached thumbnail on disk
);
CREATE INDEX idx_thumbs_access ON thumbs(last_access);
CREATE INDEX idx_thumbs_path ON thumbs(path);

CREATE TABLE index_roots (
    id                INTEGER PRIMARY KEY,
    path              TEXT NOT NULL COLLATE NOCASE UNIQUE,
    enabled           INTEGER NOT NULL DEFAULT 1,
    added_at          INTEGER NOT NULL,
    last_complete_at  INTEGER
);
CREATE TABLE index_exclusions (
    id     INTEGER PRIMARY KEY,
    kind   TEXT NOT NULL CHECK (kind IN ('folder', 'ext', 'name')),
    value  TEXT NOT NULL,                 -- lowercased
    UNIQUE (kind, value)
);
-- Resumable indexing checkpoint, one row per root. `pending` (JSON array of folders still
-- to list) is written in the same transaction as each batch of file rows.
CREATE TABLE index_state (
    root_id      INTEGER PRIMARY KEY REFERENCES index_roots(id) ON DELETE CASCADE,
    scan_id      INTEGER NOT NULL,
    status       TEXT NOT NULL CHECK (status IN ('running', 'interrupted', 'complete', 'unavailable')),
    started_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    finished_at  INTEGER,
    pending      TEXT NOT NULL DEFAULT '[]',
    items_seen   INTEGER NOT NULL DEFAULT 0,
    errors       INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT
);

CREATE TABLE settings (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);
"#;

const V2: &str = r#"
-- Blank notes were once stored as text; treat them as no note.
UPDATE annotations SET notes = NULL WHERE notes IS NOT NULL AND trim(notes) = '';
CREATE INDEX IF NOT EXISTS idx_annotations_notes ON annotations(file_id) WHERE notes IS NOT NULL;
"#;

const V3: &str = r#"
-- Monotonic "index clock" (unix ms, never decreases): new indexer pass ids are taken
-- strictly above it and every indexed_at stamp is at least it, so a pass's end sweep can
-- tell earlier rows from rows refreshed during the pass even when passes start in the same
-- millisecond or the wall clock is set back. Seeded from existing rows and pass ids.
CREATE TABLE index_clock (
    id     INTEGER PRIMARY KEY CHECK (id = 1),
    value  INTEGER NOT NULL
);
INSERT INTO index_clock(id, value)
SELECT 1, max(COALESCE((SELECT MAX(indexed_at) FROM files), 0),
              COALESCE((SELECT MAX(scan_id) FROM index_state), 0));
-- Checkpoints from older builds may carry a pass id equal to earlier rows' indexed_at,
-- which would let rows escape the end sweep; restart those passes from the top.
UPDATE index_state SET pending = '[]' WHERE status <> 'complete';
"#;

/// Apply all pending migrations. Returns the versions applied by this call.
pub(crate) fn migrate(conn: &mut Connection) -> Result<Vec<i64>> {
    migrate_with(conn, MIGRATIONS)
}

pub(crate) fn migrate_with(conn: &mut Connection, migrations: &[Migration]) -> Result<Vec<i64>> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version     INTEGER PRIMARY KEY,
            description TEXT NOT NULL,
            applied_at  INTEGER NOT NULL
        );",
    )?;
    let current = current_version(conn)?;
    let supported = migrations.last().map_or(0, |m| m.version);
    if current > supported {
        return Err(DbError::NewerSchema {
            found: current,
            supported,
        });
    }
    let mut applied = Vec::new();
    for m in migrations.iter().filter(|m| m.version > current) {
        let tx = conn.transaction()?;
        tx.execute_batch(m.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations(version, description, applied_at) VALUES (?1, ?2, ?3)",
            params![m.version, m.description, now_ms()],
        )?;
        tx.commit()?;
        tracing::info!(
            version = m.version,
            "applied database migration: {}",
            m.description
        );
        applied.push(m.version);
    }
    Ok(applied)
}

pub(crate) fn current_version(conn: &Connection) -> Result<i64> {
    Ok(conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten()
        .unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_clears_blank_notes_and_indexes_notes_on_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forge.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            migrate_with(&mut conn, &MIGRATIONS[..1]).unwrap();
            conn.execute_batch(
                r"INSERT INTO volumes(id, root, last_seen) VALUES (1, 'D:\', 0);
                 INSERT INTO files(id, volume_id, path, path_lc, parent_path, name, name_lc,
                                   kind, media_kind, indexed_at)
                 VALUES (1, 1, 'D:\a.jpg', 'd:\a.jpg', 'D:\', 'a.jpg', 'a.jpg', 'file', 'image', 0),
                        (2, 1, 'D:\b.jpg', 'd:\b.jpg', 'D:\', 'b.jpg', 'b.jpg', 'file', 'image', 0);
                 INSERT INTO annotations(file_id, notes, updated_at)
                 VALUES (1, 'keep me', 0), (2, '   ', 0);",
            )
            .unwrap();
            assert_eq!(current_version(&conn).unwrap(), 1);
        }

        let db = crate::Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), latest_version());
        assert_eq!(db.annotation(1).unwrap().notes.as_deref(), Some("keep me"));
        assert_eq!(db.annotation(2).unwrap().notes, None);
        let has_index: i64 = db
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'index' AND name = 'idx_annotations_notes'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(has_index, 1);
        drop(db);

        // Reopening applies nothing new.
        let db = crate::Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), latest_version());
        let mut conn = Connection::open(&path).unwrap();
        assert!(migrate_with(&mut conn, MIGRATIONS).unwrap().is_empty());
        assert_eq!(db.annotation(1).unwrap().notes.as_deref(), Some("keep me"));
    }
}
