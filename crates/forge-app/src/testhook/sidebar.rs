//! `ui sidebar <verb> [args]` verbs for Quick access pins and saved layouts.
//!
//! Pins are named by their display label (`crate::sidebar::pin_label`: the label, else the file
//! or folder name). Positions in `move-pin` are 1-based, as the list shows them. Paths are
//! resolved inside the script sandbox (`|` separates the two parts of a verb with two values).
//!
//! Quick access:
//! - `pins [label, label…]`: fail unless the pins' labels are exactly these, in this order.
//!   With no arguments it checks that there are no pins.
//! - `pin <path>` / `pin-file <path>`: add a folder or file pin, as the Pin commands do.
//! - `move-pin <from> <to>`: move the pin at position `from` (1-based) to position `to`.
//! - `rename-pin <label> | <new label>`: open the Rename prompt for the pin and accept it with
//!   the new text (a blank text clears the label). The prompt goes through the same accept path
//!   as the `accept` step.
//! - `unpin <label>`: remove the pin.
//! - `click-pin <label>`: what a click on the pin does (open, navigate, or report it missing).
//! - `wait-pin-status <label> present|missing [| seconds]`: wait until the last existence check
//!   of the pin reports that state (default 30 s).
//!
//! Saved layouts:
//! - `layouts [name, name…]`: fail unless the saved layouts have exactly these names, in order.
//!   With no arguments it checks that there are none.
//! - `load-layout <name>`: open the layout as "Load layout" does; fails if there is no such
//!   layout (a layout whose folders are missing is reported by its toast instead).
//!
//! Focused tab checks:
//! - `assert-split on|off`: whether the active tab shows two panes.
//! - `assert-view details|grid`: the view mode of the focused pane.
//!
//! Every verb fails with a message on bad arguments; waiting verbs fail after their timeout
//! with the status they last saw.

use std::path::Path;
use std::time::{Duration, Instant};

use super::{Flow, StepState};
use crate::actions;
use crate::app::ZxrawApp;
use crate::modal::{Modal, Prompt, PromptKind};
use crate::settings::ViewMode;
use crate::sidebar::{activate_pin, pin_label, pin_paths, pin_status};

const WAIT_LIMIT: Duration = Duration::from_secs(30);

pub(super) fn step(
    app: &mut ZxrawApp,
    _ctx: &egui::Context,
    verb: &str,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    match verb {
        "pins" => {
            let expected = names_list(args);
            let found: Vec<String> = app.settings.pins.iter().map(pin_label).collect();
            if found == expected {
                Ok(Flow::Done)
            } else {
                Err(format!("expected pins {expected:?}, found {found:?}"))
            }
        }
        "pin" | "pin-file" => {
            let path = super::sandboxed(Path::new(one(args)?))?;
            pin_paths(app, vec![(path, verb == "pin-file")]);
            Ok(Flow::Done)
        }
        "move-pin" => {
            let (from, to) = args
                .split_once(char::is_whitespace)
                .ok_or("expected `<from> <to>` (1-based positions)")?;
            let from = position(from)?;
            let to = position(to)?;
            let pins = &mut app.settings.pins;
            if from >= pins.len() || to >= pins.len() {
                return Err(format!(
                    "positions must be between 1 and {}, got {} and {}",
                    pins.len(),
                    from + 1,
                    to + 1
                ));
            }
            // Same effect as the drag-and-drop reorder in the sidebar.
            let pin = pins.remove(from);
            pins.insert(to, pin);
            Ok(Flow::Done)
        }
        "rename-pin" => {
            let (label, text) = args
                .split_once(" | ")
                .ok_or("expected `<label> | <new label>`")?;
            let index = find_pin(app, label.trim())?;
            let path = app.settings.pins[index].path.clone();
            let prompt = Prompt::new(PromptKind::RenamePin { path }, "Rename pin", text.trim());
            match actions::accept(app, Modal::Prompt(prompt)) {
                None => Ok(Flow::Done),
                Some(_) => Err("the rename prompt stayed open".into()),
            }
        }
        "unpin" => {
            let index = find_pin(app, one(args)?)?;
            app.settings.pins.remove(index);
            Ok(Flow::Done)
        }
        "click-pin" => {
            let index = find_pin(app, one(args)?)?;
            activate_pin(app, index);
            Ok(Flow::Done)
        }
        "wait-pin-status" => {
            let (body, timeout) = match args.split_once('|') {
                Some((body, seconds)) => {
                    let seconds = seconds
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| "pin status timeout must be integer seconds")?;
                    if !(1..=120).contains(&seconds) {
                        return Err("pin status timeout must be between 1 and 120 seconds".into());
                    }
                    (body, Duration::from_secs(seconds))
                }
                None => (args, WAIT_LIMIT),
            };
            let (label, state) = body
                .trim()
                .rsplit_once(char::is_whitespace)
                .ok_or("expected `<label> present|missing`")?;
            let want = match state {
                "present" => "Present",
                "missing" => "Missing",
                other => return Err(format!("expected `present` or `missing`, not `{other}`")),
            };
            let index = find_pin(app, label.trim())?;
            let path = app.settings.pins[index].path.clone();
            let status = format!("{:?}", pin_status(app, &path));
            if status == want {
                Ok(Flow::Done)
            } else if elapsed(st) >= timeout {
                Err(format!(
                    "pin `{}` is {status} after {} s, expected {want}",
                    label.trim(),
                    timeout.as_secs()
                ))
            } else {
                Ok(Flow::Wait(format!(
                    "waiting for pin `{}` to be {want}",
                    label.trim()
                )))
            }
        }
        "layouts" => {
            let expected = names_list(args);
            let found: Vec<String> = app
                .settings
                .layouts
                .iter()
                .map(|l| l.name.clone())
                .collect();
            if found == expected {
                Ok(Flow::Done)
            } else {
                Err(format!("expected layouts {expected:?}, found {found:?}"))
            }
        }
        "load-layout" => {
            let name = one(args)?;
            if crate::layouts::load_by_name(app, name) {
                Ok(Flow::Done)
            } else {
                Err(format!("no saved layout named `{name}`"))
            }
        }
        "assert-split" => {
            let want = on_off(args)?;
            let split = app.tab().right.is_some();
            if split == want {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "expected the split to be {}, it is {}",
                    on_off_word(want),
                    on_off_word(split)
                ))
            }
        }
        "assert-view" => {
            let want = match one(args)? {
                "details" => ViewMode::Details,
                "grid" => ViewMode::Grid,
                other => return Err(format!("expected `details` or `grid`, not `{other}`")),
            };
            let found = app.pane().view;
            if found == want {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "expected the focused view {want:?}, found {found:?}"
                ))
            }
        }
        _ => Err(format!("unknown `ui sidebar` verb `{verb}`")),
    }
}

/// Index of the pin whose display label is `label` (case-sensitive, as shown).
fn find_pin(app: &ZxrawApp, label: &str) -> Result<usize, String> {
    let pins = &app.settings.pins;
    pins.iter()
        .position(|pin| pin_label(pin) == label)
        .ok_or_else(|| {
            let shown: Vec<String> = pins.iter().map(pin_label).collect();
            format!("no pin labelled `{label}`; pins are {shown:?}")
        })
}

/// Comma-separated names; an empty argument means an empty list.
fn names_list(args: &str) -> Vec<String> {
    let args = args.trim();
    if args.is_empty() {
        Vec::new()
    } else {
        args.split(',').map(|s| s.trim().to_string()).collect()
    }
}

fn one(args: &str) -> Result<&str, String> {
    let value = args.trim();
    if value.is_empty() {
        Err("expected a value".into())
    } else {
        Ok(value)
    }
}

/// A 1-based list position, converted to an index.
fn position(text: &str) -> Result<usize, String> {
    let n: usize = text
        .trim()
        .parse()
        .map_err(|_| format!("`{}` is not a position", text.trim()))?;
    n.checked_sub(1)
        .ok_or_else(|| "positions start at 1".to_string())
}

fn on_off(args: &str) -> Result<bool, String> {
    match one(args)? {
        "on" => Ok(true),
        "off" => Ok(false),
        other => Err(format!("expected `on` or `off`, not `{other}`")),
    }
}

fn on_off_word(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

fn elapsed(st: &mut StepState) -> Duration {
    st.started.get_or_insert_with(Instant::now).elapsed()
}
