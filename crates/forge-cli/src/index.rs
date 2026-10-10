//! `zxraw-cli index` and `zxraw-cli search`: the same database the desktop app uses.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use forge_db::indexer::BatchCommitted;
use forge_db::{Db, IndexOutcome, Indexer, SearchOptions};
use forge_jobs::CancelToken;

use crate::util::{self, EXIT_OK, EXIT_PROBLEMS};

#[derive(Args)]
pub struct IndexArgs {
    /// Folders to index. They are added to the database's index roots.
    #[arg(required = true)]
    pub roots: Vec<PathBuf>,
    /// Database file. Defaults to %LOCALAPPDATA%\ZXraw Explorer\data\forge.db.
    #[arg(long, value_name = "FILE")]
    pub db: Option<PathBuf>,
}

#[derive(Args)]
pub struct SearchArgs {
    /// Query in the ZXraw Explorer search language (e.g. `ext:jpg size:>10mb cat`).
    pub query: String,
    /// Database file. Defaults to %LOCALAPPDATA%\ZXraw Explorer\data\forge.db.
    #[arg(long, value_name = "FILE")]
    pub db: Option<PathBuf>,
    /// Maximum number of results.
    #[arg(long, default_value_t = 100)]
    pub limit: u32,
    /// Print results as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Default database path, shared with the desktop app.
pub fn default_db_path() -> Result<PathBuf> {
    let pd = directories::ProjectDirs::from("", "", "ZXraw Explorer")
        .context("cannot determine the local data folder")?;
    Ok(pd.data_local_dir().join("forge.db"))
}

fn resolve_db(db: Option<PathBuf>) -> Result<PathBuf> {
    match db {
        Some(p) => Ok(p),
        None => default_db_path(),
    }
}

pub fn run_index(args: IndexArgs) -> Result<u8> {
    for r in &args.roots {
        util::require_exists(r)?;
        if !r.is_dir() {
            bail!("{} is not a folder", r.display());
        }
    }
    let db_path = resolve_db(args.db)?;
    let db = Db::open(&db_path).with_context(|| format!("cannot open {}", db_path.display()))?;

    let cancel = CancelToken::new();
    util::install_ctrl_c(&cancel);

    let mut problems = false;
    for root in &args.roots {
        let root_id = db.add_index_root(&util::absolute(root)?)?;
        let mut indexer =
            Indexer::new(&db)
                .cancel_token(cancel.clone())
                .on_batch(|b: &BatchCommitted| {
                    eprint!(
                        "\r  {} items indexed, {} folders pending   ",
                        b.items_total, b.pending_dirs
                    );
                });
        let report = indexer.index_root(root_id)?;
        eprintln!();
        println!(
            "{}: {} items, {} folders, {} errors, {} marked missing{}",
            report.root.display(),
            report.items,
            report.dirs,
            report.errors,
            report.marked_missing,
            if report.resumed { " (resumed)" } else { "" }
        );
        match report.outcome {
            IndexOutcome::Completed => {}
            IndexOutcome::Cancelled => {
                eprintln!("cancelled; progress is saved, re-run the same command to resume");
                problems = true;
                break;
            }
            IndexOutcome::RootUnavailable(why) => {
                eprintln!("{}: folder unavailable: {why}", root.display());
                problems = true;
            }
        }
    }
    println!("database: {}", db_path.display());
    Ok(if problems { EXIT_PROBLEMS } else { EXIT_OK })
}

pub fn run_search(args: SearchArgs) -> Result<u8> {
    let db_path = resolve_db(args.db)?;
    if !Path::new(&db_path).exists() {
        bail!(
            "no index at {}; run `zxraw-cli index <folder>` first",
            db_path.display()
        );
    }
    let db = Db::open(&db_path).with_context(|| format!("cannot open {}", db_path.display()))?;
    let opts = SearchOptions {
        limit: Some(args.limit),
        ..Default::default()
    };
    let records = db
        .search_str(&args.query, &opts)
        .with_context(|| format!("query {:?}", args.query))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&records)?);
    } else {
        for r in &records {
            println!(
                "{:>12}  {}",
                forge_core::model::format_size(r.size),
                r.path.display()
            );
        }
        println!("{} result(s)", records.len());
    }
    Ok(EXIT_OK)
}
