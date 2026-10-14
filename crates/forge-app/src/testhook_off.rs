//! Inert stand-in for the automation hook in release builds without the `automation`
//! feature: no environment variable can script, screenshot or reconfigure the app.

#[derive(Default)]
pub struct TestHook;

impl TestHook {
    pub fn from_env() -> Self {
        Self
    }

    pub fn active(&self) -> bool {
        false
    }

    pub fn record_toast(&mut self, _text: &str, _error: bool) {}
}

pub fn env_requests_automation() -> bool {
    false
}

pub fn hide_window() -> bool {
    false
}

pub fn raw_input(_app: &mut crate::app::ZxrawApp, _raw: &mut egui::RawInput) {}

pub fn tick(_app: &mut crate::app::ZxrawApp, _ctx: &egui::Context) {}

#[inline(always)]
pub(crate) fn note_entry_rect(_ui: &egui::Ui, _path: &std::path::Path, _rect: egui::Rect) {}

#[inline(always)]
pub(crate) fn note_arrow_rect(_ui: &egui::Ui, _path: &std::path::Path, _rect: egui::Rect) {}

#[inline(always)]
pub(crate) fn note_view_rect(_ui: &egui::Ui, _rect: egui::Rect) {}
