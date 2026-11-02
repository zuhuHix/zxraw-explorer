//! Writing 7z: new archives and edits of existing ones.
//!
//! New archives are LZMA2 (presets in [`super::Preset::sevenzip`]). Files are planned into
//! blocks: already-compressed formats get a Copy block each (stored, and opening one never
//! decodes anything else); executables get BCJ + LZMA2; the rest LZMA2. With `solid` on,
//! files of a kind share blocks of up to the preset's (or the chosen) solid block size,
//! sorted by extension like 7-Zip does; off, every file is a block. LZMA2 runs on several
//! threads within a block when the block is big, as many as fit [`MT_MEMORY_BUDGET`].
//!
//! With a password every block is AES-256 encrypted (one key, a fresh IV per block) and,
//! with `encrypt_names`, the header too.
//!
//! An edit rebuilds the archive into a temp file next to it: blocks without a deleted or
//! replaced file are copied as they are (no decoding, no password needed), blocks with one
//! are decoded (their kept files spilled to a temp file) and recompressed, new files are
//! added as for a new archive. The temp file replaces the original only when complete; a
//! cancelled or failed edit leaves the original untouched.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sevenz_rust2 as sz;
use sz::encoder_options::{AesEncoderOptions, Lzma2Options};

use super::sevenz_backend::{self, Opened, entries_of, sz_err};
use super::{
    ArchiveEdit, CreateOptions, CreateReport, DEFAULT_MAX_ARCHIVE_ENTRIES, Encryption,
    SevenZipPresetInfo, SourceItem, collect_sources, is_precompressed, motw, safety,
};
use crate::error::{CoreError, Result};
use crate::ops::{self, JobCtx, NodeKind, OpIssue, TempGuard};
use crate::paths;

/// Memory all LZMA2 threads of one block may use together.
const MT_MEMORY_BUDGET: u64 = 1536 << 20;
#[cfg(test)]
thread_local! {
    /// Tests: the free space the spill check sees.
    pub(super) static FAKE_FREE_SPACE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

pub(super) fn free_space(dir: &Path) -> Result<u64> {
    #[cfg(test)]
    if let Some(free) = FAKE_FREE_SPACE.with(|f| f.get()) {
        return Ok(free);
    }
    crate::split::free_space_available(dir).map_err(|e| CoreError::io(dir, e))
}

/// Free space wanted beyond the spilled files when recompressing (the new archive grows).
const SPILL_SLACK: u64 = 64 << 20;
/// 7-Zip's AES key derivation work factor (2^19 SHA-256 rounds).
const AES_CYCLES_POWER: u8 = 19;

/// Executables, for the x86 BCJ filter.
const EXECUTABLE: &[&str] = &[
    "exe", "dll", "sys", "ocx", "scr", "cpl", "efi", "drv", "com",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Lzma,
    Exe,
    Copy,
}

/// The password of 7z options; ZipCrypto doesn't exist in 7z.
fn password_of(opts: &CreateOptions) -> Result<Option<&super::Password>> {
    match &opts.encryption {
        Encryption::None => Ok(None),
        Encryption::Aes256(p) if p.is_empty() => {
            Err(CoreError::Invalid("the password must not be empty".into()))
        }
        Encryption::Aes256(p) => Ok(Some(p)),
        Encryption::ZipCryptoLegacy(_) => Err(CoreError::Invalid(
            "7z archives are encrypted with AES-256 only".into(),
        )),
    }
}

/// How blocks are encoded.
struct Methods {
    info: SevenZipPresetInfo,
    threads: usize,
    password: Option<sz::Password>,
    salt: [u8; 16],
}

impl Methods {
    fn new(opts: &CreateOptions, password: Option<&super::Password>) -> Self {
        let password = password.map(|p| sevenz_backend::sz_password(Some(p)));
        let salt = password
            .as_ref()
            .map_or([0; 16], |p| AesEncoderOptions::new(p.clone()).salt);
        Self {
            info: opts.preset.sevenzip(),
            threads: opts.worker_threads(),
            password,
            salt,
        }
    }

    fn aes(&self) -> Option<sz::EncoderConfiguration> {
        self.password.as_ref().map(|p| {
            // `new` draws a random IV (and salt); one salt keeps one key for all blocks.
            let mut o = AesEncoderOptions::new(p.clone());
            o.salt = self.salt;
            o.num_cycles_power = AES_CYCLES_POWER;
            o.into()
        })
    }

    /// LZMA2 for a block of `bytes`: the dictionary no larger than the data, several threads
    /// when the block is big enough and their memory fits the budget.
    fn lzma2(&self, bytes: u64) -> sz::EncoderConfiguration {
        let dict = u64::from(self.info.dictionary)
            .min(bytes.max(1).next_power_of_two().max(1 << 16)) as u32;
        let chunk = u64::from(dict) * 2;
        let mut lzma = lzma_rust2::LzmaOptions::with_preset(self.info.level);
        lzma.dict_size = dict;
        let per_thread = u64::from(lzma.get_memory_usage()) * 1024 + chunk * 2;
        let threads = (MT_MEMORY_BUDGET / per_thread.max(1))
            .clamp(1, self.threads as u64)
            .min(bytes / chunk.max(1));
        let mut o = if threads >= 2 {
            Lzma2Options::from_level_mt(self.info.level, threads as u32, chunk)
        } else {
            Lzma2Options::from_level(self.info.level)
        };
        o.set_dictionary_size(dict);
        o.into()
    }

    /// The coder chain for a block (output side first, as the writer wants it).
    fn chain(&self, kind: Kind, bytes: u64) -> Vec<sz::EncoderConfiguration> {
        let mut m: Vec<sz::EncoderConfiguration> = self.aes().into_iter().collect();
        match kind {
            Kind::Copy => m.push(sz::EncoderMethod::COPY.into()),
            Kind::Lzma => m.push(self.lzma2(bytes)),
            Kind::Exe => {
                m.push(self.lzma2(bytes));
                m.push(sz::EncoderMethod::BCJ_X86_FILTER.into());
            }
        }
        m
    }

    fn solid_block(&self, opts: &CreateOptions) -> u64 {
        opts.solid_block_size
            .unwrap_or(self.info.solid_block)
            .max(1 << 20)
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Counts what is written to the archive file, for the progress output figure.
struct Counting<'a> {
    inner: BufWriter<File>,
    ctx: JobCtx<'a>,
    high: u64,
    pos: u64,
}

impl Write for Counting<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.pos += n as u64;
        if self.pos > self.high {
            self.ctx.progress.add_bytes_out(self.pos - self.high);
            self.high = self.pos;
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Seek for Counting<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

type Writer<'a> = sz::ArchiveWriter<Counting<'a>>;

fn start<'a>(dest: &Path, ctx: &JobCtx<'a>) -> Result<(Writer<'a>, PathBuf, TempGuard)> {
    let dir = parent_dir(dest);
    let tmp = dir.join(ops::partial_name());
    let file = File::create_new(&tmp).map_err(|e| CoreError::io(dest, e))?;
    let guard = TempGuard(Some(tmp.clone()));
    let counting = Counting {
        inner: BufWriter::with_capacity(1 << 20, file),
        ctx: *ctx,
        high: 0,
        pos: 0,
    };
    let w = sz::ArchiveWriter::new(counting).map_err(|e| sz_err(dest, e, false))?;
    Ok((w, tmp, guard))
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Write the header (encrypted when asked) and the signature, then publish `tmp` as `dest`.
fn finish(
    mut w: Writer<'_>,
    methods: &Methods,
    encrypt_header: bool,
    tmp: &Path,
    dest: &Path,
    replace: bool,
    ctx: &JobCtx<'_>,
    report: &mut CreateReport,
) -> Result<()> {
    ctx.cancel.check()?;
    match methods.aes() {
        // The header is encrypted with the AES coder of the writer's current methods.
        Some(aes) if encrypt_header => {
            w.set_content_methods(vec![aes, sz::EncoderMethod::LZMA2.into()]);
            w.set_encrypt_header(true);
        }
        _ => w.set_encrypt_header(false),
    }
    let counting = w.finish().map_err(|e| CoreError::io(dest, e))?;
    report.bytes_out = counting.high;
    let file = counting
        .inner
        .into_inner()
        .map_err(|e| CoreError::io(dest, e.into_error()))?;
    file.sync_all().map_err(|e| CoreError::io(dest, e))?;
    drop(file);
    if replace && let Err(e) = motw::copy_zone_stream(dest, tmp) {
        tracing::warn!("could not keep the zone of {}: {e}", dest.display());
    }
    ops::place(tmp, dest, replace)
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// A file's data for the writer, opened on its first read (a solid block lists thousands),
/// with cancel/pause checks and progress.
struct Feed<'a> {
    path: PathBuf,
    offset: u64,
    limit: Option<u64>,
    name: String,
    file: Option<io::Take<File>>,
    started: bool,
    /// Count items and bytes (not for spilled data, counted when it was decoded).
    count: bool,
    ctx: JobCtx<'a>,
}

impl<'a> Feed<'a> {
    fn file(path: &Path, name: &str, ctx: &JobCtx<'a>) -> Self {
        Self {
            path: path.to_path_buf(),
            offset: 0,
            limit: None,
            name: name.to_owned(),
            file: None,
            started: false,
            count: true,
            ctx: *ctx,
        }
    }

    /// `len` bytes of `path` from `offset` (a spilled entry).
    fn range(path: &Path, offset: u64, len: u64, name: &str, ctx: &JobCtx<'a>) -> Self {
        Self {
            limit: Some(len),
            offset,
            count: false,
            ..Self::file(path, name, ctx)
        }
    }
}

impl Read for Feed<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.ctx.cancel.check().is_err() {
            return Err(io::Error::other(Cancelled));
        }
        if !self.started {
            self.started = true;
            if self.count {
                self.ctx.progress.add_items(1);
            }
            self.ctx.progress.set_current(self.name.clone());
            let mut f = File::open(&self.path)?;
            if self.offset > 0 {
                f.seek(SeekFrom::Start(self.offset))?;
            }
            self.file = Some(f.take(self.limit.unwrap_or(u64::MAX)));
        }
        let Some(f) = &mut self.file else {
            return Ok(0);
        };
        let n = f.read(buf)?;
        if n == 0 {
            self.file = None; // closed at its end
        }
        if self.count {
            self.ctx.progress.add_bytes(n as u64);
        }
        Ok(n)
    }
}

fn nt_time(t: Option<SystemTime>) -> Option<sz::NtTime> {
    t.and_then(|t| sz::NtTime::try_from(t).ok())
}

fn new_entry(name: &str, is_dir: bool, modified: Option<SystemTime>) -> sz::ArchiveEntry {
    let mut e = if is_dir {
        sz::ArchiveEntry::new_directory(name)
    } else {
        sz::ArchiveEntry::new_file(name)
    };
    if let Some(t) = nt_time(modified) {
        e.last_modified_date = t;
        e.has_last_modified_date = true;
    }
    e
}

/// Map a library error from writing to ours: cancel, a source that failed, or the archive.
fn write_err(out: &Path, e: sz::Error, ctx: &JobCtx<'_>) -> CoreError {
    if ctx.cancel.is_cancelled() {
        return CoreError::Cancelled;
    }
    match e {
        sz::Error::Io(e, what) => CoreError::Invalid(format!(
            "{}: {e}{}",
            paths::display_path(out),
            if what.is_empty() {
                String::new()
            } else {
                format!(" ({what})")
            }
        )),
        other => sz_err(out, other, false),
    }
}

struct Planned {
    item: SourceItem,
    kind: Kind,
    ext: String,
}

/// Add new files and folders: folders and empty files as entries, the rest in blocks.
#[allow(clippy::too_many_arguments)]
fn write_new(
    w: &mut Writer<'_>,
    items: Vec<SourceItem>,
    names: &mut HashSet<String>,
    methods: &Methods,
    opts: &CreateOptions,
    ctx: &JobCtx<'_>,
    report: &mut CreateReport,
    out: &Path,
    tmp: &Path,
) -> Result<()> {
    ctx.progress.add_total_items(items.len() as u64);
    ctx.progress.add_total_bytes(
        items
            .iter()
            .filter(|w| w.kind == NodeKind::File)
            .fold(0u64, |a, w| a.saturating_add(w.size)),
    );
    let mut planned = Vec::new();
    for item in items {
        ctx.cancel.check()?;
        if paths::paths_equal(&item.path, out) || paths::paths_equal(&item.path, tmp) {
            ctx.progress.add_items(1);
            continue;
        }
        if !names.insert(item.name.to_lowercase()) {
            ctx.progress.add_items(1);
            report.skipped.push(OpIssue::new(
                &item.path,
                "an entry with this name is already in the archive",
            ));
            continue;
        }
        match item.kind {
            NodeKind::Link => {
                ctx.progress.add_items(1);
                report
                    .skipped
                    .push(OpIssue::new(&item.path, "links are not archived"));
            }
            NodeKind::Dir => {
                ctx.progress.add_items(1);
                w.push_archive_entry::<&[u8]>(new_entry(&item.name, true, item.modified), None)
                    .map_err(|e| write_err(out, e, ctx))?;
                report.dirs += 1;
            }
            NodeKind::File => {
                // Unreadable files are reported now: inside a solid block one would fail
                // the whole block.
                if let Err(e) = File::open(&item.path) {
                    ctx.progress.add_items(1);
                    ctx.progress.add_bytes(item.size);
                    report.skipped.push(OpIssue::new(&item.path, e.to_string()));
                    continue;
                }
                if item.size == 0 {
                    ctx.progress.add_items(1);
                    w.push_archive_entry::<&[u8]>(
                        new_entry(&item.name, false, item.modified),
                        None,
                    )
                    .map_err(|e| write_err(out, e, ctx))?;
                    report.files += 1;
                    continue;
                }
                let ext = crate::model::ext_of(&item.path);
                let kind = if opts.auto_store && is_precompressed(&ext) {
                    Kind::Copy
                } else if EXECUTABLE.contains(&ext.as_str()) {
                    Kind::Exe
                } else {
                    Kind::Lzma
                };
                planned.push(Planned { item, kind, ext });
            }
        }
    }
    // Like 7-Zip: by kind, then extension, then name, so similar files share a block.
    planned.sort_by(|a, b| {
        (a.kind, &a.ext, a.item.name.to_lowercase()).cmp(&(
            b.kind,
            &b.ext,
            b.item.name.to_lowercase(),
        ))
    });
    let limit = methods.solid_block(opts);
    let mut i = 0;
    while i < planned.len() {
        let kind = planned[i].kind;
        let mut j = i + 1;
        let mut bytes = planned[i].item.size;
        if opts.solid && kind != Kind::Copy {
            while j < planned.len()
                && planned[j].kind == kind
                && bytes.saturating_add(planned[j].item.size) <= limit
            {
                bytes += planned[j].item.size;
                j += 1;
            }
        }
        let group = &planned[i..j];
        w.set_content_methods(methods.chain(kind, bytes));
        if group.len() == 1 {
            let p = &group[0];
            let feed = Feed::file(&p.item.path, &p.item.name, ctx);
            w.push_archive_entry(new_entry(&p.item.name, false, p.item.modified), Some(feed))
                .map_err(|e| write_err(out, e, ctx))?;
        } else {
            let entries = group
                .iter()
                .map(|p| new_entry(&p.item.name, false, p.item.modified))
                .collect();
            let feeds = group
                .iter()
                .map(|p| sz::SourceReader::new(Feed::file(&p.item.path, &p.item.name, ctx)))
                .collect();
            w.push_archive_entries(entries, feeds)
                .map_err(|e| write_err(out, e, ctx))?;
        }
        report.files += group.len() as u64;
        report.bytes_in = report.bytes_in.saturating_add(bytes);
        i = j;
    }
    Ok(())
}

pub(super) fn create(
    sources: &[PathBuf],
    out: &Path,
    opts: &CreateOptions,
    ctx: &JobCtx<'_>,
) -> Result<CreateReport> {
    if paths::exists_no_follow(out) {
        return Err(CoreError::AlreadyExists(out.to_path_buf()));
    }
    let password = password_of(opts)?;
    let methods = Methods::new(opts, password);
    let mut report = CreateReport::default();
    let items = collect_sources(sources, "", ctx, &mut report.skipped)?;
    let (mut w, tmp, mut guard) = start(out, ctx)?;
    let mut names = HashSet::new();
    write_new(
        &mut w,
        items,
        &mut names,
        &methods,
        opts,
        ctx,
        &mut report,
        out,
        &tmp,
    )?;
    finish(
        w,
        &methods,
        opts.encrypt_names,
        &tmp,
        out,
        false,
        ctx,
        &mut report,
    )?;
    guard.disarm();
    Ok(report)
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

/// One kept file of a recompressed block, spilled to the side file.
struct Spilled {
    entry: sz::ArchiveEntry,
    offset: u64,
    len: u64,
}

/// Decode block `b` of `o` and write the files `keep` wants to `spill`, in order. Every
/// file's CRC is checked as it is decoded.
fn spill_block(
    archive: &Path,
    o: &mut Opened,
    b: usize,
    keep: &dyn Fn(usize) -> bool,
    spill: &mut BufWriter<File>,
    pos: &mut u64,
    budget: &mut u64,
    encrypted: bool,
    ctx: &JobCtx<'_>,
) -> Result<Vec<Spilled>> {
    let Opened {
        archive: a,
        file,
        password,
        ..
    } = o;
    let a: &sz::Archive = a;
    let base = a.files.as_ptr() as usize;
    let mut out = Vec::new();
    let mut failed: Option<CoreError> = None;
    let mut buf = vec![0u8; 256 * 1024];
    let res = sz::BlockDecoder::new(1, b, a, password, file).for_each_entries(&mut |f, r| {
        let i = (std::ptr::from_ref(f) as usize - base) / std::mem::size_of::<sz::ArchiveEntry>();
        let wanted = f.has_stream && keep(i);
        let start = *pos;
        loop {
            if ctx.cancel.check().is_err() {
                failed = Some(CoreError::Cancelled);
                return Err(sz::Error::Other("cancelled".into()));
            }
            let n = match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(sz::Error::from(e)),
            };
            match budget.checked_sub(n as u64) {
                Some(left) => *budget = left,
                None => {
                    failed = Some(CoreError::UnsafePath(format!(
                        "{}: a block decodes to more than it declares",
                        paths::display_path(archive)
                    )));
                    return Err(sz::Error::Other("over the limit".into()));
                }
            }
            if wanted {
                if let Err(e) = spill.write_all(&buf[..n]) {
                    failed = Some(CoreError::io(archive, e));
                    return Err(sz::Error::Other("spill".into()));
                }
                *pos += n as u64;
            }
            ctx.progress.add_bytes(n as u64);
        }
        if wanted {
            out.push(Spilled {
                entry: f.clone(),
                offset: start,
                len: *pos - start,
            });
        }
        Ok(true)
    });
    if let Some(e) = failed {
        return Err(e);
    }
    res.map_err(|e| match sz_err(archive, e, encrypted) {
        CoreError::WrongPassword(s) => CoreError::WrongPassword(s),
        CoreError::PasswordRequired(s) => CoreError::PasswordRequired(s),
        other => CoreError::Invalid(format!("can't recompress a block: {other}")),
    })?;
    Ok(out)
}

pub(super) fn update(
    archive: &Path,
    edit: &ArchiveEdit,
    opts: &CreateOptions,
    ctx: &JobCtx<'_>,
) -> Result<CreateReport> {
    let password = password_of(opts)?;
    let mut report = CreateReport::default();
    let mut adds = Vec::new();
    for (src, dest) in &edit.add {
        let mut prefix = safety::normalize_entry_name(dest);
        if !prefix.is_empty() {
            if !prefix.ends_with('/') {
                prefix.push('/');
            }
            safety::validate_entry_name(&prefix)?;
        }
        if paths::paths_equal(src, archive) {
            return Err(CoreError::Invalid(
                "an archive can't be added to itself".into(),
            ));
        }
        adds.extend(collect_sources(
            std::slice::from_ref(src),
            &prefix,
            ctx,
            &mut report.skipped,
        )?);
    }
    let added: HashSet<String> = adds.iter().map(|a| a.name.to_lowercase()).collect();
    let deleted: Vec<String> = edit
        .delete
        .iter()
        .map(|d| safety::normalize_entry_name(d).to_lowercase())
        .collect();
    let is_deleted = |name: &str| {
        let lower = name.to_lowercase();
        let bare = lower.trim_end_matches('/');
        deleted.iter().any(|d| {
            let d_bare = d.trim_end_matches('/');
            bare == d_bare || (d.ends_with('/') && lower.starts_with(d.as_str()))
        })
    };

    let mut o = sevenz_backend::open(archive, password, DEFAULT_MAX_ARCHIVE_ENTRIES)?;
    if o.archive.files.iter().any(|f| f.is_anti_item) {
        return Err(CoreError::Unsupported(
            "This 7z archive holds deletion markers (an update archive); it can't be edited."
                .into(),
        ));
    }
    if let Some(dir) = archive.parent() {
        super::zip_backend::remove_stale_partials(dir, std::time::Duration::from_secs(3600));
    }
    let entries = entries_of(archive, &o.archive);
    let blocks = sevenz_backend::block_infos(&o.archive);
    // Which files go: deleted, or replaced by an added file of the same name.
    let mut removed = vec![false; o.archive.files.len()];
    let mut names = HashSet::new();
    for (i, e) in entries.iter().enumerate() {
        let Some(e) = e else { continue };
        let key = e.name.trim_end_matches('/').to_lowercase();
        if is_deleted(&e.name) {
            removed[i] = true;
            report.deleted += 1;
        } else if added.contains(&key) {
            removed[i] = true;
            report.replaced += 1;
        } else {
            names.insert(key);
        }
    }
    let file_blocks = o.archive.stream_map.file_block_index.clone();
    let block_of = |i: usize| -> Option<usize> { file_blocks.get(i).copied().flatten() };
    let mut touched = vec![false; blocks.len()];
    for (i, f) in o.archive.files.iter().enumerate() {
        if removed[i]
            && f.has_stream
            && let Some(b) = block_of(i)
        {
            touched[b] = true;
        }
    }
    let recompress: u64 = blocks
        .iter()
        .zip(&touched)
        .filter(|(_, t)| **t)
        .fold(0u64, |a, (b, _)| a.saturating_add(b.unpack_size));
    if recompress > 0
        && blocks.iter().zip(&touched).any(|(b, t)| *t && b.encrypted)
        && password.is_none()
    {
        return Err(CoreError::PasswordRequired(paths::display_path(archive)));
    }
    // The blocks to recompress are decoded: the extraction limits apply, and their kept
    // files must fit next to the archive, all before anything is written.
    let limits = &opts.limits;
    for (b, _) in touched.iter().enumerate().filter(|(_, t)| **t) {
        sevenz_backend::check_block_memory(archive, &o.archive, b, limits)?;
    }
    if recompress > limits.max_total_bytes {
        return Err(CoreError::UnsafePath(format!(
            "{}: the blocks to recompress expand to more than {} bytes",
            paths::display_path(archive),
            limits.max_total_bytes
        )));
    }
    let spill_need = o
        .archive
        .files
        .iter()
        .enumerate()
        .filter(|(i, f)| f.has_stream && !removed[*i] && block_of(*i).is_some_and(|b| touched[b]))
        .fold(0u64, |a, (_, f)| a.saturating_add(f.size));
    if spill_need > 0 {
        let dir = parent_dir(archive);
        let free = free_space(dir)?;
        // Room for the spilled files and some slack for the new archive's first blocks.
        if free < spill_need.saturating_add(SPILL_SLACK) {
            return Err(CoreError::Invalid(format!(
                "not enough free space next to {} to recompress it: {} MB needed, {} MB free",
                paths::display_path(archive),
                spill_need.saturating_add(SPILL_SLACK) >> 20,
                free >> 20
            )));
        }
    }
    // Never decode more than the touched blocks declare (nor than the limit).
    let mut decode_budget = recompress.min(limits.max_total_bytes);
    let methods = Methods::new(opts, password);

    let (mut w, tmp, mut guard) = start(archive, ctx)?;
    let raw_bytes: u64 = blocks
        .iter()
        .zip(&touched)
        .filter(|(_, t)| !**t)
        .fold(0u64, |a, (b, _)| a.saturating_add(b.pack_size));
    ctx.progress
        .add_total_bytes(raw_bytes.saturating_add(recompress));
    if recompress > 0 {
        ctx.progress.set_note(format!(
            "Recompressing {} MB of solid blocks",
            recompress.div_ceil(1 << 20)
        ));
    }

    // Folders and empty files that stay.
    for (i, f) in o.archive.files.iter().enumerate() {
        if !f.has_stream && !removed[i] && !f.is_anti_item {
            w.push_archive_entry::<&[u8]>(f.clone(), None)
                .map_err(|e| write_err(archive, e, ctx))?;
            report.kept += 1;
        }
    }
    let spill_path = parent_dir(archive).join(ops::partial_name());
    let mut spill_guard = TempGuard(None);
    let mut spill: Option<BufWriter<File>> = None;
    for (b, info) in blocks.iter().enumerate() {
        ctx.cancel.check()?;
        if !touched[b] {
            if info.files == 0 && info.pack_size == 0 {
                continue;
            }
            let Opened {
                archive: a, file, ..
            } = &mut o;
            w.push_raw_block(a, b, file, |e| e.name.clone())
                .map_err(|e| write_err(archive, e, ctx))?;
            ctx.progress.add_bytes(info.pack_size);
            report.kept += info.files as u64;
            continue;
        }
        let keep = |i: usize| !removed[i];
        if !o
            .archive
            .files
            .iter()
            .enumerate()
            .any(|(i, f)| f.has_stream && keep(i) && block_of(i) == Some(b))
        {
            ctx.progress.add_bytes(info.unpack_size);
            continue;
        }
        // Spill the kept files, then compress them as a new block (encrypted if it was).
        let sp = match &mut spill {
            Some(s) => s,
            None => {
                let f = File::create_new(&spill_path).map_err(|e| CoreError::io(&spill_path, e))?;
                spill_guard = TempGuard(Some(spill_path.clone()));
                spill.insert(BufWriter::with_capacity(1 << 20, f))
            }
        };
        sp.seek(SeekFrom::Start(0))
            .map_err(|e| CoreError::io(&spill_path, e))?;
        let mut pos = 0u64;
        let kept = spill_block(
            archive,
            &mut o,
            b,
            &keep,
            sp,
            &mut pos,
            &mut decode_budget,
            info.encrypted,
            ctx,
        )?;
        sp.flush().map_err(|e| CoreError::io(&spill_path, e))?;
        let bytes = kept.iter().fold(0u64, |a, k| a.saturating_add(k.len));
        let mut chain = methods.chain(Kind::Lzma, bytes);
        if !info.encrypted && methods.password.is_some() {
            chain.remove(0); // it wasn't encrypted: keep it that way
        }
        w.set_content_methods(chain);
        let names: Vec<String> = kept.iter().map(|k| k.entry.name.clone()).collect();
        let feeds = kept
            .iter()
            .zip(&names)
            .map(|(k, n)| {
                // These bytes were counted while decoding; the feed's count is the re-read.
                sz::SourceReader::new(Feed::range(&spill_path, k.offset, k.len, n, ctx))
            })
            .collect::<Vec<_>>();
        report.kept += kept.len() as u64;
        let entries = kept.into_iter().map(|k| k.entry).collect();
        w.push_archive_entries(entries, feeds)
            .map_err(|e| write_err(archive, e, ctx))?;
    }
    drop(spill);
    drop(spill_guard);
    let header_encrypted = o.header_encrypted;
    drop(o);
    // New files, encrypted when a password is given.
    write_new(
        &mut w,
        adds,
        &mut names,
        &methods,
        opts,
        ctx,
        &mut report,
        archive,
        &tmp,
    )?;
    ctx.progress.set_note("");
    finish(
        w,
        &methods,
        // Names stay encrypted if they were.
        header_encrypted,
        &tmp,
        archive,
        true,
        ctx,
        &mut report,
    )?;
    guard.disarm();
    Ok(report)
}
