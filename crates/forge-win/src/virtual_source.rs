//! ZXraw's own virtual files: a data object offering `FileGroupDescriptorW` and one
//! `FileContents` stream per file, read on demand from a [`VirtualProvider`]. This is how
//! items that are not files (entries of an archive) are dragged or pasted into Explorer, the
//! desktop or a mail client, the way Explorer's own zip folders offer theirs.
//!
//! * The data object lives on the UI thread (the drag and the OLE clipboard need an STA).
//!   Its formats are answered at once. Each `FileContents` stream is marshalled from the
//!   multithreaded apartment, so the target's reads run on COM worker threads and may wait
//!   for the data without blocking the window.
//! * `IDataObjectAsyncCapability` is offered: Explorer then copies after the drop returned,
//!   with its own progress dialog, and tells the result through `EndOperation`.
//! * A move (Cut, or a Shift-drag) is confirmed only when the target reports it ("Paste
//!   Succeeded", `EndOperation` or the drag's result) *and* read every file to its end (see
//!   [`move_confirmed`]). Only then is [`VirtualProvider::moved`] called, once.
//! * ZXraw's own drop target recognizes the object by a private format holding the
//!   listed paths and this process id (see [`own_paths`]), so a drag back into the app is
//!   handled as an in-app drag.

use std::io::Read;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::file_descriptor::DescriptorItem;

/// One file or folder offered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualItem {
    /// Relative path the target creates, `\`-separated; parents come before children.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// Where the data comes from. Called on COM worker threads, never the UI thread.
pub trait VirtualProvider: Send + Sync + 'static {
    /// The contents of file `index` (of [`VirtualOffer::items`]). May block until the data
    /// is ready. The reader must fail rather than end when the data can't be verified.
    fn open(&self, index: usize) -> std::io::Result<Box<dyn Read + Send>>;
    /// The target confirmed a move of every item: the source may now remove them. Called
    /// at most once, on the thread that owns the data object.
    fn moved(&self);
}

/// What a data object offers.
#[derive(Clone, Debug, Default)]
pub struct VirtualOffer {
    pub items: Vec<VirtualItem>,
    /// "Preferred DropEffect" is Move (a Cut).
    pub preferred_move: bool,
    /// The paths as this app lists them, for its own drop target.
    pub own_paths: Vec<PathBuf>,
}

/// What the target reported, as raw `DROPEFFECT` values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TargetReport {
    pub paste_succeeded: Option<u32>,
    pub performed: Option<u32>,
    /// `EndOperation`: whether it succeeded, and the effect.
    pub async_end: Option<(bool, u32)>,
    /// What the modal drag returned, when the target was not still copying.
    pub drag_returned: Option<u32>,
    /// Every file was read to its end without an error.
    pub all_served: bool,
}

const EFFECT_COPY: u32 = 1;
const EFFECT_MOVE: u32 = 2;

/// Whether `report` confirms a finished move: the target said Move, and it read every file.
pub fn move_confirmed(report: &TargetReport) -> bool {
    let moved = |effect: u32| effect & EFFECT_MOVE != 0;
    report.all_served
        && (report.paste_succeeded.is_some_and(moved)
            || report.async_end.is_some_and(|(ok, effect)| ok && moved(effect))
            || (report.async_end.is_none() && report.drag_returned.is_some_and(moved)))
}

/// The paths a data object made by this process offers its own drop target, from the bytes
/// of its private format. `None` for another process's object or bad bytes.
pub fn own_paths(bytes: &[u8]) -> Option<Vec<PathBuf>> {
    let pid = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    if pid != std::process::id() {
        return None;
    }
    let units: Vec<u16> = bytes[4..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let paths: Vec<PathBuf> = units
        .split(|&u| u == 0)
        .take_while(|s| !s.is_empty())
        .map(|s| PathBuf::from(String::from_utf16_lossy(s)))
        .collect();
    (!paths.is_empty()).then_some(paths)
}

fn own_bytes(paths: &[PathBuf]) -> Vec<u8> {
    let mut bytes = std::process::id().to_le_bytes().to_vec();
    for p in paths {
        for unit in p.to_string_lossy().encode_utf16().chain(Some(0)) {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes
}

fn descriptor(items: &[VirtualItem]) -> crate::Result<Vec<u8>> {
    let items: Vec<DescriptorItem> = items
        .iter()
        .map(|i| DescriptorItem {
            name: i.path.clone(),
            is_dir: i.is_dir,
            size: i.size,
            modified: i.modified,
        })
        .collect();
    crate::file_descriptor::build_file_group_descriptor_w(&items)
}

/// The name of the private format that marks ZXraw's own archive items.
#[cfg(windows)]
pub(crate) fn own_format() -> crate::Result<u16> {
    crate::virtual_files::registered_format(
        windows::core::w!("ZXraw Archive Items"),
        "RegisterClipboardFormatW(ZXraw Archive Items)",
    )
}

#[cfg(windows)]
pub use imp::VirtualDataObject;

/// Drops `data` on the shell's own drop target of `folder` (what Explorer and the desktop
/// use), allowing only `effect`, with the left button as a drop has it. Returns at once; the
/// shell copies on its own threads while the calling thread keeps pumping messages.
#[cfg(windows)]
fn shell_drop(
    data: &windows::Win32::System::Com::IDataObject,
    folder: &std::path::Path,
    effect: u32,
) -> crate::Result<()> {
    use windows::Win32::Foundation::POINTL;
    use windows::Win32::System::Com::IBindCtx;
    use windows::Win32::System::Ole::{DROPEFFECT, IDropTarget};
    use windows::Win32::System::SystemServices::MK_LBUTTON;
    use windows::Win32::UI::Shell::{BHID_SFUIObject, IShellItem, SHCreateItemFromParsingName};
    use windows::core::PCWSTR;

    let wide = crate::to_wide(folder);
    // SAFETY: plain shell and OLE calls on this (OLE) thread with live objects.
    unsafe {
        let item: IShellItem =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>)
                .map_err(|e| crate::api_err("SHCreateItemFromParsingName", e))?;
        let target: IDropTarget = item
            .BindToHandler(None::<&IBindCtx>, &BHID_SFUIObject)
            .map_err(|e| crate::api_err("BindToHandler(IDropTarget)", e))?;
        let mut answer = DROPEFFECT(effect);
        target
            .DragEnter(data, MK_LBUTTON, POINTL::default(), &mut answer)
            .map_err(|e| crate::api_err("IDropTarget::DragEnter", e))?;
        answer = DROPEFFECT(effect);
        target
            .Drop(data, MK_LBUTTON, POINTL::default(), &mut answer)
            .map_err(|e| crate::api_err("IDropTarget::Drop", e))?;
    }
    Ok(())
}

/// Stand-in so callers compile everywhere.
#[cfg(not(windows))]
pub struct VirtualDataObject;

#[cfg(not(windows))]
impl VirtualDataObject {
    pub fn new(
        _offer: VirtualOffer,
        _provider: std::sync::Arc<dyn VirtualProvider>,
    ) -> crate::Result<Self> {
        let _ = descriptor(&[])?;
        Err(crate::WinError::Unsupported)
    }
    pub fn set_clipboard(&self) -> crate::Result<()> {
        Err(crate::WinError::Unsupported)
    }
    pub fn is_on_clipboard(&self) -> bool {
        false
    }
    pub fn clear_clipboard_if_current(&self) {}
    pub fn shell_paste_into(&self, _folder: &std::path::Path) -> crate::Result<()> {
        Err(crate::WinError::Unsupported)
    }
    pub fn report(&self) -> TargetReport {
        TargetReport::default()
    }
}

#[cfg(windows)]
mod imp {
    use std::io::Read;
    use std::mem::ManuallyDrop;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

    use windows::Win32::Foundation::{
        DATA_S_SAMEFORMATETC, DV_E_DVASPECT, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_FAIL,
        E_INVALIDARG, E_NOTIMPL, E_POINTER, FILETIME, GlobalFree, HGLOBAL, OLE_E_ADVISENOTSUPPORTED,
        S_OK, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION,
    };
    use windows::Win32::System::Com::{
        DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
        IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE,
        STATFLAG, STATFLAG_NONAME, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STGM_READ, STGTY_STREAM,
        STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL,
        TYMED_ISTREAM,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::UI::Shell::{IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl};
    use windows::core::{BOOL, HRESULT, Interface, Ref, implement};

    use super::{TargetReport, VirtualItem, VirtualOffer, VirtualProvider, move_confirmed};

    struct Formats {
        descriptor: u16,
        contents: u16,
        preferred: u16,
        paste_succeeded: u16,
        performed: u16,
        own: u16,
    }

    struct Shared {
        items: Vec<VirtualItem>,
        descriptor: Vec<u8>,
        preferred: u32,
        own: Vec<u8>,
        formats: Formats,
        provider: Arc<dyn VirtualProvider>,
        /// Formats targets and the drag image helper set (`SetData`), as bytes.
        stored: Mutex<Vec<(u16, Vec<u8>)>>,
        /// Per item: read to its end (folders and empty files from the start).
        served: Mutex<Vec<bool>>,
        async_mode: AtomicBool,
        in_operation: AtomicBool,
        report: Mutex<TargetReport>,
        moved_told: AtomicBool,
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|p| p.into_inner())
    }

    impl Shared {
        fn snapshot(&self) -> TargetReport {
            let mut report = *lock(&self.report);
            report.all_served = lock(&self.served).iter().all(|&s| s);
            report
        }

        /// Tells the provider once the target confirmed a move.
        fn check_move(&self) {
            if move_confirmed(&self.snapshot()) && !self.moved_told.swap(true, Ordering::SeqCst)
            {
                self.provider.moved();
            }
        }

        fn hglobal_bytes(&self, format: u16) -> Option<Vec<u8>> {
            let f = &self.formats;
            if format == f.descriptor {
                Some(self.descriptor.clone())
            } else if format == f.preferred {
                Some(self.preferred.to_le_bytes().to_vec())
            } else if format == f.own {
                Some(self.own.clone())
            } else {
                lock(&self.stored)
                    .iter()
                    .find(|(cf, _)| *cf == format)
                    .map(|(_, b)| b.clone())
            }
        }
    }

    /// Keeps the multithreaded apartment alive for the process, so streams marshalled from
    /// it stay reachable after the thread that marshalled them is gone.
    fn keep_mta() {
        static MTA: OnceLock<()> = OnceLock::new();
        MTA.get_or_init(|| {
            // SAFETY: a plain call; the cookie is never released (the MTA stays up).
            if let Err(e) = unsafe { windows::Win32::System::Com::CoIncrementMTAUsage() } {
                tracing::warn!("CoIncrementMTAUsage failed: {e}");
            }
        });
    }

    struct SendStream(IStream);
    // SAFETY: the stream is our own Rust object (thread-safe, its state behind a mutex) or
    // a marshal stream, which is free-threaded.
    unsafe impl Send for SendStream {}

    /// A proxy, for the calling apartment, to `stream` exported from the multithreaded
    /// apartment: calls on it (from here, or from another process it is passed to) run on
    /// COM worker threads instead of the calling thread.
    fn mta_proxy(stream: IStream) -> windows::core::Result<IStream> {
        use windows::Win32::System::Com::Marshal::CoMarshalInterThreadInterfaceInStream;
        use windows::Win32::System::Com::StructuredStorage::CoGetInterfaceAndReleaseStream;
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

        keep_mta();
        let stream = SendStream(stream);
        let marshalled = std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let stream = stream;
                    // SAFETY: balanced below; the MTA itself is kept alive by `keep_mta`.
                    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
                    // SAFETY: `stream.0` is a live interface.
                    let r = unsafe { CoMarshalInterThreadInterfaceInStream(&IStream::IID, &stream.0) };
                    unsafe { CoUninitialize() };
                    r.map(SendStream)
                })
                .join()
                .unwrap_or_else(|_| Err(E_FAIL.into()))
        })?;
        // SAFETY: the stream holds marshal data from CoMarshalInterThreadInterfaceInStream.
        unsafe { CoGetInterfaceAndReleaseStream(&marshalled.0) }
    }

    /// An HGLOBAL medium holding a copy of `bytes`.
    fn hglobal_medium(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
        // SAFETY: a fresh block, filled while locked; the medium owns it.
        unsafe {
            let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
            let ptr = GlobalLock(global);
            if ptr.is_null() {
                let _ = GlobalFree(Some(global));
                return Err(windows::core::Error::from_thread());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(global);
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: global },
                pUnkForRelease: ManuallyDrop::new(None),
            })
        }
    }

    fn read_hglobal(global: HGLOBAL) -> windows::core::Result<Vec<u8>> {
        // SAFETY: GlobalSize and GlobalLock fail cleanly on a bad handle.
        unsafe {
            let size = GlobalSize(global);
            let ptr = GlobalLock(global);
            if ptr.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), size).to_vec();
            let _ = GlobalUnlock(global);
            Ok(bytes)
        }
    }

    fn dword(bytes: &[u8]) -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?))
    }

    #[implement(IDataObject, IDataObjectAsyncCapability)]
    struct DataObject {
        shared: Arc<Shared>,
    }

    #[allow(non_snake_case)]
    impl IDataObject_Impl for DataObject_Impl {
        fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
            // SAFETY: OLE passes a valid FORMATETC or null.
            let f = unsafe { pformatetcin.as_ref() }.ok_or(E_POINTER)?;
            if f.dwAspect != DVASPECT_CONTENT.0 {
                return Err(DV_E_DVASPECT.into());
            }
            let s = &self.shared;
            if f.cfFormat == s.formats.contents {
                if f.tymed & TYMED_ISTREAM.0 as u32 == 0 {
                    return Err(DV_E_TYMED.into());
                }
                let index = usize::try_from(f.lindex).map_err(|_| DV_E_LINDEX)?;
                if s.items.get(index).is_none_or(|i| i.is_dir) {
                    return Err(DV_E_LINDEX.into());
                }
                let stream: IStream = ContentStream {
                    shared: s.clone(),
                    index,
                    state: Mutex::new(StreamState::default()),
                }
                .into();
                let proxy = mta_proxy(stream)?;
                return Ok(STGMEDIUM {
                    tymed: TYMED_ISTREAM.0 as u32,
                    u: STGMEDIUM_0 {
                        pstm: ManuallyDrop::new(Some(proxy)),
                    },
                    pUnkForRelease: ManuallyDrop::new(None),
                });
            }
            if f.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
                return Err(DV_E_TYMED.into());
            }
            let bytes = s.hglobal_bytes(f.cfFormat).ok_or(DV_E_FORMATETC)?;
            hglobal_medium(&bytes)
        }

        fn GetDataHere(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *mut STGMEDIUM,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
            // SAFETY: as in GetData.
            let Some(f) = (unsafe { pformatetc.as_ref() }) else {
                return E_POINTER;
            };
            if f.dwAspect != DVASPECT_CONTENT.0 {
                return DV_E_DVASPECT;
            }
            let s = &self.shared;
            if f.cfFormat == s.formats.contents {
                return if f.tymed & TYMED_ISTREAM.0 as u32 == 0 {
                    DV_E_TYMED
                } else {
                    S_OK
                };
            }
            if s.hglobal_bytes(f.cfFormat).is_none() {
                DV_E_FORMATETC
            } else if f.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
                DV_E_TYMED
            } else {
                S_OK
            }
        }

        fn GetCanonicalFormatEtc(
            &self,
            _pformatectin: *const FORMATETC,
            pformatetcout: *mut FORMATETC,
        ) -> HRESULT {
            // SAFETY: OLE passes a valid out pointer or null.
            if let Some(out) = unsafe { pformatetcout.as_mut() } {
                out.ptd = std::ptr::null_mut();
            }
            DATA_S_SAMEFORMATETC
        }

        fn SetData(
            &self,
            pformatetc: *const FORMATETC,
            pmedium: *const STGMEDIUM,
            frelease: BOOL,
        ) -> windows::core::Result<()> {
            // SAFETY: OLE passes valid pointers or null.
            let (f, medium) = unsafe { (pformatetc.as_ref(), pmedium.as_ref()) };
            let (f, medium) = f.zip(medium).ok_or(E_INVALIDARG)?;
            if medium.tymed != TYMED_HGLOBAL.0 as u32 {
                return Err(E_NOTIMPL.into());
            }
            // SAFETY: the medium says it holds a global handle.
            let bytes = read_hglobal(unsafe { medium.u.hGlobal })?;
            if frelease.as_bool() {
                // SAFETY: with fRelease the medium is ours to release, once.
                unsafe { windows::Win32::System::Ole::ReleaseStgMedium(pmedium.cast_mut()) };
            }
            let s = &self.shared;
            let format = f.cfFormat;
            if format == s.formats.paste_succeeded {
                lock(&s.report).paste_succeeded = dword(&bytes);
            } else if format == s.formats.performed {
                lock(&s.report).performed = dword(&bytes);
            }
            {
                let mut stored = lock(&s.stored);
                stored.retain(|(cf, _)| *cf != format);
                stored.push((format, bytes));
            }
            if format == s.formats.paste_succeeded {
                s.check_move();
            }
            Ok(())
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
            if dwdirection != DATADIR_GET.0 as u32 {
                return Err(E_NOTIMPL.into());
            }
            let s = &self.shared;
            let format = |cf: u16, tymed: i32| FORMATETC {
                cfFormat: cf,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: tymed as u32,
            };
            let mut formats = vec![
                format(s.formats.descriptor, TYMED_HGLOBAL.0),
                format(s.formats.contents, TYMED_ISTREAM.0),
                format(s.formats.preferred, TYMED_HGLOBAL.0),
                format(s.formats.own, TYMED_HGLOBAL.0),
            ];
            formats.extend(
                lock(&s.stored)
                    .iter()
                    .map(|(cf, _)| format(*cf, TYMED_HGLOBAL.0)),
            );
            // SAFETY: `formats` is a valid slice; the enumerator copies it.
            unsafe { windows::Win32::UI::Shell::SHCreateStdEnumFmtEtc(&formats) }
        }

        fn DAdvise(
            &self,
            _pformatetc: *const FORMATETC,
            _advf: u32,
            _padvsink: Ref<IAdviseSink>,
        ) -> windows::core::Result<u32> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }
    }

    #[allow(non_snake_case)]
    impl IDataObjectAsyncCapability_Impl for DataObject_Impl {
        fn SetAsyncMode(&self, fdoopasync: BOOL) -> windows::core::Result<()> {
            self.shared
                .async_mode
                .store(fdoopasync.as_bool(), Ordering::SeqCst);
            Ok(())
        }

        fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
            Ok(self.shared.async_mode.load(Ordering::SeqCst).into())
        }

        fn StartOperation(
            &self,
            _pbcreserved: Ref<windows::Win32::System::Com::IBindCtx>,
        ) -> windows::core::Result<()> {
            self.shared.in_operation.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn InOperation(&self) -> windows::core::Result<BOOL> {
            Ok(self.shared.in_operation.load(Ordering::SeqCst).into())
        }

        fn EndOperation(
            &self,
            hresult: HRESULT,
            _pbcreserved: Ref<windows::Win32::System::Com::IBindCtx>,
            dweffects: u32,
        ) -> windows::core::Result<()> {
            let s = &self.shared;
            s.in_operation.store(false, Ordering::SeqCst);
            lock(&s.report).async_end = Some((hresult.is_ok(), dweffects));
            s.check_move();
            Ok(())
        }
    }

    #[derive(Default)]
    struct StreamState {
        reader: Option<Box<dyn Read + Send>>,
        /// Where the reader is.
        at: u64,
        /// Where the next read starts (seeks only move this).
        want: u64,
        failed: bool,
    }

    #[implement(IStream)]
    struct ContentStream {
        shared: Arc<Shared>,
        index: usize,
        state: Mutex<StreamState>,
    }

    impl ContentStream {
        fn size(&self) -> u64 {
            self.shared.items[self.index].size
        }

        /// Fills `buf` from the current position; fewer bytes only at the end.
        fn read_into(&self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut st = lock(&self.state);
            if st.failed {
                return Err(std::io::Error::other("an earlier read failed"));
            }
            let result = self.read_locked(&mut st, buf);
            match &result {
                Err(_) => st.failed = true,
                Ok(n) if *n < buf.len() && st.at == self.size() => {
                    lock(&self.shared.served)[self.index] = true;
                }
                Ok(_) => {}
            }
            result
        }

        fn read_locked(&self, st: &mut StreamState, buf: &mut [u8]) -> std::io::Result<usize> {
            if st.reader.is_none() || st.want < st.at {
                st.reader = Some(self.shared.provider.open(self.index)?);
                st.at = 0;
            }
            let reader = st.reader.as_mut().expect("opened above");
            // A seek forward: skip to it.
            let mut scratch = [0u8; 64 * 1024];
            while st.at < st.want {
                let n = (st.want - st.at).min(scratch.len() as u64) as usize;
                match reader.read(&mut scratch[..n]) {
                    Ok(0) => break,
                    Ok(got) => st.at += got as u64,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            if st.at < st.want {
                return Ok(0); // past the end
            }
            let mut got = 0;
            while got < buf.len() {
                match reader.read(&mut buf[got..]) {
                    Ok(0) => break,
                    Ok(n) => got += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            st.at += got as u64;
            st.want = st.at;
            Ok(got)
        }
    }

    #[allow(non_snake_case)]
    impl ISequentialStream_Impl for ContentStream_Impl {
        fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
            if pv.is_null() {
                return E_POINTER;
            }
            // SAFETY: the caller gives a buffer of `cb` bytes.
            let buf = unsafe { std::slice::from_raw_parts_mut(pv.cast::<u8>(), cb as usize) };
            let (n, hr) = match self.read_into(buf) {
                Ok(n) => (n, S_OK),
                Err(e) => {
                    tracing::debug!("virtual file {} failed: {e}", self.index);
                    (0, E_FAIL)
                }
            };
            // SAFETY: an optional out pointer.
            if let Some(out) = unsafe { pcbread.as_mut() } {
                *out = n as u32;
            }
            hr
        }

        fn Write(&self, _pv: *const core::ffi::c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
            STG_E_ACCESSDENIED
        }
    }

    fn filetime(time: Option<std::time::SystemTime>) -> FILETIME {
        const EPOCH: u64 = 11_644_473_600 * 10_000_000;
        let ticks = time
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| EPOCH + d.as_secs() * 10_000_000 + u64::from(d.subsec_nanos() / 100));
        FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        }
    }

    #[allow(non_snake_case)]
    impl IStream_Impl for ContentStream_Impl {
        fn Seek(
            &self,
            dlibmove: i64,
            dworigin: STREAM_SEEK,
            plibnewposition: *mut u64,
        ) -> windows::core::Result<()> {
            let mut st = lock(&self.state);
            let base = match dworigin {
                STREAM_SEEK_SET => 0,
                STREAM_SEEK_CUR => st.want,
                STREAM_SEEK_END => self.size(),
                _ => return Err(STG_E_INVALIDFUNCTION.into()),
            };
            let to = base
                .checked_add_signed(dlibmove)
                .ok_or(STG_E_INVALIDFUNCTION)?;
            st.want = to;
            // SAFETY: an optional out pointer.
            if let Some(out) = unsafe { plibnewposition.as_mut() } {
                *out = to;
            }
            Ok(())
        }

        fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
            Err(STG_E_ACCESSDENIED.into())
        }

        fn CopyTo(
            &self,
            pstm: Ref<IStream>,
            cb: u64,
            pcbread: *mut u64,
            pcbwritten: *mut u64,
        ) -> windows::core::Result<()> {
            let target = pstm.ok()?;
            let mut buf = vec![0u8; 256 * 1024];
            let (mut read, mut written) = (0u64, 0u64);
            let result = loop {
                let want = (cb - read).min(buf.len() as u64) as usize;
                if want == 0 {
                    break Ok(());
                }
                let n = match self.read_into(&mut buf[..want]) {
                    Ok(0) => break Ok(()),
                    Ok(n) => n,
                    Err(_) => break Err(E_FAIL.into()),
                };
                read += n as u64;
                let mut done = 0u32;
                // SAFETY: `buf` holds `n` bytes.
                let hr = unsafe { target.Write(buf.as_ptr().cast(), n as u32, Some(&mut done)) };
                written += u64::from(done);
                if hr.is_err() {
                    break Err(hr.into());
                }
            };
            // SAFETY: optional out pointers.
            unsafe {
                if let Some(out) = pcbread.as_mut() {
                    *out = read;
                }
                if let Some(out) = pcbwritten.as_mut() {
                    *out = written;
                }
            }
            result
        }

        fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
            Ok(())
        }

        fn Revert(&self) -> windows::core::Result<()> {
            Ok(())
        }

        fn LockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: &LOCKTYPE) -> windows::core::Result<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> windows::core::Result<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn Stat(&self, pstatstg: *mut STATSTG, grfstatflag: &STATFLAG) -> windows::core::Result<()> {
            // SAFETY: OLE passes a valid out pointer or null.
            let out = unsafe { pstatstg.as_mut() }.ok_or(E_POINTER)?;
            let item = &self.shared.items[self.index];
            let mtime = filetime(item.modified);
            *out = STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: item.size,
                mtime,
                ctime: mtime,
                atime: mtime,
                grfMode: STGM_READ,
                ..Default::default()
            };
            if *grfstatflag != STATFLAG_NONAME {
                let name = item.path.rsplit('\\').next().unwrap_or(&item.path);
                let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
                // SAFETY: a CoTaskMem block the caller frees, filled to its size.
                unsafe {
                    let ptr = windows::Win32::System::Com::CoTaskMemAlloc(wide.len() * 2).cast::<u16>();
                    if !ptr.is_null() {
                        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                        out.pwcsName = windows::core::PWSTR(ptr);
                    }
                }
            }
            Ok(())
        }

        fn Clone(&self) -> windows::core::Result<IStream> {
            Err(E_NOTIMPL.into())
        }
    }

    /// ZXraw's virtual-file data object (see the module docs). Use it on the thread that
    /// made it (the UI thread, where OLE is initialized).
    pub struct VirtualDataObject {
        obj: IDataObject,
        shared: Arc<Shared>,
    }

    impl VirtualDataObject {
        pub fn new(
            offer: VirtualOffer,
            provider: Arc<dyn VirtualProvider>,
        ) -> crate::Result<Self> {
            use crate::virtual_files::registered_format;
            use windows::core::w;

            let descriptor = super::descriptor(&offer.items)?;
            let formats = Formats {
                descriptor: crate::virtual_files::descriptor_format()?,
                contents: crate::virtual_files::contents_format()?,
                preferred: registered_format(w!("Preferred DropEffect"), "RegisterClipboardFormatW(Preferred DropEffect)")?,
                paste_succeeded: registered_format(w!("Paste Succeeded"), "RegisterClipboardFormatW(Paste Succeeded)")?,
                performed: registered_format(w!("Performed DropEffect"), "RegisterClipboardFormatW(Performed DropEffect)")?,
                own: super::own_format()?,
            };
            let served = offer
                .items
                .iter()
                .map(|i| i.is_dir || i.size == 0)
                .collect();
            let shared = Arc::new(Shared {
                descriptor,
                preferred: if offer.preferred_move {
                    super::EFFECT_MOVE
                } else {
                    super::EFFECT_COPY
                },
                own: super::own_bytes(&offer.own_paths),
                items: offer.items,
                formats,
                provider,
                stored: Mutex::new(Vec::new()),
                served: Mutex::new(served),
                async_mode: AtomicBool::new(true),
                in_operation: AtomicBool::new(false),
                report: Mutex::new(TargetReport::default()),
                moved_told: AtomicBool::new(false),
            });
            let obj: IDataObject = DataObject {
                shared: shared.clone(),
            }
            .into();
            Ok(Self { obj, shared })
        }

        pub(crate) fn data_object(&self) -> &IDataObject {
            &self.obj
        }

        /// Puts the object on the clipboard (OLE must be initialized on this thread, which
        /// must keep pumping messages while it is there).
        pub fn set_clipboard(&self) -> crate::Result<()> {
            let mut last = None;
            // Another app may hold the clipboard open for a moment.
            for attempt in 0..10 {
                // SAFETY: a live data object; OLE AddRefs it.
                match unsafe { windows::Win32::System::Ole::OleSetClipboard(&self.obj) } {
                    Ok(()) => return Ok(()),
                    Err(e) => last = Some(e),
                }
                std::thread::sleep(std::time::Duration::from_millis(10 * (attempt + 1)));
            }
            Err(crate::api_err(
                "OleSetClipboard",
                last.unwrap_or_else(|| E_FAIL.into()),
            ))
        }

        /// Whether the clipboard still holds this object.
        pub fn is_on_clipboard(&self) -> bool {
            is_current_raw(&self.obj)
        }

        /// Empties the clipboard if it still holds this object (its streams can't outlive
        /// the app, so it is never flushed).
        pub fn clear_clipboard_if_current(&self) {
            if is_current_raw(&self.obj) {
                // SAFETY: plain OLE call; releases our object and empties the clipboard.
                let _ = unsafe { windows::Win32::System::Ole::OleSetClipboard(None::<&IDataObject>) };
            }
        }

        /// Pastes this object, while it is on the clipboard, into `folder` as Explorer's Paste
        /// does: the folder's shell drop target, allowed only the "Preferred DropEffect" (Move
        /// for a Cut). Returns at once; keep pumping messages while the shell copies. For
        /// automation. The object itself is dropped: in this process the shell does not read
        /// virtual files through the `OleGetClipboard` wrapper (Explorer, in its own process,
        /// reaches this same object through its proxy).
        pub fn shell_paste_into(&self, folder: &std::path::Path) -> crate::Result<()> {
            if !self.is_on_clipboard() {
                return Err(crate::WinError::Invalid(
                    "these items are no longer on the clipboard".into(),
                ));
            }
            super::shell_drop(&self.obj, folder, self.shared.preferred)
        }

        /// The modal drag returned `effect`.
        pub fn drag_returned(&self, effect: u32) {
            if !self.shared.in_operation.load(Ordering::SeqCst) {
                lock(&self.shared.report).drag_returned = Some(effect);
                self.shared.check_move();
            }
        }

        /// What the target reported so far.
        pub fn report(&self) -> TargetReport {
            self.shared.snapshot()
        }
    }

    /// `OleIsCurrentClipboard` is S_OK only for the current object (S_FALSE otherwise,
    /// which the `Result` wrapper also reports as success), so it is called raw.
    fn is_current_raw(obj: &IDataObject) -> bool {
        windows::core::link!("ole32.dll" "system" fn OleIsCurrentClipboard(pdataobj : *mut core::ffi::c_void) -> HRESULT);
        // SAFETY: a live interface pointer for the duration of the call.
        unsafe { OleIsCurrentClipboard(obj.as_raw()) == S_OK }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_needs_the_targets_word_and_every_file_read() {
        let served = TargetReport {
            all_served: true,
            ..Default::default()
        };
        assert!(!move_confirmed(&served));
        assert!(move_confirmed(&TargetReport {
            paste_succeeded: Some(EFFECT_MOVE),
            ..served
        }));
        // A copy, or a move whose files weren't all read, deletes nothing.
        assert!(!move_confirmed(&TargetReport {
            paste_succeeded: Some(EFFECT_COPY),
            ..served
        }));
        assert!(!move_confirmed(&TargetReport {
            paste_succeeded: Some(EFFECT_MOVE),
            all_served: false,
            ..Default::default()
        }));
        // "Performed DropEffect" alone is not a confirmation.
        assert!(!move_confirmed(&TargetReport {
            performed: Some(EFFECT_MOVE),
            ..served
        }));
        assert!(move_confirmed(&TargetReport {
            async_end: Some((true, EFFECT_MOVE)),
            ..served
        }));
        assert!(!move_confirmed(&TargetReport {
            async_end: Some((false, EFFECT_MOVE)),
            ..served
        }));
        assert!(move_confirmed(&TargetReport {
            drag_returned: Some(EFFECT_MOVE),
            ..served
        }));
        // An asynchronous drop that failed overrides what the drag returned.
        assert!(!move_confirmed(&TargetReport {
            drag_returned: Some(EFFECT_MOVE),
            async_end: Some((false, EFFECT_MOVE)),
            ..served
        }));
    }

    #[test]
    fn own_paths_round_trip_only_in_this_process() {
        let paths = vec![PathBuf::from(r"D:\x\a.zip\sub\b.txt"), PathBuf::from(r"D:\x\a.zip\c")];
        assert_eq!(own_paths(&own_bytes(&paths)), Some(paths.clone()));
        let mut other = own_bytes(&paths);
        other[..4].copy_from_slice(&(std::process::id() ^ 1).to_le_bytes());
        assert_eq!(own_paths(&other), None);
        assert_eq!(own_paths(&[1, 2]), None);
    }
}

#[cfg(all(test, windows))]
#[path = "virtual_source_tests.rs"]
mod com_tests;
