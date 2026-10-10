fn main() -> eframe::Result {
    eframe::run_ui_native("hello", eframe::NativeOptions::default(), |ui, _| {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Hello from egui");
            ui.button("A button");
        });
    })
}
