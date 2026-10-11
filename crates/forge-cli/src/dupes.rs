//! `zxraw-cli dupes`: staged duplicate report. Read-only; nothing is ever deleted.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use forge_core::duplicates::{self, DupOptions};
use forge_core::hashing::to_hex;
use forge_core::model::format_size;
use forge_core::ops::JobCtx;
use forge_jobs::{CancelToken, Progress};

use crate::util::{self, EXIT_OK, EXIT_PROBLEMS};

#[derive(Args)]
pub struct DupesArgs {
    /// Folders (or single files) to scan. Nested roots are de-duplicated automatically.
    #[arg(required = true)]
    pub roots: Vec<PathBuf>,
    /// Ignore files smaller than this, e.g. `1mb`, `512k`, `4096`.
    #[arg(long, default_value = "1")]
    pub min_size: String,
    /// Confirm each hash group with a byte-for-byte comparison.
    #[arg(long)]
    pub byte_compare: bool,
    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: DupesArgs) -> Result<u8> {
    for r in &args.roots {
        util::require_exists(r)?;
    }
    let opts = DupOptions {
        min_size: util::parse_size(&args.min_size)?,
        include_empty: false,
        byte_compare: args.byte_compare,
    };
    let cancel = CancelToken::new();
    util::install_ctrl_c(&cancel);
    let progress = Progress::new();
    let ctx = JobCtx::new(&cancel, &progress);

    let report = duplicates::find_duplicates(&args.roots, &opts, &ctx)?;
    let problems = report.errors.len();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let wasted: u64 = report.groups.iter().map(|g| g.wasted_bytes()).sum();
        println!(
            "scanned {} files; candidates after size/partial/full stages: {} / {} / {}",
            report.files_scanned,
            report.stage_candidates[0],
            report.stage_candidates[1],
            report.stage_candidates[2]
        );
        let t = report.stage_times.map(|d| d.as_secs_f64());
        println!(
            "stage times (s): walk+size {:.3} / partial hash {:.3} / full hash {:.3} / byte compare {:.3}",
            t[0], t[1], t[2], t[3]
        );
        println!(
            "{} duplicate group(s), {} reclaimable by keeping one copy each",
            report.groups.len(),
            format_size(wasted)
        );
        for (i, g) in report.groups.iter().enumerate() {
            let hash = g
                .hash
                .as_ref()
                .map(|h| to_hex(h))
                .unwrap_or_else(|| "(empty files)".into());
            println!();
            println!(
                "group {} - {} x {} - {} wasted - {}",
                i + 1,
                g.files.len(),
                format_size(g.size),
                format_size(g.wasted_bytes()),
                hash
            );
            for f in &g.files {
                println!("  {}", f.display());
            }
        }
        if problems > 0 {
            eprintln!("{problems} item(s) could not be read:");
            util::print_capped(
                report
                    .errors
                    .iter()
                    .map(|e| format!("{}: {}", e.path.display(), e.reason)),
                50,
            );
        }
    }
    if report.links_skipped > 0 {
        eprintln!("{} link(s) skipped (not followed)", report.links_skipped);
    }

    Ok(if problems == 0 {
        EXIT_OK
    } else {
        EXIT_PROBLEMS
    })
}
