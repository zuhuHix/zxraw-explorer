use std::path::PathBuf;

/// Typed errors for the media pipeline. Every parser in this crate treats its input as
/// untrusted: malformed data yields one of these, never a panic.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}: not found")]
    NotFound(PathBuf),
    /// The format is recognised but not handled (e.g. HEIC without WIC, RAW without preview).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The data is malformed or truncated.
    #[error("corrupt or truncated data: {0}")]
    Corrupt(String),
    /// Image decoder error (from the `image` crate or our own JPEG validation).
    #[error("decode failed: {0}")]
    Decode(String),
    /// Declared dimensions exceed the configured pixel budget (decompression-bomb guard).
    #[error("image too large: {width}x{height} exceeds limit of {limit} pixels")]
    TooLarge { width: u32, height: u32, limit: u64 },
    /// No metadata of the requested kind was present.
    #[error("no {0} found")]
    Missing(&'static str),
    /// `ffprobe` / `ffmpeg` is not installed or not configured.
    #[error("{0} is not installed (install FFmpeg or configure its path)")]
    ToolMissing(&'static str),
    #[error("{tool} failed ({status}): {stderr}")]
    ToolFailed {
        tool: &'static str,
        status: String,
        stderr: String,
    },
    #[error("{tool} timed out after {secs:.1}s")]
    Timeout { tool: &'static str, secs: f64 },
    #[error("{what} output exceeded {limit} bytes")]
    OutputTooLarge { what: &'static str, limit: usize },
    /// Background work refused to read a cloud placeholder whose data is not on this PC.
    #[error("online-only file; not downloaded")]
    OnlineOnly,
    #[error("operation cancelled")]
    Cancelled,
}

pub type Result<T, E = MediaError> = std::result::Result<T, E>;

impl MediaError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        let path = path.into();
        match source.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound(path),
            std::io::ErrorKind::UnexpectedEof => {
                Self::Corrupt(format!("{}: unexpected end of file", path.display()))
            }
            _ => Self::Io { path, source },
        }
    }

    pub(crate) fn corrupt(msg: impl Into<String>) -> Self {
        Self::Corrupt(msg.into())
    }

    /// True for errors that indicate the external FFmpeg tools are unavailable.
    pub fn is_tool_missing(&self) -> bool {
        matches!(self, Self::ToolMissing(_))
    }
}

impl From<image::ImageError> for MediaError {
    fn from(e: image::ImageError) -> Self {
        use image::ImageError as E;
        match e {
            E::Limits(l) => Self::Decode(format!("limits exceeded: {l}")),
            E::Unsupported(u) => Self::Unsupported(u.to_string()),
            E::IoError(io) => Self::Decode(io.to_string()),
            other => Self::Decode(other.to_string()),
        }
    }
}

impl From<forge_jobs::Cancelled> for MediaError {
    fn from(_: forge_jobs::Cancelled) -> Self {
        Self::Cancelled
    }
}
