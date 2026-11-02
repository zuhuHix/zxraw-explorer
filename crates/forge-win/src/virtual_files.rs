//! Virtual files offered through `FileGroupDescriptorW` + `FileContents`: Explorer's zip
//! folders, mail attachments and other sources that have no real paths (no `CF_HDROP`).
//!
//! The data object is read on a COM thread of its own ([`with_virtual_files`]), so a slow
//! source (a big zip, a mail server) never blocks the UI thread.
//!
//! `IDataObjectAsyncCapability` is not used. The clipboard path already relies on the
//! source's data object staying usable after the hand-off, and a drop works the same way:
//! `Drop` returns at once, holding a reference to the data object, and the files are read
//! later on our own thread. Sources such as zipfldr keep their data object alive for as long
//! as a reference is held, so the async protocol would add nothing but another code path.

use std::path::Path;

use crate::file_descriptor::FileDescriptor;

/// Where the virtual files come from. `Send`, so it can be handed to a job thread.
pub struct VirtualSource {
    #[cfg(windows)]
    kind: SourceKind,
    #[cfg(not(windows))]
    _private: (),
}

#[cfg(windows)]
enum SourceKind {
    /// `OleGetClipboard` on the reading thread, but only while the clipboard is still at the
    /// sequence number it had when the paste looked at it.
    Clipboard { sequence: u32 },
    /// A dropped data object, marshalled for another thread.
    Marshalled(MarshalStream),
}

impl VirtualSource {
    /// The data object on the Windows clipboard, read when the files are opened, provided
    /// the clipboard is still at `sequence` (see [`crate::clipboard_sequence`]): if anything
    /// was copied since, opening fails rather than pasting the newer content.
    pub fn clipboard(sequence: u32) -> Self {
        #[cfg(not(windows))]
        let _ = sequence;
        Self {
            #[cfg(windows)]
            kind: SourceKind::Clipboard { sequence },
            #[cfg(not(windows))]
            _private: (),
        }
    }
}

#[cfg(windows)]
impl VirtualSource {
    /// Marshals `obj` (from the calling thread's apartment) so that another thread can use
    /// it. The calling thread must keep pumping messages while the other thread reads, when
    /// the object lives in this apartment.
    pub(crate) fn from_data_object(
        obj: &windows::Win32::System::Com::IDataObject,
    ) -> crate::Result<Self> {
        use windows::Win32::System::Com::IDataObject;
        use windows::Win32::System::Com::Marshal::CoMarshalInterThreadInterfaceInStream;
        use windows::core::Interface;

        // SAFETY: `obj` is a live interface on this thread; the call AddRefs what it needs.
        let stream = unsafe { CoMarshalInterThreadInterfaceInStream(&IDataObject::IID, obj) }
            .map_err(|e| crate::api_err("CoMarshalInterThreadInterfaceInStream", e))?;
        Ok(Self {
            kind: SourceKind::Marshalled(MarshalStream(Some(stream))),
        })
    }

    /// The data object, usable on the calling (COM-initialized) thread.
    fn into_data_object(mut self) -> crate::Result<windows::Win32::System::Com::IDataObject> {
        use windows::Win32::System::Com::StructuredStorage::CoGetInterfaceAndReleaseStream;
        use windows::Win32::System::Ole::OleGetClipboard;

        match &mut self.kind {
            // SAFETY: COM is initialized on this thread (see `with_virtual_files`).
            SourceKind::Clipboard { sequence } => {
                let data = unsafe { OleGetClipboard() }
                    .map_err(|e| crate::api_err("OleGetClipboard", e))?;
                // Checked after taking the data object: if the number still matches, it is
                // the content the paste saw.
                if crate::clipboard_sequence() != *sequence {
                    return Err(crate::WinError::Invalid(CLIPBOARD_CHANGED.into()));
                }
                Ok(data)
            }
            SourceKind::Marshalled(stream) => {
                let stream = stream.0.take().ok_or_else(|| {
                    crate::WinError::Invalid("the dropped data object was already used".into())
                })?;
                // SAFETY: the stream holds marshal data written by
                // CoMarshalInterThreadInterfaceInStream; this call releases it and the stream,
                // whether or not it succeeds.
                unsafe { CoGetInterfaceAndReleaseStream(&stream) }
                    .map_err(|e| crate::api_err("CoGetInterfaceAndReleaseStream", e))
            }
        }
    }
}

/// Why a paste of virtual files did nothing when the clipboard changed in the meantime.
pub const CLIPBOARD_CHANGED: &str =
    "the clipboard changed after Paste was pressed, so nothing was pasted; copy the items again";

/// The marshal data of a dropped data object. Released cleanly if never unmarshalled; for
/// an object in this process that release calls into its thread, which must be pumping
/// messages (the window thread always is).
#[cfg(windows)]
struct MarshalStream(Option<windows::Win32::System::Com::IStream>);

// SAFETY: a stream from CoMarshalInterThreadInterfaceInStream is a free-threaded global-memory
// stream, made to be handed to another thread.
#[cfg(windows)]
unsafe impl Send for MarshalStream {}

#[cfg(windows)]
impl Drop for MarshalStream {
    fn drop(&mut self) {
        use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
        use windows::Win32::System::Com::Marshal::CoReleaseMarshalData;
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

        let Some(stream) = self.0.take() else {
            return;
        };
        // CoReleaseMarshalData needs COM on this thread; a thread that already has it (in
        // any apartment) is used as it is.
        // SAFETY: plain COM initialization, undone below only if it succeeded here.
        let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let uninit = init.is_ok();
        if init.is_ok() || init == RPC_E_CHANGED_MODE {
            // SAFETY: the stream is positioned at the start of its marshal data, as returned.
            if let Err(e) = unsafe { CoReleaseMarshalData(&stream) } {
                tracing::debug!("could not release an unused dropped data object: {e}");
            }
        }
        drop(stream);
        if uninit {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }
}

/// Does the Windows clipboard offer virtual files (`FileGroupDescriptorW`)? Does not open
/// the clipboard. The caller prefers `CF_HDROP` when both are there.
#[cfg(windows)]
pub fn clipboard_has_virtual_files() -> bool {
    use windows::Win32::System::DataExchange::IsClipboardFormatAvailable;

    let Ok(format) = descriptor_format() else {
        return false;
    };
    // SAFETY: a read-only query of the clipboard's formats.
    unsafe { IsClipboardFormatAvailable(u32::from(format)) }.is_ok()
}

#[cfg(not(windows))]
pub fn clipboard_has_virtual_files() -> bool {
    false
}

/// The opened data object, usable only inside [`with_virtual_files`].
pub struct VirtualFiles {
    items: Vec<FileDescriptor>,
    #[cfg(windows)]
    data: windows::Win32::System::Com::IDataObject,
}

impl VirtualFiles {
    pub fn items(&self) -> &[FileDescriptor] {
        &self.items
    }

    /// Opens item `index` (a file, not a folder). Fails if the source cannot render it.
    #[cfg(windows)]
    pub fn open(&self, index: usize) -> crate::Result<VirtualContent<'_>> {
        use windows::Win32::System::Com::{
            DVASPECT_CONTENT, FORMATETC, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTORAGE,
            TYMED_ISTREAM,
        };
        use windows::Win32::System::Memory::{GlobalLock, GlobalSize};

        let item = self
            .items
            .get(index)
            .ok_or_else(|| crate::WinError::Invalid(format!("there is no virtual file {index}")))?;
        let lindex = i32::try_from(index)
            .map_err(|_| crate::WinError::Invalid("virtual file index is too large".into()))?;
        let format = FORMATETC {
            cfFormat: contents_format()?,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex,
            tymed: (TYMED_ISTREAM.0 | TYMED_HGLOBAL.0 | TYMED_ISTORAGE.0) as u32,
        };
        // SAFETY: `format` is a valid FORMATETC; the medium is owned by the guard from here.
        let medium = Medium(
            unsafe { self.data.GetData(&format) }
                .map_err(|e| crate::api_err("IDataObject::GetData(FileContents)", e))?,
        );
        match medium.0.tymed {
            t if t == TYMED_ISTREAM.0 as u32 => {
                // SAFETY: the medium says the union holds a stream; it is cloned (AddRef), and
                // the medium stays alive in the reader.
                let stream = unsafe { (*medium.0.u.pstm).clone() }.ok_or_else(|| {
                    crate::WinError::Invalid("FileContents returned a null stream".into())
                })?;
                // Explorer rewinds the stream too; a stream that cannot seek is read as it is.
                // SAFETY: a plain call on a live stream.
                let _ = unsafe { stream.Seek(0, STREAM_SEEK_SET, None) };
                Ok(VirtualContent::Stream(Box::new(StreamReader {
                    stream,
                    _medium: medium,
                })))
            }
            t if t == TYMED_HGLOBAL.0 as u32 => {
                // SAFETY: the medium says the union holds a global handle.
                let global = unsafe { medium.0.u.hGlobal };
                // SAFETY: GlobalSize and GlobalLock accept any handle and fail cleanly.
                let size = unsafe { GlobalSize(global) };
                let ptr = unsafe { GlobalLock(global) };
                if ptr.is_null() {
                    return Err(crate::api_err(
                        "GlobalLock(FileContents)",
                        windows::core::Error::from_thread(),
                    ));
                }
                let limit = match item.size {
                    Some(declared) => size.min(usize::try_from(declared).unwrap_or(usize::MAX)),
                    None => size,
                };
                Ok(VirtualContent::Stream(Box::new(GlobalReader {
                    ptr: ptr.cast::<u8>(),
                    len: limit,
                    pos: 0,
                    medium,
                })))
            }
            t if t == TYMED_ISTORAGE.0 as u32 => {
                // SAFETY: the medium says the union holds a storage; cloned (AddRef).
                let storage = unsafe { (*medium.0.u.pstg).clone() }.ok_or_else(|| {
                    crate::WinError::Invalid("FileContents returned a null storage".into())
                })?;
                Ok(VirtualContent::Storage(VirtualStorage {
                    storage,
                    _medium: medium,
                }))
            }
            other => Err(crate::WinError::Invalid(format!(
                "FileContents used an unexpected medium ({other})"
            ))),
        }
    }

    #[cfg(not(windows))]
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

/// A structured storage item, written out with [`VirtualStorage::save_as`].
pub struct VirtualStorage {
    #[cfg(windows)]
    storage: windows::Win32::System::Com::StructuredStorage::IStorage,
    #[cfg(windows)]
    _medium: Medium,
    #[cfg(not(windows))]
    _private: (),
}

impl VirtualStorage {
    /// Writes the storage to `path` as a compound file, replacing any file there.
    #[cfg(windows)]
    pub fn save_as(&self, path: &Path) -> crate::Result<()> {
        use windows::Win32::System::Com::StructuredStorage::StgCreateDocfile;
        use windows::Win32::System::Com::{
            STGC_DEFAULT, STGM_CREATE, STGM_READWRITE, STGM_SHARE_EXCLUSIVE,
        };
        use windows::core::PCWSTR;

        let wide = crate::to_wide(path);
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let dest = unsafe {
            StgCreateDocfile(
                PCWSTR(wide.as_ptr()),
                STGM_CREATE | STGM_READWRITE | STGM_SHARE_EXCLUSIVE,
                None,
            )
        }
        .map_err(|e| crate::api_err("StgCreateDocfile", e))?;
        // SAFETY: plain calls on live storages.
        unsafe { self.storage.CopyTo(None, None, &dest) }
            .map_err(|e| crate::api_err("IStorage::CopyTo", e))?;
        unsafe { dest.Commit(STGC_DEFAULT.0 as u32) }
            .map_err(|e| crate::api_err("IStorage::Commit", e))?;
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn save_as(&self, _path: &Path) -> crate::Result<()> {
        Err(crate::WinError::Unsupported)
    }
}

/// Opens `source` on a COM thread of its own and runs `f` there; returns what `f` returns.
/// A panic in `f` is passed on to the caller.
#[cfg(windows)]
pub fn with_virtual_files<R: Send>(
    source: VirtualSource,
    f: impl FnOnce(&VirtualFiles) -> R + Send,
) -> crate::Result<R> {
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || -> crate::Result<R> {
            let _apartment = OleApartment::new()?;
            let data = source.into_data_object()?;
            let items = read_descriptors(&data)?;
            let files = VirtualFiles { items, data };
            Ok(f(&files))
        });
        match worker.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

#[cfg(not(windows))]
pub fn with_virtual_files<R: Send>(
    _source: VirtualSource,
    _f: impl FnOnce(&VirtualFiles) -> R + Send,
) -> crate::Result<R> {
    Err(crate::WinError::Unsupported)
}

/// Whether `obj` offers `FileGroupDescriptorW` in global memory.
#[cfg(windows)]
pub(crate) fn data_object_has_virtual_files(
    obj: &windows::Win32::System::Com::IDataObject,
) -> bool {
    let Ok(format) = descriptor_format() else {
        return false;
    };
    // SAFETY: `hglobal_format` is a valid FORMATETC; QueryGetData only inspects it.
    unsafe { obj.QueryGetData(&hglobal_format(format)) }.is_ok()
}

/// Reads and parses `obj`'s `FileGroupDescriptorW`.
#[cfg(windows)]
pub(crate) fn read_descriptors(
    obj: &windows::Win32::System::Com::IDataObject,
) -> crate::Result<Vec<FileDescriptor>> {
    // Parsed in place: the block is never copied whole (its size is the source's to pick).
    with_hglobal(
        obj,
        descriptor_format()?,
        "IDataObject::GetData(FileGroupDescriptorW)",
        crate::file_descriptor::parse_file_group_descriptor_w,
    )?
}

/// How many virtual files `obj` offers. Reads only the descriptor header, so it is cheap
/// enough for `DragEnter` on the window thread.
#[cfg(windows)]
pub(crate) fn read_descriptor_count(
    obj: &windows::Win32::System::Com::IDataObject,
) -> crate::Result<usize> {
    with_hglobal(
        obj,
        descriptor_format()?,
        "IDataObject::GetData(FileGroupDescriptorW)",
        crate::file_descriptor::descriptor_count,
    )?
}

/// Copies out at most `max` bytes of `format` from `obj` (global memory, `lindex` -1).
#[cfg(windows)]
pub(crate) fn read_hglobal_bytes(
    obj: &windows::Win32::System::Com::IDataObject,
    format: u16,
    op: &'static str,
    max: usize,
) -> crate::Result<Vec<u8>> {
    with_hglobal(obj, format, op, |bytes| {
        bytes[..bytes.len().min(max)].to_vec()
    })
}

/// Runs `f` on the locked bytes of `format` from `obj` (global memory, `lindex` -1),
/// without copying them.
#[cfg(windows)]
pub(crate) fn with_hglobal<R>(
    obj: &windows::Win32::System::Com::IDataObject,
    format: u16,
    op: &'static str,
    f: impl FnOnce(&[u8]) -> R,
) -> crate::Result<R> {
    use windows::Win32::System::Com::TYMED_HGLOBAL;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize};

    // SAFETY: `hglobal_format` is a valid FORMATETC; the medium is owned by the guard.
    let medium =
        Medium(unsafe { obj.GetData(&hglobal_format(format)) }.map_err(|e| crate::api_err(op, e))?);
    if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
        return Err(crate::WinError::Invalid(format!(
            "{op} did not return global memory"
        )));
    }
    // SAFETY: the medium says the union holds a global handle.
    let global = unsafe { medium.0.u.hGlobal };
    // SAFETY: GlobalSize and GlobalLock accept any handle and fail cleanly.
    let size = unsafe { GlobalSize(global) };
    let ptr = unsafe { GlobalLock(global) };
    if ptr.is_null() {
        return Err(crate::api_err(op, windows::core::Error::from_thread()));
    }
    let _locked = LockedGlobal(global);
    // SAFETY: the block is locked and at least `size` bytes long.
    Ok(f(unsafe {
        std::slice::from_raw_parts(ptr.cast::<u8>(), size)
    }))
}

/// A `FORMATETC` for `format` in global memory, whole content.
#[cfg(windows)]
pub(crate) fn hglobal_format(format: u16) -> windows::Win32::System::Com::FORMATETC {
    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};

    FORMATETC {
        cfFormat: format,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// Registers a named clipboard format, or returns the existing id.
#[cfg(windows)]
pub(crate) fn registered_format(
    name: windows::core::PCWSTR,
    op: &'static str,
) -> crate::Result<u16> {
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;

    // SAFETY: `name` is a static NUL-terminated string.
    let format = unsafe { RegisterClipboardFormatW(name) };
    // Registered formats are in 0xC000..=0xFFFF, so they always fit a `cfFormat`.
    u16::try_from(format)
        .ok()
        .filter(|&f| f != 0)
        .ok_or_else(|| crate::api_err(op, windows::core::Error::from_thread()))
}

/// The shell's data object for the item `parsing_name` (e.g. `C:\x\t.zip\sub`), the one
/// Explorer copies or drags for it. Call on an OLE thread.
#[cfg(windows)]
pub(crate) fn shell_item_data_object(
    parsing_name: &str,
) -> crate::Result<windows::Win32::System::Com::IDataObject> {
    use windows::Win32::System::Com::{IBindCtx, IDataObject};
    use windows::Win32::UI::Shell::{BHID_DataObject, IShellItem, SHCreateItemFromParsingName};
    use windows::core::PCWSTR;

    let wide = crate::to_wide(parsing_name);
    // SAFETY: `wide` is NUL-terminated; the caller's thread has OLE initialized.
    let item: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>) }
            .map_err(|e| crate::api_err("SHCreateItemFromParsingName", e))?;
    // SAFETY: a plain call on a live shell item.
    unsafe { item.BindToHandler::<_, IDataObject>(None::<&IBindCtx>, &BHID_DataObject) }
        .map_err(|e| crate::api_err("IShellItem::BindToHandler(BHID_DataObject)", e))
}

/// The clipboard format [`set_clipboard_to_shell_item`] marks its data with.
pub const TEST_MARKER_FORMAT: &str = "ZXraw automation marker";

/// For automation: puts the shell item `parsing_name` on the clipboard as Explorer's Copy
/// does (its data object, through `OleSetClipboard`), plus `marker` as UTF-16 text in the
/// format [`TEST_MARKER_FORMAT`] when the data object takes it, so a test can tell its own
/// clipboard content. Call on the window thread, which keeps pumping messages while other
/// threads read the clipboard.
#[cfg(windows)]
pub fn set_clipboard_to_shell_item(parsing_name: &str, marker: &str) -> crate::Result<()> {
    use windows::Win32::System::Ole::OleSetClipboard;

    let data = shell_item_data_object(parsing_name)?;
    let format = registered_format(
        windows::core::w!("ZXraw automation marker"),
        "RegisterClipboardFormatW(marker)",
    )?;
    let mut bytes: Vec<u8> = marker
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    bytes.extend([0, 0]);
    if let Err(e) = set_hglobal(&data, format, &bytes) {
        tracing::debug!("the data object refused the marker: {e}");
    }
    // SAFETY: a live data object; OLE is initialized on this thread.
    unsafe { OleSetClipboard(&data) }.map_err(|e| crate::api_err("OleSetClipboard", e))
}

#[cfg(not(windows))]
pub fn set_clipboard_to_shell_item(_parsing_name: &str, _marker: &str) -> crate::Result<()> {
    Err(crate::WinError::Unsupported)
}

/// Gives `bytes` to `obj` as an HGLOBAL in `format` (`SetData` with fRelease).
#[cfg(windows)]
pub(crate) fn set_hglobal(
    obj: &windows::Win32::System::Com::IDataObject,
    format: u16,
    bytes: &[u8],
) -> crate::Result<()> {
    use windows::Win32::Foundation::GlobalFree;
    use windows::Win32::System::Com::{STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL};
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

    // SAFETY: a fresh block of `bytes.len()` bytes, filled while locked and handed to
    // SetData, which owns it on success; freed here otherwise.
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len())
            .map_err(|e| crate::api_err("GlobalAlloc", e))?;
        let ptr = GlobalLock(global);
        if ptr.is_null() {
            let _ = GlobalFree(Some(global));
            return Err(crate::api_err(
                "GlobalLock",
                windows::core::Error::from_thread(),
            ));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(global);
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: global },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        };
        obj.SetData(&hglobal_format(format), &medium, true)
            .inspect_err(|_| {
                let _ = GlobalFree(Some(global));
            })
            .map_err(|e| crate::api_err("IDataObject::SetData", e))
    }
}

#[cfg(windows)]
pub(crate) fn descriptor_format() -> crate::Result<u16> {
    registered_format(
        windows::core::w!("FileGroupDescriptorW"),
        "RegisterClipboardFormatW(FileGroupDescriptorW)",
    )
}

#[cfg(windows)]
pub(crate) fn contents_format() -> crate::Result<u16> {
    registered_format(
        windows::core::w!("FileContents"),
        "RegisterClipboardFormatW(FileContents)",
    )
}

/// A storage medium, released when dropped.
#[cfg(windows)]
pub(crate) struct Medium(pub(crate) windows::Win32::System::Com::STGMEDIUM);

#[cfg(windows)]
impl Drop for Medium {
    fn drop(&mut self) {
        // SAFETY: the medium came from GetData and is released exactly once.
        unsafe { windows::Win32::System::Ole::ReleaseStgMedium(&mut self.0) };
    }
}

#[cfg(windows)]
struct LockedGlobal(windows::Win32::Foundation::HGLOBAL);

#[cfg(windows)]
impl Drop for LockedGlobal {
    fn drop(&mut self) {
        // SAFETY: balances a successful GlobalLock. Zero is the normal result at lock count 0.
        let _ = unsafe { windows::Win32::System::Memory::GlobalUnlock(self.0) };
    }
}

/// `std::io::Read` over an `IStream`.
#[cfg(windows)]
struct StreamReader {
    stream: windows::Win32::System::Com::IStream,
    _medium: Medium,
}

#[cfg(windows)]
impl std::io::Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let want = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let mut got = 0u32;
        // SAFETY: `buf` has room for `want` bytes; `got` outlives the call.
        let hr = unsafe {
            self.stream
                .Read(buf.as_mut_ptr().cast(), want, Some(&mut got))
        };
        // S_FALSE (or fewer bytes) is the end of the stream; the bytes read still count.
        if hr.is_err() {
            return Err(std::io::Error::other(windows::core::Error::from(hr)));
        }
        Ok((got as usize).min(buf.len()))
    }
}

/// `std::io::Read` over a locked global block, unlocked and released when dropped.
#[cfg(windows)]
struct GlobalReader {
    ptr: *const u8,
    len: usize,
    pos: usize,
    medium: Medium,
}

#[cfg(windows)]
impl std::io::Read for GlobalReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.len - self.pos);
        // SAFETY: the block stays locked while the reader lives, and pos + n <= len <= size.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(self.pos), buf.as_mut_ptr(), n) };
        self.pos += n;
        Ok(n)
    }
}

#[cfg(windows)]
impl Drop for GlobalReader {
    fn drop(&mut self) {
        // SAFETY: the medium holds a global handle, locked once in `open`.
        let _ = unsafe { windows::Win32::System::Memory::GlobalUnlock(self.medium.0.u.hGlobal) };
    }
}

/// OLE on the current thread for the guard's lifetime.
#[cfg(windows)]
pub(crate) struct OleApartment;

#[cfg(windows)]
impl OleApartment {
    pub(crate) fn new() -> crate::Result<Self> {
        // SAFETY: plain OLE initialization, balanced in Drop.
        unsafe { windows::Win32::System::Ole::OleInitialize(None) }
            .map_err(|e| crate::api_err("OleInitialize", e))?;
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for OleApartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful OleInitialize in `new`.
        unsafe { windows::Win32::System::Ole::OleUninitialize() };
    }
}

/// Test helpers shared with `drop_target`'s tests.
#[cfg(all(test, windows))]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// Writes `t.zip` in `dir` holding `a.txt` ("hello") and `sub\b.txt` ("world").
    pub(crate) fn write_test_zip(dir: &Path) -> PathBuf {
        use std::io::Write;

        let path = dir.join("t.zip");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("a.txt", options).unwrap();
        zip.write_all(b"hello").unwrap();
        zip.add_directory("sub/", options).unwrap();
        zip.start_file("sub/b.txt", options).unwrap();
        zip.write_all(b"world").unwrap();
        zip.finish().unwrap();
        path
    }

    /// The shell's data object for `parsing_name`, as a drag of that item would carry.
    pub(crate) fn shell_data_object(
        parsing_name: &str,
    ) -> windows::Win32::System::Com::IDataObject {
        use windows::Win32::System::Com::{IBindCtx, IDataObject};
        use windows::Win32::UI::Shell::{BHID_DataObject, IShellItem, SHCreateItemFromParsingName};
        use windows::core::PCWSTR;

        let wide = crate::to_wide(parsing_name);
        // SAFETY: `wide` is NUL-terminated; COM is initialized on this thread.
        let item: IShellItem =
            unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>) }
                .unwrap();
        // SAFETY: a plain call on a live shell item.
        unsafe { item.BindToHandler::<_, IDataObject>(None::<&IBindCtx>, &BHID_DataObject) }
            .unwrap()
    }

    /// Pumps this thread's messages until `done` returns true.
    pub(crate) fn pump_until(mut done: impl FnMut() -> bool) {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out pumping messages"
            );
            let mut msg = MSG::default();
            // SAFETY: standard message pump on this thread.
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::io::Read;

    use super::test_support::{pump_until, shell_data_object, write_test_zip};
    use super::*;

    #[test]
    fn zip_folder_items_are_read_on_another_thread() {
        let dir = tempfile::tempdir().unwrap();
        let zip = write_test_zip(dir.path());
        let _ole = OleApartment::new().unwrap();
        let data = shell_data_object(&format!("{}\\sub", zip.display()));
        assert!(data_object_has_virtual_files(&data));

        let source = VirtualSource::from_data_object(&data).unwrap();
        let reader = std::thread::spawn(move || {
            with_virtual_files(source, |files| {
                let names: Vec<(String, bool)> = files
                    .items()
                    .iter()
                    .map(|item| (item.name.clone(), item.is_dir))
                    .collect();
                let file = files
                    .items()
                    .iter()
                    .position(|item| !item.is_dir)
                    .expect("a file item");
                let mut text = String::new();
                match files.open(file).unwrap() {
                    VirtualContent::Stream(mut reader) => {
                        reader.read_to_string(&mut text).unwrap();
                    }
                    VirtualContent::Storage(_) => panic!("a zip file item is a stream"),
                }
                (names, text)
            })
        });
        // The data object lives in this thread's apartment: serve the reader's calls.
        pump_until(|| reader.is_finished());
        let (names, text) = reader.join().unwrap().unwrap();
        assert_eq!(
            names,
            vec![("sub".to_string(), true), ("sub\\b.txt".to_string(), false)]
        );
        assert_eq!(text, "world");
    }

    /// A paste reads the clipboard it looked at, or nothing: once something else was
    /// copied, opening the source fails instead of pasting the newer content.
    #[test]
    fn a_clipboard_source_refuses_a_clipboard_changed_since_the_paste() {
        use crate::clipboard_test_support::{ClipboardLock, Snapshot};

        let _lock = ClipboardLock::acquire();
        let saved = match Snapshot::save() {
            Ok(saved) => saved,
            Err(why) => {
                eprintln!("skipped: the clipboard holds {why}, which this test can't restore");
                return;
            }
        };
        let dir = tempfile::tempdir().unwrap();
        let zip = write_test_zip(dir.path());
        let _ole = OleApartment::new().unwrap();
        let item = format!("{}\\sub", zip.display());
        set_clipboard_to_shell_item(&item, &dir.path().display().to_string()).unwrap();
        let sequence = crate::clipboard_sequence();
        let read = |sequence: u32| {
            let reader = std::thread::spawn(move || {
                with_virtual_files(VirtualSource::clipboard(sequence), |files| {
                    files.items().len()
                })
            });
            // This thread owns the clipboard's data object: serve the reader's calls.
            pump_until(|| reader.is_finished());
            reader.join().unwrap()
        };

        let unchanged = read(sequence);
        Snapshot::of_text("copied meanwhile")
            .restore_if(|| true)
            .unwrap();
        let changed = read(sequence);
        if let Err(e) = saved.restore_if(|| true) {
            eprintln!("warning: could not restore the clipboard: {e}");
        }

        assert_eq!(unchanged.unwrap(), 2);
        let error = changed
            .expect_err("a changed clipboard was pasted")
            .to_string();
        assert!(error.contains("clipboard changed"), "{error}");
    }

    #[test]
    fn an_unused_source_releases_its_marshal_data() {
        let dir = tempfile::tempdir().unwrap();
        let zip = write_test_zip(dir.path());
        let _ole = OleApartment::new().unwrap();
        let data = shell_data_object(&format!("{}\\a.txt", zip.display()));
        let source = VirtualSource::from_data_object(&data).unwrap();
        // Dropped on another thread without COM: released through this thread, which pumps.
        let dropper = std::thread::spawn(move || drop(source));
        pump_until(|| dropper.is_finished());
        dropper.join().unwrap();
        drop(VirtualSource::from_data_object(&data).unwrap());
    }

    #[test]
    fn a_panic_in_the_callback_reaches_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let zip = write_test_zip(dir.path());
        let _ole = OleApartment::new().unwrap();
        let data = shell_data_object(&format!("{}\\a.txt", zip.display()));
        let source = VirtualSource::from_data_object(&data).unwrap();
        let reader = std::thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = with_virtual_files(source, |_| -> () { panic!("boom") });
            }))
            .is_err()
        });
        pump_until(|| reader.is_finished());
        assert!(reader.join().unwrap());
    }

    #[test]
    fn an_index_past_the_end_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let zip = write_test_zip(dir.path());
        let _ole = OleApartment::new().unwrap();
        let data = shell_data_object(&format!("{}\\a.txt", zip.display()));
        let source = VirtualSource::from_data_object(&data).unwrap();
        let reader = std::thread::spawn(move || {
            with_virtual_files(source, |files| {
                (files.items().len(), files.open(99).is_err())
            })
        });
        pump_until(|| reader.is_finished());
        assert_eq!(reader.join().unwrap().unwrap(), (1, true));
    }
}
