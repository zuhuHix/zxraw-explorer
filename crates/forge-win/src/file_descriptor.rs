//! Parsing of `FILEGROUPDESCRIPTORW` ("FileGroupDescriptorW"), the list of virtual files a
//! data object offers through `FileContents` (Explorer's zip folders, mail attachments).
//! Pure code: the bytes come from another application and are treated as untrusted.

use std::time::SystemTime;

/// One `FILEDESCRIPTORW` entry. Optional fields are `None` when the entry's flags say the
/// source did not fill them in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDescriptor {
    /// `cFileName` up to its first NUL, as the source wrote it: a relative path that may
    /// contain `\` separators. Not validated here.
    pub name: String,
    /// `FILE_ATTRIBUTE_DIRECTORY` was set (only known with `FD_ATTRIBUTES`).
    pub is_dir: bool,
    pub attributes: Option<u32>,
    pub size: Option<u64>,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    /// The raw `dwFlags`.
    pub flags: u32,
}

/// Parses a whole `FILEGROUPDESCRIPTORW` block.
pub fn parse_file_group_descriptor_w(_bytes: &[u8]) -> crate::Result<Vec<FileDescriptor>> {
    Err(crate::WinError::Unsupported) // STUB: replaced by the parser task.
}
