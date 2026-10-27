//! Drag-and-drop decisions shared by the file views and drops from other applications:
//! which items a drop moves, whether it copies or moves, and which folder a row drops into.
use std::path::{Path, PathBuf};

pub mod external;

/// Are both paths on the same drive (or the same UNC share)? Compares the first path
/// component case-insensitively, like Explorer's move-or-copy rule.
pub fn same_volume(a: &Path, b: &Path) -> bool {
    let root = |p: &Path| {
        p.components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
    };
    root(a).is_some() && root(a) == root(b)
}

/// Explorer's rule for a drop: Ctrl forces a copy, Shift forces a move, otherwise items
/// move within one volume and copy across volumes. (Ctrl wins if both are held.) Any
/// source on another volume than `target` makes it a copy.
pub fn drop_is_copy(sources: &[PathBuf], target: &Path, ctrl: bool, shift: bool) -> bool {
    if sources.is_empty() {
        return false;
    }
    if ctrl {
        true
    } else if shift {
        false
    } else {
        !sources.iter().all(|s| same_volume(s, target))
    }
}

/// The dropped items that actually go somewhere: an item dropped onto itself or into the
/// folder it is already in is left alone, and a folder is never moved or copied into itself
/// or one of its own subfolders. Comparisons ignore ASCII case, `/` vs `\` and trailing
/// separators, as Windows paths do.
pub fn drop_sources(paths: &[PathBuf], target: &Path) -> Vec<PathBuf> {
    let t = norm(target);
    paths
        .iter()
        .filter(|s| {
            let ns = norm(s);
            let parent = s.parent().map(norm);
            if ns == t || parent.as_deref() == Some(t.as_str()) {
                return false;
            }
            // The target is the item itself or lies inside it.
            !(t == ns || t.starts_with(&format!("{ns}\\")))
        })
        .cloned()
        .collect()
}

/// The folder a details/grid row drops into: a folder row is its own target; a file row
/// inside an expanded folder (tree depth > 0) drops into that folder (its parent); a
/// top-level file row has no target of its own (the drop falls through to the pane).
pub fn row_drop_target(path: &Path, is_dir: bool, depth: usize) -> Option<PathBuf> {
    if is_dir {
        Some(path.to_path_buf())
    } else if depth > 0 {
        path.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

/// Normal form for comparing Windows paths: lowercase, `\` separators, no trailing `\`.
fn norm(p: &Path) -> String {
    let s = p.to_string_lossy().to_lowercase().replace('/', "\\");
    let trimmed = s.trim_end_matches('\\');
    if trimmed.is_empty() {
        s
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn same_volume_compares_roots() {
        assert!(same_volume(&p(r"C:\a"), &p(r"c:\b")));
        assert!(!same_volume(&p(r"C:\a"), &p(r"D:\a")));
        assert!(same_volume(
            &p(r"\\server\share\a"),
            &p(r"\\SERVER\Share\b")
        ));
        assert!(!same_volume(
            &p(r"\\server\share\a"),
            &p(r"\\server\other\a")
        ));
        assert!(!same_volume(&p(r"\\server\share\a"), &p(r"C:\a")));
    }

    #[test]
    fn drop_is_copy_follows_explorer_rule() {
        let src = vec![p(r"C:\src\a.txt")];
        // Same volume: move.
        assert!(!drop_is_copy(&src, &p(r"C:\dst"), false, false));
        // Cross volume: copy.
        assert!(drop_is_copy(&src, &p(r"D:\dst"), false, false));
        // Ctrl on same volume: copy.
        assert!(drop_is_copy(&src, &p(r"C:\dst"), true, false));
        // Shift across volumes: move.
        assert!(!drop_is_copy(&src, &p(r"D:\dst"), false, true));
        // Ctrl+Shift: copy.
        assert!(drop_is_copy(&src, &p(r"C:\dst"), true, true));
        // One source on another volume makes it a copy.
        let mixed = vec![p(r"C:\src\a.txt"), p(r"D:\src\b.txt")];
        assert!(drop_is_copy(&mixed, &p(r"C:\dst"), false, false));
        // UNC items on the same share move.
        let unc = vec![p(r"\\server\share\a.txt")];
        assert!(!drop_is_copy(&unc, &p(r"\\SERVER\share\dst"), false, false));
        // Empty sources never copy.
        assert!(!drop_is_copy(&[], &p(r"D:\dst"), false, false));
    }

    #[test]
    fn drop_sources_skips_no_op_and_self_drops() {
        let target = p(r"C:\dst");
        // Onto itself.
        assert!(drop_sources(&[p(r"C:\dst")], &target).is_empty());
        // Into its own parent (already there), with different case and slashes.
        assert!(drop_sources(&[p(r"C:\Dst\file.txt")], &p("c:/dst/")).is_empty());
        assert!(drop_sources(&[p("C:/Dst/file.txt")], &target).is_empty());
        // Folder into its own descendant.
        assert!(drop_sources(&[p(r"C:\a")], &p(r"C:\a\sub\deep")).is_empty());
        // Sibling with a shared name prefix is not a descendant.
        assert_eq!(
            drop_sources(&[p(r"C:\a\b")], &p(r"C:\a\bc")),
            vec![p(r"C:\a\b")]
        );
        // Normal item is kept.
        let keep = vec![p(r"C:\src\x.txt")];
        assert_eq!(drop_sources(&keep, &target), keep);
    }

    #[test]
    fn row_drop_target_by_kind_and_depth() {
        assert_eq!(
            row_drop_target(&p(r"C:\a\dir"), true, 0),
            Some(p(r"C:\a\dir"))
        );
        assert_eq!(
            row_drop_target(&p(r"C:\a\dir"), true, 2),
            Some(p(r"C:\a\dir"))
        );
        assert_eq!(row_drop_target(&p(r"C:\a\f.txt"), false, 0), None);
        assert_eq!(
            row_drop_target(&p(r"C:\a\f.txt"), false, 1),
            Some(p(r"C:\a"))
        );
    }
}
