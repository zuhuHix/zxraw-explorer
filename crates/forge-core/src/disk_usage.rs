//! Recursive folder size analysis ("what is using my disk?").
//!
//! Sizes are *logical* file sizes (what Explorer shows as "Size"), not allocated space:
//! compressed/sparse files and cloud placeholders may occupy less on disk. Links/junctions
//! are counted (as `links_skipped`) but never followed, so nothing is counted twice and
//! scans cannot loop.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::ops::{JobCtx, OpIssue, WalkItem, walk_tree};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageOptions {
    /// How many of the largest files to keep.
    pub top_n: usize,
    /// Max error samples kept (all errors are counted).
    pub max_error_samples: usize,
}

impl Default for UsageOptions {
    fn default() -> Self {
        Self {
            top_n: 100,
            max_error_samples: 200,
        }
    }
}

/// One folder of the usage tree. Totals are recursive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageNode {
    pub path: PathBuf,
    pub name: String,
    pub parent: Option<usize>,
    /// Child folder indices, largest first.
    pub children: Vec<usize>,
    /// Total bytes of all files below.
    pub size: u64,
    /// Bytes of files directly in this folder.
    pub own_size: u64,
    pub files: u64,
    pub dirs: u64,
    /// Unreadable entries inside this folder (direct).
    pub errors: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtUsage {
    /// Lowercase extension without dot ("" = none).
    pub ext: String,
    pub bytes: u64,
    pub files: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageReport {
    /// Flat tree; index 0 is the root. Children always have larger indices than parents.
    pub nodes: Vec<UsageNode>,
    /// Largest files, largest first.
    pub top_files: Vec<(PathBuf, u64)>,
    /// Breakdown by extension, largest first.
    pub by_ext: Vec<ExtUsage>,
    pub links_skipped: u64,
    pub errors: u64,
    pub error_samples: Vec<OpIssue>,
}

impl UsageReport {
    pub fn root(&self) -> &UsageNode {
        &self.nodes[0]
    }
    pub fn children(&self, idx: usize) -> impl Iterator<Item = &UsageNode> {
        self.nodes[idx].children.iter().map(|&c| &self.nodes[c])
    }
}

/// Scan `root` (cancellable; progress counts entries and bytes).
pub fn scan(root: &Path, opts: &UsageOptions, ctx: &JobCtx<'_>) -> Result<UsageReport> {
    let md = std::fs::metadata(root).map_err(|e| CoreError::io(root, e))?;
    if !md.is_dir() {
        return Err(CoreError::Invalid(format!(
            "{} is not a folder",
            root.display()
        )));
    }
    let mut nodes = vec![UsageNode {
        path: root.to_path_buf(),
        name: crate::paths::name_of(root),
        parent: None,
        children: Vec::new(),
        size: 0,
        own_size: 0,
        files: 0,
        dirs: 0,
        errors: 0,
    }];
    let mut index: HashMap<PathBuf, usize> = HashMap::new();
    index.insert(PathBuf::new(), 0);
    let mut top: BinaryHeap<Reverse<(u64, PathBuf)>> = BinaryHeap::new();
    let mut by_ext: HashMap<String, (u64, u64)> = HashMap::new();
    let mut links = 0;
    let mut errors = 0;
    let mut samples = Vec::new();

    walk_tree(root, ctx.cancel, |item| match item {
        WalkItem::Entry(e) => {
            ctx.progress.add_items(1);
            let parent_rel = e.rel.parent().unwrap_or(Path::new(""));
            let parent = index.get(parent_rel).copied().unwrap_or(0);
            if e.is_dir() {
                let idx = nodes.len();
                nodes.push(UsageNode {
                    path: e.path.clone(),
                    name: crate::paths::name_of(&e.path),
                    parent: Some(parent),
                    children: Vec::new(),
                    size: 0,
                    own_size: 0,
                    files: 0,
                    dirs: 0,
                    errors: 0,
                });
                nodes[parent].children.push(idx);
                index.insert(e.rel.clone(), idx);
            } else if e.is_link() {
                links += 1;
            } else {
                ctx.progress.add_bytes(e.size);
                let n = &mut nodes[parent];
                n.own_size += e.size;
                n.files += 1;
                let ext = crate::model::ext_of(&e.path);
                let slot = by_ext.entry(ext).or_default();
                slot.0 += e.size;
                slot.1 += 1;
                if opts.top_n > 0 {
                    if top.len() < opts.top_n {
                        top.push(Reverse((e.size, e.path.clone())));
                    } else if top.peek().is_some_and(|Reverse((s, _))| e.size > *s) {
                        top.pop();
                        top.push(Reverse((e.size, e.path.clone())));
                    }
                }
            }
        }
        WalkItem::Error { path, error } => {
            errors += 1;
            let rel = path.strip_prefix(root).unwrap_or(path);
            // Attribute to the folder itself if it is a known node, else to its parent.
            let idx = index
                .get(rel)
                .or_else(|| rel.parent().and_then(|p| index.get(p)))
                .copied()
                .unwrap_or(0);
            nodes[idx].errors += 1;
            if samples.len() < opts.max_error_samples {
                samples.push(OpIssue::new(path, error.to_string()));
            }
        }
    })?;

    // Aggregate bottom-up: children have larger indices than their parents.
    for i in (0..nodes.len()).rev() {
        nodes[i].size += nodes[i].own_size;
        if let Some(p) = nodes[i].parent {
            let (size, files, dirs) = (nodes[i].size, nodes[i].files, nodes[i].dirs);
            let pn = &mut nodes[p];
            pn.size += size;
            pn.files += files;
            pn.dirs += dirs + 1;
        }
    }
    let sizes: Vec<u64> = nodes.iter().map(|n| n.size).collect();
    for n in &mut nodes {
        n.children
            .sort_by(|&a, &b| sizes[b].cmp(&sizes[a]).then(a.cmp(&b)));
    }
    let mut top_files: Vec<(PathBuf, u64)> =
        top.into_iter().map(|Reverse((s, p))| (p, s)).collect();
    top_files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut by_ext: Vec<ExtUsage> = by_ext
        .into_iter()
        .map(|(ext, (bytes, files))| ExtUsage { ext, bytes, files })
        .collect();
    by_ext.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.ext.cmp(&b.ext)));
    Ok(UsageReport {
        nodes,
        top_files,
        by_ext,
        links_skipped: links,
        errors,
        error_samples: samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_jobs::{CancelToken, Progress};
    use std::fs;

    #[test]
    fn tree_totals_top_files_and_extensions() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir_all(r.join("a/b")).unwrap();
        fs::create_dir_all(r.join("c")).unwrap();
        fs::write(r.join("root.txt"), vec![0; 10]).unwrap();
        fs::write(r.join("a/x.JPG"), vec![0; 100]).unwrap();
        fs::write(r.join("a/b/y.jpg"), vec![0; 1000]).unwrap();
        fs::write(r.join("a/b/z.mov"), vec![0; 50]).unwrap();
        fs::write(r.join("c/noext"), vec![0; 5]).unwrap();
        let cancel = CancelToken::new();
        let progress = Progress::default();
        let opts = UsageOptions {
            top_n: 2,
            ..Default::default()
        };
        let rep = scan(r, &opts, &JobCtx::new(&cancel, &progress)).unwrap();
        let root = rep.root();
        assert_eq!(root.size, 1165);
        assert_eq!(root.files, 5);
        assert_eq!(root.dirs, 3);
        assert_eq!(root.own_size, 10);
        let kids: Vec<_> = rep.children(0).map(|n| (n.name.as_str(), n.size)).collect();
        assert_eq!(kids, vec![("a", 1150), ("c", 5)]);
        assert_eq!(
            rep.top_files,
            vec![(r.join("a/b/y.jpg"), 1000), (r.join("a/x.JPG"), 100)]
        );
        assert_eq!(rep.by_ext[0].ext, "jpg");
        assert_eq!(rep.by_ext[0].bytes, 1100);
        assert_eq!(rep.by_ext[0].files, 2);
        assert_eq!(progress.snapshot().bytes_done, 1165);
    }

    #[test]
    fn cancel_scan() {
        let d = tempfile::tempdir().unwrap();
        let c = CancelToken::new();
        c.cancel();
        let p = Progress::default();
        assert!(matches!(
            scan(d.path(), &UsageOptions::default(), &JobCtx::new(&c, &p)),
            Err(CoreError::Cancelled)
        ));
    }
}
