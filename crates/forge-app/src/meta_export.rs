//! Metadata report export to CSV/JSON (wave-2 stub, owned by the media agent).

use crate::app::ZxrawApp;

/// `Cmd::ExportMetadata`.
pub fn open(app: &mut ZxrawApp) {
    app.toast("Metadata export is not available yet");
}

/// Called every frame.
pub fn window(_app: &mut ZxrawApp, _ctx: &egui::Context) {}
