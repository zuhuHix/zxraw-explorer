//! The ZIP backend: reads Stored, Deflate, Deflate64, bzip2, LZMA, XZ, Zstandard, PPMd,
//! AES (AE-1/AE-2) and ZipCrypto; writes Stored, Deflate (zlib-rs, or Zopfli for Max),
//! Zstandard, bzip2 and XZ, with AES-256 (AE-2) or legacy ZipCrypto encryption.
//!
//! Names are decoded by the UTF-8 flag, then the Info-ZIP Unicode path field, then CP437.
//! Small files are compressed on a bounded pool of threads; large files stream through one
//! at a time, so memory stays bounded whatever the input size.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use chrono::{Datelike, TimeZone, Timelike};
use parking_lot::Mutex;
use zip::read::HasZipMetadata;
use zip::result::ZipError;
use zip::unstable::format::flags::ZipFlags;
use zip::unstable::write::FileOptionsExt;
use zip::write::{FileOptions, ZipFileBuilder};
use zip::{AesMode, CompressionMethod, PreparedZipFile, ZipArchive, ZipReadOptions, ZipWriter};

use super::{
    ArchiveBackend, ArchiveEdit, ArchiveEntry, ArchiveFormat, ArchiveListing, CreateOptions,
    CreateReport, Encryption, EntryEncryption, EntryVisitor, ExtractLimits, Method, Password,
    SourceItem, collect_sources, cp437, crypto_peek, motw, safety, zip_merge,
};
use crate::error::{CoreError, Result};
use crate::ops::{self, JobCtx, NodeKind, OpIssue, TempGuard};
use crate::paths;

pub(crate) struct ZipBackend;

pub(crate) static ZIP: ZipBackend = ZipBackend;

/// Files up to this size are compressed in memory on worker threads.
const PARALLEL_MAX_FILE: u64 = 4 * 1024 * 1024;

fn zip_err(path: &Path, e: ZipError) -> CoreError {
    match e {
        ZipError::Io(io) => CoreError::io(path, io),
        ZipError::InvalidPassword => CoreError::WrongPassword(paths::display_path(path)),
        ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED) => {
            CoreError::PasswordRequired(paths::display_path(path))
        }
        other => CoreError::Invalid(format!("{}: {other}", path.display())),
    }
}

fn entry_err(archive: &Path, name: &str, e: ZipError) -> CoreError {
    let label = format!("{}\\{name}", paths::display_path(archive));
    match e {
        ZipError::InvalidPassword => CoreError::WrongPassword(label),
        ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED) => {
            CoreError::PasswordRequired(label)
        }
        ZipError::Io(io) => CoreError::Invalid(format!("{name}: {io}")),
        other => CoreError::Invalid(format!("{name}: {other}")),
    }
}

fn open_zip(path: &Path) -> Result<ZipArchive<BufReader<File>>> {
    let f = File::open(path).map_err(|e| CoreError::io(path, e))?;
    ZipArchive::new(BufReader::new(f)).map_err(|e| zip_err(path, e))
}

fn zip_time_to_system(dt: zip::DateTime) -> Option<SystemTime> {
    let naive =
        chrono::NaiveDate::from_ymd_opt(dt.year().into(), dt.month().into(), dt.day().into())?
            .and_hms_opt(dt.hour().into(), dt.minute().into(), dt.second().into())?;
    // ZIP timestamps are local time without a zone.
    chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .map(SystemTime::from)
}

fn system_to_zip_time(t: SystemTime) -> zip::DateTime {
    let local: chrono::DateTime<chrono::Local> = t.into();
    zip::DateTime::from_date_and_time(
        u16::try_from(local.year()).unwrap_or(1980),
        local.month() as u8,
        local.day() as u8,
        local.hour() as u8,
        local.minute() as u8,
        local.second() as u8,
    )
    .unwrap_or_default()
}

fn method_name(m: CompressionMethod) -> String {
    #[allow(deprecated)]
    match m {
        CompressionMethod::Stored => "Stored".into(),
        CompressionMethod::Deflated => "Deflate".into(),
        CompressionMethod::Deflate64 => "Deflate64".into(),
        CompressionMethod::Bzip2 => "BZip2".into(),
        CompressionMethod::Zstd => "Zstandard".into(),
        CompressionMethod::Lzma => "LZMA".into(),
        CompressionMethod::Xz => "XZ".into(),
        CompressionMethod::Ppmd => "PPMd".into(),
        CompressionMethod::Aes => "AES".into(),
        other => format!("{other:?}"),
    }
}

/// Decode an entry name: UTF-8 when flagged (the Unicode path field, when its checksum
/// matches, has already replaced the raw name and set the flag), CP437 otherwise.
fn decode_name<R: Read>(f: &zip::read::ZipFile<'_, R>) -> String {
    let raw = f.name_raw();
    let name = if f.flags().matching(ZipFlags::LanguageEncoding) {
        String::from_utf8_lossy(raw).into_owned()
    } else {
        cp437::decode(raw)
    };
    safety::normalize_entry_name(&name)
}

fn to_entry<R: Read>(index: usize, f: &zip::read::ZipFile<'_, R>) -> ArchiveEntry {
    let name = decode_name(f);
    let is_dir = name.ends_with('/') || f.is_dir();
    let encryption = f.encrypted().then(|| {
        if f.get_metadata().aes_settings().is_some() {
            EntryEncryption::Aes
        } else {
            EntryEncryption::ZipCrypto
        }
    });
    ArchiveEntry {
        index,
        safe_name: safety::validate_entry_name(&name).is_ok(),
        is_dir,
        is_symlink: f.is_symlink(),
        encrypted: f.encrypted(),
        encryption,
        size: f.size(),
        compressed_size: f.compressed_size(),
        modified: f.last_modified().and_then(zip_time_to_system),
        crc32: f.crc32(),
        method: method_name(f.compression()),
        name,
        solid: false,
        block_offset: 0,
        unchecked: false,
        checked_by_block: false,
    }
}

impl ArchiveBackend for ZipBackend {
    fn format(&self) -> ArchiveFormat {
        ArchiveFormat::Zip
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["zip", "zipx"]
    }

    fn sniff(&self, head: &[u8]) -> bool {
        // A local header, an empty archive's end record, or a spanned archive's marker.
        head.starts_with(b"PK\x03\x04")
            || head.starts_with(b"PK\x05\x06")
            || head.starts_with(b"PK\x07\x08")
    }

    fn list(
        &self,
        archive: &Path,
        max_entries: usize,
        _password: Option<&Password>,
    ) -> Result<ArchiveListing> {
        let mut za = open_zip(archive)?;
        if za.len() > max_entries {
            return Err(CoreError::Invalid(format!(
                "archive has {} entries (listing limit {max_entries})",
                za.len()
            )));
        }
        let comment = String::from_utf8_lossy(za.comment()).into_owned();
        let mut entries = Vec::with_capacity(za.len());
        for i in 0..za.len() {
            let f = za.by_index_raw(i).map_err(|e| zip_err(archive, e))?;
            entries.push(to_entry(i, &f));
        }
        Ok(ArchiveListing {
            format: ArchiveFormat::Zip,
            entries,
            comment,
        })
    }

    fn preflight(
        &self,
        archive: &Path,
        indices: &[usize],
        password: Option<&Password>,
        limits: &ExtractLimits,
    ) -> Result<()> {
        let mut za = open_zip(archive)?;
        if za
            .has_overlapping_entries()
            .map_err(|e| zip_err(archive, e))?
        {
            return Err(CoreError::UnsafePath(
                "archive has overlapping entries (zip bomb)".into(),
            ));
        }
        for &i in indices {
            let mut f = za.by_index_raw(i).map_err(|e| zip_err(archive, e))?;
            let method = f.compression();
            if !matches!(
                method,
                CompressionMethod::Lzma | CompressionMethod::Xz | CompressionMethod::Ppmd
            ) {
                continue;
            }
            let aes = f.get_metadata().aes_settings().map(|(mode, _)| mode);
            // Room for an AES salt and verifier or the ZipCrypto header before the 64 bytes.
            let mut head = [0u8; 64 + 18];
            let mut n = 0;
            while n < head.len() {
                match f.read(&mut head[n..]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(CoreError::io(archive, e)),
                }
            }
            let decrypted;
            let head = match (f.encrypted(), password, aes) {
                (false, ..) => &head[..n],
                // Entries that can't be decrypted are not decoded either (the password is
                // checked before this).
                (true, None, _) => continue,
                (true, Some(pw), Some(mode)) => {
                    match crypto_peek::aes_head(&head[..n], mode, pw.as_bytes(), 64) {
                        Some(h) => {
                            decrypted = h;
                            &decrypted[..]
                        }
                        None => continue,
                    }
                }
                (true, Some(pw), None) => {
                    // The header's check byte: the CRC's high byte, or the modified time's
                    // with a data descriptor (bit 3).
                    let check = if f.flags().matching(ZipFlags::UsingDataDescriptor) {
                        f.last_modified().map(|t| (t.timepart() >> 8) as u8)
                    } else {
                        Some((f.crc32() >> 24) as u8)
                    };
                    // Another password than this entry's: it fails when read, as a wrong
                    // password; nothing it decrypts is parsed here.
                    let Some(h) = check.and_then(|check| {
                        crypto_peek::zipcrypto_head(&head[..n], pw.as_bytes(), check, 64)
                    }) else {
                        continue;
                    };
                    decrypted = h;
                    &decrypted[..]
                }
            };
            let need = match method {
                CompressionMethod::Lzma => safety::zip_lzma_dictionary(head),
                CompressionMethod::Xz => safety::xz_dictionary(head),
                _ => safety::zip_ppmd_memory(head),
            };
            if let Some(need) = need
                && need > limits.max_decoder_memory
            {
                return Err(CoreError::UnsafePath(format!(
                    "{}: needs {} MiB of decoder memory (limit {} MiB)",
                    decode_name(&f),
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
        _ctx: Option<&JobCtx<'_>>,
        visit: &mut EntryVisitor<'_>,
    ) -> Result<()> {
        let mut za = open_zip(archive)?;
        for i in 0..za.len() {
            let entry = {
                let f = za.by_index_raw(i).map_err(|e| zip_err(archive, e))?;
                to_entry(i, &f)
            };
            if !wanted(&entry) {
                continue;
            }
            let options = ZipReadOptions::new()
                .password(password.map(Password::as_bytes))
                .enforce_header_consistency(true);
            match za.by_index_with_options(i, options) {
                Ok(mut f) => visit(&entry, Ok(&mut f))?,
                Err(e) => visit(&entry, Err(entry_err(archive, &entry.name, e)))?,
            }
        }
        Ok(())
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
        if paths::exists_no_follow(out) {
            return Err(CoreError::AlreadyExists(out.to_path_buf()));
        }
        let mut report = CreateReport::default();
        let items = collect_sources(sources, "", ctx, &mut report.skipped)?;
        let (mut zw, tmp, mut guard) = start_output(out, ctx)?;
        let mut names = HashSet::new();
        write_items(
            &mut zw,
            items,
            &mut names,
            opts,
            ctx,
            &mut report,
            out,
            &tmp,
        )?;
        finish_output(zw, &tmp, out, false, &mut report)?;
        guard.disarm();
        Ok(report)
    }

    fn update(
        &self,
        archive: &Path,
        edit: &ArchiveEdit,
        opts: &CreateOptions,
        ctx: &JobCtx<'_>,
    ) -> Result<CreateReport> {
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

        // Split archives are refused before anything else is opened or written.
        zip_merge::check_single_disk(
            archive,
            &mut File::open(archive).map_err(|e| CoreError::io(archive, e))?,
        )?;
        if let Some(dir) = archive.parent() {
            remove_stale_partials(dir, STALE_PARTIAL_AGE);
        }
        // Kept entries are copied record by record, as stored (see `zip_merge`): nothing is
        // decoded, decrypted or re-encoded.
        let mut za = open_zip(archive)?;
        let mut src = File::open(archive).map_err(|e| CoreError::io(archive, e))?;
        let records = zip_merge::raw_records(archive, &mut src, &mut za)?;
        let comment = za.comment().to_vec();
        let mut names = HashSet::new();
        let mut kept = Vec::new();
        for (i, record) in records.iter().enumerate() {
            ctx.cancel.check()?;
            let name = decode_name(&za.by_index_raw(i).map_err(|e| zip_err(archive, e))?);
            let key = name.trim_end_matches('/').to_lowercase();
            if is_deleted(&name) {
                report.deleted += 1;
            } else if added.contains(&key) {
                report.replaced += 1;
            } else if names.insert(key) {
                kept.push(record);
            }
            // else: the first of duplicate names is kept
        }
        drop(za);

        // New files are compressed into a side archive next to it, then copied the same way.
        let side = if adds.is_empty() {
            None
        } else {
            let (mut zw, side_tmp, side_guard) = start_output(archive, ctx)?;
            write_items(
                &mut zw,
                adds,
                &mut names,
                opts,
                ctx,
                &mut report,
                archive,
                &side_tmp,
            )?;
            let w = zw.finish().map_err(|e| zip_err(archive, e))?;
            drop(
                w.inner
                    .into_inner()
                    .map_err(|e| CoreError::io(archive, e.into_error()))?,
            );
            Some((side_tmp, side_guard))
        };

        let dir = archive
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let tmp = dir.join(ops::partial_name());
        let file = File::create_new(&tmp).map_err(|e| CoreError::io(archive, e))?;
        let mut guard = TempGuard(Some(tmp.clone()));
        let mut out = zip_merge::MergeWriter::new(CountingWriter {
            inner: BufWriter::with_capacity(1 << 20, file),
            ctx: *ctx,
            high: 0,
            pos: 0,
        });
        let mut buf = vec![0u8; 1 << 20];
        ctx.progress
            .add_total_bytes(kept.iter().map(|r| r.end - r.start).sum());
        for record in kept {
            out.copy(&mut src, record, ctx, &mut buf)?;
            ctx.progress.add_bytes(record.end - record.start);
            report.kept += 1;
        }
        drop(src);
        if let Some((side_tmp, _guard)) = &side {
            let mut side_file = File::open(side_tmp).map_err(|e| CoreError::io(archive, e))?;
            let mut side_zip = open_zip(side_tmp)?;
            for record in zip_merge::raw_records(side_tmp, &mut side_file, &mut side_zip)? {
                out.copy(&mut side_file, &record, ctx, &mut buf)?;
            }
        }
        ctx.cancel.check()?;
        let w = out
            .finish(&comment)
            .map_err(|e| CoreError::io(archive, e))?;
        report.bytes_out = w.high;
        let file = w
            .inner
            .into_inner()
            .map_err(|e| CoreError::io(archive, e.into_error()))?;
        file.sync_all().map_err(|e| CoreError::io(archive, e))?;
        drop(file);
        drop(side);
        // The rebuilt file replaces the original: keep its Mark-of-the-Web.
        if let Err(e) = motw::copy_zone_stream(archive, &tmp) {
            tracing::warn!("could not keep the zone of {}: {e}", archive.display());
        }
        ops::place(&tmp, archive, true)?;
        guard.disarm();
        Ok(report)
    }
}

/// Temp files of an update cut short by a crash are removed by the next update of an
/// archive in the same folder once they are this old.
const STALE_PARTIAL_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Shown when an edit is refused because the zip has something in front of it.
pub const SFX_NOT_EDITABLE: &str = "Self-extracting archives can't be edited; extract them instead";

/// Refuse a zip with a prefix (a self-extractor stub): its first local header is not at the
/// start, or its directory places the archive further in. A rebuild would drop the stub.
pub(crate) fn check_no_prefix(archive: &Path, file: &mut File) -> Result<()> {
    let mut head = [0u8; 4];
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut head))
        .map_err(|e| CoreError::io(archive, e))?;
    let starts_right = head == *b"PK\x03\x04" || head == *b"PK\x05\x06";
    file.seek(SeekFrom::Start(0))
        .map_err(|e| CoreError::io(archive, e))?;
    let offset = ZipArchive::new(BufReader::new(&mut *file))
        .map(|za| za.offset())
        .map_err(|e| zip_err(archive, e))?;
    if !starts_right || offset > 0 {
        return Err(CoreError::Unsupported(SFX_NOT_EDITABLE.into()));
    }
    Ok(())
}

/// Whether `file` has a ZIP end-of-central-directory record where one can be: in its last
/// 22 + 65,535 bytes (the record plus the longest comment).
pub(crate) fn has_end_record(file: &mut File) -> bool {
    let Ok(len) = file.seek(SeekFrom::End(0)) else {
        return false;
    };
    let tail = len.min(22 + 65_535);
    if tail < 22 || file.seek(SeekFrom::Start(len - tail)).is_err() {
        return false;
    }
    let mut buf = vec![0u8; tail as usize];
    if file.read_exact(&mut buf).is_err() {
        return false;
    }
    buf.windows(4).rev().any(|w| w == b"PK\x05\x06")
}

/// Remove our temp files in `dir` (exactly [`ops::is_partial_name`]) last written more than
/// `age` ago that nothing has open. Each is opened exclusively (no sharing) without following
/// a reparse point, and deleted through that handle only if it is a plain file with a single
/// name: one still being written, a link, or a hard link to something else is left alone.
pub(crate) fn remove_stale_partials(dir: &Path, age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for e in entries.flatten() {
        if !ops::is_partial_name(&e.file_name().to_string_lossy()) {
            continue;
        }
        let old = e.metadata().is_ok_and(|m| {
            m.is_file()
                && m.modified()
                    .is_ok_and(|t| now.duration_since(t).is_ok_and(|a| a > age))
        });
        if old {
            delete_if_unused(&e.path());
        }
    }
}

#[cfg(windows)]
fn delete_if_unused(path: &Path) {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_DISPOSITION_INFO, FileDispositionInfo,
        GetFileInformationByHandle, SetFileInformationByHandle,
    };
    const DELETE: u32 = 0x0001_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    // Share mode 0: fails while any other handle is open.
    let Ok(file) = std::fs::OpenOptions::new()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    else {
        return;
    };
    let handle = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `handle` is open for the duration of the call and `info` is a valid out value.
    if unsafe { GetFileInformationByHandle(handle, &mut info) }.is_err()
        || info.nNumberOfLinks != 1
        || info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
    {
        return;
    }
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the pointer and size describe `disposition`, which outlives the call.
    let _ = unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfo,
            (&raw const disposition).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
}

#[cfg(not(windows))]
fn delete_if_unused(_: &Path) {}

/// Counts what is written to the archive file, for the progress output figure.
struct CountingWriter<'a, W> {
    inner: W,
    ctx: JobCtx<'a>,
    high: u64,
    pos: u64,
}

impl<W: Write> Write for CountingWriter<'_, W> {
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

impl<W: Seek> Seek for CountingWriter<'_, W> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

type Output<'a> = ZipWriter<CountingWriter<'a, BufWriter<File>>>;

/// Open a temp file next to `dest` for a new archive.
fn start_output<'a>(dest: &Path, ctx: &JobCtx<'a>) -> Result<(Output<'a>, PathBuf, TempGuard)> {
    let dir = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = dir.join(ops::partial_name());
    let file = File::create_new(&tmp).map_err(|e| CoreError::io(dest, e))?;
    let guard = TempGuard(Some(tmp.clone()));
    let writer = CountingWriter {
        inner: BufWriter::with_capacity(1 << 20, file),
        ctx: *ctx,
        high: 0,
        pos: 0,
    };
    Ok((ZipWriter::new(writer), tmp, guard))
}

fn finish_output(
    zw: Output<'_>,
    tmp: &Path,
    dest: &Path,
    replace: bool,
    report: &mut CreateReport,
) -> Result<()> {
    let w = zw.finish().map_err(|e| zip_err(dest, e))?;
    report.bytes_out = w.high;
    let file = w
        .inner
        .into_inner()
        .map_err(|e| CoreError::io(dest, e.into_error()))?;
    file.sync_all().map_err(|e| CoreError::io(dest, e))?;
    drop(file);
    ops::place(tmp, dest, replace)
}

fn file_options<'k>(
    item: &SourceItem,
    opts: &'k CreateOptions,
) -> Result<FileOptions<'k, 'static, ()>> {
    let ext = crate::model::ext_of(&item.path);
    let (method, level) = opts.method_for(&ext, item.size);
    let mtime = item.modified.map(system_to_zip_time).unwrap_or_default();
    let o = FileOptions::<()>::default()
        .compression_method(method.zip())
        .compression_level(if method == Method::Stored {
            None
        } else {
            level
        })
        .last_modified_time(mtime)
        .large_file(item.size >= u32::MAX as u64);
    Ok(match &opts.encryption {
        Encryption::None => o,
        Encryption::Aes256(pw) => o.with_aes_encryption_bytes(AesMode::Aes256, pw.as_bytes()),
        Encryption::ZipCryptoLegacy(pw) => o
            .with_deprecated_encryption(pw.as_bytes())
            .map_err(|_| CoreError::Invalid("the password must not be empty".into()))?,
    })
}

/// Read `path` into `w` in chunks, checking for cancel/pause between them.
fn pump(path: &Path, w: &mut dyn Write, ctx: &JobCtx<'_>, buf: &mut [u8]) -> Result<u64> {
    let mut src = File::open(path).map_err(|e| CoreError::io(path, e))?;
    let mut total = 0;
    loop {
        ctx.cancel.check()?;
        let n = match src.read(buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(CoreError::io(path, e)),
        };
        w.write_all(&buf[..n])
            .map_err(|e| CoreError::Invalid(format!("{}: {e}", path.display())))?;
        ctx.progress.add_bytes(n as u64);
        total += n as u64;
    }
}

/// Write `items` into `zw`: small files are compressed in parallel batches, large ones
/// streamed. `names` holds the lowercase names already in the archive.
#[allow(clippy::too_many_arguments)]
fn write_items(
    zw: &mut Output<'_>,
    items: Vec<SourceItem>,
    names: &mut HashSet<String>,
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
            .map(|w| w.size)
            .sum(),
    );
    let threads = opts.worker_threads();
    let parallel = threads > 1 && opts.encryption.is_none();
    let batch_budget = threads as u64 * 2 * PARALLEL_MAX_FILE;
    let mut buf = vec![0u8; ops::COPY_CHUNK];
    let mut batch: Vec<SourceItem> = Vec::new();
    let mut batch_bytes = 0u64;
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
        if parallel && item.kind == NodeKind::File && item.size <= PARALLEL_MAX_FILE {
            batch_bytes += item.size;
            batch.push(item);
            if batch_bytes >= batch_budget || batch.len() >= 512 {
                flush_batch(
                    zw,
                    std::mem::take(&mut batch),
                    opts,
                    threads,
                    ctx,
                    report,
                    out,
                )?;
                batch_bytes = 0;
            }
            continue;
        }
        flush_batch(
            zw,
            std::mem::take(&mut batch),
            opts,
            threads,
            ctx,
            report,
            out,
        )?;
        batch_bytes = 0;
        ctx.progress.add_items(1);
        write_one(zw, &item, opts, ctx, report, out, &mut buf)?;
    }
    flush_batch(zw, batch, opts, threads, ctx, report, out)
}

fn write_one(
    zw: &mut Output<'_>,
    item: &SourceItem,
    opts: &CreateOptions,
    ctx: &JobCtx<'_>,
    report: &mut CreateReport,
    out: &Path,
    buf: &mut [u8],
) -> Result<()> {
    let zerr = |e| zip_err(out, e);
    match item.kind {
        NodeKind::Link => report
            .skipped
            .push(OpIssue::new(&item.path, "links are not archived")),
        NodeKind::Dir => {
            let mtime = item.modified.map(system_to_zip_time).unwrap_or_default();
            let o = FileOptions::<()>::default().last_modified_time(mtime);
            zw.add_directory(format!("{}/", item.name), o)
                .map_err(zerr)?;
            report.dirs += 1;
        }
        NodeKind::File => {
            ctx.progress.set_current(item.name.clone());
            zw.start_file(item.name.as_str(), file_options(item, opts)?)
                .map_err(zerr)?;
            match pump(&item.path, zw, ctx, buf) {
                Ok(n) => report.bytes_in += n,
                Err(e) => {
                    // Keep the archive consistent; the entry is dropped and reported.
                    zw.abort_file().map_err(zerr)?;
                    if matches!(e, CoreError::Cancelled) {
                        return Err(e);
                    }
                    report.skipped.push(OpIssue::new(&item.path, e.to_string()));
                    return Ok(());
                }
            }
            report.files += 1;
        }
    }
    Ok(())
}

/// Compress a batch of small files on `threads` workers, then append them in order.
fn flush_batch(
    zw: &mut Output<'_>,
    batch: Vec<SourceItem>,
    opts: &CreateOptions,
    threads: usize,
    ctx: &JobCtx<'_>,
    report: &mut CreateReport,
    out: &Path,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<Result<PreparedZipFile>>>> =
        batch.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..threads.min(batch.len()) {
            s.spawn(|| {
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = batch.get(i) else { break };
                    let result = prepare(item, opts, ctx, &mut buf);
                    let stop = matches!(result, Err(CoreError::Cancelled));
                    *slots[i].lock() = Some(result);
                    if stop {
                        break;
                    }
                }
            });
        }
    });
    for (item, slot) in batch.iter().zip(slots) {
        ctx.progress.add_items(1);
        match slot.into_inner() {
            Some(Ok(prepared)) => {
                zw.add_prepared_file(prepared)
                    .map_err(|e| zip_err(out, e))?;
                report.files += 1;
                report.bytes_in += item.size;
            }
            Some(Err(CoreError::Cancelled)) | None => return Err(CoreError::Cancelled),
            Some(Err(e)) => report.skipped.push(OpIssue::new(&item.path, e.to_string())),
        }
    }
    Ok(())
}

fn prepare(
    item: &SourceItem,
    opts: &CreateOptions,
    ctx: &JobCtx<'_>,
    buf: &mut [u8],
) -> Result<PreparedZipFile> {
    let o = file_options(item, opts)?;
    let mut builder =
        ZipFileBuilder::new(&item.name, o).map_err(|e| CoreError::Invalid(e.to_string()))?;
    pump(&item.path, &mut builder, ctx, buf)?;
    builder
        .finish()
        .map_err(|e| CoreError::Invalid(format!("{}: {e}", item.path.display())))
}
