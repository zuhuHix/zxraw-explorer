//! COM tests of [`super`]: ZXraw's data object read back through PR #41's virtual-file
//! reader, the clipboard, and the shell's own folder drop target (Explorer's consumer).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use super::*;
use crate::virtual_files::test_support::pump_until;
use crate::virtual_files::{
    OleApartment, VirtualContent, VirtualFiles, VirtualSource, with_virtual_files,
};

/// Serves fixed contents; file `bad` fails after its first half, as a checksum failure at
/// the end of a stream does.
struct Fixed {
    contents: Vec<Option<Vec<u8>>>,
    bad: Option<usize>,
    moved: AtomicUsize,
    opened: Mutex<Vec<usize>>,
}

impl VirtualProvider for Fixed {
    fn open(&self, index: usize) -> std::io::Result<Box<dyn Read + Send>> {
        self.opened.lock().unwrap().push(index);
        let data = self.contents[index].clone().expect("a file");
        if self.bad == Some(index) {
            let head = data[..data.len() / 2].to_vec();
            return Ok(Box::new(std::io::Cursor::new(head).chain(Failing)));
        }
        Ok(Box::new(std::io::Cursor::new(data)))
    }
    fn moved(&self) {
        self.moved.fetch_add(1, Ordering::SeqCst);
    }
}

struct Failing;

impl Read for Failing {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("checksum mismatch"))
    }
}

fn big() -> Vec<u8> {
    (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect()
}

/// `docs`, `docs\a.txt`, `docs\sub`, `docs\sub\big.bin` and, when `with_bad`, `bad.bin`
/// whose stream fails.
fn offer(with_bad: bool, preferred_move: bool) -> (VirtualOffer, Arc<Fixed>) {
    let mut items = vec![
        ("docs", None),
        (r"docs\a.txt", Some(b"hello".to_vec())),
        (r"docs\sub", None),
        (r"docs\sub\big.bin", Some(big())),
    ];
    if with_bad {
        items.push(("bad.bin", Some(b"0123456789".to_vec())));
    }
    let provider = Arc::new(Fixed {
        bad: with_bad.then_some(4),
        contents: items.iter().map(|(_, c)| c.clone()).collect(),
        moved: AtomicUsize::new(0),
        opened: Mutex::new(Vec::new()),
    });
    let modified = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let offer = VirtualOffer {
        items: items
            .iter()
            .map(|(path, c)| VirtualItem {
                path: path.to_string(),
                is_dir: c.is_none(),
                size: c.as_ref().map_or(0, |c| c.len() as u64),
                modified: Some(modified),
            })
            .collect(),
        preferred_move,
        own_paths: vec![PathBuf::from(r"D:\x\a.zip\docs")],
    };
    (offer, provider)
}

/// Every item's name, kind and, for files, its contents or the read error.
type ReadBack = Vec<(String, bool, Option<Result<Vec<u8>, String>>)>;

fn read_all(files: &VirtualFiles) -> ReadBack {
    files
        .items()
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let content = (!item.is_dir).then(|| match files.open(i) {
                Ok(VirtualContent::Stream(mut r)) => {
                    let mut bytes = Vec::new();
                    r.read_to_end(&mut bytes)
                        .map(|_| bytes)
                        .map_err(|e| e.to_string())
                }
                Ok(VirtualContent::Storage(_)) => Err("a storage".into()),
                Err(e) => Err(e.to_string()),
            });
            (item.name.clone(), item.is_dir, content)
        })
        .collect()
}

/// Reads `data` through the virtual-file reader on another thread, serving its calls here.
fn read_back(data: &VirtualDataObject) -> ReadBack {
    let source = VirtualSource::from_data_object(data.data_object()).unwrap();
    let reader = std::thread::spawn(move || with_virtual_files(source, read_all));
    pump_until(|| reader.is_finished());
    reader.join().unwrap().unwrap()
}

fn set_dword(data: &VirtualDataObject, name: windows::core::PCWSTR, value: u32) {
    let format = crate::virtual_files::registered_format(name, "register").unwrap();
    crate::virtual_files::set_hglobal(data.data_object(), format, &value.to_le_bytes()).unwrap();
}

#[test]
fn the_virtual_files_read_back_through_the_virtual_file_reader() {
    let _ole = OleApartment::new().unwrap();
    let (offer, provider) = offer(true, false);
    let data = VirtualDataObject::new(offer, provider.clone()).unwrap();
    let items = read_back(&data);
    let names: Vec<(&str, bool)> = items.iter().map(|(n, d, _)| (n.as_str(), *d)).collect();
    assert_eq!(
        names,
        [
            ("docs", true),
            (r"docs\a.txt", false),
            (r"docs\sub", true),
            (r"docs\sub\big.bin", false),
            ("bad.bin", false),
        ]
    );
    assert_eq!(items[1].2, Some(Ok(b"hello".to_vec())));
    assert_eq!(items[3].2, Some(Ok(big())));
    // A failed check is a read error, never a short file that looks complete.
    assert!(matches!(items[4].2, Some(Err(_))), "{:?}", items[4].2);
    assert!(!data.report().all_served);
    // A move reported now must not delete: not every file was read.
    set_dword(&data, windows::core::w!("Paste Succeeded"), EFFECT_MOVE);
    assert_eq!(provider.moved.load(Ordering::SeqCst), 0);
}

#[test]
fn a_move_is_confirmed_once_after_every_file_was_read() {
    let _ole = OleApartment::new().unwrap();
    let (offer, provider) = offer(false, true);
    let data = VirtualDataObject::new(offer, provider.clone()).unwrap();
    let items = read_back(&data);
    assert!(
        items
            .iter()
            .all(|(_, _, c)| c.as_ref().is_none_or(Result::is_ok))
    );
    assert!(data.report().all_served);
    set_dword(&data, windows::core::w!("Paste Succeeded"), EFFECT_MOVE);
    set_dword(&data, windows::core::w!("Paste Succeeded"), EFFECT_MOVE);
    assert_eq!(provider.moved.load(Ordering::SeqCst), 1);
}

#[test]
fn a_synchronous_copy_paste_decides_no_move() {
    let _ole = OleApartment::new().unwrap();
    let (offer, provider) = offer(false, true);
    let data = VirtualDataObject::new(offer, provider.clone()).unwrap();
    read_back(&data);
    set_dword(&data, windows::core::w!("Paste Succeeded"), EFFECT_COPY);
    // Decided: a later report changes nothing.
    set_dword(&data, windows::core::w!("Paste Succeeded"), EFFECT_MOVE);
    assert_eq!(provider.moved.load(Ordering::SeqCst), 0);
}

fn async_capability(data: &VirtualDataObject) -> windows::Win32::UI::Shell::IDataObjectAsyncCapability {
    use windows::core::Interface;
    data.data_object().cast().unwrap()
}

/// Reads every file, then plays an asynchronous paste that says `paste` along the way and
/// ends with `end`; returns how often the provider was told of a move.
fn async_paste(paste: Option<u32>, end: (windows::core::HRESULT, u32)) -> usize {
    let _ole = OleApartment::new().unwrap();
    let (offer, provider) = offer(false, true);
    let data = VirtualDataObject::new(offer, provider.clone()).unwrap();
    read_back(&data);
    let capability = async_capability(&data);
    // SAFETY: plain calls on a live interface.
    unsafe {
        capability.SetAsyncMode(true).unwrap();
        capability.StartOperation(None).unwrap();
    }
    if let Some(effect) = paste {
        set_dword(&data, windows::core::w!("Paste Succeeded"), effect);
        assert_eq!(provider.moved.load(Ordering::SeqCst), 0, "decided before the end");
    }
    // SAFETY: as above.
    unsafe { capability.EndOperation(end.0, None, end.1).unwrap() };
    // A second end changes nothing.
    unsafe {
        capability
            .EndOperation(windows::Win32::Foundation::S_OK, None, EFFECT_MOVE)
            .unwrap()
    };
    provider.moved.load(Ordering::SeqCst)
}

#[test]
fn an_asynchronous_paste_is_decided_only_at_its_end() {
    use windows::Win32::Foundation::{E_FAIL, S_OK};

    // "Paste Succeeded" said Move, then the operation failed: nothing moves.
    assert_eq!(async_paste(Some(EFFECT_MOVE), (E_FAIL, EFFECT_MOVE)), 0);
    // It ended as a copy.
    assert_eq!(async_paste(Some(EFFECT_MOVE), (S_OK, EFFECT_COPY)), 0);
    assert_eq!(async_paste(None, (S_OK, EFFECT_COPY)), 0);
    // It ended as a successful move: told once.
    assert_eq!(async_paste(Some(EFFECT_MOVE), (S_OK, EFFECT_MOVE)), 1);
}

/// A target stream that takes at most `take` bytes per write (0: none, yet says S_OK).
#[windows::core::implement(windows::Win32::System::Com::IStream)]
struct ShortWriter {
    take: usize,
    got: Arc<Mutex<Vec<u8>>>,
}

#[allow(non_snake_case)]
impl windows::Win32::System::Com::ISequentialStream_Impl for ShortWriter_Impl {
    fn Read(&self, _pv: *mut core::ffi::c_void, _cb: u32, _pcbread: *mut u32) -> windows::core::HRESULT {
        windows::Win32::Foundation::E_NOTIMPL
    }
    fn Write(&self, pv: *const core::ffi::c_void, cb: u32, pcbwritten: *mut u32) -> windows::core::HRESULT {
        let n = (cb as usize).min(self.take);
        // SAFETY: the caller passes `cb` readable bytes.
        let bytes = unsafe { std::slice::from_raw_parts(pv.cast::<u8>(), n) };
        self.got.lock().unwrap().extend_from_slice(bytes);
        // SAFETY: an optional out pointer.
        if let Some(out) = unsafe { pcbwritten.as_mut() } {
            *out = n as u32;
        }
        windows::Win32::Foundation::S_OK
    }
}

#[allow(non_snake_case)]
impl windows::Win32::System::Com::IStream_Impl for ShortWriter_Impl {
    fn Seek(&self, _m: i64, _o: windows::Win32::System::Com::STREAM_SEEK, _p: *mut u64) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn SetSize(&self, _n: u64) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn CopyTo(
        &self,
        _s: windows::core::Ref<windows::Win32::System::Com::IStream>,
        _cb: u64,
        _r: *mut u64,
        _w: *mut u64,
    ) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn Commit(&self, _f: &windows::Win32::System::Com::STGC) -> windows::core::Result<()> {
        Ok(())
    }
    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn LockRegion(&self, _o: u64, _cb: u64, _t: &windows::Win32::System::Com::LOCKTYPE) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn UnlockRegion(&self, _o: u64, _cb: u64, _t: u32) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn Stat(
        &self,
        _p: *mut windows::Win32::System::Com::STATSTG,
        _f: &windows::Win32::System::Com::STATFLAG,
    ) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn Clone(&self) -> windows::core::Result<windows::Win32::System::Com::IStream> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
}

/// `CopyTo` of the one file `one.bin` into a target taking `take` bytes per write: the
/// result, the counts it reported, what the target got, and whether the file was served.
fn copy_to_short_writer(take: usize) -> (windows::core::Result<()>, u64, u64, Vec<u8>, bool) {
    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IStream, TYMED_ISTREAM};

    let _ole = OleApartment::new().unwrap();
    let provider = Arc::new(Fixed {
        bad: None,
        contents: vec![Some(big())],
        moved: AtomicUsize::new(0),
        opened: Mutex::new(Vec::new()),
    });
    let offer = VirtualOffer {
        items: vec![VirtualItem {
            path: "one.bin".into(),
            is_dir: false,
            size: big().len() as u64,
            modified: None,
        }],
        preferred_move: true,
        own_paths: Vec::new(),
    };
    let data = VirtualDataObject::new(offer, provider).unwrap();
    let format = FORMATETC {
        cfFormat: crate::virtual_files::contents_format().unwrap(),
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: 0,
        tymed: TYMED_ISTREAM.0 as u32,
    };
    // SAFETY: a valid FORMATETC; the medium is released by the guard.
    let medium = crate::virtual_files::Medium(unsafe { data.data_object().GetData(&format) }.unwrap());
    // SAFETY: the medium holds a stream.
    let stream: IStream = unsafe { (*medium.0.u.pstm).clone() }.unwrap();
    let got = Arc::new(Mutex::new(Vec::new()));
    let writer = ShortWriter {
        take,
        got: got.clone(),
    };
    let target: IStream = writer.into();
    let (mut read, mut written) = (0u64, 0u64);
    // SAFETY: plain call on live streams; this thread serves the target's calls meanwhile.
    let result = unsafe { stream.CopyTo(&target, u64::MAX, Some(&mut read), Some(&mut written)) };
    drop(target);
    let got = got.lock().unwrap().clone();
    (result, read, written, got, data.report().all_served)
}

#[test]
fn copy_to_writes_everything_through_short_writes() {
    let (result, read, written, got, served) = copy_to_short_writer(1000);
    result.unwrap();
    assert_eq!((read, written), (big().len() as u64, big().len() as u64));
    assert_eq!(got, big());
    assert!(served);
}

#[test]
fn copy_to_a_target_that_takes_nothing_fails_and_serves_nothing() {
    let (result, read, written, got, served) = copy_to_short_writer(0);
    let error = result.expect_err("a target that takes nothing");
    assert_eq!(error.code(), windows::Win32::Foundation::STG_E_MEDIUMFULL);
    assert!(read > 0);
    assert_eq!(written, 0);
    assert!(got.is_empty());
    assert!(!served, "a file the target didn't take is never counted for a move");
}

#[test]
fn the_clipboard_offers_the_virtual_files_to_other_readers() {
    use crate::clipboard_test_support::{ClipboardLock, Snapshot};

    let _lock = ClipboardLock::acquire();
    let saved = match Snapshot::save() {
        Ok(saved) => saved,
        Err(why) => {
            eprintln!("skipped: the clipboard holds {why}, which this test can't restore");
            return;
        }
    };
    let _ole = OleApartment::new().unwrap();
    let (offer, provider) = offer(false, false);
    let data = VirtualDataObject::new(offer, provider).unwrap();
    data.set_clipboard().unwrap();
    let on_clipboard = data.is_on_clipboard();
    let has_virtual = crate::virtual_files::clipboard_has_virtual_files();
    let sequence = crate::clipboard_sequence();
    let reader =
        std::thread::spawn(move || with_virtual_files(VirtualSource::clipboard(sequence), read_all));
    pump_until(|| reader.is_finished());
    let items = reader.join().unwrap();
    data.clear_clipboard_if_current();
    let still_ours = data.is_on_clipboard();
    if let Err(e) = saved.restore_if(|| true) {
        eprintln!("warning: could not restore the clipboard: {e}");
    }
    assert!(on_clipboard && has_virtual);
    let items = items.unwrap();
    assert_eq!(items.len(), 4);
    assert_eq!(items[3].2, Some(Ok(big())));
    assert!(!still_ours);
}

/// Drops `data` on `folder` through the shell's own drop target (what Explorer and the
/// desktop use), then serves its calls until `done`.
fn shell_drop(data: &VirtualDataObject, folder: &Path, shift: bool, done: impl FnMut() -> bool) {
    use windows::Win32::Foundation::POINTL;
    use windows::Win32::System::Com::IBindCtx;
    use windows::Win32::System::Ole::{DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, IDropTarget};
    use windows::Win32::System::SystemServices::{MK_LBUTTON, MK_SHIFT, MODIFIERKEYS_FLAGS};
    use windows::Win32::UI::Shell::{BHID_SFUIObject, IShellItem, SHCreateItemFromParsingName};
    use windows::core::PCWSTR;

    let wide = crate::to_wide(folder);
    // SAFETY: `wide` is NUL-terminated; OLE is initialized on this thread.
    let item: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>) }.unwrap();
    // SAFETY: plain calls on live shell objects.
    let target: IDropTarget =
        unsafe { item.BindToHandler(None::<&IBindCtx>, &BHID_SFUIObject) }.unwrap();
    let keys = MODIFIERKEYS_FLAGS(MK_LBUTTON.0 | if shift { MK_SHIFT.0 } else { 0 });
    let allowed = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
    let mut effect = allowed;
    let point = POINTL::default();
    // SAFETY: plain calls on a live drop target with a live data object.
    unsafe {
        target
            .DragEnter(data.data_object(), keys, point, &mut effect)
            .unwrap();
        effect = allowed;
        target
            .Drop(data.data_object(), keys, point, &mut effect)
            .unwrap();
    }
    data.drag_returned(effect.0);
    pump_until(done);
}

#[test]
fn the_shells_own_drop_target_writes_the_virtual_files() {
    let _ole = OleApartment::new().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let (offer, provider) = offer(false, false);
    let data = VirtualDataObject::new(offer, provider.clone()).unwrap();
    let big_file = dest.path().join(r"docs\sub\big.bin");
    shell_drop(&data, dest.path(), false, || {
        std::fs::read(&big_file).is_ok_and(|b| b == big()) && data.report().all_served
    });
    assert_eq!(
        std::fs::read(dest.path().join(r"docs\a.txt")).unwrap(),
        b"hello"
    );
    eprintln!("shell copy report: {:?}", data.report());
    assert_eq!(provider.moved.load(Ordering::SeqCst), 0, "a copy deletes nothing");
}

#[test]
fn a_shift_drop_on_the_shell_confirms_the_move() {
    let _ole = OleApartment::new().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let (offer, provider) = offer(false, true);
    let data = VirtualDataObject::new(offer, provider.clone()).unwrap();
    let big_file = dest.path().join(r"docs\sub\big.bin");
    shell_drop(&data, dest.path(), true, || {
        std::fs::read(&big_file).is_ok_and(|b| b == big()) && data.report().all_served
    });
    // The target reports after its copy: give it a moment.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    pump_until(|| provider.moved.load(Ordering::SeqCst) > 0 || std::time::Instant::now() > deadline);
    eprintln!("shell move report: {:?}", data.report());
    assert_eq!(provider.moved.load(Ordering::SeqCst), 1, "{:?}", data.report());
}

#[test]
fn a_shell_paste_of_the_clipboard_writes_the_virtual_files() {
    use crate::clipboard_test_support::{ClipboardLock, Snapshot};

    let _lock = ClipboardLock::acquire();
    let saved = match Snapshot::save() {
        Ok(saved) => saved,
        Err(why) => {
            eprintln!("skipped: the clipboard holds {why}, which this test can't restore");
            return;
        }
    };
    let _ole = OleApartment::new().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let (offer, provider) = offer(false, true);
    let data = VirtualDataObject::new(offer, provider.clone()).unwrap();
    data.set_clipboard().unwrap();
    let pasted = data.shell_paste_into(dest.path());
    let big_file = dest.path().join(r"docs\sub\big.bin");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    pump_until(|| {
        (std::fs::read(&big_file).is_ok_and(|b| b == big())
            && provider.moved.load(Ordering::SeqCst) > 0)
            || std::time::Instant::now() > deadline
    });
    let report = data.report();
    data.clear_clipboard_if_current();
    if let Err(e) = saved.restore_if(|| true) {
        eprintln!("warning: could not restore the clipboard: {e}");
    }
    pasted.unwrap();
    eprintln!("shell paste report: {report:?}");
    assert_eq!(std::fs::read(&big_file).unwrap(), big());
    assert_eq!(provider.moved.load(Ordering::SeqCst), 1, "{report:?}");
}
