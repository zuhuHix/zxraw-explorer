//! Files dragged in from other applications (the Desktop, Explorer, a zip folder, a mail
//! client).
//!
//! The window has our own OLE drop target ([`forge_win::drop_target`], in place of winit's),
//! so drags that carry no file paths can drop too: the items of a zip folder opened in
//! Explorer come as virtual files, which are copied out by [`crate::virtual_copy`]. The drop
//! target answers the source as the drop will act: every frame the app works out what a
//! drop onto the folder under the pointer would do ([`OsDrop::frame`]), so the cursor shows
//! the move or copy badge, or "no drop" onto the items' own folder. When a drop moves, the
//! move is ours alone: the source is told it was an optimized move (`DROPEFFECT_NONE`
//! returned, "Performed DropEffect" = none), so it never deletes anything itself, and the
//! move cannot happen twice.
//!
//! During an OLE drag the mouse belongs to the drag source, so the window gets no pointer
//! moves. While files hover or drop, the cursor position is read from the OS and fed to egui
//! as a pointer move, so the rows under it light up and take the drop like an in-app drag.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_win::drop_target::{DragInfo, DropEffect, DropEvent, DropTargetHandle};
use forge_win::virtual_files::VirtualSource;

/// Files dropped onto the window this frame, with the keys held at the drop. `None` keys
/// mean "use egui's modifiers" (the automation hook injects those).
#[derive(Clone)]
pub struct External {
    pub paths: Vec<PathBuf>,
    keys: Option<(bool, bool)>,
    /// Set instead of `paths` when virtual files were dropped (items of a zip folder).
    pub virtual_files: Option<VirtualDrop>,
    /// What our drop target told the source the drop does (`None` for the automation
    /// hook's drops, which have no source).
    told: Option<DropEffect>,
}

impl External {
    /// Ctrl and Shift held at the drop. A drop the source was told is a copy counts as one
    /// with Ctrl (a source that allows no move, or a cursor that showed a copy).
    pub fn keys(&self, ctx: &egui::Context) -> (bool, bool) {
        let (ctrl, shift) = self
            .keys
            .unwrap_or_else(|| ctx.input(|i| (i.modifiers.command, i.modifiers.shift)));
        (ctrl || self.told == Some(DropEffect::Copy), shift)
    }

    /// The source was told nothing happens (the cursor showed "no drop"), so nothing does.
    pub fn refused_to_source(&self) -> bool {
        self.told == Some(DropEffect::None)
    }
}

/// Dropped virtual files: how many, and the source to copy them from (taken once).
#[derive(Clone)]
pub struct VirtualDrop {
    pub count: usize,
    source: Arc<parking_lot::Mutex<Option<VirtualSource>>>,
}

impl VirtualDrop {
    pub fn take_source(&self) -> Option<VirtualSource> {
        self.source.lock().take()
    }
}

#[derive(Clone, Default)]
struct Slot {
    hovering: bool,
    dropped: Option<External>,
    /// The folder of the first drop target under the pointer this frame.
    hover_target: Option<PathBuf>,
    /// A virtual-file drop taken by a folder drop target, with that folder.
    claimed: Option<(External, PathBuf)>,
}

fn slot_id() -> egui::Id {
    egui::Id::new("dnd_external_slot")
}

/// The window's own drop target and what it answers the drag source.
#[derive(Default)]
pub struct OsDrop {
    pub handle: Option<DropTargetHandle>,
    /// What the current drag over the window carries (from `DragEnter`).
    drag: Option<DragInfo>,
    /// The automation hook's stand-in for a drag over the window, with its Ctrl and Shift.
    pub simulated: Option<(DragInfo, bool, bool)>,
    /// The last answer, and the target and keys it was worked out for.
    answered: Option<(PathBuf, bool, bool, DropEffect)>,
}

impl OsDrop {
    /// What the drop target answers now (the automation hook drops with it).
    pub fn answer(&self) -> DropEffect {
        self.answered.as_ref().map_or(DropEffect::None, |a| a.3)
    }

    fn dragging(&self) -> Option<&DragInfo> {
        self.simulated
            .as_ref()
            .map(|(info, _, _)| info)
            .or_else(|| self.drag.as_ref().filter(|_| self.is_dragging()))
    }

    fn is_dragging(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(DropTargetHandle::is_dragging)
    }

    /// Called once per frame after the UI ran: works out what a drop onto the hovered folder
    /// (or `fallback`, the focused pane's folder) would do, and tells the drop target.
    pub fn frame(&mut self, ctx: &egui::Context, fallback: &Path) {
        let Some(info) = self.dragging().cloned() else {
            self.answered = None;
            return;
        };
        let target = ctx
            .data(|d| d.get_temp::<Slot>(slot_id()))
            .and_then(|s| s.hover_target)
            .unwrap_or_else(|| fallback.to_path_buf());
        let (ctrl, shift) = match &self.simulated {
            Some((_, ctrl, shift)) => (*ctrl, *shift),
            None => self
                .handle
                .as_ref()
                .map_or((false, false), DropTargetHandle::keys),
        };
        let fresh = self.answered.as_ref().is_some_and(|(t, c, s, _)| {
            forge_core::paths::paths_equal(t, &target) && *c == ctrl && *s == shift
        });
        if !fresh {
            let effect = crate::dnd::drop_effect(&info, &target, ctrl, shift);
            self.answered = Some((target, ctrl, shift, effect));
        }
        if let Some(handle) = &self.handle {
            handle.set_answer(self.answer());
        }
    }
}

/// Called from the raw input hook every frame. `from_os` is false when the automation hook
/// drives the window: then its injected pointer and modifiers are used as they are.
pub fn raw_input(ctx: &egui::Context, raw: &mut egui::RawInput, from_os: bool, os: &mut OsDrop) {
    let mut hovering = !raw.hovered_files.is_empty() || os.simulated.is_some();
    let mut paths: Vec<PathBuf> = raw
        .dropped_files
        .iter()
        .map(|f| f.path().to_path_buf())
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    let mut keys = None;
    let mut virtual_files = None;
    let mut told = None;
    let events = os
        .handle
        .as_ref()
        .map(DropTargetHandle::take_events)
        .unwrap_or_default();
    for event in events {
        match event {
            DropEvent::Enter(info) => os.drag = Some(info),
            DropEvent::Leave => os.drag = None,
            DropEvent::Drop {
                info,
                virtual_source,
                ctrl,
                shift,
                answered,
            } => {
                os.drag = None;
                keys = Some((ctrl, shift));
                told = Some(answered);
                if !info.files.is_empty() {
                    paths = info.files;
                } else if let Some(source) = virtual_source {
                    virtual_files = Some(VirtualDrop {
                        count: info.virtual_count,
                        source: Arc::new(parking_lot::Mutex::new(Some(source))),
                    });
                }
            }
        }
    }
    hovering |= os.is_dragging();
    let dropping = !paths.is_empty() || virtual_files.is_some();
    if from_os && (hovering || dropping) {
        if let Some(pos) = cursor_pos(ctx, raw) {
            raw.events.push(egui::Event::PointerMoved(pos));
        }
        if keys.is_none() {
            keys = os_keys();
        }
    }
    let dropped = dropping.then_some(External {
        paths,
        keys,
        virtual_files,
        told,
    });
    ctx.data_mut(|d| {
        d.insert_temp(
            slot_id(),
            Slot {
                hovering,
                dropped,
                ..Slot::default()
            },
        )
    });
}

/// Are files from another application over the window (or being dropped) this frame?
pub fn active(ctx: &egui::Context) -> bool {
    ctx.data(|d| {
        d.get_temp::<Slot>(slot_id())
            .is_some_and(|s| s.hovering || s.dropped.is_some())
    })
}

/// Notes `target` as the folder under the pointer, unless a drop target before it this
/// frame already did (the first one gets the drop, see [`take`]).
pub fn note_hover_target(ctx: &egui::Context, target: &Path) {
    ctx.data_mut(|d| {
        let slot = d.get_temp_mut_or_default::<Slot>(slot_id());
        if slot.hover_target.is_none() {
            slot.hover_target = Some(target.to_path_buf());
        }
    });
}

/// Takes this frame's drop, once: the first drop target under the pointer gets it.
pub fn take(ctx: &egui::Context) -> Option<External> {
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Slot>(slot_id()).dropped.take())
}

/// A folder drop target took a virtual-file drop: the app copies it into `target` (see
/// [`take_claimed`]).
pub fn claim(ctx: &egui::Context, drop: External, target: &Path) {
    ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<Slot>(slot_id()).claimed = Some((drop, target.to_path_buf()))
    });
}

pub fn take_claimed(ctx: &egui::Context) -> Option<(External, PathBuf)> {
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Slot>(slot_id()).claimed.take())
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
