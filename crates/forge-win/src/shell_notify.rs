//! Tells Explorer (and the Desktop) about file system changes this app made, the way
//! `IFileOperation` does, so their windows update at once instead of whenever their own
//! file system watchers get round to it.

use std::path::Path;

/// `from` was renamed or moved to `to` (`SHCNE_RENAMEITEM` / `SHCNE_RENAMEFOLDER`).
pub fn notify_renamed(_from: &Path, _to: &Path, _is_dir: bool) {} // STUB
/// `path` was created (`SHCNE_CREATE` / `SHCNE_MKDIR`).
pub fn notify_created(_path: &Path, _is_dir: bool) {} // STUB
/// `path` was deleted (`SHCNE_DELETE` / `SHCNE_RMDIR`).
pub fn notify_deleted(_path: &Path, _is_dir: bool) {} // STUB
/// The contents of folder `dir` changed (`SHCNE_UPDATEDIR`).
pub fn notify_dir_updated(_dir: &Path) {} // STUB
