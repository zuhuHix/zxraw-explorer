//! `zxraw-cli`: terminal front-end over the ZXraw Explorer engine crates.
//!
//! Every command is a thin wrapper around a library function; no file logic lives here.
//!
//! Exit codes: `0` success, `1` problems found (verification failed, unreadable items,
//! refused rename/ingest, interrupted index), `2` usage or I/O error.

mod du;
mod dupes;
mod hash;
mod index;
mod ingest;
mod meta;
mod rename;
mod util;
mod verify;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "zxraw-cli",
    version,
    about = "ZXraw Explorer command-line tools"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// BLAKE3 hash files (folders are walked recursively), optionally writing a manifest.
    Hash(hash::HashArgs),
    /// Verify a b3sum-compatible manifest against the files on disk.
    Verify(verify::VerifyArgs),
    /// Find duplicate files (read-only: nothing is ever deleted).
    Dupes(dupes::DupesArgs),
    /// Folder size summary, largest files and extension breakdown.
    Du(du::DuArgs),
    /// Batch rename. Dry-run unless --apply.
    Rename(rename::RenameArgs),
    /// Print image or video metadata for one file.
    Meta(meta::MetaArgs),
    /// Index folders into the ZXraw Explorer database.
    Index(index::IndexArgs),
    /// Search the ZXraw Explorer database.
    Search(index::SearchArgs),
    /// Import media with verified copies. Plan only unless --apply.
    Ingest(ingest::IngestArgs),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Hash(a) => hash::run(a),
        Commands::Verify(a) => verify::run(a),
        Commands::Dupes(a) => dupes::run(a),
        Commands::Du(a) => du::run(a),
        Commands::Rename(a) => rename::run(a),
        Commands::Meta(a) => meta::run(a),
        Commands::Index(a) => index::run_index(a),
        Commands::Search(a) => index::run_search(a),
        Commands::Ingest(a) => ingest::run(a),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(util::EXIT_USAGE)
        }
    }
}
