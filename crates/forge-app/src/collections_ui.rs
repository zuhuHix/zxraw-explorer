//! Collections & saved searches panel (WP-L8). Owned by its sub-agent; the lead wires it in.

use crate::app::ZxrawApp;

#[derive(Default)]
pub struct CollectionsUi {
    pub open: bool,
}

/// Called every frame.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    let _ = (app, ctx);
}

/// `Cmd::AddToCollection`: add the current selection to a collection (picker).
pub fn add_selection(app: &mut ZxrawApp) {
    let _ = app;
}
