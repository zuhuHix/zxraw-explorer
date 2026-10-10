//! Staged duplicate finder and empty-folder finder. **Read-only**: nothing here deletes or
//! modifies files; the UI decides what to do with the groups.
//!
//! Stages (each one only sees candidates that survived the previous one):
//! 1. group by exact size (free: comes from the directory scan)
//! 2. xxh3 partial hash (size + first/last 64 KiB) — cheap, rules out most non-duplicates
//! 3. full BLAKE3 — the actual equality proof (collision-resistant)
//! 4. optional byte-for-byte comparison, for users who want certainty without trusting a hash

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::hashing::{self, Digest};
use crate::ops::{JobCtx, NodeKind, OpIssue, WalkItem, node_kind, walk_tree};
use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DupOptions {
    /// Ignore files smaller than this (bytes).
    pub min_size: u64,
    /// Report zero-byte files as one duplicate group (they are trivially identical).
    pub include_empty: bool,
    /// After BLAKE3, confirm groups by comparing bytes.
    pub byte_compare: bool,
}

impl Default for DupOptions {
    fn default() -> Self {
        Self {
            min_size: 1,
            include_empty: false,
            byte_compare: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub size: u64,
    /// BLAKE3 of the content (`None` for the zero-byte group).
    pub hash: Option<Digest>,
    /// Sorted paths, at least two.
    pub files: Vec<PathBuf>,
}

impl DuplicateGroup {
    /// Bytes reclaimable by keeping one copy.
    pub fn wasted_bytes(&self) -> u64 {
        self.size * (self.files.len() as u64).saturating_sub(1)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DupReport {
    /// Sorted by wasted bytes, largest first.
    pub groups: Vec<DuplicateGroup>,
    pub files_scanned: u64,
    /// Candidates remaining after each stage: [size, partial hash, full hash].
    pub stage_candidates: [u64; 3],
    pub links_skipped: u64,
    pub errors: Vec<OpIssue>,
}

/// Drop roots that are inside other roots so no file is counted twice.
fn dedupe_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let canon: Vec<PathBuf> = roots
        .iter()
        .map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| r.clone()))
        .collect();
    let mut keep = Vec::new();
    for (i, r) in canon.iter().enumerate() {
        let covered = canon
            .iter()
            .enumerate()
            .any(|(j, o)| j != i && paths::is_within(o, r) && (!paths::paths_equal(o, r) || j < i));
        if !covered {
            keep.push(roots[i].clone());
        }
    }
    keep
}

/// Collect regular files under `roots` (a root may also be a single file).
fn collect_files(
    roots: &[PathBuf],
    ctx: &JobCtx<'_>,
    errors: &mut Vec<OpIssue>,
    links: &mut u64,
    mut f: impl FnMut(PathBuf, u64),
) -> Result<()> {
    for root in dedupe_roots(roots) {
        let md = match std::fs::symlink_metadata(&root) {
            Ok(m) => m,
            Err(e) => {
                errors.push(OpIssue::new(&root, CoreError::io(&root, e).to_string()));
                continue;
            }
        };
        match node_kind(&md) {
            NodeKind::File => f(root, md.len()),
            NodeKind::Link => *links += 1,
            NodeKind::Dir => walk_tree(&root, ctx.cancel, |item| match item {
                WalkItem::Entry(e) if e.is_file() => {
                    ctx.progress.add_items(1);
                    f(e.path.clone(), e.size)
                }
                WalkItem::Entry(e) if e.is_link() => *links += 1,
                WalkItem::Entry(_) => {}
                WalkItem::Error { path, error } => {
                    errors.push(OpIssue::new(path, error.to_string()))
                }
            })?,
        }
    }
    Ok(())
}

/// Find duplicate files under `roots`. Links are never followed or reported.
pub fn find_duplicates(
    roots: &[PathBuf],
    opts: &DupOptions,
    ctx: &JobCtx<'_>,
) -> Result<DupReport> {
    let mut report = DupReport::default();
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut errors = Vec::new();
    let mut links = 0;
    ctx.progress.set_current("scanning");
    collect_files(roots, ctx, &mut errors, &mut links, |path, size| {
        report.files_scanned += 1;
        let wanted = if size == 0 {
            opts.include_empty
        } else {
            size >= opts.min_size
        };
        if wanted && seen.insert(path.clone()) {
            by_size.entry(size).or_default().push(path);
        }
    })?;
    report.links_skipped = links;
    by_size.retain(|_, v| v.len() > 1);
    report.stage_candidates[0] = by_size.values().map(|v| v.len() as u64).sum();

    let mut groups = Vec::new();
    if let Some(mut empty) = by_size.remove(&0) {
        empty.sort();
        groups.push(DuplicateGroup {
            size: 0,
            hash: None,
            files: empty,
        });
    }

    // Stage 2: partial hash.
    ctx.progress.set_current("comparing file starts and ends");
    let mut by_partial: HashMap<(u64, u64), Vec<PathBuf>> = HashMap::new();
    for (size, files) in by_size {
        for p in files {
            ctx.cancel.check()?;
            match hashing::partial_hash(&p) {
                Ok(h) => by_partial.entry((size, h)).or_default().push(p),
                Err(e) => errors.push(OpIssue::new(&p, e.to_string())),
            }
        }
    }
    by_partial.retain(|_, v| v.len() > 1);
    report.stage_candidates[1] = by_partial.values().map(|v| v.len() as u64).sum();

    // Stage 3: full BLAKE3.
    ctx.progress.set_current("hashing candidates");
    let total: u64 = by_partial
        .iter()
        .map(|((s, _), v)| s * v.len() as u64)
        .sum();
    ctx.progress.add_total_bytes(total);
    let mut by_full: HashMap<(u64, Digest), Vec<PathBuf>> = HashMap::new();
    for ((size, _), files) in by_partial {
        for p in files {
            match hashing::hash_file(&p, ctx.cancel, Some(ctx.progress)) {
                Ok(h) => by_full.entry((size, h)).or_default().push(p),
                Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                Err(e) => errors.push(OpIssue::new(&p, e.to_string())),
            }
        }
    }
    by_full.retain(|_, v| v.len() > 1);
    report.stage_candidates[2] = by_full.values().map(|v| v.len() as u64).sum();

    // Stage 4: optional byte comparison (splits groups on mismatch).
    for ((size, hash), mut files) in by_full {
        files.sort();
        let parts = if opts.byte_compare {
            ctx.progress.set_current("comparing bytes");
            split_by_bytes(files, ctx, &mut errors)?
        } else {
            vec![files]
        };
        for files in parts.into_iter().filter(|g| g.len() > 1) {
            groups.push(DuplicateGroup {
                size,
                hash: Some(hash),
                files,
            });
        }
    }
    groups.sort_by(|a, b| {
        b.wasted_bytes()
            .cmp(&a.wasted_bytes())
            .then_with(|| a.files.cmp(&b.files))
    });
    report.groups = groups;
    report.errors = errors;
    Ok(report)
}

fn split_by_bytes(
    files: Vec<PathBuf>,
    ctx: &JobCtx<'_>,
    errors: &mut Vec<OpIssue>,
) -> Result<Vec<Vec<PathBuf>>> {
    let mut groups: Vec<Vec<PathBuf>> = Vec::new();
    'next: for f in files {
        for g in groups.iter_mut() {
            match files_equal(&g[0], &f, ctx) {
                Ok(true) => {
                    g.push(f);
                    continue 'next;
                }
                Ok(false) => {}
                Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                Err(e) => {
                    errors.push(OpIssue::new(&f, e.to_string()));
                    continue 'next;
                }
            }
        }
        groups.push(vec![f]);
    }
    Ok(groups)
}

/// Byte-for-byte equality of two files.
pub fn files_equal(a: &Path, b: &Path, ctx: &JobCtx<'_>) -> Result<bool> {
    let mut fa = File::open(a).map_err(|e| CoreError::io(a, e))?;
    let mut fb = File::open(b).map_err(|e| CoreError::io(b, e))?;
    let la = fa.metadata().map_err(|e| CoreError::io(a, e))?.len();
    let lb = fb.metadata().map_err(|e| CoreError::io(b, e))?.len();
    if la != lb {
        return Ok(false);
    }
    let mut ba = vec![0u8; 256 * 1024];
    let mut bb = vec![0u8; 256 * 1024];
    loop {
        ctx.cancel.check()?;
        let n = read_full(&mut fa, &mut ba).map_err(|e| CoreError::io(a, e))?;
        let m = read_full(&mut fb, &mut bb).map_err(|e| CoreError::io(b, e))?;
        if n != m || ba[..n] != bb[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Folders below `roots` that contain no files or links at any depth. Only the topmost such
/// folder of each empty subtree is returned (deleting it removes the rest). Roots
/// themselves are never returned. Unreadable folders count as non-empty.
pub fn find_empty_dirs(roots: &[PathBuf], ctx: &JobCtx<'_>) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for root in dedupe_roots(roots) {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut nonempty: HashSet<PathBuf> = HashSet::new();
        let mark = |nonempty: &mut HashSet<PathBuf>, start: &Path| {
            let mut cur = Some(start);
            while let Some(p) = cur {
                if p == root.as_path() || !nonempty.insert(p.to_path_buf()) {
                    break;
                }
                cur = p.parent();
            }
        };
        walk_tree(&root, ctx.cancel, |item| match item {
            WalkItem::Entry(e) if e.is_dir() => dirs.push(e.path.clone()),
            WalkItem::Entry(e) => {
                if let Some(parent) = e.path.parent() {
                    mark(&mut nonempty, parent);
                }
            }
            WalkItem::Error { path, .. } => mark(&mut nonempty, path),
        })?;
        for d in dirs {
            if nonempty.contains(&d) {
                continue;
            }
            let parent_empty = d
                .parent()
                .is_some_and(|p| p != root.as_path() && !nonempty.contains(p));
            if !parent_empty {
                out.push(d);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_jobs::{CancelToken, Progress};
    use std::fs;

    #[test]
    fn staged_duplicate_detection() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir_all(r.join("a/日本")).unwrap();
        fs::create_dir_all(r.join("b")).unwrap();
        let big: Vec<u8> = (0..400_000u32).map(|i| (i % 253) as u8).collect();
        let mut big_mid = big.clone();
        big_mid[200_000] ^= 1; // same size, same ends: survives partial hash, not BLAKE3
        fs::write(r.join("a/1.bin"), &big).unwrap();
        fs::write(r.join("b/1-copy.bin"), &big).unwrap();
        fs::write(r.join("a/日本/1-again.bin"), &big).unwrap();
        fs::write(r.join("b/mid.bin"), &big_mid).unwrap();
        fs::write(r.join("a/small.txt"), b"hello").unwrap();
        fs::write(r.join("b/small.txt"), b"hello").unwrap();
        fs::write(r.join("b/other.txt"), b"world").unwrap(); // same size, different
        fs::write(r.join("a/e1"), b"").unwrap();
        fs::write(r.join("b/e2"), b"").unwrap();

        let cancel = CancelToken::new();
        let progress = Progress::default();
        let ctx = JobCtx::new(&cancel, &progress);
        // Overlapping roots must not double count.
        let roots = vec![r.to_path_buf(), r.join("a")];
        let rep = find_duplicates(&roots, &DupOptions::default(), &ctx).unwrap();
        assert_eq!(rep.files_scanned, 9);
        assert_eq!(rep.groups.len(), 2, "{:?}", rep.groups);
        assert_eq!(rep.groups[0].files.len(), 3);
        assert_eq!(rep.groups[0].size, big.len() as u64);
        assert_eq!(rep.groups[0].wasted_bytes(), 2 * big.len() as u64);
        assert_eq!(rep.groups[1].files.len(), 2);
        // size stage: 4 big + 3 five-byte files; partial: mid.bin survives, other.txt not;
        // full: mid.bin eliminated.
        assert_eq!(rep.stage_candidates, [7, 6, 5]);

        let opts = DupOptions {
            include_empty: true,
            byte_compare: true,
            min_size: 1,
        };
        let rep2 = find_duplicates(&roots, &opts, &ctx).unwrap();
        assert_eq!(rep2.groups.len(), 3);
        assert!(
            rep2.groups
                .iter()
                .any(|g| g.size == 0 && g.files.len() == 2)
        );
        let min = DupOptions {
            min_size: 10,
            ..Default::default()
        };
        assert_eq!(find_duplicates(&roots, &min, &ctx).unwrap().groups.len(), 1);
        // Nothing was deleted.
        assert!(r.join("b/1-copy.bin").exists());
    }

    #[test]
    fn cancel_duplicates() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a"), b"x").unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        let progress = Progress::default();
        let r = find_duplicates(
            &[d.path().to_path_buf()],
            &DupOptions::default(),
            &JobCtx::new(&cancel, &progress),
        );
        assert!(matches!(r, Err(CoreError::Cancelled)));
    }

    #[test]
    fn empty_dirs() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir_all(r.join("e1/e2/e3")).unwrap();
        fs::create_dir_all(r.join("full/e4")).unwrap();
        fs::write(r.join("full/f.txt"), b"x").unwrap();
        fs::create_dir_all(r.join("deep/x/y")).unwrap();
        fs::write(r.join("deep/x/y/f"), b"x").unwrap();
        fs::create_dir_all(r.join("deep/z")).unwrap();
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let got = find_empty_dirs(&[r.to_path_buf()], &JobCtx::new(&cancel, &progress)).unwrap();
        assert_eq!(
            got,
            vec![
                r.join("deep").join("z"),
                r.join("e1"),
                r.join("full").join("e4")
            ]
        );
    }
}
