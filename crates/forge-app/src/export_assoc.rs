//! Associates exports with their originals by filename rules (wave-2 stub, owned by the
//! related-files agent).

use forge_core::FileEntry;

use crate::app::ZxrawApp;

/// Related originals/exports in the preview panel (nothing yet).
pub fn panel(_app: &mut ZxrawApp, _ui: &mut egui::Ui, _entry: &FileEntry) {}

/// `Cmd::SelectOriginals`.
pub fn select_originals(app: &mut ZxrawApp) {
    app.toast("Selecting originals is not available yet");
}

/// `Cmd::SelectExports`.
pub fn select_exports(app: &mut ZxrawApp) {
    app.toast("Selecting exports is not available yet");
}
