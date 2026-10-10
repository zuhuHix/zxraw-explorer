//! Hosts the full-window media viewer (single / cull / compare) inside the app and maps
//! its annotation edits onto the library database.

use std::path::{Path, PathBuf};

use forge_core::model::{EntryKind, MediaKind};
use forge_core::query::{ColorLabel, PickState};

use crate::app::ZxrawApp;
use crate::library::Library;
use crate::viewer::{self, AnnotationStore, Viewer, ViewerMode, ViewerOutcome};

/// Adapter: viewer annotations ⇄ library (DB-backed) annotations.
struct LibraryStore<'a>(&'a mut Library);

impl AnnotationStore for LibraryStore<'_> {
    fn get(&self, path: &Path) -> viewer::Annotation {
        let a = self.0.annotations.get(path).copied().unwrap_or_default();
        viewer::Annotation {
            rating: a.rating,
            label: a.label,
            pick: a.pick,
        }
    }

    fn set(&mut self, paths: &[PathBuf], change: viewer::AnnotationChange) {
        let change = forge_db::AnnotationChange {
            rating: change.rating,
            label: change
                .label
                .map(|l| l.and_then(|i| ColorLabel::ALL.get(i as usize).copied())),
            pick: change.pick.map(|p| match p {
                1 => PickState::Pick,
                -1 => PickState::Reject,
                _ => PickState::None,
            }),
            ..Default::default()
        };
        self.0.annotate(paths.to_vec(), change);
    }
}

fn is_viewable(kind: MediaKind) -> bool {
    matches!(kind, MediaKind::Image | MediaKind::Raw | MediaKind::Video)
}

/// Open the viewer on the focused pane. `Single`/`Cull`: all viewable files of the folder
/// (in display order), starting at the cursor. `Compare`: the selected viewable files.
pub fn open(app: &mut ZxrawApp, mode: ViewerMode) {
    let pane = app.pane();
    let media: Vec<&forge_core::FileEntry> = pane
        .visible
        .iter()
        .map(|&i| &pane.entries[i])
        .filter(|e| e.kind == EntryKind::File && is_viewable(e.media_kind()))
        .collect();
    let (items, start): (Vec<PathBuf>, usize) = match mode {
        ViewerMode::Compare => {
            let sel: Vec<PathBuf> = media
                .iter()
                .filter(|e| pane.is_selected(e))
                .map(|e| e.path.clone())
                .take(4)
                .collect();
            if sel.len() < 2 {
                app.toast("Select 2–4 photos to compare");
                return;
            }
            (sel, 0)
        }
        _ => {
            let cursor = pane.cursor_entry().map(|e| e.path.clone());
            let start = cursor
                .and_then(|c| media.iter().position(|e| e.path == c))
                .unwrap_or(0);
            (media.iter().map(|e| e.path.clone()).collect(), start)
        }
    };
    if items.is_empty() {
        app.toast("No photos or videos in this folder");
        return;
    }
    let mut cfg = crate::viewer::ViewerConfig::default();
    cfg.budget_bytes = (app.settings.thumb_mem_cache_mb as usize).max(128) * 1024 * 1024;
    if let Some(dir) = &app.settings.ffmpeg_dir {
        cfg.ff = forge_media::FfTools::discover_with(Some(dir));
    }
    app.viewer = Some(Viewer::with_config(items, start, mode, &app.ops.ctx, cfg));
}

/// Draw the viewer if open. Returns true when it occupied the window this frame.
pub fn show(app: &mut ZxrawApp, ui: &mut egui::Ui) -> bool {
    let Some(v) = app.viewer.as_mut() else { return false };
    let mut store = LibraryStore(&mut app.ops.library);
    let outcome = egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ui, |ui| v.show(ui, &mut store))
        .inner;
    if let ViewerOutcome::Close { last_index } = outcome {
        // Return to the browser with the last viewed file under the cursor.
        let last = v.items().get(last_index).cloned();
        app.viewer = None;
        if let Some(p) = last.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())) {
            app.pane_mut().select_name(&p);
        }
    }
    true
}
