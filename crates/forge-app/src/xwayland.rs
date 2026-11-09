//! Starts ZXraw through winit's X11 backend under a Wayland session, so drag and drop works
//! through XWayland. winit 0.30 has no drag and drop on Wayland.
//!
//! The choice is read before the window is created: the setting (`use_xwayland`) or the
//! environment variable `ZXRAW_X11` turns it on, and `ZXRAW_X11=0` turns it off again. It is
//! only applied when XWayland is there to use (`WAYLAND_DISPLAY` and `DISPLAY` are both set).

use std::ffi::OsStr;

/// Whether the X11 backend should be used.
///
/// True when the setting is on or `ZXRAW_X11` is `1`, `true` or `yes` (any case), and both
/// `WAYLAND_DISPLAY` and `DISPLAY` are set and not empty. `ZXRAW_X11=0` (also `false`, `no`)
/// forces it off, even with the setting on.
pub fn want_x11(
    setting: bool,
    env_zxraw_x11: Option<&OsStr>,
    wayland_display: Option<&OsStr>,
    display: Option<&OsStr>,
) -> bool {
    let env = env_zxraw_x11
        .and_then(|value| value.to_str())
        .map(|value| value.trim().to_ascii_lowercase());
    let forced_off = matches!(env.as_deref(), Some("0" | "false" | "no"));
    let env_on = matches!(env.as_deref(), Some("1" | "true" | "yes"));
    let requested = !forced_off && (setting || env_on);
    let present = |value: Option<&OsStr>| value.is_some_and(|value| !value.is_empty());
    requested && present(wayland_display) && present(display)
}

/// `want_x11` with the real environment. Always false off Linux, where winit has no X11
/// choice to make.
pub fn wanted(setting: bool) -> bool {
    cfg!(target_os = "linux")
        && want_x11(
            setting,
            std::env::var_os("ZXRAW_X11").as_deref(),
            std::env::var_os("WAYLAND_DISPLAY").as_deref(),
            std::env::var_os("DISPLAY").as_deref(),
        )
}

#[cfg(test)]
mod tests {
    use super::want_x11;
    use std::ffi::OsStr;

    fn os(value: &str) -> Option<&OsStr> {
        Some(OsStr::new(value))
    }

    fn wayland() -> Option<&'static OsStr> {
        Some(OsStr::new("wayland-0"))
    }

    fn x() -> Option<&'static OsStr> {
        Some(OsStr::new(":0"))
    }

    #[test]
    fn setting_on_under_wayland_with_xwayland_uses_x11() {
        assert!(want_x11(true, None, wayland(), x()));
    }

    #[test]
    fn setting_off_and_no_env_stays_on_wayland() {
        assert!(!want_x11(false, None, wayland(), x()));
    }

    #[test]
    fn env_turns_it_on_even_with_the_setting_off() {
        for value in ["1", "true", "yes", "TRUE", " Yes "] {
            assert!(want_x11(false, os(value), wayland(), x()), "{value:?}");
        }
    }

    #[test]
    fn env_zero_forces_it_off_even_with_the_setting_on() {
        for value in ["0", "false", "NO"] {
            assert!(!want_x11(true, os(value), wayland(), x()), "{value:?}");
        }
    }

    #[test]
    fn other_env_values_do_not_change_the_setting() {
        assert!(want_x11(true, os("maybe"), wayland(), x()));
        assert!(!want_x11(false, os("maybe"), wayland(), x()));
        assert!(!want_x11(false, os(""), wayland(), x()));
    }

    #[test]
    fn needs_wayland_and_xwayland() {
        assert!(!want_x11(true, None, None, x()), "no Wayland session");
        assert!(
            !want_x11(true, None, Some(OsStr::new("")), x()),
            "empty WAYLAND_DISPLAY"
        );
        assert!(
            !want_x11(true, None, wayland(), None),
            "no DISPLAY for XWayland"
        );
        assert!(
            !want_x11(true, os("1"), wayland(), Some(OsStr::new(""))),
            "empty DISPLAY"
        );
    }
}
