//! Tells Explorer and the Desktop what a copy, move or delete changed, as Explorer's own file
//! operations do. Without it their windows only learn of our changes through their file
//! system watchers, which can take seconds: an item moved off the Desktop showed up in ZXraw
//! at once but stayed on the Desktop for a while.

use std::path::{Path, PathBuf};

use forge_core::ops::{OpReport, UndoRecord};

/// Per-item notices beyond this many are left to the folder updates (one per folder).
const ITEM_NOTICES_MAX: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellChange {
    Renamed { from: PathBuf, to: PathBuf, is_dir: bool },
    Created { path: PathBuf, is_dir: bool },
    FolderUpdated(PathBuf),
}

/// The notices for `report`: a rename for each item moved by renaming it, a creation for
/// each copy, then an update for each folder in `folders` (the destinations and the
/// sources' folders), which also covers items moved across volumes and deleted ones.
pub fn changes(report: &OpReport, folders: &[PathBuf]) -> Vec<ShellChange> {
    let mut out = Vec::new();
    for record in &report.undo {
        if out.len() >= ITEM_NOTICES_MAX {
            break;
        }
        match record {
            UndoRecord::Moved { from, to } => out.push(ShellChange::Renamed {
                from: from.clone(),
                to: to.clone(),
                is_dir: to.is_dir(),
            }),
            UndoRecord::Copied { path, .. } | UndoRecord::CreatedFile { path } => {
                out.push(ShellChange::Created {
                    path: path.clone(),
                    is_dir: path.is_dir(),
                })
            }
            UndoRecord::CreatedFolder { path } => out.push(ShellChange::Created {
                path: path.clone(),
                is_dir: true,
            }),
            UndoRecord::Renamed { .. } => {}
        }
    }
    let mut seen: Vec<&Path> = Vec::new();
    for folder in folders {
        if !seen.iter().any(|s| forge_core::paths::paths_equal(s, folder)) {
            seen.push(folder);
            out.push(ShellChange::FolderUpdated(folder.clone()));
        }
    }
    out
}

/// Sends the notices. They are posted without waiting for delivery.
pub fn announce(changes: &[ShellChange]) {
    use forge_win::shell_notify as notify;
    for change in changes {
        match change {
            ShellChange::Renamed { from, to, is_dir } => notify::notify_renamed(from, to, *is_dir),
            ShellChange::Created { path, is_dir } => notify::notify_created(path, *is_dir),
            ShellChange::FolderUpdated(dir) => notify::notify_dir_updated(dir),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug 2: a same-volume move off the Desktop is announced as a rename of the item and an
    /// update of both folders, so the Desktop drops the item at once.
    #[test]
    fn a_move_by_rename_is_announced_as_a_rename_and_both_folders() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = dir.path().join("Desktop");
        let work = dir.path().join("work");
        std::fs::create_dir_all(work.join("Shoot")).unwrap();
        std::fs::create_dir_all(&desktop).unwrap();
        std::fs::write(work.join("a.txt"), b"x").unwrap();
        let report = OpReport {
            undo: vec![
                UndoRecord::Moved {
                    from: desktop.join("a.txt"),
                    to: work.join("a.txt"),
                },
                UndoRecord::Moved {
                    from: desktop.join("Shoot"),
                    to: work.join("Shoot"),
                },
            ],
            ..OpReport::default()
        };
        let folders = [work.clone(), desktop.clone(), desktop.clone()];
        assert_eq!(
            changes(&report, &folders),
            vec![
                ShellChange::Renamed {
                    from: desktop.join("a.txt"),
                    to: work.join("a.txt"),
                    is_dir: false
                },
                ShellChange::Renamed {
                    from: desktop.join("Shoot"),
                    to: work.join("Shoot"),
                    is_dir: true
                },
                ShellChange::FolderUpdated(work),
                ShellChange::FolderUpdated(desktop),
            ]
        );
    }

    #[test]
    fn many_items_fall_back_to_folder_updates() {
        let report = OpReport {
            undo: (0..1000)
                .map(|i| UndoRecord::CreatedFolder {
                    path: PathBuf::from(format!(r"C:\nowhere\{i}")),
                })
                .collect(),
            ..OpReport::default()
        };
        let out = changes(&report, &[PathBuf::from(r"C:\nowhere")]);
        assert_eq!(out.len(), ITEM_NOTICES_MAX + 1);
        assert_eq!(
            out.last(),
            Some(&ShellChange::FolderUpdated(PathBuf::from(r"C:\nowhere")))
        );
    }
}
