//! Media ingest: plan → preview → verified copy → manifest → "safe to format?".
//!
//! Guarantees:
//! * The source is never modified (opened read-only; nothing is renamed/deleted there).
//! * Every copied file is verified by re-reading the written destination file from disk and
//!   comparing its BLAKE3 with the hash of the bytes read from the source. Verification
//!   happens on the temp file before it is renamed into place, so an unverified file never
//!   appears under its final name.
//! * Re-running an ingest is idempotent: destination files with identical content are
//!   recognised (size + BLAKE3) as "already imported", including ones that were renamed
//!   `name (2).ext` because of a name conflict on an earlier run.
//! * Name conflicts with *different* content never overwrite; the new file gets
//!   `name (2).ext` (KeepBoth).
//!
//! Template tokens (folder and file templates):
//! `{YYYY} {YY} {MM} {DD} {hh} {mm} {ss} {date}`(YYYY-MM-DD) `{time}`(hhmmss)
//! `{camera} {project} {client} {shoot} {orig_name}`(stem) `{ext}` `{seq}` `{seq:N}`
//! (zero-padded to N digits). `{{`/`}}` are literal braces. `/` in the folder template
//! creates sub-folders. If the file template has no `{ext}`, the original extension is
//! appended. `{seq}` numbers files in capture-time order within one plan.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{Datelike, NaiveDateTime, Timelike};
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::hashing::{self, Digest, ManifestEntry};
use crate::ops::{
    self, CopyFileOptions, JobCtx, NodeKind, OpIssue, WalkItem, node_kind, walk_tree,
};
use crate::paths;

/// Capture metadata for one file, provided by the media layer (EXIF/QuickTime parsing
/// lives in `forge-media`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureInfo {
    pub captured: Option<NaiveDateTime>,
    pub camera: Option<String>,
}

pub trait IngestMetadataProvider {
    fn capture_info(&self, path: &Path) -> CaptureInfo;
}

/// Provider that knows nothing: dates fall back to the file's modified time.
pub struct NoMetadata;

impl IngestMetadataProvider for NoMetadata {
    fn capture_info(&self, _: &Path) -> CaptureInfo {
        CaptureInfo::default()
    }
}

impl<F: Fn(&Path) -> CaptureInfo> IngestMetadataProvider for F {
    fn capture_info(&self, path: &Path) -> CaptureInfo {
        self(path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestSpec {
    pub sources: Vec<PathBuf>,
    pub dest_root: PathBuf,
    pub folder_template: String,
    pub file_template: String,
    pub project: String,
    pub client: String,
    pub shoot: String,
    /// Lowercase extensions to import (`None` = everything).
    pub extensions: Option<Vec<String>>,
    /// Write `forge-ingest-<timestamp>.b3` + `.json` into `dest_root`.
    pub write_manifest: bool,
}

impl IngestSpec {
    pub fn new(sources: Vec<PathBuf>, dest_root: PathBuf) -> Self {
        Self {
            sources,
            dest_root,
            folder_template: "{YYYY}/{date}".into(),
            file_template: "{orig_name}".into(),
            project: String::new(),
            client: String::new(),
            shoot: String::new(),
            extensions: None,
            write_manifest: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IngestStatus {
    /// Will be copied to `dest`.
    New,
    /// Will be copied to `dest`, which differs from the template name because of a conflict.
    Renamed,
    /// Identical content already exists at `dest`; nothing to copy.
    AlreadyImported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestItem {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub size: u64,
    pub captured: NaiveDateTime,
    pub camera: Option<String>,
    pub status: IngestStatus,
    /// Known when computed during planning (for already-imported detection).
    pub source_hash: Option<Digest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestPlan {
    pub spec: IngestSpec,
    pub items: Vec<IngestItem>,
    pub bytes_to_copy: u64,
    pub errors: Vec<OpIssue>,
}

impl IngestPlan {
    pub fn count(&self, s: IngestStatus) -> usize {
        self.items.iter().filter(|i| i.status == s).count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestedFile {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub size: u64,
    pub hash: Digest,
    /// Source modification time captured when its contents were verified, in Unix ns.
    #[serde(default)]
    pub source_mtime_ns: Option<u64>,
    /// Destination modification time captured when its contents were verified, in Unix ns.
    #[serde(default)]
    pub dest_mtime_ns: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestReport {
    pub verified: Vec<IngestedFile>,
    pub already_present: Vec<IngestedFile>,
    pub failed: Vec<OpIssue>,
    /// Sources that disappeared between planning and execution.
    pub missing: Vec<PathBuf>,
    pub manifest: Option<PathBuf>,
    pub bytes: u64,
    pub cancelled: bool,
}

impl IngestReport {
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty() && self.missing.is_empty() && !self.cancelled
    }
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

struct TokenValues<'a> {
    t: NaiveDateTime,
    camera: &'a str,
    spec: &'a IngestSpec,
    orig_name: &'a str,
    ext: &'a str,
    seq: u64,
}

enum Piece {
    Lit(String),
    Tok(String, Option<usize>),
}

fn parse_template(t: &str) -> Result<Vec<Piece>> {
    let bad = |m: &str| CoreError::Invalid(format!("template {t:?}: {m}"));
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                lit.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                lit.push('}');
            }
            '}' => return Err(bad("unmatched '}'")),
            '{' => {
                let mut tok = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => tok.push(c),
                        None => return Err(bad("unclosed '{'")),
                    }
                }
                let (name, width) = match tok.split_once(':') {
                    Some((n, w)) => {
                        let w: usize = w.parse().map_err(|_| bad("bad width"))?;
                        if n != "seq" || w > 12 {
                            return Err(bad(&format!("invalid token {{{tok}}}")));
                        }
                        (n.to_string(), Some(w))
                    }
                    None => (tok, None),
                };
                const KNOWN: &[&str] = &[
                    "YYYY",
                    "YY",
                    "MM",
                    "DD",
                    "hh",
                    "mm",
                    "ss",
                    "date",
                    "time",
                    "camera",
                    "project",
                    "client",
                    "shoot",
                    "orig_name",
                    "ext",
                    "seq",
                ];
                if !KNOWN.contains(&name.as_str()) {
                    return Err(bad(&format!("unknown token {{{name}}}")));
                }
                if !lit.is_empty() {
                    out.push(Piece::Lit(std::mem::take(&mut lit)));
                }
                out.push(Piece::Tok(name, width));
            }
            c => lit.push(c),
        }
    }
    if !lit.is_empty() {
        out.push(Piece::Lit(lit));
    }
    Ok(out)
}

/// Check a template for syntax errors and unknown tokens.
pub fn validate_template(t: &str) -> Result<()> {
    parse_template(t).map(|_| ())
}

fn render(pieces: &[Piece], v: &TokenValues<'_>) -> String {
    let t = v.t;
    let mut s = String::new();
    for p in pieces {
        match p {
            Piece::Lit(l) => s.push_str(l),
            Piece::Tok(name, width) => {
                let val = match name.as_str() {
                    "YYYY" => format!("{:04}", t.year()),
                    "YY" => format!("{:02}", t.year().rem_euclid(100)),
                    "MM" => format!("{:02}", t.month()),
                    "DD" => format!("{:02}", t.day()),
                    "hh" => format!("{:02}", t.hour()),
                    "mm" => format!("{:02}", t.minute()),
                    "ss" => format!("{:02}", t.second()),
                    "date" => t.format("%Y-%m-%d").to_string(),
                    "time" => t.format("%H%M%S").to_string(),
                    "camera" => v.camera.to_string(),
                    "project" => v.spec.project.clone(),
                    "client" => v.spec.client.clone(),
                    "shoot" => v.spec.shoot.clone(),
                    "orig_name" => v.orig_name.to_string(),
                    "ext" => v.ext.to_string(),
                    "seq" => format!("{:0w$}", v.seq, w = width.unwrap_or(1)),
                    _ => String::new(),
                };
                s.push_str(&val);
            }
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// Card/OS clutter that is never imported nor required for "safe to format".
fn is_system_junk(rel: &Path) -> bool {
    rel.components().any(|c| {
        let n = c.as_os_str().to_string_lossy();
        n.starts_with('.')
            || n.eq_ignore_ascii_case("System Volume Information")
            || n.eq_ignore_ascii_case("$RECYCLE.BIN")
            || n.eq_ignore_ascii_case("Thumbs.db")
            || n.eq_ignore_ascii_case("desktop.ini")
    })
}

/// All regular files under the source roots (roots may be files). Links are skipped.
fn source_files(
    sources: &[PathBuf],
    ctx: &JobCtx<'_>,
    errors: &mut Vec<OpIssue>,
) -> Result<Vec<(PathBuf, u64, Option<SystemTime>)>> {
    let mut out = Vec::new();
    for root in sources {
        let md = match fs::symlink_metadata(root) {
            Ok(m) => m,
            Err(e) => {
                errors.push(OpIssue::new(root, CoreError::io(root, e).to_string()));
                continue;
            }
        };
        match node_kind(&md) {
            NodeKind::File => out.push((root.clone(), md.len(), md.modified().ok())),
            NodeKind::Link => errors.push(OpIssue::new(root, "links are not imported")),
            NodeKind::Dir => walk_tree(root, ctx.cancel, |item| match item {
                WalkItem::Entry(e) if e.is_file() && !is_system_junk(&e.rel) => {
                    out.push((e.path.clone(), e.size, e.modified))
                }
                WalkItem::Entry(_) => {}
                WalkItem::Error { path, error } => {
                    errors.push(OpIssue::new(path, error.to_string()))
                }
            })?,
        }
    }
    Ok(out)
}

fn key_of(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

fn modified_ns(md: &fs::Metadata) -> Option<u64> {
    md.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos()
        .try_into()
        .ok()
}

fn same_file_stamp(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.len() == b.len() && modified_ns(a) == modified_ns(b)
}

/// Build a previewable plan. Reads source metadata and hashes only where a same-size file
/// already exists at a candidate destination. Writes nothing.
pub fn plan_ingest(
    spec: &IngestSpec,
    meta: &dyn IngestMetadataProvider,
    ctx: &JobCtx<'_>,
) -> Result<IngestPlan> {
    let folder_t: Vec<Vec<Piece>> = spec
        .folder_template
        .split(['/', '\\'])
        .map(parse_template)
        .collect::<Result<_>>()?;
    let file_t = parse_template(&spec.file_template)?;
    let has_ext_token = file_t
        .iter()
        .any(|p| matches!(p, Piece::Tok(n, _) if n == "ext"));
    if spec.sources.is_empty() {
        return Err(CoreError::Invalid("no sources selected".into()));
    }
    for s in &spec.sources {
        if paths::is_within(s, &spec.dest_root) || paths::is_within(&spec.dest_root, s) {
            return Err(CoreError::Invalid(format!(
                "destination {} overlaps source {}",
                spec.dest_root.display(),
                s.display()
            )));
        }
    }
    let exts: Option<HashSet<String>> = spec.extensions.as_ref().map(|v| {
        v.iter()
            .map(|e| e.trim_start_matches('.').to_lowercase())
            .collect()
    });

    let mut errors = Vec::new();
    ctx.progress.set_current("scanning sources");
    let mut files = source_files(&spec.sources, ctx, &mut errors)?;
    if let Some(exts) = &exts {
        files.retain(|(p, _, _)| exts.contains(&crate::model::ext_of(p)));
    }
    // Capture info + chronological order (stable for {seq}).
    let mut staged: Vec<(PathBuf, u64, NaiveDateTime, Option<String>)> = files
        .into_iter()
        .map(|(p, size, mtime)| {
            let info = meta.capture_info(&p);
            let t = info.captured.unwrap_or_else(|| {
                let st = mtime.unwrap_or(SystemTime::UNIX_EPOCH);
                chrono::DateTime::<chrono::Local>::from(st).naive_local()
            });
            (p, size, t, info.camera)
        })
        .collect();
    staged.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));

    let mut reserved: HashSet<String> = HashSet::new();
    let mut items = Vec::with_capacity(staged.len());
    let mut bytes_to_copy = 0;
    for (i, (src, size, t, camera)) in staged.into_iter().enumerate() {
        ctx.cancel.check()?;
        let name = paths::name_of(&src);
        let (stem, dot_ext) = paths::split_name_ext(&name);
        let ext = dot_ext.trim_start_matches('.');
        let cam = camera.as_deref().unwrap_or("Unknown");
        let v = TokenValues {
            t,
            camera: cam,
            spec,
            orig_name: stem,
            ext,
            seq: i as u64 + 1,
        };
        let mut dir = spec.dest_root.clone();
        for comp in &folder_t {
            let r = render(comp, &v);
            if r.trim().is_empty() {
                continue;
            }
            dir.push(paths::sanitize_file_name(&r));
        }
        let mut fname = render(&file_t, &v);
        if !has_ext_token && !ext.is_empty() {
            fname = format!("{fname}.{ext}");
        }
        let fname = paths::sanitize_file_name(&fname);
        let dest = dir.join(&fname);

        let mut source_hash = None;
        let (stem2, ext2) = paths::split_name_ext(&fname);
        let mut chosen = None;
        for n in 1u64.. {
            let cand = if n == 1 {
                dest.clone()
            } else {
                dir.join(format!("{stem2} ({n}){ext2}"))
            };
            let k = key_of(&cand);
            if reserved.contains(&k) {
                continue;
            }
            match fs::symlink_metadata(&cand) {
                Err(_) => {
                    let status = if n == 1 {
                        IngestStatus::New
                    } else {
                        IngestStatus::Renamed
                    };
                    chosen = Some((cand, status));
                }
                Ok(md) if node_kind(&md) == NodeKind::File && md.len() == size => {
                    if source_hash.is_none() {
                        match hashing::hash_file(&src, ctx.cancel, Some(ctx.progress)) {
                            Ok(h) => source_hash = Some(h),
                            Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                            Err(e) => {
                                errors.push(OpIssue::new(&src, e.to_string()));
                                break;
                            }
                        }
                    }
                    match hashing::hash_file(&cand, ctx.cancel, Some(ctx.progress)) {
                        Ok(h) if Some(h) == source_hash => {
                            chosen = Some((cand, IngestStatus::AlreadyImported));
                        }
                        Ok(_) => continue,
                        Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                        Err(_) => continue,
                    }
                }
                Ok(_) => continue,
            }
            if let Some((c, _)) = &chosen {
                reserved.insert(key_of(c));
            }
            break;
        }
        let Some((dest, status)) = chosen else {
            continue; // unreadable source, already recorded
        };
        if status != IngestStatus::AlreadyImported {
            bytes_to_copy += size;
        }
        items.push(IngestItem {
            source: src,
            dest,
            size,
            captured: t,
            camera,
            status,
            source_hash,
        });
    }
    Ok(IngestPlan {
        spec: spec.clone(),
        items,
        bytes_to_copy,
        errors,
    })
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Copy and verify every planned item. Failures are recorded and the ingest continues;
/// cancellation stops it (partial files are cleaned up). Writes the manifest if requested
/// and at least one file is accounted for.
pub fn execute_ingest(plan: &IngestPlan, ctx: &JobCtx<'_>) -> Result<IngestReport> {
    let mut report = IngestReport::default();
    for e in &plan.errors {
        report.failed.push(e.clone());
    }
    ctx.progress.add_total_items(plan.items.len() as u64);
    ctx.progress.add_total_bytes(plan.bytes_to_copy);
    let mut made_dirs: HashSet<PathBuf> = HashSet::new();
    for item in &plan.items {
        if ctx.cancel.check().is_err() {
            report.cancelled = true;
            break;
        }
        ctx.progress.add_items(1);
        ctx.progress.set_current(item.source.display().to_string());
        let src_md = match fs::symlink_metadata(&item.source) {
            Ok(m) => m,
            Err(_) => {
                report.missing.push(item.source.clone());
                ctx.progress.add_error();
                ctx.log(format!("missing: {}", item.source.display()));
                if item.status != IngestStatus::AlreadyImported {
                    ctx.progress.add_bytes(item.size);
                }
                continue;
            }
        };
        if node_kind(&src_md) != NodeKind::File {
            report.failed.push(OpIssue::new(
                &item.source,
                "source is no longer a regular file",
            ));
            ctx.progress.add_error();
            continue;
        }
        if item.status == IngestStatus::AlreadyImported {
            let verified = (|| -> Result<IngestedFile> {
                let dest_before =
                    fs::symlink_metadata(&item.dest).map_err(|e| CoreError::io(&item.dest, e))?;
                if node_kind(&dest_before) != NodeKind::File
                    || src_md.len() != item.size
                    || dest_before.len() != item.size
                {
                    return Err(CoreError::Invalid(
                        "source or destination changed since planning".into(),
                    ));
                }

                // Planning only identified the candidate. Re-read both current files before
                // accounting for an already-imported item.
                let source_hash = hashing::hash_file(&item.source, ctx.cancel, Some(ctx.progress))?;
                let dest_hash = hashing::hash_file(&item.dest, ctx.cancel, Some(ctx.progress))?;
                if source_hash != dest_hash {
                    return Err(CoreError::VerifyFailed {
                        path: item.dest.clone(),
                        detail: "source and existing destination have different BLAKE3 hashes"
                            .into(),
                    });
                }

                let source_after = fs::symlink_metadata(&item.source)
                    .map_err(|e| CoreError::io(&item.source, e))?;
                let dest_after =
                    fs::symlink_metadata(&item.dest).map_err(|e| CoreError::io(&item.dest, e))?;
                if node_kind(&source_after) != NodeKind::File
                    || node_kind(&dest_after) != NodeKind::File
                    || !same_file_stamp(&src_md, &source_after)
                    || !same_file_stamp(&dest_before, &dest_after)
                {
                    return Err(CoreError::Invalid(
                        "source or destination changed while verifying".into(),
                    ));
                }

                Ok(IngestedFile {
                    source: item.source.clone(),
                    dest: item.dest.clone(),
                    size: item.size,
                    hash: source_hash,
                    source_mtime_ns: modified_ns(&source_after),
                    dest_mtime_ns: modified_ns(&dest_after),
                })
            })();
            match verified {
                Ok(file) => report.already_present.push(file),
                Err(CoreError::Cancelled) => {
                    report.cancelled = true;
                    break;
                }
                Err(e) => {
                    report
                        .failed
                        .push(OpIssue::new(&item.source, e.to_string()));
                    ctx.progress.add_error();
                }
            }
            continue;
        }
        if let Some(parent) = item.dest.parent()
            && made_dirs.insert(parent.to_path_buf())
            && let Err(e) = fs::create_dir_all(parent)
        {
            report.failed.push(OpIssue::new(
                &item.source,
                CoreError::io(parent, e).to_string(),
            ));
            ctx.progress.add_error();
            continue;
        }
        let opts = CopyFileOptions {
            overwrite: false,
            verify: true,
            want_hash: true,
        };
        match ops::copy_file_atomic(&item.source, &item.dest, opts, ctx) {
            Ok(out) => {
                let Some(hash) = out.source_hash else {
                    report.failed.push(OpIssue::new(
                        &item.source,
                        "verified copy did not return a content hash",
                    ));
                    ctx.progress.add_error();
                    continue;
                };
                let source_after = match fs::symlink_metadata(&item.source) {
                    Ok(md) if node_kind(&md) == NodeKind::File => md,
                    Ok(_) => {
                        report.failed.push(OpIssue::new(
                            &item.source,
                            "source changed type while it was copied",
                        ));
                        ctx.progress.add_error();
                        continue;
                    }
                    Err(e) => {
                        report.failed.push(OpIssue::new(
                            &item.source,
                            CoreError::io(&item.source, e).to_string(),
                        ));
                        ctx.progress.add_error();
                        continue;
                    }
                };
                let dest_after = match fs::symlink_metadata(&item.dest) {
                    Ok(md) if node_kind(&md) == NodeKind::File => md,
                    Ok(_) => {
                        report.failed.push(OpIssue::new(
                            &item.source,
                            "destination changed type after the copy",
                        ));
                        ctx.progress.add_error();
                        continue;
                    }
                    Err(e) => {
                        report.failed.push(OpIssue::new(
                            &item.source,
                            CoreError::io(&item.dest, e).to_string(),
                        ));
                        ctx.progress.add_error();
                        continue;
                    }
                };
                if !same_file_stamp(&src_md, &source_after)
                    || source_after.len() != out.bytes
                    || dest_after.len() != out.bytes
                {
                    report.failed.push(OpIssue::new(
                        &item.source,
                        "source or destination changed while it was copied",
                    ));
                    ctx.progress.add_error();
                    continue;
                }
                report.bytes += out.bytes;
                report.verified.push(IngestedFile {
                    source: item.source.clone(),
                    dest: item.dest.clone(),
                    size: out.bytes,
                    hash,
                    source_mtime_ns: modified_ns(&source_after),
                    dest_mtime_ns: modified_ns(&dest_after),
                });
            }
            Err(CoreError::Cancelled) => {
                report.cancelled = true;
                break;
            }
            Err(e) => {
                ctx.progress.add_error();
                let missing = matches!(&e, CoreError::NotFound(p) if p == &item.source);
                if missing {
                    report.missing.push(item.source.clone());
                } else {
                    ctx.log(format!("failed: {}: {e}", item.source.display()));
                    report
                        .failed
                        .push(OpIssue::new(&item.source, e.to_string()));
                }
            }
        }
    }
    if plan.spec.write_manifest
        && !(report.verified.is_empty() && report.already_present.is_empty())
    {
        match write_ingest_manifest(&plan.spec.dest_root, &report) {
            Ok(p) => report.manifest = Some(p),
            Err(e) => report.failed.push(OpIssue::new(
                &plan.spec.dest_root,
                format!("manifest not written: {e}"),
            )),
        }
    }
    Ok(report)
}

fn write_ingest_manifest(dest_root: &Path, report: &IngestReport) -> Result<PathBuf> {
    let mut entries: Vec<ManifestEntry> = report
        .verified
        .iter()
        .chain(&report.already_present)
        .filter_map(|f| {
            let rel = f.dest.strip_prefix(dest_root).ok()?;
            Some(ManifestEntry {
                hash: f.hash,
                rel_path: paths::to_slash(rel),
            })
        })
        .collect();
    entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let b3 = paths::unique_name(dest_root, &format!("forge-ingest-{stamp}.b3"));
    hashing::write_manifest(&b3, &entries)?;
    let json = b3.with_extension("json");
    if !paths::exists_no_follow(&json) {
        let text =
            serde_json::to_string_pretty(report).map_err(|e| CoreError::Invalid(e.to_string()))?;
        hashing::write_atomic(&json, text.as_bytes())?;
    }
    Ok(b3)
}

// ---------------------------------------------------------------------------
// Safe to format?
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FormatCheck {
    pub safe: bool,
    pub checked: u64,
    /// Source files without a verified, still-present destination (with the reason).
    pub unverified: Vec<OpIssue>,
}

/// Is every file under `source_roots` (system clutter excluded) backed by a verified
/// destination from one of `reports`, and are both files still unchanged? Every check
/// re-reads and hashes the source and destination, even when `deep` is false. The boolean
/// remains for API compatibility; a size-only result is never reported as safe.
pub fn safe_to_format(
    source_roots: &[PathBuf],
    reports: &[&IngestReport],
    _deep: bool,
    ctx: &JobCtx<'_>,
) -> Result<FormatCheck> {
    let mut known: HashMap<String, &IngestedFile> = HashMap::new();
    for r in reports {
        for f in r.verified.iter().chain(&r.already_present) {
            known.insert(key_of(&f.source), f);
        }
    }
    let mut errors = Vec::new();
    let files = source_files(source_roots, ctx, &mut errors)?;
    let mut check = FormatCheck {
        checked: files.len() as u64,
        unverified: errors,
        ..Default::default()
    };
    for (src, size, _) in files {
        ctx.cancel.check()?;
        let Some(f) = known.get(&key_of(&src)) else {
            check.unverified.push(OpIssue::new(&src, "not imported"));
            continue;
        };
        let source_md = match fs::symlink_metadata(&src) {
            Ok(md) if node_kind(&md) == NodeKind::File => md,
            Ok(_) => {
                check
                    .unverified
                    .push(OpIssue::new(&src, "source is no longer a regular file"));
                continue;
            }
            Err(e) => {
                check
                    .unverified
                    .push(OpIssue::new(&src, CoreError::io(&src, e).to_string()));
                continue;
            }
        };
        if f.size != size || source_md.len() != f.size {
            check
                .unverified
                .push(OpIssue::new(&src, "source changed since it was imported"));
            continue;
        }
        let source_mtime = modified_ns(&source_md);
        if f.source_mtime_ns.is_none() || source_mtime != f.source_mtime_ns {
            check.unverified.push(OpIssue::new(
                &src,
                "source modification time changed or was unavailable",
            ));
            continue;
        }
        let dest_md = match fs::symlink_metadata(&f.dest) {
            Ok(md) if node_kind(&md) == NodeKind::File => md,
            Ok(_) => {
                check.unverified.push(OpIssue::new(
                    &src,
                    "destination is no longer a regular file",
                ));
                continue;
            }
            Err(_) => {
                check
                    .unverified
                    .push(OpIssue::new(&src, "destination file is missing"));
                continue;
            }
        };
        if dest_md.len() != f.size {
            check
                .unverified
                .push(OpIssue::new(&src, "destination size differs"));
            continue;
        }
        let dest_mtime = modified_ns(&dest_md);
        if f.dest_mtime_ns.is_none() || dest_mtime != f.dest_mtime_ns {
            check.unverified.push(OpIssue::new(
                &src,
                "destination modification time changed or was unavailable",
            ));
            continue;
        }

        let mut hashes_match = true;
        for (which, p) in [("source", &src), ("destination", &f.dest)] {
            match hashing::hash_file(p, ctx.cancel, Some(ctx.progress)) {
                Ok(h) if h == f.hash => {}
                Ok(_) => {
                    check
                        .unverified
                        .push(OpIssue::new(&src, format!("{which} content changed")));
                    hashes_match = false;
                    break;
                }
                Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                Err(e) => {
                    check.unverified.push(OpIssue::new(&src, e.to_string()));
                    hashes_match = false;
                    break;
                }
            }
        }
        if !hashes_match {
            continue;
        }

        // Detect changes during the two hash reads as well as changes made since the
        // report's verification timestamps were captured.
        let source_after = fs::symlink_metadata(&src).ok();
        let dest_after = fs::symlink_metadata(&f.dest).ok();
        if !source_after.as_ref().is_some_and(|md| {
            node_kind(md) == NodeKind::File
                && same_file_stamp(&source_md, md)
                && modified_ns(md) == f.source_mtime_ns
        }) {
            check
                .unverified
                .push(OpIssue::new(&src, "source changed while it was checked"));
        }
        if !dest_after.as_ref().is_some_and(|md| {
            node_kind(md) == NodeKind::File
                && same_file_stamp(&dest_md, md)
                && modified_ns(md) == f.dest_mtime_ns
        }) {
            check.unverified.push(OpIssue::new(
                &src,
                "destination changed while it was checked",
            ));
        }
    }
    check.safe = check.unverified.is_empty();
    Ok(check)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_jobs::{CancelToken, Progress};
    use std::fs::File;

    fn ctx_parts() -> (CancelToken, Progress) {
        (CancelToken::new(), Progress::default())
    }

    struct Card {
        _d: tempfile::TempDir,
        card: PathBuf,
        dest: PathBuf,
    }

    fn card() -> Card {
        let d = tempfile::tempdir().unwrap();
        let card = d.path().join("CARD");
        let dest = d.path().join("Library");
        fs::create_dir_all(card.join("DCIM/100CANON")).unwrap();
        fs::create_dir_all(card.join("DCIM/101CANON")).unwrap();
        fs::create_dir_all(card.join(".Trashes")).unwrap();
        fs::write(card.join("DCIM/100CANON/IMG_0001.CR3"), b"raw-one").unwrap();
        fs::write(card.join("DCIM/100CANON/IMG_0002.CR3"), b"raw-two").unwrap();
        // Same name, different content, different folder -> conflict -> KeepBoth.
        fs::write(card.join("DCIM/101CANON/IMG_0001.CR3"), b"raw-three!").unwrap();
        fs::write(card.join(".Trashes/junk"), b"x").unwrap();
        fs::create_dir_all(&dest).unwrap();
        Card { _d: d, card, dest }
    }

    fn meta(p: &Path) -> CaptureInfo {
        let day = if p.to_string_lossy().contains("101CANON") {
            2
        } else {
            1
        };
        CaptureInfo {
            captured: chrono::NaiveDate::from_ymd_opt(2024, 3, day)
                .unwrap()
                .and_hms_opt(10, 0, 0),
            camera: Some("Canon EOS R5".into()),
        }
    }

    #[test]
    fn templates() {
        assert!(validate_template("{YYYY}/{date}_{shoot}").is_ok());
        assert!(validate_template("{seq:4}_{orig_name}").is_ok());
        assert!(validate_template("{nope}").is_err());
        assert!(validate_template("{seq").is_err());
        assert!(validate_template("{date:3}").is_err());
        assert!(validate_template("{{literal}}").is_ok());
        let spec = IngestSpec {
            shoot: "Wedding".into(),
            ..IngestSpec::new(vec![], PathBuf::new())
        };
        let v = TokenValues {
            t: chrono::NaiveDate::from_ymd_opt(2024, 3, 9)
                .unwrap()
                .and_hms_opt(7, 8, 9)
                .unwrap(),
            camera: "R5",
            spec: &spec,
            orig_name: "IMG_1",
            ext: "CR3",
            seq: 7,
        };
        let p =
            parse_template("{YY}{MM}{DD}-{time}_{shoot}_{camera}_{seq:3}_{orig_name}.{ext}{{x}}")
                .unwrap();
        assert_eq!(render(&p, &v), "240309-070809_Wedding_R5_007_IMG_1.CR3{x}");
    }

    #[test]
    fn ingest_verifies_is_idempotent_and_checks_format_safety() {
        let c = card();
        let (cancel, progress) = ctx_parts();
        let ctx = JobCtx::new(&cancel, &progress);
        let src_before = fs::metadata(c.card.join("DCIM/100CANON/IMG_0001.CR3"))
            .unwrap()
            .modified()
            .unwrap();
        let spec = IngestSpec {
            folder_template: "{YYYY}/{date}_{shoot}/{camera}".into(),
            file_template: "{orig_name}".into(),
            shoot: "Smith Wedding".into(),
            ..IngestSpec::new(vec![c.card.clone()], c.dest.clone())
        };
        let plan = plan_ingest(&spec, &meta, &ctx).unwrap();
        assert_eq!(plan.items.len(), 3, "system junk excluded");
        assert_eq!(plan.count(IngestStatus::New), 3);
        let r1 = execute_ingest(&plan, &ctx).unwrap();
        assert!(r1.is_complete(), "{r1:?}");
        assert_eq!(r1.verified.len(), 3);
        let day1 = c.dest.join("2024/2024-03-01_Smith Wedding/Canon EOS R5");
        let day2 = c.dest.join("2024/2024-03-02_Smith Wedding/Canon EOS R5");
        assert_eq!(fs::read(day1.join("IMG_0001.CR3")).unwrap(), b"raw-one");
        assert_eq!(fs::read(day2.join("IMG_0001.CR3")).unwrap(), b"raw-three!");
        // Source untouched.
        assert_eq!(
            fs::metadata(c.card.join("DCIM/100CANON/IMG_0001.CR3"))
                .unwrap()
                .modified()
                .unwrap(),
            src_before
        );
        // Manifest verifies.
        let manifest = r1.manifest.clone().unwrap();
        let vr = hashing::verify_manifest(&manifest, &cancel, None).unwrap();
        assert!(vr.all_ok());
        assert_eq!(vr.ok.len(), 3);

        // Safe to format.
        let fc = safe_to_format(std::slice::from_ref(&c.card), &[&r1], true, &ctx).unwrap();
        assert!(fc.safe, "{fc:?}");
        assert_eq!(fc.checked, 3);

        // Re-run: everything already imported, nothing copied.
        let plan2 = plan_ingest(&spec, &meta, &ctx).unwrap();
        assert_eq!(plan2.count(IngestStatus::AlreadyImported), 3);
        assert_eq!(plan2.bytes_to_copy, 0);
        let r2 = execute_ingest(&plan2, &ctx).unwrap();
        assert!(r2.verified.is_empty());
        assert_eq!(r2.already_present.len(), 3);

        // A new file on the card makes it unsafe until imported.
        fs::write(c.card.join("DCIM/100CANON/IMG_0003.CR3"), b"new").unwrap();
        let fc = safe_to_format(std::slice::from_ref(&c.card), &[&r1, &r2], false, &ctx).unwrap();
        assert!(!fc.safe);
        assert_eq!(fc.unverified.len(), 1);

        // Corrupt a destination: manifest verify and deep format check catch it.
        fs::write(day1.join("IMG_0002.CR3"), b"raw-tw0").unwrap();
        let vr = hashing::verify_manifest(&manifest, &cancel, None).unwrap();
        assert_eq!(vr.mismatched.len(), 1);
        fs::remove_file(c.card.join("DCIM/100CANON/IMG_0003.CR3")).unwrap();
        let fc = safe_to_format(std::slice::from_ref(&c.card), &[&r1], true, &ctx).unwrap();
        assert!(!fc.safe);
        // And a plan now treats the corrupted file as a conflict (KeepBoth), never overwrite.
        let plan3 = plan_ingest(&spec, &meta, &ctx).unwrap();
        let it = plan3
            .items
            .iter()
            .find(|i| i.source.ends_with("IMG_0002.CR3"))
            .unwrap();
        assert_eq!(it.status, IngestStatus::Renamed);
        assert!(it.dest.ends_with("IMG_0002 (2).CR3"));
    }

    #[test]
    fn already_imported_execution_rejects_same_size_destination_changes() {
        let c = card();
        let source = c.card.join("DCIM/100CANON/IMG_0001.CR3");
        let (cancel, progress) = ctx_parts();
        let ctx = JobCtx::new(&cancel, &progress);
        let spec = IngestSpec {
            folder_template: String::new(),
            write_manifest: false,
            ..IngestSpec::new(vec![source.clone()], c.dest.clone())
        };

        let first = plan_ingest(&spec, &NoMetadata, &ctx).unwrap();
        execute_ingest(&first, &ctx).unwrap();
        let existing = plan_ingest(&spec, &NoMetadata, &ctx).unwrap();
        assert_eq!(existing.items[0].status, IngestStatus::AlreadyImported);

        // Keep the size unchanged so the old size-only execution check accepts it.
        fs::write(&existing.items[0].dest, b"bad-one").unwrap();
        let report = execute_ingest(&existing, &ctx).unwrap();
        assert!(report.already_present.is_empty(), "{report:?}");
        assert_eq!(report.failed.len(), 1, "{report:?}");
    }

    #[test]
    fn shallow_format_check_rehashes_content_changed_after_report() {
        use std::time::{Duration, UNIX_EPOCH};

        let c = card();
        let source = c.card.join("DCIM/100CANON/IMG_0001.CR3");
        let (cancel, progress) = ctx_parts();
        let ctx = JobCtx::new(&cancel, &progress);
        let spec = IngestSpec {
            folder_template: String::new(),
            write_manifest: false,
            ..IngestSpec::new(vec![source.clone()], c.dest.clone())
        };
        let plan = plan_ingest(&spec, &NoMetadata, &ctx).unwrap();
        let report = execute_ingest(&plan, &ctx).unwrap();
        let dest = report.verified[0].dest.clone();
        let source_mtime =
            UNIX_EPOCH + Duration::from_nanos(report.verified[0].source_mtime_ns.unwrap());
        let dest_mtime =
            UNIX_EPOCH + Duration::from_nanos(report.verified[0].dest_mtime_ns.unwrap());

        // Both sides still have the same size and each other, but no longer match the
        // content hash recorded by the verified copy. Restore both mtimes to isolate the
        // hash check from the separate modification-time check.
        fs::write(&source, b"new-one").unwrap();
        fs::write(&dest, b"new-one").unwrap();
        for (path, modified) in [(&source, source_mtime), (&dest, dest_mtime)] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(modified))
                .unwrap();
        }
        assert_eq!(
            modified_ns(&fs::symlink_metadata(&source).unwrap()),
            report.verified[0].source_mtime_ns
        );
        assert_eq!(
            modified_ns(&fs::symlink_metadata(&dest).unwrap()),
            report.verified[0].dest_mtime_ns
        );
        let check = safe_to_format(std::slice::from_ref(&source), &[&report], false, &ctx).unwrap();
        assert!(!check.safe, "{check:?}");
    }

    #[test]
    fn shallow_format_check_flags_source_mtime_changed_after_report() {
        use std::time::{Duration, UNIX_EPOCH};

        let c = card();
        let source = c.card.join("DCIM/100CANON/IMG_0001.CR3");
        let (cancel, progress) = ctx_parts();
        let ctx = JobCtx::new(&cancel, &progress);
        let spec = IngestSpec {
            folder_template: String::new(),
            write_manifest: false,
            ..IngestSpec::new(vec![source.clone()], c.dest.clone())
        };
        let plan = plan_ingest(&spec, &NoMetadata, &ctx).unwrap();
        let report = execute_ingest(&plan, &ctx).unwrap();
        let future = UNIX_EPOCH + Duration::from_secs(1_900_000_000);
        fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(future))
            .unwrap();

        let check = safe_to_format(std::slice::from_ref(&source), &[&report], false, &ctx).unwrap();
        assert!(!check.safe, "{check:?}");
    }

    #[test]
    fn conflicts_keep_both_and_rerun_recognises_renamed_copies() {
        let c = card();
        let (cancel, progress) = ctx_parts();
        let ctx = JobCtx::new(&cancel, &progress);
        let spec = IngestSpec {
            folder_template: "{project}".into(),
            file_template: "{orig_name}".into(),
            project: "P".into(),
            write_manifest: false,
            ..IngestSpec::new(vec![c.card.clone()], c.dest.clone())
        };
        let plan = plan_ingest(&spec, &NoMetadata, &ctx).unwrap();
        assert_eq!(plan.count(IngestStatus::Renamed), 1);
        let r = execute_ingest(&plan, &ctx).unwrap();
        assert!(r.is_complete());
        assert!(r.manifest.is_none());
        assert!(c.dest.join("P/IMG_0001 (2).CR3").exists());
        let plan2 = plan_ingest(&spec, &NoMetadata, &ctx).unwrap();
        assert_eq!(
            plan2.count(IngestStatus::AlreadyImported),
            3,
            "{:?}",
            plan2.items
        );
    }

    #[test]
    fn seq_ext_filter_and_missing_sources() {
        let c = card();
        fs::write(c.card.join("DCIM/100CANON/IMG_0001.XMP"), b"<xmp/>").unwrap();
        let (cancel, progress) = ctx_parts();
        let ctx = JobCtx::new(&cancel, &progress);
        let spec = IngestSpec {
            folder_template: String::new(),
            file_template: "{client}_{seq:4}".into(),
            client: "ACME".into(),
            extensions: Some(vec![".cr3".into()]),
            ..IngestSpec::new(vec![c.card.clone()], c.dest.clone())
        };
        let plan = plan_ingest(&spec, &meta, &ctx).unwrap();
        let names: Vec<String> = plan.items.iter().map(|i| paths::name_of(&i.dest)).collect();
        assert_eq!(names, ["ACME_0001.CR3", "ACME_0002.CR3", "ACME_0003.CR3"]);
        // Source removed between plan and execute (card pulled).
        fs::remove_file(c.card.join("DCIM/100CANON/IMG_0002.CR3")).unwrap();
        let r = execute_ingest(&plan, &ctx).unwrap();
        assert_eq!(r.missing.len(), 1);
        assert_eq!(r.verified.len(), 2);
        assert!(!r.is_complete());
        // The XMP was filtered out of the import, so formatting is not safe.
        let fc = safe_to_format(std::slice::from_ref(&c.card), &[&r], false, &ctx).unwrap();
        assert!(!fc.safe);
        assert!(
            fc.unverified
                .iter()
                .any(|u| u.path.ends_with("IMG_0001.XMP"))
        );
    }

    #[test]
    fn refuses_overlapping_dest() {
        let c = card();
        let (cancel, progress) = ctx_parts();
        let spec = IngestSpec::new(vec![c.card.clone()], c.card.join("DCIM"));
        assert!(plan_ingest(&spec, &NoMetadata, &JobCtx::new(&cancel, &progress)).is_err());
        let _ = File::open(c.card.join("DCIM/100CANON/IMG_0001.CR3")).unwrap();
    }
}
