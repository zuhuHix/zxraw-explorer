//! Ctrl+V for files: when the clipboard holds no text, and exactly once per key press.
//!
//! egui-winit turns Ctrl+V (and Shift+Insert) into `Event::Paste` only when the clipboard has
//! text, and drops the key press otherwise. Files cut or copied in Explorer put no text on the
//! clipboard, so pasting them did nothing at all. egui-winit also reports a paste for every
//! auto-repeat of a held key and for Ctrl+Shift+V.
//!
//! A Win32 message hook counts fresh presses of exactly Ctrl+V or Shift+Insert. The raw-input
//! hook adds an empty `Event::Paste` for a press egui-winit dropped (text fields ignore an
//! empty paste), and arms one file paste for the frame: an `Event::Paste` runs the Paste
//! command only by taking it with [`take_file_paste`].

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static PRESSES: AtomicU32 = AtomicU32::new(0);
/// A fresh paste press arrived with this frame's input and no file paste has used it yet.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Records one fresh paste key press.
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

/// Called with each frame's raw input: arms a file paste for a fresh press, and adds the
/// paste event egui-winit dropped.
pub fn restore_dropped(raw: &mut egui::RawInput) {
    let presses = PRESSES.swap(0, Ordering::Relaxed);
    ARMED.store(presses > 0, Ordering::Relaxed);
    restore_with(presses, &mut raw.events);
}

/// Whether an `Event::Paste` seen this frame should paste files. True at most once per
/// fresh press, so a held key or a second handler never starts another transfer.
pub fn take_file_paste() -> bool {
    ARMED.swap(false, Ordering::Relaxed)
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

    #[test]
    fn one_press_allows_one_file_paste() {
        // One test touches the globals, so tests running in parallel cannot interfere.
        let mut raw = egui::RawInput::default();
        note_press();
        restore_dropped(&mut raw);
        assert!(take_file_paste());
        assert!(!take_file_paste(), "a second handler must not paste again");
        // Auto-repeat (or Ctrl+Shift+V): egui-winit reports a text paste, but no fresh
        // press was counted, so no file paste.
        let mut raw = egui::RawInput {
            events: vec![egui::Event::Paste("text".into())],
            ..Default::default()
        };
        restore_dropped(&mut raw);
        assert!(!take_file_paste());
        // An unused press does not carry over to a later frame.
        note_press();
        restore_dropped(&mut egui::RawInput::default());
        restore_dropped(&mut egui::RawInput::default());
        assert!(!take_file_paste());
    }
}
