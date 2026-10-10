//! `zxraw-cli hash`: BLAKE3 per file, optional b3sum-compatible manifest.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Args;
use forge_core::hashing::{self, ManifestEntry};
use forge_core::paths;
use forge_jobs::CancelToken;

use crate::util::{self, EXIT_OK, EXIT_PROBLEMS};

#[derive(Args)]
pub struct HashArgs {
    /// Files or folders to hash. Folders are walked recursively; links are never followed.
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Write a b3sum-compatible manifest here. Entries are relative to the manifest's folder,
    /// so every input must live inside that folder. The manifest itself is never hashed.
    #[arg(long, value_name = "FILE")]
    pub manifest: Option<PathBuf>,
}

pub fn run(args: HashArgs) -> Result<u8> {
    let cancel = CancelToken::new();
    util::install_ctrl_c(&cancel);

    let manifest_abs = match &args.manifest {
        Some(m) => Some(util::absolute(m)?),
        None => None,
    };
    let base = manifest_abs
        .as_ref()
        .map(|m| m.parent().map(Path::to_path_buf).unwrap_or_default());

    // Validate inputs up front so a manifest is never written for a partial, mis-rooted set.
    let mut inputs = Vec::with_capacity(args.paths.len());
    for p in &args.paths {
        util::require_exists(p)?;
        let abs = util::absolute(p)?;
        if let Some(base) = &base
            && !paths::is_within(base, &abs)
        {
            bail!(
                "{} is outside the manifest folder {}; write the manifest in a common parent",
                p.display(),
                base.display()
            );
        }
        inputs.push((p.clone(), abs));
    }

    let exclude: Vec<PathBuf> = manifest_abs.iter().cloned().collect();
    let mut hashed: Vec<(PathBuf, hashing::Digest)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for (given, abs) in &inputs {
        let md = fs::symlink_metadata(given)?;
        if md.file_type().is_symlink() {
            errors.push(format!(
                "{}: is a link; links are not followed",
                given.display()
            ));
        } else if md.is_dir() {
            let (entries, errs) = hashing::build_manifest(abs, &exclude, &cancel, None)?;
            for ManifestEntry { hash, rel_path } in entries {
                hashed.push((abs.join(rel_path), hash));
            }
            errors.extend(
                errs.into_iter()
                    .map(|(p, e)| format!("{}: {e}", p.display())),
            );
        } else {
            match hashing::hash_file(abs, &cancel, None) {
                Ok(h) => hashed.push((abs.clone(), h)),
                Err(e) => errors.push(format!("{}: {e}", given.display())),
            }
        }
    }
    hashed.sort_by(|a, b| a.0.cmp(&b.0));

    match (&manifest_abs, &base) {
        (Some(manifest), Some(base)) => {
            let mut entries = Vec::with_capacity(hashed.len());
            for (path, hash) in &hashed {
                let rel = path.strip_prefix(base).map_err(|_| {
                    anyhow::anyhow!("{} is not inside {}", path.display(), base.display())
                })?;
                entries.push(ManifestEntry {
                    hash: *hash,
                    rel_path: paths::to_slash(rel),
                });
            }
            entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
            hashing::write_manifest(manifest, &entries)?;
            println!("wrote {} entries to {}", entries.len(), manifest.display());
        }
        _ => {
            for (path, hash) in &hashed {
                println!("{}  {}", hashing::to_hex(hash), path.display());
            }
        }
    }

    if errors.is_empty() {
        return Ok(EXIT_OK);
    }
    eprintln!("{} problem(s):", errors.len());
    util::print_capped(errors, 50);
    Ok(EXIT_PROBLEMS)
}
