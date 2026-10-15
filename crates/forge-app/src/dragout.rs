//! Starts an OS drag when an in-app file drag reaches the window edge.

use crate::app::ZxrawApp;

const EDGE_SLOP_POINTS: f32 = 4.0;
/// When the pointer leaves the window, egui forgets its position. A fast drag can jump
/// past the edge band between two frames, so the last position seen inside only has to be
/// this close to an edge for the exit to count.
const EXIT_MARGIN_POINTS: f32 = 64.0;
/// Each local dragged path costs a shell parse (~4 ms) on the UI thread before the modal
/// drag starts (no stat when the listing knows its kind), so very large selections are
/// refused rather than freezing the window. Network paths cost only string copying.
const MAX_DRAG_OUT_ITEMS: usize = 500;

/// Called every frame after the app has drawn its widgets.
pub fn tick(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(payload) = egui::DragAndDrop::payload::<crate::views::DragPaths>(ctx) else {
        return;
    };
    let last_id = egui::Id::new("dragout-last-pointer");
    let (pointer, window, primary_down, pointer_gone) = ctx.input(|input| {
        (
            input.pointer.latest_pos(),
            input.content_rect(),
            input.pointer.button_down(egui::PointerButton::Primary),
            input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::PointerGone)),
        )
    });
    let last_inside = match pointer {
        Some(pos) => {
            ctx.data_mut(|d| d.insert_temp(last_id, pos));
            Some(pos)
        }
        None => ctx.data(|d| d.get_temp::<egui::Pos2>(last_id)),
    };
    if !should_start_os_drag(
        true,
        pointer.or(last_inside),
        window,
        primary_down,
        pointer_gone,
    ) {
        return;
    }

    let paths = payload.0.clone();
    let kinds = payload.1.clone();
    egui::DragAndDrop::clear_payload(ctx);
    ctx.data_mut(|d| d.remove::<egui::Pos2>(last_id));
    if paths.len() > MAX_DRAG_OUT_ITEMS {
        app.toast_err(format!(
            "Drag out at most {MAX_DRAG_OUT_ITEMS} items at a time ({} selected); \
             use Copy and paste for larger selections",
            paths.len()
        ));
        return;
    }
    // The OLE drag is modal; a scripted run has no one to drop it and would hang.
    if app.testhook.active() {
        tracing::info!(
            "automated run: not starting an OS drag-out of {} item(s)",
            paths.len()
        );
        return;
    }
    let entry = paths.first().and_then(|path| {
        app.pane()
            .entries
            .iter()
            .find(|entry| entry.path == *path && entry.kind == forge_core::EntryKind::File)
            .cloned()
    });
    let thumbnail = entry
        .as_ref()
        .and_then(|entry| app.ops.thumbs.cached_drag_thumbnail(entry));
    let drag_thumbnail = thumbnail
        .as_deref()
        .map(|image| forge_win::dragout::DragThumbnail {
            width: image.width,
            height: image.height,
            rgba: &image.rgba,
        });
    // This runs on the UI thread, so nothing here may wait on a share. Each item carries the
    // kind the pane listing showed, so forge-win never stats it, and a network flag (UNC, or a
    // mapped drive by its drive type, which Windows answers locally). If any item is on the
    // network, forge-win builds a CF_HDROP-only data object from the path strings, with no
    // shell resolution, and uses the cached thumbnail or a stock icon as the drag image.
    let items = drag_items(&paths, &kinds);
    match forge_win::dragout::active_window() {
        Ok(hwnd) => {
            if let Err(error) =
                forge_win::dragout::drag_out_items(hwnd, &items, false, drag_thumbnail)
            {
                tracing::warn!("OS drag-out failed: {error}");
            }
            // Re-list the source pane after the modal OLE drag returns. MOVE is disabled by
            // default, but a target can still alter files independently while the drag runs.
            app.pane_mut().refresh();
        }
        Err(error) => tracing::warn!("could not find the app window for drag-out: {error}"),
    }
}

/// Pairs each dragged path with the kind its listing showed and whether it is on the network.
/// A symlink or junction counts as known (the listing saw it), so it is not statted either.
fn drag_items(
    paths: &[std::path::PathBuf],
    kinds: &[Option<forge_core::EntryKind>],
) -> Vec<forge_win::dragout::DragItem> {
    paths
        .iter()
        .enumerate()
        .map(|(index, path)| forge_win::dragout::DragItem {
            path: path.clone(),
            is_dir: kinds
                .get(index)
                .copied()
                .flatten()
                .map(|kind| kind == forge_core::EntryKind::Dir),
            on_network: forge_core::netpath::is_network_path(path),
        })
        .collect()
}

/// Pure trigger decision used by the UI and its regression test.
fn should_start_os_drag(
    has_drag_paths: bool,
    pointer: Option<egui::Pos2>,
    window: egui::Rect,
    primary_down: bool,
    pointer_gone: bool,
) -> bool {
    if !has_drag_paths || !primary_down {
        return false;
    }
    let Some(pointer) = pointer else {
        return false;
    };
    // The pointer left the window: count it only if it was last seen near an edge, so a
    // spurious `PointerGone` mid-window never starts a modal OS drag.
    if pointer_gone {
        return !window.shrink(EXIT_MARGIN_POINTS).contains(pointer);
    }
    !window.contains(pointer)
        || pointer.x <= window.left() + EDGE_SLOP_POINTS
        || pointer.x >= window.right() - EDGE_SLOP_POINTS
        || pointer.y <= window.top() + EDGE_SLOP_POINTS
        || pointer.y >= window.bottom() - EDGE_SLOP_POINTS
}

#[cfg(test)]
mod tests {
    use super::{drag_items, should_start_os_drag};

    #[test]
    fn drag_items_carry_listed_kinds_and_flag_unc_paths() {
        use forge_core::EntryKind;
        use std::path::PathBuf;

        let paths = vec![
            PathBuf::from(r"\\nas\share\a.jpg"),
            PathBuf::from(r"C:\photos\album"),
            PathBuf::from(r"C:\photos\b.jpg"),
        ];
        let items = drag_items(&paths, &[Some(EntryKind::File), Some(EntryKind::Dir)]);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].is_dir, Some(false));
        assert!(items[0].on_network);
        assert_eq!(items[1].is_dir, Some(true));
        // A missing kind stays unknown rather than guessed.
        assert_eq!(items[2].is_dir, None);
    }

    fn window() -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 600.0))
    }

    #[test]
    fn drag_out_only_triggers_for_file_payload_with_button_at_or_outside_edge() {
        let rect = window();
        assert!(!should_start_os_drag(
            true,
            Some(egui::pos2(400.0, 300.0)),
            rect,
            true,
            false,
        ));
        assert!(!should_start_os_drag(
            false,
            Some(egui::pos2(1.0, 300.0)),
            rect,
            true,
            false,
        ));
        assert!(!should_start_os_drag(
            true,
            Some(egui::pos2(1.0, 300.0)),
            rect,
            false,
            false,
        ));
        assert!(should_start_os_drag(
            true,
            Some(egui::pos2(3.0, 300.0)),
            rect,
            true,
            false,
        ));
        assert!(should_start_os_drag(
            true,
            Some(egui::pos2(-1.0, 300.0)),
            rect,
            true,
            false,
        ));
        // Pointer gone: only when last seen near an edge.
        assert!(should_start_os_drag(
            true,
            Some(egui::pos2(20.0, 300.0)),
            rect,
            true,
            true
        ));
        assert!(!should_start_os_drag(
            true,
            Some(egui::pos2(400.0, 300.0)),
            rect,
            true,
            true
        ));
        assert!(!should_start_os_drag(true, None, rect, true, true));
        assert!(!should_start_os_drag(
            true,
            Some(egui::pos2(20.0, 300.0)),
            rect,
            false,
            true
        ));
    }
}
