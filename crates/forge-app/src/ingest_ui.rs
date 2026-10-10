//! Media ingest window (WP-L2). Owned by its sub-agent; the lead wires it in.

use crate::app::ZxrawApp;

#[derive(Default)]
pub struct IngestState {
    pub open: bool,
}

pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    let _ = (app, ctx);
}
