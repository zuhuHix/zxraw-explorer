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
    set_dword(&data, windows::core::w!("Paste Succeeded"), EFFECT_COPY);
    assert_eq!(provider.moved.load(Ordering::SeqCst), 0, "a copy");
    set_dword(&data, windows::core::w!("Paste Succeeded"), EFFECT_MOVE);
    set_dword(&data, windows::core::w!("Paste Succeeded"), EFFECT_MOVE);
    assert_eq!(provider.moved.load(Ordering::SeqCst), 1);
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
