//! Window placement helpers that must not flash on screen.
//!
//! A hidden window maximized with plain `ShowWindow(SW_MAXIMIZE)` is painted on screen for
//! a moment. [`maximize_hidden`] avoids that by cloaking the window with DWM while it is
//! maximized. [`uncloak`] removes that cloak again and verifies that it is gone.

use crate::{Result, WinError};

/// `DWM_CLOAKED_APP`: the window was cloaked by its own app (our cloak, not the shell's).
#[cfg(windows)]
const DWM_CLOAKED_APP: u32 = 0x1;

/// Converts a raw window handle into a Win32 `HWND`.
#[cfg(windows)]
fn hwnd_handle(hwnd: isize) -> windows::Win32::Foundation::HWND {
    windows::Win32::Foundation::HWND(hwnd as *mut std::ffi::c_void)
}

/// Sets or clears the app's DWM cloak (`DWMWA_CLOAK`). A cloaked window is composed off screen.
#[cfg(windows)]
fn set_cloak(hwnd: isize, on: bool) -> Result<()> {
    use std::ffi::c_void;

    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    use windows::core::BOOL;

    let value = BOOL(on as i32);
    unsafe {
        DwmSetWindowAttribute(
            hwnd_handle(hwnd),
            DWMWA_CLOAK,
            &value as *const BOOL as *const c_void,
            std::mem::size_of::<BOOL>() as u32,
        )
    }
    .map_err(|e| crate::api_err("DwmSetWindowAttribute", e))
}

/// Reads the `DWMWA_CLOAKED` flags of `hwnd`. Only the `DWM_CLOAKED_APP` bit is ours.
#[cfg(windows)]
fn read_cloaked(hwnd: isize) -> Result<u32> {
    use std::ffi::c_void;

    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};

    let mut flags: u32 = 0;
    unsafe {
        DwmGetWindowAttribute(
            hwnd_handle(hwnd),
            DWMWA_CLOAKED,
            &mut flags as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
    }
    .map_err(|e| crate::api_err("DwmGetWindowAttribute", e))?;
    Ok(flags)
}

/// The error reported when the cloak is still set after the last attempt.
#[cfg(windows)]
fn still_cloaked() -> WinError {
    WinError::Api {
        op: "DwmSetWindowAttribute(DWMWA_CLOAK)",
        code: 0,
        message: "window is still cloaked".into(),
    }
}

/// Maximizes the hidden top-level window `hwnd` and leaves it hidden, so that when it is
/// shown later it appears already maximized. The window is DWM-cloaked while it is
/// maximized, so the user never sees it.
///
/// If the window is already visible, it is simply maximized.
///
/// If cloaking fails, nothing is maximized and the error is returned. If an error is
/// returned after cloaking, [`uncloak`] has already tried twice and the window may still be
/// cloaked. The caller should call [`uncloak`] again before showing the window some other way.
#[cfg(windows)]
pub fn maximize_hidden(hwnd: isize) -> Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{
        IsWindowVisible, SW_HIDE, SW_MAXIMIZE, ShowWindow,
    };

    if hwnd == 0 {
        return Err(WinError::Invalid(
            "maximize_hidden needs a window handle, got 0".into(),
        ));
    }
    let handle = hwnd_handle(hwnd);

    unsafe {
        // Nothing to hide: just maximize it.
        if IsWindowVisible(handle).as_bool() {
            let _ = ShowWindow(handle, SW_MAXIMIZE);
            return Ok(());
        }
    }

    // Cloak first, so the SW_MAXIMIZE below never paints. SW_HIDE keeps the maximized
    // placement, so a later show appears maximized. If cloaking fails, stop here: the
    // window is not cloaked, so maximizing it would flash.
    set_cloak(hwnd, true)?;
    unsafe {
        let _ = ShowWindow(handle, SW_MAXIMIZE);
        let _ = ShowWindow(handle, SW_HIDE);
    }
    // Always uncloak, so the window is never left invisible to the user.
    uncloak(hwnd)
}

#[cfg(not(windows))]
pub fn maximize_hidden(_hwnd: isize) -> Result<()> {
    Err(WinError::Unsupported)
}

/// Removes the app's DWM cloak from `hwnd` and checks that it is gone. Tries twice.
///
/// After each attempt the `DWMWA_CLOAKED` flags are read, and the call succeeds when the
/// `DWM_CLOAKED_APP` bit is clear. Other bits (shell or inherited cloaking) belong to
/// someone else and are ignored. If the read itself fails, the result of the set call is
/// taken as the answer.
///
/// On error the window may still be cloaked: the last failed attempt's error is returned,
/// or a "still cloaked" error if the set call reported success but the bit stayed set.
#[cfg(windows)]
pub fn uncloak(hwnd: isize) -> Result<()> {
    if hwnd == 0 {
        return Err(WinError::Invalid(
            "uncloak needs a window handle, got 0".into(),
        ));
    }

    let mut last_err = still_cloaked();
    for _ in 0..2 {
        let set = set_cloak(hwnd, false);
        match read_cloaked(hwnd) {
            Ok(flags) if flags & DWM_CLOAKED_APP == 0 => return Ok(()),
            Ok(_) => {
                last_err = match set {
                    Err(e) => e,
                    Ok(()) => still_cloaked(),
                };
            }
            Err(_) => match set {
                Ok(()) => return Ok(()),
                Err(e) => last_err = e,
            },
        }
    }
    Err(last_err)
}

#[cfg(not(windows))]
pub fn uncloak(_hwnd: isize) -> Result<()> {
    Err(WinError::Unsupported)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::ffi::c_void;

    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, IsWindowVisible, IsZoomed, WINDOW_EX_STYLE,
        WS_OVERLAPPEDWINDOW,
    };
    use windows::core::{BOOL, PCWSTR, w};

    /// Destroys the test window when dropped, even if an assertion fails.
    struct Window(HWND);

    impl Drop for Window {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    /// Creates a hidden top-level window for a test.
    fn hidden_test_window() -> Window {
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
        Window(hwnd)
    }

    #[test]
    fn a_hidden_window_is_maximized_without_being_shown() {
        let window = hidden_test_window();
        let hwnd = window.0;

        maximize_hidden(hwnd.0 as isize).expect("maximize_hidden");

        unsafe {
            assert!(!IsWindowVisible(hwnd).as_bool(), "window must stay hidden");
            assert!(IsZoomed(hwnd).as_bool(), "window must be maximized");
        }
        assert_eq!(
            read_cloaked(hwnd.0 as isize).expect("read cloak") & DWM_CLOAKED_APP,
            0,
            "window must be uncloaked by the app after the call"
        );
        drop(window);
    }

    #[test]
    fn uncloak_clears_the_app_cloak() {
        let window = hidden_test_window();
        let raw = window.0.0 as isize;

        let on = BOOL(1);
        unsafe {
            DwmSetWindowAttribute(
                window.0,
                DWMWA_CLOAK,
                &on as *const BOOL as *const c_void,
                std::mem::size_of::<BOOL>() as u32,
            )
        }
        .expect("DwmSetWindowAttribute(DWMWA_CLOAK, TRUE)");
        assert_ne!(
            read_cloaked(raw).expect("read cloak") & DWM_CLOAKED_APP,
            0,
            "test setup: the app cloak must be set"
        );

        uncloak(raw).expect("uncloak");

        assert_eq!(
            read_cloaked(raw).expect("read cloak") & DWM_CLOAKED_APP,
            0,
            "the app cloak must be cleared"
        );
    }

    #[test]
    fn zero_hwnd_is_rejected() {
        assert!(matches!(maximize_hidden(0), Err(WinError::Invalid(_))));
    }

    #[test]
    fn uncloak_rejects_zero_hwnd() {
        assert!(matches!(uncloak(0), Err(WinError::Invalid(_))));
    }
}
