//! Ctrl+V when the clipboard holds no text.
//!
//! egui-winit turns Ctrl+V (and Shift+Insert) into `Event::Paste` only when the clipboard has
//! text, and drops the key press otherwise. Files cut or copied in Explorer put no text on the
//! clipboard, so pasting them did nothing at all. A Win32 message hook counts paste key
//! presses, and the raw-input hook adds an empty `Event::Paste` for any that egui-winit
//! dropped. Text fields ignore an empty paste.

use std::sync::atomic::{AtomicU32, Ordering};

static PRESSES: AtomicU32 = AtomicU32::new(0);

/// Records one paste key press that egui may or may not have turned into an event.
pub fn note_press() {
    PRESSES.fetch_add(1, Ordering::Relaxed);
}

/// Message hook for the winit event loop. Never consumes the message.
pub fn msg_hook(msg: *const std::ffi::c_void) -> bool {
    if forge_win::is_paste_keydown(msg) {
        note_press();
    }
    false
}

/// Adds the paste event egui-winit dropped, if a paste key was pressed since the last frame.
pub fn restore_dropped(raw: &mut egui::RawInput) {
    restore_with(PRESSES.swap(0, Ordering::Relaxed), &mut raw.events);
}

fn restore_with(presses: u32, events: &mut Vec<egui::Event>) {
    if presses > 0 && !events.iter().any(|e| matches!(e, egui::Event::Paste(_))) {
        events.push(egui::Event::Paste(String::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_paste_key_becomes_an_empty_paste() {
        let mut events = Vec::new();
        restore_with(1, &mut events);
        assert_eq!(events, vec![egui::Event::Paste(String::new())]);
    }

    #[test]
    fn a_paste_egui_already_reported_is_not_doubled() {
        let mut events = vec![egui::Event::Paste("text".into())];
        restore_with(2, &mut events);
        assert_eq!(events.len(), 1);
        let mut none = Vec::new();
        restore_with(0, &mut none);
        assert!(none.is_empty());
    }
}
