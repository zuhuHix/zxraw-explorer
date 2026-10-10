//! Video workflow UI: preview filmstrip/scrubbing, proxies, clip grouping (WP-L9).
//! Owned by its sub-agent; the lead wires it in.

use forge_core::FileEntry;

use crate::app::ZxrawApp;

#[derive(Default)]
pub struct VideoUi {}

/// Extra preview-panel content for a video file (drawn below the generic info).
pub fn preview(app: &mut ZxrawApp, ui: &mut egui::Ui, entry: &FileEntry) {
    let _ = (app, ui, entry);
}

/// Called every frame (dialogs such as the proxy generator).
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    let _ = (app, ctx);
}

/// `Cmd::GenerateProxies` for the current selection.
pub fn generate_proxies(app: &mut ZxrawApp) {
    let _ = app;
}
