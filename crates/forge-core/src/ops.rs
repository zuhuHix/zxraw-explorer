//! File operations engine: copy / move / delete / create / rename.
//!
//! Safety properties (see docs/ARCHITECTURE.md "Safety invariants"):
//! * Copies stream into `.forge-partial-<random>` in the destination directory and are
//!   renamed into place only when complete (and verified, if requested). Interrupted or
//!   failed copies never leave a truncated file under the final name; the partial file is
//!   removed on error and on cancel.
//! * Recursive scans never follow symlinks/junctions. Links are recreated as links where
//!   the OS allows it, otherwise skipped with a logged reason.
//! * Overwrites happen only through an explicit [`ConflictPolicy`] or a per-file answer from
//!   the [`ConflictResolver`].
//! * Moves try a cheap `rename` first; across volumes they copy, *always* verify, and delete
//!   the source file only after the verified copy is in place.
//! * Nothing here panics on I/O errors: failures are recorded per item in [`OpReport`] and
//!   the operation continues (or stops early for a root whose source disappeared).

use std::cell::Cell;
#[cfg(test)]
use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use forge_jobs::{CancelToken, JobHandle, Progress};
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::fault;
use crate::hashing::{self, Digest};
use crate::paths;
use crate::stamp::{Stamp, handle_stamp, path_stamp};

/// Copy buffer size. 2 MiB keeps large sequential transfers efficient while still checking
/// cancellation/pause several times per second even on slow USB media.
pub const COPY_CHUNK: usize = 2 * 1024 * 1024;
/// Prefix of in-flight temp files. UIs may hide entries starting with this.
pub const PARTIAL_PREFIX: &str = ".forge-partial-";

// ---------------------------------------------------------------------------
// Job context
// ---------------------------------------------------------------------------

/// What long-running engine functions need from a job: cancellation/pause, progress and a
/// report log. Build it from a registered [`JobHandle`] or from loose parts (tests, CLI).
#[derive(Clone, Copy)]
pub struct JobCtx<'a> {
    pub cancel: &'a CancelToken,
    pub progress: &'a Progress,
    job: Option<&'a JobHandle>,
}

impl<'a> JobCtx<'a> {
    pub fn new(cancel: &'a CancelToken, progress: &'a Progress) -> Self {
        Self {
            cancel,
            progress,
            job: None,
        }
    }

    pub fn from_job(job: &'a JobHandle) -> Self {
        Self {
            cancel: &job.cancel,
            progress: job.progress.as_ref(),
            job: Some(job),
        }
    }

    /// Append a line to the job report (and the debug log).
    pub fn log(&self, line: impl Into<String>) {
        let line = line.into();
        tracing::debug!("{line}");
        if let Some(j) = self.job {
            j.log(line);
        }
    }
}

// ---------------------------------------------------------------------------
// Tree walking (shared by hashing, duplicates, disk usage, compare, ingest)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeKind {
    File,
    Dir,
    /// Symlink or junction (name-surrogate reparse point). Never followed.
    Link,
}

/// Classify `symlink_metadata` output. Only *name-surrogate* reparse points (symlinks,
/// junctions) count as links; cloud placeholders (OneDrive) and dedup files are regular
/// files and are copied normally.
pub fn node_kind(md: &fs::Metadata) -> NodeKind {
    let ft = md.file_type();
    if ft.is_symlink() {
        NodeKind::Link
    } else if ft.is_dir() {
        NodeKind::Dir
    } else {
        NodeKind::File
    }
}

#[derive(Debug, Clone)]
pub struct WalkEntry {
    pub path: PathBuf,
    /// Path relative to the walk root.
    pub rel: PathBuf,
    /// 1 for direct children of the root.
    pub depth: usize,
    pub kind: NodeKind,
    /// File size (0 for dirs and links).
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub readonly: bool,
}

impl WalkEntry {
    pub fn is_file(&self) -> bool {
        self.kind == NodeKind::File
    }
    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }
    pub fn is_link(&self) -> bool {
        self.kind == NodeKind::Link
    }
}

#[derive(Debug)]
pub enum WalkItem {
    Entry(WalkEntry),
    /// A child (or a directory listing) could not be read; the walk continues.
    Error {
        path: PathBuf,
        error: CoreError,
    },
}

fn walk_entry(path: PathBuf, rel: PathBuf, depth: usize, md: &fs::Metadata) -> WalkEntry {
    let kind = node_kind(md);
    WalkEntry {
        path,
        rel,
        depth,
        kind,
        size: if kind == NodeKind::File { md.len() } else { 0 },
        modified: md.modified().ok(),
        readonly: md.permissions().readonly(),
    }
}

/// Depth-first, pre-order walk of everything below directory `root` (the root itself is
/// not reported). Children are visited in name order. Links are reported but never
/// descended into. Unreadable entries produce [`WalkItem::Error`] and the walk continues;
/// only an unreadable `root` or cancellation return `Err`.
pub fn walk_tree(
    root: &Path,
    cancel: &CancelToken,
    mut visit: impl FnMut(&WalkItem),
) -> Result<()> {
    let mut stack = Vec::new();
    push_children(root, Path::new(""), 1, &mut stack, &mut visit)
        .map_err(|e| CoreError::io(root, e))?;
    let mut n: u64 = 0;
    while let Some(e) = stack.pop() {
        n += 1;
        if n.is_multiple_of(64) {
            cancel.check()?;
        }
        // Cloud placeholder folders are listed too: that fetches names and sizes, not contents.
        let descend = e.is_dir().then(|| (e.path.clone(), e.rel.clone(), e.depth));
        visit(&WalkItem::Entry(e));
        if let Some((path, rel, depth)) = descend
            && let Err(err) = push_children(&path, &rel, depth + 1, &mut stack, &mut visit)
        {
            let error = CoreError::io(&path, err);
            visit(&WalkItem::Error { path, error });
        }
    }
    cancel.check()?;
    Ok(())
}

fn push_children(
    dir: &Path,
    rel: &Path,
    depth: usize,
    stack: &mut Vec<WalkEntry>,
    visit: &mut impl FnMut(&WalkItem),
) -> io::Result<()> {
    let mut v = Vec::new();
    for item in fs::read_dir(dir)? {
        match item {
            Ok(de) => {
                let path = de.path();
                // DirEntry::metadata does not traverse links (lstat / FindFirstFile data).
                match de.metadata() {
                    Ok(md) => v.push(walk_entry(path, rel.join(de.file_name()), depth, &md)),
                    Err(e) => {
                        let error = CoreError::io(&path, e);
                        visit(&WalkItem::Error { path, error });
                    }
                }
            }
            Err(e) => visit(&WalkItem::Error {
                path: dir.to_path_buf(),
                error: CoreError::io(dir, e),
            }),
        }
    }
    // Reverse order so popping yields ascending names.
    v.sort_by(|a, b| b.rel.cmp(&a.rel));
    stack.extend(v);
    Ok(())
}

// ---------------------------------------------------------------------------
// Small fs helpers
// ---------------------------------------------------------------------------

static NAME_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 16 hex chars, unique per process call and very likely unique across processes.
pub(crate) fn random_hex() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = NAME_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut seed = Vec::with_capacity(32);
    seed.extend_from_slice(&nanos.to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    seed.extend_from_slice(&n.to_le_bytes());
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(&seed))
}

/// A fresh temp file name: `.forge-partial-<random>`.
pub fn partial_name() -> String {
    format!("{PARTIAL_PREFIX}{}", random_hex())
}

/// Is `name` one of our in-flight temp files?
pub fn is_partial_name(name: &str) -> bool {
    name.starts_with(PARTIAL_PREFIX)
}

#[cfg(windows)]
fn win_attrs(md: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes()
}

/// Does this link point at a directory (directory symlink / junction)? Needed to remove or
/// recreate it correctly on Windows. Never follows the link.
fn link_is_dir(md: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
        win_attrs(md) & FILE_ATTRIBUTE_DIRECTORY != 0
    }
    #[cfg(not(windows))]
    {
        let _ = md;
        false
    }
}

/// Make a file writable so it can be replaced/deleted.
fn clear_readonly(path: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(path)?;
    let mut perms = md.permissions();
    if !perms.readonly() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(perms.mode() | 0o200);
    }
    #[cfg(not(unix))]
    {
        // On Windows this only clears FILE_ATTRIBUTE_READONLY.
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
    }
    fs::set_permissions(path, perms)
}

fn set_readonly(path: &Path) -> io::Result<()> {
    let mut perms = fs::symlink_metadata(path)?.permissions();
    perms.set_readonly(true);
    fs::set_permissions(path, perms)
}

fn remove_file_force(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            clear_readonly(path)?;
            fs::remove_file(path)
        }
        r => r,
    }
}

/// Remove a symlink/junction itself, never its target.
fn remove_link(path: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(path)?;
    if link_is_dir(&md) {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// Recreate the link at `src` as a new link at `dest` (same target text).
fn copy_link(src: &Path, dest: &Path) -> io::Result<()> {
    let target = fs::read_link(src)?;
    #[cfg(windows)]
    {
        let md = fs::symlink_metadata(src)?;
        if link_is_dir(&md) {
            std::os::windows::fs::symlink_dir(&target, dest)
        } else {
            std::os::windows::fs::symlink_file(&target, dest)
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, dest)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (target, dest);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "links not supported",
        ))
    }
}

/// Removes the temp file on drop unless disarmed.
pub(crate) struct TempGuard(pub(crate) Option<PathBuf>);

impl TempGuard {
    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take()
            && let Err(e) = fs::remove_file(&p)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!("could not remove partial file {}: {e}", p.display());
        }
    }
}

// ---------------------------------------------------------------------------
// Single-file atomic copy
// ---------------------------------------------------------------------------

#[cfg(test)]
type ChunkHook = Box<dyn Fn(&Path, u64)>;

#[cfg(test)]
thread_local! {
    /// Test hook invoked after every chunk written: (source label, bytes written so far).
    pub(crate) static AFTER_CHUNK: RefCell<Option<ChunkHook>> =
        RefCell::new(None);
}

#[cfg(test)]
type BeforeMoveRenameHook = Box<dyn Fn(&Path)>;

#[cfg(test)]
thread_local! {
    /// Test hook after a Replace choice but before the same-volume move rename.
    static BEFORE_MOVE_RENAME: RefCell<Option<BeforeMoveRenameHook>> = RefCell::new(None);
}

#[cfg(test)]
thread_local! {
    /// Test hook after the Replace destination was verified, before it is moved aside (called
    /// with the destination).
    static BEFORE_REPLACE_ASIDE: RefCell<Option<BeforeMoveRenameHook>> = RefCell::new(None);
}

#[cfg(test)]
thread_local! {
    /// Test hook after the Replace destination was moved aside (called with the destination).
    static AFTER_REPLACE_ASIDE: RefCell<Option<BeforeMoveRenameHook>> = RefCell::new(None);
}

#[cfg(test)]
thread_local! {
    /// Test hook after the new file is in place, before the previous version is deleted (called
    /// with the previous version's aside path).
    static BEFORE_ASIDE_DELETE: RefCell<Option<BeforeMoveRenameHook>> = RefCell::new(None);
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CopyFileOptions {
    /// Replace an existing destination file. Without this the copy fails with
    /// `AlreadyExists` rather than clobbering anything (no-replace placement).
    pub overwrite: bool,
    /// Re-read the written data from disk and compare its BLAKE3 with the hash of the bytes
    /// read from the source. On mismatch nothing is placed under the final name.
    pub verify: bool,
    /// Return the BLAKE3 of the source data even when not verifying.
    pub want_hash: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyOutcome {
    pub bytes: u64,
    /// BLAKE3 of the source bytes as read (present if `verify` or `want_hash`).
    pub source_hash: Option<Digest>,
}

/// Options for [`write_stream_atomic`].
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamOptions {
    pub overwrite: bool,
    pub verify: bool,
    pub want_hash: bool,
    pub modified: Option<SystemTime>,
    pub readonly: bool,
    /// Abort with `Invalid` if the stream yields more than this many bytes.
    pub limit: Option<u64>,
    /// Windows file attributes to apply at creation (hidden/system/archive…); readonly is
    /// applied after placement instead.
    pub attributes: u32,
}

/// Copy one regular file to `dest` atomically (temp file + rename), preserving the modified
/// time, the readonly flag and (on Windows) hidden/system attributes.
pub fn copy_file_atomic(
    src: &Path,
    dest: &Path,
    opts: CopyFileOptions,
    ctx: &JobCtx<'_>,
) -> Result<CopyOutcome> {
    let mut counted = 0;
    let mut dest = dest.to_path_buf();
    copy_file_counted(
        src,
        &mut dest,
        opts,
        ctx,
        &mut counted,
        Publish {
            keep_both: false,
            still_ok: None,
            still_ok_message: None,
            replace: None,
        },
    )
}

/// [`copy_file_atomic`] that asks `still_ok` right before the finished, verified copy is
/// published, and abandons the copy (removing its temp file) if it returns false. Sync
/// uses it so a destination edited while a long copy runs is not overwritten; only the
/// instant between this check and the rename remains.
pub fn copy_file_atomic_guarded(
    src: &Path,
    dest: &Path,
    opts: CopyFileOptions,
    ctx: &JobCtx<'_>,
    still_ok: &dyn Fn() -> bool,
) -> Result<CopyOutcome> {
    let mut counted = 0;
    let mut dest = dest.to_path_buf();
    copy_file_counted(
        src,
        &mut dest,
        opts,
        ctx,
        &mut counted,
        Publish {
            keep_both: false,
            still_ok: Some(still_ok),
            still_ok_message: Some("destination changed since preview; it was not overwritten"),
            replace: None,
        },
    )
}

fn copy_file_counted(
    src: &Path,
    dest: &mut PathBuf,
    opts: CopyFileOptions,
    ctx: &JobCtx<'_>,
    counted: &mut u64,
    publish: Publish<'_>,
) -> Result<CopyOutcome> {
    let f = File::open(src).map_err(|e| CoreError::io(src, e))?;
    let md = f.metadata().map_err(|e| CoreError::io(src, e))?;
    if md.is_dir() {
        return Err(CoreError::Invalid(format!("{} is a folder", src.display())));
    }
    #[cfg(windows)]
    let attributes = win_attrs(&md) & (0x2 | 0x4 | 0x20 | 0x2000);
    #[cfg(not(windows))]
    let attributes = 0;
    let sopts = StreamOptions {
        overwrite: opts.overwrite,
        verify: opts.verify,
        want_hash: opts.want_hash,
        modified: md.modified().ok(),
        readonly: md.permissions().readonly(),
        limit: None,
        attributes,
    };
    stream_into_place(
        fault::reader(f, src),
        src,
        dest,
        sopts,
        ctx,
        counted,
        publish,
    )
}

/// Write an arbitrary stream (e.g. an archive entry) to `dest` with the same temp-file +
/// rename discipline as [`copy_file_atomic`].
pub fn write_stream_atomic<R: Read>(
    reader: R,
    label: &Path,
    dest: &Path,
    opts: StreamOptions,
    ctx: &JobCtx<'_>,
) -> Result<CopyOutcome> {
    write_stream_placed(reader, label, dest, opts, false, ctx).map(|(out, _)| out)
}

/// Like [`write_stream_atomic`], but when `keep_both` is set and `dest` is taken at publish
/// time, the next free name is used. Returns the final path actually written.
pub(crate) fn write_stream_placed<R: Read>(
    reader: R,
    label: &Path,
    dest: &Path,
    opts: StreamOptions,
    keep_both: bool,
    ctx: &JobCtx<'_>,
) -> Result<(CopyOutcome, PathBuf)> {
    let mut counted = 0;
    let mut placed = dest.to_path_buf();
    let publish = Publish {
        keep_both,
        still_ok: None,
        still_ok_message: None,
        replace: None,
    };
    let out = stream_into_place(reader, label, &mut placed, opts, ctx, &mut counted, publish)?;
    Ok((out, placed))
}

/// How a finished temp file is published: under the next free name when the destination
/// is taken (`keep_both`), and only if `still_ok` (when given) agrees at that moment.
struct Publish<'a> {
    keep_both: bool,
    still_ok: Option<&'a dyn Fn() -> bool>,
    still_ok_message: Option<&'a str>,
    /// Replace the destination captured in this state (see [`replace_guarded`]) instead of
    /// checking `still_ok` and renaming over it. The cell receives the previous version's
    /// path when it could not be deleted afterwards.
    replace: Option<(&'a ReplaceState, &'a Cell<Option<PathBuf>>)>,
}

fn stream_into_place<R: Read>(
    mut reader: R,
    label: &Path,
    dest: &mut PathBuf,
    opts: StreamOptions,
    ctx: &JobCtx<'_>,
    counted: &mut u64,
    publish: Publish<'_>,
) -> Result<CopyOutcome> {
    let Publish {
        keep_both,
        still_ok,
        still_ok_message,
        replace,
    } = publish;
    let dir = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| CoreError::Invalid(format!("{} has no parent folder", dest.display())))?;
    let tmp = dir.join(partial_name());
    let mut oo = OpenOptions::new();
    oo.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        if opts.attributes != 0 {
            oo.attributes(opts.attributes);
        }
    }
    let mut out = oo
        .open(&tmp)
        .map_err(|e| CoreError::io(dest.as_path(), e))?;
    let mut guard = TempGuard(Some(tmp.clone()));

    let mut hasher = (opts.verify || opts.want_hash).then(blake3::Hasher::new);
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut total: u64 = 0;
    loop {
        ctx.cancel.check()?;
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(CoreError::io(label, e)),
        };
        total += n as u64;
        if let Some(limit) = opts.limit
            && total > limit
        {
            return Err(CoreError::Invalid(format!(
                "{}: data exceeds the allowed size",
                label.display()
            )));
        }
        out.write_all(&buf[..n])
            .map_err(|e| CoreError::io(dest.as_path(), e))?;
        if let Some(h) = hasher.as_mut() {
            h.update(&buf[..n]);
        }
        ctx.progress.add_bytes(n as u64);
        *counted += n as u64;
        #[cfg(test)]
        AFTER_CHUNK.with(|h| {
            if let Some(f) = &*h.borrow() {
                f(label, total);
            }
        });
    }
    if let Some(m) = opts.modified
        && let Err(e) = out.set_modified(m)
    {
        ctx.log(format!(
            "{}: could not preserve modified time: {e}",
            dest.display()
        ));
    }
    out.sync_all()
        .map_err(|e| CoreError::io(dest.as_path(), e))?;
    drop(out);

    let source_hash = hasher.map(|h| *h.finalize().as_bytes());
    if opts.verify {
        let expected = source_hash.expect("hasher present when verifying");
        fault::corrupt_before_verify(label, &tmp).map_err(|e| CoreError::io(dest.as_path(), e))?;
        let actual = hashing::hash_file(&tmp, ctx.cancel, None)?;
        if actual != expected {
            return Err(CoreError::VerifyFailed {
                path: dest.to_path_buf(),
                detail: "written data does not match the source (BLAKE3 mismatch)".into(),
            });
        }
    }
    ctx.cancel.check()?;
    if let Some((state, leftover)) = replace {
        let kept = replace_guarded(&tmp, dest, state).map_err(|refused| {
            CoreError::Invalid(format!("{}: {}", dest.display(), refused.message()))
        })?;
        leftover.set(kept);
    } else {
        if let Some(still_ok) = still_ok
            && !still_ok()
        {
            let message = still_ok_message
                .unwrap_or("destination changed while it was being copied; it was not overwritten");
            return Err(CoreError::Invalid(format!("{}: {message}", dest.display())));
        }
        let mut placed = false;
        for _ in 0..1024 {
            match place(&tmp, dest, opts.overwrite) {
                Ok(()) => {
                    placed = true;
                    break;
                }
                Err(CoreError::AlreadyExists(_)) if keep_both && !opts.overwrite => {
                    let dir = dest.parent().unwrap_or(Path::new(""));
                    *dest = paths::unique_name(dir, &paths::name_of(dest));
                }
                Err(e) => return Err(e),
            }
        }
        if !placed {
            return Err(CoreError::AlreadyExists(dest.clone()));
        }
    }
    guard.disarm();
    if opts.readonly
        && let Err(e) = set_readonly(dest)
    {
        ctx.log(format!("{}: could not set read-only: {e}", dest.display()));
    }
    Ok(CopyOutcome {
        bytes: total,
        source_hash,
    })
}

/// Marker in the name a destination is moved aside to while a Replace is published. These hold
/// the user's data, so they are visible and never cleaned up automatically.
const REPLACED_MARKER: &str = ".zxraw-replaced-";

/// `name` with the aside marker and `hex` inserted before its extension, the stem truncated so
/// the result fits the 255 UTF-16 unit limit of a file name.
fn aside_name(name: &str, hex: &str) -> String {
    const MAX_UNITS: usize = 255;
    let units = |s: &str| s.encode_utf16().count();
    let suffix = format!("{REPLACED_MARKER}{hex}");
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && units(&name[i..]) <= 32 => name.split_at(i),
        _ => (name, ""),
    };
    let budget = MAX_UNITS - units(&suffix) - units(ext);
    let mut used = 0;
    let stem: String = stem
        .chars()
        .take_while(|c| {
            used += c.len_utf16();
            used <= budget
        })
        .collect();
    format!("{stem}{suffix}{ext}")
}

/// Win32 ERROR_SHARING_VIOLATION: someone has the file open in a conflicting way.
const ERROR_SHARING_VIOLATION: i32 = 32;

fn is_sharing_violation(e: &io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_SHARING_VIOLATION)
}

/// Open `path` itself (a link is not followed) for reading and deleting while denying writers:
/// this fails with a sharing violation if anyone has it open for writing, and while the handle is
/// held nobody can open it for writing. The DELETE access lets the object be renamed through this
/// handle (see [`rename_open_no_replace`]); other programs may still rename or delete it.
fn open_denying_writers(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
            FILE_SHARE_DELETE, FILE_SHARE_READ,
        };
        OpenOptions::new()
            .access_mode(FILE_GENERIC_READ.0 | DELETE.0)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_DELETE.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(path)
    }
    #[cfg(not(windows))]
    File::open(path)
}

/// Rename the object open as `file` (found at `from`) to `to`, never replacing. Renaming the
/// handle, not the path, moves exactly the file that was verified through it.
#[cfg(windows)]
fn rename_open_no_replace(file: &File, _from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FILE_RENAME_INFO, FileRenameInfo, SetFileInformationByHandle,
    };
    let name: Vec<u16> = paths::long_path_if_needed(to)
        .as_os_str()
        .encode_wide()
        .collect();
    let name_offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = name_offset + (name.len() + 1) * 2;
    // u64 words keep the buffer 8-byte aligned, as FILE_RENAME_INFO requires. Zeroed, so the
    // handle-relative RootDirectory is null and the NUL terminator is present.
    let mut buf = vec![0u64; size.div_ceil(8)];
    let base = buf.as_mut_ptr().cast::<u8>();
    // SAFETY: `buf` is zeroed and holds `size` bytes. The header fields are written through
    // raw pointers, and the name is copied to the byte offset of the FileName array, so no
    // reference to the one-element array is made and nothing is written past `size`.
    unsafe {
        let info = base.cast::<FILE_RENAME_INFO>();
        (&raw mut (*info).Anonymous.ReplaceIfExists).write(false);
        (&raw mut (*info).FileNameLength).write((name.len() * 2) as u32);
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            base.add(name_offset).cast::<u16>(),
            name.len(),
        );
    }
    // SAFETY: `file` is a live handle opened with DELETE access, and `buf` outlives the call
    // and holds the FILE_RENAME_INFO built above, `size` bytes long.
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileRenameInfo,
            base.cast(),
            size as u32,
        )
    }
    .map_err(|e| {
        // ERROR_ALREADY_EXISTS (183) / ERROR_FILE_EXISTS (80) -> AlreadyExists.
        let code = (e.code().0 as u32) & 0xFFFF;
        if code == 183 || code == 80 {
            io::Error::new(io::ErrorKind::AlreadyExists, "destination already exists")
        } else {
            io::Error::from_raw_os_error(code as i32)
        }
    })
}

#[cfg(not(windows))]
fn rename_open_no_replace(_file: &File, from: &Path, to: &Path) -> io::Result<()> {
    paths::rename_no_replace(from, to)
}

/// Why [`replace_guarded`] did not publish.
#[derive(Debug)]
enum Refusal {
    /// The destination changed after the Replace decision.
    Changed,
    /// Another program has the destination open for writing.
    InUse,
    /// Publishing failed for another reason.
    Failed(io::Error),
}

/// Why [`replace_guarded`] did not publish. The original destination is back in place unless
/// `kept_as` names where it had to be left.
#[derive(Debug)]
struct ReplaceRefused {
    reason: Refusal,
    kept_as: Option<PathBuf>,
}

impl ReplaceRefused {
    fn message(&self) -> String {
        let base = match &self.reason {
            Refusal::Changed => {
                "destination changed after you chose Replace; not overwritten".to_string()
            }
            Refusal::InUse => "destination is in use; not overwritten".to_string(),
            Refusal::Failed(e) => format!("could not replace the destination: {e}"),
        };
        match &self.kept_as {
            Some(p) => format!("{base}; the previous version was kept as {}", p.display()),
            None => base,
        }
    }
}

/// Publish `new` at `dest` in place of the destination captured in `state`, only if that is
/// still the one there. The destination is opened once, with writers denied, and verified
/// through that handle (full stamp, file id included, then content). The same handle is then
/// renamed aside, so the object that was verified is the one moved, and it stays open through
/// the publish, so nothing can write to it in between. The new file is renamed into `dest`
/// with no replace: a save that lands there meanwhile makes that rename fail and is kept. On any
/// failure the previous version is renamed back through the handle.
/// Returns the aside path when the replace succeeded but that previous version could not be
/// deleted; a crash midway leaves it under its `.zxraw-replaced-` name, never deleted.
fn replace_guarded(
    new: &Path,
    dest: &Path,
    state: &ReplaceState,
) -> std::result::Result<Option<PathBuf>, ReplaceRefused> {
    let refuse = |reason, kept_as| ReplaceRefused { reason, kept_as };
    let file = match open_denying_writers(dest) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(refuse(Refusal::Changed, None));
        }
        Err(e) if is_sharing_violation(&e) => return Err(refuse(Refusal::InUse, None)),
        Err(e) => return Err(refuse(Refusal::Failed(e), None)),
    };
    if !state.matches_open(&file, dest) {
        return Err(refuse(Refusal::Changed, None));
    }
    #[cfg(test)]
    BEFORE_REPLACE_ASIDE.with(|hook| {
        if let Some(f) = &*hook.borrow() {
            f(dest);
        }
    });
    let dir = dest.parent().unwrap_or(Path::new(""));
    let aside = dir.join(aside_name(&paths::name_of(dest), &random_hex()[..8]));
    if let Err(e) = rename_open_no_replace(&file, dest, &aside) {
        // If the path no longer holds the verified object, the destination changed.
        let reason = if fs::symlink_metadata(dest).is_err()
            || path_stamp(dest).ok().as_ref() != state.stamp.as_ref()
        {
            Refusal::Changed
        } else if is_sharing_violation(&e) {
            Refusal::InUse
        } else {
            Refusal::Failed(e)
        };
        return Err(refuse(reason, None));
    }
    #[cfg(test)]
    AFTER_REPLACE_ASIDE.with(|hook| {
        if let Some(f) = &*hook.borrow() {
            f(dest);
        }
    });
    match paths::rename_no_replace(new, dest) {
        Ok(()) => {
            drop(file);
            #[cfg(test)]
            BEFORE_ASIDE_DELETE.with(|hook| {
                if let Some(f) = &*hook.borrow() {
                    f(&aside);
                }
            });
            // The replace has succeeded; a leftover aside file is reported, not failed over.
            let leftover = remove_file_force(&aside).or_else(|_| fs::remove_dir(&aside));
            Ok(leftover.err().map(|_| aside))
        }
        Err(e) => {
            // Put the verified previous version back through the same handle.
            let kept_as = rename_open_no_replace(&file, &aside, dest)
                .err()
                .map(|_| aside.clone());
            drop(file);
            // AlreadyExists: someone saved a new file at dest meanwhile, and it is kept.
            let reason = if e.kind() == io::ErrorKind::AlreadyExists {
                Refusal::Changed
            } else {
                Refusal::Failed(e)
            };
            Err(refuse(reason, kept_as))
        }
    }
}

/// Move a finished temp file to its final name, respecting an explicit overwrite policy.
pub(crate) fn place(tmp: &Path, dest: &Path, overwrite: bool) -> Result<()> {
    let io = |e| CoreError::io(dest, e);
    if overwrite {
        if fs::symlink_metadata(dest).is_ok_and(|m| m.permissions().readonly()) {
            clear_readonly(dest).map_err(io)?;
        }
        return fs::rename(tmp, dest).map_err(io);
    }
    // No check-then-rename fallback: rename_no_replace keeps publication atomic.
    match paths::rename_no_replace(tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            Err(CoreError::AlreadyExists(dest.to_path_buf()))
        }
        Err(e) => Err(io(e)),
    }
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OpKind {
    Copy,
    Move,
}

/// How to handle a destination that already exists.
///
/// Folder-onto-folder is always a *merge* (as in Explorer); the policy then applies to the
/// files inside. Exception: `KeepBoth` on a top-level folder creates `Name (2)` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ConflictPolicy {
    /// Ask the [`ConflictResolver`] per conflict (default).
    #[default]
    Ask,
    Skip,
    Overwrite,
    /// Overwrite only if the source's modified time is newer than the destination's.
    OverwriteIfNewer,
    /// Keep both: the new item gets `name (2).ext`.
    KeepBoth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictDecision {
    Skip,
    Overwrite,
    OverwriteIfNewer,
    KeepBoth,
    /// Stop the whole operation (cancels the job's token).
    Cancel,
}

#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub source_kind: NodeKind,
    pub dest_kind: NodeKind,
    pub source_size: u64,
    pub dest_size: u64,
    pub source_modified: Option<SystemTime>,
    pub dest_modified: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub decision: ConflictDecision,
    /// Apply this decision to all remaining conflicts of the operation.
    pub apply_to_all: bool,
}

impl Resolution {
    pub fn once(decision: ConflictDecision) -> Self {
        Self {
            decision,
            apply_to_all: false,
        }
    }
    pub fn all(decision: ConflictDecision) -> Self {
        Self {
            decision,
            apply_to_all: true,
        }
    }
}

/// Answers conflicts when the policy is [`ConflictPolicy::Ask`]. Called on the job thread;
/// a UI implementation typically sends the question over a channel and blocks for the
/// answer. Any `FnMut(&ConflictInfo) -> Resolution` closure implements it.
pub trait ConflictResolver {
    fn resolve(&mut self, conflict: &ConflictInfo) -> Resolution;
}

impl<F: FnMut(&ConflictInfo) -> Resolution> ConflictResolver for F {
    fn resolve(&mut self, conflict: &ConflictInfo) -> Resolution {
        self(conflict)
    }
}

/// Resolver for non-interactive contexts: skips every conflict.
pub struct SkipConflicts;

impl ConflictResolver for SkipConflicts {
    fn resolve(&mut self, _: &ConflictInfo) -> Resolution {
        Resolution::all(ConflictDecision::Skip)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct OpOptions {
    pub policy: ConflictPolicy,
    /// Verify copies by re-reading the destination (always on for cross-volume moves).
    pub verify: bool,
    /// Never use `rename` for moves (copy + verify + delete instead). Useful when the
    /// caller knows the volumes differ, and to exercise the fallback in tests.
    pub force_copy_for_move: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRoot {
    pub source: PathBuf,
    /// Intended destination (may change at execution time for `KeepBoth`).
    pub dest: PathBuf,
    pub kind: NodeKind,
    /// Bytes of all regular files in this root.
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItem {
    pub root: usize,
    /// Path relative to the root's source ("" for the root itself).
    pub rel: PathBuf,
    pub kind: NodeKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpIssue {
    pub path: PathBuf,
    pub reason: String,
}

impl OpIssue {
    pub fn new(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            reason: reason.into(),
        }
    }
}

/// A scanned copy/move request. Items are grouped by root, each root's items in pre-order
/// (a folder always precedes its contents).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpPlan {
    pub kind: OpKind,
    pub options: OpOptions,
    pub dest_dir: PathBuf,
    pub roots: Vec<PlanRoot>,
    pub items: Vec<PlanItem>,
    pub files: u64,
    pub dirs: u64,
    pub links: u64,
    pub bytes: u64,
    /// Problems found while scanning (missing sources, unreadable folders). They are
    /// reported as failures when the plan executes.
    pub issues: Vec<OpIssue>,
}

/// Scan `sources` for a copy/move into `dest_dir`. Does not follow links. Fails (without
/// side effects) if the destination is not a folder or a folder would be copied/moved into
/// itself.
pub fn plan(
    kind: OpKind,
    sources: &[PathBuf],
    dest_dir: &Path,
    options: OpOptions,
    cancel: &CancelToken,
) -> Result<OpPlan> {
    // The destination folder itself may be a link the user navigated into explicitly.
    let dmd = fs::metadata(dest_dir).map_err(|e| CoreError::io(dest_dir, e))?;
    if !dmd.is_dir() {
        return Err(CoreError::Invalid(format!(
            "destination {} is not a folder",
            dest_dir.display()
        )));
    }
    let dest_canon = fs::canonicalize(dest_dir).ok();
    let verb = match kind {
        OpKind::Copy => "copy",
        OpKind::Move => "move",
    };
    let mut plan = OpPlan {
        kind,
        options,
        dest_dir: dest_dir.to_path_buf(),
        roots: Vec::new(),
        items: Vec::new(),
        files: 0,
        dirs: 0,
        links: 0,
        bytes: 0,
        issues: Vec::new(),
    };
    let mut seen: Vec<&PathBuf> = Vec::new();
    for src in sources {
        cancel.check()?;
        if seen.iter().any(|s| paths::paths_equal(s, src)) {
            continue;
        }
        seen.push(src);
        let md = match fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(e) => {
                plan.issues
                    .push(OpIssue::new(src, CoreError::io(src, e).to_string()));
                continue;
            }
        };
        let Some(name) = src.file_name() else {
            return Err(CoreError::Invalid(format!(
                "cannot {verb} {}: not a file or folder",
                src.display()
            )));
        };
        let nk = node_kind(&md);
        if nk == NodeKind::Dir {
            let lexical = paths::is_within(src, dest_dir);
            let canonical = match (fs::canonicalize(src).ok(), &dest_canon) {
                (Some(a), Some(b)) => paths::is_within(&a, b),
                _ => false,
            };
            if lexical || canonical {
                return Err(CoreError::Invalid(format!(
                    "cannot {verb} the folder \"{}\" into itself",
                    name.to_string_lossy()
                )));
            }
        }
        let root = plan.roots.len();
        let size = if nk == NodeKind::File { md.len() } else { 0 };
        plan.items.push(PlanItem {
            root,
            rel: PathBuf::new(),
            kind: nk,
            size,
            modified: md.modified().ok(),
        });
        let mut bytes = size;
        let mut counts = [0u64; 3];
        counts[nk as usize] += 1;
        if nk == NodeKind::Dir {
            let items = &mut plan.items;
            let issues = &mut plan.issues;
            walk_tree(src, cancel, |item| match item {
                WalkItem::Entry(e) => {
                    bytes += e.size;
                    counts[e.kind as usize] += 1;
                    items.push(PlanItem {
                        root,
                        rel: e.rel.clone(),
                        kind: e.kind,
                        size: e.size,
                        modified: e.modified,
                    });
                }
                WalkItem::Error { path, error } => {
                    issues.push(OpIssue::new(path, error.to_string()));
                }
            })?;
        }
        plan.files += counts[NodeKind::File as usize];
        plan.dirs += counts[NodeKind::Dir as usize];
        plan.links += counts[NodeKind::Link as usize];
        plan.bytes += bytes;
        plan.roots.push(PlanRoot {
            source: src.clone(),
            dest: dest_dir.join(name),
            kind: nk,
            bytes,
        });
    }
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Undo
// ---------------------------------------------------------------------------

/// Cheap content fingerprint used to make sure an undo does not destroy later changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub files: u64,
    pub bytes: u64,
    pub newest_modified: Option<SystemTime>,
}

/// Fingerprint a file or a folder tree (links not followed).
pub fn fingerprint(path: &Path) -> Result<Fingerprint> {
    let md = fs::symlink_metadata(path).map_err(|e| CoreError::io(path, e))?;
    let mut fp = Fingerprint {
        files: 0,
        bytes: 0,
        newest_modified: None,
    };
    match node_kind(&md) {
        NodeKind::File => {
            fp.files = 1;
            fp.bytes = md.len();
            fp.newest_modified = md.modified().ok();
        }
        NodeKind::Link => {}
        NodeKind::Dir => {
            let mut err = None;
            walk_tree(path, &CancelToken::new(), |item| match item {
                WalkItem::Entry(e) if e.is_file() => {
                    fp.files += 1;
                    fp.bytes += e.size;
                    fp.newest_modified = fp.newest_modified.max(e.modified);
                }
                WalkItem::Entry(_) => {}
                WalkItem::Error { path, .. } => err = Some(path.clone()),
            })?;
            if let Some(p) = err {
                return Err(CoreError::Invalid(format!("{} is unreadable", p.display())));
            }
        }
    }
    Ok(fp)
}

/// Information needed to reverse an operation. Only genuinely reversible operations produce
/// records: renames, same-volume moves (via rename, without overwriting), newly created
/// items, and copies to a *new* name. Overwrites and cross-volume moves are not recorded:
/// the replaced data is gone (or the move would need a second full copy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UndoRecord {
    Renamed {
        from: PathBuf,
        to: PathBuf,
    },
    Moved {
        from: PathBuf,
        to: PathBuf,
    },
    CreatedFolder {
        path: PathBuf,
    },
    CreatedFile {
        path: PathBuf,
    },
    /// A copy created `path` (the source still exists). Undo deletes the copy, but only if
    /// its fingerprint is unchanged.
    Copied {
        path: PathBuf,
        fingerprint: Fingerprint,
    },
}

impl UndoRecord {
    pub fn describe(&self) -> String {
        match self {
            Self::Renamed { from, to } => format!(
                "rename \"{}\" back to \"{}\"",
                paths::name_of(to),
                paths::name_of(from)
            ),
            Self::Moved { from, to } => {
                format!("move {} back to {}", to.display(), from.display())
            }
            Self::CreatedFolder { path } => format!("remove new folder {}", path.display()),
            Self::CreatedFile { path } => format!("remove new file {}", path.display()),
            Self::Copied { path, .. } => format!("delete copy {}", path.display()),
        }
    }
}

/// Reverse one record. Refuses (rather than guesses) when the world changed since: the
/// original location is occupied, a created folder is no longer empty, a created file has
/// content, or a copy was modified.
pub fn undo(rec: &UndoRecord) -> Result<()> {
    match rec {
        UndoRecord::Renamed { from, to } => {
            let same_parent = match (from.parent(), to.parent()) {
                (Some(a), Some(b)) => paths::paths_equal(a, b),
                _ => false,
            };
            if same_parent {
                rename_entry(to, &paths::name_of(from)).map(|_| ())
            } else {
                rename_back(to, from)
            }
        }
        UndoRecord::Moved { from, to } => {
            if let Some(parent) = from.parent() {
                fs::create_dir_all(parent).map_err(|e| CoreError::io(parent, e))?;
            }
            rename_back(to, from)
        }
        UndoRecord::CreatedFolder { path } => fs::remove_dir(path).map_err(|e| {
            if fs::read_dir(path).is_ok_and(|mut d| d.next().is_some()) {
                CoreError::Invalid(format!("{} is no longer empty", path.display()))
            } else {
                CoreError::io(path, e)
            }
        }),
        UndoRecord::CreatedFile { path } => {
            let md = fs::symlink_metadata(path).map_err(|e| CoreError::io(path, e))?;
            if md.len() != 0 {
                return Err(CoreError::Invalid(format!(
                    "{} has been modified since it was created",
                    path.display()
                )));
            }
            remove_file_force(path).map_err(|e| CoreError::io(path, e))
        }
        UndoRecord::Copied {
            path,
            fingerprint: fp,
        } => {
            if &fingerprint(path)? != fp {
                return Err(CoreError::Invalid(format!(
                    "{} has been modified since it was copied",
                    path.display()
                )));
            }
            let cancel = CancelToken::new();
            let progress = Progress::default();
            let r =
                delete_permanently(std::slice::from_ref(path), &JobCtx::new(&cancel, &progress));
            match r.failed.into_iter().next() {
                Some(f) => Err(CoreError::Invalid(format!(
                    "{}: {}",
                    f.path.display(),
                    f.reason
                ))),
                None => Ok(()),
            }
        }
    }
}

fn rename_back(current: &Path, original: &Path) -> Result<()> {
    paths::rename_no_replace(current, original).map_err(|e| CoreError::io(original, e))
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpReport {
    /// Destination paths of completed files/links (and of whole folders moved by rename).
    pub done: Vec<PathBuf>,
    pub skipped: Vec<OpIssue>,
    pub failed: Vec<OpIssue>,
    /// Things that went wrong without failing the item, e.g. a replaced file's previous version
    /// that could not be deleted afterwards.
    #[serde(default)]
    pub warnings: Vec<OpIssue>,
    pub bytes: u64,
    pub cancelled: bool,
    /// Reversal records, in execution order (undo in reverse order).
    pub undo: Vec<UndoRecord>,
}

impl OpReport {
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty() && !self.cancelled
    }
}

/// Execute a plan as a registered job (logs into the job report).
pub fn execute(plan: &OpPlan, job: &JobHandle, resolver: &mut dyn ConflictResolver) -> OpReport {
    execute_with(plan, &JobCtx::from_job(job), resolver)
}

/// Execute a plan with an explicit context.
pub fn execute_with(
    plan: &OpPlan,
    ctx: &JobCtx<'_>,
    resolver: &mut dyn ConflictResolver,
) -> OpReport {
    let mut ex = Executor {
        plan,
        ctx,
        resolver,
        policy: plan.options.policy,
        report: OpReport::default(),
        try_rename: false,
    };
    ex.run();
    ex.report
}

enum Placement {
    New(PathBuf),
    KeepBoth(PathBuf),
    Merge(PathBuf),
    Overwrite(PathBuf, ReplaceState),
    Skip(String),
    Fail(String),
}

/// Destinations up to this size are also hashed when a Replace decision is presented: reading
/// them twice is cheap next to the copy.
const REPLACE_HASH_LIMIT: u64 = 4 * 1024 * 1024;

/// What, besides the stamp, must still match before a Replace is published.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ContentCheck {
    /// Nothing more: folders, files over REPLACE_HASH_LIMIT, and online-only placeholders
    /// (reading those would download them).
    StampOnly,
    /// BLAKE3 of a file up to REPLACE_HASH_LIMIT.
    Hash(Digest),
    /// Where a link at the destination points.
    LinkTarget(PathBuf),
    /// The check was required but could not be made; the destination is treated as changed.
    Unreadable,
}

/// Identity of a destination observed when a Replace decision is presented. It is checked
/// again immediately before an overwrite is published so a later save at the destination is
/// preserved. Size and last-write time alone miss a same-size rewrite within one clock tick,
/// so the file id (catches saves that rename a new file over it) and a content check are
/// compared too. Links are compared by their target. Online-only placeholders are not read,
/// so that capturing them never downloads them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplaceState {
    kind: NodeKind,
    stamp: Option<Stamp>,
    check: ContentCheck,
}

impl ReplaceState {
    /// State for a destination that is never checked beyond its kind (no overwrite possible).
    fn unchecked(kind: NodeKind) -> Self {
        Self {
            kind,
            stamp: None,
            check: ContentCheck::StampOnly,
        }
    }

    fn capture(path: &Path, metadata: &fs::Metadata) -> Self {
        Self::capture_with(path, metadata, &crate::cloud::FsAttributes)
    }

    /// [`capture`](Self::capture) with an injectable attribute source. Files are read only when
    /// their find data says the data is on this PC.
    fn capture_with(
        path: &Path,
        metadata: &fs::Metadata,
        attrs: &dyn crate::cloud::AttributeSource,
    ) -> Self {
        let kind = node_kind(metadata);
        match kind {
            NodeKind::Dir => Self::unchecked(kind),
            NodeKind::File => Self {
                kind,
                stamp: path_stamp(path).ok(),
                check: if !crate::cloud::background_read_allowed_with(path, attrs) {
                    ContentCheck::StampOnly
                } else if metadata.len() <= REPLACE_HASH_LIMIT {
                    match hashing::hash_file(path, &CancelToken::new(), None) {
                        Ok(digest) => ContentCheck::Hash(digest),
                        Err(_) => ContentCheck::Unreadable,
                    }
                } else {
                    ContentCheck::StampOnly
                },
            },
            NodeKind::Link => Self {
                kind,
                stamp: path_stamp(path).ok(),
                check: match fs::read_link(path) {
                    Ok(target) => ContentCheck::LinkTarget(target),
                    Err(_) => ContentCheck::Unreadable,
                },
            },
        }
    }

    /// The destination open as `file` (found at `path`) is still the captured one: full stamp,
    /// file id included, then content. It is checked on the open object, before the rename
    /// aside, so a save that replaces the path meanwhile is not the object that gets moved.
    fn matches_open(&self, file: &File, path: &Path) -> bool {
        if self.kind == NodeKind::Dir {
            return false;
        }
        // A destination that could not be stamped when captured is treated as changed.
        let Some(stamp) = &self.stamp else {
            return false;
        };
        if handle_stamp(file).ok().as_ref() != Some(stamp) {
            return false;
        }
        match &self.check {
            ContentCheck::StampOnly => true,
            ContentCheck::Hash(digest) => {
                hashing::hash_reader(file, path, &CancelToken::new(), None)
                    .is_ok_and(|now| now == *digest)
            }
            ContentCheck::LinkTarget(target) => fs::read_link(path).is_ok_and(|now| now == *target),
            ContentCheck::Unreadable => false,
        }
    }

    /// [`matches_open`](Self::matches_open) for the unit tests, opening `path` itself.
    #[cfg(test)]
    fn still_matches(&self, path: &Path) -> bool {
        open_denying_writers(path).is_ok_and(|f| self.matches_open(&f, path))
    }
}

struct Executor<'p, 'c, 'r> {
    plan: &'p OpPlan,
    ctx: &'c JobCtx<'c>,
    resolver: &'r mut dyn ConflictResolver,
    policy: ConflictPolicy,
    report: OpReport,
    /// Whether moves within the current root should attempt `rename` first. Turned off
    /// after the root-level rename failed (different volume), to avoid one failing
    /// syscall per file.
    try_rename: bool,
}

const VANISHED: &str = "source no longer exists (deleted, or the device was disconnected)";

impl Executor<'_, '_, '_> {
    fn is_move(&self) -> bool {
        self.plan.kind == OpKind::Move
    }

    fn run(&mut self) {
        let p = self.ctx.progress;
        p.add_total_items(self.plan.items.len() as u64);
        p.add_total_bytes(self.plan.bytes);
        for issue in &self.plan.issues {
            self.ctx.log(format!(
                "failed: {}: {}",
                issue.path.display(),
                issue.reason
            ));
            p.add_error();
            self.report.failed.push(issue.clone());
        }
        let mut start = 0;
        for (ri, root) in self.plan.roots.iter().enumerate() {
            let end = start
                + self.plan.items[start..]
                    .iter()
                    .take_while(|it| it.root == ri)
                    .count();
            let items = &self.plan.items[start..end];
            start = end;
            if items.is_empty() {
                continue;
            }
            if let Err(e) = self.run_root(root, items) {
                debug_assert!(matches!(e, CoreError::Cancelled));
                self.report.cancelled = true;
                self.ctx.log("operation cancelled");
                break;
            }
        }
    }

    fn skip(&mut self, path: &Path, reason: impl Into<String>) {
        let reason = reason.into();
        self.ctx
            .log(format!("skipped: {}: {reason}", path.display()));
        self.report.skipped.push(OpIssue::new(path, reason));
    }

    fn fail(&mut self, path: &Path, reason: impl Into<String>) {
        let reason = reason.into();
        self.ctx
            .log(format!("failed: {}: {reason}", path.display()));
        self.ctx.progress.add_error();
        self.report.failed.push(OpIssue::new(path, reason));
    }

    /// Record that a replaced file's previous version was kept at `aside`, not deleted.
    fn warn_kept_previous(&mut self, dest: &Path, aside: &Path) {
        let reason = format!(
            "replaced, but the previous version could not be deleted; it was kept as {}",
            aside.display()
        );
        self.ctx.log(format!("{}: {reason}", dest.display()));
        self.report.warnings.push(OpIssue::new(dest, reason));
    }

    /// Account progress for items that will not be processed.
    fn consume(&self, items: &[PlanItem]) {
        self.ctx.progress.add_items(items.len() as u64);
        self.ctx
            .progress
            .add_bytes(items.iter().map(|i| i.size).sum());
    }

    fn decide(&mut self, info: &ConflictInfo) -> Result<ConflictDecision> {
        let d = match self.policy {
            ConflictPolicy::Ask => {
                let r = self.resolver.resolve(info);
                if r.apply_to_all {
                    self.policy = match r.decision {
                        ConflictDecision::Skip => ConflictPolicy::Skip,
                        ConflictDecision::Overwrite => ConflictPolicy::Overwrite,
                        ConflictDecision::OverwriteIfNewer => ConflictPolicy::OverwriteIfNewer,
                        ConflictDecision::KeepBoth => ConflictPolicy::KeepBoth,
                        ConflictDecision::Cancel => ConflictPolicy::Ask,
                    };
                }
                r.decision
            }
            ConflictPolicy::Skip => ConflictDecision::Skip,
            ConflictPolicy::Overwrite => ConflictDecision::Overwrite,
            ConflictPolicy::OverwriteIfNewer => ConflictDecision::OverwriteIfNewer,
            ConflictPolicy::KeepBoth => ConflictDecision::KeepBoth,
        };
        if d == ConflictDecision::Cancel {
            self.ctx.cancel.cancel();
            return Err(CoreError::Cancelled);
        }
        Ok(d)
    }

    fn unique(dest: &Path, kind: NodeKind) -> PathBuf {
        let dir = dest.parent().unwrap_or(Path::new(""));
        paths::unique_name_with(
            dir,
            &paths::name_of(dest),
            kind != NodeKind::Dir,
            paths::exists_no_follow,
        )
    }

    /// Decide where `item` (at `src`) goes, given the intended `dest`.
    fn placement(
        &mut self,
        src: &Path,
        item: &PlanItem,
        dest: PathBuf,
        is_root: bool,
    ) -> Result<Placement> {
        if is_root && paths::paths_equal(src, &dest) {
            // Copy into the folder it already lives in: always keep both.
            return Ok(Placement::KeepBoth(Self::unique(&dest, item.kind)));
        }
        let Ok(dmd) = fs::symlink_metadata(&dest) else {
            return Ok(Placement::New(dest));
        };
        let dkind = node_kind(&dmd);
        // Capture before asking the resolver: it may wait while another app saves a new
        // version, and Replace must apply only to the item described in this prompt.
        // Hashing is expensive, so only capture when an overwrite is actually possible.
        let replace_state = if item.kind == NodeKind::File
            && dkind != NodeKind::Dir
            && !matches!(self.policy, ConflictPolicy::Skip | ConflictPolicy::KeepBoth)
        {
            ReplaceState::capture(&dest, &dmd)
        } else {
            ReplaceState::unchecked(dkind)
        };
        if item.kind == NodeKind::Dir && dkind == NodeKind::Dir {
            if is_root && self.policy == ConflictPolicy::KeepBoth {
                return Ok(Placement::KeepBoth(Self::unique(&dest, item.kind)));
            }
            return Ok(Placement::Merge(dest));
        }
        if !is_root && item.kind == NodeKind::Dir {
            return Ok(Placement::Fail(
                "an item with this name already exists at the destination and is not a folder"
                    .into(),
            ));
        }
        let info = ConflictInfo {
            source: src.to_path_buf(),
            dest: dest.clone(),
            source_kind: item.kind,
            dest_kind: dkind,
            source_size: item.size,
            dest_size: if dkind == NodeKind::File {
                dmd.len()
            } else {
                0
            },
            source_modified: item.modified,
            dest_modified: dmd.modified().ok(),
        };
        Ok(match self.decide(&info)? {
            ConflictDecision::Skip => Placement::Skip("already exists at the destination".into()),
            ConflictDecision::KeepBoth => Placement::KeepBoth(Self::unique(&dest, item.kind)),
            d @ (ConflictDecision::Overwrite | ConflictDecision::OverwriteIfNewer) => {
                let newer = matches!(
                    (info.source_modified, info.dest_modified),
                    (Some(s), Some(d)) if s > d
                );
                if d == ConflictDecision::OverwriteIfNewer && !newer {
                    Placement::Skip("destination is the same age or newer".into())
                } else if item.kind != NodeKind::File || dkind == NodeKind::Dir {
                    Placement::Fail(format!(
                        "cannot replace a {} with a {}",
                        kind_word(dkind),
                        kind_word(item.kind)
                    ))
                } else {
                    Placement::Overwrite(dest, replace_state)
                }
            }
            ConflictDecision::Cancel => unreachable!("handled in decide"),
        })
    }

    fn create_new_dir(&mut self, path: PathBuf, keep_both: bool) -> Option<PathBuf> {
        let mut path = path;
        for _ in 0..1024 {
            match fs::create_dir(&path) {
                Ok(()) => return Some(path),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && keep_both => {
                    path = Self::unique(&path, NodeKind::Dir);
                }
                Err(e) => {
                    self.fail(&path, CoreError::io(&path, e).to_string());
                    return None;
                }
            }
        }
        self.fail(&path, CoreError::AlreadyExists(path.clone()).to_string());
        None
    }

    fn prepare_new_root_dir(
        &mut self,
        src_root: &Path,
        items: &[PlanItem],
        root_bytes: u64,
        mut dest: PathBuf,
        keep_both: bool,
    ) -> Option<PathBuf> {
        if self.try_rename {
            for _ in 0..1024 {
                match paths::rename_no_replace(src_root, &dest) {
                    Ok(()) => {
                        self.consume(items);
                        self.report.bytes += root_bytes;
                        self.report.done.push(dest.clone());
                        self.report.undo.push(UndoRecord::Moved {
                            from: src_root.to_path_buf(),
                            to: dest,
                        });
                        return None;
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists && keep_both => {
                        dest = Self::unique(&dest, NodeKind::Dir);
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        self.fail(src_root, CoreError::AlreadyExists(dest).to_string());
                        self.consume(items);
                        return None;
                    }
                    Err(e)
                        if e.kind() == io::ErrorKind::NotFound
                            && !paths::exists_no_follow(src_root) =>
                    {
                        self.fail(src_root, VANISHED);
                        self.consume(items);
                        return None;
                    }
                    Err(e) => {
                        self.try_rename = false;
                        self.ctx.log(format!(
                            "{}: rename not possible ({e}); copying instead",
                            src_root.display()
                        ));
                        break;
                    }
                }
            }
            if self.try_rename {
                self.fail(
                    src_root,
                    "could not find a free destination after repeated conflicts",
                );
                self.consume(items);
                return None;
            }
        }
        let created = self.create_new_dir(dest, keep_both);
        if created.is_none() {
            self.consume(items);
        }
        created
    }

    fn run_root(&mut self, root: &PlanRoot, items: &[PlanItem]) -> Result<()> {
        self.ctx.cancel.check()?;
        self.try_rename = self.is_move() && !self.plan.options.force_copy_for_move;
        if !paths::exists_no_follow(&root.source) {
            self.fail(&root.source, VANISHED);
            self.consume(items);
            return Ok(());
        }
        if self.is_move() && paths::paths_equal(&root.source, &root.dest) {
            self.skip(&root.source, "already in the destination folder");
            self.consume(items);
            return Ok(());
        }
        let first = &items[0];
        match root.kind {
            NodeKind::File => {
                self.transfer_file(&root.source, first, root.dest.clone(), true)?;
                Ok(())
            }
            NodeKind::Link => {
                self.transfer_link(&root.source, first, root.dest.clone(), true)?;
                Ok(())
            }
            NodeKind::Dir => self.run_dir_root(root, items),
        }
    }

    fn run_dir_root(&mut self, root: &PlanRoot, items: &[PlanItem]) -> Result<()> {
        let src_root = &root.source;
        let (dest_root, fresh) =
            match self.placement(src_root, &items[0], root.dest.clone(), true)? {
                Placement::New(p) => match self.prepare_new_root_dir(
                    src_root,
                    items,
                    root.bytes,
                    p,
                    self.policy == ConflictPolicy::KeepBoth,
                ) {
                    Some(p) => (p, true),
                    None => return Ok(()),
                },
                Placement::KeepBoth(p) => {
                    match self.prepare_new_root_dir(src_root, items, root.bytes, p, true) {
                        Some(p) => (p, true),
                        None => return Ok(()),
                    }
                }
                Placement::Merge(p) => (p, false),
                Placement::Skip(r) => {
                    self.skip(src_root, r);
                    self.consume(items);
                    return Ok(());
                }
                Placement::Fail(r) => {
                    self.fail(src_root, r);
                    self.consume(items);
                    return Ok(());
                }
                Placement::Overwrite(_, _) => unreachable!("folders are never overwritten"),
            };
        self.ctx.progress.add_items(1);

        let mut blocked: Vec<&Path> = Vec::new();
        let mut idx = 1;
        while idx < items.len() {
            let item = &items[idx];
            idx += 1;
            if blocked.iter().any(|b| item.rel.starts_with(b)) {
                self.consume(std::slice::from_ref(item));
                continue;
            }
            let src = src_root.join(&item.rel);
            let dest = dest_root.join(&item.rel);
            let vanished = match item.kind {
                NodeKind::Dir => {
                    if !paths::exists_no_follow(&src) {
                        self.fail(&src, VANISHED);
                        blocked.push(&item.rel);
                        self.ctx.progress.add_items(1);
                        true
                    } else {
                        match self.placement(&src, item, dest, false)? {
                            Placement::New(p) => {
                                if self.create_new_dir(p, false).is_none() {
                                    blocked.push(&item.rel);
                                }
                            }
                            Placement::KeepBoth(p) => {
                                if self.create_new_dir(p, true).is_none() {
                                    blocked.push(&item.rel);
                                }
                            }
                            Placement::Merge(_) => {}
                            Placement::Skip(r) => {
                                self.skip(&src, r);
                                blocked.push(&item.rel);
                            }
                            Placement::Fail(r) => {
                                self.fail(&src, r);
                                blocked.push(&item.rel);
                            }
                            Placement::Overwrite(_, _) => {
                                unreachable!("folders are never overwritten")
                            }
                        }
                        self.ctx.progress.add_items(1);
                        false
                    }
                }
                NodeKind::File => self.transfer_file(&src, item, dest, false)?,
                NodeKind::Link => self.transfer_link(&src, item, dest, false)?,
            };
            if (vanished && !paths::exists_no_follow(src_root)) || fault::source_is_gone(src_root) {
                // The whole source is gone (e.g. card pulled): don't grind through
                // thousands of doomed items.
                let rest = &items[idx..];
                if !rest.is_empty() {
                    self.fail(
                        src_root,
                        format!("{VANISHED}; {} remaining item(s) not processed", rest.len()),
                    );
                    self.consume(rest);
                }
                break;
            }
        }

        if self.is_move() {
            // Remove emptied source folders bottom-up; non-empty ones (skipped/failed
            // content) stay.
            for item in items.iter().rev().filter(|i| i.kind == NodeKind::Dir) {
                let _ = fs::remove_dir(src_root.join(&item.rel));
            }
        } else if fresh {
            match fingerprint(&dest_root) {
                Ok(fp) => self.report.undo.push(UndoRecord::Copied {
                    path: dest_root,
                    fingerprint: fp,
                }),
                Err(e) => self.ctx.log(format!("no undo record: {e}")),
            }
        }
        Ok(())
    }

    /// Returns Ok(true) if the failure was "source vanished".
    fn transfer_file(
        &mut self,
        src: &Path,
        item: &PlanItem,
        dest: PathBuf,
        is_root: bool,
    ) -> Result<bool> {
        if !paths::exists_no_follow(src) {
            self.fail(src, VANISHED);
            self.consume(std::slice::from_ref(item));
            return Ok(true);
        }
        let (mut dest, overwrite, keep_both, replace_state) =
            match self.placement(src, item, dest, is_root)? {
                Placement::New(p) => (p, false, self.policy == ConflictPolicy::KeepBoth, None),
                Placement::KeepBoth(p) => (p, false, true, None),
                Placement::Overwrite(p, state) => (p, true, false, Some(state)),
                Placement::Skip(r) => {
                    self.skip(src, r);
                    self.consume(std::slice::from_ref(item));
                    return Ok(false);
                }
                Placement::Fail(r) => {
                    self.fail(src, r);
                    self.consume(std::slice::from_ref(item));
                    return Ok(false);
                }
                Placement::Merge(_) => unreachable!("files never merge"),
            };
        self.ctx.progress.set_current(src.display().to_string());
        let mv = self.is_move();
        if self.try_rename {
            loop {
                if overwrite {
                    #[cfg(test)]
                    BEFORE_MOVE_RENAME.with(|hook| {
                        if let Some(f) = &*hook.borrow() {
                            f(&dest);
                        }
                    });
                    let outcome = match replace_state.as_ref() {
                        Some(state) => replace_guarded(src, &dest, state),
                        None => Err(ReplaceRefused {
                            reason: Refusal::Changed,
                            kept_as: None,
                        }),
                    };
                    match outcome {
                        Ok(leftover) => {
                            self.consume(std::slice::from_ref(item));
                            self.report.bytes += item.size;
                            self.report.done.push(dest.clone());
                            if let Some(aside) = leftover {
                                self.warn_kept_previous(&dest, &aside);
                            }
                            return Ok(false);
                        }
                        Err(ReplaceRefused {
                            reason: Refusal::Failed(e),
                            kept_as: None,
                        }) => {
                            if e.kind() == io::ErrorKind::NotFound && !paths::exists_no_follow(src)
                            {
                                self.fail(src, VANISHED);
                                self.consume(std::slice::from_ref(item));
                                return Ok(true);
                            }
                            // Could not publish by rename (e.g. another volume): copy instead.
                            break;
                        }
                        Err(refused) => {
                            self.fail(src, refused.message());
                            self.consume(std::slice::from_ref(item));
                            return Ok(false);
                        }
                    }
                }
                match paths::rename_no_replace(src, &dest) {
                    Ok(()) => {
                        self.consume(std::slice::from_ref(item));
                        self.report.bytes += item.size;
                        self.report.done.push(dest.clone());
                        self.report.undo.push(UndoRecord::Moved {
                            from: src.to_path_buf(),
                            to: dest,
                        });
                        return Ok(false);
                    }
                    Err(e)
                        if e.kind() == io::ErrorKind::AlreadyExists && keep_both && !overwrite =>
                    {
                        dest = Self::unique(&dest, NodeKind::File);
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        self.fail(src, CoreError::AlreadyExists(dest.clone()).to_string());
                        self.consume(std::slice::from_ref(item));
                        return Ok(false);
                    }
                    Err(e)
                        if e.kind() == io::ErrorKind::NotFound && !paths::exists_no_follow(src) =>
                    {
                        self.fail(src, VANISHED);
                        self.consume(std::slice::from_ref(item));
                        return Ok(true);
                    }
                    Err(_) => break,
                }
            }
        }
        let opts = CopyFileOptions {
            overwrite,
            verify: self.plan.options.verify || mv,
            want_hash: false,
        };
        let mut counted = 0;
        let replace_leftover = Cell::new(None);
        let res = copy_file_counted(
            src,
            &mut dest,
            opts,
            self.ctx,
            &mut counted,
            Publish {
                keep_both,
                still_ok: None,
                still_ok_message: None,
                replace: replace_state
                    .as_ref()
                    .filter(|_| overwrite)
                    .map(|state| (state, &replace_leftover)),
            },
        );
        self.ctx.progress.add_items(1);
        match res {
            Ok(out) => {
                if let Some(aside) = replace_leftover.take() {
                    self.warn_kept_previous(&dest, &aside);
                }
                self.report.bytes += out.bytes;
                // Sources that grew/shrank during the copy: keep the bar consistent.
                self.ctx
                    .progress
                    .add_bytes(item.size.saturating_sub(counted));
                if mv {
                    if let Err(e) = remove_file_force(src) {
                        self.fail(
                            src,
                            format!("copied, but the source could not be deleted: {e}"),
                        );
                        return Ok(false);
                    }
                } else if is_root
                    && !overwrite
                    && let Ok(fp) = fingerprint(&dest)
                {
                    self.report.undo.push(UndoRecord::Copied {
                        path: dest.clone(),
                        fingerprint: fp,
                    });
                }
                self.report.done.push(dest);
                Ok(false)
            }
            Err(CoreError::Cancelled) => Err(CoreError::Cancelled),
            Err(e) => {
                self.ctx
                    .progress
                    .add_bytes(item.size.saturating_sub(counted));
                let vanished = matches!(&e, CoreError::NotFound(p) if p == src)
                    && !paths::exists_no_follow(src);
                if vanished {
                    self.fail(src, VANISHED);
                } else {
                    self.fail(src, e.to_string());
                }
                Ok(vanished)
            }
        }
    }

    fn transfer_link(
        &mut self,
        src: &Path,
        item: &PlanItem,
        dest: PathBuf,
        is_root: bool,
    ) -> Result<bool> {
        self.ctx.progress.add_items(1);
        if !paths::exists_no_follow(src) {
            self.fail(src, VANISHED);
            return Ok(true);
        }
        let (mut dest, keep_both) = match self.placement(src, item, dest, is_root)? {
            Placement::New(p) => (p, self.policy == ConflictPolicy::KeepBoth),
            Placement::KeepBoth(p) => (p, true),
            Placement::Skip(r) => {
                self.skip(src, r);
                return Ok(false);
            }
            Placement::Fail(r) => {
                self.fail(src, r);
                return Ok(false);
            }
            Placement::Overwrite(_, _) => {
                self.skip(src, "replacing existing items with a link is not supported");
                return Ok(false);
            }
            Placement::Merge(_) => unreachable!("links never merge"),
        };
        if self.try_rename {
            for _ in 0..1024 {
                match paths::rename_no_replace(src, &dest) {
                    Ok(()) => {
                        self.report.done.push(dest.clone());
                        self.report.undo.push(UndoRecord::Moved {
                            from: src.to_path_buf(),
                            to: dest,
                        });
                        return Ok(false);
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists && keep_both => {
                        dest = Self::unique(&dest, NodeKind::Link);
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        self.fail(src, CoreError::AlreadyExists(dest).to_string());
                        return Ok(false);
                    }
                    Err(_) => {
                        self.try_rename = false;
                        break;
                    }
                }
            }
            if self.try_rename && keep_both {
                self.fail(
                    src,
                    "could not find a free destination after repeated conflicts",
                );
                return Ok(false);
            }
        }
        for _ in 0..1024 {
            match copy_link(src, &dest) {
                Ok(()) => {
                    if self.is_move()
                        && let Err(e) = remove_link(src)
                    {
                        self.fail(
                            src,
                            format!("link copied, but the original could not be removed: {e}"),
                        );
                        return Ok(false);
                    }
                    self.report.done.push(dest);
                    return Ok(false);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && keep_both => {
                    dest = Self::unique(&dest, NodeKind::Link);
                }
                Err(e) => {
                    self.skip(
                        src,
                        format!(
                            "link not copied (links are never followed; recreating it failed: {e})"
                        ),
                    );
                    return Ok(false);
                }
            }
        }
        self.fail(
            src,
            "could not find a free destination after repeated conflicts",
        );
        Ok(false)
    }
}

fn kind_word(k: NodeKind) -> &'static str {
    match k {
        NodeKind::File => "file",
        NodeKind::Dir => "folder",
        NodeKind::Link => "link",
    }
}

// ---------------------------------------------------------------------------
// Create / rename / delete
// ---------------------------------------------------------------------------

/// Create a new folder `parent/name`. Fails if anything with that name exists.
pub fn create_folder(parent: &Path, name: &str) -> Result<(PathBuf, UndoRecord)> {
    paths::validate_file_name(name)?;
    let p = parent.join(name);
    fs::create_dir(&p).map_err(|e| CoreError::io(&p, e))?;
    Ok((p.clone(), UndoRecord::CreatedFolder { path: p }))
}

/// Create a new folder with the first free name based on `base` ("New folder (2)"…).
pub fn create_folder_unique(parent: &Path, base: &str) -> Result<(PathBuf, UndoRecord)> {
    paths::validate_file_name(base)?;
    for _ in 0..16 {
        let p = paths::unique_name_with(parent, base, false, paths::exists_no_follow);
        match fs::create_dir(&p) {
            Ok(()) => return Ok((p.clone(), UndoRecord::CreatedFolder { path: p })),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue, // raced
            Err(e) => return Err(CoreError::io(&p, e)),
        }
    }
    Err(CoreError::AlreadyExists(parent.join(base)))
}

/// Create a new empty file `parent/name`. Fails if anything with that name exists.
pub fn create_file(parent: &Path, name: &str) -> Result<(PathBuf, UndoRecord)> {
    paths::validate_file_name(name)?;
    let p = parent.join(name);
    File::create_new(&p).map_err(|e| CoreError::io(&p, e))?;
    Ok((p.clone(), UndoRecord::CreatedFile { path: p }))
}

/// Rename `path` to `new_name` within the same folder. Fails if the target exists, except
/// for case-only renames (`photo.jpg` -> `Photo.JPG`) on case-insensitive filesystems,
/// which are done in two steps through a temporary name.
pub fn rename_entry(path: &Path, new_name: &str) -> Result<(PathBuf, UndoRecord)> {
    paths::validate_file_name(new_name)?;
    let old_name = path
        .file_name()
        .ok_or_else(|| CoreError::Invalid(format!("cannot rename {}", path.display())))?
        .to_string_lossy()
        .into_owned();
    fs::symlink_metadata(path).map_err(|e| CoreError::io(path, e))?;
    if old_name == new_name {
        return Err(CoreError::Invalid("the name is unchanged".into()));
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    let target = parent.join(new_name);
    let io_t = |e| CoreError::io(&target, e);
    if paths::exists_no_follow(&target) {
        let case_only = old_name.to_lowercase() == new_name.to_lowercase();
        // On a case-sensitive filesystem a distinct entry with exactly `new_name` exists.
        let exact_exists = fs::read_dir(parent)
            .map_err(|e| CoreError::io(parent, e))?
            .flatten()
            .any(|d| d.file_name().to_string_lossy() == new_name);
        if !case_only || exact_exists {
            return Err(CoreError::AlreadyExists(target));
        }
        let tmp = parent.join(format!(".forge-rename-{}", random_hex()));
        paths::rename_no_replace(path, &tmp).map_err(|e| CoreError::io(path, e))?;
        if let Err(e) = paths::rename_no_replace(&tmp, &target) {
            let _ = paths::rename_no_replace(&tmp, path);
            return Err(io_t(e));
        }
    } else {
        paths::rename_no_replace(path, &target).map_err(io_t)?;
    }
    Ok((
        target.clone(),
        UndoRecord::Renamed {
            from: path.to_path_buf(),
            to: target,
        },
    ))
}

/// Permanently delete files/folders (recursive). Links are removed, never followed.
/// Read-only files are deleted too (the UI's confirmation is the gate). Continues past
/// errors; folders that could not be emptied are reported as failed.
pub fn delete_permanently(targets: &[PathBuf], ctx: &JobCtx<'_>) -> OpReport {
    let mut report = OpReport::default();
    let fail = |report: &mut OpReport, p: &Path, e: String| {
        ctx.log(format!("failed: {}: {e}", p.display()));
        ctx.progress.add_error();
        report.failed.push(OpIssue::new(p, e));
    };
    for target in targets {
        if ctx.cancel.check().is_err() {
            report.cancelled = true;
            break;
        }
        if target.file_name().is_none() {
            fail(
                &mut report,
                target,
                "refusing to delete a drive or root folder".into(),
            );
            continue;
        }
        let md = match fs::symlink_metadata(target) {
            Ok(m) => m,
            Err(e) => {
                fail(&mut report, target, CoreError::io(target, e).to_string());
                continue;
            }
        };
        ctx.progress.set_current(target.display().to_string());
        match node_kind(&md) {
            NodeKind::Link => match remove_link(target) {
                Ok(()) => report.done.push(target.clone()),
                Err(e) => fail(&mut report, target, e.to_string()),
            },
            NodeKind::File => match remove_file_force(target) {
                Ok(()) => report.done.push(target.clone()),
                Err(e) => fail(&mut report, target, e.to_string()),
            },
            NodeKind::Dir => {
                let mut entries = Vec::new();
                let walked = walk_tree(target, ctx.cancel, |item| match item {
                    WalkItem::Entry(e) => entries.push((e.path.clone(), e.kind)),
                    WalkItem::Error { path, error } => {
                        ctx.log(format!("failed: {}: {error}", path.display()))
                    }
                });
                if let Err(e) = walked {
                    if matches!(e, CoreError::Cancelled) {
                        report.cancelled = true;
                        break;
                    }
                    fail(&mut report, target, e.to_string());
                    continue;
                }
                ctx.progress.add_total_items(entries.len() as u64);
                // Reverse pre-order = children before parents.
                for (i, (p, k)) in entries.iter().rev().enumerate() {
                    if i % 64 == 0 && ctx.cancel.check().is_err() {
                        report.cancelled = true;
                        break;
                    }
                    let r = match k {
                        NodeKind::Link => remove_link(p),
                        NodeKind::File => remove_file_force(p),
                        NodeKind::Dir => fs::remove_dir(p),
                    };
                    if let Err(e) = r
                        && e.kind() != io::ErrorKind::NotFound
                    {
                        fail(&mut report, p, e.to_string());
                    }
                    ctx.progress.add_items(1);
                }
                if report.cancelled {
                    break;
                }
                match fs::remove_dir(target) {
                    Ok(()) => report.done.push(target.clone()),
                    Err(e) => fail(&mut report, target, e.to_string()),
                }
            }
        }
        ctx.progress.add_items(1);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Fx {
        _d: tempfile::TempDir,
        src: PathBuf,
        dst: PathBuf,
    }

    fn fx() -> Fx {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        let dst = d.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        Fx { _d: d, src, dst }
    }

    fn run(kind: OpKind, sources: &[PathBuf], dest: &Path, options: OpOptions) -> OpReport {
        run_with(kind, sources, dest, options, &mut SkipConflicts)
    }

    fn run_with(
        kind: OpKind,
        sources: &[PathBuf],
        dest: &Path,
        options: OpOptions,
        resolver: &mut dyn ConflictResolver,
    ) -> OpReport {
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let p = plan(kind, sources, dest, options, &cancel).unwrap();
        execute_with(&p, &JobCtx::new(&cancel, &progress), resolver)
    }

    fn no_partials(dir: &Path) -> bool {
        let mut found = false;
        walk_tree(dir, &CancelToken::new(), |i| {
            if let WalkItem::Entry(e) = i {
                found |= is_partial_name(&paths::name_of(&e.path));
            }
        })
        .unwrap();
        !found
    }

    fn no_replaced_leftovers(dir: &Path) -> bool {
        replaced_leftovers(dir).is_empty()
    }

    fn replaced_leftovers(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| paths::name_of(p).contains(REPLACED_MARKER))
            .collect()
    }

    fn opts(policy: ConflictPolicy) -> OpOptions {
        OpOptions {
            policy,
            verify: true,
            force_copy_for_move: false,
        }
    }

    #[test]
    fn copy_tree_unicode_preserves_mtime_and_readonly() {
        let f = fx();
        let tree = f.src.join("Фото 日本 📷");
        fs::create_dir_all(tree.join("sub/deeper")).unwrap();
        fs::create_dir_all(tree.join("empty")).unwrap();
        fs::write(tree.join("a.txt"), b"alpha").unwrap();
        let big: Vec<u8> = (0..5_000_000u32).map(|i| (i * 7) as u8).collect();
        fs::write(tree.join("sub/deeper/big.bin"), &big).unwrap();
        let ro = tree.join("sub/ro.txt");
        fs::write(&ro, b"ro").unwrap();
        set_readonly(&ro).unwrap();
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
        File::options()
            .write(true)
            .open(tree.join("a.txt"))
            .unwrap()
            .set_modified(old)
            .unwrap();

        let r = run(
            OpKind::Copy,
            std::slice::from_ref(&tree),
            &f.dst,
            opts(ConflictPolicy::Ask),
        );
        assert!(r.is_clean(), "{r:?}");
        let out = f.dst.join("Фото 日本 📷");
        assert_eq!(fs::read(out.join("sub/deeper/big.bin")).unwrap(), big);
        assert!(out.join("empty").is_dir());
        assert_eq!(
            fs::metadata(out.join("a.txt")).unwrap().modified().unwrap(),
            old
        );
        assert!(
            fs::metadata(out.join("sub/ro.txt"))
                .unwrap()
                .permissions()
                .readonly()
        );
        assert!(no_partials(&f.dst));
        assert!(matches!(r.undo.as_slice(), [UndoRecord::Copied { .. }]));
        // Undo of a copy deletes the unchanged copy (incl. read-only files).
        undo(&r.undo[0]).unwrap();
        assert!(!out.exists());
        assert!(tree.join("a.txt").exists());
    }

    #[test]
    fn long_paths() {
        let f = fx();
        let mut deep = f.src.clone();
        while deep.as_os_str().len() < 400 {
            deep.push("a_rather_long_directory_name_for_testing");
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("file.txt"), b"deep").unwrap();
        let root = f.src.join("a_rather_long_directory_name_for_testing");
        let r = run(
            OpKind::Copy,
            std::slice::from_ref(&root),
            &f.dst,
            opts(ConflictPolicy::Ask),
        );
        assert!(r.is_clean(), "{r:?}");
        let rel = deep.strip_prefix(&f.src).unwrap();
        assert_eq!(fs::read(f.dst.join(rel).join("file.txt")).unwrap(), b"deep");
        assert!(f.dst.join(rel).join("file.txt").as_os_str().len() > MAX_TEST_PATH);
        let progress = Progress::default();
        let d = delete_permanently(&[root], &JobCtx::new(&CancelToken::new(), &progress));
        assert!(d.failed.is_empty(), "{d:?}");
    }
    const MAX_TEST_PATH: usize = paths::MAX_PATH;

    fn conflict_fx(policy: ConflictPolicy, src_newer: bool) -> (Fx, OpReport) {
        let f = fx();
        let s = f.src.join("x.txt");
        let d = f.dst.join("x.txt");
        fs::write(&s, b"new content").unwrap();
        fs::write(&d, b"old").unwrap();
        let t_old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        let t_new = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        let (ts, td) = if src_newer {
            (t_new, t_old)
        } else {
            (t_old, t_new)
        };
        File::options()
            .write(true)
            .open(&s)
            .unwrap()
            .set_modified(ts)
            .unwrap();
        File::options()
            .write(true)
            .open(&d)
            .unwrap()
            .set_modified(td)
            .unwrap();
        let r = run(OpKind::Copy, &[s], &f.dst, opts(policy));
        (f, r)
    }

    #[test]
    fn conflict_policies() {
        let (f, r) = conflict_fx(ConflictPolicy::Skip, true);
        assert_eq!(fs::read(f.dst.join("x.txt")).unwrap(), b"old");
        assert_eq!(r.skipped.len(), 1);

        let (f, r) = conflict_fx(ConflictPolicy::Overwrite, false);
        assert_eq!(fs::read(f.dst.join("x.txt")).unwrap(), b"new content");
        assert!(r.undo.is_empty(), "overwrites are not undoable");

        let (f, r) = conflict_fx(ConflictPolicy::OverwriteIfNewer, true);
        assert_eq!(fs::read(f.dst.join("x.txt")).unwrap(), b"new content");
        assert!(r.skipped.is_empty());
        let (f, r) = conflict_fx(ConflictPolicy::OverwriteIfNewer, false);
        assert_eq!(fs::read(f.dst.join("x.txt")).unwrap(), b"old");
        assert_eq!(r.skipped.len(), 1);

        let (f, r) = conflict_fx(ConflictPolicy::KeepBoth, true);
        assert_eq!(fs::read(f.dst.join("x.txt")).unwrap(), b"old");
        assert_eq!(fs::read(f.dst.join("x (2).txt")).unwrap(), b"new content");
        assert!(r.is_clean());

        // Ask + skip via SkipConflicts.
        let (f, _) = conflict_fx(ConflictPolicy::Ask, true);
        assert_eq!(fs::read(f.dst.join("x.txt")).unwrap(), b"old");
        assert!(no_partials(&f.dst));
    }

    #[test]
    fn replace_preserves_a_destination_changed_during_copy() {
        let f = fx();
        let source = f.src.join("replace.bin");
        let dest = f.dst.join("replace.bin");
        fs::write(&source, vec![b's'; COPY_CHUNK * 2]).unwrap();
        fs::write(&dest, b"version shown in Replace prompt").unwrap();
        let hook_dest = dest.clone();
        AFTER_CHUNK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_, bytes| {
                if bytes == COPY_CHUNK as u64 {
                    fs::write(&hook_dest, b"newer version saved during copy").unwrap();
                }
            }))
        });

        let report = run(
            OpKind::Copy,
            std::slice::from_ref(&source),
            &f.dst,
            opts(ConflictPolicy::Overwrite),
        );
        AFTER_CHUNK.with(|hook| *hook.borrow_mut() = None);

        assert_eq!(
            fs::read(&dest).unwrap(),
            b"newer version saved during copy",
            "the destination changed after the Replace decision must survive"
        );
        assert!(
            report.failed.iter().any(|issue| {
                issue.path == source
                    && issue
                        .reason
                        .contains("changed after you chose Replace; not overwritten")
            }),
            "{report:?}"
        );
        assert!(no_partials(&f.dst));

        let unchanged = fx();
        let source = unchanged.src.join("replace.bin");
        let dest = unchanged.dst.join("replace.bin");
        fs::write(&source, b"replacement bytes").unwrap();
        fs::write(&dest, b"old bytes").unwrap();
        let report = run(
            OpKind::Copy,
            std::slice::from_ref(&source),
            &unchanged.dst,
            opts(ConflictPolicy::Overwrite),
        );
        assert!(report.failed.is_empty(), "{report:?}");
        assert_eq!(fs::read(dest).unwrap(), b"replacement bytes");
    }

    #[test]
    fn same_volume_move_preserves_a_destination_changed_after_replace() {
        let f = fx();
        let source = f.src.join("move.bin");
        let dest = f.dst.join("move.bin");
        fs::write(&source, b"move source").unwrap();
        fs::write(&dest, b"version shown in Replace prompt").unwrap();
        let hook_dest = dest.clone();
        BEFORE_MOVE_RENAME.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_| {
                fs::write(&hook_dest, b"newer version saved before rename").unwrap();
            }))
        });

        let report = run(
            OpKind::Move,
            std::slice::from_ref(&source),
            &f.dst,
            opts(ConflictPolicy::Overwrite),
        );
        BEFORE_MOVE_RENAME.with(|hook| *hook.borrow_mut() = None);

        assert_eq!(
            fs::read(dest).unwrap(),
            b"newer version saved before rename"
        );
        assert!(source.exists(), "a refused move must keep its source");
        assert!(
            report.failed.iter().any(|issue| {
                issue
                    .reason
                    .contains("changed after you chose Replace; not overwritten")
            }),
            "{report:?}"
        );
    }

    #[test]
    fn replace_detects_a_same_size_rewrite_with_the_same_modified_time() {
        let f = fx();
        let source = f.src.join("replace.bin");
        let dest = f.dst.join("replace.bin");
        fs::write(&source, vec![b's'; COPY_CHUNK * 2]).unwrap();
        fs::write(&dest, b"version shown in Replace prompt").unwrap();
        let original_modified = fs::metadata(&dest).unwrap().modified().unwrap();
        let rewritten: &[u8] = b"newer version saved during copy";
        let hook_dest = dest.clone();
        AFTER_CHUNK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_, bytes| {
                if bytes == COPY_CHUNK as u64 {
                    // A save within the same clock tick: same size, same last-write time.
                    fs::write(&hook_dest, rewritten).unwrap();
                    let file = fs::OpenOptions::new().write(true).open(&hook_dest).unwrap();
                    file.set_modified(original_modified).unwrap();
                }
            }))
        });

        let report = run(
            OpKind::Copy,
            std::slice::from_ref(&source),
            &f.dst,
            opts(ConflictPolicy::Overwrite),
        );
        AFTER_CHUNK.with(|hook| *hook.borrow_mut() = None);

        let metadata = fs::metadata(&dest).unwrap();
        assert_eq!(metadata.len(), rewritten.len() as u64);
        assert_eq!(metadata.modified().unwrap(), original_modified);
        assert_eq!(fs::read(&dest).unwrap(), rewritten);
        assert!(
            report.failed.iter().any(|issue| {
                issue.path == source
                    && issue
                        .reason
                        .contains("changed after you chose Replace; not overwritten")
            }),
            "{report:?}"
        );
        assert!(no_partials(&f.dst));
    }

    #[test]
    fn move_replace_detects_a_same_size_rewrite_with_the_same_modified_time() {
        let f = fx();
        let source = f.src.join("move.bin");
        let dest = f.dst.join("move.bin");
        fs::write(&source, b"move source").unwrap();
        fs::write(&dest, b"version shown in Replace prompt").unwrap();
        let original_modified = fs::metadata(&dest).unwrap().modified().unwrap();
        let hook_dest = dest.clone();
        BEFORE_MOVE_RENAME.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_| {
                fs::write(&hook_dest, b"newer version saved during copy").unwrap();
                let file = fs::OpenOptions::new().write(true).open(&hook_dest).unwrap();
                file.set_modified(original_modified).unwrap();
            }))
        });

        let report = run(
            OpKind::Move,
            std::slice::from_ref(&source),
            &f.dst,
            opts(ConflictPolicy::Overwrite),
        );
        BEFORE_MOVE_RENAME.with(|hook| *hook.borrow_mut() = None);

        let metadata = fs::metadata(&dest).unwrap();
        assert_eq!(
            metadata.len(),
            b"newer version saved during copy".len() as u64
        );
        assert_eq!(metadata.modified().unwrap(), original_modified);
        assert_eq!(fs::read(&dest).unwrap(), b"newer version saved during copy");
        assert!(source.exists(), "a refused move must keep its source");
        assert!(
            report.failed.iter().any(|issue| {
                issue.path == source
                    && issue
                        .reason
                        .contains("changed after you chose Replace; not overwritten")
            }),
            "{report:?}"
        );
    }

    #[test]
    fn move_replace_keeps_a_save_that_lands_after_the_check() {
        let f = fx();
        let source = f.src.join("move.bin");
        let dest = f.dst.join("move.bin");
        fs::write(&source, b"move source").unwrap();
        fs::write(&dest, b"version shown in Replace prompt").unwrap();
        let hook_dest = dest.clone();
        AFTER_REPLACE_ASIDE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_| {
                // An editor's atomic save: write a temp file, then rename it over the target.
                let save = hook_dest.with_extension("tmp-save");
                fs::write(&save, b"editor save").unwrap();
                fs::rename(&save, &hook_dest).unwrap();
            }))
        });

        let report = run(
            OpKind::Move,
            std::slice::from_ref(&source),
            &f.dst,
            opts(ConflictPolicy::Overwrite),
        );
        AFTER_REPLACE_ASIDE.with(|hook| *hook.borrow_mut() = None);

        assert_eq!(fs::read(&dest).unwrap(), b"editor save");
        assert!(source.exists(), "a refused move must keep its source");
        assert!(
            report.failed.iter().any(|issue| {
                issue.path == source
                    && issue
                        .reason
                        .contains("destination changed after you chose Replace; not overwritten")
                    && issue.reason.contains("kept as")
            }),
            "{report:?}"
        );
        let leftovers = replaced_leftovers(&f.dst);
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        assert_eq!(
            fs::read(&leftovers[0]).unwrap(),
            b"version shown in Replace prompt"
        );
    }

    #[test]
    fn copy_replace_keeps_a_save_that_lands_after_the_check() {
        let f = fx();
        let source = f.src.join("replace.bin");
        let dest = f.dst.join("replace.bin");
        fs::write(&source, vec![b's'; COPY_CHUNK * 2]).unwrap();
        fs::write(&dest, b"version shown in Replace prompt").unwrap();
        let hook_dest = dest.clone();
        AFTER_REPLACE_ASIDE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_| {
                let save = hook_dest.with_extension("tmp-save");
                fs::write(&save, b"editor save").unwrap();
                fs::rename(&save, &hook_dest).unwrap();
            }))
        });

        let report = run(
            OpKind::Copy,
            std::slice::from_ref(&source),
            &f.dst,
            opts(ConflictPolicy::Overwrite),
        );
        AFTER_REPLACE_ASIDE.with(|hook| *hook.borrow_mut() = None);

        assert_eq!(fs::read(&dest).unwrap(), b"editor save");
        assert!(
            report.failed.iter().any(|issue| {
                issue.path == source
                    && issue
                        .reason
                        .contains("destination changed after you chose Replace; not overwritten")
                    && issue.reason.contains("kept as")
            }),
            "{report:?}"
        );
        let leftovers = replaced_leftovers(&f.dst);
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        assert_eq!(
            fs::read(&leftovers[0]).unwrap(),
            b"version shown in Replace prompt"
        );
        assert!(no_partials(&f.dst));
    }

    #[cfg(windows)]
    #[test]
    fn replace_denies_writers_while_publishing() {
        let f = fx();
        let source = f.src.join("replace.bin");
        let dest = f.dst.join("replace.bin");
        fs::write(&source, b"replacement bytes").unwrap();
        fs::write(&dest, b"version shown in Replace prompt").unwrap();
        AFTER_REPLACE_ASIDE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|dest| {
                // The moved-aside previous version is held open by the Replace: no writer.
                let aside = replaced_leftovers(dest.parent().unwrap());
                assert_eq!(aside.len(), 1, "{aside:?}");
                let err = OpenOptions::new()
                    .write(true)
                    .open(&aside[0])
                    .expect_err("the previous version must not be writable");
                assert_eq!(err.raw_os_error(), Some(ERROR_SHARING_VIOLATION), "{err}");
            }))
        });

        let report = run(
            OpKind::Copy,
            std::slice::from_ref(&source),
            &f.dst,
            opts(ConflictPolicy::Overwrite),
        );
        AFTER_REPLACE_ASIDE.with(|hook| *hook.borrow_mut() = None);

        assert!(report.failed.is_empty(), "{report:?}");
        assert_eq!(fs::read(&dest).unwrap(), b"replacement bytes");
        assert!(no_replaced_leftovers(&f.dst));
        assert!(no_partials(&f.dst));
    }

    #[test]
    fn replace_leaves_no_aside_file_after_success() {
        for kind in [OpKind::Copy, OpKind::Move] {
            let f = fx();
            let source = f.src.join("replace.bin");
            let dest = f.dst.join("replace.bin");
            fs::write(&source, b"replacement bytes").unwrap();
            fs::write(&dest, b"old bytes").unwrap();
            set_readonly(&dest).unwrap();

            let report = run(
                kind,
                std::slice::from_ref(&source),
                &f.dst,
                opts(ConflictPolicy::Overwrite),
            );

            assert!(report.failed.is_empty(), "{kind:?}: {report:?}");
            assert_eq!(fs::read(&dest).unwrap(), b"replacement bytes", "{kind:?}");
            assert!(no_replaced_leftovers(&f.dst), "{kind:?}");
        }
    }

    #[test]
    fn replace_state_fails_closed_when_the_hash_cannot_be_read() {
        let f = fx();
        let dest = f.dst.join("small.txt");
        fs::write(&dest, b"small destination").unwrap();
        let md = fs::symlink_metadata(&dest).unwrap();

        let captured = ReplaceState::capture(&dest, &md);
        assert!(matches!(captured.check, ContentCheck::Hash(_)));
        assert!(captured.still_matches(&dest));

        let unreadable = ReplaceState {
            kind: NodeKind::File,
            stamp: path_stamp(&dest).ok(),
            check: ContentCheck::Unreadable,
        };
        assert!(unreadable.stamp.is_some());
        assert!(!unreadable.still_matches(&dest));
    }

    #[cfg(windows)]
    #[test]
    fn replace_state_checks_link_identity() {
        // Junctions need no symlink privilege, and are links to the engine.
        let junction = |link: &Path, target: &Path| {
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|o| o.status.success())
        };
        let f = fx();
        let target_a = f.src.join("target_a");
        let target_b = f.src.join("target_b");
        fs::create_dir(&target_a).unwrap();
        fs::create_dir(&target_b).unwrap();
        let dest = f.dst.join("link");
        if !junction(&dest, &target_a) {
            eprintln!("mklink unavailable; skipping");
            return;
        }
        let md = fs::symlink_metadata(&dest).unwrap();
        let captured = ReplaceState::capture(&dest, &md);
        assert_eq!(captured.kind, NodeKind::Link);
        assert!(matches!(captured.check, ContentCheck::LinkTarget(_)));
        assert!(captured.still_matches(&dest));

        fs::remove_dir(&dest).unwrap();
        assert!(junction(&dest, &target_b));
        assert!(!captured.still_matches(&dest), "retargeted link");
    }

    #[test]
    fn replace_state_does_not_hash_online_only_placeholders() {
        let f = fx();
        let path = f.dst.join("placeholder.txt");
        fs::write(&path, b"cloud file").unwrap();
        crate::cloud::simulate_online_only(path.clone());
        let md = fs::symlink_metadata(&path).unwrap();

        let captured = ReplaceState::capture(&path, &md);
        assert_eq!(captured.check, ContentCheck::StampOnly);
        assert!(captured.stamp.is_some());
        assert!(captured.still_matches(&path));

        crate::cloud::clear_simulated_online_only(&path);
    }

    #[cfg(windows)]
    #[test]
    fn replace_refuses_a_destination_open_for_writing() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let shares = [
            FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0,
            // Without FILE_SHARE_DELETE the destination cannot even be renamed aside.
            FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0,
        ];
        for share in shares {
            let f = fx();
            let source = f.src.join("replace.bin");
            let dest = f.dst.join("replace.bin");
            fs::write(&source, vec![b's'; COPY_CHUNK * 2]).unwrap();
            fs::write(&dest, b"version shown in Replace prompt").unwrap();
            let mut writer = OpenOptions::new()
                .write(true)
                .share_mode(share)
                .open(&dest)
                .unwrap();

            let report = run(
                OpKind::Copy,
                std::slice::from_ref(&source),
                &f.dst,
                opts(ConflictPolicy::Overwrite),
            );

            assert!(
                report.failed.iter().any(|issue| {
                    issue.path == source
                        && issue
                            .reason
                            .contains("destination is in use; not overwritten")
                        && !issue.reason.contains("kept as")
                }),
                "{share}: {report:?}"
            );
            assert_eq!(
                fs::read(&dest).unwrap(),
                b"version shown in Replace prompt",
                "{share}"
            );
            assert!(no_replaced_leftovers(&f.dst), "{share}");
            assert!(no_partials(&f.dst), "{share}");
            writer.write_all(b"!").unwrap();
        }
    }

    #[test]
    fn replace_state_compares_the_full_identity_through_the_handle() {
        let f = fx();
        let a = f.dst.join("a.txt");
        let b = f.dst.join("b.txt");
        fs::write(&a, b"same bytes").unwrap();
        let md = fs::symlink_metadata(&a).unwrap();
        let state = ReplaceState::capture(&a, &md);
        let mtime = md.modified().unwrap();

        // Same bytes and time, different file: the file id differs.
        fs::write(&b, b"same bytes").unwrap();
        OpenOptions::new()
            .write(true)
            .open(&b)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert!(!state.matches_open(&open_denying_writers(&b).unwrap(), &b));
        assert!(state.matches_open(&open_denying_writers(&a).unwrap(), &a));

        // The same file, same length and time, different bytes.
        fs::write(&a, b"diff bytes").unwrap();
        OpenOptions::new()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert!(!state.matches_open(&open_denying_writers(&a).unwrap(), &a));
    }

    #[test]
    fn replace_state_does_not_hash_files_whose_find_data_says_online_only() {
        struct Attrs(Option<u32>);
        impl crate::cloud::AttributeSource for Attrs {
            fn attributes(&self, _: &Path) -> Option<u32> {
                self.0
            }
        }
        let f = fx();
        let path = f.dst.join("local.txt");
        fs::write(&path, b"local file").unwrap();
        let md = fs::symlink_metadata(&path).unwrap();

        let recall_on_open = Attrs(Some(crate::cloud::FILE_ATTRIBUTE_RECALL_ON_OPEN));
        let captured = ReplaceState::capture_with(&path, &md, &recall_on_open);
        assert_eq!(captured.check, ContentCheck::StampOnly);
        // Unreadable attributes fail closed: no hash either.
        let unknown = ReplaceState::capture_with(&path, &md, &Attrs(None));
        assert_eq!(unknown.check, ContentCheck::StampOnly);
        let local = ReplaceState::capture_with(&path, &md, &Attrs(Some(0x20)));
        assert!(matches!(local.check, ContentCheck::Hash(_)));
    }

    #[test]
    fn replace_keeps_a_destination_swapped_after_the_check() {
        let small = b"version shown in Replace prompt".to_vec();
        let large = vec![b'd'; REPLACE_HASH_LIMIT as usize + 1];
        for original in [small, large] {
            let f = fx();
            let source = f.src.join("replace.bin");
            let dest = f.dst.join("replace.bin");
            fs::write(&source, vec![b's'; COPY_CHUNK * 2]).unwrap();
            fs::write(&dest, &original).unwrap();
            let original_modified = fs::metadata(&dest).unwrap().modified().unwrap();
            let saved = vec![b'e'; original.len()];
            let hook_dest = dest.clone();
            let hook_saved = saved.clone();
            BEFORE_REPLACE_ASIDE.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |_| {
                    // An editor's save after the check: same size and time, new file id.
                    let save = hook_dest.with_extension("save");
                    fs::write(&save, &hook_saved).unwrap();
                    OpenOptions::new()
                        .write(true)
                        .open(&save)
                        .unwrap()
                        .set_modified(original_modified)
                        .unwrap();
                    // Our handle on the old version shares DELETE, so this replaces it.
                    fs::rename(&save, &hook_dest).unwrap();
                }))
            });

            let report = run(
                OpKind::Copy,
                std::slice::from_ref(&source),
                &f.dst,
                opts(ConflictPolicy::Overwrite),
            );
            BEFORE_REPLACE_ASIDE.with(|hook| *hook.borrow_mut() = None);

            assert_eq!(fs::read(&dest).unwrap(), saved, "{}", original.len());
            assert!(
                report.failed.iter().any(|issue| {
                    issue.path == source
                        && issue.reason.contains(
                            "destination changed after you chose Replace; not overwritten",
                        )
                }),
                "{report:?}"
            );
            assert!(no_replaced_leftovers(&f.dst), "{report:?}");
            assert!(no_partials(&f.dst));
        }
    }

    #[test]
    fn aside_names_fit_the_file_name_limit() {
        let units = |s: &str| s.encode_utf16().count();
        let long = format!("{}.jpg", "a".repeat(246));
        let name = aside_name(&long, "0123abcd");
        assert!(units(&name) <= 255, "{name}");
        assert!(name.contains(REPLACED_MARKER), "{name}");
        assert!(name.ends_with(".jpg"), "{name}");

        let emoji = format!("{}.png", "\u{1F600}".repeat(126));
        let name = aside_name(&emoji, "0123abcd");
        assert!(units(&name) <= 255, "{name}");
        assert!(name.contains(REPLACED_MARKER), "{name}");
        assert!(name.ends_with(".png"), "{name}");

        let short = aside_name("photo.jpg", "0123abcd");
        assert!(short.starts_with("photo.zxraw-replaced-"), "{short}");
        assert!(short.ends_with(".jpg"), "{short}");
    }

    #[test]
    fn replace_works_for_a_250_char_name() {
        let f = fx();
        let name = format!("{}.jpg", "a".repeat(246));
        let source = f.src.join(&name);
        let dest = f.dst.join(&name);
        fs::write(&source, b"replacement bytes").unwrap();
        fs::write(&dest, b"old bytes").unwrap();

        let report = run(
            OpKind::Copy,
            std::slice::from_ref(&source),
            &f.dst,
            opts(ConflictPolicy::Overwrite),
        );

        assert!(report.failed.is_empty(), "{report:?}");
        assert_eq!(fs::read(&dest).unwrap(), b"replacement bytes");
        assert!(no_replaced_leftovers(&f.dst));
    }

    #[cfg(windows)]
    #[test]
    fn replace_warns_when_the_previous_version_cannot_be_deleted() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let f = fx();
        let source = f.src.join("replace.bin");
        let dest = f.dst.join("replace.bin");
        fs::write(&source, b"replacement bytes").unwrap();
        fs::write(&dest, b"version shown in Replace prompt").unwrap();
        let held: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));
        let hook_held = Arc::clone(&held);
        BEFORE_ASIDE_DELETE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |aside| {
                // Hold the previous version open without FILE_SHARE_DELETE: it cannot be deleted.
                // The Replace's own handle is closed by now.
                let file = OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ.0)
                    .open(aside)
                    .unwrap();
                *hook_held.lock().unwrap() = Some(file);
            }))
        });

        let report = run(
            OpKind::Copy,
            std::slice::from_ref(&source),
            &f.dst,
            opts(ConflictPolicy::Overwrite),
        );
        BEFORE_ASIDE_DELETE.with(|hook| *hook.borrow_mut() = None);

        assert!(report.failed.is_empty(), "{report:?}");
        assert_eq!(fs::read(&dest).unwrap(), b"replacement bytes");
        assert_eq!(report.warnings.len(), 1, "{report:?}");
        assert!(report.warnings[0].reason.contains("kept as"), "{report:?}");
        let leftovers = replaced_leftovers(&f.dst);
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        assert_eq!(
            fs::read(&leftovers[0]).unwrap(),
            b"version shown in Replace prompt"
        );
        drop(held.lock().unwrap().take());
    }

    #[test]
    fn replace_detects_a_file_renamed_over_the_destination() {
        let f = fx();
        let source = f.src.join("replace.bin");
        let dest = f.dst.join("replace.bin");
        let original: &[u8] = b"version shown in Replace prompt";
        fs::write(&source, vec![b's'; COPY_CHUNK * 2]).unwrap();
        fs::write(&dest, original).unwrap();
        let original_modified = fs::metadata(&dest).unwrap().modified().unwrap();
        let hook_dest = dest.clone();
        let saving = f.dst.join("replace.bin.saving");
        AFTER_CHUNK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_, bytes| {
                if bytes == COPY_CHUNK as u64 {
                    // Same bytes and mtime as the original: only the file id differs.
                    fs::write(&saving, original).unwrap();
                    let file = fs::OpenOptions::new().write(true).open(&saving).unwrap();
                    file.set_modified(original_modified).unwrap();
                    drop(file);
                    fs::rename(&saving, &hook_dest).unwrap();
                }
            }))
        });

        let report = run(
            OpKind::Copy,
            std::slice::from_ref(&source),
            &f.dst,
            opts(ConflictPolicy::Overwrite),
        );
        AFTER_CHUNK.with(|hook| *hook.borrow_mut() = None);

        assert_eq!(fs::read(&dest).unwrap(), original);
        assert!(
            report.failed.iter().any(|issue| {
                issue.path == source
                    && issue
                        .reason
                        .contains("changed after you chose Replace; not overwritten")
            }),
            "{report:?}"
        );
        assert!(no_partials(&f.dst));
    }

    #[test]
    fn racing_destination_is_preserved_and_keep_both_retries() {
        let f = fx();
        let source = f.src.join("race.txt");
        fs::write(&source, b"new payload").unwrap();
        let raced_dest = f.dst.join("race.txt");
        let hook_dest = raced_dest.clone();
        AFTER_CHUNK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_, _| {
                if !hook_dest.exists() {
                    fs::write(&hook_dest, b"created during copy").unwrap();
                }
            }))
        });

        let report = run(
            OpKind::Copy,
            std::slice::from_ref(&source),
            &f.dst,
            opts(ConflictPolicy::KeepBoth),
        );
        AFTER_CHUNK.with(|hook| *hook.borrow_mut() = None);

        assert!(report.is_clean(), "{report:?}");
        assert_eq!(fs::read(&raced_dest).unwrap(), b"created during copy");
        assert_eq!(
            fs::read(f.dst.join("race (2).txt")).unwrap(),
            b"new payload"
        );
        assert!(no_partials(&f.dst));
    }

    #[test]
    fn racing_destination_without_keep_both_is_reported_and_preserved() {
        let f = fx();
        let source = f.src.join("race.txt");
        fs::write(&source, b"new payload").unwrap();
        let raced_dest = f.dst.join("race.txt");
        let hook_dest = raced_dest.clone();
        AFTER_CHUNK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_, _| {
                if !hook_dest.exists() {
                    fs::write(&hook_dest, b"created during copy").unwrap();
                }
            }))
        });

        let report = run(
            OpKind::Copy,
            std::slice::from_ref(&source),
            &f.dst,
            opts(ConflictPolicy::Skip),
        );
        AFTER_CHUNK.with(|hook| *hook.borrow_mut() = None);

        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert_eq!(fs::read(&raced_dest).unwrap(), b"created during copy");
        assert!(!f.dst.join("race (2).txt").exists());
        assert!(no_partials(&f.dst));
    }

    #[test]
    fn ask_resolver_apply_to_all_and_cancel() {
        let f = fx();
        for n in ["a", "b", "c"] {
            fs::write(f.src.join(n), b"src").unwrap();
            fs::write(f.dst.join(n), b"dst").unwrap();
        }
        let srcs: Vec<_> = ["a", "b", "c"].iter().map(|n| f.src.join(n)).collect();
        let asked = Mutex::new(0);
        let mut resolver = |_: &ConflictInfo| {
            *asked.lock().unwrap() += 1;
            Resolution::all(ConflictDecision::Overwrite)
        };
        let r = run_with(
            OpKind::Copy,
            &srcs,
            &f.dst,
            opts(ConflictPolicy::Ask),
            &mut resolver,
        );
        assert_eq!(*asked.lock().unwrap(), 1);
        assert_eq!(r.done.len(), 3);
        assert_eq!(fs::read(f.dst.join("c")).unwrap(), b"src");

        fs::write(f.src.join("a"), b"again").unwrap();
        let mut cancel = |_: &ConflictInfo| Resolution::once(ConflictDecision::Cancel);
        let r = run_with(
            OpKind::Copy,
            &srcs,
            &f.dst,
            opts(ConflictPolicy::Ask),
            &mut cancel,
        );
        assert!(r.cancelled);
        assert_eq!(fs::read(f.dst.join("a")).unwrap(), b"src");
    }

    #[test]
    fn copy_into_same_folder_keeps_both() {
        let f = fx();
        let s = f.src.join("doc.txt");
        fs::write(&s, b"1").unwrap();
        let r = run(
            OpKind::Copy,
            std::slice::from_ref(&s),
            &f.src,
            opts(ConflictPolicy::Ask),
        );
        assert!(r.is_clean());
        assert!(f.src.join("doc (2).txt").exists());
    }

    #[test]
    fn refuses_dir_into_itself() {
        let f = fx();
        let a = f.src.join("A");
        fs::create_dir_all(a.join("B")).unwrap();
        for k in [OpKind::Copy, OpKind::Move] {
            let c = CancelToken::new();
            assert!(matches!(
                plan(
                    k,
                    std::slice::from_ref(&a),
                    &a.join("B"),
                    OpOptions::default(),
                    &c
                ),
                Err(CoreError::Invalid(_))
            ));
            assert!(plan(k, std::slice::from_ref(&a), &a, OpOptions::default(), &c).is_err());
        }
        // A sibling with a common prefix is fine.
        fs::create_dir_all(f.src.join("AB")).unwrap();
        assert!(
            plan(
                OpKind::Copy,
                &[a],
                &f.src.join("AB"),
                OpOptions::default(),
                &CancelToken::new()
            )
            .is_ok()
        );
    }

    #[test]
    fn cancel_mid_copy_leaves_no_partial_files() {
        let f = fx();
        let s = f.src.join("huge.bin");
        fs::write(&s, vec![1u8; COPY_CHUNK * 4]).unwrap();
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let c2 = cancel.clone();
        AFTER_CHUNK.with(|h| *h.borrow_mut() = Some(Box::new(move |_, _| c2.cancel())));
        let p = plan(
            OpKind::Copy,
            std::slice::from_ref(&s),
            &f.dst,
            OpOptions::default(),
            &cancel,
        )
        .unwrap();
        let r = execute_with(&p, &JobCtx::new(&cancel, &progress), &mut SkipConflicts);
        AFTER_CHUNK.with(|h| *h.borrow_mut() = None);
        assert!(r.cancelled);
        assert!(progress.snapshot().bytes_done > 0);
        assert_eq!(
            fs::read_dir(&f.dst).unwrap().count(),
            0,
            "no partial or final file"
        );
        assert!(s.exists());
    }

    #[test]
    fn disconnect_mid_copy_cleans_partial_and_move_keeps_failed_source() {
        for kind in [OpKind::Copy, OpKind::Move] {
            let f = fx();
            let card = f.src.join("card");
            fs::create_dir_all(&card).unwrap();
            fs::write(card.join("a-first.bin"), b"done").unwrap();
            let large = vec![0x5a; COPY_CHUNK * 2];
            fs::write(card.join("b-large.bin"), &large).unwrap();
            fs::write(card.join("c-last.bin"), b"not reached").unwrap();

            let options = OpOptions {
                force_copy_for_move: kind == OpKind::Move,
                verify: true,
                ..Default::default()
            };
            let cancel = CancelToken::new();
            let progress = Progress::default();
            let plan = plan(kind, std::slice::from_ref(&card), &f.dst, options, &cancel).unwrap();
            crate::fault::arm_source_gone_after(&card, 8).unwrap();
            let report = execute_with(&plan, &JobCtx::new(&cancel, &progress), &mut SkipConflicts);
            crate::fault::disarm();

            let copied = f.dst.join("card");
            assert!(
                !report.cancelled,
                "disconnect is a reported I/O failure: {report:?}"
            );
            assert!(
                report
                    .failed
                    .iter()
                    .any(|issue| issue.path.ends_with("b-large.bin"))
            );
            assert_eq!(fs::read(copied.join("a-first.bin")).unwrap(), b"done");
            assert!(!copied.join("b-large.bin").exists());
            assert!(!copied.join("c-last.bin").exists());
            assert!(no_partials(&f.dst));
            assert_eq!(fs::read(card.join("b-large.bin")).unwrap(), large);
            assert_eq!(fs::read(card.join("c-last.bin")).unwrap(), b"not reached");
            if kind == OpKind::Move {
                assert!(!card.join("a-first.bin").exists());
                assert!(card.join("b-large.bin").exists());
            } else {
                assert_eq!(fs::read(card.join("a-first.bin")).unwrap(), b"done");
            }
        }
    }

    #[test]
    fn move_same_volume_rename_and_undo() {
        let f = fx();
        let d = f.src.join("folder");
        fs::create_dir_all(d.join("in")).unwrap();
        fs::write(d.join("in/f.txt"), b"x").unwrap();
        let r = run(
            OpKind::Move,
            std::slice::from_ref(&d),
            &f.dst,
            opts(ConflictPolicy::Ask),
        );
        assert!(r.is_clean());
        assert!(!d.exists());
        assert!(f.dst.join("folder/in/f.txt").exists());
        assert_eq!(r.undo.len(), 1);
        undo(&r.undo[0]).unwrap();
        assert!(d.join("in/f.txt").exists());
        assert!(!f.dst.join("folder").exists());
    }

    #[test]
    fn move_cross_volume_fallback() {
        let f = fx();
        let d = f.src.join("folder");
        fs::create_dir_all(d.join("in/empty")).unwrap();
        fs::write(d.join("in/f.txt"), b"payload").unwrap();
        fs::write(d.join("top.txt"), b"top").unwrap();
        let o = OpOptions {
            force_copy_for_move: true,
            ..opts(ConflictPolicy::Ask)
        };
        let r = run(OpKind::Move, std::slice::from_ref(&d), &f.dst, o);
        assert!(r.is_clean(), "{r:?}");
        assert!(!d.exists(), "source tree removed after verified copy");
        assert_eq!(fs::read(f.dst.join("folder/in/f.txt")).unwrap(), b"payload");
        assert!(f.dst.join("folder/in/empty").is_dir());
        assert!(
            r.undo.is_empty(),
            "cross-volume moves are not recorded as undoable"
        );
        assert!(no_partials(&f.dst));
    }

    #[test]
    fn move_merge_keeps_skipped_sources() {
        let f = fx();
        let d = f.src.join("m");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("keep.txt"), b"src").unwrap();
        fs::write(d.join("new.txt"), b"new").unwrap();
        fs::create_dir_all(f.dst.join("m")).unwrap();
        fs::write(f.dst.join("m/keep.txt"), b"dst").unwrap();
        let r = run(
            OpKind::Move,
            std::slice::from_ref(&d),
            &f.dst,
            opts(ConflictPolicy::Skip),
        );
        assert_eq!(r.skipped.len(), 1);
        assert!(d.join("keep.txt").exists(), "skipped source stays");
        assert!(!d.join("new.txt").exists());
        assert_eq!(fs::read(f.dst.join("m/new.txt")).unwrap(), b"new");
        assert_eq!(fs::read(f.dst.join("m/keep.txt")).unwrap(), b"dst");
    }

    #[test]
    fn source_vanishing_between_plan_and_execute() {
        let f = fx();
        let d = f.src.join("card");
        fs::create_dir_all(&d).unwrap();
        for i in 0..5 {
            fs::write(d.join(format!("{i}.jpg")), b"img").unwrap();
        }
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let ctx = JobCtx::new(&cancel, &progress);
        let p = plan(
            OpKind::Copy,
            std::slice::from_ref(&d),
            &f.dst,
            OpOptions::default(),
            &cancel,
        )
        .unwrap();
        fs::remove_file(d.join("2.jpg")).unwrap();
        let r = execute_with(&p, &ctx, &mut SkipConflicts);
        assert_eq!(r.failed.len(), 1);
        assert!(r.failed[0].reason.contains("no longer exists"));
        assert_eq!(r.done.len(), 4);

        // Whole source gone ("card pulled").
        let p = plan(
            OpKind::Copy,
            std::slice::from_ref(&d),
            &f.src,
            OpOptions::default(),
            &cancel,
        )
        .unwrap();
        fs::remove_dir_all(&d).unwrap();
        let r = execute_with(&p, &ctx, &mut SkipConflicts);
        assert_eq!(r.failed.len(), 1);
        assert!(!r.cancelled);

        // Root folder vanishes mid-operation after the first file.
        let d2 = f.src.join("card2");
        fs::create_dir_all(&d2).unwrap();
        for i in 0..5 {
            fs::write(d2.join(format!("{i}.jpg")), b"img").unwrap();
        }
        let p = plan(
            OpKind::Copy,
            std::slice::from_ref(&d2),
            &f.dst,
            OpOptions::default(),
            &cancel,
        )
        .unwrap();
        let d2c = d2.clone();
        AFTER_CHUNK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |_, _| {
                let _ = fs::remove_dir_all(&d2c);
            }))
        });
        let r = execute_with(&p, &ctx, &mut SkipConflicts);
        AFTER_CHUNK.with(|h| *h.borrow_mut() = None);
        assert!(r.done.len() <= 1, "{r:?}");
        assert!(!r.failed.is_empty());
        assert!(no_partials(&f.dst));
    }

    #[test]
    fn create_rename_delete() {
        let f = fx();
        let (dir, u1) = create_folder(&f.src, "New").unwrap();
        assert!(create_folder(&f.src, "New").is_err());
        assert!(create_folder(&f.src, "bad|name").is_err());
        let (dir2, _) = create_folder_unique(&f.src, "New").unwrap();
        assert_eq!(dir2, f.src.join("New (2)"));
        let (file, u2) = create_file(&dir, "a.txt").unwrap();
        assert!(create_file(&dir, "a.txt").is_err());

        // Rename to an existing name fails.
        create_file(&dir, "b.txt").unwrap();
        assert!(matches!(
            rename_entry(&file, "b.txt"),
            Err(CoreError::AlreadyExists(_))
        ));
        // Case-only rename works everywhere.
        let (renamed, u3) = rename_entry(&file, "A.TXT").unwrap();
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"A.TXT".to_string()), "{names:?}");
        assert!(!names.contains(&"a.txt".to_string()));
        undo(&u3).unwrap();
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"a.txt".to_string()));
        let _ = renamed;

        // Created-file undo refuses once content exists.
        fs::write(&file, b"data").unwrap();
        assert!(undo(&u2).is_err());
        fs::write(&file, b"").unwrap();
        undo(&u2).unwrap();
        // Created-folder undo refuses while not empty.
        assert!(undo(&u1).is_err());

        let progress = Progress::default();
        let r = delete_permanently(
            &[dir.clone(), dir2],
            &JobCtx::new(&CancelToken::new(), &progress),
        );
        assert!(r.failed.is_empty(), "{r:?}");
        assert!(!dir.exists());
        assert!(undo(&u1).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_not_followed() {
        let f = fx();
        let outside = f.dst.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("precious.txt"), b"keep").unwrap();
        let tree = f.src.join("t");
        fs::create_dir_all(&tree).unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("link")).unwrap();
        let p = plan(
            OpKind::Copy,
            &[tree.clone()],
            &f.dst,
            OpOptions::default(),
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(p.links, 1);
        assert_eq!(p.files, 0);
        let progress = Progress::default();
        let r = delete_permanently(&[tree], &JobCtx::new(&CancelToken::new(), &progress));
        assert!(r.failed.is_empty());
        assert!(outside.join("precious.txt").exists());
    }

    #[cfg(windows)]
    #[test]
    fn links_are_not_followed_windows() {
        // Junctions need no privilege; create one with mklink if available.
        let f = fx();
        let outside = f.dst.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("precious.txt"), b"keep").unwrap();
        let tree = f.src.join("t");
        fs::create_dir_all(&tree).unwrap();
        let junction = tree.join("junction");
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("mklink unavailable; skipping");
            return;
        }
        let p = plan(
            OpKind::Copy,
            std::slice::from_ref(&tree),
            &f.dst,
            OpOptions::default(),
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(p.links, 1);
        assert_eq!(p.files, 0, "junction target not scanned");
        let progress = Progress::default();
        let r = delete_permanently(
            std::slice::from_ref(&tree),
            &JobCtx::new(&CancelToken::new(), &progress),
        );
        assert!(r.failed.is_empty(), "{r:?}");
        assert!(!tree.exists());
        assert!(
            outside.join("precious.txt").exists(),
            "junction target untouched"
        );
    }

    #[test]
    fn write_stream_limit() {
        let f = fx();
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let ctx = JobCtx::new(&cancel, &progress);
        let data = vec![0u8; 1000];
        let o = StreamOptions {
            limit: Some(999),
            ..Default::default()
        };
        let r = write_stream_atomic(&data[..], Path::new("mem"), &f.dst.join("x"), o, &ctx);
        assert!(r.is_err());
        assert_eq!(fs::read_dir(&f.dst).unwrap().count(), 0);
        let r = copy_file_atomic(
            &f.src.join("missing"),
            &f.dst.join("y"),
            CopyFileOptions::default(),
            &ctx,
        );
        assert!(matches!(r, Err(CoreError::NotFound(_))));
    }

    #[test]
    fn guarded_copy_publishes_nothing_when_the_destination_changed_meanwhile() {
        let f = fx();
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let ctx = JobCtx::new(&cancel, &progress);
        let src = f.src.join("new.bin");
        let dest = f.dst.join("edited.bin");
        fs::write(&src, b"source bytes").unwrap();
        fs::write(&dest, b"the user's edit").unwrap();
        let opts = CopyFileOptions {
            overwrite: true,
            ..Default::default()
        };
        let r = copy_file_atomic_guarded(&src, &dest, opts, &ctx, &|| false);
        assert!(r.is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"the user's edit");
        assert_eq!(fs::read_dir(&f.dst).unwrap().count(), 1, "the temp file is removed");
        let r = copy_file_atomic_guarded(&src, &dest, opts, &ctx, &|| true);
        assert!(r.is_ok());
        assert_eq!(fs::read(&dest).unwrap(), b"source bytes");
    }
}
