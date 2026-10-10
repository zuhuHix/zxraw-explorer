//! `zxraw-cli du`: folder size summary, largest files and extension breakdown.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Args;
use forge_core::disk_usage::{self, UsageOptions};
use forge_core::model::format_size;
use forge_core::ops::JobCtx;
use forge_jobs::{CancelToken, Progress};

use crate::util::{self, EXIT_OK};

#[derive(Args)]
pub struct DuArgs {
    /// Folder to analyse.
    pub root: PathBuf,
    /// How many entries to list in each section.
    #[arg(long, default_value_t = 20)]
    pub top: usize,
}

pub fn run(args: DuArgs) -> Result<u8> {
    util::require_exists(&args.root)?;
    if !args.root.is_dir() {
        bail!("{} is not a folder", args.root.display());
    }
    let cancel = CancelToken::new();
    util::install_ctrl_c(&cancel);
    let progress = Progress::new();
    let ctx = JobCtx::new(&cancel, &progress);
    let opts = UsageOptions {
        top_n: args.top.max(1),
        ..Default::default()
    };
    let report = disk_usage::scan(&args.root, &opts, &ctx)?;

    let root = report.root();
    let total = root.size.max(1);
    println!(
        "{}: {} in {} files, {} folders",
        root.path.display(),
        format_size(root.size),
        root.files,
        root.dirs
    );

    println!();
    println!("Largest folders (direct children):");
    let mut kids: Vec<_> = report.children(0).collect();
    kids.sort_by_key(|k| std::cmp::Reverse(k.size));
    for k in kids.iter().take(args.top) {
        println!(
            "  {:>10}  {:>5.1}%  {}",
            format_size(k.size),
            k.size as f64 * 100.0 / total as f64,
            k.name
        );
    }

    println!();
    println!("Largest files:");
    let mut files = report.top_files.clone();
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    for (path, size) in files.iter().take(args.top) {
        println!("  {:>10}  {}", format_size(*size), path.display());
    }

    println!();
    println!("By extension:");
    let mut exts = report.by_ext.clone();
    exts.sort_by_key(|e| std::cmp::Reverse(e.bytes));
    for e in exts.iter().take(args.top) {
        let ext = if e.ext.is_empty() {
            "(none)"
        } else {
            e.ext.as_str()
        };
        println!(
            "  {:>10}  {:>8} files  .{}",
            format_size(e.bytes),
            e.files,
            ext
        );
    }

    if report.links_skipped > 0 {
        println!();
        println!("{} link(s) not followed", report.links_skipped);
    }
    if report.errors > 0 {
        eprintln!(
            "warning: {} item(s) could not be read; totals may be low",
            report.errors
        );
        util::print_capped(
            report
                .error_samples
                .iter()
                .map(|e| format!("{}: {}", e.path.display(), e.reason)),
            10,
        );
    }
    Ok(EXIT_OK)
}
