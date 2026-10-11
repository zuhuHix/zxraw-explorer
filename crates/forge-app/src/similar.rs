//! Burst and near-duplicate grouping in the browser (wave-2 stub, owned by the browser agent).

use crate::app::ZxrawApp;

/// `Cmd::GroupSimilar`: toggles grouping for the focused pane.
pub fn toggle(app: &mut ZxrawApp) {
    app.toast("Grouping similar photos is not available yet");
}
