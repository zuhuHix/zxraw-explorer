//! Windows file clipboard support (`CF_HDROP`, preferred drop effect, and text paths).

use std::path::PathBuf;

use crate::Result;

/// The operation the shell should use when the files are pasted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardEffect {
    Copy,
    Move,
}

/// What one look at the clipboard found, all read while it was held open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardRead {
    /// The file list and its effect, `None` if there are no files.
    pub files: Option<(Vec<PathBuf>, ClipboardEffect)>,
    /// The owner token, if the content is a publish of ours.
    pub owner: Option<ClipboardToken>,
    /// The clipboard sequence number of this content.
    pub sequence: u32,
}

/// Marks one publish of ours: stored in a private clipboard format next to the file list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardToken(u64);

impl ClipboardToken {
    /// A token with this value, for tests that never touch the clipboard.
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    /// Whether this process made the publish (any of its publishes, not only the latest).
    pub fn is_from_this_process(self) -> bool {
        self.0 >> 32 == u64::from(std::process::id())
    }
}

/// Counter for [`ClipboardToken`]s. Starts at 1 so a token is never zero in its low half.
#[cfg(windows)]
static NEXT_TOKEN_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// A token unique per publish, and across processes: process id in the high half.
#[cfg(windows)]
fn next_token() -> ClipboardToken {
    use std::sync::atomic::Ordering;

    let counter = NEXT_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    ClipboardToken((u64::from(std::process::id()) << 32) | u64::from(counter))
}

/// Places paths on the Windows clipboard in formats understood by Explorer and text editors,
/// marked with a fresh owner token so that [`get_files_and_owner`] reports it as ours.
#[cfg(windows)]
pub fn set_files(paths: &[PathBuf], effect: ClipboardEffect) -> Result<ClipboardToken> {
    let token = next_token();
    set_files_impl(paths, effect, Some(token))?;
    Ok(token)
}

/// Like [`set_files`], unless another app has put something on the clipboard since sequence
/// `baseline` (our own content, from this process, does not count): then nothing changes and
/// the result is `Ok(None)`. Checked and written in one clipboard session, so nothing can
/// land in between.
#[cfg(windows)]
pub fn set_files_if_unchanged(
    paths: &[PathBuf],
    effect: ClipboardEffect,
    baseline: u32,
) -> Result<Option<ClipboardToken>> {
    use windows::Win32::System::DataExchange::EmptyClipboard;

    let token = next_token();
    let mut payloads = build_payloads(paths, effect, Some(token))?;
    let window = ClipboardOwnerWindow::new()?;
    let _clipboard = open_clipboard_retry(Some(window.hwnd))?;
    if clipboard_sequence() != baseline
        && !read_owner_token_open()?.is_some_and(ClipboardToken::is_from_this_process)
    {
        return Ok(None);
    }
    unsafe { EmptyClipboard() }.map_err(|e| crate::api_err("EmptyClipboard", e))?;
    write_payloads(&mut payloads)?;
    Ok(Some(token))
}

#[cfg(not(windows))]
pub fn set_files_if_unchanged(
    _paths: &[PathBuf],
    _effect: ClipboardEffect,
    _baseline: u32,
) -> Result<Option<ClipboardToken>> {
    Err(crate::WinError::Unsupported)
}

/// Places paths on the clipboard as Explorer's Copy and Cut do: `CF_HDROP` and the preferred
/// drop effect, with no text. For tests that stand in for another app.
#[cfg(windows)]
pub fn set_files_without_text(paths: &[PathBuf], effect: ClipboardEffect) -> Result<()> {
    set_files_impl(paths, effect, None)
}

#[cfg(not(windows))]
pub fn set_files_without_text(_paths: &[PathBuf], _effect: ClipboardEffect) -> Result<()> {
    Err(crate::WinError::Unsupported)
}

/// Writes a publish. `ours` is `Some` for our own publishes (text and owner marker included);
/// `None` is what Explorer's Copy and Cut put on the clipboard.
#[cfg(windows)]
fn set_files_impl(
    paths: &[PathBuf],
    effect: ClipboardEffect,
    ours: Option<ClipboardToken>,
) -> Result<()> {
    use windows::Win32::System::DataExchange::EmptyClipboard;

    // Allocate everything before replacing the current clipboard.
    let mut payloads = build_payloads(paths, effect, ours)?;
    let _window = ClipboardOwnerWindow::new()?;
    let _clipboard = open_clipboard_retry(Some(_window.hwnd))?;
    unsafe { EmptyClipboard() }.map_err(|e| crate::api_err("EmptyClipboard", e))?;
    write_payloads(&mut payloads)
}

/// One clipboard format of a publish, with its data already allocated.
#[cfg(windows)]
struct ClipboardPayload {
    format: u32,
    op: &'static str,
    data: GlobalMem,
}

#[cfg(windows)]
impl ClipboardPayload {
    fn new(format: u32, op: &'static str, bytes: &[u8]) -> Result<Self> {
        Ok(Self {
            format,
            op,
            data: GlobalMem::new(bytes)?,
        })
    }
}

/// Builds every format of a publish, validating `paths` first. Does not touch the clipboard.
#[cfg(windows)]
fn build_payloads(
    paths: &[PathBuf],
    effect: ClipboardEffect,
    ours: Option<ClipboardToken>,
) -> Result<Vec<ClipboardPayload>> {
    use std::os::windows::ffi::OsStrExt;

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
    let preferred_format = register_format(
        w!("Preferred DropEffect"),
        "RegisterClipboardFormatW(Preferred DropEffect)",
    )?;
    let drop_effect: u32 = match effect {
        ClipboardEffect::Copy => 1,
        ClipboardEffect::Move => 2,
    };
    let effect_bytes = drop_effect.to_ne_bytes();

    // Our owner token goes first: a file list on the clipboard is never ours unmarked, even
    // for a moment, or after a write that fails part way (see `write_payloads`).
    let mut payloads = Vec::new();
    if let Some(token) = ours {
        payloads.push(ClipboardPayload::new(
            owner_format()?,
            "SetClipboardData(ZXraw Explorer Clipboard Owner)",
            &token.0.to_ne_bytes(),
        )?);
    }
    payloads.push(ClipboardPayload::new(
        CF_HDROP.0 as u32,
        "SetClipboardData(CF_HDROP)",
        &dropfiles,
    )?);
    payloads.push(ClipboardPayload::new(
        preferred_format,
        "SetClipboardData(Preferred DropEffect)",
        &effect_bytes,
    )?);
    if ours.is_some() {
        let mut text_wide = Vec::new();
        for (index, path) in wide_paths.iter().enumerate() {
            if index != 0 {
                text_wide.extend_from_slice(&[b'\r' as u16, b'\n' as u16]);
            }
            text_wide.extend_from_slice(path);
        }
        text_wide.push(0);
        payloads.push(ClipboardPayload::new(
            CF_UNICODETEXT.0 as u32,
            "SetClipboardData(CF_UNICODETEXT)",
            u16s_as_bytes(&text_wide),
        )?);
    }
    Ok(payloads)
}

/// Puts the payloads on an open, emptied clipboard. Handles transfer to Windows as they go.
/// If one fails, the clipboard is emptied again before it is closed: half a publish (a file
/// list without the rest) is never left behind.
#[cfg(windows)]
fn write_payloads(payloads: &mut [ClipboardPayload]) -> Result<()> {
    use windows::Win32::System::DataExchange::EmptyClipboard;

    write_all_or_empty(
        payloads,
        |payload| set_clipboard_data(payload.format, &mut payload.data, payload.op),
        || {
            let _ = unsafe { EmptyClipboard() };
        },
    )
}

/// Runs `write` on each item in order; on the first failure runs `empty` and returns it.
fn write_all_or_empty<T>(
    items: &mut [T],
    mut write: impl FnMut(&mut T) -> Result<()>,
    empty: impl FnOnce(),
) -> Result<()> {
    for item in items {
        if let Err(e) = write(item) {
            empty();
            return Err(e);
        }
    }
    Ok(())
}

/// Registers a named clipboard format, or returns the existing id.
#[cfg(windows)]
fn register_format(name: windows::core::PCWSTR, op: &'static str) -> Result<u32> {
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;

    let format = unsafe { RegisterClipboardFormatW(name) };
    if format == 0 {
        return Err(crate::api_err(op, windows::core::Error::from_thread()));
    }
    Ok(format)
}

/// The private format that holds a [`ClipboardToken`] next to our file list.
#[cfg(windows)]
fn owner_format() -> Result<u32> {
    use windows::core::w;

    register_format(
        w!("ZXraw Explorer Clipboard Owner"),
        "RegisterClipboardFormatW(ZXraw Explorer Clipboard Owner)",
    )
}

#[cfg(not(windows))]
pub fn set_files(_paths: &[PathBuf], _effect: ClipboardEffect) -> Result<ClipboardToken> {
    Err(crate::WinError::Unsupported)
}

/// Retrieves the clipboard file list and preferred effect, if `CF_HDROP` is available.
#[cfg(windows)]
pub fn get_files() -> Result<Option<(Vec<PathBuf>, ClipboardEffect)>> {
    get_files_and_owner().map(|read| read.files)
}

/// Like [`get_files`], and also returns the owner token of our own publish, if one is present.
/// Both are read in one clipboard session.
#[cfg(windows)]
pub fn get_files_and_owner() -> Result<ClipboardRead> {
    let _window = ClipboardOwnerWindow::new()?;
    let _clipboard = open_clipboard_retry(Some(_window.hwnd))?;
    // Nobody else can change the clipboard while it is open, so these three agree.
    Ok(ClipboardRead {
        files: read_files_open()?,
        owner: read_owner_token_open()?,
        sequence: clipboard_sequence(),
    })
}

/// Reads the file list and preferred effect from an open clipboard.
#[cfg(windows)]
fn read_files_open() -> Result<Option<(Vec<PathBuf>, ClipboardEffect)>> {
    use windows::Win32::System::DataExchange::{
        GetClipboardData, IsClipboardFormatAvailable, RegisterClipboardFormatW,
    };
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::core::w;

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

#[cfg(not(windows))]
pub fn get_files_and_owner() -> Result<ClipboardRead> {
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

/// Reads the owner token from an open clipboard: `Ok(None)` when there is none (or a value
/// too short to be one). `Err` is a failure to read it, which says nothing about the owner.
#[cfg(windows)]
fn read_owner_token_open() -> Result<Option<ClipboardToken>> {
    use windows::Win32::System::DataExchange::{GetClipboardData, IsClipboardFormatAvailable};

    let format = owner_format()?;
    if unsafe { IsClipboardFormatAvailable(format) }.is_err() {
        return Ok(None);
    }
    let handle = unsafe { GetClipboardData(format) }
        .map_err(|e| crate::api_err("GetClipboardData(ZXraw Explorer Clipboard Owner)", e))?;
    match read_global_u64(handle, "GlobalLock(ZXraw Explorer Clipboard Owner)") {
        Ok(value) => Ok(Some(ClipboardToken(value))),
        Err(crate::WinError::Invalid(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Empties the clipboard if its owner token is `token`: `Ok(true)` when emptied, `Ok(false)`
/// when the clipboard holds something else (not ours any more). `Err` is a clipboard
/// failure (another app holding it open, say) and worth trying again.
#[cfg(windows)]
pub fn clear_if_token(token: ClipboardToken) -> Result<bool> {
    use windows::Win32::System::DataExchange::EmptyClipboard;

    let window = ClipboardOwnerWindow::new()?;
    let _clipboard = open_clipboard_retry(Some(window.hwnd))?;
    if read_owner_token_open()? != Some(token) {
        return Ok(false);
    }
    unsafe { EmptyClipboard() }.map_err(|e| crate::api_err("EmptyClipboard", e))?;
    Ok(true)
}

#[cfg(not(windows))]
pub fn clear_if_token(_token: ClipboardToken) -> Result<bool> {
    Err(crate::WinError::Unsupported)
}

/// If the owner token is `token`, replaces the clipboard with `paths` as a new publish of ours
/// and returns the new token. Empty `paths` clears the clipboard instead. If the token differs
/// (another app took the clipboard), nothing changes and the result is `Ok(None)`.
#[cfg(windows)]
pub fn replace_files_if_token(
    paths: &[PathBuf],
    effect: ClipboardEffect,
    token: ClipboardToken,
) -> Result<Option<ClipboardToken>> {
    use windows::Win32::System::DataExchange::EmptyClipboard;

    let new_token = next_token();
    // Allocate before opening, so a bad path or allocation failure leaves the clipboard alone.
    let mut payloads = if paths.is_empty() {
        Vec::new()
    } else {
        build_payloads(paths, effect, Some(new_token))?
    };
    let _window = ClipboardOwnerWindow::new()?;
    let _clipboard = open_clipboard_retry(Some(_window.hwnd))?;
    if read_owner_token_open()? != Some(token) {
        return Ok(None);
    }
    unsafe { EmptyClipboard() }.map_err(|e| crate::api_err("EmptyClipboard", e))?;
    if paths.is_empty() {
        return Ok(None);
    }
    write_payloads(&mut payloads)?;
    Ok(Some(new_token))
}

#[cfg(not(windows))]
pub fn replace_files_if_token(
    _paths: &[PathBuf],
    _effect: ClipboardEffect,
    _token: ClipboardToken,
) -> Result<Option<ClipboardToken>> {
    Err(crate::WinError::Unsupported)
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
    read_global_bytes::<4>(handle, op).map(u32::from_ne_bytes)
}

#[cfg(windows)]
fn read_global_u64(handle: windows::Win32::Foundation::HANDLE, op: &'static str) -> Result<u64> {
    read_global_bytes::<8>(handle, op).map(u64::from_ne_bytes)
}

/// Copies the first `N` bytes of a global handle. Fails if the handle is shorter.
#[cfg(windows)]
fn read_global_bytes<const N: usize>(
    handle: windows::Win32::Foundation::HANDLE,
    op: &'static str,
) -> Result<[u8; N]> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    let hglobal = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(hglobal) };
    if size < N {
        return Err(crate::WinError::Invalid(format!(
            "clipboard value is shorter than {N} bytes"
        )));
    }
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        return Err(crate::api_err(op, windows::core::Error::from_thread()));
    }
    let mut bytes = [0_u8; N];
    unsafe {
        std::ptr::copy_nonoverlapping(ptr.cast::<u8>(), bytes.as_mut_ptr(), N);
        // A zero return is normal when the final lock is released.
        let _ = GlobalUnlock(hglobal);
    }
    Ok(bytes)
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

    #[test]
    fn a_publish_waits_for_nothing_newer_from_another_app() {
        let _guard = ClipboardTestGuard::new();
        let ours = vec![PathBuf::from(r"C:\owner-a.txt")];
        let theirs = vec![PathBuf::from(r"C:\theirs.txt")];
        // Unchanged since the baseline, or changed only by us: written.
        let baseline = clipboard_sequence();
        assert!(
            set_files_if_unchanged(&ours, ClipboardEffect::Move, baseline)
                .unwrap()
                .is_some()
        );
        assert!(
            set_files_if_unchanged(&ours, ClipboardEffect::Move, baseline)
                .unwrap()
                .is_some()
        );
        // Another app copied since: left alone.
        set_files_without_text(&theirs, ClipboardEffect::Copy).unwrap();
        assert_eq!(
            set_files_if_unchanged(&ours, ClipboardEffect::Move, baseline).unwrap(),
            None
        );
        assert_eq!(get_files().unwrap(), Some((theirs, ClipboardEffect::Copy)));
    }

    #[test]
    fn owner_token_round_trips_and_differs_per_publish() {
        let _guard = ClipboardTestGuard::new();
        let paths = vec![PathBuf::from(r"C:\owner-a.txt")];
        let first = set_files(&paths, ClipboardEffect::Copy).unwrap();
        let second = set_files(&paths, ClipboardEffect::Move).unwrap();
        assert_ne!(first, second);
        let read = get_files_and_owner().unwrap();
        assert_eq!(
            (read.files, read.owner),
            (Some((paths.clone(), ClipboardEffect::Move)), Some(second))
        );
        assert!(second.is_from_this_process());
        assert!(
            !ClipboardToken::from_raw(u64::from(std::process::id() ^ 1) << 32 | 5)
                .is_from_this_process()
        );
    }

    #[test]
    fn another_apps_identical_list_is_not_ours() {
        let _guard = ClipboardTestGuard::new();
        let paths = vec![PathBuf::from(r"C:\owner-a.txt")];
        let token = set_files(&paths, ClipboardEffect::Copy).unwrap();
        set_files_without_text(&paths, ClipboardEffect::Move).unwrap();

        let read = get_files_and_owner().unwrap();
        assert_eq!(
            (read.files, read.owner),
            (Some((paths.clone(), ClipboardEffect::Move)), None)
        );
        assert!(!clear_if_token(token).unwrap());
        assert_eq!(
            get_files().unwrap(),
            Some((paths.clone(), ClipboardEffect::Move))
        );
    }

    #[test]
    fn clear_if_token_clears_only_our_publish() {
        let _guard = ClipboardTestGuard::new();
        let paths = vec![PathBuf::from(r"C:\owner-a.txt")];
        let stale = set_files(&paths, ClipboardEffect::Copy).unwrap();
        let current = set_files(&paths, ClipboardEffect::Copy).unwrap();

        assert!(!clear_if_token(stale).unwrap());
        assert!(get_files().unwrap().is_some());
        assert!(clear_if_token(current).unwrap());
        assert_eq!(get_files().unwrap(), None);
    }

    #[test]
    fn replace_files_if_token_updates_only_our_publish() {
        let _guard = ClipboardTestGuard::new();
        let a = PathBuf::from(r"C:\owner-a.txt");
        let b = PathBuf::from(r"C:\owner-b.txt");
        let token = set_files(&[a.clone(), b.clone()], ClipboardEffect::Copy).unwrap();

        let replaced =
            replace_files_if_token(std::slice::from_ref(&b), ClipboardEffect::Move, token).unwrap();
        let new_token = replaced.expect("our publish is replaced");
        assert_ne!(new_token, token);
        let read = get_files_and_owner().unwrap();
        assert_eq!(
            (read.files, read.owner),
            (
                Some((vec![b.clone()], ClipboardEffect::Move)),
                Some(new_token)
            )
        );

        // The old token no longer matches, so another replace changes nothing.
        assert_eq!(
            replace_files_if_token(std::slice::from_ref(&a), ClipboardEffect::Move, token).unwrap(),
            None
        );
        assert_eq!(
            get_files().unwrap(),
            Some((vec![b.clone()], ClipboardEffect::Move))
        );

        // Empty paths clear the clipboard.
        assert_eq!(
            replace_files_if_token(&[], ClipboardEffect::Move, new_token).unwrap(),
            None
        );
        assert_eq!(get_files().unwrap(), None);
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
mod write_tests {
    use super::write_all_or_empty;

    #[test]
    fn a_write_that_fails_part_way_empties_the_clipboard() {
        let mut written = Vec::new();
        let mut emptied = false;
        let mut formats = ["owner", "files", "effect", "text"];
        let result = write_all_or_empty(
            &mut formats,
            |f| {
                if *f == "effect" {
                    return Err(crate::WinError::Invalid("SetClipboardData failed".into()));
                }
                written.push(*f);
                Ok(())
            },
            || emptied = true,
        );
        assert!(result.is_err());
        assert!(emptied, "half a publish must not stay on the clipboard");
        assert_eq!(written, ["owner", "files"], "the owner token goes first");

        let mut emptied = false;
        assert!(write_all_or_empty(&mut formats, |_| Ok(()), || emptied = true).is_ok());
        assert!(!emptied);
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
