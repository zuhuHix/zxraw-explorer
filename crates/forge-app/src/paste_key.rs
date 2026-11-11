//! Ctrl+V for files: when the clipboard holds no text, and exactly once per key press.
//!
//! egui-winit turns Ctrl+V (and Shift+Insert) into `Event::Paste` only when the clipboard has
//! text, and drops the key press otherwise. Files cut or copied in Explorer put no text on the
//! clipboard, so pasting them did nothing at all. egui-winit also reports a paste for every
//! auto-repeat of a held key and for Ctrl+Shift+V.
//!
//! On Windows a Win32 message hook counts fresh presses of exactly Ctrl+V or Shift+Insert.
//! Elsewhere there is no such hook, so [`KeyWatch`] counts them from the events egui-winit
//! does deliver (see there). The raw-input hook adds an empty `Event::Paste` for a press
//! egui-winit dropped (text fields ignore an empty paste), and arms one file paste for the
//! frame: an `Event::Paste` runs the Paste command only by taking it with [`take_file_paste`].

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static PRESSES: AtomicU32 = AtomicU32::new(0);
/// A fresh paste press arrived with this frame's input and no file paste has used it yet.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Records one fresh paste key press.
pub fn note_press() {
    PRESSES.fetch_add(1, Ordering::Relaxed);
}

/// Message hook for the winit event loop. Never consumes the message.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn msg_hook(msg: *const std::ffi::c_void) -> bool {
    if forge_win::is_paste_keydown(msg) {
        note_press();
    }
    false
}

/// Called with each frame's raw input: arms a file paste for a fresh press, and adds the
/// paste event egui-winit dropped.
pub fn restore_dropped(raw: &mut egui::RawInput) {
    #[cfg(not(windows))]
    {
        static WATCH: std::sync::Mutex<KeyWatch> = std::sync::Mutex::new(KeyWatch::new());
        let mut watch = WATCH.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..watch.presses(&raw.events) {
            note_press();
        }
    }
    let presses = PRESSES.swap(0, Ordering::Relaxed);
    ARMED.store(presses > 0, Ordering::Relaxed);
    restore_with(presses, &mut raw.events);
}

/// Whether an `Event::Paste` seen this frame should paste files. True at most once per
/// fresh press, so a held key or a second handler never starts another transfer.
pub fn take_file_paste() -> bool {
    ARMED.swap(false, Ordering::Relaxed)
}

/// Fresh presses of exactly Ctrl+V or Shift+Insert, read from what egui-winit delivers where
/// no message hook sees the keyboard (Linux):
///
/// - Ctrl+V (any Ctrl+V chord) never arrives as a key press: it becomes `Event::Paste` when
///   the clipboard has text, again for every auto-repeat, and nothing at all otherwise. Its
///   key release still arrives. So the first paste event with Ctrl held while V is up is a
///   press; with no text, a V release with no press seen before it is one, judged by every
///   modifier held together with Ctrl (Ctrl+Shift+V stays Ctrl+Shift+V when Shift is let go
///   first).
/// - Shift+Insert is an ordinary key on Linux: a press while Insert is up.
#[cfg_attr(windows, allow(dead_code))]
struct KeyWatch {
    /// V went down (as a Ctrl+V paste or as a plain key) and has not been released yet.
    v_down: bool,
    insert_down: bool,
    /// The modifiers held, from `ModifiersChanged` (a paste event carries none).
    mods: egui::Modifiers,
    /// While Ctrl is held: every modifier pressed with it since Ctrl went down or since the
    /// last other key, so a press dropped by egui-winit is judged by the whole chord. Unknown
    /// after the window gains focus with Ctrl already down.
    chord: Option<egui::Modifiers>,
    /// The chord Ctrl was let go from, while nothing but modifier changes followed: what a
    /// dropped press was made with when Ctrl is let go before V.
    released_chord: Option<egui::Modifiers>,
}

#[cfg_attr(windows, allow(dead_code))]
impl KeyWatch {
    const fn new() -> Self {
        Self {
            v_down: false,
            insert_down: false,
            mods: egui::Modifiers::NONE,
            chord: None,
            released_chord: None,
        }
    }

    /// Something other than a modifier change or V happened: a later bare V release is no
    /// longer the end of the chord seen so far.
    /// Modifiers held now still count through the V release's own modifiers; ones pressed
    /// from here on are added to the chord.
    fn other_input(&mut self) {
        self.chord = self.mods.command.then_some(egui::Modifiers {
            alt: false,
            shift: false,
            ..self.mods
        });
        self.released_chord = None;
    }

    fn presses(&mut self, events: &[egui::Event]) -> u32 {
        use egui::{Event, Key, Modifiers};
        let mut presses = 0;
        for event in events {
            match event {
                Event::ModifiersChanged(m) => {
                    match (self.mods.command, m.command) {
                        (false, true) => {
                            self.chord = Some(*m);
                            self.released_chord = None;
                        }
                        (true, true) => self.chord = self.chord.map(|c| c | *m),
                        (true, false) => self.released_chord = self.chord,
                        (false, false) => {}
                    }
                    self.mods = *m;
                }
                // Also sent for a dedicated Paste key, with no Ctrl: that is no Ctrl+V.
                Event::Paste(_) if self.mods.command => {
                    if !self.v_down && self.mods.matches_exact(Modifiers::COMMAND) {
                        presses += 1;
                    }
                    self.v_down = true;
                }
                Event::Key {
                    key: Key::V,
                    pressed,
                    modifiers: m,
                    ..
                } => {
                    if *pressed {
                        self.v_down = true;
                        self.other_input();
                    } else {
                        let at_press = if m.command {
                            self.chord.map(|c| c | *m)
                        } else {
                            self.released_chord
                        };
                        let exact = at_press.is_some_and(|c| c.matches_exact(Modifiers::COMMAND));
                        if !self.v_down && exact {
                            presses += 1;
                        }
                        self.v_down = false;
                        self.other_input();
                    }
                }
                Event::Key {
                    key: Key::Insert,
                    pressed,
                    modifiers: m,
                    ..
                } => {
                    if *pressed && !self.insert_down && m.matches_exact(Modifiers::SHIFT) {
                        presses += 1;
                    }
                    self.insert_down = *pressed;
                    self.other_input();
                }
                Event::Key { .. } | Event::Text(_) | Event::Copy | Event::Cut | Event::Paste(_) => {
                    self.other_input()
                }
                // A release made in another window never reaches this one, and a key held
                // when the window gains focus was pressed elsewhere.
                Event::WindowFocused(focused) => {
                    self.v_down = false;
                    self.insert_down = false;
                    if !focused {
                        self.mods = Modifiers::NONE;
                    }
                    self.chord = None;
                    self.released_chord = None;
                }
                _ => {}
            }
        }
        presses
    }
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

    fn key(key: egui::Key, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers,
        }
    }

    /// Linux: what egui-winit delivers for Ctrl+V with and without text on the clipboard,
    /// held keys, other chords and Shift+Insert.
    #[test]
    fn the_key_watch_counts_each_fresh_paste_press_once() {
        use egui::{Event, Key, Modifiers};
        let (none, ctrl, shift) = (Modifiers::NONE, Modifiers::COMMAND, Modifiers::SHIFT);
        let cs = ctrl | shift;
        let mods = Event::ModifiersChanged;
        let paste = || Event::Paste("/media/usb/a.jpg".into());
        let v_up = |m| key(Key::V, false, m);
        let mut w = KeyWatch::new();
        // Text on the clipboard: a paste event on press, again for each auto-repeat.
        assert_eq!(w.presses(&[mods(ctrl), paste()]), 1);
        assert_eq!(w.presses(&[paste()]), 0, "auto-repeat");
        assert_eq!(w.presses(&[v_up(ctrl)]), 0);
        // Press and release in one frame, twice.
        assert_eq!(w.presses(&[paste(), v_up(ctrl)]), 1);
        assert_eq!(w.presses(&[paste(), v_up(ctrl), mods(none)]), 1);
        // No text: the press is dropped, only the release arrives.
        assert_eq!(w.presses(&[mods(ctrl), v_up(ctrl)]), 1);
        // ... also when Ctrl is let go before V.
        assert_eq!(w.presses(&[mods(none), v_up(none)]), 1);
        // Plain V (type to filter) is no paste.
        assert_eq!(w.presses(&[key(Key::V, true, none), v_up(none)]), 0);
        // Ctrl+Shift+V, with and without text.
        assert_eq!(w.presses(&[mods(cs), paste(), v_up(cs)]), 0);
        assert_eq!(w.presses(&[v_up(cs), mods(none)]), 0);
        // Shift+Insert: once per press, not for auto-repeat or Ctrl+Shift+Insert.
        let insert = |pressed, m| key(Key::Insert, pressed, m);
        assert_eq!(w.presses(&[mods(shift), insert(true, shift)]), 1);
        assert_eq!(w.presses(&[insert(true, shift)]), 0, "auto-repeat");
        assert_eq!(w.presses(&[insert(false, shift)]), 0);
        assert_eq!(
            w.presses(&[mods(cs), insert(true, cs), insert(false, cs)]),
            0
        );
        // V held while the window lost focus: the next Ctrl+V still counts.
        assert_eq!(w.presses(&[mods(ctrl), paste()]), 1);
        assert_eq!(w.presses(&[Event::WindowFocused(false)]), 0);
        assert_eq!(w.presses(&[mods(ctrl), paste()]), 1);
        assert_eq!(w.presses(&[v_up(ctrl), mods(none)]), 0);
    }

    /// Bare V releases that are not the end of a Ctrl+V: chords whose extra modifier is let
    /// go first, a stale chord, a V held into the window, and a dedicated Paste key.
    #[test]
    fn the_key_watch_judges_a_bare_release_by_the_whole_chord() {
        use egui::{Event, Key, Modifiers};
        let (none, ctrl, shift) = (Modifiers::NONE, Modifiers::COMMAND, Modifiers::SHIFT);
        let (cs, ca) = (ctrl | shift, ctrl | Modifiers::ALT);
        let mods = Event::ModifiersChanged;
        let paste = || Event::Paste("text".into());
        let v_up = |m| key(Key::V, false, m);
        let mut w = KeyWatch::new();
        // Ctrl+Shift+V and Ctrl+Alt+V, no text, the extra modifier let go before V.
        assert_eq!(
            w.presses(&[mods(ctrl), mods(cs), mods(ctrl), v_up(ctrl)]),
            0
        );
        assert_eq!(w.presses(&[mods(none)]), 0);
        assert_eq!(
            w.presses(&[mods(ctrl), mods(ca), mods(ctrl), v_up(ctrl)]),
            0
        );
        assert_eq!(w.presses(&[mods(none)]), 0);
        // ... and with Ctrl let go last, before V.
        let shift_first = [mods(ctrl), mods(cs), mods(ctrl), mods(none), v_up(none)];
        assert_eq!(w.presses(&shift_first), 0);
        // A new chord starts from Ctrl going down: plain Ctrl+V counts again.
        assert_eq!(w.presses(&[mods(ctrl), v_up(ctrl), mods(none)]), 1);
        // A chord is judged once: a later bare release is no Ctrl+V.
        assert_eq!(w.presses(&[mods(ctrl), mods(none), v_up(none)]), 1);
        assert_eq!(w.presses(&[v_up(none)]), 0);
        // Other input after Ctrl was let go ends the chord.
        let typed = [Event::Text("a".into()), key(Key::A, true, none)];
        assert_eq!(
            w.presses(&[mods(ctrl), mods(none), typed[0].clone(), v_up(none)]),
            0
        );
        assert_eq!(
            w.presses(&[mods(ctrl), mods(none), typed[1].clone(), v_up(none)]),
            0
        );
        // Ctrl+Shift+A, Shift let go, then Ctrl+V with no text: counts.
        let ctrl_a = key(Key::A, true, cs);
        assert_eq!(
            w.presses(&[mods(cs), ctrl_a, mods(ctrl), v_up(ctrl), mods(none)]),
            1
        );
        // V held while the window gains focus: its release is no paste.
        let focus = Event::WindowFocused(true);
        assert_eq!(
            w.presses(&[mods(ctrl), focus.clone(), v_up(ctrl), mods(none)]),
            0
        );
        assert_eq!(w.presses(&[mods(ctrl), mods(none), focus, v_up(none)]), 0);
        // A dedicated Paste key sends a paste with no Ctrl: it neither counts nor hides the
        // next Ctrl+V.
        assert_eq!(w.presses(&[paste()]), 0);
        assert_eq!(w.presses(&[mods(ctrl), paste(), v_up(ctrl), mods(none)]), 1);
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
