//! Shared by tests that use the real Windows clipboard: forge-win's clipboard unit tests
//! (as a `cfg(test)` module) and the forge-app e2e suite (included with `#[path]`). It uses
//! only `windows` and std.
//!
//! - [`ClipboardLock`]: a named mutex, so such tests run one at a time across test binaries
//!   and worktrees.
//! - [`Snapshot`]: the user's clipboard, saved before a test writes to it. It fails closed:
//!   when the clipboard holds something it cannot put back exactly (files, which could be
//!   a cut; GDI formats with no DIB), the test must be skipped rather than overwrite it.
//! - [`Snapshot::restore_if`]: puts it back if a check, made inside the same clipboard open,
//!   agrees.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, GlobalFree, HANDLE, HGLOBAL, HWND, WAIT_FAILED};
use windows::Win32::System::DataExchange::{
    CloseClipboard, CountClipboardFormats, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Threading::{
    CreateMutexW, INFINITE, ReleaseMutex, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
};
use windows::core::w;

pub const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;
/// `CF_BITMAP` and `CF_PALETTE`: the system makes them from a DIB again.
const FROM_DIB: [u32; 2] = [2, 9];
/// Formats whose handle is not global memory.
const NOT_GLOBAL: [u32; 8] = [2, 3, 9, 14, 0x80, 0x82, 0x83, 0x8E];

/// Holds the named mutex `Local\ZXrawExplorerClipboardTests`. It is recursive for the
/// thread holding it; `WAIT_ABANDONED` (a test process died holding it) also hands it over.
pub struct ClipboardLock {
    handle: HANDLE,
    held: bool,
}

impl ClipboardLock {
    pub fn acquire() -> Self {
        let handle = unsafe { CreateMutexW(None, false, w!("Local\\ZXrawExplorerClipboardTests")) }
            .expect("create the clipboard test mutex");
        let mut lock = Self {
            handle,
            held: false,
        };
        let wait = unsafe { WaitForSingleObject(lock.handle, INFINITE) };
        assert_ne!(wait, WAIT_FAILED, "wait for the clipboard test mutex");
        lock.held = true;
        lock
    }
}

impl Drop for ClipboardLock {
    fn drop(&mut self) {
        unsafe {
            if self.held {
                let _ = ReleaseMutex(self.handle);
            }
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Closes the clipboard when dropped.
struct Open;

impl Open {
    /// Opens the clipboard for `owner`, retrying while another process holds it.
    fn new(owner: Option<HWND>) -> Option<Self> {
        for _ in 0..50 {
            if unsafe { OpenClipboard(owner) }.is_ok() {
                return Some(Open);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        None
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// Paths in the open clipboard's `CF_HDROP`, if it has one.
fn dropped_paths() -> Option<Vec<PathBuf>> {
    unsafe {
        let handle = GetClipboardData(CF_HDROP).ok()?;
        let drop = HDROP(handle.0);
        let count = DragQueryFileW(drop, u32::MAX, None);
        Some(
            (0..count)
                .map(|i| {
                    let mut buf = vec![0u16; DragQueryFileW(drop, i, None) as usize + 1];
                    let len = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
                    PathBuf::from(String::from_utf16_lossy(&buf[..len]))
                })
                .collect(),
        )
    }
}

fn inside(path: &Path, dir: &Path) -> bool {
    let lower = |p: &Path| p.to_string_lossy().to_lowercase();
    Path::new(&lower(path)).starts_with(lower(dir))
}

/// With the clipboard open: whether it is empty or holds only files inside `dir`.
pub fn holds_only_files_in(dir: &Path) -> bool {
    let empty = unsafe { CountClipboardFormats() } == 0;
    empty
        || dropped_paths()
            .is_some_and(|paths| !paths.is_empty() && paths.iter().all(|p| inside(p, dir)))
}

/// Opens the clipboard and reports `(sequence, holds_only_files_in(dir))`; `None` while it
/// cannot be opened.
pub fn check_files_in(dir: &Path) -> Option<(u32, bool)> {
    let _open = Open::new(None)?;
    Some((sequence(), holds_only_files_in(dir)))
}

pub struct Snapshot {
    formats: Vec<(u32, Vec<u8>)>,
    /// The clipboard sequence the formats belong to (read in the same open).
    pub sequence: u32,
}

impl Snapshot {
    /// Saves every format, or says why the clipboard cannot be put back exactly. A file list
    /// is refused even as a copy (it may be a cut, and replaying a cut would move files
    /// again) unless none of its files exist any more (a leftover from an earlier test run;
    /// such a list is dropped and the clipboard restored without it).
    pub fn save() -> Result<Self, String> {
        let open = Open::new(None).ok_or("the clipboard could not be opened")?;
        let sequence = sequence();
        let mut formats = Vec::new();
        let mut skipped = Vec::new();
        let mut stale_files = false;
        unsafe {
            let mut format = EnumClipboardFormats(0);
            while format != 0 {
                if format == CF_HDROP {
                    let paths = dropped_paths().unwrap_or_default();
                    if paths.iter().any(|p| p.exists()) {
                        return Err("a list of files (a copy or a cut)".into());
                    }
                    stale_files = true;
                }
                let bytes = (!NOT_GLOBAL.contains(&format))
                    .then(|| GetClipboardData(format).ok())
                    .flatten()
                    .and_then(|handle| {
                        let global = HGLOBAL(handle.0);
                        let size = GlobalSize(global);
                        let ptr = GlobalLock(global) as *const u8;
                        if ptr.is_null() {
                            return None;
                        }
                        let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
                        let _ = GlobalUnlock(global);
                        Some(bytes)
                    });
                match bytes {
                    Some(bytes) => formats.push((format, bytes)),
                    None => skipped.push(format),
                }
                format = EnumClipboardFormats(format);
            }
        }
        drop(open);
        let has_dib = formats.iter().any(|(f, _)| *f == CF_DIB || *f == CF_DIBV5);
        let lost: Vec<u32> = skipped
            .into_iter()
            .filter(|f| !(has_dib && FROM_DIB.contains(f)))
            .collect();
        if !lost.is_empty() {
            return Err(format!("clipboard formats {lost:?} that cannot be saved"));
        }
        if stale_files {
            // Everything that came with the stale file list goes with it.
            formats.clear();
        }
        Ok(Self { formats, sequence })
    }

    /// Puts the saved formats back if `owned` (called with the clipboard open, so nothing can
    /// change between the check and the restore) says so. `Ok(false)`: `owned` said no.
    pub fn restore_if(&self, owned: impl FnOnce() -> bool) -> Result<bool, String> {
        // Copies first, so nothing can fail between emptying the clipboard and refilling it.
        let mut globals = Vec::new();
        let free_all = |globals: &mut Vec<(u32, HGLOBAL)>| {
            for (_, global) in globals.drain(..) {
                let _ = unsafe { GlobalFree(Some(global)) };
            }
        };
        for (format, bytes) in &self.formats {
            let copied = unsafe { copy_to_global(bytes) };
            match copied {
                Some(global) => globals.push((*format, global)),
                None => {
                    free_all(&mut globals);
                    return Err(format!("could not copy format {format}"));
                }
            }
        }
        // With no owner, SetClipboardData fails after EmptyClipboard.
        let owner = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("ZXraw test clipboard restore"),
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
        };
        let Ok(owner) = owner else {
            free_all(&mut globals);
            return Err("could not create the clipboard owner window".into());
        };
        let result = (|| {
            let Some(_open) = Open::new(Some(owner)) else {
                free_all(&mut globals);
                return Err("the clipboard could not be opened".to_string());
            };
            if !owned() {
                free_all(&mut globals);
                return Ok(false);
            }
            if let Err(e) = unsafe { EmptyClipboard() } {
                free_all(&mut globals);
                return Err(format!("EmptyClipboard: {e}"));
            }
            let mut failed = Vec::new();
            for (format, global) in globals.drain(..) {
                // The clipboard owns the memory only when it accepts it.
                if unsafe { SetClipboardData(format, Some(HANDLE(global.0))) }.is_err() {
                    let _ = unsafe { GlobalFree(Some(global)) };
                    failed.push(format);
                }
            }
            if failed.is_empty() {
                Ok(true)
            } else {
                Err(format!("formats {failed:?} were not restored"))
            }
        })();
        let _ = unsafe { DestroyWindow(owner) };
        result
    }

    /// The saved Unicode text, if any.
    pub fn text(&self) -> Option<String> {
        let (_, bytes) = self.formats.iter().find(|(f, _)| *f == CF_UNICODETEXT)?;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        Some(String::from_utf16_lossy(&units))
    }

    /// A snapshot holding just `text`, to put on the clipboard with [`Snapshot::restore_if`].
    pub fn of_text(text: &str) -> Self {
        let bytes = text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        Self {
            formats: vec![(CF_UNICODETEXT, bytes)],
            sequence: 0,
        }
    }
}

unsafe fn copy_to_global(bytes: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).ok()?;
        let ptr = GlobalLock(global) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(Some(global));
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(global);
        Some(global)
    }
}
