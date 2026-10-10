//! Small helpers shared by the subcommands: exit codes, size parsing, path handling.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use forge_jobs::CancelToken;

/// Everything succeeded.
pub const EXIT_OK: u8 = 0;
/// The command ran but found problems (verification failures, refused changes, ...).
pub const EXIT_PROBLEMS: u8 = 1;
/// Usage error or I/O failure (reported through `anyhow`, mapped in `main`).
pub const EXIT_USAGE: u8 = 2;

/// Parse a size such as `1mb`, `512k` or `4096` (binary units; see `forge_core::query`).
pub fn parse_size(s: &str) -> Result<u64> {
    forge_core::query::parse_size(s)
        .with_context(|| format!("invalid size {s:?} (try 1024, 512k, 1mb, 2gb)"))
}

/// Absolute form of a path without resolving links (the path must exist to be meaningful).
pub fn absolute(p: &Path) -> Result<PathBuf> {
    std::path::absolute(p).with_context(|| format!("cannot resolve {}", p.display()))
}

/// Fail with a usage error unless `p` exists.
pub fn require_exists(p: &Path) -> Result<()> {
    if std::fs::symlink_metadata(p).is_err() {
        bail!("{}: no such file or folder", p.display());
    }
    Ok(())
}

/// Install a Ctrl+C handler that cancels `token`. Only the first call in a process takes
/// effect; later calls are ignored.
pub fn install_ctrl_c(token: &CancelToken) {
    let t = token.clone();
    let _ = ctrlc::set_handler(move || {
        eprintln!("\ncancelling (progress so far is kept)...");
        t.cancel();
    });
}

/// Print problems to stderr, capped so a huge tree does not flood the terminal.
pub fn print_capped(lines: impl IntoIterator<Item = String>, limit: usize) -> usize {
    let mut n = 0;
    for line in lines {
        n += 1;
        if n <= limit {
            eprintln!("  {line}");
        }
    }
    if n > limit {
        eprintln!("  ... and {} more", n - limit);
    }
    n
}
