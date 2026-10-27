//! Drag-and-drop decisions shared by the file views, the sidebar and drops from other
//! applications: which items a drop moves, whether it copies or moves, and which folder a
//! row drops into.
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf, Prefix};

use forge_core::paths::{is_within, paths_equal};

pub mod external;

/// One folder drop target, for in-app drags and for files dragged in from other
/// applications. Returns whether something is held over `resp`, and the transfer to start
/// (`paths`, `copy`) if it was dropped onto it this frame and is not a no-op.
pub fn folder_drop(resp: &egui::Response, target: &Path) -> (bool, Option<(Vec<PathBuf>, bool)>) {
    let from_outside = resp.contains_pointer() && external::active(&resp.ctx);
    let hovering = from_outside
        || resp
            .dnd_hover_payload::<crate::views::DragPaths>()
            .is_some();
    let dropped = if let Some(p) = resp.dnd_release_payload::<crate::views::DragPaths>() {
        let keys = resp.ctx.input(|i| (i.modifiers.command, i.modifiers.shift));
        Some((p.0.clone(), keys))
    } else if from_outside {
        external::take(&resp.ctx).map(|drop| {
            let keys = drop.keys(&resp.ctx);
            (drop.paths, keys)
        })
    } else {
        None
    };
    let transfer = dropped.and_then(|(paths, (ctrl, shift))| {
        let paths = drop_sources(&paths, target);
        if paths.is_empty() {
            return None;
        }
        let copy = drop_is_copy(&paths, target, ctrl, shift);
        Some((paths, copy))
    });
    (hovering, transfer)
}

/// The volume identity of an existing path (see `forge_win::files::volume_identity`), or
/// `None` when it cannot be determined.
fn os_volume(path: &Path) -> Option<String> {
    forge_win::files::volume_identity(path).ok()
}

/// Are both paths on the same volume? Folder mount points count (a disk mounted
/// at `C:\Mount` is not `C:`); a volume that cannot be determined counts as another one.
pub fn same_volume(a: &Path, b: &Path) -> bool {
    same_volume_with(a, b, &mut os_volume)
}

fn same_volume_with(a: &Path, b: &Path, volume: &mut dyn FnMut(&Path) -> Option<String>) -> bool {
    // Different drive letters or shares are taken as different volumes without asking the
    // disk. (A `subst` drive on the same volume then copies, which is the safe side.)
    let root = root_key(a);
    if root.is_none() || root != root_key(b) {
        return false;
    }
    matches!((volume(a), volume(b)), (Some(x), Some(y)) if x == y)
}

/// Explorer's rule for a drop: Ctrl forces a copy, Shift forces a move, otherwise items
/// move within one volume and copy across volumes. (Ctrl wins if both are held.) Any
/// source on another volume than `target`, or whose volume is unknown, makes it a copy.
pub fn drop_is_copy(sources: &[PathBuf], target: &Path, ctrl: bool, shift: bool) -> bool {
    drop_is_copy_with(sources, target, ctrl, shift, &mut os_volume)
}

fn drop_is_copy_with(
    sources: &[PathBuf],
    target: &Path,
    ctrl: bool,
    shift: bool,
    volume: &mut dyn FnMut(&Path) -> Option<String>,
) -> bool {
    if sources.is_empty() {
        return false;
    }
    if ctrl {
        return true;
    }
    if shift {
        return false;
    }
    // An item moves by renaming its entry in the folder that holds it, so its volume is
    // its folder's (a mount point folder itself still moves within its parent's volume).
    // Each folder is asked once.
    let mut cache: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut cached = |p: &Path| {
        cache
            .entry(p.to_path_buf())
            .or_insert_with(|| volume(p))
            .clone()
    };
    !sources.iter().all(|source| {
        let folder = source.parent().unwrap_or(source);
        same_volume_with(folder, target, &mut cached)
    })
}

/// The dropped items that actually go somewhere: an item dropped onto itself or into the
/// folder it is already in is left alone, and a folder is never moved or copied into itself
/// or one of its own subfolders. Paths compare as Windows does (case, `/` and `\`, trailing
/// separators, `\\?\` prefixes).
pub fn drop_sources(paths: &[PathBuf], target: &Path) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|source| {
            let in_place = source
                .parent()
                .is_some_and(|parent| paths_equal(parent, target));
            !in_place && !is_within(source, target)
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

/// The drive or share a path starts with, lowercased, with `\\?\` forms unified
/// (`\\?\C:` is `c:`, `\\?\UNC\srv\share` is `\\srv\share`).
fn root_key(p: &Path) -> Option<String> {
    let Some(Component::Prefix(prefix)) = p.components().next() else {
        return None;
    };
    let key = match prefix.kind() {
        Prefix::Disk(d) | Prefix::VerbatimDisk(d) => format!("{}:", d as char),
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
            r"\\{}\{}",
            server.to_string_lossy(),
            share.to_string_lossy()
        ),
        _ => prefix.as_os_str().to_string_lossy().into_owned(),
    };
    Some(key.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// A stand-in for the disk: `C:\Mount` holds another volume, X: cannot be identified,
    /// shares are their own volumes.
    fn fake_volume(path: &Path) -> Option<String> {
        let root = root_key(path)?;
        if root == "x:" {
            None
        } else if root == "c:" && is_within(&p(r"C:\Mount"), path) {
            Some("mounted".into())
        } else {
            Some(root)
        }
    }

    fn copies(sources: &[PathBuf], target: &str, ctrl: bool, shift: bool) -> bool {
        drop_is_copy_with(sources, &p(target), ctrl, shift, &mut fake_volume)
    }

    #[test]
    fn same_volume_compares_roots_then_asks_the_disk() {
        let mut v = fake_volume;
        assert!(same_volume_with(&p(r"C:\a"), &p(r"c:\b"), &mut v));
        assert!(!same_volume_with(&p(r"C:\a"), &p(r"D:\a"), &mut v));
        assert!(same_volume_with(
            &p(r"\\server\share\a"),
            &p(r"\\SERVER\Share\b"),
            &mut v
        ));
        assert!(!same_volume_with(
            &p(r"\\server\share\a"),
            &p(r"\\server\other\a"),
            &mut v
        ));
        assert!(!same_volume_with(
            &p(r"\\server\share\a"),
            &p(r"C:\a"),
            &mut v
        ));
        // Verbatim forms are the same roots.
        assert!(same_volume_with(&p(r"\\?\C:\a"), &p(r"C:\b"), &mut v));
        assert!(same_volume_with(
            &p(r"\\?\UNC\srv\share\a"),
            &p(r"\\srv\share\b"),
            &mut v
        ));
        // A disk mounted in a folder is another volume.
        assert!(!same_volume_with(&p(r"C:\a"), &p(r"C:\Mount\b"), &mut v));
        // A volume that cannot be determined is another volume.
        assert!(!same_volume_with(&p(r"X:\a"), &p(r"X:\b"), &mut v));
    }

    #[test]
    fn same_volume_on_the_real_disk() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(same_volume(dir.path(), &sub));
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(!drop_is_copy(&[file], &sub, false, false));
    }

    #[test]
    fn drop_is_copy_follows_explorer_rule() {
        let src = vec![p(r"C:\src\a.txt")];
        // Same volume: move.
        assert!(!copies(&src, r"C:\dst", false, false));
        // Cross volume: copy.
        assert!(copies(&src, r"D:\dst", false, false));
        // Ctrl on same volume: copy.
        assert!(copies(&src, r"C:\dst", true, false));
        // Shift across volumes: move.
        assert!(!copies(&src, r"D:\dst", false, true));
        // Ctrl+Shift: copy.
        assert!(copies(&src, r"C:\dst", true, true));
        // One source on another volume makes it a copy.
        let mixed = vec![p(r"C:\src\a.txt"), p(r"D:\src\b.txt")];
        assert!(copies(&mixed, r"C:\dst", false, false));
        // UNC items on the same share move.
        let unc = vec![p(r"\\server\share\a.txt")];
        assert!(!copies(&unc, r"\\SERVER\share\dst", false, false));
        // Empty sources never copy.
        assert!(!copies(&[], r"D:\dst", false, false));
    }

    #[test]
    fn drop_is_copy_respects_mount_points_and_unknown_volumes() {
        // Into a folder that holds another disk: copy.
        assert!(copies(&[p(r"C:\src\a.txt")], r"C:\Mount\dst", false, false));
        // Out of it onto C:: copy.
        assert!(copies(&[p(r"C:\Mount\a.txt")], r"C:\dst", false, false));
        // The mount point folder itself lives on C:, so it moves within C:.
        assert!(!copies(&[p(r"C:\Mount")], r"C:\dst", false, false));
        // Unknown volume: copy, unless Shift asks for a move.
        assert!(copies(&[p(r"X:\src\a.txt")], r"X:\dst", false, false));
        assert!(!copies(&[p(r"X:\src\a.txt")], r"X:\dst", false, true));
        // Verbatim source, plain target, same volume: move.
        assert!(!copies(&[p(r"\\?\C:\src\a.txt")], r"C:\dst", false, false));
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
    fn drop_sources_sees_through_verbatim_prefixes() {
        // Onto itself, own parent and own descendant, with `\\?\` on either side.
        assert!(drop_sources(&[p(r"\\?\C:\dst")], &p(r"C:\dst")).is_empty());
        assert!(drop_sources(&[p(r"C:\dst\a.txt")], &p(r"\\?\C:\dst")).is_empty());
        assert!(drop_sources(&[p(r"\\?\C:\a")], &p(r"C:\a\sub")).is_empty());
        assert!(drop_sources(&[p(r"\\srv\share\a")], &p(r"\\?\UNC\srv\share\a\sub")).is_empty());
        assert!(drop_sources(&[p(r"\\?\UNC\SRV\share\d\x.txt")], &p(r"\\srv\share\d")).is_empty());
        // Another folder is still a real drop.
        assert_eq!(
            drop_sources(&[p(r"\\?\C:\a\x.txt")], &p(r"C:\b")),
            vec![p(r"\\?\C:\a\x.txt")]
        );
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
