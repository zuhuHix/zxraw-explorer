//! Virtual files offered through `FileGroupDescriptorW` + `FileContents` (STUB, see task).

use std::path::Path;

use crate::file_descriptor::FileDescriptor;

/// Where the virtual files come from. `Send`, so it can be handed to a job thread.
pub struct VirtualSource {
    _private: (),
}

impl VirtualSource {
    /// The data object on the Windows clipboard, read when the files are opened.
    pub fn clipboard() -> Self {
        Self { _private: () }
    }
}

/// Does the Windows clipboard offer virtual files (`FileGroupDescriptorW`)?
pub fn clipboard_has_virtual_files() -> bool {
    false
}

/// The opened data object, usable only inside [`with_virtual_files`].
pub struct VirtualFiles {
    items: Vec<FileDescriptor>,
}

impl VirtualFiles {
    pub fn items(&self) -> &[FileDescriptor] {
        &self.items
    }
    pub fn open(&self, _index: usize) -> crate::Result<VirtualContent<'_>> {
        Err(crate::WinError::Unsupported)
    }
}

/// One item's contents.
pub enum VirtualContent<'a> {
    /// Read it to the end (the size may differ from the descriptor's).
    Stream(Box<dyn std::io::Read + 'a>),
    /// A structured storage (e.g. an Outlook message): written as a compound file.
    Storage(VirtualStorage),
}

pub struct VirtualStorage {
    _private: (),
}

impl VirtualStorage {
    pub fn save_as(&self, _path: &Path) -> crate::Result<()> {
        Err(crate::WinError::Unsupported)
    }
}

/// Opens `source` on a COM thread of its own and runs `f` there; returns what `f` returns.
pub fn with_virtual_files<R: Send>(
    _source: VirtualSource,
    _f: impl FnOnce(&VirtualFiles) -> R + Send,
) -> crate::Result<R> {
    Err(crate::WinError::Unsupported)
}
