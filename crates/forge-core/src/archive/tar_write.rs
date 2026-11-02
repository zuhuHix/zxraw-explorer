//! Writing tar archives (plain or compressed) and single compressed files.
//!
//! Members are POSIX ustar headers; a PAX header is added when a name is not ASCII or
//! doesn't fit ustar's fields, or a size or time doesn't fit its numbers. Owners are 0/0
//! with empty names, modes 644 (files) and 755 (folders); modified times are kept. The
//! archive ends with two zero blocks, padded to a 10 KiB record as `tar` does.
//!
//! Editing rewrites the whole archive into a temp file next to it: kept members are copied
//! as they are (headers and data, nothing re-encoded but the compression), then new files
//! are appended, and the stream is compressed again. The original is decoded to its end
//! first, so a damaged one (when it has a checksum) is never rewritten with a fresh one. The
//! temp file replaces the original only when complete; a cancelled or failed edit leaves it
//! untouched.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::codec::{self, Codec, Encoder, StreamLevel};
use super::tar_backend::{self, TarBackend, stream_err};
use super::tar_read::{self, SeekFile, Source, TarParser};
use super::{
    ArchiveEdit, CreateOptions, CreateReport, DEFAULT_MAX_ARCHIVE_ENTRIES, SourceItem,
    collect_sources, motw, safety,
};
use crate::error::{CoreError, Result};
use crate::ops::{self, JobCtx, NodeKind, OpIssue, TempGuard};
use crate::paths;

/// Free space wanted beyond the estimate when rewriting.
const REWRITE_SLACK: u64 = 64 << 20;
/// Tar's record: archives end padded to a multiple of it.
const RECORD: u64 = 10240;
/// Largest number an 11-digit octal field holds.
const OCTAL_11_MAX: u64 = 0o777_7777_7777;

/// The stream level and threads for `codec` under `opts`.
pub(crate) fn level_for(codec: Codec, opts: &CreateOptions) -> StreamLevel {
    let mut l = opts.preset.stream_level(codec);
    let wanted = opts.worker_threads() as u32;
    l.threads = match codec {
        Codec::Xz => codec::xz_threads(l.level, l.extreme, wanted),
        Codec::Zstd => {
            let per = codec::zstd_memory(l.level, l.long).max(1);
            wanted.min(((1536u64 << 20) / per).clamp(1, 64) as u32)
        }
        _ => 1,
    };
    l
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// The temp file, counting what is written and remembering the first error (Brotli's last
/// write doesn't report it).
struct Out<'a> {
    inner: BufWriter<File>,
    ctx: JobCtx<'a>,
    written: u64,
    error: Option<io::ErrorKind>,
}

impl Write for Out<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.inner.write(buf) {
            Ok(n) => {
                self.written += n as u64;
                self.ctx.progress.add_bytes_out(n as u64);
                Ok(n)
            }
            Err(e) => {
                self.error.get_or_insert(e.kind());
                Err(e)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().inspect_err(|e| {
            self.error.get_or_insert(e.kind());
        })
    }
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// A temp file next to `dest` behind an encoder.
fn start<'a>(
    dest: &Path,
    codec: Codec,
    level: StreamLevel,
    gzip_name: Option<&str>,
    ctx: &JobCtx<'a>,
) -> Result<(Encoder<Out<'a>>, PathBuf, TempGuard)> {
    let tmp = parent_dir(dest).join(ops::partial_name());
    let file = File::create_new(&tmp).map_err(|e| CoreError::io(dest, e))?;
    let guard = TempGuard(Some(tmp.clone()));
    let out = Out {
        inner: BufWriter::with_capacity(1 << 20, file),
        ctx: *ctx,
        written: 0,
        error: None,
    };
    let enc = Encoder::new(codec, level, out, gzip_name).map_err(|e| CoreError::io(dest, e))?;
    Ok((enc, tmp, guard))
}

/// End the stream and publish `tmp` as `dest`.
fn finish(
    enc: Encoder<Out<'_>>,
    tmp: &Path,
    dest: &Path,
    replace: bool,
    ctx: &JobCtx<'_>,
    report: &mut CreateReport,
) -> Result<()> {
    ctx.cancel.check()?;
    let mut out = enc.finish().map_err(|e| CoreError::io(dest, e))?;
    out.flush().map_err(|e| CoreError::io(dest, e))?;
    if let Some(kind) = out.error {
        return Err(CoreError::io(dest, io::Error::from(kind)));
    }
    report.bytes_out = out.written;
    let file = out
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

fn wio(dest: &Path) -> impl Fn(io::Error) -> CoreError + '_ {
    move |e| CoreError::io(dest, e)
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

/// Write `v` as zero-padded octal filling `field` but its last byte (NUL); `false` if it
/// doesn't fit.
fn octal(field: &mut [u8], v: u64) -> bool {
    let digits = field.len() - 1;
    let text = format!("{v:0digits$o}");
    if text.len() > digits {
        return false;
    }
    field[..digits].copy_from_slice(text.as_bytes());
    field[digits] = 0;
    true
}

/// Write `v` as GNU base-256 (for sizes beyond octal; PAX states them too).
fn base256(field: &mut [u8], v: u64) {
    field.fill(0);
    let n = field.len();
    field[n - 8..].copy_from_slice(&v.to_be_bytes());
    field[0] |= 0x80;
}

fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    // "<len> <key>=<value>\n", where <len> counts its own digits.
    let body = key.len() + value.len() + 3; // ' ', '=', '\n'
    let mut len = body + 1;
    while len != body + len.to_string().len() {
        len = body + len.to_string().len();
    }
    let mut r = format!("{len} {key}=").into_bytes();
    r.extend_from_slice(value);
    r.push(b'\n');
    r
}

/// Split an ASCII name into ustar's prefix (155) and name (100) fields, if it fits.
fn ustar_split(name: &str) -> Option<(&str, &str)> {
    if !name.is_ascii() || name.contains('\0') {
        return None;
    }
    if name.len() <= 100 {
        return Some(("", name));
    }
    if name.len() > 256 {
        return None;
    }
    // The prefix ends at a '/' (not stored), as far right as the name part allows.
    name.char_indices()
        .rev()
        .find(|&(i, c)| c == '/' && i > 0 && i <= 155 && (1..=100).contains(&(name.len() - i - 1)))
        .map(|(i, _)| (&name[..i], &name[i + 1..]))
}

fn unix_secs(t: Option<SystemTime>) -> i64 {
    match t {
        Some(t) => match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
        },
        None => 0,
    }
}

/// A plain ASCII stand-in for a name ustar can't hold (readers without PAX show it).
fn ascii_fallback(name: &str, dir: bool) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_ascii() && c != '\0' { c } else { '_' })
        .collect();
    let keep = if dir { 99 } else { 100 };
    if s.len() > keep {
        let cut = s.len() - keep;
        s = s[cut..].to_owned();
    }
    if dir && !s.ends_with('/') {
        s.push('/');
    }
    s
}

fn header_block(
    name_field: &str,
    prefix: &str,
    flag: u8,
    size: u64,
    mtime: i64,
    dir: bool,
) -> [u8; 512] {
    let mut h = [0u8; 512];
    let n = name_field.as_bytes();
    h[..n.len().min(100)].copy_from_slice(&n[..n.len().min(100)]);
    octal(&mut h[100..108], if dir { 0o755 } else { 0o644 });
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    if !octal(&mut h[124..136], size) {
        base256(&mut h[124..136], size);
    }
    let secs = u64::try_from(mtime).unwrap_or(0).min(OCTAL_11_MAX);
    octal(&mut h[136..148], secs);
    h[156] = flag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let p = prefix.as_bytes();
    h[345..345 + p.len().min(155)].copy_from_slice(&p[..p.len().min(155)]);
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    let text = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(text.as_bytes());
    h
}

/// The headers of one member (a PAX header first when needed). `name` has no trailing `/`.
pub(crate) fn member_headers(
    name: &str,
    dir: bool,
    size: u64,
    mtime: Option<SystemTime>,
) -> Vec<u8> {
    let full = if dir {
        format!("{name}/")
    } else {
        name.to_owned()
    };
    let secs = unix_secs(mtime);
    let mut records = Vec::new();
    let split = ustar_split(&full);
    if split.is_none() {
        records.extend(pax_record("path", full.as_bytes()));
    }
    if size > OCTAL_11_MAX {
        records.extend(pax_record("size", size.to_string().as_bytes()));
    }
    if !(0..=OCTAL_11_MAX as i64).contains(&secs) {
        records.extend(pax_record("mtime", secs.to_string().as_bytes()));
    }
    let mut out = Vec::with_capacity(1536);
    let flag = if dir { b'5' } else { b'0' };
    if !records.is_empty() {
        let base = full.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let pax_name = ascii_fallback(&format!("PaxHeaders/{base}"), false);
        let rlen = records.len() as u64;
        out.extend_from_slice(&header_block(&pax_name, "", b'x', rlen, secs, false));
        out.extend_from_slice(&records);
        out.resize(out.len() + tar_read::padding(rlen) as usize, 0);
    }
    let (prefix, field) = match split {
        Some((p, n)) => (p.to_owned(), n.to_owned()),
        None => (String::new(), ascii_fallback(&full, dir)),
    };
    out.extend_from_slice(&header_block(
        &field,
        &prefix,
        flag,
        if dir { 0 } else { size },
        secs,
        dir,
    ));
    out
}

// ---------------------------------------------------------------------------
// Writing members
// ---------------------------------------------------------------------------

/// Append source items as members. Names already in `names` (lowercase) are skipped and
/// reported; links are not archived.
fn write_items<W: Write>(
    w: &mut W,
    items: Vec<SourceItem>,
    names: &mut HashSet<String>,
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
            .map(|w| w.size)
            .sum(),
    );
    let mut buf = vec![0u8; ops::COPY_CHUNK];
    for item in items {
        ctx.cancel.check()?;
        ctx.progress.add_items(1);
        if paths::paths_equal(&item.path, out) || paths::paths_equal(&item.path, tmp) {
            continue;
        }
        if !names.insert(item.name.to_lowercase()) {
            report.skipped.push(OpIssue::new(
                &item.path,
                "an entry with this name is already in the archive",
            ));
            continue;
        }
        match item.kind {
            NodeKind::Link => report
                .skipped
                .push(OpIssue::new(&item.path, "links are not archived")),
            NodeKind::Dir => {
                w.write_all(&member_headers(&item.name, true, 0, item.modified))
                    .map_err(wio(out))?;
                report.dirs += 1;
            }
            NodeKind::File => {
                ctx.progress.set_current(item.name.clone());
                // Opened first: a file that can't be read is skipped, not half-written.
                let mut f = match File::open(&item.path) {
                    Ok(f) => f,
                    Err(e) => {
                        report.skipped.push(OpIssue::new(
                            &item.path,
                            CoreError::io(&item.path, e).to_string(),
                        ));
                        continue;
                    }
                };
                let md = f.metadata().map_err(|e| CoreError::io(&item.path, e))?;
                let size = md.len();
                w.write_all(&member_headers(
                    &item.name,
                    false,
                    size,
                    md.modified().ok().or(item.modified),
                ))
                .map_err(wio(out))?;
                let copied = pump(&mut f, w, size, ctx, &mut buf, out);
                match copied {
                    Ok(n) if n == size => {}
                    Ok(n) => report.warnings.push(format!(
                        "{}: its size changed while it was archived ({n} of {size} bytes read); \
                         its copy in the archive is incomplete",
                        item.path.display()
                    )),
                    Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                    Err(e @ CoreError::Io { .. }) if is_out_err(&e, out) => return Err(e),
                    Err(e) => report.warnings.push(format!(
                        "{}: it could not be read completely ({e}); its copy in the archive \
                         is incomplete",
                        item.path.display()
                    )),
                }
                report.bytes_in += size;
                report.files += 1;
            }
        }
    }
    Ok(())
}

fn is_out_err(e: &CoreError, out: &Path) -> bool {
    matches!(e, CoreError::Io { path, .. } if path == out)
}

/// Copy exactly `size` bytes of `f` (zeros for what it no longer has), then the padding.
/// Returns the bytes actually read from the file.
fn pump<W: Write>(
    f: &mut File,
    w: &mut W,
    size: u64,
    ctx: &JobCtx<'_>,
    buf: &mut [u8],
    out: &Path,
) -> Result<u64> {
    let mut left = size;
    let mut read_err = None;
    while left > 0 {
        ctx.cancel.check()?;
        let want = (buf.len() as u64).min(left) as usize;
        let n = match f.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                read_err = Some(e);
                break;
            }
        };
        w.write_all(&buf[..n]).map_err(wio(out))?;
        ctx.progress.add_bytes(n as u64);
        left -= n as u64;
    }
    let read = size - left;
    // Keep the archive consistent whatever the file did.
    let zeros = [0u8; 4096];
    let mut fill = left + tar_read::padding(size);
    while fill > 0 {
        let n = fill.min(zeros.len() as u64) as usize;
        w.write_all(&zeros[..n]).map_err(wio(out))?;
        fill -= n as u64;
    }
    match read_err {
        Some(e) => Err(CoreError::Invalid(e.to_string())),
        None => Ok(read),
    }
}

/// Two zero blocks, then zeros to the end of the record.
fn write_end<W: Write>(w: &mut W, written: u64, out: &Path) -> Result<()> {
    let end = written + 2 * tar_read::BLOCK;
    let total = end.div_ceil(RECORD) * RECORD;
    let zeros = vec![0u8; (total - written) as usize];
    w.write_all(&zeros).map_err(wio(out))
}

/// Counts bytes written through it (the uncompressed tar's length).
struct Tally<'w, W> {
    w: &'w mut W,
    n: u64,
}

impl<W: Write> Write for Tally<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.w.write(buf)?;
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

pub(super) fn create(
    codec: Codec,
    sources: &[PathBuf],
    out: &Path,
    opts: &CreateOptions,
    ctx: &JobCtx<'_>,
) -> Result<CreateReport> {
    if !opts.encryption.is_none() {
        return Err(CoreError::Unsupported(
            "tar archives can't be encrypted; choose ZIP or 7z for a password".into(),
        ));
    }
    if paths::exists_no_follow(out) {
        return Err(CoreError::AlreadyExists(out.to_path_buf()));
    }
    let mut report = CreateReport::default();
    let items = collect_sources(sources, "", ctx, &mut report.skipped)?;
    let (mut enc, tmp, mut guard) = start(out, codec, level_for(codec, opts), None, ctx)?;
    let mut tally = Tally { w: &mut enc, n: 0 };
    let mut names = HashSet::new();
    write_items(&mut tally, items, &mut names, ctx, &mut report, out, &tmp)?;
    let written = tally.n;
    write_end(&mut enc, written, out)?;
    finish(enc, &tmp, out, false, ctx, &mut report)?;
    guard.disarm();
    Ok(report)
}

/// Compress one file into a single compressed file (`.gz`, `.xz`...). A gzip file keeps the
/// source's name (when ASCII) in its header.
pub(super) fn compress_file(
    codec: Codec,
    src: &Path,
    out: &Path,
    opts: &CreateOptions,
    ctx: &JobCtx<'_>,
) -> Result<CreateReport> {
    if !opts.encryption.is_none() {
        return Err(CoreError::Unsupported(format!(
            ".{} files can't be encrypted",
            codec.extension()
        )));
    }
    if paths::exists_no_follow(out) {
        return Err(CoreError::AlreadyExists(out.to_path_buf()));
    }
    if paths::paths_equal(src, out) {
        return Err(CoreError::Invalid(
            "a file can't be compressed onto itself".into(),
        ));
    }
    let md = std::fs::symlink_metadata(src).map_err(|e| CoreError::io(src, e))?;
    if crate::ops::node_kind(&md) != NodeKind::File {
        return Err(CoreError::Invalid(format!(
            "{}: only a file can be compressed to .{}; choose tar.{} for folders",
            paths::display_path(src),
            codec.extension(),
            codec.extension()
        )));
    }
    let mut f = File::open(src).map_err(|e| CoreError::io(src, e))?;
    let size = f.metadata().map_err(|e| CoreError::io(src, e))?.len();
    let name = paths::name_of(src);
    let gzip_name = name.is_ascii().then_some(name.as_str());
    let (mut enc, tmp, mut guard) = start(out, codec, level_for(codec, opts), gzip_name, ctx)?;
    ctx.progress.add_total_items(1);
    ctx.progress.add_total_bytes(size);
    ctx.progress.set_current(name.clone());
    let mut buf = vec![0u8; ops::COPY_CHUNK];
    let mut total = 0u64;
    loop {
        ctx.cancel.check()?;
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(CoreError::io(src, e)),
        };
        enc.write_all(&buf[..n]).map_err(wio(out))?;
        ctx.progress.add_bytes(n as u64);
        total += n as u64;
    }
    ctx.progress.add_items(1);
    let mut report = CreateReport {
        files: 1,
        bytes_in: total,
        ..Default::default()
    };
    finish(enc, &tmp, out, false, ctx, &mut report)?;
    guard.disarm();
    Ok(report)
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

pub(super) fn update(
    backend: &TarBackend,
    archive: &Path,
    edit: &ArchiveEdit,
    opts: &CreateOptions,
    ctx: &JobCtx<'_>,
) -> Result<CreateReport> {
    if !opts.encryption.is_none() {
        return Err(CoreError::Unsupported(
            "tar archives can't be encrypted; files are added without a password".into(),
        ));
    }
    let codec = backend.codec;
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

    let s = tar_backend::scan(archive, codec, DEFAULT_MAX_ARCHIVE_ENTRIES, Some(ctx))?;
    // Members kept: listed (the last of its name), not deleted, not replaced.
    let mut keep = vec![false; s.members.len()];
    let mut names = HashSet::new();
    for e in &s.entries {
        let key = e.name.trim_end_matches('/').to_lowercase();
        if is_deleted(&e.name) {
            report.deleted += 1;
        } else if added.contains(&key) {
            report.replaced += 1;
        } else {
            keep[e.index] = true;
            names.insert(key);
        }
    }
    // Everything is decoded and written again: the limits apply, and the new archive must
    // fit next to the old one.
    let limits = &opts.limits;
    let decoded = if codec == Codec::None {
        s.file_size
    } else {
        s.decoded
    };
    if decoded > limits.max_total_bytes {
        return Err(CoreError::UnsafePath(format!(
            "{}: the archive expands to more than {} bytes",
            paths::display_path(archive),
            limits.max_total_bytes
        )));
    }
    let add_bytes = adds
        .iter()
        .filter(|a| a.kind == NodeKind::File)
        .fold(0u64, |a, i| a.saturating_add(i.size));
    let need = s
        .file_size
        .saturating_add(add_bytes)
        .saturating_add(REWRITE_SLACK);
    let dir = parent_dir(archive);
    let free = super::sevenz_writer::free_space(dir)?;
    if free < need {
        return Err(CoreError::Invalid(format!(
            "not enough free space next to {} to rewrite it: {} MB needed, {} MB free",
            paths::display_path(archive),
            need >> 20,
            free >> 20
        )));
    }
    super::zip_backend::remove_stale_partials(dir, std::time::Duration::from_secs(3600));
    ctx.progress.add_total_bytes(decoded);
    ctx.progress.set_note(if codec == Codec::None {
        format!("Rewriting {} MB", decoded.div_ceil(1 << 20))
    } else {
        format!("Recompressing {} MB", decoded.div_ceil(1 << 20))
    });

    let (mut enc, tmp, mut guard) = start(archive, codec, level_for(codec, opts), None, ctx)?;
    let mut tally = Tally { w: &mut enc, n: 0 };
    let err = |e| stream_err(archive, e);
    if codec == Codec::None {
        let src = SeekFile::open(archive, 0).map_err(|e| CoreError::io(archive, e))?;
        copy_kept(
            TarParser::new(src, s.members.len()),
            &s,
            &keep,
            &mut tally,
            ctx,
            archive,
            None,
        )?;
    } else {
        let file = File::open(archive).map_err(|e| CoreError::io(archive, e))?;
        let dec = codec::decoder(
            codec,
            std::io::BufReader::with_capacity(256 * 1024, file),
            opts.limits.max_decoder_memory,
        )
        .map_err(err)?;
        copy_kept(
            TarParser::new(dec, s.members.len()),
            &s,
            &keep,
            &mut tally,
            ctx,
            archive,
            Some(s.decoded),
        )?;
    }
    report.kept = keep.iter().filter(|k| **k).count() as u64;
    ctx.progress.set_note("");
    write_items(
        &mut tally,
        adds,
        &mut names,
        ctx,
        &mut report,
        archive,
        &tmp,
    )?;
    let written = tally.n;
    write_end(&mut enc, written, archive)?;
    finish(enc, &tmp, archive, true, ctx, &mut report)?;
    guard.disarm();
    Ok(report)
}

/// Copy the kept members' headers and data as they are. A compressed source is read to its
/// end (`decoded` bytes), so its checksum is checked before anything is published.
fn copy_kept<R: Source, W: Write>(
    mut p: TarParser<R>,
    s: &tar_backend::Scan,
    keep: &[bool],
    w: &mut W,
    ctx: &JobCtx<'_>,
    archive: &Path,
    decoded: Option<u64>,
) -> Result<()> {
    let err = |e| stream_err(archive, e);
    let changed = || {
        CoreError::Invalid(format!(
            "{}: the archive changed while it was read",
            paths::display_path(archive)
        ))
    };
    p.keep_raw_headers();
    let mut buf = vec![0u8; ops::COPY_CHUNK];
    for (k, want) in s.members.iter().enumerate() {
        ctx.cancel.check()?;
        let m = p.next_member().map_err(err)?.ok_or_else(changed)?;
        if m.header_hash != want.header_hash || m.start != want.start {
            return Err(changed());
        }
        ctx.progress.add_bytes(m.data - m.start);
        if !keep[k] {
            continue;
        }
        let raw = p.raw_headers.take().unwrap_or_default();
        w.write_all(&raw).map_err(wio(archive))?;
        p.raw_headers = Some(Vec::new());
        let mut data = p.data();
        let mut left = m.stored;
        while left > 0 {
            ctx.cancel.check()?;
            let want = (buf.len() as u64).min(left) as usize;
            let n = data.read(&mut buf[..want]).map_err(err)?;
            if n == 0 {
                return Err(changed());
            }
            w.write_all(&buf[..n]).map_err(wio(archive))?;
            ctx.progress.add_bytes(n as u64);
            left -= n as u64;
        }
        let pad = vec![0u8; tar_read::padding(m.stored) as usize];
        w.write_all(&pad).map_err(wio(archive))?;
    }
    if p.next_member().map_err(err)?.is_some() {
        return Err(changed());
    }
    if let Some(decoded) = decoded {
        // To the stream's end: its checksum.
        let tar_end = p.pos();
        let mut rest = p.into_inner();
        let mut n = 0u64;
        loop {
            ctx.cancel.check()?;
            match rest.read(&mut buf) {
                Ok(0) => break,
                Ok(k) => {
                    n += k as u64;
                    if tar_end + n > decoded {
                        return Err(changed());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(err(e)),
            }
        }
        if tar_end + n != decoded {
            return Err(changed());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pax_record_lengths_count_themselves() {
        let r = pax_record("path", b"abc");
        assert_eq!(r, b"12 path=abc\n");
        let long = vec![b'x'; 95];
        let r = pax_record("path", &long);
        let text = String::from_utf8(r.clone()).unwrap();
        let (len, _) = text.split_once(' ').unwrap();
        assert_eq!(len.parse::<usize>().unwrap(), r.len());
    }

    #[test]
    fn names_go_to_ustar_fields_or_pax() {
        assert_eq!(ustar_split("a/b.txt"), Some(("", "a/b.txt")));
        let long = format!("{}/{}", "d".repeat(120), "f".repeat(90));
        assert_eq!(ustar_split(&long), Some((&long[..120], &long[121..])));
        assert_eq!(ustar_split("café"), None);
        // A long folder name never leaves the name field empty.
        let dir = format!("{}/", "d".repeat(120));
        assert_eq!(ustar_split(&dir), None);
        assert_eq!(ustar_split(&"x".repeat(101)), None);
        let h = member_headers("café/ü.txt", false, 5, None);
        assert_eq!(h.len(), 512 * 3);
        assert_eq!(h[156], b'x');
        let h = member_headers("plain.txt", false, 5, None);
        assert_eq!(h.len(), 512);
        assert!(tar_read::checksum_ok(h[..512].try_into().unwrap()));
    }
}
