//! Archives: browsing, safe extraction, integrity tests, creation and editing.
//!
//! Formats plug in through [`ArchiveBackend`]: ZIP (see `zip_backend`), 7z (see
//! `sevenz_backend`), tar plain or compressed and single compressed files (see
//! `tar_backend`), and RAR, read only (see `rar_backend`).
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

mod codec;
mod cp437;
mod crypto_peek;
mod motw;
mod presets;
mod rar_backend;
mod safety;
mod sevenz_backend;
mod sevenz_guard;
mod sevenz_writer;
pub mod sfx;
mod stamp;
pub mod stage;
pub use stamp::{ArchiveStamp, archive_stamp};
mod tar_backend;
mod tar_read;
mod tar_write;
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
#[cfg(all(test, windows))]
mod tests_leftovers;
#[cfg(test)]
mod tests_name_collision;
#[cfg(test)]
mod tests_rar;
#[cfg(test)]
mod tests_sevenz;
#[cfg(test)]
mod tests_sevenz_fixtures;
#[cfg(test)]
mod tests_sevenz_hostile;
#[cfg(test)]
mod tests_sfx;
#[cfg(test)]
mod tests_tar;
#[cfg(test)]
mod tests_tar_hostile;
#[cfg(test)]
mod tests_stamp;

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

pub use codec::{Codec, StreamLevel};
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
    /// A tar, plain (`Codec::None`) or in a compression stream (`.tar.gz`...).
    Tar(Codec),
    /// One compressed file (`.gz`, `.xz`...), shown as an archive of one file.
    Compressed(Codec),
    /// RAR (any version), opened and extracted only: the UnRAR licence forbids writing it.
    Rar,
}

impl ArchiveFormat {
    pub fn name(self) -> &'static str {
        match self {
            ArchiveFormat::Zip => "ZIP",
            ArchiveFormat::SevenZip => "7z",
            ArchiveFormat::Tar(c) => match c {
                Codec::None => "tar",
                Codec::Gzip => "tar.gz",
                Codec::Bzip2 => "tar.bz2",
                Codec::Xz => "tar.xz",
                Codec::Zstd => "tar.zst",
                Codec::Lz4 => "tar.lz4",
                Codec::Brotli => "tar.br",
            },
            ArchiveFormat::Compressed(c) => c.name(),
            ArchiveFormat::Rar => "RAR",
        }
    }

    /// The extension new archives of this format get (no dot).
    pub fn extension(self) -> &'static str {
        match self {
            ArchiveFormat::Zip => "zip",
            ArchiveFormat::SevenZip => "7z",
            ArchiveFormat::Tar(c) => c.tar_extension(),
            ArchiveFormat::Compressed(c) => c.extension(),
            ArchiveFormat::Rar => "rar",
        }
    }

    /// Whether archives of this format can hold a password.
    pub fn can_encrypt(self) -> bool {
        matches!(self, ArchiveFormat::Zip | ArchiveFormat::SevenZip)
    }

    /// Whether ZXraw can create and edit archives of this format (RAR: no).
    pub fn can_write(self) -> bool {
        self != ArchiveFormat::Rar
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
    /// RAR5 AES-256, whose password check value refuses a wrong password up front.
    RarAes,
    /// RAR 3.x/4.x AES-128: no password check, a wrong password fails the file's CRC.
    Rar4Aes,
}

impl EntryEncryption {
    /// Whether a data error of an entry encrypted this way may be a wrong password.
    pub fn ambiguous_errors(self) -> bool {
        matches!(
            self,
            EntryEncryption::ZipCrypto | EntryEncryption::SevenZipAes | EntryEncryption::Rar4Aes
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
    /// Its data has no checksum (no CRC of its own or of its block): decoding it with a wrong
    /// password can't be noticed. Such encrypted entries are held back unless the password
    /// is proven (see [`ExtractOptions::allow_unchecked_encrypted`]).
    #[serde(default)]
    pub unchecked: bool,
    /// Its only checksum is its solid block's (no CRC of its own): it is decoded to temp and
    /// published only once the whole block decoded and matched (see
    /// [`ArchiveBackend::read_entries_checked`]).
    #[serde(default)]
    pub checked_by_block: bool,
    /// The compressed block holding its data (7z), for grouping files of one block.
    #[serde(default)]
    pub block: Option<usize>,
    /// Decoded bytes up to the end of its block, when reading it means decoding the block to
    /// its end (a file of a compressed tar is published only once the whole stream decoded
    /// and its checksum matched); 0 otherwise.
    #[serde(default)]
    pub block_end: u64,
    /// Why this entry is listed but never extracted (tar hard links, devices, FIFOs).
    #[serde(default)]
    pub skip: Option<String>,
}

impl ArchiveEntry {
    /// Bytes decoded to read this entry: its block up to it, then its own data (or the whole
    /// block, see [`ArchiveEntry::block_end`]).
    pub fn decode_cost(&self) -> u64 {
        self.block_offset
            .saturating_add(self.size)
            .max(self.block_end)
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

/// Told the outcome of a block's checksum (see [`ArchiveBackend::read_entries_checked`]).
pub type BlockEnd<'a> = dyn FnMut(Result<()>) -> Result<()> + 'a;

/// Why files waiting for their solid block's checksum were not extracted.
pub const BLOCK_CHECK_FAILED: &str =
    "damaged data or wrong password: its solid block's checksum doesn't match";

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
    /// [`list`](Self::list) within `limits` (entries; for a tar, which is decoded to its end
    /// to be listed, also its decoders' memory and its decoded size, checked from its headers
    /// before anything is decoded), counting progress (bytes of the archive read) and
    /// checking for cancel in `ctx`.
    fn list_progress(
        &self,
        archive: &Path,
        limits: &ExtractLimits,
        password: Option<&Password>,
        ctx: Option<&JobCtx<'_>>,
    ) -> Result<ArchiveListing> {
        let _ = ctx;
        self.list(archive, limits.max_entries, password)
    }
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
    /// [`read_entries`](Self::read_entries), plus `block_end` after every block holding a
    /// wanted [`ArchiveEntry::checked_by_block`] entry, once the block was decoded to its end:
    /// `Ok` when its checksum matched, the error when it didn't (or the block failed). The
    /// default (no such blocks) is `read_entries`.
    fn read_entries_checked(
        &self,
        archive: &Path,
        wanted: &dyn Fn(&ArchiveEntry) -> bool,
        password: Option<&Password>,
        ctx: Option<&JobCtx<'_>>,
        visit: &mut EntryVisitor<'_>,
        block_end: &mut BlockEnd<'_>,
    ) -> Result<()> {
        let _ = block_end;
        self.read_entries(archive, wanted, password, ctx, visit)
    }
    /// Check `password` against the encrypted entries among `entries` before anything is
    /// extracted: [`CoreError::PasswordRequired`] / [`CoreError::WrongPassword`] when it can't
    /// open them, else whether a checksum proved it. The default opens the cheapest one to
    /// read (ZIP's encryption checks the password itself).
    fn check_password(
        &self,
        archive: &Path,
        entries: &[&ArchiveEntry],
        password: Option<&Password>,
    ) -> Result<PasswordProof> {
        match entries
            .iter()
            .filter(|e| e.encrypted && !e.is_dir)
            .min_by_key(|e| e.block_offset)
        {
            Some(first) => {
                open_one(self, archive, first.index, password).map(|()| PasswordProof::Proven)
            }
            None => Ok(PasswordProof::Proven),
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

/// How sure [`ArchiveBackend::check_password`] is of a password it didn't refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordProof {
    /// Checked (or nothing needs it).
    Proven,
    /// Nothing refused it, but no checksum confirmed it either (7z AES). Entries with a
    /// checksum are still verified as they are read; entries without one are held back.
    Unverified,
}

/// Why an encrypted entry without a checksum was not extracted (see
/// [`ExtractOptions::allow_unchecked_encrypted`]).
pub const UNCHECKED_REASON: &str =
    "the password can't be confirmed for this file (the archive stores no checksum)";

fn read_only(path: &Path, format: ArchiveFormat) -> CoreError {
    if format == ArchiveFormat::Rar {
        return CoreError::Unsupported(format!(
            "{}: {}",
            paths::display_path(path),
            rar_backend::READ_ONLY
        ));
    }
    CoreError::Invalid(format!(
        "{}: {} archives can't be written",
        path.display(),
        format.name()
    ))
}

static BACKENDS: &[&dyn ArchiveBackend] = &[
    &zip_backend::ZIP,
    &sevenz_backend::SEVEN_ZIP,
    &rar_backend::RAR,
];

/// Why RAR archives are never created or edited.
pub const RAR_READ_ONLY: &str = rar_backend::READ_ONLY;

/// Every registered backend.
pub fn backends() -> &'static [&'static dyn ArchiveBackend] {
    BACKENDS
}

/// The backend for a file name's extension, if any (no I/O). `x.tar.gz` and `x.tgz` are
/// tar archives; `x.gz` is a single compressed file.
pub fn backend_for_name(path: &Path) -> Option<&'static dyn ArchiveBackend> {
    if let Some((codec, tar)) = codec::by_name(&paths::name_of(path)) {
        return Some(if tar {
            tar_backend::tar_backend(codec)
        } else {
            tar_backend::single_backend(codec)
        });
    }
    let ext = crate::model::ext_of(path);
    if rar_backend::is_old_volume_ext(&ext) {
        // `x.r00`...: a later volume of an old-style RAR set (opened from `x.rar`).
        return Some(&rar_backend::RAR);
    }
    BACKENDS
        .iter()
        .copied()
        .find(|b| b.extensions().contains(&ext.as_str()))
}

/// Whether a file name has a browsable archive extension (no I/O). A `.exe` is not one: it
/// runs. See [`is_archive_path`] for the one the user opened as an archive.
pub fn is_archive_name(path: &Path) -> bool {
    backend_for_name(path).is_some()
}

/// Whether a path may be browsed as an archive: a name with an archive extension, or a file
/// the user opened as one (a self-extracting `.exe`, see [`sfx::mark_open_as_archive`]).
pub fn is_archive_path(path: &Path) -> bool {
    is_archive_name(path) || sfx::is_open_as_archive(path)
}

/// The backend for an archive file, by what it holds: its signature (a 7z renamed `.zip`
/// is still read as 7z, and edited as 7z; a compression stream is a tar when a tar header
/// starts its decoded data, else a single compressed file), then a tar header, then a
/// self-extractor's payload (a program stub in front: see [`sfx`]), then a ZIP end record
/// near its end, then its extension.
pub fn backend_for_file(path: &Path) -> Result<&'static dyn ArchiveBackend> {
    let mut file = fs::File::open(path).map_err(|e| CoreError::io(path, e))?;
    let mut head = [0u8; 512];
    let mut n = 0;
    while n < head.len() {
        match file.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(CoreError::io(path, e)),
        }
    }
    if let Some(b) = BACKENDS.iter().copied().find(|b| b.sniff(&head[..n])) {
        return Ok(b);
    }
    if let Some(c) = codec::sniff(&head[..n]) {
        return Ok(tar_backend::stream_backend(path, c));
    }
    // Before the end record: a tar whose last file is a zip has one near its end.
    if tar_read::looks_like_tar(&head[..n]) {
        return Ok(tar_backend::tar_backend(Codec::None));
    }
    if head[..n].starts_with(b"MZ") {
        if let Some(b) = sfx::sfx_backend(path)? {
            return Ok(b);
        }
    }
    if zip_backend::has_end_record(&mut file) {
        return Ok(&zip_backend::ZIP);
    }
    // Brotli has no signature: by name, then a tar inside or not.
    if let Some((Codec::Brotli, _)) = codec::by_name(&paths::name_of(path)) {
        return Ok(tar_backend::stream_backend(path, Codec::Brotli));
    }
    backend_for_name(path)
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
        if !matches!(c, Component::Normal(_)) || !is_archive_path(&prefix) {
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

/// Cached indexes by archive path: the [`archive_stamp`] value they were built from, the index,
/// and its weight. Only strong stamps are cached (see [`index_cached_stamped`]).
type IndexCache = Mutex<lru::LruCache<PathBuf, (u64, Arc<ArchiveIndex>, u64)>>;

/// Memory the cached indexes may take together (a big tar's names can be hundreds of MB):
/// the least recently used go first, and one heavier than this isn't kept.
const INDEX_CACHE_BUDGET: u64 = 512 << 20;

/// The memory of an index (see [`ArchiveIndex::memory_estimate`]).
fn index_weight(idx: &ArchiveIndex) -> u64 {
    idx.memory_estimate()
}

fn cache() -> &'static IndexCache {
    static CACHE: OnceLock<IndexCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(lru::LruCache::new(std::num::NonZeroUsize::new(8).unwrap())))
}

/// The browsing index of an archive, cached while its [`archive_stamp`] stays the same (the
/// central directory is read once, not on every folder change). A weak stamp is never cached.
pub fn index_cached(archive: &Path) -> Result<Arc<ArchiveIndex>> {
    index_cached_with(archive, None)
}

/// [`index_cached`] for an archive whose names may be encrypted (7z): `password` is used
/// only when the index isn't cached yet. Errors as [`ArchiveBackend::list`].
pub fn index_cached_with(archive: &Path, password: Option<&Password>) -> Result<Arc<ArchiveIndex>> {
    index_cached_progress(archive, password, None)
}

/// [`index_cached_with`], counting progress and checking for cancel in `ctx` while the
/// listing is built (a compressed tar is decoded to its end to list it).
pub fn index_cached_progress(
    archive: &Path,
    password: Option<&Password>,
    ctx: Option<&JobCtx<'_>>,
) -> Result<Arc<ArchiveIndex>> {
    index_cached_stamped(archive, archive_stamp(archive)?, password, ctx)
}

/// [`index_cached_progress`] for an archive whose `stamp` ([`archive_stamp`]) the caller took.
///
/// A weak stamp (no file id) can't tell a replaced archive from the old one, so its index is
/// always listed again and not kept. The cost is one listing per call: a zip, 7z or rar reads
/// its directory; a compressed tar is decoded to its end (the only format where this is slow).
pub fn index_cached_stamped(
    archive: &Path,
    stamp: ArchiveStamp,
    password: Option<&Password>,
    ctx: Option<&JobCtx<'_>>,
) -> Result<Arc<ArchiveIndex>> {
    let lookup = paths::lexical_normalize(archive);
    if let Some((s, idx, _)) = cache().lock().get(&lookup)
        && stamp.strong
        && *s == stamp.value
    {
        return Ok(idx.clone());
    }
    // The tar walk cache is keyed by size and time too: a replaced file must not get its walk.
    tar_backend::forget(archive);
    let listing = backend_for_file(archive)?.list_progress(
        archive,
        &ExtractLimits::default(),
        password,
        ctx,
    )?;
    let idx = Arc::new(ArchiveIndex::build(listing));
    let weight = index_weight(&idx);
    let mut cache = cache().lock();
    cache.pop(&lookup);
    if stamp.strong && weight <= INDEX_CACHE_BUDGET {
        let mut total = weight + cache.iter().map(|(_, (_, _, w))| *w).sum::<u64>();
        while total > INDEX_CACHE_BUDGET
            && let Some((_, (_, _, w))) = cache.pop_lru()
        {
            total -= w;
        }
        cache.put(lookup, (stamp.value, idx.clone(), weight));
    }
    Ok(idx)
}

/// Forget the cached index of an archive (after it was edited).
pub fn invalidate_cached(archive: &Path) {
    cache().lock().pop(&paths::lexical_normalize(archive));
    tar_backend::forget(archive);
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
    /// Extract encrypted entries that have no checksum even when the password is not proven
    /// (a wrong one would write garbage unnoticed). Off: they are skipped with
    /// [`UNCHECKED_REASON`] and listed in [`ExtractReport::unchecked`].
    #[serde(default)]
    pub allow_unchecked_encrypted: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            limits: ExtractLimits::default(),
            policy: ConflictPolicy::default(),
            password: None,
            mark_of_the_web: true,
            allow_unchecked_encrypted: false,
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
    /// Names of encrypted entries without a checksum skipped because the password was not
    /// proven (also in `skipped`): extract them with `allow_unchecked_encrypted` if wanted.
    pub unchecked: Vec<String>,
    /// [`extract_to`] made `dest` itself (its `create_dir` succeeded): only then may a caller
    /// remove it again.
    #[serde(default)]
    pub created_dest: bool,
}

/// List for a job: cancel stops a long listing (a compressed tar is decoded to its end);
/// its progress is not counted as the job's.
fn list_cancellable(
    backend: &dyn ArchiveBackend,
    archive: &Path,
    limits: &ExtractLimits,
    password: Option<&Password>,
    ctx: &JobCtx<'_>,
) -> Result<ArchiveListing> {
    let quiet = forge_jobs::Progress::default();
    let only_cancel = JobCtx::new(ctx.cancel, &quiet);
    backend.list_progress(archive, limits, password, Some(&only_cancel))
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
    // The destination, resolved and pinned from the volume root for the whole job, before
    // the (possibly long) planning: entries are written beneath its final path and reported
    // beneath `dest_dir`.
    let base = safety::PinnedBase::open(dest_dir)?;
    extract_pinned(archive, &base, dest_dir, opts, selection, ctx, None)
}

/// Something an extraction put in place: a file it published or a folder it made, beneath
/// its destination, with the identity it had then.
struct Published {
    rel: PathBuf,
    identity: Option<ops::FileIdentity>,
    dir: bool,
    /// It replaced a file that was already there: never taken back (the old one is gone).
    replaced: bool,
}

/// [`extract`] into `base`, a destination already pinned (with its ancestors, while the
/// caller holds them); reports show paths beneath `dest_dir`. With `track`, every file
/// published and folder made is recorded there as it happens (kept when the job fails).
fn extract_pinned(
    archive: &Path,
    base: &safety::PinnedBase,
    dest_dir: &Path,
    opts: &ExtractOptions,
    selection: Option<&Selection>,
    ctx: &JobCtx<'_>,
    track: Option<&std::cell::RefCell<Vec<Published>>>,
) -> Result<ExtractReport> {
    let record = |path: &Path, identity: Option<ops::FileIdentity>, dir: bool, replaced: bool| {
        if let Some(track) = track
            && let Ok(rel) = path.strip_prefix(base.root())
        {
            track.borrow_mut().push(Published {
                rel: rel.to_path_buf(),
                identity,
                dir,
                replaced,
            });
        }
    };
    #[cfg(test)]
    safety::run_hook(&safety::AFTER_DEST_PINNED, base.root());
    let backend = backend_for_file(archive)?;
    let limits = &opts.limits;
    let listing = list_cancellable(backend, archive, limits, opts.password.as_ref(), ctx)?;
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
        if let Some(reason) = &e.skip {
            report.skipped.push(OpIssue::new(&rel, reason.clone()));
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
    let proof = backend.check_password(archive, &planned_entries, password)?;
    if proof == PasswordProof::Unverified && !opts.allow_unchecked_encrypted {
        // With a password nothing confirmed, a file nothing checks could be garbage.
        planned.retain(|p| {
            if !(p.entry.encrypted && p.entry.unchecked && !p.entry.is_dir) {
                return true;
            }
            report.skipped.push(OpIssue::new(&p.rel, UNCHECKED_REASON));
            report.unchecked.push(p.entry.name.clone());
            selected_total = selected_total.saturating_sub(p.entry.size);
            false
        });
    }
    // Blocks with more files to hold than handles we keep open are checked first (decoded
    // once to verify their checksum, after the preflight above), then extracted directly.
    let verified_blocks =
        verify_crowded_blocks(backend, archive, password, ctx, &mut planned, &mut report)?;
    let root = base.root();
    // Leftovers of crashed or older runs; before any entry of this job is written.
    remove_leftovers(base, &planned);
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
    // What reports show: the same item beneath the destination as the user named it.
    let shown = |path: &Path| match path.strip_prefix(root) {
        Ok(rel) => dest_dir.join(rel),
        Err(_) => path.to_path_buf(),
    };
    // Files waiting for their solid block's checksum (temp files, deleted if dropped), with
    // their folder inside the destination; and what became of them.
    // Each held file keeps its open handle and the pins of the folders it goes into until it
    // is published or dropped: neither it nor its folders can be swapped meanwhile.
    // Also whether it replaces a file already there.
    let held: std::cell::RefCell<Vec<(ops::HeldFile, PathBuf, safety::PinnedDirs, bool)>> =
        Default::default();
    let settled: std::cell::RefCell<(Vec<PathBuf>, Vec<OpIssue>)> = Default::default();
    let mut visit = |entry: &ArchiveEntry, reader: Result<&mut dyn Read>| -> Result<()> {
        ctx.cancel.check()?;
        let p = &planned[by_index[&entry.index]];
        ctx.progress.add_items(1);
        // Read from a listing of the archive made again (it changed since it was checked):
        // never under another entry's name.
        if entry.name != p.entry.name || entry.size != p.entry.size {
            report.failed.push(OpIssue::new(
                &p.rel,
                "the archive changed while it was extracted",
            ));
            return Ok(());
        }
        ctx.progress.set_current(entry.name.clone());
        let target = root.join(&p.rel);
        let parent_rel = if p.entry.is_dir {
            p.rel.as_path()
        } else {
            p.rel.parent().unwrap_or(Path::new(""))
        };
        // Held until the entry is in place: the folders it is written into can't be swapped.
        let mut made = |rel: &Path, identity| record(&root.join(rel), identity, true, false);
        let pinned = match safety::pin_dirs_with(base, parent_rel, &mut made) {
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
        let verified = p.entry.block.is_some_and(|b| verified_blocks.contains(&b));
        if p.entry.checked_by_block && !verified {
            return match ops::write_stream_held(reader, &label, &dest, sopts, keep_both, ctx) {
                Ok((out, file)) => {
                    remaining = remaining.saturating_sub(out.bytes);
                    report.bytes += out.bytes;
                    held.borrow_mut()
                        .push((file, shown(&dest), pinned, overwrite));
                    Ok(())
                }
                Err(CoreError::Cancelled) => Err(CoreError::Cancelled),
                Err(e) => {
                    report
                        .failed
                        .push(OpIssue::new(shown(&dest), data_error(&p.entry, e)));
                    Ok(())
                }
            };
        }
        match ops::write_stream_placed(reader, &label, &dest, sopts, keep_both, ctx) {
            Ok((out, placed, identity)) => {
                // `overwrite` only when a file had the name: else publishing never replaces.
                record(&placed, identity, false, overwrite);
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
    // The block decoded to its end: publish what waited for it, or drop it all.
    let mut block_end = |outcome: Result<()>| -> Result<()> {
        let waiting = std::mem::take(&mut *held.borrow_mut());
        let mut settled = settled.borrow_mut();
        for (file, label, pinned, replaced) in waiting {
            if outcome.is_err() {
                drop(file); // deleted through its handle
                settled.1.push(OpIssue::new(label, BLOCK_CHECK_FAILED));
                continue;
            }
            // Still pinned since it was written; renamed through its own handle.
            let identity = file.identity();
            let published = file.publish();
            drop(pinned);
            match published {
                Ok(placed) => {
                    record(&placed, identity, false, replaced);
                    if let Some(zone) = &zone
                        && let Err(e) = motw::write_zone(&placed, zone, archive)
                    {
                        tracing::warn!("could not mark {} as downloaded: {e}", placed.display());
                    }
                    settled.0.push(shown(&placed));
                }
                Err(e) => settled.1.push(OpIssue::new(label, e.to_string())),
            }
        }
        Ok(())
    };
    let wanted = |e: &ArchiveEntry| by_index.contains_key(&e.index);
    let result = backend.read_entries_checked(
        archive,
        &wanted,
        password,
        Some(ctx),
        &mut visit,
        &mut block_end,
    );
    // Anything still waiting (cancelled mid-block) is dropped: nothing of it is published.
    held.borrow_mut().clear();
    let (published, failed) = settled.into_inner();
    report.extracted.extend(published);
    report.failed.extend(failed);
    match result {
        Ok(()) => {}
        Err(CoreError::Cancelled) => report.cancelled = true,
        Err(e) => return Err(e),
    }
    Ok(report)
}

/// Most files held open per solid block while its checksum is pending (each holds a handle
/// and its folders' pins).
const MAX_HELD_PER_BLOCK: usize = 4096;

#[cfg(test)]
thread_local! {
    /// Tests: a smaller [`MAX_HELD_PER_BLOCK`].
    pub(crate) static HELD_CAP: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn held_cap() -> usize {
    #[cfg(test)]
    if let Some(cap) = HELD_CAP.with(|c| c.get()) {
        return cap;
    }
    MAX_HELD_PER_BLOCK
}

/// Blocks holding more planned [`ArchiveEntry::checked_by_block`] files than
/// [`MAX_HELD_PER_BLOCK`] are decoded once first, only to check their checksum. Those that
/// match are returned (their files are then extracted directly); the files of those that
/// don't are taken out of `planned` and reported failed.
fn verify_crowded_blocks(
    backend: &dyn ArchiveBackend,
    archive: &Path,
    password: Option<&Password>,
    ctx: &JobCtx<'_>,
    planned: &mut Vec<Planned>,
    report: &mut ExtractReport,
) -> Result<HashSet<usize>> {
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for p in planned.iter().filter(|p| p.entry.checked_by_block) {
        if let Some(b) = p.entry.block {
            *counts.entry(b).or_default() += 1;
        }
    }
    let mut crowded: Vec<usize> = counts
        .into_iter()
        .filter(|&(_, n)| n > held_cap())
        .map(|(b, _)| b)
        .collect();
    if crowded.is_empty() {
        return Ok(HashSet::new());
    }
    crowded.sort_unstable();
    ctx.progress
        .set_note("Checking solid blocks before extracting (too many files to hold)");
    let checked: HashSet<usize> = crowded.iter().copied().collect();
    let in_crowded =
        |e: &ArchiveEntry| e.checked_by_block && e.block.is_some_and(|b| checked.contains(&b));
    let names: HashSet<usize> = planned
        .iter()
        .filter(|p| in_crowded(&p.entry))
        .map(|p| p.entry.index)
        .collect();
    let mut buf = vec![0u8; 256 * 1024];
    let mut drain = |_: &ArchiveEntry, reader: Result<&mut dyn Read>| -> Result<()> {
        if let Ok(r) = reader {
            loop {
                ctx.cancel.check()?;
                match r.read(&mut buf) {
                    Ok(0) | Err(_) => break, // a bad read fails the block's check
                    Ok(_) => {}
                }
            }
        }
        Ok(())
    };
    // One outcome per crowded block, in block order (as the backend reads them).
    let mut outcomes = Vec::new();
    let mut block_end = |outcome: Result<()>| -> Result<()> {
        outcomes.push(outcome.is_ok());
        Ok(())
    };
    backend.read_entries_checked(
        archive,
        &|e| names.contains(&e.index),
        password,
        Some(ctx),
        &mut drain,
        &mut block_end,
    )?;
    ctx.progress.set_note("");
    let mut verified = HashSet::new();
    for (i, b) in crowded.iter().enumerate() {
        if outcomes.get(i).copied().unwrap_or(false) {
            verified.insert(*b);
        }
    }
    planned.retain(|p| {
        let bad = in_crowded(&p.entry) && p.entry.block.is_some_and(|b| !verified.contains(&b));
        if bad {
            report.failed.push(OpIssue::new(&p.rel, BLOCK_CHECK_FAILED));
        }
        !bad
    });
    Ok(verified)
}

/// The reason shown for an entry whose data could not be read.
fn data_error(entry: &ArchiveEntry, e: CoreError) -> String {
    if is_password_error(&e) {
        return e.to_string();
    }
    let text = e.to_string();
    // A disk error is what it says; bad data (the 7z reader reports it as `InvalidData`)
    // may be the password.
    let disk_error =
        matches!(&e, CoreError::Io { source, .. } if source.kind() != io::ErrorKind::InvalidData);
    if entry
        .encryption
        .is_some_and(EntryEncryption::ambiguous_errors)
        && !disk_error
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

/// The archive's name without its extension(s) (`photos.tar.gz` gives `photos`).
pub fn archive_stem(archive: &Path) -> String {
    let name = paths::name_of(archive);
    let mut stem = match name.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s.to_owned(),
        _ => name,
    };
    if stem.len() > 4 && stem.to_lowercase().ends_with(".tar") {
        stem.truncate(stem.len() - 4);
    }
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
    let listing = list_cancellable(backend, archive, &opts.limits, opts.password.as_ref(), ctx)?;
    let nest = match target {
        ExtractTarget::NamedFolder => true,
        ExtractTarget::Smart => matches!(top_level(&listing), TopLevel::Many),
    };
    if !nest {
        return extract(archive, parent, opts, None, ctx);
    }
    let stem = archive_stem(archive);
    let (name, reuse) = match target {
        ExtractTarget::NamedFolder => (stem, true),
        // Smart: never merge into an unrelated folder; use the next free name.
        ExtractTarget::Smart => (paths::name_of(&paths::unique_name(parent, &stem)), false),
    };
    let folder = NewFolder {
        parent,
        name: &name,
        reuse,
    };
    extract_into_folder(archive, folder, opts, None, ctx, |result| match result {
        Err(_) => true,
        Ok(r) => r.cancelled || r.extracted.is_empty() && !r.failed.is_empty(),
    })
}

/// The folder [`extract_into_folder`] extracts into.
#[derive(Debug, Clone, Copy)]
pub struct NewFolder<'a> {
    /// The existing folder it is made in.
    pub parent: &'a Path,
    /// Its name (one plain name, no separators).
    pub name: &'a str,
    /// Extract into it when it already exists (a real folder, not a link); else that fails.
    pub reuse: bool,
}

/// Make the folder `folder.name` in `folder.parent` and extract `archive` (all of it, or
/// `selection`) into it. `parent` (from the volume root down) is pinned before the folder is
/// made, and the folder as soon as it is: neither can be renamed or swapped for a link from
/// then until the job, including taking back what it wrote, is over. The report's
/// `created_dest` says whether this call made the folder.
///
/// When it did and `undo_if` holds for the result, the files it published, then the folders
/// it made (if empty) and the folder itself (if empty) are removed again, all while still
/// pinned and each only if it is still the item the job made (see [`remove_created_tree`]);
/// a file that replaced one already there, anything replaced meanwhile and anything only the
/// weak identity (no file id) matched stay and are added to the report's `failed`.
pub fn extract_into_folder(
    archive: &Path,
    folder: NewFolder<'_>,
    opts: &ExtractOptions,
    selection: Option<&Selection>,
    ctx: &JobCtx<'_>,
    undo_if: impl FnOnce(&Result<ExtractReport>) -> bool,
) -> Result<ExtractReport> {
    let name = std::ffi::OsStr::new(folder.name);
    let shown_dest = folder.parent.join(name);
    let parent = safety::PinnedBase::open(folder.parent)?;
    let real = parent.root().join(name);
    let created = match fs::create_dir(&real) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && folder.reuse => false,
        Err(e) => return Err(CoreError::io(&shown_dest, e)),
    };
    // Checked by handle (not a link) and pinned from here on.
    let dest = match parent.child(name) {
        Ok(dest) => dest,
        Err(e) => {
            if created {
                let _ = fs::remove_dir(&real); // only if nothing is in it
            }
            return Err(e);
        }
    };
    let published = std::cell::RefCell::new(Vec::new());
    let mut result = extract_pinned(
        archive,
        &dest,
        &shown_dest,
        opts,
        selection,
        ctx,
        Some(&published),
    )
    .map(|mut r| {
        r.created_dest = created;
        r
    });
    if created && undo_if(&result) {
        #[cfg(test)]
        safety::run_hook(&safety::BEFORE_CREATED_CLEANUP, dest.root());
        let left = remove_created_tree(&parent, dest, &shown_dest, published.into_inner());
        match &mut result {
            Ok(report) => report.failed.extend(left),
            Err(_) => {
                for issue in left {
                    tracing::warn!("{}: {}", issue.path.display(), issue.reason);
                }
            }
        }
    }
    drop(parent);
    result
}

/// Message for something the extraction wrote that was replaced before it was taken back.
const CHANGED_AFTER_EXTRACTION: &str = "changed by another program after extraction; left in place";
/// Message for something only the weak identity (no file id) matched: never taken back.
const UNCONFIRMED: &str = "couldn't confirm it's the extracted copy; left in place";
/// Message for a file the extraction wrote over an existing one: never taken back.
const REPLACED_KEPT: &str = "replaced an existing file; the new copy is kept";

/// Take back what an extraction put in the folder it created (`dest`, inside `parent`,
/// reported beneath `shown_dest`): the files it published, then the folders it made, deepest
/// first, then the folder itself, each only if it is still the one the job put there (same
/// identity, proven by a file id) and a folder only if empty. A file that replaced one
/// already there is kept (taking it back would leave nothing). Each is opened relative to its
/// pinned parent folder (never through a link) and deleted through that handle. Returns what
/// was left in place because it was a replacement, changed, couldn't be confirmed or could
/// not be removed.
fn remove_created_tree(
    parent: &safety::PinnedBase,
    dest: safety::PinnedBase,
    shown_dest: &Path,
    mut published: Vec<Published>,
) -> Vec<OpIssue> {
    let mut left = Vec::new();
    // Files first, then folders deepest first (a folder only goes once it is empty).
    published.sort_by_key(|p| (p.dir, std::cmp::Reverse(p.rel.components().count())));
    for p in &published {
        let Some(name) = p.rel.file_name() else {
            continue;
        };
        let parent_rel = p.rel.parent().unwrap_or(Path::new(""));
        let shown = shown_dest.join(&p.rel);
        if p.replaced {
            left.push(OpIssue::new(shown, REPLACED_KEPT));
            continue;
        }
        let pins = match safety::pin_existing(&dest, parent_rel) {
            Ok(pins) => pins,
            // Its folder is gone: so is it.
            Err(CoreError::NotFound(_)) => continue,
            Err(e) => {
                if !p.dir {
                    left.push(OpIssue::new(
                        shown,
                        format!("its folder changed after extraction; left in place ({e})"),
                    ));
                }
                continue;
            }
        };
        let folder = pins.last().unwrap_or_else(|| dest.handle());
        match safety::remove_if_same(folder, name, p.dir, p.identity) {
            safety::Removal::Removed | safety::Removal::Gone | safety::Removal::NotEmpty => {}
            safety::Removal::Changed => left.push(OpIssue::new(shown, CHANGED_AFTER_EXTRACTION)),
            safety::Removal::Unconfirmed => left.push(OpIssue::new(shown, UNCONFIRMED)),
            safety::Removal::Failed(e) => left.push(OpIssue::new(
                shown,
                format!("could not be removed after the extraction stopped: {e}"),
            )),
        }
    }
    // Its own pin denies deleting it: let go of it (its ancestors stay pinned by `parent`).
    let identity = dest.identity();
    let name = dest.root().file_name().map(std::ffi::OsStr::to_os_string);
    drop(dest);
    if let Some(name) = name {
        match safety::remove_if_same(parent.handle(), &name, true, identity) {
            safety::Removal::Changed => {
                left.push(OpIssue::new(shown_dest, CHANGED_AFTER_EXTRACTION));
            }
            safety::Removal::Unconfirmed => left.push(OpIssue::new(shown_dest, UNCONFIRMED)),
            safety::Removal::Failed(e) => {
                tracing::warn!("could not remove {}: {e}", shown_dest.display());
            }
            _ => {}
        }
    }
    left
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
    /// It decoded, but nothing can confirm it: an encrypted file without any checksum and a
    /// password no checksum proved. Neither OK nor failed.
    #[serde(default)]
    pub unverified: bool,
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
    /// Files that decoded but can't be verified (see [`EntryTest::unverified`]).
    pub fn unverified(&self) -> impl Iterator<Item = &EntryTest> {
        self.entries
            .iter()
            .filter(|e| e.error.is_none() && e.unverified)
    }
    /// Everything decoded and was verified.
    pub fn is_ok(&self) -> bool {
        !self.cancelled && self.failures().next().is_none() && self.unverified().next().is_none()
    }
}

/// Why a tested file is neither OK nor failed.
pub const CANT_VERIFY: &str = "can't be verified (no checksum)";

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
    let listing = list_cancellable(backend, archive, limits, password, ctx)?;
    let files: Vec<&ArchiveEntry> = listing
        .entries
        .iter()
        .filter(|e| !e.is_dir && e.skip.is_none())
        .collect();
    let indices: Vec<usize> = files.iter().map(|e| e.index).collect();
    backend.preflight(archive, &indices, password, limits)?;
    // Testing writes nothing: every file is decoded. Under a password no checksum proved,
    // an encrypted file without any checksum is reported as unverifiable, not OK.
    let proof = backend.check_password(archive, &files, password)?;
    ctx.progress.add_total_items(files.len() as u64);
    ctx.progress
        .add_total_bytes(files.iter().fold(0u64, |a, e| a.saturating_add(e.size)));
    let mut report = TestReport::default();
    let mut buf = vec![0u8; 256 * 1024];
    // Files whose only checksum is their solid block's, by their place in the report, until
    // the block's end says whether they are fine.
    let pending: std::cell::RefCell<Vec<usize>> = Default::default();
    let bad: std::cell::RefCell<Vec<usize>> = Default::default();
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
        if error.is_none() && entry.checked_by_block {
            pending.borrow_mut().push(report.entries.len());
        }
        report.entries.push(EntryTest {
            name: entry.name.clone(),
            size: entry.size,
            // No checksum at all (a plain tar, Brotli), or encrypted without one under a
            // password nothing proved.
            unverified: error.is_none()
                && entry.unchecked
                && (!entry.encrypted || proof == PasswordProof::Unverified),
            error,
        });
        Ok(())
    };
    let mut block_end = |outcome: Result<()>| -> Result<()> {
        let waiting = std::mem::take(&mut *pending.borrow_mut());
        if outcome.is_err() {
            bad.borrow_mut().extend(waiting);
        }
        Ok(())
    };
    let wanted = |e: &ArchiveEntry| !e.is_dir && e.skip.is_none();
    let result = backend.read_entries_checked(
        archive,
        &wanted,
        password,
        Some(ctx),
        &mut visit,
        &mut block_end,
    );
    for i in bad.into_inner() {
        if let Some(e) = report.entries.get_mut(i) {
            e.error = Some(BLOCK_CHECK_FAILED.to_owned());
        }
    }
    // Decoded, but the block's checksum was never reached (cancelled): not verified.
    for i in pending.into_inner() {
        if let Some(e) = report.entries.get_mut(i) {
            e.unverified = true;
        }
    }
    match result {
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
/// earlier from the same unchanged archive is reused, but only under a strong stamp: a weak
/// one can't rule out a replaced archive, so the entry is always extracted again (one entry,
/// read from the archive as it is now). Fails for entries over `max_bytes`.
pub fn extract_entry_to_temp(
    archive: &Path,
    entry_name: &str,
    password: Option<&Password>,
    max_bytes: u64,
    ctx: &JobCtx<'_>,
) -> Result<PathBuf> {
    let stamp = archive_stamp(archive)?;
    let key = format!(
        "{}|{:016x}",
        paths::lexical_normalize(archive)
            .display()
            .to_string()
            .to_lowercase(),
        stamp.value
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
    let index = index_cached_stamped(archive, stamp, password, None)?;
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
    let reusable = stamp.strong
        && node.modified.is_some()
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
        // Opened files are never garbage from an unproven password.
        allow_unchecked_encrypted: false,
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

/// Leftover temp files older than this are removed from a folder an extraction writes into.
/// A running job's files are always younger (they are written and renamed in seconds).
const LEFTOVER_AGE: Duration = Duration::from_secs(3600);

/// Remove the leftover temp files (see [`ops::remove_stale_partials`]) from the destination
/// and from each folder of `planned` that files are written into, when the folder already
/// exists. A missing folder is never created here. Each folder is pinned while it is cleaned.
fn remove_leftovers(base: &safety::PinnedBase, planned: &[Planned]) {
    ops::remove_stale_partials(base.root(), LEFTOVER_AGE);
    let folders: HashSet<&Path> = planned
        .iter()
        .filter(|p| !p.entry.is_dir)
        .filter_map(|p| p.rel.parent())
        .filter(|rel| !rel.as_os_str().is_empty())
        .collect();
    for rel in folders {
        let path = base.root().join(rel);
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        // Held while cleaned: the folder cannot be swapped for a link meanwhile.
        if let Ok(_pinned) = safety::pin_dirs(base, rel) {
            ops::remove_stale_partials(&path, LEFTOVER_AGE);
        }
    }
}

/// Remove temp folders of opened entries that were last written more than `older_than` ago,
/// and stray temp files older than that (see [`prune_dir`]).
pub fn prune_temp(older_than: Duration) {
    prune_dir(&temp_root(), older_than);
}

/// The startup prune, for `root`. Only top-level entries are touched:
/// - A stage folder `out-<pid>-<id>` of a process still running is kept at any age.
/// - A folder is removed only when it and every direct child were last written more than
///   `older_than` ago, so a file still being written keeps its folder.
/// - A link or junction is removed as the link itself, never followed.
/// - Files in use fail to delete and are ignored.
fn prune_dir(root: &Path, older_than: Duration) {
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    ops::remove_stale_partials(root, older_than);
    for e in rd.flatten() {
        let path = e.path();
        let Ok(md) = fs::symlink_metadata(&path) else {
            continue;
        };
        if safety::is_link_or_reparse(&md) {
            if ops::is_older_than(md.modified(), older_than) {
                let _ = fs::remove_dir(&path).or_else(|_| fs::remove_file(&path));
            }
            continue;
        }
        if !md.is_dir() || stage_owner_alive(&e.file_name().to_string_lossy()) {
            continue;
        }
        if ops::is_older_than(md.modified(), older_than) && children_older_than(&path, older_than) {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// Is every direct child of `dir` last written more than `age` ago? Unreadable counts as not.
fn children_older_than(dir: &Path, age: Duration) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    rd.into_iter().all(|c| {
        c.is_ok_and(|c| {
            ops::is_older_than(
                fs::symlink_metadata(c.path()).and_then(|m| m.modified()),
                age,
            )
        })
    })
}

/// Is `name` a stage folder `out-<pid>-<id>` whose process is still running?
fn stage_owner_alive(name: &str) -> bool {
    let Some((pid, id)) = name
        .strip_prefix("out-")
        .and_then(|rest| rest.split_once('-'))
    else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(pid) && digits(id) && pid.parse::<u32>().is_ok_and(process_alive)
}

/// Is the process with this id still running? An id that cannot be opened is dead, except
/// when access is denied (the process exists but is not ours to query).
#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: the handle is used and closed here only; the out value is a valid u32.
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => {
                let mut code = 0u32;
                let alive =
                    GetExitCodeProcess(handle, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
                let _ = CloseHandle(handle);
                alive
            }
            Err(e) => e.code() == ERROR_ACCESS_DENIED.to_hresult(),
        }
    }
}

/// Without Windows process queries every stage folder is treated as live (never pruned early).
#[cfg(not(windows))]
fn process_alive(_: u32) -> bool {
    true
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
    sfx::refuse_edit_if_sfx(archive)?;
    let backend = backend_for_file(archive)?;
    if !backend.can_write() {
        // RAR: refused before anything is read.
        return Err(read_only(archive, backend.format()));
    }
    if backend.format() == ArchiveFormat::Zip {
        // Before anything reads the directory: a split zip can't be rebuilt as one file.
        let mut file = fs::File::open(archive).map_err(|e| CoreError::io(archive, e))?;
        zip_merge::check_single_disk(archive, &mut file)?;
        zip_backend::check_no_prefix(archive, &mut file)?;
    }
    check_editable_names(backend, archive, opts)?;
    let warnings = check_update_password(backend, archive, edit, opts)?;
    let result = backend.update(archive, edit, opts, ctx).map(|mut report| {
        report.warnings.extend(warnings);
        report
    });
    invalidate_cached(archive);
    result
}

/// Why an archive whose names collide isn't edited (see [`has_colliding_names`]).
pub const CASE_COLLISION: &str = "This archive has names that differ only in capitals \
     (e.g. A.txt and a.txt); ZXraw can't edit it safely. Extract it instead.";
/// The same, for a file and a folder of one name, or one name given twice.
pub const NAME_COLLISION: &str = "This archive has a file and a folder of the same name, or \
     the same name twice; ZXraw can't edit it safely. Extract it instead.";

/// How two names of an archive collide (see [`name_collision`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameCollision {
    /// Names that differ only in capitals (`A.txt` and `a.txt`).
    Case,
    /// A file and a folder of the same name (`docs` and `docs/`, or `docs/x.txt`).
    FileAndFolder,
    /// The same name twice.
    Duplicate,
}

/// Whether two entries of `entries` would be one path on Windows (see [`NameCollision`]);
/// folders named only by the paths inside them count. Edits keep or drop entries by name,
/// so an archive like this is never edited: an edit could lose an entry it wasn't asked to
/// delete.
pub fn name_collision(entries: &[ArchiveEntry]) -> Option<NameCollision> {
    // Lowercase name -> (the name as written, a folder?).
    let mut seen: HashMap<String, (&str, bool)> = HashMap::with_capacity(entries.len());
    let mut folders: HashSet<String> = HashSet::new();
    for e in entries {
        let name = e.name.trim_end_matches(['/', '\\']);
        if name.is_empty() {
            continue;
        }
        let key = name.replace('\\', "/").to_lowercase();
        let is_dir = e.is_dir || e.name.ends_with(['/', '\\']);
        if let Some(&(first, first_dir)) = seen.get(&key) {
            return Some(if first != name {
                NameCollision::Case
            } else if first_dir != is_dir {
                NameCollision::FileAndFolder
            } else {
                NameCollision::Duplicate
            });
        }
        let mut parent = key.as_str();
        while let Some((p, _)) = parent.rsplit_once('/') {
            if !folders.insert(p.to_owned()) {
                break;
            }
            parent = p;
        }
        seen.insert(key, (name, is_dir));
    }
    seen.iter()
        .any(|(key, (_, is_dir))| !is_dir && folders.contains(key))
        .then_some(NameCollision::FileAndFolder)
}

/// Whether two entries of `entries` would be one path on Windows (see [`name_collision`]).
pub fn has_colliding_names(entries: &[ArchiveEntry]) -> bool {
    name_collision(entries).is_some()
}

/// Refuse to edit an archive with colliding names (or, for a tar, a name given twice: only
/// the last one is listed), before anything is written.
fn check_editable_names(
    backend: &dyn ArchiveBackend,
    archive: &Path,
    opts: &CreateOptions,
) -> Result<()> {
    let listing = backend.list(
        archive,
        DEFAULT_MAX_ARCHIVE_ENTRIES,
        opts.encryption.password(),
    )?;
    let collision = match name_collision(&listing.entries) {
        Some(c) => Some(c),
        None => match backend.format() {
            ArchiveFormat::Tar(codec) if tar_backend::has_superseded(archive, codec)? => {
                Some(NameCollision::Duplicate)
            }
            _ => None,
        },
    };
    match collision {
        None => Ok(()),
        Some(NameCollision::Case) => Err(CoreError::Unsupported(CASE_COLLISION.into())),
        Some(_) => Err(CoreError::Unsupported(NAME_COLLISION.into())),
    }
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
        // 7z AES has no header check to sample: the backend decodes the encrypted blocks
        // from their start until a file's CRC proves the password (opening a file in the
        // middle of a solid block would decode the block up to it). New files never get a
        // password nothing confirmed.
        return match sevenz_backend::verify_password(archive, &files, Some(password))? {
            sevenz_backend::PasswordCheck::Failed => Err(CoreError::WrongPassword(label())),
            sevenz_backend::PasswordCheck::Unverified => Err(CoreError::Unsupported(format!(
                "{}: the password can't be confirmed (no encrypted file small enough to \
                 check against its checksum), so no files were added",
                label()
            ))),
            _ => Ok(Vec::new()),
        };
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
