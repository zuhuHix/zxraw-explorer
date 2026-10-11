//! `ui viewer <verb> [args]` script verbs.
//!
//! - `assert-open single|cull|compare|none`: check the active viewer mode, or that it is closed.
//! - `assert-current <name>`: check the viewer's current filename.
//! - `wait-decoded [secs]`: wait for the current viewer item to decode or fail (default 30 s).
//! - `assert-error-shown`: require the current viewer item's decode-error placeholder.
//! - `assert-error-contains <text>`: require the current viewer error to contain the text.
//! - `assert-rating <name> <0-5>`: poll a fresh library DB query until the rating matches.
//! - `assert-label <name> <color|none>`: check a persisted red/yellow/green/blue/purple label.
//! - `assert-pick <name> <pick|reject|none>`: check the persisted pick state.
//! - `wait-preview-error <name> [secs]` / `assert-preview-error <name>`: wait for or check
//!   the thumbnail failure placeholder for the selected preview-panel item.
//! - `wait-video-unavailable <name> [secs]` / `assert-video-unavailable <name>`: check the
//!   selected video preview reports missing FFmpeg tools.
//! - `thumbs-idle [secs]`: wait for thumbnail queues, workers, results and uploads to drain
//!   (default 30 s).
//! - `assert-thumbs-pending <n>`: require at least n queued/running results or pending uploads.
//! - `assert-thumbs-cancelled <n>`: require at least n thumbnail jobs to have been cancelled.
//! - `assert-keys-free`: require that no command palette is open and no text field has
//!   keyboard focus, so viewer keys act again.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use forge_core::query::{ColorLabel, PickState};

use super::{Flow, StepState};
use crate::app::ZxrawApp;
use crate::viewer::ViewerMode;

const WAIT_LIMIT: Duration = Duration::from_secs(30);
const ANNOTATION_WAIT: Duration = Duration::from_secs(5);
const WAIT_MAX: u64 = 300;

pub(super) fn step(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    verb: &str,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    match verb {
        "assert-open" => {
            let expected = parse_open_mode(args)?;
            let found = app.viewer.as_ref().map(|viewer| viewer.mode());
            if found == expected {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "expected viewer mode {expected:?}, found {found:?}"
                ))
            }
        }
        "assert-current" => {
            let expected = one_arg(args, "expected a filename")?;
            let viewer = app.viewer.as_ref().ok_or("the viewer is not open")?;
            let Some(path) = viewer.items().get(viewer.current_index()) else {
                return Err("the viewer has no current item".into());
            };
            if filename_matches(path, expected) {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "expected current item `{expected}`, found `{}`",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ))
            }
        }
        "wait-decoded" => {
            let timeout = timeout_arg(args, WAIT_LIMIT)?;
            let viewer = app.viewer.as_ref().ok_or("the viewer is not open")?;
            if viewer.current_decode_complete() {
                Ok(Flow::Done)
            } else if elapsed(st) >= timeout {
                Err(format!(
                    "current viewer item did not decode within {} s",
                    timeout.as_secs()
                ))
            } else {
                Ok(Flow::Wait("waiting for current viewer decode".into()))
            }
        }
        "assert-error-shown" => {
            no_args(args)?;
            let viewer = app.viewer.as_ref().ok_or("the viewer is not open")?;
            if viewer.current_decode_error().is_some() {
                Ok(Flow::Done)
            } else {
                Err("the current viewer item has no decode-error placeholder".into())
            }
        }
        "assert-error-contains" => {
            let expected = args.trim();
            if expected.is_empty() {
                return Err("expected error text".into());
            }
            let viewer = app.viewer.as_ref().ok_or("the viewer is not open")?;
            match viewer.current_decode_error() {
                Some(error) if error.contains(expected) => Ok(Flow::Done),
                Some(error) => Err(format!(
                    "viewer error `{error}` did not contain `{expected}`"
                )),
                None => Err("the current viewer item has no decode error".into()),
            }
        }
        "assert-rating" | "assert-label" | "assert-pick" => {
            let (name, expected) = two_args(args, "expected a filename and value")?;
            match verb {
                "assert-rating" => {
                    let rating = expected
                        .parse::<u8>()
                        .map_err(|_| "rating must be between 0 and 5")?;
                    if rating > 5 {
                        return Err("rating must be between 0 and 5".into());
                    }
                    wait_annotation(app, name, st, "rating", |a| a.rating == rating)
                }
                "assert-label" => {
                    let label = parse_label(expected)?;
                    wait_annotation(app, name, st, "label", |a| a.label == label)
                }
                _ => {
                    let pick = parse_pick(expected)?;
                    wait_annotation(app, name, st, "pick", |a| a.pick == pick)
                }
            }
        }
        "wait-preview-error" | "assert-preview-error" => {
            let (name, timeout) = name_and_timeout(args, verb == "assert-preview-error")?;
            let path = pane_path(app, name)?;
            if !app.show_preview {
                return Err("the preview panel is not open".into());
            }
            if app.ops.thumbs.failed_path(&path) {
                Ok(Flow::Done)
            } else if verb == "assert-preview-error" {
                Err(format!(
                    "preview for `{name}` has no thumbnail-error placeholder yet"
                ))
            } else if elapsed(st) >= timeout {
                Err(format!(
                    "preview for `{name}` did not report a thumbnail error"
                ))
            } else {
                Ok(Flow::Wait(format!(
                    "waiting for preview decode failure for `{name}`"
                )))
            }
        }
        "wait-video-unavailable" | "assert-video-unavailable" => {
            let (name, timeout) = name_and_timeout(args, verb == "assert-video-unavailable")?;
            let entry = pane_entry(app, name)?;
            if app
                .pane()
                .cursor_entry()
                .is_none_or(|current| current.path != entry.path)
            {
                return Err(format!("`{name}` is not the current preview item"));
            }
            if app.ops.video_ui.tools_unavailable() {
                Ok(Flow::Done)
            } else if verb == "assert-video-unavailable" {
                Err(format!(
                    "video preview for `{name}` does not report missing tools"
                ))
            } else if elapsed(st) >= timeout {
                Err(format!(
                    "video preview for `{name}` did not report missing tools"
                ))
            } else {
                Ok(Flow::Wait(format!(
                    "waiting for missing-tool status for `{name}`"
                )))
            }
        }
        "assert-keys-free" => {
            no_args(args)?;
            if app.palette.is_some() {
                Err("the command palette is still open".into())
            } else if ctx.egui_wants_keyboard_input() {
                Err("a text field still has keyboard focus".into())
            } else {
                Ok(Flow::Done)
            }
        }
        "thumbs-idle" => {
            let timeout = timeout_arg(args, WAIT_LIMIT)?;
            if app.ops.thumbs.is_idle() {
                Ok(Flow::Done)
            } else if elapsed(st) >= timeout {
                Err(format!(
                    "thumbnail work did not drain within {} s",
                    timeout.as_secs()
                ))
            } else {
                Ok(Flow::Wait(format!(
                    "{} thumbnail result(s)/upload(s) pending",
                    app.ops.thumbs.pending_count()
                )))
            }
        }
        "assert-thumbs-pending" => {
            let expected = one_arg(args, "expected a minimum pending count")?
                .parse::<usize>()
                .map_err(|_| "pending count must be a non-negative integer")?;
            let found = app.ops.thumbs.pending_count();
            if found >= expected {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "expected at least {expected} thumbnail item(s), found {found}"
                ))
            }
        }
        "assert-thumbs-cancelled" => {
            let expected = one_arg(args, "expected a minimum cancellation count")?
                .parse::<u64>()
                .map_err(|_| "cancellation count must be a non-negative integer")?;
            let found = app.ops.thumbs.service().stats().cancelled;
            if found >= expected {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "expected at least {expected} cancelled thumbnail job(s), found {found}"
                ))
            }
        }
        _ => Err(format!("unknown `ui viewer` verb `{verb}`")),
    }
}

fn parse_open_mode(value: &str) -> Result<Option<ViewerMode>, String> {
    match value.trim() {
        "single" => Ok(Some(ViewerMode::Single)),
        "cull" => Ok(Some(ViewerMode::Cull)),
        "compare" => Ok(Some(ViewerMode::Compare)),
        "none" => Ok(None),
        _ => Err("expected single, cull, compare or none".into()),
    }
}

fn parse_label(value: &str) -> Result<Option<ColorLabel>, String> {
    match value {
        "red" => Ok(Some(ColorLabel::Red)),
        "yellow" => Ok(Some(ColorLabel::Yellow)),
        "green" => Ok(Some(ColorLabel::Green)),
        "blue" => Ok(Some(ColorLabel::Blue)),
        "purple" => Ok(Some(ColorLabel::Purple)),
        "none" => Ok(None),
        _ => Err("label must be red, yellow, green, blue, purple or none".into()),
    }
}

fn parse_pick(value: &str) -> Result<PickState, String> {
    match value {
        "pick" => Ok(PickState::Pick),
        "reject" => Ok(PickState::Reject),
        "none" => Ok(PickState::None),
        _ => Err("pick must be pick, reject or none".into()),
    }
}

fn filename_matches(path: &Path, name: &str) -> bool {
    path.file_name()
        .is_some_and(|found| found.to_string_lossy().eq_ignore_ascii_case(name))
}

fn pane_entry<'a>(app: &'a ZxrawApp, name: &str) -> Result<&'a forge_core::FileEntry, String> {
    let mut found = app
        .pane()
        .entries
        .iter()
        .filter(|entry| filename_matches(&entry.path, name));
    let entry = found
        .next()
        .ok_or_else(|| format!("no file named `{name}` in the current folder"))?;
    if found.next().is_some() {
        return Err(format!(
            "more than one file named `{name}` is in the current folder"
        ));
    }
    Ok(entry)
}

fn pane_path(app: &ZxrawApp, name: &str) -> Result<PathBuf, String> {
    Ok(pane_entry(app, name)?.path.clone())
}

fn annotation_path(app: &ZxrawApp, name: &str) -> Result<PathBuf, String> {
    if let Some(viewer) = app.viewer.as_ref() {
        let mut found = viewer
            .items()
            .iter()
            .filter(|path| filename_matches(path, name));
        let path = found
            .next()
            .ok_or_else(|| format!("no viewer item named `{name}`"))?;
        if found.next().is_some() {
            return Err(format!("more than one viewer item is named `{name}`"));
        }
        return Ok(path.clone());
    }
    pane_path(app, name)
}

fn read_persisted_annotation(app: &ZxrawApp, path: &Path) -> Result<forge_db::Annotation, String> {
    let db = forge_db::Db::open(app.paths.data.join("forge.db")).map_err(|e| e.to_string())?;
    let ids = db
        .file_ids(&[path.to_path_buf()])
        .map_err(|e| e.to_string())?;
    match ids.first() {
        Some((_, id)) => db.annotation(*id).map_err(|e| e.to_string()),
        None => Ok(forge_db::Annotation::default()),
    }
}

fn wait_annotation(
    app: &ZxrawApp,
    name: &str,
    st: &mut StepState,
    field: &str,
    matches: impl Fn(&forge_db::Annotation) -> bool,
) -> Result<Flow, String> {
    let path = annotation_path(app, name)?;
    let waited = elapsed(st);
    if waited < Duration::from_millis(100) {
        return Ok(Flow::Wait(
            "allowing the asynchronous library write to finish".into(),
        ));
    }
    match read_persisted_annotation(app, &path) {
        Ok(annotation) if matches(&annotation) => Ok(Flow::Done),
        Ok(annotation) if waited >= ANNOTATION_WAIT => Err(format!(
            "persisted {field} for `{name}` did not match; fresh DB value: {annotation:?}"
        )),
        Ok(_) => Ok(Flow::Wait(format!(
            "waiting for persisted {field} on `{name}`"
        ))),
        Err(error) if waited >= ANNOTATION_WAIT => Err(format!(
            "fresh DB query for `{name}` failed after {} s: {error}",
            ANNOTATION_WAIT.as_secs()
        )),
        Err(_) => Ok(Flow::Wait(format!(
            "waiting for a fresh DB query for `{name}`"
        ))),
    }
}

fn elapsed(st: &mut StepState) -> Duration {
    st.started.get_or_insert_with(Instant::now).elapsed()
}

fn one_arg<'a>(args: &'a str, error: &str) -> Result<&'a str, String> {
    let mut parts = args.split_whitespace();
    let value = parts.next().ok_or_else(|| error.to_string())?;
    if parts.next().is_some() {
        return Err("too many arguments".into());
    }
    Ok(value)
}

fn two_args<'a>(args: &'a str, error: &str) -> Result<(&'a str, &'a str), String> {
    let mut parts = args.split_whitespace();
    let first = parts.next().ok_or_else(|| error.to_string())?;
    let second = parts.next().ok_or_else(|| error.to_string())?;
    if parts.next().is_some() {
        return Err("too many arguments".into());
    }
    Ok((first, second))
}

fn no_args(args: &str) -> Result<(), String> {
    if args.trim().is_empty() {
        Ok(())
    } else {
        Err("this verb takes no arguments".into())
    }
}

fn timeout_arg(args: &str, default: Duration) -> Result<Duration, String> {
    if args.trim().is_empty() {
        return Ok(default);
    }
    let secs = one_arg(args, "expected timeout seconds")?
        .parse::<u64>()
        .map_err(|_| "timeout must be an integer number of seconds")?;
    if !(1..=WAIT_MAX).contains(&secs) {
        return Err(format!("timeout must be between 1 and {WAIT_MAX} seconds"));
    }
    Ok(Duration::from_secs(secs))
}

fn name_and_timeout(args: &str, assertion: bool) -> Result<(&str, Duration), String> {
    let mut parts = args.split_whitespace();
    let name = parts.next().ok_or("expected a filename")?;
    let timeout = match parts.next() {
        Some(secs) if !assertion => timeout_arg(secs, WAIT_LIMIT)?,
        Some(_) => return Err("assertion verbs do not take a timeout".into()),
        None => WAIT_LIMIT,
    };
    if parts.next().is_some() {
        return Err("too many arguments".into());
    }
    Ok((name, timeout))
}

#[cfg(test)]
mod tests {
    use super::{parse_label, parse_open_mode, parse_pick};
    use crate::viewer::ViewerMode;
    use forge_core::query::{ColorLabel, PickState};

    #[test]
    fn parses_viewer_modes_and_closed_state() {
        assert_eq!(parse_open_mode("single").unwrap(), Some(ViewerMode::Single));
        assert_eq!(parse_open_mode("cull").unwrap(), Some(ViewerMode::Cull));
        assert_eq!(
            parse_open_mode("compare").unwrap(),
            Some(ViewerMode::Compare)
        );
        assert_eq!(parse_open_mode("none").unwrap(), None);
        assert!(parse_open_mode("gallery").is_err());
    }

    #[test]
    fn parses_persisted_label_and_pick_values() {
        assert_eq!(parse_label("red").unwrap(), Some(ColorLabel::Red));
        assert_eq!(parse_label("purple").unwrap(), Some(ColorLabel::Purple));
        assert_eq!(parse_label("none").unwrap(), None);
        assert!(parse_label("orange").is_err());
        assert_eq!(parse_pick("pick").unwrap(), PickState::Pick);
        assert_eq!(parse_pick("reject").unwrap(), PickState::Reject);
        assert_eq!(parse_pick("none").unwrap(), PickState::None);
        assert!(parse_pick("maybe").is_err());
    }
}
