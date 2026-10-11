//! Starts an OS drag when an in-app file drag reaches the window edge.

use crate::app::ZxrawApp;

const EDGE_SLOP_POINTS: f32 = 4.0;

/// Called every frame after the app has drawn its widgets.
pub fn tick(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(payload) = egui::DragAndDrop::payload::<crate::views::DragPaths>(ctx) else {
        return;
    };
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
    if !should_start_os_drag(true, pointer, window, primary_down, pointer_gone) {
        return;
    }

    let paths = payload.0.clone();
    egui::DragAndDrop::clear_payload(ctx);
    // The OLE drag is modal; a scripted run has no one to drop it and would hang.
    if app.testhook.active() {
        tracing::info!("automated run: not starting an OS drag-out of {} item(s)", paths.len());
        return;
    }
    match forge_win::dragout::active_window() {
        Ok(hwnd) => {
            if let Err(error) = forge_win::dragout::drag_out(hwnd, &paths, false) {
                tracing::warn!("OS drag-out failed: {error}");
            }
            // Re-list the source pane after the modal OLE drag returns. MOVE is disabled by
            // default, but a target can still alter files independently while the drag runs.
            app.pane_mut().refresh();
        }
        Err(error) => tracing::warn!("could not find the app window for drag-out: {error}"),
    }
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
    if pointer_gone {
        return true;
    }
    let Some(pointer) = pointer else {
        return false;
    };
    !window.contains(pointer)
        || pointer.x <= window.left() + EDGE_SLOP_POINTS
        || pointer.x >= window.right() - EDGE_SLOP_POINTS
        || pointer.y <= window.top() + EDGE_SLOP_POINTS
        || pointer.y >= window.bottom() - EDGE_SLOP_POINTS
}

#[cfg(test)]
mod tests {
    use super::should_start_os_drag;

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
        assert!(should_start_os_drag(true, None, rect, true, true));
        assert!(!should_start_os_drag(true, None, rect, false, true));
    }
}
