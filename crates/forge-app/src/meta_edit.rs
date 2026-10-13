//! Batch metadata editor: writes keywords, title, caption, creator and copyright to XMP sidecars.

use crate::app::ZxrawApp;

/// `Cmd::EditMetadata`.
pub fn open(_app: &mut ZxrawApp) {}

/// Called every frame by the app shell.
pub fn window(_app: &mut ZxrawApp, _ctx: &egui::Context) {}
