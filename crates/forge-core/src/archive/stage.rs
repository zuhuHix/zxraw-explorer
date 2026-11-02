//! Archive entries staged for other apps: the data behind ZXraw's virtual files, when items
//! of an archive pane are dragged or pasted into Explorer, the desktop or a mail client.
//!
//! [`plan`] picks the files and folders of a selection with the same checks as [`extract`]
//! (safe names, the size and ratio limits, links skipped, Windows name collisions), without
//! touching the disk. A [`Stage`] then extracts them on first demand, in one pass in archive
//! order (each solid 7z block is decoded once), into a private temp folder, and each file is
//! read back through a [`StagedReader`] while it is written:
//!
//! * A file with a checksum of its own (zip, most 7z) is served as it is decoded. Its last
//!   read fails instead of ending when the checksum doesn't match, so the target reports an
//!   error rather than keeping a damaged copy.
//! * A file whose only checksum is its solid block's is served once the whole block decoded
//!   and matched.
//! * Under a password nothing proved (7z AES), encrypted files without any checksum are
//!   refused ([`UNCHECKED_REASON`]), as extraction holds them back.
//!
//! The temp folder is removed when the [`Stage`] is dropped (and old ones by
//! [`prune_temp`]).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::cell::RefCell;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::SystemTime;

use forge_jobs::{CancelToken, Progress};

use super::{
    ArchiveEntry, ArchiveListing, BLOCK_CHECK_FAILED, ExtractLimits, Password, PasswordProof,
    Selection, UNCHECKED_REASON, backend_for_file, collision_key, data_error, is_password_error,
    validate_entry_name,
};
use crate::error::{CoreError, Result};
use crate::ops::{JobCtx, OpIssue};

/// One file or folder of a [`StagePlan`], relative to the selection's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageItem {
    /// Validated relative path (no `..`, no absolute or reserved names).
    pub rel: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// For files: its position in [`StagePlan::files`] (what [`Stage::open`] takes).
    pub file: Option<usize>,
}

/// What a selection offers: folders first (parents before children), then files in archive
/// order.
#[derive(Debug, Clone, Default)]
pub struct StagePlan {
    pub items: Vec<StageItem>,
    /// Links and names that collide on Windows, left out as extraction leaves them out.
    pub skipped: Vec<OpIssue>,
    /// The file entries, in the order [`StageItem::file`] counts them.
    pub files: Vec<ArchiveEntry>,
    /// Names of the folder entries the selection covers (folders with an entry of their own).
    pub folder_entries: Vec<String>,
    /// Names of the covered entries left out (those in `skipped`): a move leaves them, and
    /// the folders holding them, in the archive.
    pub left_out: Vec<String>,
}

impl StagePlan {
    /// The exact entry names a confirmed move of every planned file removes: each file, and
    /// each folder entry nothing left out sits under. Names carry no trailing `/`, so an
    /// archive edit deletes just that entry, never a folder with whatever it holds by then.
    ///
    /// Check them with [`unambiguous_names`] against the archive before deleting: a name
    /// another entry shares (`A.txt` next to `a.txt`) would delete that one too.
    pub fn move_names(&self) -> Vec<String> {
        let left_out: Vec<String> = self.left_out.iter().map(|n| n.to_lowercase()).collect();
        let mut names: Vec<String> = self
            .files
            .iter()
            .map(|e| e.name.trim_end_matches('/').to_string())
            .collect();
        names.extend(self.folder_entries.iter().filter_map(|folder| {
            let bare = folder.trim_end_matches('/');
            let prefix = format!("{}/", bare.to_lowercase());
            (!left_out.iter().any(|n| n.starts_with(&prefix))).then(|| bare.to_string())
        }));
        names
    }

    /// Whether any planned file is encrypted (it needs the archive's password).
    pub fn any_encrypted(&self) -> bool {
        self.files.iter().any(|e| e.encrypted)
    }
}

/// `names` (to delete) without those more than one entry of `listing` answers to under an
/// archive edit's rule: a name without a trailing `/` deletes every entry with that name,
/// ignoring case, a file or a folder. Returns the rest, and how many were dropped. Works
/// for any backend: it reads the listing only.
pub fn unambiguous_names(listing: &ArchiveListing, names: &[String]) -> (Vec<String>, usize) {
    let mut count: HashMap<String, usize> = HashMap::new();
    for e in &listing.entries {
        *count
            .entry(e.name.trim_end_matches('/').to_lowercase())
            .or_default() += 1;
    }
    let (keep, dropped): (Vec<&String>, Vec<&String>) = names.iter().partition(|n| {
        count
            .get(&n.trim_end_matches('/').to_lowercase())
            .is_none_or(|&c| c <= 1)
    });
    (keep.into_iter().cloned().collect(), dropped.len())
}

/// Whether two entries of `listing` share a name ignoring case and a trailing `/` (`A.txt`
/// and `a.txt`, a file `docs` and a folder `docs/`). Rewriting such an archive keeps only the
/// first of them, so a move's removal refuses it (see `move_names`).
pub fn has_shared_names(listing: &ArchiveListing) -> bool {
    let mut seen = HashSet::new();
    !listing
        .entries
        .iter()
        .all(|e| seen.insert(e.name.trim_end_matches('/').to_lowercase()))
}

/// Why [`plan`] (and [`extract`]) leave out a link entry.
pub const LEFT_OUT_LINK: &str = "symbolic link entries are not extracted";
/// Why a file named like a folder is left out.
pub const LEFT_OUT_LIKE_FOLDER: &str = "a folder in the archive has the same name";
/// Why a file named like an earlier one is left out.
pub const LEFT_OUT_SAME_NAME: &str =
    "another entry has the same name (names are not case-sensitive on Windows)";

/// Whether `reason` is one of the rules that always leave an entry out (not a conflict, an
/// error or a cancel).
pub fn is_left_out_reason(reason: &str) -> bool {
    [LEFT_OUT_LINK, LEFT_OUT_LIKE_FOLDER, LEFT_OUT_SAME_NAME].contains(&reason)
}

/// The files and folders `selection` covers in `listing`, checked as [`extract`] checks
/// them: an unsafe name anywhere in the archive or an entry over the limits refuses it all.
pub fn plan(
    listing: &ArchiveListing,
    selection: &Selection,
    limits: &ExtractLimits,
) -> Result<StagePlan> {
    let mut plan = StagePlan::default();
    let mut archive_total: u64 = 0;
    let mut selected_total: u64 = 0;
    // (relative path, entry) of every covered entry, folders included.
    let mut planned: Vec<(PathBuf, &ArchiveEntry)> = Vec::new();
    for e in &listing.entries {
        if e.name.trim_end_matches('/').is_empty() && e.is_dir {
            continue;
        }
        validate_entry_name(&e.name)?;
        if !e.is_dir && e.size > 1024 * 1024 && e.size / e.compressed_size.max(1) > limits.max_ratio
        {
            return Err(CoreError::UnsafePath(format!(
                "{}: suspicious compression ratio (zip bomb)",
                e.name
            )));
        }
        if !e.is_dir && e.size > limits.max_file_bytes {
            return Err(CoreError::Invalid(format!(
                "{}: expands to more than {} bytes",
                e.name, limits.max_file_bytes
            )));
        }
        archive_total = archive_total.saturating_add(e.size);
        if archive_total > limits.max_total_bytes {
            return Err(CoreError::Invalid(format!(
                "archive expands to more than {} bytes",
                limits.max_total_bytes
            )));
        }
        if selection.covering(&e.name).is_none() {
            continue;
        }
        let rel = match selection.relative(&e.name).trim_end_matches('/') {
            "" => continue, // the base folder itself
            rest => validate_entry_name(rest)?,
        };
        if e.is_symlink {
            plan.skipped.push(OpIssue::new(&rel, LEFT_OUT_LINK));
            plan.left_out.push(e.name.clone());
            continue;
        }
        if !e.is_dir {
            selected_total = selected_total.saturating_add(e.size);
        }
        planned.push((rel, e));
    }
    if selected_total > limits.max_total_bytes {
        return Err(CoreError::Invalid(format!(
            "the selection expands to more than {} bytes",
            limits.max_total_bytes
        )));
    }

    // Every folder: those with an entry, and those only implied by a file's path. The first
    // spelling of a name wins (names are not case-sensitive on Windows).
    let mut folders: Vec<(PathBuf, Option<SystemTime>)> = Vec::new();
    let mut folder_at: HashMap<String, usize> = HashMap::new();
    let mut add_folder = |rel: &Path, modified: Option<SystemTime>| {
        let key = collision_key(rel);
        match folder_at.get(&key) {
            Some(&i) => {
                if folders[i].1.is_none() {
                    folders[i].1 = modified;
                }
            }
            None => {
                folder_at.insert(key, folders.len());
                folders.push((rel.to_path_buf(), modified));
            }
        }
    };
    for (rel, e) in &planned {
        let parent = if e.is_dir {
            plan.folder_entries.push(e.name.clone());
            add_folder(rel, e.modified);
            rel.parent()
        } else {
            rel.parent()
        };
        for a in parent.into_iter().flat_map(Path::ancestors) {
            if !a.as_os_str().is_empty() {
                add_folder(a, None);
            }
        }
    }
    // A file named like a folder, or like an earlier file, is left out (extraction's rule).
    let mut seen: HashSet<String> = HashSet::new();
    let mut files: Vec<(PathBuf, &ArchiveEntry)> = Vec::new();
    for (rel, e) in planned.into_iter().filter(|(_, e)| !e.is_dir) {
        let key = collision_key(&rel);
        let reason = if folder_at.contains_key(&key) {
            LEFT_OUT_LIKE_FOLDER
        } else if !seen.insert(key) {
            LEFT_OUT_SAME_NAME
        } else {
            files.push((rel, e));
            continue;
        };
        plan.skipped.push(OpIssue::new(&rel, reason));
        plan.left_out.push(e.name.clone());
    }
    folders.sort_by_key(|(rel, _)| rel.components().count());
    plan.items.extend(folders.into_iter().map(|(rel, modified)| StageItem {
        rel,
        is_dir: true,
        size: 0,
        modified,
        file: None,
    }));
    for (k, (rel, e)) in files.into_iter().enumerate() {
        plan.items.push(StageItem {
            rel,
            is_dir: false,
            size: e.size,
            modified: e.modified,
            file: Some(k),
        });
        plan.files.push(e.clone());
    }
    Ok(plan)
}

/// What a [`Stage`] tells its owner (called on its worker thread).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageEvent {
    /// The archive's password is missing (`wrong: false`) or wrong: nothing was served.
    Password { wrong: bool },
    /// The stage is done. `failed` lists the files that were refused or failed (their
    /// streams returned an error), `unchecked` the names of those refused for lack of a
    /// checksum under an unproven password (also in `failed`).
    Finished {
        failed: Vec<OpIssue>,
        unchecked: Vec<String>,
    },
}

/// Free space wanted on the temp volume beyond the staged files.
const SPACE_SLACK: u64 = 64 << 20;
const CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone)]
enum FileState {
    Pending,
    /// Bytes written so far.
    Writing(u64),
    /// Written in full, waiting for its solid block's checksum.
    Held(u64),
    Done(u64),
    Failed(String),
}

#[derive(Default)]
struct State {
    files: Vec<FileState>,
    started: bool,
    finished: bool,
    /// The [`Stage`] was dropped: whoever finishes last removes the folder.
    owner_gone: bool,
}

type Events = Box<dyn Fn(StageEvent) + Send + Sync>;
/// What [`Stage::new`]'s `hold` returns while the archive may be read: kept until the stage
/// has finished reading it.
pub type ArchiveHold = Box<dyn Send>;
type Hold = Box<dyn Fn() -> Option<ArchiveHold> + Send + Sync>;

struct Inner {
    archive: PathBuf,
    files: Vec<ArchiveEntry>,
    password: Option<Password>,
    limits: ExtractLimits,
    dir: PathBuf,
    hold: Hold,
    events: Events,
    cancel: CancelToken,
    state: Mutex<State>,
    changed: Condvar,
}

/// The planned files of one archive, extracted on first demand into `dir`. Dropping it
/// cancels the extraction and removes the folder.
pub struct Stage {
    inner: Arc<Inner>,
}

impl Stage {
    /// Nothing is read or written until the first [`open`](Self::open). `hold` is called
    /// first (off the caller's thread): `None` when the archive changed since it was listed
    /// (every file fails), else a guard kept until the stage stopped reading the archive (a
    /// handle that keeps writers out, say). `dir` must be a new folder only this stage uses.
    pub fn new(
        archive: PathBuf,
        files: Vec<ArchiveEntry>,
        password: Option<Password>,
        limits: ExtractLimits,
        dir: PathBuf,
        hold: impl Fn() -> Option<ArchiveHold> + Send + Sync + 'static,
        events: impl Fn(StageEvent) + Send + Sync + 'static,
    ) -> Self {
        let n = files.len();
        Self {
            inner: Arc::new(Inner {
                archive,
                files,
                password,
                limits,
                dir,
                hold: Box::new(hold),
                events: Box::new(events),
                cancel: CancelToken::new(),
                state: Mutex::new(State {
                    files: vec![FileState::Pending; n],
                    ..State::default()
                }),
                changed: Condvar::new(),
            }),
        }
    }

    /// A reader of file `k` (of [`StagePlan::files`]). Starts the extraction on its first
    /// call; reads wait for the data.
    pub fn open(&self, k: usize) -> io::Result<StagedReader> {
        let entry = self.inner.files.get(k).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("there is no staged file {k}"))
        })?;
        let mut state = self.inner.lock();
        if !state.started {
            state.started = true;
            let inner = self.inner.clone();
            let spawned = std::thread::Builder::new()
                .name("archive-stage".into())
                .spawn(move || inner.run());
            if let Err(e) = spawned {
                state.finished = true;
                for f in &mut state.files {
                    *f = FileState::Failed(format!("could not start extracting: {e}"));
                }
            }
        }
        Ok(StagedReader {
            inner: self.inner.clone(),
            k,
            wait_for_block: entry.checked_by_block,
            file: None,
            pos: 0,
        })
    }

    /// The folder the files are written to.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let running = {
            let mut state = self.inner.lock();
            state.owner_gone = true;
            state.started && !state.finished
        };
        self.inner.cancel.cancel();
        self.inner.changed.notify_all();
        if !running {
            // The last release may come on the UI thread: a big folder is removed off it.
            let dir = self.inner.dir.clone();
            let spawned = std::thread::Builder::new()
                .name("archive-stage-cleanup".into())
                .spawn(move || {
                    let _ = fs::remove_dir_all(dir);
                });
            if spawned.is_err() {
                let _ = fs::remove_dir_all(&self.inner.dir);
            }
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn set(&self, k: usize, to: FileState) {
        self.lock().files[k] = to;
        self.changed.notify_all();
    }

    fn file_path(&self, k: usize) -> PathBuf {
        self.dir.join(format!("{k}.bin"))
    }

    fn run(self: Arc<Self>) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.extract()))
            .unwrap_or_else(|_| Err(CoreError::Invalid("extraction stopped unexpectedly".into())));
        let mut failed = Vec::new();
        let gone = {
            let mut state = self.lock();
            let reason = match &result {
                Ok(()) => "the archive no longer holds it".to_string(),
                Err(e) => e.to_string(),
            };
            for (k, f) in state.files.iter_mut().enumerate() {
                match f {
                    FileState::Done(_) => {}
                    FileState::Failed(r) => {
                        failed.push(OpIssue::new(&self.files[k].name, r.clone()));
                    }
                    _ => {
                        failed.push(OpIssue::new(&self.files[k].name, reason.clone()));
                        *f = FileState::Failed(reason.clone());
                    }
                }
            }
            state.finished = true;
            state.owner_gone
        };
        self.changed.notify_all();
        if gone {
            let _ = fs::remove_dir_all(&self.dir);
        }
        match result {
            Err(CoreError::Cancelled) => {}
            Err(e) if is_password_error(&e) => (self.events)(StageEvent::Password {
                wrong: matches!(e, CoreError::WrongPassword(_)),
            }),
            _ => {
                let unchecked = failed
                    .iter()
                    .filter(|f| f.reason == UNCHECKED_REASON)
                    .map(|f| f.path.to_string_lossy().into_owned())
                    .collect();
                (self.events)(StageEvent::Finished { failed, unchecked });
            }
        }
    }

    fn extract(&self) -> Result<()> {
        self.cancel.check()?;
        // Held through every read of the archive below.
        let Some(_hold) = (self.hold)() else {
            return Err(CoreError::Invalid(
                "the archive changed since it was listed; open it again".into(),
            ));
        };
        let backend = backend_for_file(&self.archive)?;
        let indices: Vec<usize> = self.files.iter().map(|e| e.index).collect();
        let password = self.password.as_ref();
        backend.preflight(&self.archive, &indices, password, &self.limits)?;
        let refs: Vec<&ArchiveEntry> = self.files.iter().collect();
        let proof = backend.check_password(&self.archive, &refs, password)?;
        let mut wanted: HashMap<usize, usize> = HashMap::new();
        let mut need: u64 = 0;
        for (k, e) in self.files.iter().enumerate() {
            if proof == PasswordProof::Unverified && e.encrypted && e.unchecked {
                self.set(k, FileState::Failed(UNCHECKED_REASON.into()));
            } else if e.size == 0 && !e.checked_by_block {
                // Nothing to decode or check (an empty file stores no data).
                self.set(k, FileState::Done(0));
                wanted.insert(e.index, k);
            } else {
                wanted.insert(e.index, k);
                need = need.saturating_add(e.size);
            }
        }
        fs::create_dir_all(&self.dir).map_err(|e| CoreError::io(&self.dir, e))?;
        let free = crate::split::free_space_available(&self.dir)
            .map_err(|e| CoreError::io(&self.dir, e))?;
        if need.saturating_add(SPACE_SLACK) > free {
            return Err(CoreError::Invalid(format!(
                "not enough free space in the temp folder ({} MB needed, {} MB free)",
                need.div_ceil(1 << 20),
                free >> 20
            )));
        }
        let progress = Progress::default();
        let ctx = JobCtx::new(&self.cancel, &progress);
        let mut remaining = self.limits.max_total_bytes;
        let mut buf = vec![0u8; CHUNK];
        // Files written in full that wait for their block's checksum.
        let held: RefCell<Vec<usize>> = RefCell::new(Vec::new());
        let mut visit = |entry: &ArchiveEntry, reader: Result<&mut dyn Read>| -> Result<()> {
            self.cancel.check()?;
            let Some(&k) = wanted.get(&entry.index) else {
                return Ok(());
            };
            if matches!(self.lock().files[k], FileState::Done(_)) {
                return Ok(());
            }
            let reader = match reader {
                Ok(r) => r,
                Err(e) if is_password_error(&e) => return Err(e),
                Err(e) => {
                    self.set(k, FileState::Failed(data_error(entry, e)));
                    return Ok(());
                }
            };
            // Never trust the declared size beyond the limits.
            let limit = entry.size.min(self.limits.max_file_bytes).min(remaining);
            match self.write_one(k, reader, limit, entry.size, &mut buf) {
                Ok(n) => {
                    remaining = remaining.saturating_sub(n);
                    if entry.checked_by_block {
                        held.borrow_mut().push(k);
                        self.set(k, FileState::Held(n));
                    } else {
                        self.set(k, FileState::Done(n));
                    }
                }
                Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                Err(e) => self.set(k, FileState::Failed(data_error(entry, e))),
            }
            Ok(())
        };
        let mut block_end = |outcome: Result<()>| -> Result<()> {
            for k in std::mem::take(&mut *held.borrow_mut()) {
                let now = self.lock().files[k].clone();
                let to = match (&outcome, now) {
                    (Ok(()), FileState::Held(n)) => FileState::Done(n),
                    (Ok(()), other) => other,
                    (Err(_), _) => FileState::Failed(BLOCK_CHECK_FAILED.into()),
                };
                self.set(k, to);
            }
            Ok(())
        };
        let is_wanted = |e: &ArchiveEntry| wanted.contains_key(&e.index);
        backend.read_entries_checked(
            &self.archive,
            &is_wanted,
            password,
            Some(&ctx),
            &mut visit,
            &mut block_end,
        )
    }

    /// Writes file `k` from `reader`, publishing its progress. Fails when the data is
    /// longer than `limit` or differs from the `size` the archive lists.
    fn write_one(
        &self,
        k: usize,
        reader: &mut dyn Read,
        limit: u64,
        size: u64,
        buf: &mut [u8],
    ) -> Result<u64> {
        let path = self.file_path(k);
        let mut out = File::create(&path).map_err(|e| CoreError::io(&path, e))?;
        self.set(k, FileState::Writing(0));
        let mut total: u64 = 0;
        loop {
            self.cancel.check()?;
            let n = match reader.read(buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(CoreError::io(Path::new(&self.files[k].name), e)),
            };
            total += n as u64;
            if total > limit {
                return Err(CoreError::Invalid(format!(
                    "{}: more data than the archive lists, or over the size limit",
                    self.files[k].name
                )));
            }
            out.write_all(&buf[..n])
                .map_err(|e| CoreError::io(&path, e))?;
            self.set(k, FileState::Writing(total));
        }
        if total != size {
            return Err(CoreError::Invalid(format!(
                "{}: the data ends before the size the archive lists",
                self.files[k].name
            )));
        }
        Ok(total)
    }
}

/// Reads one staged file as it is written (see the module docs). Its last read fails when
/// the file failed its checks; reads wait while its data is not there yet.
pub struct StagedReader {
    inner: Arc<Inner>,
    k: usize,
    /// Its only checksum is its block's: nothing is served before the block matched.
    wait_for_block: bool,
    file: Option<File>,
    pos: u64,
}

impl StagedReader {
    /// The size the archive lists for the file.
    pub fn size(&self) -> u64 {
        self.inner.files[self.k].size
    }

    /// Bytes ready to read past `pos`, or the end (`Ok(None)`), waiting until either.
    fn ready(&self) -> io::Result<Option<u64>> {
        let mut state = self.inner.lock();
        loop {
            if self.inner.cancel.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let available = match &state.files[self.k] {
                FileState::Failed(reason) => return Err(io::Error::other(reason.clone())),
                FileState::Done(n) if *n <= self.pos => return Ok(None),
                FileState::Done(n) => Some(*n),
                FileState::Writing(n) | FileState::Held(n)
                    if !self.wait_for_block && *n > self.pos =>
                {
                    Some(*n)
                }
                _ => None,
            };
            if let Some(n) = available {
                return Ok(Some(n - self.pos));
            }
            state = self
                .inner
                .changed
                .wait(state)
                .unwrap_or_else(|p| p.into_inner());
        }
    }
}

impl Read for StagedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let Some(available) = self.ready()? else {
            return Ok(0);
        };
        if self.file.is_none() {
            let mut file = File::open(self.inner.file_path(self.k))?;
            file.seek(SeekFrom::Start(self.pos))?;
            self.file = Some(file);
        }
        let want = buf.len().min(usize::try_from(available).unwrap_or(usize::MAX));
        let file = self.file.as_mut().expect("opened above");
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the staged file is shorter than written",
            ));
        }
        self.pos += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{CreateOptions, Encryption, Preset, create, list};
    use std::time::{Duration, Instant};
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    type Log = Arc<Mutex<Vec<StageEvent>>>;

    /// A path from `/`-separated parts, in this platform's form.
    fn p(path: &str) -> PathBuf {
        path.split('/').collect()
    }

    /// Writes a zip of `entries` (name, data), each stored with `method`.
    fn write_zip(path: &Path, method: CompressionMethod, entries: &[(&str, &[u8])]) {
        let mut zw = ZipWriter::new(File::create(path).unwrap());
        for (name, data) in entries {
            zw.start_file(*name, SimpleFileOptions::default().compression_method(method))
                .unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }

    /// The items of `staged` as (relative path, is folder, size).
    fn shape(staged: &StagePlan) -> Vec<(PathBuf, bool, u64)> {
        staged
            .items
            .iter()
            .map(|i| (i.rel.clone(), i.is_dir, i.size))
            .collect()
    }

    /// A stage over `staged`'s files of `archive`, logging its events.
    fn stage_of(
        archive: &Path,
        staged: &StagePlan,
        dir: PathBuf,
        password: Option<Password>,
    ) -> (Stage, Log) {
        let log: Log = Arc::default();
        let events = log.clone();
        let stage = Stage::new(
            archive.to_path_buf(),
            staged.files.clone(),
            password,
            ExtractLimits::default(),
            dir,
            || Some(Box::new(()) as ArchiveHold),
            move |e| events.lock().unwrap().push(e),
        );
        (stage, log)
    }

    fn read_file(stage: &Stage, k: usize) -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        stage.open(k)?.read_to_end(&mut data)?;
        Ok(data)
    }

    /// The `Finished` event's (failed, unchecked), once it has come.
    fn finished(log: &Log) -> Option<(Vec<OpIssue>, Vec<String>)> {
        log.lock().unwrap().iter().find_map(|e| match e {
            StageEvent::Finished { failed, unchecked } => Some((failed.clone(), unchecked.clone())),
            _ => None,
        })
    }

    /// Polls for up to 2 s: the stage's worker may still be finishing.
    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_zip_selection_streams_every_file_with_its_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("a.zip");
        write_zip(
            &archive,
            CompressionMethod::Deflated,
            &[
                ("docs/a.txt", &b"alpha"[..]),
                ("docs/sub/b.txt", &b"bravo bravo"[..]),
                ("top.txt", &b"not selected"[..]),
            ],
        );
        let staged = plan(
            &list(&archive).unwrap(),
            &Selection::new("", vec!["docs/".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        assert_eq!(
            shape(&staged),
            vec![
                (p("docs"), true, 0),
                (p("docs/sub"), true, 0),
                (p("docs/a.txt"), false, 5),
                (p("docs/sub/b.txt"), false, 11),
            ]
        );

        let dir = tmp.path().join("stage");
        let (stage, log) = stage_of(&archive, &staged, dir.clone(), None);
        assert_eq!(read_file(&stage, 0).unwrap(), b"alpha".to_vec());
        assert_eq!(read_file(&stage, 1).unwrap(), b"bravo bravo".to_vec());
        wait_until("the finished event", || finished(&log).is_some());
        assert!(finished(&log).unwrap().0.is_empty());
        assert!(dir.exists());

        drop(stage);
        wait_until("the stage folder to be removed", || !dir.exists());
    }

    #[test]
    fn a_selection_under_a_base_folder_is_relative_to_it() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("base.zip");
        write_zip(
            &archive,
            CompressionMethod::Deflated,
            &[
                ("docs/a.txt", &b"alpha"[..]),
                ("docs/sub/b.txt", &b"bravo bravo"[..]),
                ("top.txt", &b"not selected"[..]),
            ],
        );
        let staged = plan(
            &list(&archive).unwrap(),
            &Selection::new("docs/", vec!["docs/sub/".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        assert_eq!(
            shape(&staged),
            vec![(p("sub"), true, 0), (p("sub/b.txt"), false, 11)]
        );
        let (stage, _log) = stage_of(&archive, &staged, tmp.path().join("stage"), None);
        assert_eq!(read_file(&stage, 0).unwrap(), b"bravo bravo".to_vec());
    }

    #[test]
    fn a_large_file_reads_back_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("large.zip");
        // xorshift64: incompressible, so the compression ratio check stays out of it.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let data: Vec<u8> = (0..3 * 1024 * 1024)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        write_zip(&archive, CompressionMethod::Deflated, &[("big.bin", &data[..])]);
        let staged = plan(
            &list(&archive).unwrap(),
            &Selection::new("", vec!["big.bin".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        assert_eq!(staged.files[0].size, data.len() as u64);
        let (stage, log) = stage_of(&archive, &staged, tmp.path().join("stage"), None);
        let got = read_file(&stage, 0).unwrap();
        assert!(got == data, "the staged copy differs from the original");
        wait_until("the finished event", || finished(&log).is_some());
        assert!(finished(&log).unwrap().0.is_empty());
    }

    #[test]
    fn a_damaged_stored_file_fails_its_last_read_and_is_listed_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("damaged.zip");
        let probe = b"STORED-PROBE-0123456789-abcdefghijklmnop".to_vec();
        write_zip(
            &archive,
            CompressionMethod::Stored,
            &[("good.txt", &b"fine"[..]), ("damaged.txt", &probe[..])],
        );
        // Flip one byte of the stored data in the file itself.
        let mut bytes = fs::read(&archive).unwrap();
        let at = bytes
            .windows(probe.len())
            .position(|w| w == probe.as_slice())
            .expect("the stored data is in the archive as it is");
        bytes[at + probe.len() / 2] ^= 0x55;
        fs::write(&archive, &bytes).unwrap();

        let staged = plan(
            &list(&archive).unwrap(),
            &Selection::new("", vec!["good.txt".into(), "damaged.txt".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        let good = staged.files.iter().position(|e| e.name == "good.txt").unwrap();
        let bad = staged
            .files
            .iter()
            .position(|e| e.name == "damaged.txt")
            .unwrap();
        let (stage, log) = stage_of(&archive, &staged, tmp.path().join("stage"), None);
        assert_eq!(read_file(&stage, good).unwrap(), b"fine".to_vec());
        assert!(
            read_file(&stage, bad).is_err(),
            "a damaged file must not read to its end"
        );
        wait_until("the finished event", || finished(&log).is_some());
        let (failed, _) = finished(&log).unwrap();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].path, p("damaged.txt"));
    }

    #[test]
    fn a_solid_7z_selection_reads_back_every_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        for i in 0..6 {
            fs::write(
                src.join(format!("f{i}.txt")),
                format!("file {i} ").repeat(50 + i * 7),
            )
            .unwrap();
        }
        let archive = tmp.path().join("solid.7z");
        let opts = CreateOptions {
            preset: Preset::Fast,
            solid: true,
            ..Default::default()
        };
        let (c, pr) = (CancelToken::new(), Progress::default());
        create(
            std::slice::from_ref(&src),
            &archive,
            &opts,
            &JobCtx::new(&c, &pr),
        )
        .unwrap();
        let staged = plan(
            &list(&archive).unwrap(),
            &Selection::new("", vec!["src/".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        assert_eq!(staged.files.len(), 6);
        assert!(
            staged.files.iter().any(|e| e.solid),
            "the files should share a solid block"
        );
        let (stage, _log) = stage_of(&archive, &staged, tmp.path().join("stage"), None);
        for (k, e) in staged.files.iter().enumerate() {
            let want = fs::read(tmp.path().join(&e.name)).unwrap();
            assert_eq!(read_file(&stage, k).unwrap(), want, "{}", e.name);
        }
    }

    #[test]
    fn rar_selections_read_back_every_file() {
        // Solid, and a file split over volumes (staged from the set's last volume).
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/archive/fixtures");
        for (name, wanted) in [
            ("rar5-solid.rar", "src/sub/data.bin"),
            ("rar4-solid.rar", "src/hello.txt"),
            ("rar5-vol.part4.rar", "src/big.bin"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let archive = fixtures.join(name);
            let staged = plan(
                &list(&archive).unwrap(),
                &Selection::new("", vec!["src/".into()]),
                &ExtractLimits::default(),
            )
            .unwrap();
            let k = staged.files.iter().position(|e| e.name == wanted).unwrap();
            let (stage, _log) = stage_of(&archive, &staged, tmp.path().join("stage"), None);
            let data = read_file(&stage, k).unwrap();
            assert_eq!(data.len() as u64, staged.files[k].size, "{name}");
            if wanted == "src/hello.txt" {
                assert_eq!(
                    data,
                    "hello rar
"
                    .repeat(200)
                    .into_bytes()
                );
            }
        }
    }

    #[test]
    fn plan_refuses_a_selection_over_the_total_size_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("big.zip");
        write_zip(
            &archive,
            CompressionMethod::Deflated,
            &[("a.txt", &vec![b'a'; 1000][..])],
        );
        let limits = ExtractLimits {
            max_total_bytes: 500,
            ..ExtractLimits::default()
        };
        let listing = list(&archive).unwrap();
        assert!(matches!(
            plan(&listing, &Selection::new("", vec!["a.txt".into()]), &limits),
            Err(CoreError::Invalid(_))
        ));
    }

    #[test]
    fn a_case_only_name_collision_is_skipped_not_staged() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("case.zip");
        write_zip(
            &archive,
            CompressionMethod::Deflated,
            &[("A.txt", &b"upper"[..]), ("a.txt", &b"lower"[..])],
        );
        let staged = plan(
            &list(&archive).unwrap(),
            &Selection::new("", vec!["A.txt".into(), "a.txt".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        assert_eq!(staged.files.len(), 1);
        assert_eq!(staged.files[0].name, "A.txt");
        assert_eq!(staged.items.len(), 1);
        assert_eq!(staged.skipped.len(), 1, "{:?}", staged.skipped);
        assert_eq!(staged.skipped[0].path, p("a.txt"));
        assert!(staged.skipped[0].reason.contains("same name"));
    }

    #[test]
    fn a_changed_archive_fails_every_read() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("changed.zip");
        write_zip(
            &archive,
            CompressionMethod::Deflated,
            &[("one.txt", &b"1"[..]), ("two.txt", &b"2"[..])],
        );
        let staged = plan(
            &list(&archive).unwrap(),
            &Selection::new("", vec!["one.txt".into(), "two.txt".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        let log: Log = Arc::default();
        let events = log.clone();
        let stage = Stage::new(
            archive.clone(),
            staged.files.clone(),
            None,
            ExtractLimits::default(),
            tmp.path().join("stage"),
            || None,
            move |e| events.lock().unwrap().push(e),
        );
        for k in 0..staged.files.len() {
            assert!(read_file(&stage, k).is_err(), "file {k} must not be served");
        }
        wait_until("the finished event", || finished(&log).is_some());
        let (failed, _) = finished(&log).unwrap();
        assert_eq!(failed.len(), 2, "{failed:?}");
        assert!(failed.iter().all(|f| f.reason.contains("changed")));
    }

    /// A 7z of two files encrypted with AES-256 (names left in the clear, so it lists
    /// without the password).
    fn encrypted_7z(tmp: &Path) -> PathBuf {
        let src = tmp.join("secret");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), b"classified alpha").unwrap();
        fs::write(src.join("b.txt"), b"classified bravo").unwrap();
        let archive = tmp.join("secret.7z");
        let opts = CreateOptions {
            encryption: Encryption::Aes256(Password::new("right")),
            encrypt_names: false,
            ..Default::default()
        };
        let (c, pr) = (CancelToken::new(), Progress::default());
        create(
            std::slice::from_ref(&src),
            &archive,
            &opts,
            &JobCtx::new(&c, &pr),
        )
        .unwrap();
        archive
    }

    #[test]
    fn a_wrong_password_for_a_7z_stage_is_reported_and_fails_the_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = encrypted_7z(tmp.path());
        let staged = plan(
            &list(&archive).unwrap(),
            &Selection::new("", vec!["secret/".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        assert!(staged.any_encrypted());
        let (stage, log) = stage_of(
            &archive,
            &staged,
            tmp.path().join("stage"),
            Some(Password::new("wrong")),
        );
        assert!(read_file(&stage, 0).is_err());
        wait_until("the password event", || !log.lock().unwrap().is_empty());
        assert_eq!(
            *log.lock().unwrap(),
            vec![StageEvent::Password { wrong: true }]
        );
    }

    #[test]
    fn a_missing_password_for_a_7z_stage_is_reported_and_fails_the_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = encrypted_7z(tmp.path());
        let staged = plan(
            &list(&archive).unwrap(),
            &Selection::new("", vec!["secret/".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        let (stage, log) = stage_of(&archive, &staged, tmp.path().join("stage"), None);
        assert!(read_file(&stage, 1).is_err());
        wait_until("the password event", || !log.lock().unwrap().is_empty());
        assert_eq!(
            *log.lock().unwrap(),
            vec![StageEvent::Password { wrong: false }]
        );
    }

    /// `docs/` (an entry), `docs/a.txt`, a link `docs/link` and `top.txt`.
    fn zip_with_link(path: &Path) {
        let mut zw = ZipWriter::new(File::create(path).unwrap());
        let options = SimpleFileOptions::default();
        zw.add_directory("docs/", options).unwrap();
        zw.start_file("docs/a.txt", options).unwrap();
        zw.write_all(b"moved").unwrap();
        zw.add_symlink("docs/link", "a.txt", options).unwrap();
        zw.start_file("top.txt", options).unwrap();
        zw.write_all(b"top").unwrap();
        zw.finish().unwrap();
    }

    #[test]
    fn a_move_removes_only_what_was_offered_and_keeps_a_folder_with_a_link_left_in_it() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("l.zip");
        zip_with_link(&zip);
        let listing = list(&zip).unwrap();
        let staged = plan(
            &listing,
            &Selection::new("", vec!["docs/".into(), "top.txt".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        assert_eq!(staged.left_out, ["docs/link"]);
        assert_eq!(staged.folder_entries, ["docs/"]);
        let mut names = staged.move_names();
        names.sort();
        // The file goes; the link stays, and so does the folder holding it. No name ends
        // with `/`, so nothing is deleted recursively.
        assert_eq!(names, ["docs/a.txt", "top.txt"]);

        let whole = plan(
            &listing,
            &Selection::new("docs/", vec!["docs/a.txt".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        assert_eq!(whole.move_names(), ["docs/a.txt"]);
    }

    #[test]
    fn a_folder_with_nothing_left_out_is_removed_by_its_own_entry_name() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("f.zip");
        let mut zw = ZipWriter::new(File::create(&zip).unwrap());
        let options = SimpleFileOptions::default();
        zw.add_directory("docs/", options).unwrap();
        zw.add_directory("docs/empty/", options).unwrap();
        zw.start_file("docs/a.txt", options).unwrap();
        zw.write_all(b"a").unwrap();
        zw.finish().unwrap();
        let staged = plan(
            &list(&zip).unwrap(),
            &Selection::new("", vec!["docs/".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        let mut names = staged.move_names();
        names.sort();
        assert_eq!(names, ["docs", "docs/a.txt", "docs/empty"]);
    }

    #[test]
    fn names_another_entry_shares_are_not_deleted() {
        let entry = |name: &str| ArchiveEntry {
            name: name.into(),
            is_dir: name.ends_with('/'),
            ..Default::default()
        };
        let listing = ArchiveListing {
            format: crate::archive::ArchiveFormat::Zip,
            entries: ["A.txt", "a.txt", "docs", "docs/", "docs/in.txt", "solo.txt"]
                .into_iter()
                .map(entry)
                .collect(),
            comment: String::new(),
        };
        let names: Vec<String> = ["A.txt", "docs", "docs/in.txt", "solo.txt", "gone.txt"]
            .into_iter()
            .map(String::from)
            .collect();
        let (keep, dropped) = unambiguous_names(&listing, &names);
        assert_eq!(keep, ["docs/in.txt", "solo.txt", "gone.txt"]);
        assert_eq!(dropped, 2);
        assert!(has_shared_names(&listing));
        let plain = ArchiveListing {
            entries: ["a.txt", "b/", "b/a.txt"].into_iter().map(entry).collect(),
            ..listing
        };
        assert!(!has_shared_names(&plain));
    }

    #[test]
    fn a_name_collision_stays_in_the_archive_with_its_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("c.zip");
        let mut zw = ZipWriter::new(File::create(&zip).unwrap());
        let options = SimpleFileOptions::default();
        zw.add_directory("sub/", options).unwrap();
        zw.start_file("sub/A.txt", options).unwrap();
        zw.write_all(b"upper").unwrap();
        zw.start_file("sub/a.txt", options).unwrap();
        zw.write_all(b"lower").unwrap();
        zw.finish().unwrap();
        let staged = plan(
            &list(&zip).unwrap(),
            &Selection::new("", vec!["sub/".into()]),
            &ExtractLimits::default(),
        )
        .unwrap();
        assert_eq!(staged.left_out, ["sub/a.txt"]);
        assert_eq!(staged.move_names(), ["sub/A.txt"]);
    }

    #[test]
    fn extraction_leaves_out_entries_for_the_same_reasons_as_the_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("l.zip");
        zip_with_link(&zip);
        let dest = tmp.path().join("out");
        fs::create_dir_all(&dest).unwrap();
        let (cancel, progress) = (CancelToken::new(), Progress::default());
        let report = crate::archive::extract(
            &zip,
            &dest,
            &crate::archive::ExtractOptions::default(),
            None,
            &JobCtx::new(&cancel, &progress),
        )
        .unwrap();
        assert!(!report.skipped.is_empty());
        assert!(report.skipped.iter().all(|s| is_left_out_reason(&s.reason)), "{:?}", report.skipped);
        assert!(!is_left_out_reason("already exists"));
    }
}
