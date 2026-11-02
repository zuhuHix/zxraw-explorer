//! Tar archives (plain or in a compression stream) and single compressed files.
//!
//! A tar has no directory: listing it walks every header, and through a compressor that
//! means decoding the whole stream. A listing is therefore built once per version of the
//! file (size and modified time) and cached here ([`scan`]) with each member's offsets in
//! the uncompressed stream, so extraction, tests and temp copies don't walk it again.
//!
//! Reading members:
//! * a plain `.tar` seeks straight to each wanted member's headers (re-checked against the
//!   listing) and reads its data;
//! * a compressed tar is decoded once from its start for all wanted members together. Its
//!   only checksum is the stream's (gzip CRC-32, the xz check, ...), known only at its end,
//!   so its files are [`ArchiveEntry::checked_by_block`] (the whole stream is one block,
//!   [`ArchiveEntry::block_end`] its size): extraction holds them until the stream was
//!   decoded to its end and its checksum matched, as for a solid 7z block. A stream without
//!   a checksum is read only up to the last member wanted.
//!
//! Safety rules beyond the shared ones (see the module docs of `archive`): links (symbolic
//! and hard) are listed but never created; devices and FIFOs are listed but never
//! extracted; when a name occurs more than once, the last member wins (as `tar` does) and
//! the earlier ones are left out of the listing; `./` prefixes are dropped. Permissions and
//! owners are ignored.
//!
//! A single compressed file (`.gz`, `.bz2`, `.xz`, `.zst`, `.lz4`, `.br`) lists as one
//! file: the name in its gzip header when that is a plain file name, else the file's name
//! without the extension. Its size is exact: the xz index or the Zstandard frame headers
//! when they state it, else the stream is decoded once to count it (the gzip trailer's
//! size is only modulo 4 GiB and is never trusted).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use parking_lot::Mutex;

use super::codec::{self, Codec};
use super::tar_read::{self, Kind, Member, SeekFile, Source, SparseReader, TarParser};
use super::{
    ArchiveBackend, ArchiveEdit, ArchiveEntry, ArchiveFormat, ArchiveListing, BlockEnd,
    CreateOptions, CreateReport, EntryVisitor, ExtractLimits, Password, archive_stem,
    normalize_entry_name, validate_entry_name,
};
use crate::error::{CoreError, Result};
use crate::ops::JobCtx;
use crate::paths;

/// Memory a decoder may use while listing or reading (xz dictionaries, Zstandard windows).
fn decoder_memory(limits: Option<&ExtractLimits>) -> u64 {
    limits.map_or(ExtractLimits::default().max_decoder_memory, |l| {
        l.max_decoder_memory
    })
}

// ---------------------------------------------------------------------------
// Errors and cancel
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

fn is_cancel(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<Cancelled>())
}

/// A stream error in words, for the archive at `path`.
pub(super) fn stream_err(path: &Path, e: io::Error) -> CoreError {
    if is_cancel(&e) {
        return CoreError::Cancelled;
    }
    let label = paths::display_path(path);
    let text = e.to_string();
    let lower = text.to_lowercase();
    if lower.contains("memory") && (lower.contains("limit") || lower.contains("out of")) {
        return CoreError::UnsafePath(format!(
            "{label}: decoding it needs more memory than allowed ({text})"
        ));
    }
    match e.kind() {
        io::ErrorKind::UnexpectedEof => {
            CoreError::Invalid(format!("{label}: the archive is truncated ({text})"))
        }
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput | io::ErrorKind::Other => {
            CoreError::Invalid(format!("{label}: damaged archive ({text})"))
        }
        _ => CoreError::io(path, e),
    }
}

/// Reads the archive file, counting progress and checking for cancel.
struct Watched<'c, R> {
    inner: R,
    ctx: Option<&'c JobCtx<'c>>,
}

impl<R: Read> Read for Watched<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(ctx) = self.ctx
            && ctx.cancel.check().is_err()
        {
            return Err(io::Error::other(Cancelled));
        }
        let n = self.inner.read(buf)?;
        if let Some(ctx) = self.ctx {
            ctx.progress.add_bytes(n as u64);
        }
        Ok(n)
    }
}

/// Decoded bytes of a stream read to its end, bounded.
fn drain(r: &mut dyn Read, max: u64, ctx: Option<&JobCtx<'_>>) -> io::Result<u64> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        if let Some(ctx) = ctx
            && ctx.cancel.check().is_err()
        {
            return Err(io::Error::other(Cancelled));
        }
        match r.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => {
                total += n as u64;
                if total > max {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("it expands to more than {max} bytes"),
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// The listing cache
// ---------------------------------------------------------------------------

/// What one walk of a tar found.
pub(super) struct Scan {
    pub codec: Codec,
    pub members: Vec<Member>,
    /// The listing: one entry per name (the last member of a name), `index` the member's.
    pub entries: Vec<ArchiveEntry>,
    /// Decoded bytes of the whole stream (the tar and anything after its end).
    pub decoded: u64,
    /// The compressed file's size.
    pub file_size: u64,
}

/// What a single compressed file holds.
pub(super) struct SingleScan {
    pub codec: Codec,
    pub entry: ArchiveEntry,
}

enum Cached {
    Tar(Arc<Scan>),
    Single(Arc<SingleScan>),
}

type Key = (u64, Option<SystemTime>, bool);
type ScanCache = Mutex<lru::LruCache<PathBuf, (Key, Cached)>>;

fn cache() -> &'static ScanCache {
    static CACHE: OnceLock<ScanCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(lru::LruCache::new(std::num::NonZeroUsize::new(6).unwrap())))
}

fn key_of(archive: &Path, tar: bool) -> Result<(PathBuf, Key)> {
    let md = std::fs::metadata(archive).map_err(|e| CoreError::io(archive, e))?;
    Ok((
        paths::lexical_normalize(archive),
        (md.len(), md.modified().ok(), tar),
    ))
}

/// Forget what was cached about `archive` (after it was rewritten).
pub(super) fn forget(archive: &Path) {
    cache().lock().pop(&paths::lexical_normalize(archive));
}

/// The walk of a tar, cached while the file's size and modified time stay the same.
pub(super) fn scan(
    archive: &Path,
    codec: Codec,
    max_entries: usize,
    ctx: Option<&JobCtx<'_>>,
) -> Result<Arc<Scan>> {
    let (path, key) = key_of(archive, true)?;
    if let Some((k, Cached::Tar(s))) = cache().lock().get(&path)
        && *k == key
        && s.codec == codec
    {
        if s.members.len() > max_entries {
            return Err(too_many(max_entries));
        }
        return Ok(s.clone());
    }
    let s = Arc::new(walk(archive, codec, max_entries, ctx)?);
    cache().lock().put(path, (key, Cached::Tar(s.clone())));
    Ok(s)
}

fn too_many(max: usize) -> CoreError {
    CoreError::Invalid(format!("the archive has more than {max} entries"))
}

/// Walk the whole tar (and, compressed, its stream to the end, which checks its checksum).
fn walk(
    archive: &Path,
    codec: Codec,
    max_entries: usize,
    ctx: Option<&JobCtx<'_>>,
) -> Result<Scan> {
    let err = |e| stream_err(archive, e);
    let file = File::open(archive).map_err(|e| CoreError::io(archive, e))?;
    let file_size = file
        .metadata()
        .map_err(|e| CoreError::io(archive, e))?
        .len();
    if let Some(ctx) = ctx {
        ctx.progress.add_total_bytes(file_size);
        ctx.progress.set_note("Reading the archive's contents");
    }
    let mut members = Vec::new();
    let (decoded, checksum) = if codec == Codec::None {
        drop(file);
        let mut p = TarParser::new(
            SeekFile::open(archive, 0).map_err(|e| CoreError::io(archive, e))?,
            max_entries,
        );
        while let Some(m) = p.next_member().map_err(err)? {
            if let Some(ctx) = ctx {
                ctx.cancel.check()?;
                ctx.progress.add_bytes(m.end.saturating_sub(m.start));
            }
            members.push(m);
        }
        // Past the end marker: padding, or anything (nothing reads it).
        (file_size, false)
    } else {
        let mut head = [0u8; 16];
        let n = read_head(archive, &mut head)?;
        let checksum = codec::stream_has_checksum(codec, &head[..n]);
        let watched = Watched {
            inner: BufReader::with_capacity(256 * 1024, file),
            ctx,
        };
        let dec = codec::decoder(codec, watched, decoder_memory(None)).map_err(err)?;
        let mut p = TarParser::new(dec, max_entries);
        while let Some(m) = p.next_member().map_err(err)? {
            members.push(m);
        }
        let tar_end = p.pos();
        let mut dec = p.into_inner();
        let max = ExtractLimits::default().max_total_bytes;
        let rest = drain(&mut dec, max.saturating_sub(tar_end), ctx).map_err(err)?;
        (tar_end + rest, checksum)
    };
    let entries = entries_of(archive, codec, &members, decoded, checksum, file_size);
    Ok(Scan {
        codec,
        members,
        entries,
        decoded,
        file_size,
    })
}

fn read_head(archive: &Path, head: &mut [u8]) -> Result<usize> {
    let mut f = File::open(archive).map_err(|e| CoreError::io(archive, e))?;
    let mut n = 0;
    while n < head.len() {
        match f.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(CoreError::io(archive, e)),
        }
    }
    Ok(n)
}

/// The name a member lists under: `/`-separated, without `./` prefixes, folders ending
/// with `/`; empty for the archive's root (`./`).
fn listed_name(m: &Member) -> String {
    let mut name = normalize_entry_name(&m.name);
    while let Some(rest) = name.strip_prefix("./") {
        name = rest.to_owned();
    }
    if name == "." {
        name.clear();
    }
    let trimmed = name.trim_end_matches('/').len();
    name.truncate(trimmed);
    if m.kind == Kind::Dir && !name.is_empty() {
        name.push('/');
    }
    name
}

fn skip_reason(m: &Member) -> Option<String> {
    match m.kind {
        Kind::Hardlink => Some(format!(
            "hard link to {}: links are never created",
            normalize_entry_name(&m.link)
        )),
        Kind::CharDevice | Kind::BlockDevice => Some("device entries are not extracted".into()),
        Kind::Fifo => Some("FIFO (named pipe) entries are not extracted".into()),
        _ => None,
    }
}

fn entries_of(
    archive: &Path,
    codec: Codec,
    members: &[Member],
    decoded: u64,
    checksum: bool,
    file_size: u64,
) -> Vec<ArchiveEntry> {
    let compressed = codec != Codec::None;
    // The last member of each name wins; earlier ones are left out.
    let mut last: HashMap<String, usize> = HashMap::new();
    let names: Vec<String> = members.iter().map(listed_name).collect();
    for (i, n) in names.iter().enumerate() {
        if !n.is_empty() {
            last.insert(n.trim_end_matches('/').to_owned(), i);
        }
    }
    let _ = archive;
    let mut out = Vec::with_capacity(last.len());
    for (i, (m, name)) in members.iter().zip(names).enumerate() {
        if name.is_empty() || last.get(name.trim_end_matches('/')) != Some(&i) {
            continue;
        }
        let is_file = m.is_file();
        let has_data = is_file && m.size > 0;
        let compressed_size = if !is_file {
            0
        } else if compressed {
            // Its share of the stream.
            if decoded == 0 {
                0
            } else {
                (u128::from(m.stored) * u128::from(file_size)).div_ceil(u128::from(decoded)) as u64
            }
        } else {
            m.stored
        };
        let checked = compressed && checksum && has_data;
        out.push(ArchiveEntry {
            index: i,
            safe_name: validate_entry_name(&name).is_ok(),
            is_dir: m.kind == Kind::Dir,
            is_symlink: m.kind == Kind::Symlink,
            encrypted: false,
            encryption: None,
            size: if is_file { m.size } else { 0 },
            compressed_size,
            modified: m.mtime,
            crc32: 0,
            method: if m.sparse.is_some() {
                format!("{} (sparse)", codec.name())
            } else {
                codec.name().to_owned()
            },
            solid: compressed && is_file,
            block_offset: if compressed && is_file { m.data } else { 0 },
            unchecked: has_data && !(compressed && checksum),
            checked_by_block: checked,
            block: (compressed && is_file).then_some(0),
            block_end: if checked { decoded } else { 0 },
            skip: skip_reason(m),
            name,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Reading members
// ---------------------------------------------------------------------------

/// Visit the wanted entries of a scanned tar, in archive order (see the module docs).
fn read_tar(
    archive: &Path,
    s: &Scan,
    wanted: &dyn Fn(&ArchiveEntry) -> bool,
    ctx: Option<&JobCtx<'_>>,
    visit: &mut EntryVisitor<'_>,
    block_end: &mut BlockEnd<'_>,
) -> Result<()> {
    let picked: Vec<&ArchiveEntry> = s.entries.iter().filter(|e| wanted(e)).collect();
    if picked.is_empty() {
        return Ok(());
    }
    let err = |e| stream_err(archive, e);
    let changed = || {
        CoreError::Invalid(format!(
            "{}: the archive changed while it was read",
            paths::display_path(archive)
        ))
    };
    let same = |a: &Member, b: &Member| {
        a.header_hash == b.header_hash && a.start == b.start && a.data == b.data
    };
    if s.codec == Codec::None {
        for e in picked {
            if let Some(ctx) = ctx {
                ctx.cancel.check()?;
            }
            let want = &s.members[e.index];
            let file =
                SeekFile::open(archive, want.start).map_err(|e| CoreError::io(archive, e))?;
            let mut p = TarParser::at(file, want.start, usize::MAX);
            let m = p.next_member().map_err(err)?.ok_or_else(changed)?;
            if !same(&m, want) {
                return Err(changed());
            }
            visit_member(e, &m, &mut p, visit)?;
        }
        return Ok(());
    }
    // One pass from the start of the stream.
    let need_check = picked.iter().any(|e| e.checked_by_block);
    let last = picked.last().map_or(0, |e| e.index);
    let by_member: HashMap<usize, &ArchiveEntry> = picked.iter().map(|e| (e.index, *e)).collect();
    let file = File::open(archive).map_err(|e| CoreError::io(archive, e))?;
    let watched = Watched {
        inner: BufReader::with_capacity(256 * 1024, file),
        ctx: None,
    };
    let dec = codec::decoder(s.codec, watched, decoder_memory(None)).map_err(err)?;
    let mut p = TarParser::new(dec, s.members.len());
    let result = (|| -> Result<()> {
        for (k, want) in s.members.iter().enumerate() {
            if !need_check && k > last {
                return Ok(());
            }
            if let Some(ctx) = ctx {
                ctx.cancel.check()?;
            }
            let m = p.next_member().map_err(err)?.ok_or_else(changed)?;
            if !same(&m, want) {
                return Err(changed());
            }
            if let Some(e) = by_member.get(&k) {
                visit_member(e, &m, &mut p, visit)?;
            }
        }
        if p.next_member().map_err(err)?.is_some() {
            return Err(changed());
        }
        let tar_end = p.pos();
        let rest = drain(p.inner_mut(), s.decoded.saturating_sub(tar_end), ctx).map_err(err)?;
        if tar_end + rest != s.decoded {
            return Err(changed());
        }
        Ok(())
    })();
    if need_check {
        match &result {
            Err(CoreError::Cancelled) => {}
            Ok(()) => block_end(Ok(()))?,
            Err(e) => block_end(Err(CoreError::Invalid(e.to_string())))?,
        }
    }
    result
}

/// Hand one member's data to the visitor (a sparse file with its holes as zeros).
fn visit_member<R: Source>(
    e: &ArchiveEntry,
    m: &Member,
    p: &mut TarParser<R>,
    visit: &mut EntryVisitor<'_>,
) -> Result<()> {
    if !m.is_file() {
        let mut empty: &[u8] = &[];
        return visit(e, Ok(&mut empty));
    }
    match &m.sparse {
        Some(map) => {
            let mut r = SparseReader::new(p.data(), map.clone(), m.size);
            visit(e, Ok(&mut r))
        }
        None => {
            let mut r = p.data();
            visit(e, Ok(&mut r))
        }
    }
}

// ---------------------------------------------------------------------------
// The tar backends
// ---------------------------------------------------------------------------

pub(crate) struct TarBackend {
    pub codec: Codec,
}

pub(crate) static TARS: [TarBackend; 7] = [
    TarBackend { codec: Codec::None },
    TarBackend { codec: Codec::Gzip },
    TarBackend {
        codec: Codec::Bzip2,
    },
    TarBackend { codec: Codec::Xz },
    TarBackend { codec: Codec::Zstd },
    TarBackend { codec: Codec::Lz4 },
    TarBackend {
        codec: Codec::Brotli,
    },
];

pub(crate) fn tar_backend(codec: Codec) -> &'static TarBackend {
    TARS.iter().find(|b| b.codec == codec).expect("every codec")
}

impl ArchiveBackend for TarBackend {
    fn format(&self) -> ArchiveFormat {
        ArchiveFormat::Tar(self.codec)
    }

    fn extensions(&self) -> &'static [&'static str] {
        match self.codec {
            Codec::None => &["tar"],
            Codec::Gzip => &["tgz", "taz"],
            Codec::Bzip2 => &["tbz", "tbz2", "tb2"],
            Codec::Xz => &["txz"],
            Codec::Zstd => &["tzst"],
            Codec::Lz4 => &["tlz4"],
            Codec::Brotli => &[],
        }
    }

    fn sniff(&self, head: &[u8]) -> bool {
        // Content detection is `backend_for_file`'s: the codec, then a tar header inside.
        let _ = head;
        false
    }

    fn list(
        &self,
        archive: &Path,
        max_entries: usize,
        password: Option<&Password>,
    ) -> Result<ArchiveListing> {
        self.list_progress(archive, max_entries, password, None)
    }

    fn list_progress(
        &self,
        archive: &Path,
        max_entries: usize,
        _password: Option<&Password>,
        ctx: Option<&JobCtx<'_>>,
    ) -> Result<ArchiveListing> {
        let s = scan(archive, self.codec, max_entries, ctx)?;
        Ok(ArchiveListing {
            format: self.format(),
            entries: s.entries.clone(),
            comment: String::new(),
        })
    }

    fn preflight(
        &self,
        _archive: &Path,
        _indices: &[usize],
        _password: Option<&Password>,
        _limits: &ExtractLimits,
    ) -> Result<()> {
        // Members can't overlap, and decoder memory is bounded as the stream is read.
        Ok(())
    }

    fn read_entries(
        &self,
        archive: &Path,
        wanted: &dyn Fn(&ArchiveEntry) -> bool,
        _password: Option<&Password>,
        ctx: Option<&JobCtx<'_>>,
        visit: &mut EntryVisitor<'_>,
    ) -> Result<()> {
        let s = scan(archive, self.codec, usize::MAX, None)?;
        read_tar(archive, &s, wanted, ctx, visit, &mut |_| Ok(()))
    }

    fn read_entries_checked(
        &self,
        archive: &Path,
        wanted: &dyn Fn(&ArchiveEntry) -> bool,
        _password: Option<&Password>,
        ctx: Option<&JobCtx<'_>>,
        visit: &mut EntryVisitor<'_>,
        block_end: &mut BlockEnd<'_>,
    ) -> Result<()> {
        let s = scan(archive, self.codec, usize::MAX, None)?;
        read_tar(archive, &s, wanted, ctx, visit, block_end)
    }

    fn can_write(&self) -> bool {
        self.codec != Codec::Brotli
    }

    fn create(
        &self,
        sources: &[PathBuf],
        out: &Path,
        opts: &CreateOptions,
        ctx: &JobCtx<'_>,
    ) -> Result<CreateReport> {
        super::tar_write::create(self.codec, sources, out, opts, ctx)
    }

    fn update(
        &self,
        archive: &Path,
        edit: &ArchiveEdit,
        opts: &CreateOptions,
        ctx: &JobCtx<'_>,
    ) -> Result<CreateReport> {
        let r = super::tar_write::update(self, archive, edit, opts, ctx);
        forget(archive);
        r
    }
}

// ---------------------------------------------------------------------------
// Single compressed files
// ---------------------------------------------------------------------------

pub(crate) struct SingleBackend {
    pub codec: Codec,
}

pub(crate) static SINGLES: [SingleBackend; 6] = [
    SingleBackend { codec: Codec::Gzip },
    SingleBackend {
        codec: Codec::Bzip2,
    },
    SingleBackend { codec: Codec::Xz },
    SingleBackend { codec: Codec::Zstd },
    SingleBackend { codec: Codec::Lz4 },
    SingleBackend {
        codec: Codec::Brotli,
    },
];

pub(crate) fn single_backend(codec: Codec) -> &'static SingleBackend {
    SINGLES
        .iter()
        .find(|b| b.codec == codec)
        .unwrap_or(&SINGLES[0])
}

/// The name of the one file in a single compressed file.
fn single_name(archive: &Path, head: &[u8]) -> String {
    if let Some(name) = codec::gzip_name(head) {
        return name;
    }
    let name = paths::name_of(archive);
    match codec::by_name(&name) {
        // `x.tgz` that holds no tar: its contents are `x.tar`.
        Some((_, true)) => format!("{}.tar", archive_stem(archive)),
        _ => {
            let stem = match name.rsplit_once('.') {
                Some((s, _)) if !s.is_empty() => s.to_owned(),
                _ => format!("{name}.out"),
            };
            paths::sanitize_file_name(&stem)
        }
    }
}

/// The exact size a Zstandard file states in its frame headers, if every frame states one.
/// Only headers are read (blocks are skipped by their sizes).
fn zstd_declared_size(archive: &Path) -> Option<u64> {
    let mut f = BufReader::new(File::open(archive).ok()?);
    let len = f.get_ref().metadata().ok()?.len();
    let mut pos = 0u64;
    let mut total = 0u64;
    let mut frames = 0;
    while pos < len {
        frames += 1;
        if frames > 1_000_000 {
            return None;
        }
        let mut magic = [0u8; 4];
        f.read_exact(&mut magic).ok()?;
        let m = u32::from_le_bytes(magic);
        if (0x184d_2a50..=0x184d_2a5f).contains(&m) {
            // Skippable frame.
            let mut sz = [0u8; 4];
            f.read_exact(&mut sz).ok()?;
            let n = u64::from(u32::from_le_bytes(sz));
            f.seek_relative(i64::try_from(n).ok()?).ok()?;
            pos = pos.checked_add(8 + n)?;
            continue;
        }
        if m != 0xfd2f_b528 {
            return None;
        }
        let mut fhd = [0u8; 1];
        f.read_exact(&mut fhd).ok()?;
        let fhd = fhd[0];
        let fcs_flag = fhd >> 6;
        let single = fhd & 0x20 != 0;
        let checksum = fhd & 0x04 != 0;
        let dict = match fhd & 3 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let fcs_len = match (fcs_flag, single) {
            (0, false) => return None, // size not stated
            (0, true) => 1,
            (1, _) => 2,
            (2, _) => 4,
            _ => 8,
        };
        let mut skip = vec![0u8; usize::from(!single) + dict];
        f.read_exact(&mut skip).ok()?;
        let mut fcs = [0u8; 8];
        f.read_exact(&mut fcs[..fcs_len]).ok()?;
        let mut size = u64::from_le_bytes(fcs);
        if fcs_len == 2 {
            size += 256;
        }
        total = total.checked_add(size)?;
        pos = pos.checked_add(4 + 1 + skip.len() as u64 + fcs_len as u64)?;
        loop {
            let mut bh = [0u8; 3];
            f.read_exact(&mut bh).ok()?;
            let h = u32::from(bh[0]) | u32::from(bh[1]) << 8 | u32::from(bh[2]) << 16;
            let last = h & 1 != 0;
            let kind = (h >> 1) & 3;
            let size = u64::from(h >> 3);
            let stored = match kind {
                0 | 2 => size,
                1 => 1,
                _ => return None,
            };
            f.seek_relative(i64::try_from(stored).ok()?).ok()?;
            pos = pos.checked_add(3 + stored)?;
            if last {
                break;
            }
        }
        if checksum {
            f.seek_relative(4).ok()?;
            pos = pos.checked_add(4)?;
        }
    }
    (pos == len).then_some(total)
}

/// What a single compressed file holds, cached like tar listings.
pub(super) fn single_scan(
    archive: &Path,
    codec: Codec,
    ctx: Option<&JobCtx<'_>>,
) -> Result<Arc<SingleScan>> {
    let (path, key) = key_of(archive, false)?;
    if let Some((k, Cached::Single(s))) = cache().lock().get(&path)
        && *k == key
        && s.codec == codec
    {
        return Ok(s.clone());
    }
    let mut head = [0u8; 300];
    let n = read_head(archive, &mut head)?;
    let head = &head[..n];
    let file_size = std::fs::metadata(archive)
        .map_err(|e| CoreError::io(archive, e))?
        .len();
    let declared = match codec {
        Codec::Xz => File::open(archive)
            .ok()
            .and_then(|f| lzma_rust2::xz_uncompressed_size(BufReader::new(f)).ok()),
        Codec::Zstd => zstd_declared_size(archive),
        _ => None,
    };
    let size = match declared {
        Some(size) => size,
        None => {
            // Decoded once to count it (and to check its checksum).
            let file = File::open(archive).map_err(|e| CoreError::io(archive, e))?;
            if let Some(ctx) = ctx {
                ctx.progress.add_total_bytes(file_size);
                ctx.progress.set_note("Reading the compressed file");
            }
            let watched = Watched {
                inner: BufReader::with_capacity(256 * 1024, file),
                ctx,
            };
            let mut dec = codec::decoder(codec, watched, decoder_memory(None))
                .map_err(|e| stream_err(archive, e))?;
            drain(&mut dec, ExtractLimits::default().max_file_bytes, ctx)
                .map_err(|e| stream_err(archive, e))?
        }
    };
    let checksum = codec::stream_has_checksum(codec, head);
    let name = single_name(archive, head);
    let entry = ArchiveEntry {
        index: 0,
        safe_name: validate_entry_name(&name).is_ok(),
        name,
        size,
        compressed_size: file_size,
        modified: std::fs::metadata(archive)
            .ok()
            .and_then(|m| m.modified().ok()),
        method: codec.name().to_owned(),
        // Checked as it is read: the stream's end is the file's end.
        unchecked: size > 0 && !checksum,
        ..Default::default()
    };
    let s = Arc::new(SingleScan { codec, entry });
    cache().lock().put(path, (key, Cached::Single(s.clone())));
    Ok(s)
}

impl ArchiveBackend for SingleBackend {
    fn format(&self) -> ArchiveFormat {
        ArchiveFormat::Compressed(self.codec)
    }

    fn extensions(&self) -> &'static [&'static str] {
        match self.codec {
            Codec::Gzip => &["gz"],
            Codec::Bzip2 => &["bz2"],
            Codec::Xz => &["xz"],
            Codec::Zstd => &["zst"],
            Codec::Lz4 => &["lz4"],
            Codec::Brotli => &["br"],
            Codec::None => &[],
        }
    }

    fn sniff(&self, _head: &[u8]) -> bool {
        false
    }

    fn list(
        &self,
        archive: &Path,
        max_entries: usize,
        password: Option<&Password>,
    ) -> Result<ArchiveListing> {
        self.list_progress(archive, max_entries, password, None)
    }

    fn list_progress(
        &self,
        archive: &Path,
        _max_entries: usize,
        _password: Option<&Password>,
        ctx: Option<&JobCtx<'_>>,
    ) -> Result<ArchiveListing> {
        let s = single_scan(archive, self.codec, ctx)?;
        Ok(ArchiveListing {
            format: self.format(),
            entries: vec![s.entry.clone()],
            comment: String::new(),
        })
    }

    fn preflight(
        &self,
        _archive: &Path,
        _indices: &[usize],
        _password: Option<&Password>,
        _limits: &ExtractLimits,
    ) -> Result<()> {
        Ok(())
    }

    fn read_entries(
        &self,
        archive: &Path,
        wanted: &dyn Fn(&ArchiveEntry) -> bool,
        _password: Option<&Password>,
        _ctx: Option<&JobCtx<'_>>,
        visit: &mut EntryVisitor<'_>,
    ) -> Result<()> {
        let s = single_scan(archive, self.codec, None)?;
        if !wanted(&s.entry) {
            return Ok(());
        }
        let file = File::open(archive).map_err(|e| CoreError::io(archive, e))?;
        let mut dec = codec::decoder(
            self.codec,
            BufReader::with_capacity(256 * 1024, file),
            decoder_memory(None),
        )
        .map_err(|e| stream_err(archive, e))?;
        // Read to the stream's end, where its checksum is checked.
        visit(&s.entry, Ok(&mut dec))
    }

    fn create(
        &self,
        sources: &[PathBuf],
        out: &Path,
        opts: &CreateOptions,
        ctx: &JobCtx<'_>,
    ) -> Result<CreateReport> {
        match sources {
            [one] => super::tar_write::compress_file(self.codec, one, out, opts, ctx),
            _ => Err(CoreError::Invalid(format!(
                "a .{} file holds one file; choose tar.{} to compress several",
                self.codec.extension(),
                self.codec.extension()
            ))),
        }
    }

    fn update(
        &self,
        archive: &Path,
        _edit: &ArchiveEdit,
        _opts: &CreateOptions,
        _ctx: &JobCtx<'_>,
    ) -> Result<CreateReport> {
        Err(CoreError::Unsupported(format!(
            "{}: a single compressed file holds one file and can't be edited; extract it \
             or make a tar archive instead",
            paths::display_path(archive)
        )))
    }
}

/// The backend for a file that starts with `codec`'s signature (or, for Brotli, is named
/// so): a tar inside (its first 512 decoded bytes are a tar header, or zeros in a file
/// named as a tar) or a single compressed file.
pub(super) fn stream_backend(path: &Path, codec: Codec) -> &'static dyn super::ArchiveBackend {
    let named_tar = codec::by_name(&paths::name_of(path)).is_some_and(|(_, tar)| tar);
    let mut head = Vec::with_capacity(512);
    let peeked = File::open(path).ok().and_then(|f| {
        let dec = codec::decoder(codec, f, decoder_memory(None)).ok()?;
        dec.take(512).read_to_end(&mut head).ok()
    });
    let is_tar = peeked.is_some()
        && (tar_read::looks_like_tar(&head)
            || named_tar && head.len() == 512 && head.iter().all(|&b| b == 0)
            || named_tar && head.is_empty());
    if is_tar {
        tar_backend(codec)
    } else {
        single_backend(codec)
    }
}

/// Seek helper for tests: the offset of a member's data in the uncompressed stream.
#[cfg(test)]
pub(super) fn member_offsets(archive: &Path, codec: Codec) -> Vec<(String, u64)> {
    scan(archive, codec, usize::MAX, None)
        .unwrap()
        .members
        .iter()
        .map(|m| (m.name.clone(), m.data))
        .collect()
}
