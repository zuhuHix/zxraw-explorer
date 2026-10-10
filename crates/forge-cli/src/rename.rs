//! `zxraw-cli rename`: batch rename preview, applied only with `--apply`.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use chrono::{DateTime, Local};
use clap::Args;
use forge_core::rename::{
    self, CaseMode, ExistingNames, RenameInput, RenameProblem, RenameRule, RenameStatus,
};

use crate::util::{self, EXIT_OK, EXIT_PROBLEMS};

#[derive(Args)]
pub struct RenameArgs {
    /// Files to rename (folders are not supported).
    #[arg(required = true)]
    pub files: Vec<PathBuf>,
    /// A rule, applied in order. Forms: `template:PATTERN` (tokens {name} {ext} {n:3}
    /// {parent} {date:FMT}), `replace:FIND=>REPL`, `regex:PAT=>REPL`, `lower`, `upper`,
    /// `title`, `trim` (stem only), `ext-lower`, `ext-upper`.
    #[arg(long = "rule", value_name = "RULE", required = true)]
    pub rules: Vec<String>,
    /// Actually rename. Without this flag nothing on disk changes.
    #[arg(long)]
    pub apply: bool,
}

/// Parse one `--rule` value into an engine rule.
pub fn parse_rule(s: &str) -> Result<RenameRule, String> {
    let split_pair = |body: &str, what: &str| -> Result<(String, String), String> {
        body.split_once("=>")
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .ok_or_else(|| format!("{what} rule needs FIND=>REPLACE, got {body:?}"))
    };
    if let Some(p) = s.strip_prefix("template:") {
        return Ok(RenameRule::Template {
            pattern: p.to_string(),
            start: 1,
            step: 1,
        });
    }
    if let Some(body) = s.strip_prefix("replace:") {
        let (find, replace) = split_pair(body, "replace")?;
        return Ok(RenameRule::Replace {
            find,
            replace,
            case_sensitive: true,
        });
    }
    if let Some(body) = s.strip_prefix("regex:") {
        let (pattern, replacement) = split_pair(body, "regex")?;
        return Ok(RenameRule::Regex {
            pattern,
            replacement,
        });
    }
    Ok(match s {
        "lower" => RenameRule::ChangeCase(CaseMode::Lower),
        "upper" => RenameRule::ChangeCase(CaseMode::Upper),
        "title" => RenameRule::ChangeCase(CaseMode::Title),
        "trim" => RenameRule::Trim,
        "ext-lower" => RenameRule::ExtensionCase(CaseMode::Lower),
        "ext-upper" => RenameRule::ExtensionCase(CaseMode::Upper),
        other => return Err(format!("unknown rule {other:?}")),
    })
}

pub fn run(args: RenameArgs) -> Result<u8> {
    let rules: Vec<RenameRule> = match args
        .rules
        .iter()
        .map(|r| parse_rule(r))
        .collect::<Result<_, _>>()
    {
        Ok(r) => r,
        Err(msg) => bail!("{msg}"),
    };

    let mut items = Vec::with_capacity(args.files.len());
    let mut seen_paths = HashSet::new();
    for f in &args.files {
        util::require_exists(f)?;
        let md = fs::metadata(f)?;
        if !md.is_file() {
            bail!("{} is not a file", f.display());
        }
        let abs = util::absolute(f)?;
        if !seen_paths.insert(abs.to_string_lossy().to_lowercase()) {
            continue;
        }
        let mut item = RenameInput::new(abs);
        item.date = md
            .modified()
            .ok()
            .map(|t| DateTime::<Local>::from(t).date_naive());
        items.push(item);
    }

    // Collision detection needs the names already present in each affected folder.
    let mut existing = ExistingNames::default();
    let dirs: BTreeSet<PathBuf> = items
        .iter()
        .filter_map(|it| it.path.parent().map(Path::to_path_buf))
        .collect();
    for dir in &dirs {
        let names: Vec<String> = fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        existing.add_dir(dir, names);
    }

    let previews = rename::preview_with_existing(&items, &rules, &existing);
    let problems: Vec<_> = previews.iter().filter(|p| p.problem.is_some()).collect();
    let changes = previews.iter().filter(|p| p.is_change()).count();

    for p in &previews {
        let name = display_name(&p.old_path);
        match &p.problem {
            Some(problem) => println!("!  {name}  ->  {}  [{}]", p.new_name, describe(problem)),
            None if p.is_change() => println!("   {name}  ->  {}", p.new_name),
            None => println!("   {name}  (unchanged)"),
        }
    }
    println!();

    if !problems.is_empty() {
        eprintln!(
            "refusing: {} item(s) have problems; nothing was renamed",
            problems.len()
        );
        return Ok(EXIT_PROBLEMS);
    }
    if !args.apply {
        println!("dry run: {changes} file(s) would be renamed. Re-run with --apply to rename.");
        return Ok(EXIT_OK);
    }
    if changes == 0 {
        println!("nothing to rename");
        return Ok(EXIT_OK);
    }

    let report = rename::apply(&previews);
    match report.status {
        RenameStatus::Completed => {
            println!("renamed {} file(s)", report.renamed.len());
            Ok(EXIT_OK)
        }
        RenameStatus::NothingToDo => {
            println!("nothing to rename");
            Ok(EXIT_OK)
        }
        RenameStatus::Refused { blocked } => {
            eprintln!("refused: {} item(s) blocked", blocked.len());
            Ok(EXIT_PROBLEMS)
        }
        RenameStatus::RolledBack { failed, error } => {
            eprintln!(
                "failed at {}: {error}; all changes were reverted",
                failed.display()
            );
            Ok(EXIT_PROBLEMS)
        }
        RenameStatus::RollbackIncomplete { failed, error } => {
            eprintln!(
                "failed at {}: {error}; revert also failed:",
                failed.display()
            );
            for s in &report.stranded {
                eprintln!(
                    "  {} (should be {}): {}",
                    s.current.display(),
                    s.original.display(),
                    s.error
                );
            }
            Ok(EXIT_PROBLEMS)
        }
    }
}

fn display_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn describe(p: &RenameProblem) -> String {
    p.to_string()
}
