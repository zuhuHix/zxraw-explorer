//! `zxraw-cli verify`: check a manifest against files on disk.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use forge_core::hashing;
use forge_jobs::CancelToken;

use crate::util::{self, EXIT_OK, EXIT_PROBLEMS};

#[derive(Args)]
pub struct VerifyArgs {
    /// A b3sum-compatible manifest (as written by `zxraw-cli hash --manifest`, or `b3sum`).
    pub manifest: PathBuf,
    /// Folder the manifest paths are relative to. Defaults to the manifest's folder.
    #[arg(long)]
    pub root: Option<PathBuf>,
}

pub fn run(args: VerifyArgs) -> Result<u8> {
    let cancel = CancelToken::new();
    util::install_ctrl_c(&cancel);

    let text = fs::read_to_string(&args.manifest)
        .with_context(|| format!("cannot read manifest {}", args.manifest.display()))?;
    let entries = hashing::parse_manifest(&text)
        .with_context(|| format!("invalid manifest {}", args.manifest.display()))?;
    let root = args.root.clone().unwrap_or_else(|| {
        args.manifest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    });

    let report = hashing::verify_entries(&root, &entries, &cancel, None)?;
    println!(
        "{} checked: {} ok, {} mismatched, {} missing, {} unreadable",
        entries.len(),
        report.ok.len(),
        report.mismatched.len(),
        report.missing.len(),
        report.unreadable.len()
    );
    for p in &report.mismatched {
        println!("MISMATCH  {p}");
    }
    for p in &report.missing {
        println!("MISSING   {p}");
    }
    for (p, why) in &report.unreadable {
        println!("UNREADABLE {p}: {why}");
    }

    Ok(if report.all_ok() {
        EXIT_OK
    } else {
        EXIT_PROBLEMS
    })
}
