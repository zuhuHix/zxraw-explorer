//! ZXraw's own OLE drop target (STUB, see task).

use std::path::PathBuf;

use crate::virtual_files::VirtualSource;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DropEffect {
    #[default]
    None,
    Copy,
    Move,
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

/// Registered on a window; revokes on drop. Lives on the window's thread.
pub struct DropTargetHandle {
    _private: (),
}

impl DropTargetHandle {
    pub fn take_events(&self) -> Vec<DropEvent> {
        Vec::new()
    }
    pub fn is_dragging(&self) -> bool {
        false
    }
    /// Ctrl and Shift from the latest `DragEnter`/`DragOver`.
    pub fn keys(&self) -> (bool, bool) {
        (false, false)
    }
    pub fn set_answer(&self, _effect: DropEffect) {}
}

/// Replaces the window's drop target (winit's) with ours.
pub fn register(_hwnd: isize, _wake: Box<dyn Fn() + Send + Sync>) -> crate::Result<DropTargetHandle> {
    Err(crate::WinError::Unsupported)
}
