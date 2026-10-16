#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
mod app_icon;
mod batch_export;
mod batch_rename;
mod breadcrumb;
mod collections_ui;
mod commands;
mod crash;
mod default_app;
mod delivery_meta;
mod delivery_presets;
mod dialogs;
mod dragout;
mod export_assoc;
mod extra_tools_ui;
mod ffmpeg_setup;
mod folder_sizes;
mod fonts;
mod git_status;
mod gps_ui;
mod ingest_ui;
mod layouts;
mod library;
mod media_durations;
mod media_meta;
mod meta_edit;
mod meta_export;
mod modal;
mod notices;
mod ops_ui;
mod pane;
mod path_probe;
mod palette;
mod paste_key;
mod platform;
mod preview;
mod replace_ui;
mod search_ui;
mod selection;
mod tags_ui;
mod watermark;
mod terminal;
mod settings;
mod settings_ui;
mod sidebar;
mod similar;
mod startup;
// The automation hook exists only in debug builds or with the `automation` feature;
// release builds get an inert stub with the same surface.
#[cfg(any(debug_assertions, feature = "automation"))]
mod testhook;
#[cfg(not(any(debug_assertions, feature = "automation")))]
#[path = "testhook_off.rs"]
mod testhook;
mod theme;
mod thumbs;
mod tools_ui;
mod updater;
mod video_ui;
mod viewer;
mod viewer_host;
mod views;
mod watcher;

use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Installer / uninstaller switches: each does its job and exits without opening a window.
fn run_switch(argument: &std::ffi::OsStr) -> Option<anyhow::Result<&'static str>> {
    let exe = || std::env::current_exe().map_err(anyhow::Error::from);
    Some(match argument.to_str()? {
        "--register-context-menu" => exe()
            .and_then(|exe| forge_win::register_context_menu(exe).map_err(Into::into))
            .map(|()| "Explorer context menu registered."),
        "--unregister-context-menu" => forge_win::unregister_context_menu()
            .map_err(Into::into)
            .map(|()| "Explorer context menu removed."),
        "--unregister-default-folder" => forge_win::unregister_default_folder_handler()
            .map_err(Into::into)
            .map(|()| "Default folder and drive handlers restored."),
        "--register-file-types" => exe()
            .and_then(|exe| {
                forge_win::register_file_types(exe, &default_app::file_types()).map_err(Into::into)
            })
            .map(|()| "Photo file types registered; confirm the default in Settings > Default apps."),
        "--unregister-file-types" => forge_win::unregister_file_types()
            .map_err(Into::into)
            .map(|()| "Photo file types removed."),
        _ => return None,
    })
}

fn main() -> eframe::Result {
    if let Some(argument) = std::env::args_os().nth(1)
        && let Some(result) = run_switch(&argument)
    {
        match result {
            Ok(message) => {
                println!("{message}");
                return Ok(());
            }
            Err(error) => {
                eprintln!("{}: {error}", argument.to_string_lossy());
                std::process::exit(1);
            }
        }
    }

    let paths = settings::Paths::resolve();

    // Rolling daily log file; file paths are only logged at debug level.
    let file = tracing_appender::rolling::daily(&paths.logs, "zxraw.log");
    let (writer, _guard) = tracing_appender::non_blocking(file);
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("ZXRAW_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_writer(writer).with_ansi(false))
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    #[cfg(windows)]
    if !testhook::env_requests_automation() {
        // Lets the installer see that the app is running (`AppMutex` in zxraw.iss).
        if let Err(error) = forge_win::process::hold_named_mutex(updater::APP_MUTEX) {
            tracing::warn!("could not create the app mutex: {error}");
        }
        match std::env::current_exe() {
            Ok(current_exe) => match forge_win::refresh_if_registered(&current_exe) {
                Ok(true) => tracing::info!("refreshed the Explorer context-menu command path"),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!("could not refresh the Explorer context-menu command: {error}")
                }
            },
            Err(error) => {
                tracing::warn!(
                    "could not determine the executable path for context-menu refresh: {error}"
                )
            }
        }
        default_app::refresh_registration();
        match std::env::current_exe() {
            Ok(current_exe) => match forge_win::refresh_default_folder_handler(&current_exe) {
                Ok(true) => tracing::info!("refreshed the default-folder command path"),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!("could not refresh the default-folder command: {error}")
                }
            },
            Err(error) => tracing::warn!(
                "could not determine the executable path for folder refresh: {error}"
            ),
        }
    }

    let default_hook = std::panic::take_hook();
    let crash_dir = paths.data.join("crashes");
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        let message = crash::panic_message(info.payload());
        let location = info.location().map(|location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        });
        crash::write_crash_report(
            &crash_dir,
            &message,
            location.as_deref(),
            &std::backtrace::Backtrace::force_capture(),
        );
        default_hook(info);
    }));
    tracing::info!("ZXraw Explorer {} starting", env!("CARGO_PKG_VERSION"));
    let pending_crash = crash::pending_crash_report(&paths);
    if let Some(report) = &pending_crash {
        tracing::warn!(
            "the previous session ended with a crash; report saved at {}",
            report.display()
        );
    }

    // Read early so the window opens with the chosen icon instead of switching after.
    let startup = settings::load_json::<settings::Settings>(&paths.config.join("settings.json"));
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("ZXraw Explorer")
        .with_inner_size([1360.0, 840.0])
        .with_min_inner_size([640.0, 400.0])
        .with_icon(app_icon::icon_data(*startup.app_icon))
        .with_drag_and_drop(true);
    let hidden = testhook::hide_window();
    if hidden {
        // Automated runs open on a private desktop that is never shown, so nobody can see or
        // click them (see `testhook::hide_window`). Their geometry is not saved.
        let desktop = format!("zxraw-automation-{}", std::process::id());
        if let Err(e) = forge_win::desktop::move_thread_to_private_desktop(&desktop) {
            // Still usable: the window then just appears on the user's desktop.
            tracing::warn!("could not open the automation desktop: {e}");
        }
        viewport = viewport.with_active(false).with_taskbar(false);
    }
    // eframe stores viewport geometry separately from ZXraw's own settings. Keep it in the same
    // throwaway profile when UI automation or an explicit profile is active.
    let persistence_path = (cfg!(any(debug_assertions, feature = "automation"))
        && (testhook::env_requests_automation()
            || std::env::var_os("ZXRAW_PROFILE_DIR").is_some()))
    .then(|| paths.data.join("eframe.ron"));
    // Taken when the app is created, so a retry can tell whether the app started.
    let app_args = std::rc::Rc::new(std::cell::RefCell::new(Some((paths, pending_crash))));
    let run = |dx12_only: bool| {
        // A saved maximized state is applied after the window is created, not by winit, so the
        // hidden window never flashes on screen (see `startup`). Fresh for each attempt.
        let maximize = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let options = eframe::NativeOptions {
            // Sees Ctrl+V even when egui-winit drops it. See `paste_key`.
            event_loop_builder: Some(Box::new(|builder| {
                use winit::platform::windows::EventLoopBuilderExtWindows;
                builder.with_msg_hook(paste_key::msg_hook);
            })),
            viewport: viewport.clone(),
            persistence_path: persistence_path.clone(),
            persist_window: !hidden,
            // Strips `maximized` from the builder so winit does not maximize the hidden window.
            // See `startup::defer_maximized`.
            window_builder: Some(Box::new({
                let maximize = maximize.clone();
                move |builder| {
                    let (builder, maximized) = startup::defer_maximized(builder);
                    maximize.store(maximized, std::sync::atomic::Ordering::Relaxed);
                    builder
                }
            })),
            // DX12 alone skips loading the other drivers at startup. See `startup::wgpu_options`.
            wgpu_options: startup::wgpu_options(dx12_only),
            ..Default::default()
        };
        let app_args = app_args.clone();
        eframe::run_native(
            "ZXraw Explorer",
            options,
            Box::new(move |cc| {
                // Maximizes the hidden window while it is cloaked. See `startup::maximize_hidden`.
                if maximize.load(std::sync::atomic::Ordering::Relaxed) {
                    startup::maximize_hidden(cc);
                }
                let (paths, pending_crash) = app_args
                    .borrow_mut()
                    .take()
                    .expect("the app is created once per run");
                Ok(Box::new(app::ZxrawApp::new(cc, paths, pending_crash)))
            }),
        )
    };
    let dx12_only = startup::dx12_only_by_default();
    let result = run(dx12_only);
    if startup::retry_with_default_backends(dx12_only, app_args.borrow().is_none(), &result) {
        if let Err(error) = &result {
            tracing::warn!(
                "DX12 graphics setup failed ({error}); retrying with the default graphics backends"
            );
        }
        return run(false);
    }
    result
}
