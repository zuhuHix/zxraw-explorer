//! Files dragged in from other applications (the Desktop, Explorer, a mail client).
//!
//! winit's OLE drop target always answers the source with `DROPEFFECT_COPY`, so the source
//! never deletes the originals itself: when a drop moves, the move is ours alone and cannot
//! happen twice. (The drag cursor still shows the copy badge; changing it would need our own
//! `IDropTarget`.)
//!
//! During an OLE drag the mouse belongs to the drag source, so the window gets no pointer
//! moves. While files hover or drop, the cursor position is read from the OS and fed to egui
//! as a pointer move, so the rows under it light up and take the drop like an in-app drag.

use std::path::PathBuf;

/// Files dropped onto the window this frame, with the keys held at the drop. `None` keys
/// mean "use egui's modifiers" (the automation hook injects those).
#[derive(Clone, Debug)]
pub struct External {
    pub paths: Vec<PathBuf>,
    keys: Option<(bool, bool)>,
}

impl External {
    /// Ctrl and Shift held at the drop.
    pub fn keys(&self, ctx: &egui::Context) -> (bool, bool) {
        self.keys
            .unwrap_or_else(|| ctx.input(|i| (i.modifiers.command, i.modifiers.shift)))
    }
}

#[derive(Clone, Default)]
struct Slot {
    hovering: bool,
    dropped: Option<External>,
}

fn slot_id() -> egui::Id {
    egui::Id::new("dnd_external_slot")
}

/// Called from the raw input hook every frame. `from_os` is false when the automation hook
/// drives the window: then its injected pointer and modifiers are used as they are.
pub fn raw_input(ctx: &egui::Context, raw: &mut egui::RawInput, from_os: bool) {
    let hovering = !raw.hovered_files.is_empty();
    let paths: Vec<PathBuf> = raw
        .dropped_files
        .iter()
        .map(|f| f.path().to_path_buf())
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    let mut keys = None;
    if from_os && (hovering || !paths.is_empty()) {
        if let Some(pos) = cursor_pos(ctx, raw) {
            raw.events.push(egui::Event::PointerMoved(pos));
        }
        keys = os_keys();
    }
    let dropped = (!paths.is_empty()).then_some(External { paths, keys });
    ctx.data_mut(|d| d.insert_temp(slot_id(), Slot { hovering, dropped }));
}

/// Are files from another application over the window (or being dropped) this frame?
pub fn active(ctx: &egui::Context) -> bool {
    ctx.data(|d| {
        d.get_temp::<Slot>(slot_id())
            .is_some_and(|s| s.hovering || s.dropped.is_some())
    })
}

/// Takes this frame's drop, once: the first drop target under the pointer gets it.
pub fn take(ctx: &egui::Context) -> Option<External> {
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Slot>(slot_id()).dropped.take())
}

/// The OS cursor in window points.
#[cfg(windows)]
fn cursor_pos(ctx: &egui::Context, raw: &egui::RawInput) -> Option<egui::Pos2> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let info = raw.viewports.get(&raw.viewport_id)?;
    let inner = info.inner_rect?;
    let native = info.native_pixels_per_point?;
    let mut point = POINT::default();
    // SAFETY: `point` is a valid out pointer for the call.
    unsafe { GetCursorPos(&mut point) }.ok()?;
    let ppp = native * ctx.zoom_factor();
    Some(egui::pos2(point.x as f32 / ppp, point.y as f32 / ppp) - inner.min.to_vec2())
}

#[cfg(not(windows))]
fn cursor_pos(_ctx: &egui::Context, _raw: &egui::RawInput) -> Option<egui::Pos2> {
    None
}

/// Ctrl and Shift as the keyboard has them now (egui's modifiers go stale during an OLE drag).
#[cfg(windows)]
fn os_keys() -> Option<(bool, bool)> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_SHIFT};
    // SAFETY: plain key state queries.
    let down = |vk: i32| unsafe { GetAsyncKeyState(vk) } < 0;
    Some((down(i32::from(VK_CONTROL.0)), down(i32::from(VK_SHIFT.0))))
}

#[cfg(not(windows))]
fn os_keys() -> Option<(bool, bool)> {
    None
}
