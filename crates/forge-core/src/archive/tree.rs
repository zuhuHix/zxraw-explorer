//! Folder view of an archive listing, for browsing an archive like a folder. Archives often
//! leave out entries for their folders; those are implied by the paths of the files.

use std::collections::HashMap;
use std::time::SystemTime;

use super::{ArchiveEntry, ArchiveListing};

/// A file or folder inside an archive.
#[derive(Debug, Clone)]
pub struct ArchiveNode {
    pub name: String,
    /// Inner path, `/`-separated, without a trailing `/`.
    pub path: String,
    pub is_dir: bool,
    /// For folders: the total of everything inside.
    pub size: u64,
    pub compressed_size: u64,
    pub modified: Option<SystemTime>,
    pub encrypted: bool,
    /// Compression method of a file ("" for folders).
    pub method: String,
    /// The listing entry, if the node has one (implied folders don't).
    pub entry: Option<usize>,
    /// A file sharing a solid 7z block with others: `compressed_size` is its share.
    pub solid: bool,
    /// Bytes decoded before this file's data in its block (see `ArchiveEntry`).
    pub block_offset: u64,
    /// Bytes decoded to its block's end before it is published (see `ArchiveEntry`).
    pub block_end: u64,
}

impl ArchiveNode {
    /// Bytes decoded to read this file: its block up to it, then its own data (or the
    /// whole block, when its checksum is checked before the file is published).
    pub fn decode_cost(&self) -> u64 {
        self.block_offset
            .saturating_add(self.size)
            .max(self.block_end)
    }

    /// `compressed / size` in percent, for files with data.
    pub fn ratio_percent(&self) -> Option<u32> {
        (self.size > 0)
            .then(|| ((self.compressed_size as f64 / self.size as f64) * 100.0).round() as u32)
    }
}

/// An archive listing indexed by folder.
#[derive(Debug)]
pub struct ArchiveIndex {
    pub listing: ArchiveListing,
    nodes: Vec<ArchiveNode>,
    /// Node of each path, by lowercase path (`""` is the root and has no node).
    by_path: HashMap<String, usize>,
    /// Children of each folder, by lowercase folder path (`""` for the root).
    children: HashMap<String, Vec<usize>>,
    /// Entries left out because their names are unsafe (they are never extracted).
    pub unsafe_entries: usize,
}

/// Heap memory of a `HashMap` with `capacity` and entries of `slot` bytes, roughly: its
/// buckets (a power of two, at most 7/8 full) and their control bytes.
fn map_bytes(capacity: usize, slot: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = (capacity.saturating_mul(8) / 7).next_power_of_two();
    buckets * (slot + 1) + 16
}

impl ArchiveIndex {
    /// An upper estimate of the memory the index holds (for caches): its listing, its nodes
    /// and both maps, with every string, vector and map counted by capacity.
    pub fn memory_estimate(&self) -> u64 {
        use std::mem::size_of;
        let entries = self.listing.entries.capacity() * size_of::<ArchiveEntry>()
            + self
                .listing
                .entries
                .iter()
                .map(|e| {
                    e.name.capacity()
                        + e.method.capacity()
                        + e.skip.as_ref().map_or(0, String::capacity)
                })
                .sum::<usize>()
            + self.listing.comment.capacity();
        let nodes = self.nodes.capacity() * size_of::<ArchiveNode>()
            + self
                .nodes
                .iter()
                .map(|n| n.name.capacity() + n.path.capacity() + n.method.capacity())
                .sum::<usize>();
        let by_path = map_bytes(self.by_path.capacity(), size_of::<(String, usize)>())
            + self.by_path.keys().map(String::capacity).sum::<usize>();
        let children = map_bytes(self.children.capacity(), size_of::<(String, Vec<usize>)>())
            + self
                .children
                .iter()
                .map(|(k, v)| k.capacity() + v.capacity() * size_of::<usize>())
                .sum::<usize>();
        // Allocator bookkeeping and rounding, per allocation.
        let allocations = self.listing.entries.len() * 2
            + self.nodes.len() * 3
            + self.by_path.len()
            + self.children.len() * 2;
        (size_of::<Self>() + entries + nodes + by_path + children + allocations * 16) as u64
    }

    pub fn build(listing: ArchiveListing) -> Self {
        let mut idx = Self {
            listing: ArchiveListing {
                format: listing.format,
                entries: Vec::new(),
                comment: String::new(),
            },
            nodes: Vec::new(),
            by_path: HashMap::new(),
            children: HashMap::new(),
            unsafe_entries: 0,
        };
        idx.children.insert(String::new(), Vec::new());
        for (i, e) in listing.entries.iter().enumerate() {
            let path = e.name.trim_matches('/');
            if path.is_empty() {
                continue;
            }
            if !e.safe_name {
                idx.unsafe_entries += 1;
                continue;
            }
            idx.add(path, e, i);
        }
        // Folder totals: add every file to each of its ancestors.
        let files: Vec<(String, u64, u64)> = idx
            .nodes
            .iter()
            .filter(|n| !n.is_dir)
            .map(|n| (n.path.clone(), n.size, n.compressed_size))
            .collect();
        for (path, size, comp) in files {
            let mut cur = path.as_str();
            while let Some((parent, _)) = cur.rsplit_once('/') {
                if let Some(&n) = idx.by_path.get(&parent.to_lowercase()) {
                    idx.nodes[n].size += size;
                    idx.nodes[n].compressed_size += comp;
                }
                cur = parent;
            }
        }
        for kids in idx.children.values_mut() {
            kids.sort_by_key(|&n| idx.nodes[n].name.to_lowercase());
        }
        idx.listing = listing;
        idx
    }

    fn add(&mut self, path: &str, e: &ArchiveEntry, entry: usize) {
        let parent = self.ensure_dir(path.rsplit_once('/').map_or("", |(p, _)| p));
        let key = path.to_lowercase();
        if let Some(&existing) = self.by_path.get(&key) {
            // A folder implied earlier gets its own entry's details; a duplicate file keeps
            // the first one (as extraction does).
            let node = &mut self.nodes[existing];
            if node.is_dir && node.entry.is_none() && e.is_dir {
                node.entry = Some(entry);
                node.modified = e.modified;
            }
            return;
        }
        let name = path.rsplit_once('/').map_or(path, |(_, n)| n).to_owned();
        let n = self.nodes.len();
        self.nodes.push(ArchiveNode {
            name,
            path: path.to_owned(),
            is_dir: e.is_dir,
            size: if e.is_dir { 0 } else { e.size },
            compressed_size: if e.is_dir { 0 } else { e.compressed_size },
            modified: e.modified,
            encrypted: e.encrypted,
            method: if e.is_dir {
                String::new()
            } else {
                e.method.clone()
            },
            entry: Some(entry),
            solid: e.solid && !e.is_dir,
            block_offset: e.block_offset,
            block_end: e.block_end,
        });
        self.by_path.insert(key.clone(), n);
        if e.is_dir {
            self.children.entry(key).or_default();
        }
        self.children.entry(parent).or_default().push(n);
    }

    /// Make sure folder `path` exists (implied if needed); returns its lowercase key.
    fn ensure_dir(&mut self, path: &str) -> String {
        let key = path.to_lowercase();
        if path.is_empty() || self.by_path.contains_key(&key) {
            return key;
        }
        let parent = self.ensure_dir(path.rsplit_once('/').map_or("", |(p, _)| p));
        let n = self.nodes.len();
        self.nodes.push(ArchiveNode {
            name: path.rsplit_once('/').map_or(path, |(_, n)| n).to_owned(),
            path: path.to_owned(),
            is_dir: true,
            size: 0,
            compressed_size: 0,
            modified: None,
            encrypted: false,
            method: String::new(),
            entry: None,
            solid: false,
            block_offset: 0,
            block_end: 0,
        });
        self.by_path.insert(key.clone(), n);
        self.children.entry(key.clone()).or_default();
        self.children.entry(parent).or_default().push(n);
        key
    }

    /// The node at an inner path (case-insensitive); `None` for the root or a missing path.
    pub fn node(&self, path: &str) -> Option<&ArchiveNode> {
        let key = path.trim_matches('/').to_lowercase();
        self.by_path.get(&key).map(|&n| &self.nodes[n])
    }

    /// Whether `path` is the root or a folder of the archive.
    pub fn is_dir(&self, path: &str) -> bool {
        let key = path.trim_matches('/');
        key.is_empty() || self.node(key).is_some_and(|n| n.is_dir)
    }

    /// The children of folder `path` (`""` for the root), sorted by name; `None` if `path`
    /// is not a folder of the archive.
    pub fn children(&self, path: &str) -> Option<Vec<&ArchiveNode>> {
        let key = path.trim_matches('/').to_lowercase();
        self.children
            .get(&key)
            .map(|kids| kids.iter().map(|&n| &self.nodes[n]).collect())
    }

    /// The listing entry of a node.
    pub fn entry(&self, node: &ArchiveNode) -> Option<&ArchiveEntry> {
        node.entry.and_then(|i| self.listing.entries.get(i))
    }

    /// Every node, in listing order (folders implied by paths last).
    pub fn nodes(&self) -> &[ArchiveNode] {
        &self.nodes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ArchiveFormat;

    fn entry(name: &str, size: u64) -> ArchiveEntry {
        ArchiveEntry {
            index: 0,
            name: name.into(),
            is_dir: name.ends_with('/'),
            is_symlink: false,
            encrypted: false,
            encryption: None,
            size,
            compressed_size: size / 2,
            modified: None,
            crc32: 0,
            method: "Deflate".into(),
            safe_name: !name.contains(".."),
            ..Default::default()
        }
    }

    #[test]
    fn implied_folders_totals_and_lookup() {
        let idx = ArchiveIndex::build(ArchiveListing {
            format: ArchiveFormat::Zip,
            entries: vec![
                entry("b/c/deep.txt", 10),
                entry("a.txt", 4),
                entry("B/", 0),
                entry("b/two.txt", 6),
                entry("../evil", 1),
            ],
            comment: String::new(),
        });
        assert_eq!(idx.unsafe_entries, 1);
        let root: Vec<&str> = idx
            .children("")
            .unwrap()
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(root, ["a.txt", "b"]);
        let b = idx.node("B").unwrap();
        assert!(b.is_dir);
        assert_eq!(b.size, 16);
        assert!(b.entry.is_some(), "the explicit folder entry is attached");
        let kids: Vec<&str> = idx
            .children("b/")
            .unwrap()
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(kids, ["c", "two.txt"]);
        assert!(idx.is_dir("b/c") && idx.is_dir("") && !idx.is_dir("a.txt"));
        assert!(idx.children("a.txt").is_none() && idx.children("nope").is_none());
        assert_eq!(idx.node("b/c/deep.txt").unwrap().ratio_percent(), Some(50));
    }
}
