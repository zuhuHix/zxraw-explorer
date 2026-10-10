use std::path::PathBuf;

use forge_core::query::ParseError;

/// Errors from the persistence layer.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid search query: {0}")]
    Query(#[from] ParseError),
    #[error(
        "database schema version {found} is newer than this build of ZXraw Explorer supports ({supported})"
    )]
    NewerSchema { found: i64, supported: i64 },
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("operation cancelled")]
    Cancelled,
}

pub type Result<T, E = DbError> = std::result::Result<T, E>;

impl DbError {
    /// True if SQLite reported that the file is corrupt or not a database at all.
    pub fn is_corruption(&self) -> bool {
        matches!(
            self,
            DbError::Sqlite(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                )
        )
    }

    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        DbError::Io {
            path: path.into(),
            source,
        }
    }
}

impl From<forge_jobs::Cancelled> for DbError {
    fn from(_: forge_jobs::Cancelled) -> Self {
        DbError::Cancelled
    }
}
