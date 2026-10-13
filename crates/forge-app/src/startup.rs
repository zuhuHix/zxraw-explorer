//! Startup window and graphics setup.
//!
//! Two things made the first launch slow and ugly:
//! - eframe creates the window hidden. When the saved state is maximized, winit maximizes it
//!   while it is still hidden with `ShowWindow(SW_MAXIMIZE)` and hides it again, so a blank
//!   window flashed on screen several times. We keep `maximized` away from winit and maximize
//!   the hidden window ourselves, cloaked (see [`maximize_hidden`]).
//! - eframe's default wgpu setup loads the Vulkan and GL drivers as well, which takes seconds
//!   on cold starts. We use DX12 alone (see [`wgpu_options`]). If that setup fails before the
//!   app starts, main runs again with eframe's default backends (see
//!   [`retry_with_default_backends`]).

use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};
use eframe::wgpu::Backends;
use eframe::wgpu::rwh::{HasWindowHandle, RawWindowHandle};

/// Takes `maximized` out of the builder so winit does not maximize the hidden window.
///
/// Returns the builder without it, and `true` when the window should be maximized later with
/// [`maximize_hidden`].
pub fn defer_maximized(mut builder: egui::ViewportBuilder) -> (egui::ViewportBuilder, bool) {
    if builder.maximized == Some(true) {
        builder.maximized = None;
        (builder, true)
    } else {
        (builder, false)
    }
}

/// Maximizes the hidden main window while it is cloaked, so the first shown frame is already
/// maximized. Falls back to egui's own maximize command when the native call is not possible.
pub fn maximize_hidden(cc: &eframe::CreationContext<'_>) {
    let hwnd = match cc.window_handle().map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
        Ok(_) => {
            tracing::warn!("window is not a Win32 window; maximizing through egui instead");
            return maximize_through_egui(cc);
        }
        Err(error) => {
            tracing::warn!("could not read the window handle ({error}); maximizing through egui");
            return maximize_through_egui(cc);
        }
    };
    if let Err(error) = forge_win::window::maximize_hidden(hwnd) {
        tracing::warn!("could not maximize the hidden window ({error}); trying to uncloak it");
        if let Err(error) = forge_win::window::uncloak(hwnd) {
            tracing::warn!("could not uncloak the window ({error}); window may stay invisible");
        }
        maximize_through_egui(cc);
    }
}

fn maximize_through_egui(cc: &eframe::CreationContext<'_>) {
    cc.egui_ctx
        .send_viewport_cmd(egui::ViewportCommand::Maximized(true));
}

/// Whether to use DX12 alone. `false` when the `WGPU_BACKEND` environment variable is set, which
/// restores eframe's own choice (Vulkan, DX12 and GL).
///
/// DX12 alone is the default: Windows 10 and 11 always have it, wgpu prefers the hardware
/// adapter, and the WARP software adapter is the fallback when the GPU driver has no DX12
/// support. When DX12 cannot start at all, main retries with eframe's default backends (see
/// [`retry_with_default_backends`]). Probing for a hardware adapter first was measured at
/// 350-650 ms, more than it saves.
pub fn dx12_only_by_default() -> bool {
    std::env::var_os("WGPU_BACKEND").is_none()
}

/// The wgpu configuration for the main window: eframe's defaults, with the backends narrowed
/// to DX12 when `dx12_only` is set.
pub fn wgpu_options(dx12_only: bool) -> WgpuConfiguration {
    let mut options = WgpuConfiguration::default();
    if dx12_only {
        let mut setup = WgpuSetupCreateNew::without_display_handle();
        setup.instance_descriptor.backends = Backends::DX12;
        options.wgpu_setup = WgpuSetup::CreateNew(setup);
    }
    options
}

/// Whether main should run the app again with eframe's default backends. That is only useful
/// when the DX12-only setup failed before the app started, so nothing was shown or done yet.
pub fn retry_with_default_backends(
    dx12_only: bool,
    app_started: bool,
    result: &eframe::Result,
) -> bool {
    dx12_only && !app_started && matches!(result, Err(eframe::Error::Wgpu(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defer_maximized_takes_maximized_true_out_of_the_builder() {
        let builder = egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 840.0])
            .with_maximized(true);
        let (builder, deferred) = defer_maximized(builder);
        assert!(deferred);
        assert_eq!(builder.maximized, None);
        assert_eq!(builder.inner_size, Some(egui::vec2(1360.0, 840.0)));
    }

    #[test]
    fn defer_maximized_leaves_other_values_alone() {
        let unmaximized = egui::ViewportBuilder::default().with_maximized(false);
        let (builder, deferred) = defer_maximized(unmaximized);
        assert!(!deferred);
        assert_eq!(builder.maximized, Some(false));

        let unset = egui::ViewportBuilder::default().with_inner_size([640.0, 400.0]);
        let (builder, deferred) = defer_maximized(unset);
        assert!(!deferred);
        assert_eq!(builder.maximized, None);
        assert_eq!(builder.inner_size, Some(egui::vec2(640.0, 400.0)));
    }

    fn no_surface_formats() -> eframe::Result {
        Err(eframe::Error::Wgpu(
            eframe::egui_wgpu::WgpuError::NoSurfaceFormatsAvailable,
        ))
    }

    #[test]
    fn retries_with_default_backends_when_dx12_setup_fails_before_the_app_starts() {
        assert!(retry_with_default_backends(
            true,
            false,
            &no_surface_formats()
        ));
    }

    #[test]
    fn does_not_retry_once_the_app_has_started() {
        assert!(!retry_with_default_backends(
            true,
            true,
            &no_surface_formats()
        ));
    }

    #[test]
    fn does_not_retry_when_already_using_default_backends() {
        assert!(!retry_with_default_backends(
            false,
            false,
            &no_surface_formats()
        ));
    }

    #[test]
    fn does_not_retry_on_success() {
        assert!(!retry_with_default_backends(true, false, &Ok(())));
    }

    #[test]
    fn does_not_retry_on_other_errors() {
        let error = eframe::Error::AppCreation("x".into());
        assert!(!retry_with_default_backends(true, false, &Err(error)));
    }
}
