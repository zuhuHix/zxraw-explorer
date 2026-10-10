//! `zxraw-cli ingest`: plan by default; `--apply` copies, verifies and writes a manifest.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use chrono::{DateTime, NaiveDateTime};
use clap::Args;
use forge_core::ingest::{self, CaptureInfo, IngestMetadataProvider, IngestSpec, IngestStatus};
use forge_core::model::{MediaKind, ext_of, format_size, media_kind_for_ext};
use forge_core::ops::JobCtx;
use forge_jobs::{CancelToken, Progress};
use forge_media::ImageMetadata;

use crate::util::{self, EXIT_OK, EXIT_PROBLEMS};

#[derive(Args)]
pub struct IngestArgs {
    /// Source files or folders (never modified).
    #[arg(long = "from", required = true, num_args = 1..)]
    pub from: Vec<PathBuf>,
    /// Destination root folder.
    #[arg(long = "to", required = true)]
    pub to: PathBuf,
    /// Folder template, relative to --to. `/` makes sub-folders.
    /// Tokens: {YYYY} {YY} {MM} {DD} {hh} {mm} {ss} {date} {time} {camera} {project}
    /// {client} {shoot} {orig_name} {ext} {seq} {seq:N}.
    #[arg(long, default_value = "{YYYY}/{YYYY}-{MM}-{DD}")]
    pub folders: String,
    /// File name template. Without {ext} the original extension is appended.
    #[arg(long, default_value = "{orig_name}")]
    pub names: String,
    /// Only import these extensions (comma separated, e.g. `jpg,cr3,mp4`).
    #[arg(long, value_delimiter = ',')]
    pub ext: Option<Vec<String>>,
    #[arg(long, default_value = "")]
    pub project: String,
    #[arg(long, default_value = "")]
    pub client: String,
    #[arg(long, default_value = "")]
    pub shoot: String,
    /// Copy and verify. Without this flag the plan is printed and nothing is written.
    #[arg(long)]
    pub apply: bool,
}

/// Capture date and camera from image metadata (EXIF). Videos and unreadable files fall
/// back to the file's modified time, as the engine does without metadata.
pub struct MediaMetaProvider;

impl IngestMetadataProvider for MediaMetaProvider {
    fn capture_info(&self, path: &Path) -> CaptureInfo {
        let ext = ext_of(path);
        if !matches!(media_kind_for_ext(&ext), MediaKind::Image | MediaKind::Raw) {
            return CaptureInfo::default();
        }
        match forge_media::read_image_metadata(path) {
            Ok(m) => CaptureInfo {
                captured: captured_of(&m),
                camera: camera_of(&m),
            },
            Err(_) => CaptureInfo::default(),
        }
    }
}

/// Camera wall-clock time: the local part of `date_taken` if present (keeps the camera's
/// date even when an offset is recorded), else the UTC-based capture timestamp.
fn captured_of(m: &ImageMetadata) -> Option<NaiveDateTime> {
    if let Some(local) = m.date_taken.as_deref().and_then(|s| s.get(..19))
        && let Ok(t) = NaiveDateTime::parse_from_str(local, "%Y-%m-%dT%H:%M:%S")
    {
        return Some(t);
    }
    m.capture_timestamp
        .and_then(|t| DateTime::from_timestamp(t, 0))
        .map(|d| d.naive_utc())
}

fn camera_of(m: &ImageMetadata) -> Option<String> {
    let s = m.model.as_deref().or(m.make.as_deref())?.trim();
    (!s.is_empty()).then(|| s.to_string())
}

pub fn run(args: IngestArgs) -> Result<u8> {
    for s in &args.from {
        util::require_exists(s)?;
    }
    let mut spec = IngestSpec::new(args.from.clone(), args.to.clone());
    spec.folder_template = args.folders.clone();
    spec.file_template = args.names.clone();
    spec.project = args.project.clone();
    spec.client = args.client.clone();
    spec.shoot = args.shoot.clone();
    spec.extensions = args.ext.as_ref().map(|v| {
        v.iter()
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .collect()
    });
    spec.write_manifest = true;

    let cancel = CancelToken::new();
    util::install_ctrl_c(&cancel);
    let progress = Progress::new();
    let ctx = JobCtx::new(&cancel, &progress);

    let plan = ingest::plan_ingest(&spec, &MediaMetaProvider, &ctx)?;
    print_plan(&plan);

    if !args.apply {
        if !plan.errors.is_empty() {
            eprintln!(
                "plan has {} problem(s); fix them before --apply",
                plan.errors.len()
            );
            return Ok(EXIT_PROBLEMS);
        }
        println!("dry run: nothing was copied. Re-run with --apply to copy and verify.");
        return Ok(EXIT_OK);
    }
    if !plan.errors.is_empty() {
        bail!(
            "refusing to apply: the plan has {} problem(s); nothing was copied",
            plan.errors.len()
        );
    }

    let report = ingest::execute_ingest(&plan, &ctx)?;
    let bytes: u64 = report.verified.iter().map(|f| f.size).sum();
    println!(
        "copied and verified {} file(s), {}; {} already present",
        report.verified.len(),
        format_size(bytes),
        report.already_present.len()
    );
    for f in &report.failed {
        eprintln!("failed: {}: {}", f.path.display(), f.reason);
    }
    for m in &report.missing {
        eprintln!("missing source: {}", m.display());
    }
    if report.cancelled {
        eprintln!("cancelled; verified files are kept, re-run to finish");
    }
    match &report.manifest {
        Some(m) => println!("manifest: {}", m.display()),
        None => eprintln!("no manifest was written"),
    }
    Ok(if report.is_complete() {
        EXIT_OK
    } else {
        EXIT_PROBLEMS
    })
}

fn print_plan(plan: &ingest::IngestPlan) {
    let new = plan.count(IngestStatus::New);
    let renamed = plan.count(IngestStatus::Renamed);
    let present = plan.count(IngestStatus::AlreadyImported);
    println!(
        "plan: {} file(s) to copy ({}), {} renamed for name conflicts, {} already imported, {} problem(s)",
        new + renamed,
        format_size(plan.bytes_to_copy),
        renamed,
        present,
        plan.errors.len()
    );
    for item in &plan.items {
        let tag = match item.status {
            IngestStatus::New => "copy   ",
            IngestStatus::Renamed => "rename ",
            IngestStatus::AlreadyImported => "present",
        };
        println!(
            "  {tag} {}  ->  {}",
            item.source.display(),
            item.dest.display()
        );
    }
    if renamed > 0 {
        println!(
            "conflicts: {renamed} file(s) keep both (name already taken by different content)"
        );
    }
    for e in &plan.errors {
        eprintln!("problem: {}: {}", e.path.display(), e.reason);
    }
}
