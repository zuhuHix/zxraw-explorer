#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
mod batch_rename;
mod breadcrumb;
mod collections_ui;
mod commands;
mod crash;
mod extra_tools_ui;
mod fonts;
mod ingest_ui;
mod library;
mod media_meta;
mod modal;
mod ops_ui;
mod pane;
mod palette;
mod platform;
mod preview;
mod search_ui;
mod tags_ui;
mod settings;
mod settings_ui;
mod sidebar;
mod testhook;
mod theme;
mod thumbs;
mod tools_ui;
mod video_ui;
mod viewer;
mod viewer_host;
mod views;
mod watcher;

use tracing_subscriber::{EnvFilter, fmt, prelude::*};

fn main() -> eframe::Result {
    if let Some(argument) = std::env::args_os().nth(1) {
        let register = argument == "--register-context-menu";
        let unregister = argument == "--unregister-context-menu";
        if register || unregister {
            let result = if register {
                std::env::current_exe()
                    .map_err(anyhow::Error::from)
                    .and_then(|exe| {
                        forge_win::register_context_menu(exe).map_err(anyhow::Error::from)
                    })
            } else {
                forge_win::unregister_context_menu().map_err(anyhow::Error::from)
            };
            match result {
                Ok(()) => {
                    println!(
                        "Explorer context menu {}.",
                        if register { "registered" } else { "removed" }
                    );
                    return Ok(());
                }
                Err(error) => {
                    eprintln!("Could not update the Explorer context menu: {error}");
                    std::process::exit(1);
                }
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
    if let Some(report) = crash::pending_crash_report(&paths) {
        tracing::warn!(
            "the previous session ended with a crash; report saved at {}",
            report.display()
        );
    }

    let icon = image::load_from_memory(include_bytes!("../../../assets/zxraw.png"))
        .expect("embedded application icon PNG must be valid")
        .to_rgba8();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ZXraw Explorer")
            .with_inner_size([1360.0, 840.0])
            .with_min_inner_size([640.0, 400.0])
            .with_icon(egui::IconData {
                width: icon.width(),
                height: icon.height(),
                rgba: icon.into_raw(),
            })
            .with_drag_and_drop(true),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native(
        "ZXraw Explorer",
        options,
        Box::new(move |cc| Ok(Box::new(app::ZxrawApp::new(cc, paths)))),
    )
}
