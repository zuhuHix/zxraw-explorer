//! Power-user tool windows: folder size, duplicates, checksums, folder compare (WP-L3).
//! Owned by its sub-agent; the lead wires it in.

use std::path::PathBuf;

use crate::app::ZxrawApp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    FolderSize,
    Duplicates,
    Checksums,
    CompareFolders,
}

#[derive(Default)]
pub struct ToolsState {}

impl ToolsState {
    /// Open `tool`. `dir` = focused pane folder, `selection` = selected paths (or empty),
    /// `other_dir` = the other pane's folder in dual-pane mode.
    pub fn open(&mut self, tool: Tool, dir: PathBuf, selection: Vec<PathBuf>, other_dir: Option<PathBuf>) {
        let _ = (tool, dir, selection, other_dir);
    }
}

pub fn windows(app: &mut ZxrawApp, ctx: &egui::Context) {
    let _ = (app, ctx);
}
