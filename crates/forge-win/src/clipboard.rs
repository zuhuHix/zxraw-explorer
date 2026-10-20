//! Windows file clipboard support (`CF_HDROP`, preferred drop effect, and text paths).

use std::path::PathBuf;

use crate::Result;

/// The operation the shell should use when the files are pasted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardEffect {
    Copy,
    Move,
}

/// Places paths on the Windows clipboard in formats understood by Explorer and text editors.
#[cfg(windows)]
pub fn set_files(paths: &[PathBuf], effect: ClipboardEffect) -> Result<()> {
    set_files_impl(paths, effect, true)
}

/// Places paths on the clipboard as Explorer's Copy and Cut do: `CF_HDROP` and the preferred
/// drop effect, with no text. For tests that stand in for another app.
#[cfg(windows)]
pub fn set_files_without_text(paths: &[PathBuf], effect: ClipboardEffect) -> Result<()> {
    set_files_impl(paths, effect, false)
}

#[cfg(not(windows))]
pub fn set_files_without_text(_paths: &[PathBuf], _effect: ClipboardEffect) -> Result<()> {
    Err(crate::WinError::Unsupported)
}

#[cfg(windows)]
fn set_files_impl(paths: &[PathBuf], effect: ClipboardEffect, with_text: bool) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::System::DataExchange::{EmptyClipboard, RegisterClipboardFormatW};
    use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
    use windows::core::w;

    if paths.is_empty() {
        return Err(crate::WinError::Invalid(
            "at least one file path is required".to_string(),
        ));
    }

    let wide_paths = paths
        .iter()
        .map(|path| {
            let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            if wide.is_empty() || wide.contains(&0) {
                return Err(crate::WinError::Invalid(format!(
                    "path is empty or contains a NUL: {}",
                    path.display()
                )));
            }
            Ok(wide)
        })
        .collect::<Result<Vec<_>>>()?;

    let dropfiles = encode_dropfiles(&wide_paths)?;
    let preferred_format = unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) };
    if preferred_format == 0 {
        return Err(crate::api_err(
            "RegisterClipboardFormatW(Preferred DropEffect)",
            windows::core::Error::from_thread(),
        ));
    }
    let drop_effect: u32 = match effect {
        ClipboardEffect::Copy => 1,
        ClipboardEffect::Move => 2,
    };
    let effect_bytes = drop_effect.to_ne_bytes().to_vec();
    let mut text_wide = Vec::new();
    for (index, path) in wide_paths.iter().enumerate() {
        if index != 0 {
            text_wide.extend_from_slice(&[b'\r' as u16, b'\n' as u16]);
        }
        text_wide.extend_from_slice(path);
    }
    text_wide.push(0);
    let text_bytes = u16s_as_bytes(&text_wide);

    // Allocate everything before replacing the current clipboard. Each handle is freed
    // locally unless SetClipboardData succeeds and transfers ownership to Windows.
    let mut drop_handle = GlobalMem::new(&dropfiles)?;
    let mut effect_handle = GlobalMem::new(&effect_bytes)?;
    let mut text_handle = GlobalMem::new(text_bytes)?;

    let _window = ClipboardOwnerWindow::new()?;
    let _clipboard = open_clipboard_retry(Some(_window.hwnd))?;
    unsafe { EmptyClipboard() }.map_err(|e| crate::api_err("EmptyClipboard", e))?;

    set_clipboard_data(
        CF_HDROP.0 as u32,
        &mut drop_handle,
        "SetClipboardData(CF_HDROP)",
    )?;
    set_clipboard_data(
        preferred_format,
        &mut effect_handle,
        "SetClipboardData(Preferred DropEffect)",
    )?;
    if with_text {
        set_clipboard_data(
            CF_UNICODETEXT.0 as u32,
            &mut text_handle,
            "SetClipboardData(CF_UNICODETEXT)",
        )?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn set_files(_paths: &[PathBuf], _effect: ClipboardEffect) -> Result<()> {
    Err(crate::WinError::Unsupported)
}

/// Retrieves the clipboard file list and preferred effect, if `CF_HDROP` is available.
#[cfg(windows)]
pub fn get_files() -> Result<Option<(Vec<PathBuf>, ClipboardEffect)>> {
    use windows::Win32::System::DataExchange::{
        GetClipboardData, IsClipboardFormatAvailable, RegisterClipboardFormatW,
    };
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::core::w;

    let _window = ClipboardOwnerWindow::new()?;
    let _clipboard = open_clipboard_retry(Some(_window.hwnd))?;
    if unsafe { IsClipboardFormatAvailable(CF_HDROP.0 as u32) }.is_err() {
        return Ok(None);
    }

    let hdrop = unsafe { GetClipboardData(CF_HDROP.0 as u32) }
        .map_err(|e| crate::api_err("GetClipboardData(CF_HDROP)", e))?;
    let paths = read_hdrop(hdrop)?;

    let preferred_format = unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) };
    let effect = if preferred_format != 0
        && unsafe { IsClipboardFormatAvailable(preferred_format) }.is_ok()
    {
        let handle = unsafe { GetClipboardData(preferred_format) }
            .map_err(|e| crate::api_err("GetClipboardData(Preferred DropEffect)", e))?;
        let value = read_global_dword(handle, "GlobalLock(Preferred DropEffect)")?;
        if value & 2 != 0 {
            ClipboardEffect::Move
        } else {
            ClipboardEffect::Copy
        }
    } else {
        ClipboardEffect::Copy
    };

    Ok(Some((paths, effect)))
}

#[cfg(not(windows))]
pub fn get_files() -> Result<Option<(Vec<PathBuf>, ClipboardEffect)>> {
    Err(crate::WinError::Unsupported)
}

/// Returns the system clipboard sequence number, or zero on non-Windows targets.
#[cfg(windows)]
pub fn clipboard_sequence() -> u32 {
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

#[cfg(not(windows))]
pub fn clipboard_sequence() -> u32 {
    0
}

/// Whether the clipboard holds text (`CF_UNICODETEXT`). Does not open the clipboard.
#[cfg(windows)]
pub fn clipboard_has_text() -> bool {
    use windows::Win32::System::DataExchange::IsClipboardFormatAvailable;
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32) }.is_ok()
}

#[cfg(not(windows))]
pub fn clipboard_has_text() -> bool {
    false
}

/// Whether `msg` (a `*const MSG` from a message loop hook) is a fresh key press of the paste
/// shortcut: exactly Ctrl+V or Shift+Insert. Auto-repeats of a held key do not count.
#[cfg(windows)]
pub fn is_paste_keydown(msg: *const std::ffi::c_void) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
    use windows::Win32::UI::WindowsAndMessaging::{MSG, WM_KEYDOWN};

    if msg.is_null() {
        return false;
    }
    // SAFETY: winit passes a valid `MSG` for the duration of the hook call.
    let msg = unsafe { &*msg.cast::<MSG>() };
    if msg.message != WM_KEYDOWN {
        return false;
    }
    // The key state as of this message, not the live keyboard.
    let down = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| unsafe {
        GetKeyState(i32::from(vk.0)) < 0
    };
    // Bit 30 of lParam is the previous key state: set for an auto-repeat.
    let repeat = msg.lParam.0 & (1 << 30) != 0;
    paste_chord(
        msg.wParam.0 as u16,
        down(VK_CONTROL),
        down(VK_SHIFT),
        down(VK_MENU),
        repeat,
    )
}

#[cfg(not(windows))]
pub fn is_paste_keydown(_msg: *const std::ffi::c_void) -> bool {
    false
}

/// Exactly Ctrl+V or Shift+Insert for virtual key `vk` and the modifier state, and not an
/// auto-repeat. Extra modifiers (Ctrl+Shift+V, AltGr+V) are other shortcuts or text input.
fn paste_chord(vk: u16, ctrl: bool, shift: bool, alt: bool, repeat: bool) -> bool {
    const VK_V: u16 = b'V' as u16;
    const VK_INSERT: u16 = 0x2D;
    !repeat && !alt && ((vk == VK_V && ctrl && !shift) || (vk == VK_INSERT && shift && !ctrl))
}

/// Empties the clipboard if its sequence (when provided) and ordered path list match.
///
/// Clipboard errors are ignored because this helper is intended for best-effort cut cleanup.
#[cfg(windows)]
pub fn clear_if_owned_files(paths: &[PathBuf], expected_sequence: Option<u32>) -> Option<u32> {
    use windows::Win32::System::DataExchange::{
        EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
    };
    use windows::Win32::System::Ole::CF_HDROP;

    if paths.is_empty() {
        return None;
    }
    let Ok(window) = ClipboardOwnerWindow::new() else {
        return None;
    };
    let Ok(_clipboard) = open_clipboard_retry(Some(window.hwnd)) else {
        return None;
    };
    if expected_sequence.is_some_and(|expected| clipboard_sequence() != expected) {
        return None;
    }
    if unsafe { IsClipboardFormatAvailable(CF_HDROP.0 as u32) }.is_err() {
        return None;
    }
    let Ok(handle) = (unsafe { GetClipboardData(CF_HDROP.0 as u32) }) else {
        return None;
    };
    let Ok(current) = read_hdrop(handle) else {
        return None;
    };
    if current == paths {
        unsafe { EmptyClipboard() }.ok()?;
        Some(clipboard_sequence())
    } else {
        None
    }
}

#[cfg(not(windows))]
pub fn clear_if_owned_files(_paths: &[PathBuf], _expected_sequence: Option<u32>) -> Option<u32> {
    None
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct DropFiles {
    p_files: u32,
    x: i32,
    y: i32,
    f_nc: i32,
    f_wide: i32,
}

#[cfg(windows)]
fn encode_dropfiles(paths: &[Vec<u16>]) -> Result<Vec<u8>> {
    let header_size = std::mem::size_of::<DropFiles>();
    let offset = u32::try_from(header_size)
        .map_err(|_| crate::WinError::Invalid("DROPFILES offset is too large".to_string()))?;
    let mut units = Vec::new();
    for path in paths {
        units.len().checked_add(path.len() + 1).ok_or_else(|| {
            crate::WinError::Invalid("clipboard file list is too large".to_string())
        })?;
        units.extend_from_slice(path);
        units.push(0);
    }
    units.push(0);
    let payload_bytes = units
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| crate::WinError::Invalid("clipboard file list is too large".to_string()))?;
    let total_bytes = header_size
        .checked_add(payload_bytes)
        .ok_or_else(|| crate::WinError::Invalid("clipboard file list is too large".to_string()))?;

    let mut bytes = vec![0_u8; total_bytes];
    let header = DropFiles {
        p_files: offset,
        x: 0,
        y: 0,
        f_nc: 0,
        f_wide: 1,
    };
    // Read/write unaligned keeps this safe even though the byte buffer only promises u8 alignment.
    unsafe {
        std::ptr::write_unaligned(bytes.as_mut_ptr().cast::<DropFiles>(), header);
    }
    let payload = u16s_as_bytes(&units);
    bytes[header_size..].copy_from_slice(payload);
    Ok(bytes)
}

#[cfg(windows)]
fn u16s_as_bytes(units: &[u16]) -> &[u8] {
    // u16 has no padding, and the returned view cannot outlive `units`.
    unsafe { std::slice::from_raw_parts(units.as_ptr().cast::<u8>(), std::mem::size_of_val(units)) }
}

#[cfg(windows)]
struct GlobalMem {
    handle: windows::Win32::Foundation::HGLOBAL,
    owned: bool,
}

#[cfg(windows)]
impl GlobalMem {
    fn new(bytes: &[u8]) -> Result<Self> {
        use windows::Win32::System::Memory::{
            GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
        };

        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }
            .map_err(|e| crate::api_err("GlobalAlloc", e))?;
        let ptr = unsafe { GlobalLock(handle) };
        if ptr.is_null() {
            unsafe {
                let _ = windows::Win32::Foundation::GlobalFree(Some(handle));
            }
            return Err(crate::api_err(
                "GlobalLock",
                windows::core::Error::from_thread(),
            ));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
            // A zero result is the normal result when the lock count reaches zero.
            let _ = GlobalUnlock(handle);
        }
        Ok(Self {
            handle,
            owned: true,
        })
    }

    fn transfer(&mut self) {
        self.owned = false;
    }
}

#[cfg(windows)]
impl Drop for GlobalMem {
    fn drop(&mut self) {
        if self.owned {
            unsafe {
                let _ = windows::Win32::Foundation::GlobalFree(Some(self.handle));
            }
        }
    }
}

#[cfg(windows)]
fn set_clipboard_data(format: u32, data: &mut GlobalMem, op: &'static str) -> Result<()> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::SetClipboardData;

    let handle = HANDLE(data.handle.0);
    unsafe { SetClipboardData(format, Some(handle)) }.map_err(|e| crate::api_err(op, e))?;
    data.transfer();
    Ok(())
}

#[cfg(windows)]
struct ClipboardOwnerWindow {
    hwnd: windows::Win32::Foundation::HWND,
}

#[cfg(windows)]
impl ClipboardOwnerWindow {
    fn new() -> Result<Self> {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, WINDOW_EX_STYLE, WINDOW_STYLE,
        };
        use windows::core::w;

        // NULL cannot be used as the clipboard owner: EmptyClipboard then makes
        // SetClipboardData fail. A hidden STATIC window on this thread is sufficient.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("ZXraw Explorer clipboard owner"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
        }
        .map_err(|e| crate::api_err("CreateWindowExW(clipboard owner)", e))?;
        Ok(Self { hwnd })
    }
}

#[cfg(windows)]
impl Drop for ClipboardOwnerWindow {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd) };
    }
}

#[cfg(windows)]
struct ClipboardGuard;

#[cfg(windows)]
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::System::DataExchange::CloseClipboard() };
    }
}

#[cfg(windows)]
fn open_clipboard_retry(owner: Option<windows::Win32::Foundation::HWND>) -> Result<ClipboardGuard> {
    use std::{thread, time::Duration};

    use windows::Win32::System::DataExchange::OpenClipboard;

    let mut last_error = None;
    for attempt in 0..10 {
        match unsafe { OpenClipboard(owner) } {
            Ok(()) => return Ok(ClipboardGuard),
            Err(error) => last_error = Some(error),
        }
        if attempt < 9 {
            thread::sleep(Duration::from_millis(10));
        }
    }
    Err(crate::api_err(
        "OpenClipboard",
        last_error.expect("at least one clipboard open attempt"),
    ))
}

#[cfg(windows)]
fn read_hdrop(handle: windows::Win32::Foundation::HANDLE) -> Result<Vec<PathBuf>> {
    use std::{mem::size_of, os::windows::ffi::OsStringExt};

    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    let hglobal = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(hglobal) };
    let header_size = size_of::<DropFiles>();
    if size < header_size {
        return Err(crate::WinError::Invalid(
            "CF_HDROP is shorter than a DROPFILES header".to_string(),
        ));
    }
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        return Err(crate::api_err(
            "GlobalLock(CF_HDROP)",
            windows::core::Error::from_thread(),
        ));
    }
    let result = (|| {
        let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) };
        let header = unsafe { std::ptr::read_unaligned(ptr.cast::<DropFiles>()) };
        let offset = header.p_files as usize;
        if offset < header_size
            || offset > size
            || !offset.is_multiple_of(std::mem::align_of::<u16>())
        {
            return Err(crate::WinError::Invalid(
                "CF_HDROP has an invalid DROPFILES file-list offset".to_string(),
            ));
        }
        if header.f_wide != 0 {
            if (size - offset) < 2 * size_of::<u16>() {
                return Err(crate::WinError::Invalid(
                    "CF_HDROP wide file list is missing its terminator".to_string(),
                ));
            }
            let units = unsafe {
                std::slice::from_raw_parts(
                    ptr.cast::<u16>().add(offset / size_of::<u16>()),
                    (size - offset) / size_of::<u16>(),
                )
            };
            if !has_double_nul_u16(units) {
                return Err(crate::WinError::Invalid(
                    "CF_HDROP wide file list is not double-NUL terminated".to_string(),
                ));
            }
        } else {
            if (size - offset) < 2 {
                return Err(crate::WinError::Invalid(
                    "CF_HDROP ANSI file list is missing its terminator".to_string(),
                ));
            }
            if !has_double_nul_bytes(&bytes[offset..]) {
                return Err(crate::WinError::Invalid(
                    "CF_HDROP ANSI file list is not double-NUL terminated".to_string(),
                ));
            }
        }

        let hdrop = HDROP(handle.0);
        let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
        if count == 0 {
            return Err(crate::WinError::Invalid(
                "CF_HDROP contains no file paths".to_string(),
            ));
        }
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let length = unsafe { DragQueryFileW(hdrop, index, None) } as usize;
            let max_length = (size - offset) / size_of::<u16>();
            if length == 0 || length >= max_length {
                return Err(crate::WinError::Invalid(
                    "CF_HDROP contains an invalid path length".to_string(),
                ));
            }
            let mut name = vec![0_u16; length + 1];
            let copied = unsafe { DragQueryFileW(hdrop, index, Some(&mut name)) } as usize;
            if copied != length || name[length] != 0 {
                return Err(crate::WinError::Invalid(
                    "DragQueryFileW returned an inconsistent path".to_string(),
                ));
            }
            paths.push(PathBuf::from(std::ffi::OsString::from_wide(
                &name[..length],
            )));
        }
        Ok(paths)
    })();
    unsafe {
        // A zero return is normal when the final lock is released.
        let _ = GlobalUnlock(hglobal);
    }
    result
}

#[cfg(windows)]
fn has_double_nul_u16(units: &[u16]) -> bool {
    units.windows(2).any(|pair| pair == [0, 0])
}

#[cfg(windows)]
fn has_double_nul_bytes(bytes: &[u8]) -> bool {
    bytes.windows(2).any(|pair| pair == [0, 0])
}

#[cfg(windows)]
fn read_global_dword(handle: windows::Win32::Foundation::HANDLE, op: &'static str) -> Result<u32> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    let hglobal = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(hglobal) };
    if size < std::mem::size_of::<u32>() {
        return Err(crate::WinError::Invalid(
            "clipboard DWORD is shorter than four bytes".to_string(),
        ));
    }
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        return Err(crate::api_err(op, windows::core::Error::from_thread()));
    }
    let value = unsafe { std::ptr::read_unaligned(ptr.cast::<u32>()) };
    unsafe {
        // A zero return is normal when the final lock is released.
        let _ = GlobalUnlock(hglobal);
    }
    Ok(value)
}

#[cfg(all(windows, test))]
fn u16_payload_to_string(handle: windows::Win32::Foundation::HANDLE) -> Result<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    let hglobal = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(hglobal) };
    if size < size_of::<u16>() {
        return Err(crate::WinError::Invalid(
            "CF_UNICODETEXT is too short".to_string(),
        ));
    }
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        return Err(crate::api_err(
            "GlobalLock(CF_UNICODETEXT)",
            windows::core::Error::from_thread(),
        ));
    }
    let units = unsafe { std::slice::from_raw_parts(ptr.cast::<u16>(), size / size_of::<u16>()) };
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    let text = String::from_utf16_lossy(&units[..end]);
    unsafe {
        let _ = GlobalUnlock(hglobal);
    }
    Ok(text)
}

#[cfg(all(test, windows))]
mod tests {
    use std::{path::PathBuf, process::Command, sync::Mutex};

    use windows::Win32::System::DataExchange::{
        EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, SetClipboardData,
    };
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    use super::*;

    static CLIPBOARD_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct ClipboardTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved_text: Option<String>,
    }

    impl ClipboardTestGuard {
        fn new() -> Self {
            let lock = CLIPBOARD_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let saved_text = read_test_text().ok().flatten();
            Self {
                _lock: lock,
                saved_text,
            }
        }
    }

    impl Drop for ClipboardTestGuard {
        fn drop(&mut self) {
            if let Some(text) = &self.saved_text {
                let _ = set_test_text(text);
            }
        }
    }

    #[test]
    fn files_round_trip_for_copy_and_move_including_long_unicode_paths() {
        let _guard = ClipboardTestGuard::new();
        let long_component = format!("{}-長い名前.txt", "x".repeat(280));
        let paths = vec![
            PathBuf::from(r"C:\ZXraw Explorer\日本語\résumé.txt"),
            PathBuf::from(format!(r"\?C:ZXraw clipboard{long_component}")),
        ];
        assert!(paths[1].as_os_str().len() > 260);

        for effect in [ClipboardEffect::Copy, ClipboardEffect::Move] {
            set_files(&paths, effect).unwrap();
            assert_eq!(get_files().unwrap(), Some((paths.clone(), effect)));
            let expected_text = paths
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>()
                .join("\r\n");
            assert_eq!(
                read_test_text().unwrap().as_deref(),
                Some(expected_text.as_str())
            );
        }
    }

    #[test]
    fn plain_text_has_no_file_list() {
        let _guard = ClipboardTestGuard::new();
        set_test_text("plain clipboard text").unwrap();
        assert_eq!(get_files().unwrap(), None);
    }

    #[test]
    fn setting_files_increments_clipboard_sequence() {
        let _guard = ClipboardTestGuard::new();
        let before = clipboard_sequence();
        set_files(&[PathBuf::from(r"C:\sequence.txt")], ClipboardEffect::Copy).unwrap();
        assert_ne!(clipboard_sequence(), before);
    }

    #[test]
    fn clear_if_owned_files_does_not_clear_a_changed_sequence() {
        let _guard = ClipboardTestGuard::new();
        let paths = vec![PathBuf::from(r"C:\owned.txt")];
        set_files(&paths, ClipboardEffect::Move).unwrap();
        let owned_sequence = clipboard_sequence();
        set_test_text("external replacement").unwrap();

        assert_eq!(clear_if_owned_files(&paths, Some(owned_sequence)), None);
        assert_eq!(
            read_test_text().unwrap().as_deref(),
            Some("external replacement")
        );
    }

    #[test]
    fn powershell_reads_file_drop_list() {
        let _guard = ClipboardTestGuard::new();
        let paths = vec![PathBuf::from(r"C:\ZXraw Explorer\PowerShell interop.txt")];
        set_files(&paths, ClipboardEffect::Copy).unwrap();

        let output = match Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-STA",
                "-Command",
                "Get-Clipboard -Format FileDropList | ForEach-Object { $_.FullName }",
            ])
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("powershell.exe unavailable; manual interop check skipped");
                return;
            }
            Err(error) => panic!("could not run PowerShell clipboard check: {error}"),
        };
        assert!(
            output.status.success(),
            "PowerShell failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), paths[0].to_string_lossy());
    }

    /// Explorer's own paste (the shell "paste" verb, as Ctrl+V in a folder window or on the
    /// desktop runs it) moves a folder that `set_files` put on the clipboard as a cut.
    #[test]
    fn shell_paste_moves_a_cut() {
        let _guard = ClipboardTestGuard::new();
        let root = tempfile::tempdir().unwrap();
        let (source, target) = (root.path().join("source"), root.path().join("target"));
        let folder = source.join("Shoot A");
        std::fs::create_dir_all(folder.join("nested")).unwrap();
        std::fs::write(folder.join("nested").join("a.txt"), b"folder a").unwrap();
        std::fs::create_dir(&target).unwrap();
        set_files(std::slice::from_ref(&folder), ClipboardEffect::Move).unwrap();

        // The verb returns before the shell's background move ends; keep its process alive.
        let script = format!(
            "(New-Object -ComObject Shell.Application).NameSpace('{}').Self.InvokeVerb('paste'); \
             $t = Join-Path '{}' 'Shoot A\\nested\\a.txt'; \
             for ($i = 0; $i -lt 100 -and -not (Test-Path $t); $i++) {{ Start-Sleep -Milliseconds 100 }}; \
             Start-Sleep -Milliseconds 500",
            target.display(),
            target.display()
        );
        let output = match Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-STA",
                "-Command",
                &script,
            ])
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("powershell.exe unavailable; shell paste check skipped");
                return;
            }
            Err(error) => panic!("could not run the shell paste: {error}"),
        };
        assert!(
            output.status.success(),
            "PowerShell failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(target.join("Shoot A").join("nested").join("a.txt")).unwrap(),
            b"folder a"
        );
        assert!(
            !folder.exists(),
            "the shell copied the cut instead of moving it"
        );
    }

    fn read_test_text() -> Result<Option<String>> {
        let _window = ClipboardOwnerWindow::new()?;
        let _clipboard = open_clipboard_retry(Some(_window.hwnd))?;
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32) }.is_err() {
            return Ok(None);
        }
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }
            .map_err(|e| crate::api_err("GetClipboardData(CF_UNICODETEXT)", e))?;
        u16_payload_to_string(handle).map(Some)
    }

    fn set_test_text(text: &str) -> Result<()> {
        let mut units: Vec<u16> = text.encode_utf16().collect();
        units.push(0);
        let mut handle = GlobalMem::new(u16s_as_bytes(&units))?;
        let _window = ClipboardOwnerWindow::new()?;
        let _clipboard = open_clipboard_retry(Some(_window.hwnd))?;
        unsafe { EmptyClipboard() }.map_err(|e| crate::api_err("EmptyClipboard(test)", e))?;
        let raw = windows::Win32::Foundation::HANDLE(handle.handle.0);
        unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, Some(raw)) }
            .map_err(|e| crate::api_err("SetClipboardData(test text)", e))?;
        handle.transfer();
        Ok(())
    }
}

#[cfg(test)]
mod paste_key_tests {
    use super::paste_chord;

    #[test]
    fn paste_chord_matches_ctrl_v_and_shift_insert_only() {
        let v = b'V' as u16;
        assert!(paste_chord(v, true, false, false, false));
        assert!(paste_chord(0x2D, false, true, false, false));
        assert!(!paste_chord(v, true, true, false, false), "Ctrl+Shift+V");
        assert!(!paste_chord(v, false, false, false, false));
        assert!(!paste_chord(v, true, false, true, false), "AltGr+V");
        assert!(!paste_chord(b'C' as u16, true, false, false, false));
        assert!(!paste_chord(0x2D, true, true, false, false));
        assert!(!paste_chord(0x2D, false, true, true, false));
        assert!(!paste_chord(0x2D, false, false, false, false));
    }

    #[test]
    fn auto_repeat_of_a_held_paste_key_does_not_count() {
        assert!(!paste_chord(b'V' as u16, true, false, false, true));
        assert!(!paste_chord(0x2D, false, true, false, true));
    }
}
