//! Window placement helpers that must not flash on screen.
//!
//! A hidden window maximized with plain `ShowWindow(SW_MAXIMIZE)` is painted on screen for
//! a moment. [`maximize_hidden`] avoids that by cloaking the window with DWM while it is
//! maximized.

use crate::{Result, WinError};

/// Maximizes the hidden top-level window `hwnd` and leaves it hidden, so that when it is
/// shown later it appears already maximized. The window is DWM-cloaked while it is
/// maximized, so the user never sees it.
///
/// If the window is already visible, it is simply maximized.
#[cfg(windows)]
pub fn maximize_hidden(hwnd: isize) -> Result<()> {
    use std::ffi::c_void;

    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    use windows::Win32::UI::WindowsAndMessaging::{
        IsWindowVisible, SW_HIDE, SW_MAXIMIZE, ShowWindow,
    };
    use windows::core::BOOL;

    if hwnd == 0 {
        return Err(WinError::Invalid(
            "maximize_hidden needs a window handle, got 0".into(),
        ));
    }
    let hwnd = HWND(hwnd as *mut c_void);

    // Sets or clears DWM cloaking. A cloaked window is composed off screen.
    let set_cloak = |on: bool| -> Result<()> {
        let value = BOOL(on as i32);
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_CLOAK,
                &value as *const BOOL as *const c_void,
                std::mem::size_of::<BOOL>() as u32,
            )
        }
        .map_err(|e| crate::api_err("DwmSetWindowAttribute", e))
    };

    unsafe {
        // Nothing to hide: just maximize it.
        if IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_MAXIMIZE);
            return Ok(());
        }
    }

    // Cloak first, so the SW_MAXIMIZE below never paints. SW_HIDE keeps the maximized
    // placement, so a later show appears maximized. If cloaking fails, stop here: the
    // window is not cloaked, so maximizing it would flash.
    set_cloak(true)?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_MAXIMIZE);
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    // Always uncloak, so the window is never left invisible to the user.
    set_cloak(false)
}

#[cfg(not(windows))]
pub fn maximize_hidden(_hwnd: isize) -> Result<()> {
    Err(WinError::Unsupported)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::ffi::c_void;

    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, IsWindowVisible, IsZoomed, WINDOW_EX_STYLE,
        WS_OVERLAPPEDWINDOW,
    };
    use windows::core::{PCWSTR, w};

    /// Destroys the test window when dropped, even if an assertion fails.
    struct Window(HWND);

    impl Drop for Window {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    #[test]
    fn a_hidden_window_is_maximized_without_being_shown() {
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                PCWSTR::null(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                400,
                300,
                None,
                None,
                None,
                None,
            )
        }
        .expect("CreateWindowExW");
        let window = Window(hwnd);

        maximize_hidden(hwnd.0 as isize).expect("maximize_hidden");

        unsafe {
            assert!(!IsWindowVisible(hwnd).as_bool(), "window must stay hidden");
            assert!(IsZoomed(hwnd).as_bool(), "window must be maximized");

            let mut cloaked: u32 = 1;
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut c_void,
                std::mem::size_of::<u32>() as u32,
            )
            .expect("DwmGetWindowAttribute");
            assert_eq!(cloaked, 0, "window must be uncloaked after the call");
        }
        drop(window);
    }

    #[test]
    fn zero_hwnd_is_rejected() {
        assert!(matches!(maximize_hidden(0), Err(WinError::Invalid(_))));
    }
}
