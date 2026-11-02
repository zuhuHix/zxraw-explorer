//! Archives: browsing, safe extraction, integrity tests, creation and editing.
//!
//! Formats plug in through [`ArchiveBackend`]: ZIP (see `zip_backend`) and 7z (see
//! `sevenz_backend`).
//! Everything format-independent lives here, so a new format only has to list entries and
//! stream their data:
//!
//! * [`extract`] validates every entry name of the archive *before anything is written*
//!   (absolute paths, `..`, drive/UNC/device prefixes, `:` streams, NUL, reserved device names
//!   reject the whole archive), skips links (never created) and case-insensitive collisions,
//!   checks entry count, declared total size, per-entry compression ratio, overlapping entries
//!   and decoder memory up front, and caps the *actual* bytes while streaming, so lying headers
//!   cannot exceed the limits;
//! * parent folders are created component by component and an existing link/junction inside
//!   the destination aborts the entry (no writing through planted links);
//! * each file is written to a temp sibling and renamed into place when complete; a cancelled
//!   or failed entry leaves nothing behind;
//! * the archive's Mark-of-the-Web (`Zone.Identifier`) is copied to every extracted file, as
//!   Explorer does;
//! * encrypted entries need a [`Password`]; a missing or wrong one is reported as
//!   [`CoreError::PasswordRequired`] / [`CoreError::WrongPassword`], apart from corruption.
//!   Passwords are never logged (their `Debug` is redacted) and are wiped when dropped.

mod cp437;
mod crypto_peek;
mod motw;
mod presets;
mod safety;
mod sevenz_backend;
mod sevenz_guard;
mod sevenz_writer;
mod tree;
mod zip_backend;
mod zip_merge;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_codecs;
#[cfg(test)]
mod tests_edit;
#[cfg(test)]
mod tests_hostile;
#[cfg(test)]
mod tests_sevenz;
#[cfg(test)]
mod tests_sevenz_fixtures;
#[cfg(test)]
mod tests_sevenz_hostile;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::ops::{self, ConflictPolicy, JobCtx, NodeKind, OpIssue, StreamOptions, node_kind};
use crate::paths;

pub use presets::{
    CreateOptions, Encryption, Method, Preset, SevenZipPresetInfo, ZOPFLI_MAX_FILE,
    is_precompressed,
};
pub use safety::{collision_key, normalize_entry_name, validate_entry_name};
pub use tree::{ArchiveIndex, ArchiveNode};

const DEFAULT_MAX_ARCHIVE_ENTRIES: usize = 100_000;

// ---------------------------------------------------------------------------
// Formats and backends
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArchiveFormat {
    Zip,
    SevenZip,
}

impl ArchiveFormat {
    pub fn name(self) -> &'static str {
        match self {
            ArchiveFormat::Zip => "ZIP",
            ArchiveFormat::SevenZip => "7z",
        }
    }

    /// The extension new archives of this format get (no dot).
    pub fn extension(self) -> &'static str {
        match self {
            ArchiveFormat::Zip => "zip",
            ArchiveFormat::SevenZip => "7z",
        }
    }
}

/// A password for encrypted entries. Its `Debug` output is redacted and its bytes are wiped
/// when it is dropped; keep it in memory for the session only.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(zeroize::Zeroizing<Vec<u8>>);

impl Password {
    pub fn new(text: &str) -> Self {
        Self(zeroize::Zeroizing::new(text.as_bytes().to_vec()))
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

/// How an entry is encrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryEncryption {
    Aes,
    ZipCrypto,
    /// 7z AES-256. It has no password check of its own: a wrong password shows up as bad
    /// data (the 7z backend tests the start of each encrypted block before extracting).
    SevenZipAes,
}

impl EntryEncryption {
    /// Whether a data error of an entry encrypted this way may be a wrong password.
    pub fn ambiguous_errors(self) -> bool {
        matches!(
            self,
            EntryEncryption::ZipCrypto | EntryEncryption::SevenZipAes
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArchiveEntry {
    /// Position in the archive's directory.
    pub index: usize,
    /// Decoded name, `/`-separated; directories end with `/`.
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub encrypted: bool,
    pub encryption: Option<EntryEncryption>,
    pub size: u64,
    pub compressed_size: u64,
    pub modified: Option<SystemTime>,
    pub crc32: u32,
    /// Compression method, for display ("Deflate", "Zstandard", ...).
    pub method: String,
    /// Whether the name passes extraction safety checks.
    pub safe_name: bool,
    /// The entry shares a compressed block with others (a solid 7z block): its
    /// `compressed_size` is its share of the block, and reading it decodes the block from its
    /// start.
    #[serde(default)]
    pub solid: bool,
    /// Decoded bytes before this entry's data in its block, which reading it decodes and
    /// discards first (0 unless `solid`).
    #[serde(default)]
    pub block_offset: u64,
}

impl ArchiveEntry {
    /// Bytes decoded to read this entry: its block up to it, then its own data.
    pub fn decode_cost(&self) -> u64 {
        self.block_offset.saturating_add(self.size)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveListing {
    pub format: ArchiveFormat,
    pub entries: Vec<ArchiveEntry>,
    pub comment: String,
}

impl ArchiveListing {
    pub fn any_encrypted(&self) -> bool {
        self.entries.iter().any(|e| e.encrypted)
    }
}

/// Called for every wanted entry in archive order, with a reader of its decoded data or the
/// error that prevented opening it (a missing or wrong password, an unsupported method...).
pub type EntryVisitor<'a> = dyn FnMut(&ArchiveEntry, Result<&mut dyn Read>) -> Result<()> + 'a;

/// One archive format. Implementations list entries and stream their data; everything
/// else (safety checks, conflicts, progress, MOTW) is shared in this module.
pub trait ArchiveBackend: Sync {
    fn format(&self) -> ArchiveFormat;
    /// Lowercase extensions handled by this backend.
    fn extensions(&self) -> &'static [&'static str];
    /// Whether the first bytes of a file look like this format.
    fn sniff(&self, head: &[u8]) -> bool;
    /// Read the directory (no entry data). Fails if there are more than `max_entries`. A 7z
    /// archive with encrypted names needs `password` to be listed at all: without one this is
    /// [`CoreError::PasswordRequired`], with a wrong one [`CoreError::WrongPassword`].
    fn list(
        &self,
        archive: &Path,
        max_entries: usize,
        password: Option<&Password>,
    ) -> Result<ArchiveListing>;
    /// Structural checks before any data is extracted from the entries at `indices`:
    /// overlapping entries, decoder memory above `limits.max_decoder_memory` (for encrypted
    /// entries, read from their data decrypted with `password`)...
    fn preflight(
        &self,
        archive: &Path,
        indices: &[usize],
        password: Option<&Password>,
        limits: &ExtractLimits,
    ) -> Result<()>;
    /// Stream the entries `wanted` accepts to `visit`, in archive order (7z: entries without
    /// data first, then block by block). Implementations must check local headers against
    /// the directory and verify checksums as they read. `ctx`, when given, is checked for
    /// cancel and pause while data that no entry wants has to be decoded anyway (the start of
    /// a solid block).
    fn read_entries(
        &self,
        archive: &Path,
        wanted: &dyn Fn(&ArchiveEntry) -> bool,
        password: Option<&Password>,
        ctx: Option<&JobCtx<'_>>,
        visit: &mut EntryVisitor<'_>,
    ) -> Result<()>;
    /// Check `password` against the encrypted entries among `entries` before anything is
    /// extracted: [`CoreError::PasswordRequired`] / [`CoreError::WrongPassword`] when it can't
    /// open them. The default opens the cheapest one to read.
    fn check_password(
        &self,
        archive: &Path,
        entries: &[&ArchiveEntry],
        password: Option<&Password>,
    ) -> Result<()> {
        match entries
            .iter()
            .filter(|e| e.encrypted && !e.is_dir)
            .min_by_key(|e| e.block_offset)
        {
            Some(first) => open_one(self, archive, first.index, password),
            None => Ok(()),
        }
    }
    fn can_write(&self) -> bool {
        false
    }
    /// Create a new archive at `out` (which must not exist) from files and folders.
    fn create(
        &self,
        _sources: &[PathBuf],
        out: &Path,
        _opts: &CreateOptions,
        _ctx: &JobCtx<'_>,
    ) -> Result<CreateReport> {
        Err(read_only(out, self.format()))
    }
    /// Rewrite `archive` with `edit` applied, replacing it atomically when done.
    fn update(
        &self,
        archive: &Path,
        _edit: &ArchiveEdit,
        _opts: &CreateOptions,
        _ctx: &JobCtx<'_>,
    ) -> Result<CreateReport> {
        Err(read_only(archive, self.format()))
    }
}

fn read_only(path: &Path, format: ArchiveFormat) -> CoreError {
    CoreError::Invalid(format!(
        "{}: {} archives can't be written",
        path.display(),
        format.name()
    ))
}

static BACKENDS: &[&dyn ArchiveBackend] = &[&zip_backend::ZIP, &sevenz_backend::SEVEN_ZIP];

/// Every registered backend.
pub fn backends() -> &'static [&'static dyn ArchiveBackend] {
    BACKENDS
}

/// The backend for a file name's extension, if any (no I/O).
pub fn backend_for_name(path: &Path) -> Option<&'static dyn ArchiveBackend> {
    let ext = crate::model::ext_of(path);
    BACKENDS
        .iter()
        .copied()
        .find(|b| b.extensions().contains(&ext.as_str()))
}

/// Whether a file name has a browsable archive extension (no I/O).
pub fn is_archive_name(path: &Path) -> bool {
    backend_for_name(path).is_some()
}

/// The backend for an archive file: by extension, else by its first bytes.
pub fn backend_for_file(path: &Path) -> Result<&'static dyn ArchiveBackend> {
    if let Some(b) = backend_for_name(path) {
        return Ok(b);
    }
    let mut head = [0u8; 16];
    let n = fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .map_err(|e| CoreError::io(path, e))?;
    BACKENDS
        .iter()
        .copied()
        .find(|b| b.sniff(&head[..n]))
        .ok_or_else(|| CoreError::Invalid(format!("{}: not a supported archive", path.display())))
}

/// List an archive's entries without extracting anything.
pub fn list(archive: &Path) -> Result<ArchiveListing> {
    list_with_password(archive, None)
}

/// List an archive whose entry names may be encrypted (7z): see [`ArchiveBackend::list`].
pub fn list_with_password(archive: &Path, password: Option<&Password>) -> Result<ArchiveListing> {
    backend_for_file(archive)?.list(archive, DEFAULT_MAX_ARCHIVE_ENTRIES, password)
}

/// List the entries of a ZIP file (kept for existing callers; see [`list`]).
pub fn list_zip(path: &Path) -> Result<Vec<ArchiveEntry>> {
    list_zip_with_limit(path, DEFAULT_MAX_ARCHIVE_ENTRIES)
}

fn list_zip_with_limit(path: &Path, max_entries: usize) -> Result<Vec<ArchiveEntry>> {
    Ok(zip_backend::ZIP.list(path, max_entries, None)?.entries)
}

// ---------------------------------------------------------------------------
// Browsing: archive paths and the listing cache
// ---------------------------------------------------------------------------

/// Split a path that points into an archive (`D:\x\a.zip\sub\f.txt`) into the archive file
/// and the `/`-separated inner path (`sub/f.txt`; empty for the archive's root). `None` when
/// no ancestor is an archive file. Touches the file system for candidate components only.
pub fn split_archive_path(path: &Path) -> Option<(PathBuf, String)> {
    let comps: Vec<Component<'_>> = path.components().collect();
    let mut prefix = PathBuf::new();
    for (i, c) in comps.iter().enumerate() {
        prefix.push(c);
        if !matches!(c, Component::Normal(_)) || !is_archive_name(&prefix) {
            continue;
        }
        if fs::metadata(&prefix).is_ok_and(|m| m.is_file()) {
            let inner: Vec<String> = comps[i + 1..]
                .iter()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            return Some((prefix, inner.join("/")));
        }
    }
    None
}

/// The path shown for `inner` inside `archive` (the inverse of [`split_archive_path`]).
pub fn inner_path(archive: &Path, inner: &str) -> PathBuf {
    let mut p = archive.to_path_buf();
    for part in inner.split('/').filter(|s| !s.is_empty()) {
        p.push(part);
    }
    p
}

type CacheKey = (PathBuf, u64, Option<SystemTime>);
type IndexCache = Mutex<lru::LruCache<PathBuf, (CacheKey, Arc<ArchiveIndex>)>>;

fn cache() -> &'static IndexCache {
    static CACHE: OnceLock<IndexCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(lru::LruCache::new(std::num::NonZeroUsize::new(8).unwrap())))
}

/// The browsing index of an archive, cached while the file's size and modified time stay
/// the same (the central directory is read once, not on every folder change).
pub fn index_cached(archive: &Path) -> Result<Arc<ArchiveIndex>> {
    index_cached_with(archive, None)
}

/// [`index_cached`] for an archive whose names may be encrypted (7z): `password` is used
/// only when the index isn't cached yet. Errors as [`ArchiveBackend::list`].
pub fn index_cached_with(archive: &Path, password: Option<&Password>) -> Result<Arc<ArchiveIndex>> {
    let md = fs::metadata(archive).map_err(|e| CoreError::io(archive, e))?;
    let key: CacheKey = (archive.to_path_buf(), md.len(), md.modified().ok());
    let lookup = paths::lexical_normalize(archive);
    if let Some((k, idx)) = cache().lock().get(&lookup)
        && *k == key
    {
        return Ok(idx.clone());
    }
    let idx = Arc::new(ArchiveIndex::build(list_with_password(archive, password)?));
    cache().lock().put(lookup, (key, idx.clone()));
    Ok(idx)
}

/// Forget the cached index of an archive (after it was edited).
pub fn invalidate_cached(archive: &Path) {
    cache().lock().pop(&paths::lexical_normalize(archive));
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractLimits {
    pub max_entries: usize,
    /// Maximum total uncompressed bytes.
    pub max_total_bytes: u64,
    /// Maximum uncompressed bytes of one entry.
    pub max_file_bytes: u64,
    /// Maximum uncompressed/compressed ratio for entries larger than 1 MiB.
    pub max_ratio: u64,
    /// Maximum memory one entry's decoder may declare it needs (LZMA/XZ dictionaries, PPMd
    /// models). Zstandard frames are capped by the decoder itself (128 MiB windows).
    pub max_decoder_memory: u64,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ARCHIVE_ENTRIES,
            max_total_bytes: 64 * 1024 * 1024 * 1024,
            max_file_bytes: 32 * 1024 * 1024 * 1024,
            max_ratio: 1_000,
            max_decoder_memory: 1024 * 1024 * 1024,
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractOptions {
    pub limits: ExtractLimits,
    /// Conflict handling for files that already exist (`Ask` is treated as `Skip`;
    /// interactive questions belong in the UI before extraction).
    pub policy: ConflictPolicy,
    /// For encrypted entries. Never serialized.
    #[serde(skip)]
    pub password: Option<Password>,
    /// Copy the archive's Mark-of-the-Web to the extracted files.
    #[serde(default = "yes")]
    pub mark_of_the_web: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            limits: ExtractLimits::default(),
            policy: ConflictPolicy::default(),
            password: None,
            mark_of_the_web: true,
        }
    }
}

/// Which entries to extract, and the folder inside the archive they are relative to: with
/// `base` `"docs/"`, entry `docs/a/b.txt` extracts to `<dest>\a\b.txt`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Selection {
    /// `""` or a folder path ending with `/`.
    pub base: String,
    /// Entry names; a name ending with `/` selects that folder and everything inside it
    /// (it need not have an entry of its own).
    pub names: Vec<String>,
}

impl Selection {
    pub fn new(base: impl Into<String>, names: Vec<String>) -> Self {
        let mut base = normalize_entry_name(&base.into());
        if !base.is_empty() && !base.ends_with('/') {
            base.push('/');
        }
        Self { base, names }
    }

    /// The selection item that covers `name`, if any.
    fn covering(&self, name: &str) -> Option<usize> {
        let lower = name.to_lowercase();
        self.names.iter().position(|n| {
            let n = n.to_lowercase();
            n == lower || (n.ends_with('/') && lower.starts_with(&n))
        })
    }

    fn relative<'n>(&self, name: &'n str) -> &'n str {
        if self.base.is_empty() {
            return name;
        }
        match name.get(..self.base.len()) {
            Some(head) if head.to_lowercase() == self.base.to_lowercase() => {
                &name[self.base.len()..]
            }
            _ => name,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractReport {
    /// The folder the entries were extracted into.
    pub dest: PathBuf,
    pub extracted: Vec<PathBuf>,
    pub skipped: Vec<OpIssue>,
    pub failed: Vec<OpIssue>,
    pub bytes: u64,
    pub cancelled: bool,
}

struct Planned {
    entry: ArchiveEntry,
    rel: PathBuf,
}

fn is_password_error(e: &CoreError) -> bool {
    matches!(
        e,
        CoreError::PasswordRequired(_) | CoreError::WrongPassword(_)
    )
}

/// Extract `archive` (all of it, or `selection`) into the existing folder `dest_dir` safely
/// (see the module docs). Returns `Err` without writing anything if the archive is unsafe,
/// over the limits, or needs a password that is missing or wrong.
pub fn extract(
    archive: &Path,
    dest_dir: &Path,
    opts: &ExtractOptions,
    selection: Option<&Selection>,
    ctx: &JobCtx<'_>,
) -> Result<ExtractReport> {
    if !fs::metadata(dest_dir)
        .map_err(|e| CoreError::io(dest_dir, e))?
        .is_dir()
    {
        return Err(CoreError::Invalid(format!(
            "{} is not a folder",
            dest_dir.display()
        )));
    }
    let backend = backend_for_file(archive)?;
    let limits = &opts.limits;
    let listing = backend.list(archive, limits.max_entries, opts.password.as_ref())?;
    let mut report = ExtractReport {
        dest: dest_dir.to_path_buf(),
        ..Default::default()
    };

    // Pass 1: validate everything before touching the disk.
    let mut planned: Vec<Planned> = Vec::new();
    let mut used = vec![false; selection.map_or(0, |s| s.names.len())];
    let mut archive_total: u64 = 0;
    let mut selected_total: u64 = 0;
    for e in &listing.entries {
        let trimmed = e.name.trim_end_matches('/');
        if trimmed.is_empty() && e.is_dir {
            continue; // "/" root entry
        }
        let full_rel = validate_entry_name(&e.name)?;
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
        let rel = match selection {
            None => full_rel,
            Some(sel) => {
                let Some(item) = sel.covering(&e.name) else {
                    continue;
                };
                used[item] = true;
                match sel.relative(&e.name).trim_end_matches('/') {
                    "" => continue, // the base folder itself
                    rest => validate_entry_name(rest)?,
                }
            }
        };
        if e.is_symlink {
            report.skipped.push(OpIssue::new(
                &rel,
                "symbolic link entries are not extracted",
            ));
            continue;
        }
        selected_total = selected_total.saturating_add(e.size);
        planned.push(Planned {
            entry: e.clone(),
            rel,
        });
    }
    if let Some(sel) = selection {
        for (name, _) in sel.names.iter().zip(&used).filter(|(_, u)| !**u) {
            report.failed.push(OpIssue::new(
                Path::new(name),
                "selected entry was not found in the archive",
            ));
        }
    }

    // Case-insensitive collisions: the first entry wins, a file and a folder of the same
    // name can't both exist.
    let dir_keys: HashSet<String> = planned
        .iter()
        .flat_map(|p| {
            let rel = if p.entry.is_dir {
                p.rel.as_path()
            } else {
                p.rel.parent().unwrap_or(Path::new(""))
            };
            rel.ancestors()
                .filter(|a| !a.as_os_str().is_empty())
                .map(collision_key)
                .collect::<Vec<_>>()
        })
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    planned.retain(|p| {
        if p.entry.is_dir {
            return true;
        }
        let key = collision_key(&p.rel);
        let reason = if dir_keys.contains(&key) {
            "a folder in the archive has the same name"
        } else if !seen.insert(key) {
            "another entry has the same name (names are not case-sensitive on Windows)"
        } else {
            return true;
        };
        report.skipped.push(OpIssue::new(&p.rel, reason));
        selected_total = selected_total.saturating_sub(p.entry.size);
        false
    });

    let indices: Vec<usize> = planned.iter().map(|p| p.entry.index).collect();
    let password = opts.password.as_ref();
    // Limits first: checking the password opens an entry, which sets up its decoder. The
    // preflight reads encrypted headers only when the password passes their check.
    backend.preflight(archive, &indices, password, limits)?;
    let planned_entries: Vec<&ArchiveEntry> = planned.iter().map(|p| &p.entry).collect();
    backend.check_password(archive, &planned_entries, password)?;
    // The destination, resolved and pinned from the volume root for the whole job; entries
    // are written beneath its final path and reported beneath `dest_dir`.
    let base = safety::PinnedBase::open(dest_dir)?;
    let root = base.root();
    let zone = if opts.mark_of_the_web {
        motw::read_zone(archive)
    } else {
        None
    };

    ctx.progress.add_total_items(planned.len() as u64);
    ctx.progress.add_total_bytes(selected_total);

    // Pass 2: extract.
    let by_index: HashMap<usize, usize> = planned
        .iter()
        .enumerate()
        .map(|(i, p)| (p.entry.index, i))
        .collect();
    let mut remaining = limits.max_total_bytes;
    let mut visit = |entry: &ArchiveEntry, reader: Result<&mut dyn Read>| -> Result<()> {
        ctx.cancel.check()?;
        let p = &planned[by_index[&entry.index]];
        ctx.progress.add_items(1);
        ctx.progress.set_current(entry.name.clone());
        let target = root.join(&p.rel);
        // What reports show: the same item beneath the destination as the user named it.
        let shown = |path: &Path| match path.strip_prefix(root) {
            Ok(rel) => dest_dir.join(rel),
            Err(_) => path.to_path_buf(),
        };
        let parent_rel = if p.entry.is_dir {
            p.rel.as_path()
        } else {
            p.rel.parent().unwrap_or(Path::new(""))
        };
        // Held until the entry is in place: the folders it is written into can't be swapped.
        let _pinned = match safety::pin_dirs(&base, parent_rel) {
            Ok(pinned) => pinned,
            Err(e) => {
                report
                    .failed
                    .push(OpIssue::new(shown(&target), e.to_string()));
                return Ok(());
            }
        };
        if p.entry.is_dir {
            return Ok(());
        }
        let reader = match reader {
            Ok(r) => r,
            Err(e) => {
                report
                    .failed
                    .push(OpIssue::new(shown(&target), e.to_string()));
                ctx.progress.add_bytes(p.entry.size);
                return Ok(());
            }
        };
        let (dest, overwrite, keep_both) = match fs::symlink_metadata(&target) {
            Err(_) => (target, false, false),
            Ok(md) if node_kind(&md) != NodeKind::File => {
                report.failed.push(OpIssue::new(
                    shown(&target),
                    "a folder or link with this name exists",
                ));
                return Ok(());
            }
            Ok(_) => match opts.policy {
                ConflictPolicy::Overwrite => (target, true, false),
                // The name is re-checked at publish time; a racing file gets the next name.
                ConflictPolicy::KeepBoth => (target, false, true),
                _ => {
                    report
                        .skipped
                        .push(OpIssue::new(shown(&target), "already exists"));
                    ctx.progress.add_bytes(p.entry.size);
                    return Ok(());
                }
            },
        };
        let sopts = StreamOptions {
            overwrite,
            modified: p.entry.modified,
            // Never trust the declared size beyond the limits.
            limit: Some(p.entry.size.min(limits.max_file_bytes).min(remaining)),
            exact: Some(p.entry.size),
            ..Default::default()
        };
        let label = PathBuf::from(&p.entry.name);
        match ops::write_stream_placed(reader, &label, &dest, sopts, keep_both, ctx) {
            Ok((out, placed)) => {
                remaining = remaining.saturating_sub(out.bytes);
                report.bytes += out.bytes;
                if let Some(zone) = &zone
                    && let Err(e) = motw::write_zone(&placed, zone, archive)
                {
                    tracing::warn!("could not mark {} as downloaded: {e}", placed.display());
                }
                report.extracted.push(shown(&placed));
                Ok(())
            }
            Err(CoreError::Cancelled) => Err(CoreError::Cancelled),
            Err(e) => {
                report
                    .failed
                    .push(OpIssue::new(shown(&dest), data_error(&p.entry, e)));
                Ok(())
            }
        }
    };
    let wanted = |e: &ArchiveEntry| by_index.contains_key(&e.index);
    match backend.read_entries(archive, &wanted, password, Some(ctx), &mut visit) {
        Ok(()) => {}
        Err(CoreError::Cancelled) => report.cancelled = true,
        Err(e) => return Err(e),
    }
    Ok(report)
}

/// The reason shown for an entry whose data could not be read.
fn data_error(entry: &ArchiveEntry, e: CoreError) -> String {
    if is_password_error(&e) {
        return e.to_string();
    }
    let text = e.to_string();
    if entry
        .encryption
        .is_some_and(EntryEncryption::ambiguous_errors)
        && !matches!(e, CoreError::Io { .. })
    {
        // ZipCrypto's password check is one byte: a wrong password passes it 1 time in 256
        // and then fails as bad data. 7z AES has no check at all.
        format!("{text} (damaged data or wrong password)")
    } else {
        text
    }
}

/// Open the encrypted entry `index` with `password` to check it before extracting anything.
fn open_one<B: ArchiveBackend + ?Sized>(
    backend: &B,
    archive: &Path,
    index: usize,
    password: Option<&Password>,
) -> Result<()> {
    let mut outcome = Ok(());
    backend.read_entries(
        archive,
        &|e| e.index == index,
        password,
        None,
        &mut |_, reader| {
            if let Err(e) = reader
                && is_password_error(&e)
            {
                outcome = Err(e);
            }
            Ok(())
        },
    )?;
    outcome
}

/// Extract the whole ZIP (kept for existing callers; see [`extract`]).
pub fn extract_zip(
    archive: &Path,
    dest_dir: &Path,
    opts: &ExtractOptions,
    ctx: &JobCtx<'_>,
) -> Result<ExtractReport> {
    extract(archive, dest_dir, opts, None, ctx)
}

/// Extract only the named entries, keeping their folders (see [`extract`]).
pub fn extract_zip_entries(
    zip: &Path,
    dest: &Path,
    entries: &[String],
    opts: &ExtractOptions,
    ctx: &JobCtx<'_>,
) -> Result<ExtractReport> {
    let sel = Selection::new("", entries.to_vec());
    extract(zip, dest, opts, Some(&sel), ctx)
}

/// What the top level of an archive holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopLevel {
    Empty,
    /// One folder (and everything is inside it).
    Folder(String),
    /// One file.
    File(String),
    Many,
}

/// Classify the top level of `listing` (unsafe names are ignored here; extraction rejects them).
pub fn top_level(listing: &ArchiveListing) -> TopLevel {
    // The one top-level name, and whether it was seen as a file and as a folder.
    let mut first: Option<(String, bool, bool)> = None;
    for e in &listing.entries {
        let name = e.name.trim_matches('/');
        if name.is_empty() {
            continue;
        }
        let (head, folder) = match name.split_once('/') {
            Some((h, _)) => (h, true),
            None => (name, e.is_dir),
        };
        match &mut first {
            None => first = Some((head.to_owned(), !folder, folder)),
            Some((h, as_file, as_folder)) if h.eq_ignore_ascii_case(head) => {
                *as_file |= !folder;
                *as_folder |= folder;
            }
            Some(_) => return TopLevel::Many,
        }
    }
    match first {
        None => TopLevel::Empty,
        // A file and a folder of the same name: two things, not one.
        Some((_, true, true)) => TopLevel::Many,
        Some((h, false, _)) => TopLevel::Folder(h),
        Some((h, true, false)) => TopLevel::File(h),
    }
}

/// Where the extract commands put an archive's contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtractTarget {
    /// "Extract here (smart)": into the archive's folder when the archive holds one folder
    /// or one file, else into a new folder named after the archive.
    Smart,
    /// "Extract to `<name>\`": always into a folder named after the archive.
    NamedFolder,
}

/// The archive's name without its extension(s) (`photos.tar.gz` would give `photos`).
pub fn archive_stem(archive: &Path) -> String {
    let name = paths::name_of(archive);
    let stem = match name.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s.to_owned(),
        _ => name,
    };
    paths::sanitize_file_name(&stem)
}

/// Extract `archive` next to `parent` as `target` says. A folder the command creates is
/// removed again if the extraction is cancelled or fails before anything was extracted.
pub fn extract_to(
    archive: &Path,
    parent: &Path,
    target: ExtractTarget,
    opts: &ExtractOptions,
    ctx: &JobCtx<'_>,
) -> Result<ExtractReport> {
    let backend = backend_for_file(archive)?;
    let listing = backend.list(archive, opts.limits.max_entries, opts.password.as_ref())?;
    let nest = match target {
        ExtractTarget::NamedFolder => true,
        ExtractTarget::Smart => matches!(top_level(&listing), TopLevel::Many),
    };
    if !nest {
        return extract(archive, parent, opts, None, ctx);
    }
    let stem = archive_stem(archive);
    let dest = parent.join(&stem);
    let created = match fs::create_dir(&dest) {
        Ok(()) => true,
        Err(e)
            if e.kind() == io::ErrorKind::AlreadyExists && target == ExtractTarget::NamedFolder =>
        {
            safety::verify_real_directory(&dest)?;
            false
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // Smart: never merge into an unrelated folder; use the next free name.
            let fresh = paths::unique_name(parent, &stem);
            fs::create_dir(&fresh).map_err(|e| CoreError::io(&fresh, e))?;
            return finish_created(archive, fresh, opts, ctx);
        }
        Err(e) => return Err(CoreError::io(&dest, e)),
    };
    if created {
        finish_created(archive, dest, opts, ctx)
    } else {
        extract(archive, &dest, opts, None, ctx)
    }
}

fn finish_created(
    archive: &Path,
    dest: PathBuf,
    opts: &ExtractOptions,
    ctx: &JobCtx<'_>,
) -> Result<ExtractReport> {
    let result = extract(archive, &dest, opts, None, ctx);
    let failed_empty = match &result {
        Err(_) => true,
        Ok(r) => r.cancelled || r.extracted.is_empty() && !r.failed.is_empty(),
    };
    if failed_empty {
        remove_created_tree(&dest, result.as_ref().ok());
    }
    result
}

/// Remove the files a cancelled extraction placed in the folder it created, then the folder
/// itself if nothing else is in it.
fn remove_created_tree(dest: &Path, report: Option<&ExtractReport>) {
    if let Some(r) = report {
        for f in &r.extracted {
            let _ = fs::remove_file(f);
        }
    }
    // Empty folders only, deepest first: anything not ours stays.
    let mut dirs = Vec::new();
    let _ = ops::walk_tree(dest, &forge_jobs::CancelToken::new(), |item| {
        if let ops::WalkItem::Entry(e) = item
            && e.kind == NodeKind::Dir
        {
            dirs.push(e.path.clone());
        }
    });
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let _ = fs::remove_dir(d);
    }
    let _ = fs::remove_dir(dest);
}

// ---------------------------------------------------------------------------
// Integrity test
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryTest {
    pub name: String,
    pub size: u64,
    /// `None` when the entry's data decoded and its checksum matched.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestReport {
    pub entries: Vec<EntryTest>,
    pub bytes: u64,
    pub cancelled: bool,
}

impl TestReport {
    pub fn failures(&self) -> impl Iterator<Item = &EntryTest> {
        self.entries.iter().filter(|e| e.error.is_some())
    }
    pub fn is_ok(&self) -> bool {
        !self.cancelled && self.failures().next().is_none()
    }
}

/// Decode every file entry and verify its checksum, reporting per entry. Structural
/// problems (overlapping entries, unsafe names are *not* one: they are reported per entry)
/// and a missing or wrong password are errors.
pub fn test_archive(
    archive: &Path,
    password: Option<&Password>,
    limits: &ExtractLimits,
    ctx: &JobCtx<'_>,
) -> Result<TestReport> {
    let backend = backend_for_file(archive)?;
    let listing = backend.list(archive, limits.max_entries, password)?;
    let files: Vec<&ArchiveEntry> = listing.entries.iter().filter(|e| !e.is_dir).collect();
    let indices: Vec<usize> = files.iter().map(|e| e.index).collect();
    backend.preflight(archive, &indices, password, limits)?;
    backend.check_password(archive, &files, password)?;
    ctx.progress.add_total_items(files.len() as u64);
    ctx.progress
        .add_total_bytes(files.iter().map(|e| e.size).sum());
    let mut report = TestReport::default();
    let mut buf = vec![0u8; 256 * 1024];
    let mut visit = |entry: &ArchiveEntry, reader: Result<&mut dyn Read>| -> Result<()> {
        ctx.cancel.check()?;
        ctx.progress.add_items(1);
        ctx.progress.set_current(entry.name.clone());
        let mut error =
            (!entry.safe_name).then(|| "unsafe name; it would not be extracted".to_owned());
        match reader {
            Err(e) => error = Some(e.to_string()),
            Ok(r) => {
                let cap = entry.size.min(limits.max_file_bytes);
                let mut total = 0u64;
                loop {
                    ctx.cancel.check()?;
                    match r.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            total += n as u64;
                            ctx.progress.add_bytes(n as u64);
                            if total > cap {
                                error = Some("more data than the archive declares".into());
                                break;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            error = Some(data_error(entry, CoreError::Invalid(e.to_string())));
                            break;
                        }
                    }
                }
                if error.is_none() && total != entry.size {
                    error = Some(format!("{total} bytes decoded, {} declared", entry.size));
                }
                report.bytes += total;
            }
        }
        report.entries.push(EntryTest {
            name: entry.name.clone(),
            size: entry.size,
            error,
        });
        Ok(())
    };
    let wanted = |e: &ArchiveEntry| !e.is_dir;
    match backend.read_entries(archive, &wanted, password, Some(ctx), &mut visit) {
        Ok(()) => {}
        Err(CoreError::Cancelled) => report.cancelled = true,
        Err(e) => return Err(e),
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Opening single entries
// ---------------------------------------------------------------------------

/// Where entries opened from archives are extracted (`%TEMP%\ZXraw Archive Temp`).
pub fn temp_root() -> PathBuf {
    std::env::temp_dir().join("ZXraw Archive Temp")
}

/// Extract one file entry to a private temp folder and return its path, to open it with
/// its app or preview it. The archive's Mark-of-the-Web is applied. A copy extracted
/// earlier from the same unchanged archive is reused. Fails for entries over `max_bytes`.
pub fn extract_entry_to_temp(
    archive: &Path,
    entry_name: &str,
    password: Option<&Password>,
    max_bytes: u64,
    ctx: &JobCtx<'_>,
) -> Result<PathBuf> {
    let md = fs::metadata(archive).map_err(|e| CoreError::io(archive, e))?;
    let stamp = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let key = format!(
        "{}|{}|{stamp}",
        paths::lexical_normalize(archive)
            .display()
            .to_string()
            .to_lowercase(),
        md.len()
    );
    let dir = temp_root().join(format!(
        "{:016x}",
        xxhash_rust::xxh3::xxh3_64(key.as_bytes())
    ));
    let rel = validate_entry_name(entry_name)?;
    let file_name = rel
        .file_name()
        .ok_or_else(|| CoreError::Invalid(format!("{entry_name}: not a file")))?
        .to_owned();
    let index = index_cached_with(archive, password)?;
    let node = index
        .node(entry_name.trim_end_matches('/'))
        .filter(|n| !n.is_dir)
        .ok_or_else(|| CoreError::NotFound(inner_path(archive, entry_name)))?;
    if node.size > max_bytes {
        return Err(CoreError::Invalid(format!(
            "{entry_name} is too large to open from the archive ({} bytes); extract it first",
            node.size
        )));
    }
    // One folder per entry (by its full path), so `a/config.json` and `b/config.json` never
    // share a copy.
    let dir = dir.join(format!(
        "{:016x}",
        xxhash_rust::xxh3::xxh3_64(node.path.to_lowercase().as_bytes())
    ));
    let base = match node.path.rsplit_once('/') {
        Some((parent, _)) => format!("{parent}/"),
        None => String::new(),
    };
    let target = dir.join(&file_name);
    // Reused only when it is still the file this extraction wrote: a plain file (not a link)
    // with the entry's size and modified time.
    let reusable = node.modified.is_some()
        && fs::symlink_metadata(&target).is_ok_and(|m| {
            m.is_file()
                && !safety::is_link_or_reparse(&m)
                && m.len() == node.size
                && m.modified().ok() == node.modified
        });
    if reusable {
        return Ok(target);
    }
    fs::create_dir_all(&dir).map_err(|e| CoreError::io(&dir, e))?;
    let opts = ExtractOptions {
        limits: ExtractLimits {
            max_file_bytes: max_bytes,
            ..Default::default()
        },
        policy: ConflictPolicy::Overwrite,
        password: password.cloned(),
        mark_of_the_web: true,
    };
    let sel = Selection::new(base, vec![node.path.clone()]);
    let report = extract(archive, &dir, &opts, Some(&sel), ctx)?;
    if report.cancelled {
        return Err(CoreError::Cancelled);
    }
    if let Some(issue) = report.failed.first().or(report.skipped.first()) {
        return Err(CoreError::Invalid(format!(
            "{}: {}",
            issue.path.display(),
            issue.reason
        )));
    }
    Ok(target)
}

/// Remove temp folders of opened entries that were last written more than `older_than` ago.
pub fn prune_temp(older_than: Duration) {
    let Ok(rd) = fs::read_dir(temp_root()) else {
        return;
    };
    let now = SystemTime::now();
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > older_than));
        if old && e.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

// ---------------------------------------------------------------------------
// Creation and editing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateReport {
    pub files: u64,
    pub dirs: u64,
    /// Bytes read from the source files.
    pub bytes_in: u64,
    /// Size of the archive written.
    pub bytes_out: u64,
    /// Entries carried over unchanged (raw-copied, not recompressed) by an update.
    pub kept: u64,
    /// Entries an update replaced or deleted.
    pub replaced: u64,
    pub deleted: u64,
    pub skipped: Vec<OpIssue>,
    /// Things the user should know about a finished update (one archive, several passwords).
    pub warnings: Vec<String>,
}

/// Kept for existing callers.
pub type ZipReport = CreateReport;

/// Changes to apply to an existing archive.
#[derive(Debug, Clone, Default)]
pub struct ArchiveEdit {
    /// Files and folders to add, each into the archive folder `dest` (`""` or ending with `/`).
    /// An entry of the same name (ignoring case) is replaced.
    pub add: Vec<(PathBuf, String)>,
    /// Entry names to delete; a name ending with `/` deletes that folder and its contents.
    pub delete: Vec<String>,
}

/// Create a new archive at `out` from files and folders. Folder entries are stored relative
/// to each source's parent (`photos/…`). Links are skipped (never followed). Fails if `out`
/// exists; the archive is written to a temp file and renamed into place when complete.
pub fn create(
    sources: &[PathBuf],
    out: &Path,
    opts: &CreateOptions,
    ctx: &JobCtx<'_>,
) -> Result<CreateReport> {
    let backend = backend_for_name(out)
        .ok_or_else(|| CoreError::Invalid(format!("{}: unknown archive type", out.display())))?;
    backend.create(sources, out, opts, ctx)
}

/// Create a ZIP with the default (Balanced) options (kept for existing callers).
pub fn create_zip(sources: &[PathBuf], out: &Path, ctx: &JobCtx<'_>) -> Result<CreateReport> {
    zip_backend::ZIP.create(sources, out, &CreateOptions::default(), ctx)
}

/// Apply `edit` to `archive`: the archive is rebuilt into a temp file next to it (unchanged
/// entries copied record by record, as stored) and atomically replaces the original when done.
/// A cancelled or failed update leaves the original untouched.
///
/// Kept entries are never decoded or decrypted, so deleting needs no password. Files added to
/// an archive with encrypted entries must be encrypted, with the password in
/// `opts.encryption`: without one the update fails with [`CoreError::PasswordRequired`].
/// When the archive has encrypted files, the password must open at least one of them, or
/// the update fails with [`CoreError::WrongPassword`]; both before anything is written. When
/// it opens some but not others, the update goes ahead and says so in the report's warnings.
pub fn update(
    archive: &Path,
    edit: &ArchiveEdit,
    opts: &CreateOptions,
    ctx: &JobCtx<'_>,
) -> Result<CreateReport> {
    let backend = backend_for_file(archive)?;
    if backend.format() == ArchiveFormat::Zip {
        // Before anything reads the directory: a split zip can't be rebuilt as one file.
        let mut file = fs::File::open(archive).map_err(|e| CoreError::io(archive, e))?;
        zip_merge::check_single_disk(archive, &mut file)?;
    }
    let warnings = check_update_password(backend, archive, edit, opts)?;
    let result = backend.update(archive, edit, opts, ctx).map(|mut report| {
        report.warnings.extend(warnings);
        report
    });
    invalidate_cached(archive);
    result
}

/// Shown when the password given for new files opens some encrypted entries but not others.
pub const SEVERAL_PASSWORDS: &str =
    "This archive uses more than one password; new files will use the one you entered.";

/// Encrypted files whose password checks are tried (the smallest ones).
const PASSWORD_SAMPLE: usize = 64;

/// Largest entry decoded in full to confirm a password beyond its header check.
const PASSWORD_PROBE_MAX: u64 = 64 * 1024 * 1024;

/// The password new files of an encrypted archive will get must open its existing encrypted
/// files: each sampled file's header check (no data decoded), then the smallest file that
/// passed it decoded in full within the extraction limits, so a check passed by chance
/// (1 in 256 for ZipCrypto, 1 in 65,536 for AES) is caught by the checksum. With only
/// encrypted folders there is nothing to check against; the caller asked for the password
/// twice. Returns warnings for the report.
fn check_update_password(
    backend: &dyn ArchiveBackend,
    archive: &Path,
    edit: &ArchiveEdit,
    opts: &CreateOptions,
) -> Result<Vec<String>> {
    if edit.add.is_empty() {
        return Ok(Vec::new());
    }
    let listing = backend.list(
        archive,
        DEFAULT_MAX_ARCHIVE_ENTRIES,
        opts.encryption.password(),
    )?;
    if !listing.any_encrypted() {
        return Ok(Vec::new());
    }
    let label = || paths::display_path(archive);
    let password = match &opts.encryption {
        Encryption::None => return Err(CoreError::PasswordRequired(label())),
        Encryption::Aes256(p) | Encryption::ZipCryptoLegacy(p) => p,
    };
    let mut files: Vec<&ArchiveEntry> = listing
        .entries
        .iter()
        .filter(|e| e.encrypted && !e.is_dir)
        .collect();
    if files.is_empty() {
        return Ok(Vec::new());
    }
    if backend.format() == ArchiveFormat::SevenZip {
        // 7z AES has no header check to sample: the backend decodes the start of the
        // encrypted blocks (opening a file in the middle of a solid block would decode the
        // block up to it).
        backend.check_password(archive, &files, Some(password))?;
        return Ok(Vec::new());
    }
    files.sort_by_key(|e| e.size);
    files.truncate(PASSWORD_SAMPLE);
    let sample: HashSet<usize> = files.iter().map(|e| e.index).collect();
    // Opening an entry sets up its decoder: the same decoder-memory limits as extraction
    // apply first (entries this password doesn't decrypt are left out by their header check).
    let limits = ExtractLimits::default();
    let indices: Vec<usize> = files.iter().map(|e| e.index).collect();
    backend.preflight(archive, &indices, Some(password), &limits)?;
    // Header checks only: the visitor reads nothing.
    let mut opened = Vec::new();
    let mut refused = 0;
    backend.read_entries(
        archive,
        &|e| sample.contains(&e.index),
        Some(password),
        None,
        &mut |e, reader| {
            match reader {
                Ok(_) => opened.push(e.clone()),
                Err(err) if is_password_error(&err) => refused += 1,
                Err(_) => {}
            }
            Ok(())
        },
    )?;
    let Some(probe) = opened.iter().min_by_key(|e| e.size) else {
        return Err(CoreError::WrongPassword(label()));
    };
    let bomb =
        probe.size > 1024 * 1024 && probe.size / probe.compressed_size.max(1) > limits.max_ratio;
    if probe.size <= PASSWORD_PROBE_MAX && !bomb {
        let mut decoded = false;
        backend.read_entries(
            archive,
            &|e| e.index == probe.index,
            Some(password),
            None,
            &mut |_, reader| {
                if let Ok(r) = reader {
                    let limit = probe.size.saturating_add(1);
                    decoded = io::copy(&mut r.take(limit), &mut io::sink())
                        .is_ok_and(|n| n == probe.size);
                }
                Ok(())
            },
        )?;
        if !decoded {
            return Err(CoreError::WrongPassword(label()));
        }
    }
    Ok(if refused > 0 {
        vec![SEVERAL_PASSWORDS.to_owned()]
    } else {
        Vec::new()
    })
}

/// Source files and folders to add, flattened (folders walked, links reported).
pub(crate) struct SourceItem {
    pub path: PathBuf,
    /// Entry name (`/`-separated; no trailing `/` for folders).
    pub name: String,
    pub kind: NodeKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// Walk `sources` into items named `<prefix><source name>/<relative path>`.
pub(crate) fn collect_sources(
    sources: &[PathBuf],
    prefix: &str,
    ctx: &JobCtx<'_>,
    skipped: &mut Vec<OpIssue>,
) -> Result<Vec<SourceItem>> {
    let mut work = Vec::new();
    for src in sources {
        let md = fs::symlink_metadata(src).map_err(|e| CoreError::io(src, e))?;
        let name = paths::name_of(src);
        if name.is_empty() {
            return Err(CoreError::Invalid(format!(
                "cannot archive {}",
                src.display()
            )));
        }
        let name = format!("{prefix}{name}");
        let kind = node_kind(&md);
        work.push(SourceItem {
            path: src.clone(),
            name: name.clone(),
            kind,
            size: md.len(),
            modified: md.modified().ok(),
        });
        if kind == NodeKind::Dir {
            ops::walk_tree(src, ctx.cancel, |item| match item {
                ops::WalkItem::Entry(e) => work.push(SourceItem {
                    path: e.path.clone(),
                    name: format!("{name}/{}", paths::to_slash(&e.rel)),
                    kind: e.kind,
                    size: e.size,
                    modified: e.modified,
                }),
                ops::WalkItem::Error { path, error } => {
                    skipped.push(OpIssue::new(path, error.to_string()))
                }
            })?;
        }
    }
    Ok(work)
}
