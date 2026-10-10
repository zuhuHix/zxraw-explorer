#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
mod batch_rename;
mod breadcrumb;
mod commands;
mod fonts;
mod modal;
mod ops_ui;
mod pane;
mod palette;
mod platform;
mod preview;
mod settings;
mod sidebar;
mod testhook;
mod theme;
mod thumbs;
mod views;

use tracing_subscriber::{EnvFilter, fmt, prelude::*};

fn main() -> eframe::Result {
    let paths = settings::Paths::resolve();

    // Rolling daily log file; file paths are only logged at debug level.
    let file = tracing_appender::rolling::daily(&paths.logs, "zxraw.log");
    let (writer, _guard) = tracing_appender::non_blocking(file);
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("ZXRAW_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_writer(writer).with_ansi(false))
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_hook(info);
    }));
    tracing::info!("ZXraw Explorer {} starting", env!("CARGO_PKG_VERSION"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ZXraw Explorer")
            .with_inner_size([1360.0, 840.0])
            .with_min_inner_size([640.0, 400.0])
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
