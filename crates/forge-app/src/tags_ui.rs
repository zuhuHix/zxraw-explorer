//! Tags/keywords and notes editor for the selection, shown in the preview panel (WP-T).
//! Owned by its sub-agent; the lead wires it in.

use forge_core::FileEntry;

use crate::app::ZxrawApp;

#[derive(Default)]
pub struct TagsUi {}

/// Drawn in the preview panel below the file info for any file.
pub fn panel(app: &mut ZxrawApp, ui: &mut egui::Ui, entry: &FileEntry) {
    let _ = (app, ui, entry);
}
