//! Offline GPS panel in the preview (wave-2 stub, owned by the media agent).

use forge_core::FileEntry;

use crate::app::ZxrawApp;

/// Shows the coordinates of `entry` in the preview panel (nothing yet).
pub fn panel(_app: &mut ZxrawApp, _ui: &mut egui::Ui, _entry: &FileEntry) {}
