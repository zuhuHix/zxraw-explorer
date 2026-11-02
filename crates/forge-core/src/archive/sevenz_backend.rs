//! 7z backend (`sevenz-rust2`): listing, streaming entries block by block, password checks,
//! decoder-memory preflight, creation and editing (see `sevenz_writer`).
//!
//! A 7z archive stores its files' data in *blocks* (folders); a solid block holds many files
//! compressed as one stream, so reading a file decodes its block from the start. Reading is
//! therefore planned per block: blocks holding no wanted entry are never decoded, each wanted
//! block is decoded once for all of its wanted entries, and decoding stops after the last one.
//! The data before a wanted entry is decoded and dropped here, with cancel/pause checks.
//!
//! Headers are checked by `sevenz_guard` before the library parses them. 7z AES has no
//! password check: before handing out data of an encrypted block, the first bytes of the
//! block are decoded (and a small first file decoded in full against its CRC); a failure
//! there is reported as a wrong password.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sevenz_rust2 as sz;

use super::sevenz_guard::{self, ID_AES, decoder_memory};
use super::{
    ArchiveBackend, ArchiveEdit, ArchiveEntry, ArchiveFormat, ArchiveListing, CreateOptions,
    CreateReport, EntryEncryption, EntryVisitor, ExtractLimits, Password, archive_stem,
    normalize_entry_name, validate_entry_name,
};
use crate::error::{CoreError, Result};
use crate::ops::JobCtx;
use crate::paths;

pub(super) struct SevenZipBackend;

pub(super) static SEVEN_ZIP: SevenZipBackend = SevenZipBackend;

const SIGNATURE: [u8; 6] = [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C];

/// Bytes of an encrypted block decoded to test the password before its data is handed out.
const PASSWORD_PEEK: usize = 64 * 1024;
/// Encrypted blocks tested by [`SevenZipBackend::check_password`].
const PASSWORD_BLOCKS: usize = 8;

/// An archive opened and its header parsed (after `sevenz_guard` checked it).
pub(super) struct Opened {
    pub archive: sz::Archive,
    pub file: BufReader<File>,
    pub password: sz::Password,
    pub header_encrypted: bool,
}

pub(super) fn sz_password(password: Option<&Password>) -> sz::Password {
    match password.and_then(|p| std::str::from_utf8(p.as_bytes()).ok()) {
        Some(text) => sz::Password::from(text),
        None => sz::Password::empty(),
    }
}

/// Open `path`, checking its header first. Header-encrypted archives need `password`.
pub(super) fn open(path: &Path, password: Option<&Password>, max_entries: usize) -> Result<Opened> {
    let mut file = File::open(path).map_err(|e| CoreError::io(path, e))?;
    let info = sevenz_guard::check(path, &mut file, password, max_entries)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| CoreError::io(path, e))?;
    let mut file = BufReader::with_capacity(1 << 16, file);
    // An unencrypted header doesn't need the password: don't hand it to the library.
    let pw = if info.encrypted {
        sz_password(password)
    } else {
        sz::Password::empty()
    };
    let archive = sz::Archive::read(&mut file, &pw).map_err(|e| sz_err(path, e, info.encrypted))?;
    if archive.files.len() > max_entries {
        return Err(CoreError::Invalid(format!(
            "archive has {} entries (listing limit {max_entries})",
            archive.files.len()
        )));
    }
    Ok(Opened {
        archive,
        file,
        password: sz_password(password),
        header_encrypted: info.encrypted,
    })
}

/// A library error as ours. `encrypted`: the data involved is encrypted, so a decoding
/// failure may be a wrong password.
pub(super) fn sz_err(path: &Path, e: sz::Error, encrypted: bool) -> CoreError {
    let label = || paths::display_path(path);
    match e {
        sz::Error::PasswordRequired => CoreError::PasswordRequired(label()),
        sz::Error::MaybeBadPassword(_) => CoreError::WrongPassword(label()),
        sz::Error::ChecksumVerificationFailed if encrypted => CoreError::WrongPassword(label()),
        sz::Error::Io(e, _) | sz::Error::FileOpen(e, _) => {
            // Data errors from the decoders come wrapped in `io::Error`.
            match e
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<sz::Error>())
            {
                Some(sz::Error::ChecksumVerificationFailed) if encrypted => {
                    CoreError::WrongPassword(label())
                }
                Some(inner) => CoreError::Invalid(format!("{}: {}", label(), describe(inner))),
                None if e.kind() == io::ErrorKind::UnexpectedEof => {
                    CoreError::Invalid(format!("{}: the archive is truncated", label()))
                }
                None => CoreError::io(path, e),
            }
        }
        sz::Error::UnsupportedCompressionMethod(m) => {
            CoreError::Unsupported(format!("{}: unsupported 7z method {m}", label()))
        }
        sz::Error::MaxMemLimited { actaul_kb, .. } => CoreError::UnsafePath(format!(
            "{}: needs {} MiB of decoder memory",
            label(),
            actaul_kb >> 10
        )),
        other => CoreError::Invalid(format!(
            "{}: damaged or unsupported 7z archive ({})",
            label(),
            describe(&other)
        )),
    }
}

fn describe(e: &sz::Error) -> String {
    match e {
        sz::Error::ChecksumVerificationFailed => "CRC mismatch".into(),
        sz::Error::NextHeaderCrcMismatch => "header CRC mismatch".into(),
        sz::Error::Other(s) | sz::Error::Unsupported(s) => s.to_string(),
        sz::Error::UnsupportedCompressionMethod(m) => format!("unsupported method {m}"),
        sz::Error::Io(e, _) => e.to_string(),
        other => format!("{other:?}"),
    }
}

/// Entry data for visitors, with the library's data errors in words.
struct Friendly<R>(R);

impl<R: Read> Read for Friendly<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf).map_err(|e| {
            let text = match e
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<sz::Error>())
            {
                Some(sz::Error::ChecksumVerificationFailed) => "CRC mismatch".to_owned(),
                Some(inner) => describe(inner),
                None => return e,
            };
            io::Error::new(io::ErrorKind::InvalidData, text)
        })
    }
}

fn nt_to_system(t: sz::NtTime) -> Option<SystemTime> {
    // 100 ns ticks since 1601-01-01; 11,644,473,600 s before the Unix epoch.
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let ticks = u64::from(t);
    if ticks == 0 {
        return None;
    }
    if ticks >= UNIX_EPOCH_TICKS {
        let d = ticks - UNIX_EPOCH_TICKS;
        SystemTime::UNIX_EPOCH
            .checked_add(Duration::new(d / 10_000_000, (d % 10_000_000) as u32 * 100))
    } else {
        let d = UNIX_EPOCH_TICKS - ticks;
        SystemTime::UNIX_EPOCH
            .checked_sub(Duration::new(d / 10_000_000, (d % 10_000_000) as u32 * 100))
    }
}

/// `FILE_ATTRIBUTE_REPARSE_POINT`, or a Unix symlink in the high 16 bits (7-Zip's
/// `FILE_ATTRIBUTE_UNIX_EXTENSION`, 0x8000).
fn is_link(e: &sz::ArchiveEntry) -> bool {
    if !e.has_windows_attributes {
        return false;
    }
    let a = e.windows_attributes;
    a & 0x400 != 0 || (a & 0x8000 != 0 && (a >> 16) & 0o170000 == 0o120000)
}

fn method_name(id: &[u8]) -> Option<&'static str> {
    Some(match id {
        [0x00] => "Copy",
        [0x03, 0x01, 0x01] => "LZMA",
        [0x21] => "LZMA2",
        [0x03, 0x04, 0x01] => "PPMd",
        [0x04, 0x02, 0x02] => "BZip2",
        [0x04, 0x01, 0x08] => "Deflate",
        [0x04, 0x01, 0x09] => "Deflate64",
        [0x03, 0x03, 0x01, 0x03] => "BCJ",
        [0x03, 0x03, 0x01, 0x1B] => "BCJ2",
        [0x03, 0x03, 0x05, 0x01] => "ARM",
        [0x0A] => "ARM64",
        [0x03, 0x03, 0x07, 0x01] => "ARMT",
        [0x03, 0x03, 0x02, 0x05] => "PPC",
        [0x03, 0x03, 0x04, 0x01] => "IA64",
        [0x03, 0x03, 0x08, 0x05] => "SPARC",
        [0x0B] => "RISCV",
        [0x03] => "Delta",
        [0x04, 0xF7, 0x11, 0x01] => "Zstandard",
        [0x04, 0xF7, 0x11, 0x02] => "Brotli",
        [0x04, 0xF7, 0x11, 0x04] => "LZ4",
        [0x06, 0xF1, 0x07, 0x01] => "AES-256",
        _ => return None,
    })
}

/// What a block is, for listing and planning.
pub(super) struct BlockInfo {
    pub encrypted: bool,
    /// "LZMA2 BCJ": the coders without AES, compression first.
    pub method: String,
    pub pack_size: u64,
    pub unpack_size: u64,
    /// Files with data in it.
    pub files: usize,
}

pub(super) fn block_infos(a: &sz::Archive) -> Vec<BlockInfo> {
    let firsts = a.stream_map.block_first_pack_stream_index();
    let pack_sizes = a.pack_sizes();
    let mut counts = vec![0usize; a.blocks.len()];
    for (f, b) in a.files.iter().zip(&a.stream_map.file_block_index) {
        if let Some(b) = *b
            && f.has_stream
            && f.size > 0
            && let Some(c) = counts.get_mut(b)
        {
            *c += 1;
        }
    }
    a.blocks
        .iter()
        .enumerate()
        .map(|(i, block)| {
            let start = firsts.get(i).copied().unwrap_or(pack_sizes.len());
            let end = firsts.get(i + 1).copied().unwrap_or(pack_sizes.len());
            let pack_size = pack_sizes
                .get(start.min(end)..end)
                .map_or(0, |s| s.iter().sum());
            let names: Vec<&str> = block
                .coders
                .iter()
                .filter(|c| c.encoder_method_id() != ID_AES)
                .map(|c| method_name(c.encoder_method_id()).unwrap_or("?"))
                .collect();
            BlockInfo {
                encrypted: block.coders.iter().any(|c| c.encoder_method_id() == ID_AES),
                method: names.join(" "),
                pack_size,
                unpack_size: block.get_unpack_size(),
                files: counts[i],
            }
        })
        .collect()
}

/// Our entries, by file index (`None` for anti-items, which only delete files when an
/// update archive is applied; they are never listed or extracted).
pub(super) fn entries_of(path: &Path, a: &sz::Archive) -> Vec<Option<ArchiveEntry>> {
    let blocks = block_infos(a);
    let mut offsets = vec![0u64; a.blocks.len()];
    let stem = archive_stem(path);
    a.files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            if f.is_anti_item {
                return None;
            }
            let block = a.stream_map.file_block_index.get(i).copied().flatten();
            let info = block.and_then(|b| blocks.get(b));
            let mut name = normalize_entry_name(&f.name);
            if name.is_empty() && !f.is_directory {
                // A nameless entry (packed from a stream): 7-Zip names it after the archive.
                name = stem.clone();
            }
            let is_dir = f.is_directory;
            if is_dir && !name.ends_with('/') {
                name.push('/');
            }
            let (block_offset, compressed_size) = match (block, info) {
                (Some(b), Some(info)) if f.has_stream && f.size > 0 => {
                    let off = offsets[b];
                    offsets[b] = off.saturating_add(f.size);
                    // Rounded up: a tiny file of a big block still takes some room.
                    let share = if info.unpack_size == 0 {
                        0
                    } else {
                        (u128::from(f.size) * u128::from(info.pack_size))
                            .div_ceil(u128::from(info.unpack_size)) as u64
                    };
                    (off, share)
                }
                _ => (0, 0),
            };
            let encrypted = info.is_some_and(|b| b.encrypted);
            Some(ArchiveEntry {
                index: i,
                safe_name: validate_entry_name(&name).is_ok(),
                name,
                is_dir,
                is_symlink: is_link(f),
                encrypted,
                encryption: encrypted.then_some(EntryEncryption::SevenZipAes),
                size: if is_dir { 0 } else { f.size },
                compressed_size,
                modified: if f.has_last_modified_date {
                    nt_to_system(f.last_modified_date)
                } else {
                    None
                },
                crc32: if f.has_crc { f.crc as u32 } else { 0 },
                method: info.map_or_else(String::new, |b| b.method.clone()),
                solid: info.is_some_and(|b| b.files > 1),
                block_offset,
            })
        })
        .collect()
}

fn has_data(f: &sz::ArchiveEntry) -> bool {
    f.has_stream && f.size > 0
}

/// The index of `entry` in `a.files` (the library hands out references into it).
fn file_index(a: &sz::Archive, entry: &sz::ArchiveEntry) -> usize {
    let base = a.files.as_ptr() as usize;
    (std::ptr::from_ref(entry) as usize - base) / std::mem::size_of::<sz::ArchiveEntry>()
}

/// Read up to `n` bytes of `r`.
fn read_up_to(r: &mut dyn Read, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Sentinel error: the visitor failed; its error is kept aside.
fn visitor_stop() -> sz::Error {
    sz::Error::Other("stopped by the visitor".into())
}

/// Decode and drop the rest of `r`, checking for cancel/pause.
fn drain(r: &mut dyn Read, ctx: Option<&JobCtx<'_>>, buf: &mut [u8]) -> io::Result<()> {
    loop {
        if let Some(ctx) = ctx
            && ctx.cancel.check().is_err()
        {
            return Err(io::Error::new(io::ErrorKind::Interrupted, Cancelled));
        }
        match r.read(buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted && e.get_ref().is_none() => {}
            Err(e) => return Err(e),
        }
    }
}

#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

fn is_cancel(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Cancelled>())
}

/// Whether block `b` is compressed (a wrong key makes its decoder fail early), not stored.
fn is_compressed(a: &sz::Archive, b: usize) -> bool {
    a.blocks[b].coders.iter().any(|c| {
        let id = c.encoder_method_id();
        id != ID_AES && decoder_memory(id, c.properties()).is_some_and(|m| m > 0)
    })
}

/// Test `password` on block `b`: decode its first bytes (a small first file in full,
/// against its CRC). `Some(false)` when that fails; `None` when it passed but proves
/// nothing: stored (Copy) data decrypts to *something* with any key, and only a CRC tells.
fn peek_block(o: &mut Opened, b: usize) -> Result<Option<bool>> {
    let Opened {
        archive,
        file,
        password,
        ..
    } = o;
    let compressed = is_compressed(archive, b);
    let mut outcome = None;
    let dec = sz::BlockDecoder::new(1, b, archive, password, file);
    let res = dec.for_each_entries(&mut |f, r| {
        if !has_data(f) {
            return Ok(true);
        }
        let ok = read_up_to(r, PASSWORD_PEEK).is_ok();
        // A first file read in full was checked against its CRC.
        let verified = compressed || f.size <= PASSWORD_PEEK as u64;
        outcome = (!ok || verified).then_some(ok);
        Ok(false)
    });
    match res {
        Ok(_) => Ok(outcome),
        Err(sz::Error::PasswordRequired) => Err(CoreError::PasswordRequired(String::new())),
        // Building the decoders failed after the key was derived: bad data, as a wrong key
        // gives.
        Err(_) => Ok(Some(false)),
    }
}

impl SevenZipBackend {
    /// Stream wanted entries of an opened archive (see [`ArchiveBackend::read_entries`]).
    pub(super) fn read_opened(
        &self,
        path: &Path,
        o: &mut Opened,
        entries: &[Option<ArchiveEntry>],
        wanted: &dyn Fn(&ArchiveEntry) -> bool,
        ctx: Option<&JobCtx<'_>>,
        visit: &mut EntryVisitor<'_>,
    ) -> Result<()> {
        let label = || paths::display_path(path);
        // Entries without data (folders, empty files) first: folders exist before their files.
        for (i, f) in o.archive.files.iter().enumerate() {
            if has_data(f) {
                continue;
            }
            if let Some(Some(e)) = entries.get(i)
                && wanted(e)
            {
                let mut empty: &[u8] = &[];
                visit(e, Ok(&mut empty))?;
            }
        }
        let blocks = block_infos(&o.archive);
        // The wanted files with data of each block, in order.
        let mut wanted_by_block: Vec<Vec<usize>> = vec![Vec::new(); blocks.len()];
        for (i, f) in o.archive.files.iter().enumerate() {
            if let Some(b) = o
                .archive
                .stream_map
                .file_block_index
                .get(i)
                .copied()
                .flatten()
                && has_data(f)
                && entries.get(i).and_then(Option::as_ref).is_some_and(wanted)
                && let Some(list) = wanted_by_block.get_mut(b)
            {
                list.push(i);
            }
        }
        let mut buf = vec![0u8; 256 * 1024];
        for (b, info) in blocks.iter().enumerate() {
            let wanted_here = std::mem::take(&mut wanted_by_block[b]);
            let Some(&last) = wanted_here.last() else {
                continue;
            };
            let entry = |i: usize| entries[i].as_ref().expect("wanted entries exist");
            let mut visited = 0usize;
            let mut stored: Option<CoreError> = None;
            let mut called = false;
            let mut tested = !info.encrypted;
            let mut bad_password = false;
            let mut noted = false;
            let res = {
                let Opened {
                    archive,
                    file,
                    password,
                    ..
                } = &mut *o;
                let files_ptr = &*archive;
                let dec = sz::BlockDecoder::new(1, b, files_ptr, password, file);
                dec.for_each_entries(&mut |f, r| {
                    called = true;
                    let i = file_index(files_ptr, f);
                    if !has_data(f) {
                        return Ok(i < last);
                    }
                    let is_wanted = wanted_here.binary_search(&i).is_ok();
                    // The block's first data: test the password before handing anything out.
                    let mut head = Vec::new();
                    if !tested {
                        tested = true;
                        match read_up_to(r, PASSWORD_PEEK) {
                            Ok(h) => head = h,
                            Err(_) => {
                                bad_password = true;
                                return Ok(false);
                            }
                        }
                    }
                    if !is_wanted {
                        if !noted && let Some(ctx) = ctx {
                            noted = true;
                            ctx.progress
                                .set_note("Decoding the solid block up to the selected files");
                        }
                        return match drain(r, ctx, &mut buf) {
                            Ok(()) => Ok(true),
                            Err(e) if is_cancel(&e) => {
                                stored = Some(CoreError::Cancelled);
                                Err(visitor_stop())
                            }
                            Err(e) => Err(sz::Error::from(e)),
                        };
                    }
                    if noted && let Some(ctx) = ctx {
                        noted = false;
                        ctx.progress.set_note("");
                    }
                    visited += 1;
                    let mut chained = Friendly(io::Cursor::new(head).chain(r));
                    if let Err(e) = visit(entry(i), Ok(&mut chained)) {
                        stored = Some(e);
                        return Err(visitor_stop());
                    }
                    Ok(i < last)
                })
            };
            if noted && let Some(ctx) = ctx {
                ctx.progress.set_note("");
            }
            if let Some(e) = stored {
                return Err(e);
            }
            let failure = if bad_password {
                Some(CoreError::WrongPassword(label()))
            } else {
                match res {
                    Ok(_) => None,
                    Err(sz::Error::PasswordRequired) => Some(CoreError::PasswordRequired(label())),
                    Err(e) if !called => Some(sz_err(path, e, info.encrypted)),
                    Err(e) => Some(match sz_err(path, e, info.encrypted) {
                        CoreError::WrongPassword(_) => CoreError::Invalid(format!(
                            "{}: damaged data in its block (or a wrong password)",
                            label()
                        )),
                        other => other,
                    }),
                }
            };
            if let Some(err) = failure {
                // Every wanted entry of the block not handed out yet gets the error.
                for &i in &wanted_here[visited.min(wanted_here.len())..] {
                    let e = match &err {
                        CoreError::PasswordRequired(s) => CoreError::PasswordRequired(s.clone()),
                        CoreError::WrongPassword(s) => CoreError::WrongPassword(s.clone()),
                        CoreError::Unsupported(s) => CoreError::Unsupported(s.clone()),
                        CoreError::UnsafePath(s) => CoreError::UnsafePath(s.clone()),
                        other => CoreError::Invalid(other.to_string()),
                    };
                    visit(entry(i), Err(e))?;
                }
            }
        }
        Ok(())
    }
}

impl ArchiveBackend for SevenZipBackend {
    fn format(&self) -> ArchiveFormat {
        ArchiveFormat::SevenZip
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["7z"]
    }

    fn sniff(&self, head: &[u8]) -> bool {
        head.starts_with(&SIGNATURE)
    }

    fn list(
        &self,
        archive: &Path,
        max_entries: usize,
        password: Option<&Password>,
    ) -> Result<ArchiveListing> {
        let o = open(archive, password, max_entries)?;
        Ok(ArchiveListing {
            format: ArchiveFormat::SevenZip,
            entries: entries_of(archive, &o.archive)
                .into_iter()
                .flatten()
                .collect(),
            comment: String::new(),
        })
    }

    fn preflight(
        &self,
        archive: &Path,
        indices: &[usize],
        password: Option<&Password>,
        limits: &ExtractLimits,
    ) -> Result<()> {
        let o = open(archive, password, limits.max_entries)?;
        let a = &o.archive;
        let mut checked = vec![false; a.blocks.len()];
        for &i in indices {
            let Some(b) = a.stream_map.file_block_index.get(i).copied().flatten() else {
                continue;
            };
            if std::mem::replace(&mut checked[b], true) {
                continue;
            }
            let block = &a.blocks[b];
            if block.coders.len() as u64 > sevenz_guard::MAX_CODERS {
                return Err(CoreError::UnsafePath(format!(
                    "{}: a block has {} coders",
                    paths::display_path(archive),
                    block.coders.len()
                )));
            }
            let mut need = 0u64;
            for c in &block.coders {
                let m = decoder_memory(c.encoder_method_id(), c.properties()).ok_or_else(|| {
                    CoreError::Invalid(format!(
                        "{}: damaged 7z archive (bad coder properties)",
                        paths::display_path(archive)
                    ))
                })?;
                need = need.saturating_add(m);
            }
            if need > limits.max_decoder_memory {
                let name = a.files.get(i).map_or("", |f| f.name.as_str());
                return Err(CoreError::UnsafePath(format!(
                    "{name}: needs {} MiB of decoder memory (limit {} MiB)",
                    need >> 20,
                    limits.max_decoder_memory >> 20
                )));
            }
        }
        Ok(())
    }

    fn read_entries(
        &self,
        archive: &Path,
        wanted: &dyn Fn(&ArchiveEntry) -> bool,
        password: Option<&Password>,
        ctx: Option<&JobCtx<'_>>,
        visit: &mut EntryVisitor<'_>,
    ) -> Result<()> {
        let mut o = open(archive, password, usize::MAX)?;
        let entries = entries_of(archive, &o.archive);
        self.read_opened(archive, &mut o, &entries, wanted, ctx, visit)
    }

    fn check_password(
        &self,
        archive: &Path,
        entries: &[&ArchiveEntry],
        password: Option<&Password>,
    ) -> Result<()> {
        let label = || paths::display_path(archive);
        if !entries.iter().any(|e| e.encrypted && !e.is_dir) {
            return Ok(());
        }
        let Some(password) = password else {
            return Err(CoreError::PasswordRequired(label()));
        };
        let mut o = open(archive, Some(password), usize::MAX)?;
        if o.header_encrypted {
            // The header decoded and passed its CRC with this password.
            return Ok(());
        }
        let mut blocks: Vec<usize> = entries
            .iter()
            .filter(|e| e.encrypted && !e.is_dir && e.size > 0)
            .filter_map(|e| {
                o.archive
                    .stream_map
                    .file_block_index
                    .get(e.index)
                    .copied()
                    .flatten()
            })
            .collect();
        blocks.sort_unstable();
        blocks.dedup();
        // Compressed blocks first: they prove a password, stored ones mostly can't.
        blocks.sort_by_key(|&b| !is_compressed(&o.archive, b));
        blocks.truncate(PASSWORD_BLOCKS);
        // Passes only when some block proves the password (or none can prove anything).
        let mut proven = false;
        let mut failed = false;
        for b in blocks {
            match peek_block(&mut o, b) {
                Ok(Some(true)) => proven = true,
                Ok(Some(false)) => failed = true,
                Ok(None) => {}
                Err(CoreError::PasswordRequired(_)) => {
                    return Err(CoreError::PasswordRequired(label()));
                }
                Err(e) => return Err(e),
            }
        }
        if proven || !failed {
            Ok(())
        } else {
            Err(CoreError::WrongPassword(label()))
        }
    }

    fn can_write(&self) -> bool {
        true
    }

    fn create(
        &self,
        sources: &[PathBuf],
        out: &Path,
        opts: &CreateOptions,
        ctx: &JobCtx<'_>,
    ) -> Result<CreateReport> {
        super::sevenz_writer::create(sources, out, opts, ctx)
    }

    fn update(
        &self,
        archive: &Path,
        edit: &ArchiveEdit,
        opts: &CreateOptions,
        ctx: &JobCtx<'_>,
    ) -> Result<CreateReport> {
        super::sevenz_writer::update(archive, edit, opts, ctx)
    }
}
