#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    eframe::run_ui_native("Forge", eframe::NativeOptions::default(), |ui, _| {
        ui.label("Forge");
    })
}
