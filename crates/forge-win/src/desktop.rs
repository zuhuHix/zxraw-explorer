//! A private, invisible Windows desktop for automated UI runs.
//!
//! Windows created on another desktop are never shown on the user's screen and cannot
//! receive the user's mouse or keyboard input, so tests can run while the PC is in use.

use crate::Result;

/// Moves the calling thread onto a new, private desktop named `name`, so every window the
/// thread creates afterwards lives there. Must be called before the thread creates any
/// window (Windows refuses the switch otherwise). The desktop closes when the process ends.
#[cfg(windows)]
pub fn move_thread_to_private_desktop(name: &str) -> Result<()> {
    use windows::Win32::System::StationsAndDesktops::{
        CreateDesktopW, DESKTOP_CONTROL_FLAGS, SetThreadDesktop,
    };
    use windows::core::PCWSTR;

    // GENERIC_ALL: the thread needs to create, draw and enumerate windows there.
    const GENERIC_ALL: u32 = 0x1000_0000;
    let wide = crate::to_wide(name);
    unsafe {
        let desktop = CreateDesktopW(
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            None,
            DESKTOP_CONTROL_FLAGS(0),
            GENERIC_ALL,
            None,
        )
        .map_err(|e| crate::api_err("CreateDesktopW", e))?;
        // The handle stays open for the life of the process: the desktop must outlive
        // every window on it.
        SetThreadDesktop(desktop).map_err(|e| crate::api_err("SetThreadDesktop", e))
    }
}

#[cfg(not(windows))]
pub fn move_thread_to_private_desktop(_name: &str) -> Result<()> {
    Err(crate::WinError::Unsupported)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_thread_can_move_to_a_private_desktop() {
        let name = format!("forge-win-test-{}", std::process::id());
        std::thread::spawn(move || move_thread_to_private_desktop(&name))
            .join()
            .unwrap()
            .unwrap();
    }
}
