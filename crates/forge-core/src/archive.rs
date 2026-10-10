//! ZIP browsing, safe extraction and creation.
//!
//! Extraction treats every archive as hostile:
//! * every entry name is validated with [`paths::safe_join`] *before anything is written*
//!   (absolute paths, `..`, drive/UNC prefixes, `:` streams, reserved names → the whole
//!   archive is rejected);
//! * symlink entries and encrypted entries are skipped (reported), never created;
//! * entry count, declared total size, per-entry compression ratio and overlapping entries
//!   ("quoted overlap" bombs) are checked up front, and the *actual* decompressed bytes are
//!   capped while streaming, so lying headers cannot exceed the limits;
//! * parent folders are created component by component and an existing link/junction
//!   inside the destination aborts the entry (no writing through planted links);
//! * files are written via temp file + rename like every other copy.
//!
//! 7z/RAR/TAR are not supported (documented limitation).

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{Datelike, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::error::{CoreError, Result};
use crate::ops::{
    self, ConflictPolicy, JobCtx, NodeKind, OpIssue, StreamOptions, TempGuard, WalkItem, node_kind,
    walk_tree,
};
use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveEntry {
    /// Name as stored (`/`-separated; directories end with `/`).
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub encrypted: bool,
    pub size: u64,
    pub compressed_size: u64,
    pub modified: Option<SystemTime>,
    /// Whether the name passes extraction safety checks.
    pub safe_name: bool,
}

fn zip_err(path: &Path, e: zip::result::ZipError) -> CoreError {
    match e {
        zip::result::ZipError::Io(io) => CoreError::io(path, io),
        other => CoreError::Invalid(format!("{}: {other}", path.display())),
    }
}

fn open_zip(path: &Path) -> Result<ZipArchive<io::BufReader<File>>> {
    let f = File::open(path).map_err(|e| CoreError::io(path, e))?;
    ZipArchive::new(io::BufReader::new(f)).map_err(|e| zip_err(path, e))
}

fn entry_name(raw: &[u8], decoded: Option<String>) -> String {
    decoded.unwrap_or_else(|| String::from_utf8_lossy(raw).into_owned())
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

/// List the entries of a ZIP file without extracting anything.
pub fn list_zip(path: &Path) -> Result<Vec<ArchiveEntry>> {
    let mut za = open_zip(path)?;
    let mut out = Vec::with_capacity(za.len());
    for i in 0..za.len() {
        let f = za.by_index_raw(i).map_err(|e| zip_err(path, e))?;
        let name = entry_name(f.name_raw(), f.name().ok().map(|c| c.into_owned()));
        let trimmed = name.trim_end_matches('/');
        out.push(ArchiveEntry {
            safe_name: paths::validate_relative(trimmed).is_ok(),
            is_dir: f.is_dir(),
            is_symlink: f.is_symlink(),
            encrypted: f.encrypted(),
            size: f.size(),
            compressed_size: f.compressed_size(),
            modified: f.last_modified().and_then(zip_time_to_system),
            name,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractLimits {
    pub max_entries: usize,
    /// Maximum total uncompressed bytes.
    pub max_total_bytes: u64,
    /// Maximum uncompressed/compressed ratio for entries larger than 1 MiB.
    pub max_ratio: u64,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_total_bytes: 64 * 1024 * 1024 * 1024,
            max_ratio: 1_000,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractOptions {
    pub limits: ExtractLimits,
    /// Conflict handling for files that already exist (`Ask` is treated as `Skip`;
    /// interactive questions belong in the UI before extraction).
    pub policy: ConflictPolicy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractReport {
    pub extracted: Vec<PathBuf>,
    pub skipped: Vec<OpIssue>,
    pub failed: Vec<OpIssue>,
    pub bytes: u64,
    pub cancelled: bool,
}

struct Planned {
    index: usize,
    name: String,
    rel: PathBuf,
    is_dir: bool,
    size: u64,
}

/// Extract `archive` into the existing folder `dest_dir` safely (see module docs).
/// Returns `Err` without writing anything if the archive is unsafe or over the limits.
pub fn extract_zip(
    archive: &Path,
    dest_dir: &Path,
    opts: &ExtractOptions,
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
    let mut za = open_zip(archive)?;
    let limits = &opts.limits;
    if za.len() > limits.max_entries {
        return Err(CoreError::Invalid(format!(
            "archive has {} entries (limit {})",
            za.len(),
            limits.max_entries
        )));
    }
    if za
        .has_overlapping_files()
        .map_err(|e| zip_err(archive, e))?
    {
        return Err(CoreError::UnsafePath(
            "archive has overlapping entries (zip bomb)".into(),
        ));
    }

    // Pass 1: validate everything before touching the disk.
    let mut report = ExtractReport::default();
    let mut planned = Vec::new();
    let mut total: u64 = 0;
    for i in 0..za.len() {
        let f = za.by_index_raw(i).map_err(|e| zip_err(archive, e))?;
        let name = entry_name(f.name_raw(), f.name().ok().map(|c| c.into_owned()));
        let trimmed = name.trim_end_matches(['/', '\\']);
        if trimmed.is_empty() && f.is_dir() {
            continue; // "/" root entry
        }
        let rel = paths::validate_relative(trimmed)?;
        if f.is_symlink() {
            report.skipped.push(OpIssue::new(
                &rel,
                "symbolic link entries are not extracted",
            ));
            continue;
        }
        if f.encrypted() {
            report
                .skipped
                .push(OpIssue::new(&rel, "encrypted entries are not supported"));
            continue;
        }
        let size = f.size();
        if !f.is_dir() && size > 1024 * 1024 && size / f.compressed_size().max(1) > limits.max_ratio
        {
            return Err(CoreError::UnsafePath(format!(
                "{name}: suspicious compression ratio (zip bomb)"
            )));
        }
        total = total.saturating_add(size);
        if total > limits.max_total_bytes {
            return Err(CoreError::Invalid(format!(
                "archive expands to more than {} bytes",
                limits.max_total_bytes
            )));
        }
        planned.push(Planned {
            index: i,
            name,
            rel,
            is_dir: f.is_dir(),
            size,
        });
    }
    ctx.progress.add_total_items(planned.len() as u64);
    ctx.progress.add_total_bytes(total);

    // Pass 2: extract.
    let mut seen: HashSet<String> = HashSet::new();
    let mut remaining = limits.max_total_bytes;
    for p in planned {
        if ctx.cancel.check().is_err() {
            report.cancelled = true;
            break;
        }
        ctx.progress.add_items(1);
        let key = paths::to_slash(&p.rel).to_lowercase();
        if !p.is_dir && !seen.insert(key) {
            report
                .skipped
                .push(OpIssue::new(&p.rel, "duplicate name inside the archive"));
            continue;
        }
        let target = dest_dir.join(&p.rel);
        let parent_rel = if p.is_dir {
            p.rel.as_path()
        } else {
            p.rel.parent().unwrap_or(Path::new(""))
        };
        if let Err(e) = ensure_dirs_no_links(dest_dir, parent_rel) {
            report.failed.push(OpIssue::new(&target, e.to_string()));
            continue;
        }
        if p.is_dir {
            continue;
        }
        let (dest, overwrite) = match fs::symlink_metadata(&target) {
            Err(_) => (target, false),
            Ok(md) if node_kind(&md) != NodeKind::File => {
                report.failed.push(OpIssue::new(
                    &target,
                    "a folder or link with this name exists",
                ));
                continue;
            }
            Ok(_) => match opts.policy {
                ConflictPolicy::Overwrite => (target, true),
                ConflictPolicy::KeepBoth => {
                    let dir = target.parent().unwrap_or(dest_dir).to_path_buf();
                    (paths::unique_name(&dir, &paths::name_of(&target)), false)
                }
                _ => {
                    report.skipped.push(OpIssue::new(&target, "already exists"));
                    ctx.progress.add_bytes(p.size);
                    continue;
                }
            },
        };
        let mut f = match za.by_index(p.index) {
            Ok(f) => f,
            Err(e) => {
                report
                    .failed
                    .push(OpIssue::new(&dest, zip_err(archive, e).to_string()));
                continue;
            }
        };
        let modified = f.last_modified().and_then(zip_time_to_system);
        let sopts = StreamOptions {
            overwrite,
            modified,
            // Never trust the declared size beyond the global budget.
            limit: Some(p.size.min(remaining)),
            ..Default::default()
        };
        let label = PathBuf::from(&p.name);
        match ops::write_stream_atomic(&mut f, &label, &dest, sopts, ctx) {
            Ok(out) => {
                remaining = remaining.saturating_sub(out.bytes);
                report.bytes += out.bytes;
                report.extracted.push(dest);
            }
            Err(CoreError::Cancelled) => {
                report.cancelled = true;
                break;
            }
            Err(e) => report.failed.push(OpIssue::new(&dest, e.to_string())),
        }
    }
    Ok(report)
}

/// Create `base/rel` one component at a time, refusing to pass through links or files.
fn ensure_dirs_no_links(base: &Path, rel: &Path) -> Result<()> {
    let mut cur = base.to_path_buf();
    for c in rel.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(md) => match node_kind(&md) {
                NodeKind::Dir => {}
                NodeKind::Link => {
                    return Err(CoreError::UnsafePath(format!(
                        "{} is a link; refusing to extract through it",
                        cur.display()
                    )));
                }
                NodeKind::File => {
                    return Err(CoreError::Invalid(format!(
                        "{} exists and is not a folder",
                        cur.display()
                    )));
                }
            },
            Err(_) => match fs::create_dir(&cur) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(CoreError::io(&cur, e)),
            },
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Creation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZipReport {
    pub files: u64,
    pub dirs: u64,
    pub bytes_in: u64,
    pub skipped: Vec<OpIssue>,
}

/// Formats that are already compressed: stored instead of deflated (faster, same size).
const STORE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "jxl", "mp4", "m4v", "mov", "mkv",
    "webm", "mp3", "aac", "m4a", "ogg", "opus", "flac", "zip", "7z", "rar", "gz", "tgz", "bz2",
    "xz", "zst", "cr3", "docx", "xlsx", "pptx",
];

/// Create a new ZIP at `out` from files and folders. Folder entries are stored relative to
/// each source's parent (`photos/…`). Links are skipped (never followed). Fails if `out`
/// exists; the archive is written to a temp file and renamed into place when complete.
pub fn create_zip(sources: &[PathBuf], out: &Path, ctx: &JobCtx<'_>) -> Result<ZipReport> {
    if paths::exists_no_follow(out) {
        return Err(CoreError::AlreadyExists(out.to_path_buf()));
    }
    let dir = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = dir.join(ops::partial_name());
    let file = File::create_new(&tmp).map_err(|e| CoreError::io(out, e))?;
    let mut guard = TempGuard(Some(tmp.clone()));
    let mut zw = ZipWriter::new(io::BufWriter::new(file));
    let mut report = ZipReport::default();
    let mut names: HashSet<String> = HashSet::new();

    // Pre-scan for totals.
    let mut work: Vec<(PathBuf, String, NodeKind, u64, Option<SystemTime>)> = Vec::new();
    for src in sources {
        let md = fs::symlink_metadata(src).map_err(|e| CoreError::io(src, e))?;
        let name = paths::name_of(src);
        if name.is_empty() {
            return Err(CoreError::Invalid(format!(
                "cannot archive {}",
                src.display()
            )));
        }
        let kind = node_kind(&md);
        work.push((
            src.clone(),
            name.clone(),
            kind,
            md.len(),
            md.modified().ok(),
        ));
        if kind == NodeKind::Dir {
            walk_tree(src, ctx.cancel, |item| match item {
                WalkItem::Entry(e) => work.push((
                    e.path.clone(),
                    format!("{name}/{}", paths::to_slash(&e.rel)),
                    e.kind,
                    e.size,
                    e.modified,
                )),
                WalkItem::Error { path, error } => {
                    report.skipped.push(OpIssue::new(path, error.to_string()))
                }
            })?;
        }
    }
    let total: u64 = work
        .iter()
        .filter(|w| w.2 == NodeKind::File)
        .map(|w| w.3)
        .sum();
    ctx.progress.add_total_items(work.len() as u64);
    ctx.progress.add_total_bytes(total);

    let mut buf = vec![0u8; ops::COPY_CHUNK];
    for (path, name, kind, size, modified) in work {
        ctx.cancel.check()?;
        ctx.progress.add_items(1);
        if paths::paths_equal(&path, out) || paths::paths_equal(&path, &tmp) {
            continue;
        }
        if !names.insert(name.to_lowercase()) {
            report.skipped.push(OpIssue::new(
                &path,
                "an entry with this name is already in the archive",
            ));
            continue;
        }
        let mtime = modified.map(system_to_zip_time).unwrap_or_default();
        let zerr = |e| zip_err(out, e);
        match kind {
            NodeKind::Link => report
                .skipped
                .push(OpIssue::new(&path, "links are not archived")),
            NodeKind::Dir => {
                let o = SimpleFileOptions::default().last_modified_time(mtime);
                zw.add_directory(format!("{name}/"), o).map_err(zerr)?;
                report.dirs += 1;
            }
            NodeKind::File => {
                let ext = crate::model::ext_of(&path);
                let method = if STORE_EXTS.contains(&ext.as_str()) {
                    CompressionMethod::Stored
                } else {
                    CompressionMethod::Deflated
                };
                let o = SimpleFileOptions::default()
                    .compression_method(method)
                    .last_modified_time(mtime)
                    .large_file(size >= u32::MAX as u64);
                let mut src = match File::open(&path) {
                    Ok(f) => f,
                    Err(e) => {
                        report
                            .skipped
                            .push(OpIssue::new(&path, CoreError::io(&path, e).to_string()));
                        ctx.progress.add_bytes(size);
                        continue;
                    }
                };
                zw.start_file(name.as_str(), o).map_err(zerr)?;
                loop {
                    ctx.cancel.check()?;
                    let n = match src.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(CoreError::io(&path, e)),
                    };
                    zw.write_all(&buf[..n]).map_err(|e| CoreError::io(out, e))?;
                    ctx.progress.add_bytes(n as u64);
                    report.bytes_in += n as u64;
                }
                report.files += 1;
            }
        }
    }
    let w = zw.finish().map_err(|e| zip_err(out, e))?;
    let file = w
        .into_inner()
        .map_err(|e| CoreError::io(out, e.into_error()))?;
    file.sync_all().map_err(|e| CoreError::io(out, e))?;
    drop(file);
    ops::place(&tmp, out, false)?;
    guard.disarm();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_jobs::{CancelToken, Progress};

    fn ctx_parts() -> (CancelToken, Progress) {
        (CancelToken::new(), Progress::default())
    }

    fn raw_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut zw = ZipWriter::new(File::create(path).unwrap());
        for (name, data) in entries {
            zw.start_file(*name, SimpleFileOptions::default()).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }

    #[test]
    fn create_list_extract_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("Проект 日本");
        fs::create_dir_all(src.join("sub/empty")).unwrap();
        fs::write(src.join("a.txt"), b"hello hello hello").unwrap();
        fs::write(src.join("sub/b.jpg"), vec![9u8; 5000]).unwrap();
        let single = d.path().join("single.md");
        fs::write(&single, b"# hi").unwrap();
        let out = d.path().join("out.zip");
        let (c, p) = ctx_parts();
        let ctx = JobCtx::new(&c, &p);
        let rep = create_zip(&[src.clone(), single], &out, &ctx).unwrap();
        assert_eq!(rep.files, 3);
        assert_eq!(rep.dirs, 3);
        assert!(
            create_zip(std::slice::from_ref(&src), &out, &ctx).is_err(),
            "no overwrite"
        );

        let list = list_zip(&out).unwrap();
        assert!(list.iter().all(|e| e.safe_name));
        assert!(
            list.iter()
                .any(|e| e.name == "Проект 日本/sub/b.jpg" && e.size == 5000)
        );

        let x = d.path().join("x");
        fs::create_dir(&x).unwrap();
        let r = extract_zip(&out, &x, &ExtractOptions::default(), &ctx).unwrap();
        assert!(r.failed.is_empty(), "{r:?}");
        assert_eq!(r.extracted.len(), 3);
        assert_eq!(
            fs::read(x.join("Проект 日本/a.txt")).unwrap(),
            b"hello hello hello"
        );
        assert!(x.join("Проект 日本/sub/empty").is_dir());
        // Second extraction: existing files skipped by default, KeepBoth renames.
        let r = extract_zip(&out, &x, &ExtractOptions::default(), &ctx).unwrap();
        assert_eq!(r.skipped.len(), 3);
        let kb = ExtractOptions {
            policy: ConflictPolicy::KeepBoth,
            ..Default::default()
        };
        extract_zip(&out, &x, &kb, &ctx).unwrap();
        assert!(x.join("single (2).md").exists());
    }

    #[test]
    fn zip_slip_is_rejected_before_writing() {
        let d = tempfile::tempdir().unwrap();
        let x = d.path().join("x");
        fs::create_dir(&x).unwrap();
        let (c, p) = ctx_parts();
        let ctx = JobCtx::new(&c, &p);
        for evil in [
            "../evil.txt",
            "ok/../../evil.txt",
            "/abs.txt",
            "C:/Windows/evil.txt",
            "C:evil.txt",
            "\\\\server\\share\\evil.txt",
            "..\\evil.txt",
            "file.txt:ads",
            "CON",
        ] {
            let z = d.path().join("evil.zip");
            raw_zip(&z, &[("good.txt", b"fine"), (evil, b"pwned")]);
            let r = extract_zip(&z, &x, &ExtractOptions::default(), &ctx);
            assert!(r.is_err(), "{evil:?} must be rejected");
            assert!(list_zip(&z).unwrap().iter().any(|e| !e.safe_name));
            assert_eq!(
                fs::read_dir(&x).unwrap().count(),
                0,
                "nothing written for {evil:?}"
            );
            assert!(!d.path().join("evil.txt").exists());
            fs::remove_file(&z).unwrap();
        }
    }

    #[test]
    fn zip_bomb_limits() {
        let d = tempfile::tempdir().unwrap();
        let x = d.path().join("x");
        fs::create_dir(&x).unwrap();
        let (c, p) = ctx_parts();
        let ctx = JobCtx::new(&c, &p);
        let z = d.path().join("bomb.zip");
        let zeros = vec![0u8; 20 * 1024 * 1024];
        raw_zip(&z, &[("zeros.bin", &zeros)]);
        // 20 MiB of zeros deflates ~1000:1.
        let strict = ExtractOptions {
            limits: ExtractLimits {
                max_ratio: 100,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(matches!(
            extract_zip(&z, &x, &strict, &ctx),
            Err(CoreError::UnsafePath(_))
        ));
        let small = ExtractOptions {
            limits: ExtractLimits {
                max_total_bytes: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        raw_zip(&z, &[("a.txt", b"0123456789abc")]);
        assert!(extract_zip(&z, &x, &small, &ctx).is_err());
        let few = ExtractOptions {
            limits: ExtractLimits {
                max_entries: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        raw_zip(&z, &[("a", b"1"), ("b", b"2")]);
        assert!(extract_zip(&z, &x, &few, &ctx).is_err());
        assert_eq!(fs::read_dir(&x).unwrap().count(), 0);
    }

    #[test]
    fn symlink_entries_skipped() {
        let d = tempfile::tempdir().unwrap();
        let z = d.path().join("l.zip");
        let mut zw = ZipWriter::new(File::create(&z).unwrap());
        zw.add_symlink("link", "/etc/passwd", SimpleFileOptions::default())
            .unwrap();
        zw.start_file("real.txt", SimpleFileOptions::default())
            .unwrap();
        zw.write_all(b"x").unwrap();
        zw.finish().unwrap();
        let x = d.path().join("x");
        fs::create_dir(&x).unwrap();
        let (c, p) = ctx_parts();
        let r = extract_zip(&z, &x, &ExtractOptions::default(), &JobCtx::new(&c, &p)).unwrap();
        assert_eq!(r.skipped.len(), 1);
        assert_eq!(r.extracted, vec![x.join("real.txt")]);
        assert!(!paths::exists_no_follow(&x.join("link")));
    }
}
