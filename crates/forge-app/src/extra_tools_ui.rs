//! Contact sheets, archive browser/extract/create, file split/join (WP-L10).
//! Owned by its sub-agent; the lead wires it in.

use crate::app::ZxrawApp;

#[derive(Default)]
pub struct ExtraToolsUi {}

/// Called every frame.
pub fn window(app: &mut ZxrawApp, ctx: &egui::Context) {
    let _ = (app, ctx);
}

/// `Cmd::ContactSheet` for the selection (or all images in the folder).
pub fn contact_sheet(app: &mut ZxrawApp) {
    let _ = app;
}

/// `Cmd::BrowseArchive` / opening a .zip: show its contents.
pub fn browse_archive(app: &mut ZxrawApp) {
    let _ = app;
}

/// `Cmd::CreateArchive` from the selection.
pub fn create_archive(app: &mut ZxrawApp) {
    let _ = app;
}

/// `Cmd::SplitFile` / `Cmd::JoinFiles`.
pub fn split_file(app: &mut ZxrawApp) {
    let _ = app;
}
pub fn join_files(app: &mut ZxrawApp) {
    let _ = app;
}
