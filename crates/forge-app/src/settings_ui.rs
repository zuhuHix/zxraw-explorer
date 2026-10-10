//! Settings window and external-tool integration ("Open in Lightroom/Resolve…") (WP-L4).
//! Owned by its sub-agent; the lead wires it in.

use crate::app::ZxrawApp;

#[derive(Default)]
pub struct SettingsUi {
    pub open: bool,
}

pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    let _ = (app, ctx);
}

/// `Cmd::OpenIn`: show the "Open in…" picker for the current selection (external tools
/// such as Lightroom, Photoshop, DaVinci Resolve). Drawn by [`window`].
pub fn open_in(app: &mut ZxrawApp) {
    let _ = app;
}
