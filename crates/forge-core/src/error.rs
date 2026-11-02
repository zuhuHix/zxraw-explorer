use std::path::PathBuf;

/// Typed errors for the filesystem engine. Variants carry the path involved so the UI can
/// show actionable diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}: access denied")]
    AccessDenied(PathBuf),
    #[error("{0}: not found")]
    NotFound(PathBuf),
    #[error("{0}: already exists")]
    AlreadyExists(PathBuf),
    #[error("unsafe path rejected: {0}")]
    UnsafePath(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("verification failed for {path}: {detail}")]
    VerifyFailed { path: PathBuf, detail: String },
    #[error("operation cancelled")]
    Cancelled,
    /// An archive entry is encrypted and no password was given.
    #[error("{0}: a password is needed")]
    PasswordRequired(String),
    /// The password given for an encrypted archive entry is not the right one.
    #[error("{0}: wrong password")]
    WrongPassword(String),
    /// Something this version can't do with the input; the message is for the user as is.
    #[error("{0}")]
    Unsupported(String),
}

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

impl CoreError {
    /// Classify an `io::Error` for a path into the most useful variant.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        let path = path.into();
        match source.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound(path),
            std::io::ErrorKind::PermissionDenied => Self::AccessDenied(path),
            std::io::ErrorKind::AlreadyExists => Self::AlreadyExists(path),
            _ => Self::Io { path, source },
        }
    }
}

impl From<forge_jobs::Cancelled> for CoreError {
    fn from(_: forge_jobs::Cancelled) -> Self {
        Self::Cancelled
    }
}
