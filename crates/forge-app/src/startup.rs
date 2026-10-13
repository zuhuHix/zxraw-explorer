//! Startup window and graphics setup.
//!
//! Two things made the first launch slow and ugly:
//! - eframe creates the window hidden. When the saved state is maximized, winit maximizes it
//!   while it is still hidden with `ShowWindow(SW_MAXIMIZE)` and hides it again, so a blank
//!   window flashed on screen several times. We keep `maximized` away from winit and maximize
//!   the hidden window ourselves, cloaked (see [`maximize_hidden`]).
//! - eframe's default wgpu setup loads the Vulkan and GL drivers as well, which takes seconds
//!   on cold starts. We use DX12 alone (see [`wgpu_options`]).

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
        tracing::warn!("could not maximize the hidden window ({error}); maximizing through egui");
        maximize_through_egui(cc);
    }
}

fn maximize_through_egui(cc: &eframe::CreationContext<'_>) {
    cc.egui_ctx
        .send_viewport_cmd(egui::ViewportCommand::Maximized(true));
}

/// Picks the wgpu backends. `None` keeps eframe's default setup (Vulkan, DX12 and GL), which
/// is used when the user chose a backend with `WGPU_BACKEND`.
///
/// Otherwise DX12 alone: Windows 10 and 11 always have it, wgpu prefers the hardware adapter,
/// and the WARP software adapter is the fallback when the GPU driver has no DX12 support.
/// Probing for a hardware adapter first was measured at 350-650 ms, more than it saves.
fn choose_backends(env_override: bool) -> Option<Backends> {
    (!env_override).then_some(Backends::DX12)
}

/// The wgpu configuration for the main window: eframe's defaults, with the backends narrowed
/// to DX12 when [`choose_backends`] says so.
pub fn wgpu_options() -> WgpuConfiguration {
    let mut options = WgpuConfiguration::default();
    match choose_backends(std::env::var_os("WGPU_BACKEND").is_some()) {
        Some(backends) => {
            let mut setup = WgpuSetupCreateNew::without_display_handle();
            setup.instance_descriptor.backends = backends;
            options.wgpu_setup = WgpuSetup::CreateNew(setup);
        }
        None => tracing::info!("graphics backends: eframe default (WGPU_BACKEND is set)"),
    }
    options
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

    #[test]
    fn dx12_only_unless_the_user_chose_a_backend() {
        assert_eq!(choose_backends(false), Some(Backends::DX12));
        assert_eq!(choose_backends(true), None);
    }
}
