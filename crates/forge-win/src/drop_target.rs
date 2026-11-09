//! ZXraw's own OLE drop target. It replaces winit's on the main window, which only takes
//! `CF_HDROP` and always answers Copy: this one also takes virtual files (Explorer's zip
//! folders) and answers what the app decides under the pointer.
//!
//! The target never returns `DROPEFFECT_MOVE`, so a source never deletes anything itself.
//! A move is done by the app and reported as an optimized move (see [`drop_reply`]).
//!
//! On Linux (X11) the same API is served by an XDND target on its own connection, through
//! the window's `XdndProxy`: see [`crate::xdnd`]. There a move is reported to the source as
//! `XdndActionPrivate`, for the same reason.

use std::path::PathBuf;

use crate::virtual_files::VirtualSource;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DropEffect {
    #[default]
    None,
    Copy,
    Move,
}

/// `DROPEFFECT` bits, kept here so the pure rules below compile everywhere.
pub(crate) const EFFECT_NONE: u32 = 0;
pub(crate) const EFFECT_COPY: u32 = 1;
pub(crate) const EFFECT_MOVE: u32 = 2;
#[cfg_attr(not(any(all(test, windows), target_os = "linux")), allow(dead_code))]
pub(crate) const EFFECT_LINK: u32 = 4;

impl DropEffect {
    /// The `DROPEFFECT` value.
    pub fn bits(self) -> u32 {
        match self {
            DropEffect::None => EFFECT_NONE,
            DropEffect::Copy => EFFECT_COPY,
            DropEffect::Move => EFFECT_MOVE,
        }
    }
}

/// What a drag over the window carries.
#[derive(Clone, Debug, Default)]
pub struct DragInfo {
    /// `CF_HDROP` paths, if the data object has them.
    pub files: Vec<PathBuf>,
    /// Number of virtual files (`FileGroupDescriptorW`) when there are no `CF_HDROP` paths.
    pub virtual_count: usize,
}

pub enum DropEvent {
    Enter(DragInfo),
    Leave,
    Drop {
        info: DragInfo,
        /// Set when `info.files` is empty and virtual files were dropped.
        virtual_source: Option<VirtualSource>,
        ctrl: bool,
        shift: bool,
        /// What `Drop` told the source (the hover answer at the drop, limited to what it allows).
        answered: DropEffect,
    },
}

/// What [`DropTargetHandle::simulate_drop_of_shell_item`] saw, as raw `DROPEFFECT` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimulatedDrop {
    /// The effect `Drop` returned to the source.
    pub returned: u32,
    /// "Performed DropEffect" read back from the data object, if set.
    pub performed: Option<u32>,
    /// "Logical Performed DropEffect" read back from the data object, if set.
    pub logical_performed: Option<u32>,
}

/// The effect to show for the app's `answer`, limited to what the source `allowed`.
/// Virtual files are only ever copied.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn resolve_effect(answer: DropEffect, allowed: u32, is_virtual: bool) -> DropEffect {
    let answer = match answer {
        DropEffect::Move if is_virtual => DropEffect::Copy,
        other => other,
    };
    let copy = || {
        if allowed & EFFECT_COPY != 0 {
            DropEffect::Copy
        } else {
            DropEffect::None
        }
    };
    match answer {
        DropEffect::Move if allowed & EFFECT_MOVE != 0 => DropEffect::Move,
        DropEffect::Move | DropEffect::Copy => copy(),
        DropEffect::None => DropEffect::None,
    }
}

/// The answer before the app has given one in this drag (it overrides within a frame).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn provisional_answer(is_virtual: bool, ctrl: bool, shift: bool) -> DropEffect {
    if !is_virtual && shift && !ctrl {
        DropEffect::Move
    } else {
        DropEffect::Copy
    }
}

/// What `Drop` tells the source for `answered`: `(returned effect, "Performed DropEffect",
/// "Logical Performed DropEffect")`. Never `DROPEFFECT_MOVE` as the returned or performed
/// effect, so the source never deletes its originals; a move is an optimized move (MS docs,
/// "Handling Optimized Move Operations"): performed none, logically a move.
#[cfg_attr(not(windows), allow(dead_code))]
fn drop_reply(answered: DropEffect) -> (u32, Option<u32>, Option<u32>) {
    match answered {
        DropEffect::Move => (EFFECT_NONE, Some(EFFECT_NONE), Some(EFFECT_MOVE)),
        DropEffect::Copy => (EFFECT_COPY, Some(EFFECT_COPY), None),
        DropEffect::None => (EFFECT_NONE, None, None),
    }
}

/// The drag being hovered, as read at `DragEnter`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct CurrentDrag {
    pub(crate) info: DragInfo,
    pub(crate) is_virtual: bool,
}

/// State shared by the COM object and the handle.
#[derive(Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct Shared {
    pub(crate) events: Vec<DropEvent>,
    pub(crate) dragging: bool,
    pub(crate) keys: (bool, bool),
    pub(crate) answer: DropEffect,
    /// The app has called `set_answer` since this drag entered.
    pub(crate) answered_by_app: bool,
    /// `None` when nothing is dragged over us, or it is nothing we take.
    pub(crate) drag: Option<CurrentDrag>,
}

/// Registered on a window; revokes on drop. Lives on the window's thread.
pub struct DropTargetHandle {
    shared: std::sync::Arc<std::sync::Mutex<Shared>>,
    #[cfg(windows)]
    hwnd: isize,
    #[cfg(windows)]
    target: windows::Win32::System::Ole::IDropTarget,
    /// The XDND target thread; dropping it removes `XdndProxy` and stops the thread.
    #[cfg(target_os = "linux")]
    xdnd: crate::xdnd::TargetThread,
}

pub(crate) fn lock(shared: &std::sync::Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl DropTargetHandle {
    pub fn take_events(&self) -> Vec<DropEvent> {
        std::mem::take(&mut lock(&self.shared).events)
    }
    pub fn is_dragging(&self) -> bool {
        lock(&self.shared).dragging
    }
    /// Ctrl and Shift from the latest `DragEnter`/`DragOver`.
    pub fn keys(&self) -> (bool, bool) {
        lock(&self.shared).keys
    }
    /// The effect for the pointer's current place, set every frame while dragging.
    pub fn set_answer(&self, effect: DropEffect) {
        let mut shared = lock(&self.shared);
        #[cfg(target_os = "linux")]
        let changed = shared.dragging && (!shared.answered_by_app || shared.answer != effect);
        shared.answer = effect;
        shared.answered_by_app = true;
        // X sources only hear from us in reply to a pointer move: tell the target thread, so
        // a new answer reaches the source's cursor while the pointer rests.
        #[cfg(target_os = "linux")]
        if changed {
            drop(shared);
            self.xdnd.answer_changed();
        }
    }
}

#[cfg(windows)]
impl DropTargetHandle {
    /// Drops the shell item `parsing_name` on this target, as a real OLE drop would: its
    /// shell data object goes through our `DragEnter`, `DragOver` (after `set_answer(answer)`)
    /// and `Drop`, with Copy, Move and Link allowed. For e2e tests; call on the window thread.
    pub fn simulate_drop_of_shell_item(
        &self,
        parsing_name: &str,
        ctrl: bool,
        shift: bool,
        answer: DropEffect,
    ) -> crate::Result<SimulatedDrop> {
        use windows::Win32::Foundation::POINTL;
        use windows::Win32::System::Ole::DROPEFFECT;
        use windows::Win32::System::SystemServices::{
            MK_CONTROL, MK_LBUTTON, MK_SHIFT, MODIFIERKEYS_FLAGS,
        };
        let data = crate::virtual_files::shell_item_data_object(parsing_name)?;

        let mut modifiers = 0;
        if ctrl {
            modifiers |= MK_CONTROL.0;
        }
        if shift {
            modifiers |= MK_SHIFT.0;
        }
        let held = MODIFIERKEYS_FLAGS(modifiers | MK_LBUTTON.0);
        let released = MODIFIERKEYS_FLAGS(modifiers);
        let allowed = DROPEFFECT(EFFECT_COPY | EFFECT_MOVE | EFFECT_LINK);
        let pt = POINTL { x: 0, y: 0 };

        let mut effect = allowed;
        // SAFETY: calls through our own registered interface, as OLE makes them; `effect`
        // outlives each call.
        unsafe { self.target.DragEnter(&data, held, pt, &mut effect) }
            .map_err(|e| crate::api_err("IDropTarget::DragEnter", e))?;
        self.set_answer(answer);
        let mut effect = allowed;
        unsafe { self.target.DragOver(held, pt, &mut effect) }
            .map_err(|e| crate::api_err("IDropTarget::DragOver", e))?;
        let mut effect = allowed;
        unsafe { self.target.Drop(&data, released, pt, &mut effect) }
            .map_err(|e| crate::api_err("IDropTarget::Drop", e))?;

        let read_dword = |format: crate::Result<u16>| {
            let bytes = crate::virtual_files::read_hglobal_bytes(
                &data,
                format.ok()?,
                "IDataObject::GetData(drop effect)",
                4,
            )
            .ok()?;
            Some(u32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?))
        };
        Ok(SimulatedDrop {
            returned: effect.0,
            performed: read_dword(performed_format()),
            logical_performed: read_dword(logical_performed_format()),
        })
    }
}

#[cfg(not(windows))]
impl DropTargetHandle {
    pub fn simulate_drop_of_shell_item(
        &self,
        _parsing_name: &str,
        _ctrl: bool,
        _shift: bool,
        _answer: DropEffect,
    ) -> crate::Result<SimulatedDrop> {
        Err(crate::WinError::Unsupported)
    }
}

#[cfg(windows)]
impl Drop for DropTargetHandle {
    fn drop(&mut self) {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Ole::RevokeDragDrop;

        // SAFETY: revokes our registration on the window's thread; fails harmlessly if the
        // window is already gone.
        let _ = unsafe { RevokeDragDrop(HWND(self.hwnd as *mut std::ffi::c_void)) };
    }
}

/// Replaces the window's drop target (winit's) with ours. Call on the window's thread,
/// with OLE initialized there. `wake` runs after every event (to request a repaint).
#[cfg(windows)]
pub fn register(hwnd: isize, wake: Box<dyn Fn() + Send + Sync>) -> crate::Result<DropTargetHandle> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Ole::{IDropTarget, RegisterDragDrop, RevokeDragDrop};

    let window = HWND(hwnd as *mut std::ffi::c_void);
    let shared = std::sync::Arc::new(std::sync::Mutex::new(Shared::default()));
    let target: IDropTarget = Target {
        shared: shared.clone(),
        wake,
    }
    .into();
    // SAFETY: plain OLE calls on the window's thread; there may be no target to revoke.
    let _ = unsafe { RevokeDragDrop(window) };
    unsafe { RegisterDragDrop(window, &target) }
        .map_err(|e| crate::api_err("RegisterDragDrop", e))?;
    Ok(DropTargetHandle {
        shared,
        hwnd,
        target,
    })
}

/// Takes over XDND drops on the X11 toplevel `hwnd` (winit's window) on `$DISPLAY`: sets its
/// `XdndProxy` to a window of ours, served by a background thread (see [`crate::xdnd`]).
/// `wake` runs after every event and every pointer move of a drag (to request a repaint).
/// [`WinError::Unsupported`](crate::WinError::Unsupported) without `DISPLAY` (Wayland).
#[cfg(target_os = "linux")]
pub fn register(hwnd: isize, wake: Box<dyn Fn() + Send + Sync>) -> crate::Result<DropTargetHandle> {
    let display = crate::xdnd::display_name().ok_or(crate::WinError::Unsupported)?;
    register_on(&display, hwnd, wake)
}

/// [`register`] on a named X display.
#[cfg(target_os = "linux")]
pub(crate) fn register_on(
    display: &str,
    hwnd: isize,
    wake: Box<dyn Fn() + Send + Sync>,
) -> crate::Result<DropTargetHandle> {
    let window = u32::try_from(hwnd)
        .ok()
        .filter(|&w| w != 0)
        .ok_or_else(|| crate::WinError::Invalid(format!("not an X11 window id: {hwnd}")))?;
    let shared = std::sync::Arc::new(std::sync::Mutex::new(Shared::default()));
    let xdnd = crate::xdnd::TargetThread::start(display, window, shared.clone(), wake)?;
    Ok(DropTargetHandle { shared, xdnd })
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn register(
    _hwnd: isize,
    _wake: Box<dyn Fn() + Send + Sync>,
) -> crate::Result<DropTargetHandle> {
    Err(crate::WinError::Unsupported)
}

#[cfg(windows)]
fn performed_format() -> crate::Result<u16> {
    crate::virtual_files::registered_format(
        windows::core::w!("Performed DropEffect"),
        "RegisterClipboardFormatW(Performed DropEffect)",
    )
}

#[cfg(windows)]
fn logical_performed_format() -> crate::Result<u16> {
    crate::virtual_files::registered_format(
        windows::core::w!("Logical Performed DropEffect"),
        "RegisterClipboardFormatW(Logical Performed DropEffect)",
    )
}

/// Reads what a data object carries: `CF_HDROP` paths, else the virtual file count, else
/// `None` (nothing we take).
#[cfg(windows)]
fn read_drag(obj: &windows::Win32::System::Com::IDataObject) -> Option<CurrentDrag> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Com::TYMED_HGLOBAL;
    use windows::Win32::System::Ole::CF_HDROP;

    use crate::virtual_files::{Medium, hglobal_format};

    // Archive items dragged out of this app and back in: handled as the in-app drag of
    // their listed paths, not read through their virtual-file streams.
    if let Ok(format) = crate::virtual_source::own_format()
        && let Ok(bytes) = crate::virtual_files::read_hglobal_bytes(
            obj,
            format,
            "IDataObject::GetData(ZXraw Archive Items)",
            1 << 22,
        )
        && let Some(files) = crate::virtual_source::own_paths(&bytes)
    {
        return Some(CurrentDrag {
            info: DragInfo {
                files,
                virtual_count: 0,
            },
            is_virtual: false,
        });
    }
    // SAFETY: `hglobal_format` is a valid FORMATETC; the medium is owned by the guard.
    if let Ok(medium) = unsafe { obj.GetData(&hglobal_format(CF_HDROP.0)) }.map(Medium)
        && medium.0.tymed == TYMED_HGLOBAL.0 as u32
    {
        // SAFETY: the medium says the union holds a global handle.
        let global = unsafe { medium.0.u.hGlobal };
        if let Ok(files) = crate::clipboard::read_hdrop(HANDLE(global.0))
            && !files.is_empty()
        {
            return Some(CurrentDrag {
                info: DragInfo {
                    files,
                    virtual_count: 0,
                },
                is_virtual: false,
            });
        }
    }
    if !crate::virtual_files::data_object_has_virtual_files(obj) {
        return None;
    }
    let count = crate::virtual_files::read_descriptor_count(obj).ok()?;
    (count > 0).then(|| CurrentDrag {
        info: DragInfo {
            files: Vec::new(),
            virtual_count: count,
        },
        is_virtual: true,
    })
}

/// Sets a DWORD format on the data object (the source reads it after the drop). Failures
/// are ignored: the drop has already happened.
#[cfg(windows)]
fn set_dword(
    obj: &windows::Win32::System::Com::IDataObject,
    format: crate::Result<u16>,
    value: u32,
) {
    use windows::Win32::Foundation::GlobalFree;
    use windows::Win32::System::Com::{STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL};
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

    let Ok(format) = format else {
        return;
    };
    // SAFETY: a fresh 4-byte block, written while locked and handed to SetData, or freed.
    unsafe {
        let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, 4) else {
            return;
        };
        let ptr = GlobalLock(global);
        if ptr.is_null() {
            let _ = GlobalFree(Some(global));
            return;
        }
        std::ptr::write_unaligned(ptr.cast::<u32>(), value);
        let _ = GlobalUnlock(global);
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: global },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        };
        // With fRelease = TRUE the data object owns the block once SetData succeeds.
        if obj
            .SetData(&crate::virtual_files::hglobal_format(format), &medium, true)
            .is_err()
        {
            let _ = GlobalFree(Some(global));
        }
    }
}

#[cfg(windows)]
#[windows::core::implement(windows::Win32::System::Ole::IDropTarget)]
struct Target {
    shared: std::sync::Arc<std::sync::Mutex<Shared>>,
    wake: Box<dyn Fn() + Send + Sync>,
}

#[cfg(windows)]
fn keys_of(state: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS) -> (bool, bool) {
    use windows::Win32::System::SystemServices::{MK_CONTROL, MK_SHIFT};

    (state.0 & MK_CONTROL.0 != 0, state.0 & MK_SHIFT.0 != 0)
}

/// Reads `*effect` (what the source allows), or nothing for a null pointer.
#[cfg(windows)]
fn allowed_effects(effect: *mut windows::Win32::System::Ole::DROPEFFECT) -> u32 {
    if effect.is_null() {
        return EFFECT_NONE;
    }
    // SAFETY: OLE passes a valid in/out DROPEFFECT pointer.
    unsafe { (*effect).0 }
}

#[cfg(windows)]
fn answer_effect(effect: *mut windows::Win32::System::Ole::DROPEFFECT, value: u32) {
    if !effect.is_null() {
        // SAFETY: as in `allowed_effects`.
        unsafe { *effect = windows::Win32::System::Ole::DROPEFFECT(value) };
    }
}

// COM calls into the data object are never made while `shared` is locked: an outgoing call
// pumps messages, and the app's frame (which locks it too) could run inside it.
#[cfg(windows)]
#[allow(non_snake_case)]
impl windows::Win32::System::Ole::IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        pdataobj: windows::core::Ref<windows::Win32::System::Com::IDataObject>,
        grfkeystate: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _pt: &windows::Win32::Foundation::POINTL,
        pdweffect: *mut windows::Win32::System::Ole::DROPEFFECT,
    ) -> windows::core::Result<()> {
        let allowed = allowed_effects(pdweffect);
        let (ctrl, shift) = keys_of(grfkeystate);
        let drag = pdataobj.as_ref().and_then(read_drag);
        let entered = drag.is_some();
        let effect = {
            let mut shared = lock(&self.shared);
            shared.keys = (ctrl, shift);
            shared.answered_by_app = false;
            shared.dragging = entered;
            match drag {
                Some(drag) => {
                    shared.answer = provisional_answer(drag.is_virtual, ctrl, shift);
                    shared.events.push(DropEvent::Enter(drag.info.clone()));
                    let effect = resolve_effect(shared.answer, allowed, drag.is_virtual);
                    shared.drag = Some(drag);
                    effect
                }
                None => {
                    shared.answer = DropEffect::None;
                    shared.drag = None;
                    DropEffect::None
                }
            }
        };
        if entered {
            (self.wake)();
        }
        answer_effect(pdweffect, effect.bits());
        Ok(())
    }

    fn DragOver(
        &self,
        grfkeystate: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _pt: &windows::Win32::Foundation::POINTL,
        pdweffect: *mut windows::Win32::System::Ole::DROPEFFECT,
    ) -> windows::core::Result<()> {
        let allowed = allowed_effects(pdweffect);
        let (ctrl, shift) = keys_of(grfkeystate);
        let effect = {
            let mut shared = lock(&self.shared);
            shared.keys = (ctrl, shift);
            match shared.drag.as_ref().map(|drag| drag.is_virtual) {
                Some(is_virtual) => {
                    if !shared.answered_by_app {
                        shared.answer = provisional_answer(is_virtual, ctrl, shift);
                    }
                    resolve_effect(shared.answer, allowed, is_virtual)
                }
                None => DropEffect::None,
            }
        };
        answer_effect(pdweffect, effect.bits());
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        let left = {
            let mut shared = lock(&self.shared);
            shared.dragging = false;
            let left = shared.drag.take().is_some();
            if left {
                shared.events.push(DropEvent::Leave);
            }
            left
        };
        if left {
            (self.wake)();
        }
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: windows::core::Ref<windows::Win32::System::Com::IDataObject>,
        grfkeystate: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _pt: &windows::Win32::Foundation::POINTL,
        pdweffect: *mut windows::Win32::System::Ole::DROPEFFECT,
    ) -> windows::core::Result<()> {
        let allowed = allowed_effects(pdweffect);
        let (ctrl, shift) = keys_of(grfkeystate);
        let (cached, answer, answered_by_app) = {
            let mut shared = lock(&self.shared);
            shared.dragging = false;
            shared.keys = (ctrl, shift);
            (shared.drag.take(), shared.answer, shared.answered_by_app)
        };
        let Some(obj) = pdataobj.as_ref() else {
            answer_effect(pdweffect, EFFECT_NONE);
            return Ok(());
        };
        let Some(drag) = cached.or_else(|| read_drag(obj)) else {
            answer_effect(pdweffect, EFFECT_NONE);
            return Ok(());
        };
        let answer = if answered_by_app {
            answer
        } else {
            provisional_answer(drag.is_virtual, ctrl, shift)
        };
        let answered = resolve_effect(answer, allowed, drag.is_virtual);
        let virtual_source = if drag.is_virtual {
            match VirtualSource::from_data_object(obj) {
                Ok(source) => Some(source),
                Err(e) => {
                    tracing::warn!("could not keep the dropped virtual files: {e}");
                    answer_effect(pdweffect, EFFECT_NONE);
                    return Ok(());
                }
            }
        } else {
            None
        };
        lock(&self.shared).events.push(DropEvent::Drop {
            info: drag.info,
            virtual_source,
            ctrl,
            shift,
            answered,
        });
        (self.wake)();

        let (returned, performed, logical) = drop_reply(answered);
        if let Some(value) = performed {
            set_dword(obj, performed_format(), value);
        }
        if let Some(value) = logical {
            set_dword(obj, logical_performed_format(), value);
        }
        answer_effect(pdweffect, returned);
        Ok(())
    }
}

#[cfg(test)]
mod rule_tests {
    use super::*;

    #[test]
    fn drop_reply_never_returns_move() {
        for answered in [DropEffect::None, DropEffect::Copy, DropEffect::Move] {
            let (returned, performed, _) = drop_reply(answered);
            assert_eq!(returned & EFFECT_MOVE, 0, "{answered:?}");
            assert_ne!(performed, Some(EFFECT_MOVE), "{answered:?}");
        }
        assert_eq!(
            drop_reply(DropEffect::Move),
            (EFFECT_NONE, Some(EFFECT_NONE), Some(EFFECT_MOVE))
        );
        assert_eq!(
            drop_reply(DropEffect::Copy),
            (EFFECT_COPY, Some(EFFECT_COPY), None)
        );
        assert_eq!(drop_reply(DropEffect::None), (EFFECT_NONE, None, None));
    }

    #[test]
    fn effect_rule_falls_back_to_what_the_source_allows() {
        let all = EFFECT_COPY | EFFECT_MOVE | EFFECT_LINK;
        assert_eq!(
            resolve_effect(DropEffect::Move, all, false),
            DropEffect::Move
        );
        assert_eq!(
            resolve_effect(DropEffect::Move, EFFECT_COPY, false),
            DropEffect::Copy
        );
        assert_eq!(
            resolve_effect(DropEffect::Move, EFFECT_LINK, false),
            DropEffect::None
        );
        assert_eq!(
            resolve_effect(DropEffect::Copy, all, false),
            DropEffect::Copy
        );
        assert_eq!(
            resolve_effect(DropEffect::Copy, EFFECT_MOVE, false),
            DropEffect::None
        );
        assert_eq!(
            resolve_effect(DropEffect::None, all, false),
            DropEffect::None
        );
    }

    #[test]
    fn virtual_files_are_capped_at_copy() {
        let all = EFFECT_COPY | EFFECT_MOVE;
        assert_eq!(
            resolve_effect(DropEffect::Move, all, true),
            DropEffect::Copy
        );
        assert_eq!(
            resolve_effect(DropEffect::Move, EFFECT_MOVE, true),
            DropEffect::None
        );
        assert_eq!(provisional_answer(true, false, true), DropEffect::Copy);
    }

    #[test]
    fn provisional_answer_follows_the_keys() {
        assert_eq!(provisional_answer(false, false, false), DropEffect::Copy);
        assert_eq!(provisional_answer(false, false, true), DropEffect::Move);
        assert_eq!(provisional_answer(false, true, false), DropEffect::Copy);
        assert_eq!(provisional_answer(false, true, true), DropEffect::Copy);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
    };
    use windows::core::w;

    use super::*;
    use crate::virtual_files::test_support::{pump_until, write_test_zip};
    use crate::virtual_files::{OleApartment, VirtualContent, with_virtual_files};

    /// OLE, a message-only window with our target registered, and a wake counter.
    struct Fixture {
        handle: Option<DropTargetHandle>,
        wakes: Arc<AtomicUsize>,
        hwnd: HWND,
        _ole: OleApartment,
    }

    impl Fixture {
        fn new() -> Self {
            let ole = OleApartment::new().unwrap();
            // SAFETY: a plain message-only window on this thread, destroyed in Drop.
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("ZXraw drop target test"),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    Some(HWND_MESSAGE),
                    None,
                    None,
                    None,
                )
            }
            .unwrap();
            let wakes = Arc::new(AtomicUsize::new(0));
            let counter = wakes.clone();
            let handle = register(
                hwnd.0 as isize,
                Box::new(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .unwrap();
            Self {
                handle: Some(handle),
                wakes,
                hwnd,
                _ole: ole,
            }
        }

        fn handle(&self) -> &DropTargetHandle {
            self.handle.as_ref().unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.handle = None;
            // SAFETY: the window was created on this thread.
            let _ = unsafe { DestroyWindow(self.hwnd) };
        }
    }

    /// The single `Drop` event after an `Enter`.
    fn the_drop(events: Vec<DropEvent>) -> (DragInfo, Option<VirtualSource>, DropEffect) {
        let mut events = events.into_iter();
        assert!(matches!(events.next(), Some(DropEvent::Enter(_))));
        match (events.next(), events.next()) {
            (
                Some(DropEvent::Drop {
                    info,
                    virtual_source,
                    answered,
                    ..
                }),
                None,
            ) => (info, virtual_source, answered),
            _ => panic!("expected Enter then Drop"),
        }
    }

    #[test]
    fn dropping_a_file_reports_its_path_and_never_moves_it() {
        let fixture = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("dropped file.txt");
        std::fs::write(&file, b"x").unwrap();
        let name = file.to_str().unwrap();

        let copy = fixture
            .handle()
            .simulate_drop_of_shell_item(name, false, false, DropEffect::Copy)
            .unwrap();
        assert_eq!(
            (copy.returned, copy.performed),
            (EFFECT_COPY, Some(EFFECT_COPY))
        );
        let (info, source, answered) = the_drop(fixture.handle().take_events());
        assert_eq!(info.files, vec![file.clone()]);
        assert!(source.is_none());
        assert_eq!(answered, DropEffect::Copy);
        assert!(!fixture.handle().is_dragging());
        assert_eq!(fixture.wakes.load(Ordering::SeqCst), 2);

        let moved = fixture
            .handle()
            .simulate_drop_of_shell_item(name, false, true, DropEffect::Move)
            .unwrap();
        assert_eq!(
            moved,
            SimulatedDrop {
                returned: EFFECT_NONE,
                performed: Some(EFFECT_NONE),
                logical_performed: Some(EFFECT_MOVE),
            }
        );
        let (info, _, answered) = the_drop(fixture.handle().take_events());
        assert_eq!(info.files, vec![file.clone()]);
        assert_eq!(answered, DropEffect::Move);
        assert_eq!(fixture.handle().keys(), (false, true));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(file.exists(), "nobody may delete the dropped original");
    }

    #[test]
    fn dropping_a_zip_folder_gives_virtual_files_capped_at_copy() {
        let fixture = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let zip = write_test_zip(dir.path());

        let drop = fixture
            .handle()
            .simulate_drop_of_shell_item(
                &format!("{}\\sub", zip.display()),
                false,
                false,
                DropEffect::Move,
            )
            .unwrap();
        assert_eq!(drop.returned, EFFECT_COPY);
        let (info, source, answered) = the_drop(fixture.handle().take_events());
        assert!(info.files.is_empty());
        assert_eq!(info.virtual_count, 2);
        assert_eq!(answered, DropEffect::Copy);

        // The source is read on another thread while this (window) thread pumps.
        let source = source.expect("a virtual source");
        let reader = std::thread::spawn(move || {
            with_virtual_files(source, |files| {
                let index = files.items().iter().position(|item| !item.is_dir).unwrap();
                let mut text = String::new();
                if let VirtualContent::Stream(mut stream) = files.open(index).unwrap() {
                    std::io::Read::read_to_string(&mut stream, &mut text).unwrap();
                }
                (files.items().len(), text)
            })
        });
        pump_until(|| reader.is_finished());
        assert_eq!(reader.join().unwrap().unwrap(), (2, "world".to_string()));
    }

    #[test]
    fn a_drag_of_nothing_we_take_is_refused_without_events() {
        use windows::Win32::Foundation::POINTL;
        use windows::Win32::System::Com::IDataObject;
        use windows::Win32::System::Ole::DROPEFFECT;
        use windows::Win32::System::SystemServices::MK_LBUTTON;
        use windows::Win32::UI::Shell::SHCreateDataObject;

        let fixture = Fixture::new();
        // SAFETY: an empty shell data object: no CF_HDROP, no virtual files.
        let data: IDataObject =
            unsafe { SHCreateDataObject(None, None, None::<&IDataObject>) }.unwrap();
        let target = &fixture.handle().target;
        let pt = POINTL { x: 0, y: 0 };
        let mut effect = DROPEFFECT(EFFECT_COPY | EFFECT_MOVE);
        // SAFETY: calls through our own interface, as OLE makes them.
        unsafe { target.DragEnter(&data, MK_LBUTTON, pt, &mut effect) }.unwrap();
        assert_eq!(effect.0, EFFECT_NONE);
        fixture.handle().set_answer(DropEffect::Copy);
        let mut effect = DROPEFFECT(EFFECT_COPY | EFFECT_MOVE);
        unsafe { target.DragOver(MK_LBUTTON, pt, &mut effect) }.unwrap();
        assert_eq!(effect.0, EFFECT_NONE);
        unsafe { target.DragLeave() }.unwrap();
        assert!(fixture.handle().take_events().is_empty());
        assert!(!fixture.handle().is_dragging());
        assert_eq!(fixture.wakes.load(Ordering::SeqCst), 0);
    }
}
