//! `ui system <verb> [args]` verbs for platform integration scenarios.
//!
//! - `wait-thumbnail <filename> [| seconds]`: wait until a requested grid thumbnail is
//!   cached or fails (default 30 s; reports a failed decode as an error).
//! - `assert-thumbnail-ready <filename>`: require a generated grid thumbnail in the media
//!   cache.
//! - `assert-thumbnail-error <filename>`: require the grid's decode-error placeholder.
//! - `wait-update <version>`: wait until the "update available" notice offers `version`
//!   (the check runs a few seconds after startup; needs `ZXRAW_UPDATE_API`).
//! - `update-now`: press the notice's "Update now".
//! - `assert-no-update`: require that no update notice is showing.

use std::path::Path;
use std::time::{Duration, Instant};

use super::{Flow, StepState};
use crate::app::ZxrawApp;

const WAIT_LIMIT: Duration = Duration::from_secs(30);

pub(super) fn step(
    app: &mut ZxrawApp,
    _ctx: &egui::Context,
    verb: &str,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    match verb {
        "wait-thumbnail" => {
            let (name, timeout) = filename_and_timeout(args)?;
            match thumbnail_state(app, name)? {
                ThumbnailState::Ready | ThumbnailState::Failed => Ok(Flow::Done),
                ThumbnailState::Pending if elapsed(st) >= timeout => Err(format!(
                    "thumbnail for `{name}` did not finish within {} s",
                    timeout.as_secs()
                )),
                ThumbnailState::Pending => {
                    Ok(Flow::Wait(format!("waiting for thumbnail `{name}`")))
                }
            }
        }
        "assert-thumbnail-ready" => {
            let name = filename(args)?;
            match thumbnail_state(app, name)? {
                ThumbnailState::Ready => Ok(Flow::Done),
                ThumbnailState::Failed => Err(format!("thumbnail for `{name}` failed to decode")),
                ThumbnailState::Pending => Err(format!("thumbnail for `{name}` is not cached yet")),
            }
        }
        "assert-thumbnail-error" => {
            let name = filename(args)?;
            let path = entry_path(app, name)?;
            if app.ops.thumbs.failed_path(path) {
                Ok(Flow::Done)
            } else {
                Err(format!("thumbnail for `{name}` has no error placeholder"))
            }
        }
        "wait-update" => {
            let version = filename(args)?;
            match &app.notices.update {
                Some(update) if update.version == version => Ok(Flow::Done),
                Some(update) => Err(format!(
                    "the update notice offers {}, not {version}",
                    update.version
                )),
                None if elapsed(st) >= WAIT_LIMIT => {
                    Err(format!("no update notice for {version} within 30 s"))
                }
                None => Ok(Flow::Wait("waiting for the update notice".into())),
            }
        }
        "update-now" => {
            let update = app
                .notices
                .update
                .clone()
                .ok_or("no update notice is showing")?;
            crate::updater::update_now(app, &update);
            Ok(Flow::Done)
        }
        "assert-no-update" => match &app.notices.update {
            None => Ok(Flow::Done),
            Some(update) => Err(format!("an update notice offers {}", update.version)),
        },
        _ => Err(format!("unknown `ui system` verb `{verb}`")),
    }
}

#[derive(Clone, Copy)]
enum ThumbnailState {
    Pending,
    Ready,
    Failed,
}

fn thumbnail_state(app: &ZxrawApp, name: &str) -> Result<ThumbnailState, String> {
    let entry = app
        .pane()
        .entries
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("no file named `{name}` in the current folder"))?;
    if app.ops.thumbs.failed_path(&entry.path) {
        return Ok(ThumbnailState::Failed);
    }
    let service = app.ops.thumbs.service();
    let ready = [128, 256, 384, 512].into_iter().any(|size| {
        service
            .get_cached(&entry.path, size, entry.modified, entry.size)
            .is_some()
    });
    Ok(if ready {
        ThumbnailState::Ready
    } else {
        ThumbnailState::Pending
    })
}

fn entry_path<'a>(app: &'a ZxrawApp, name: &str) -> Result<&'a Path, String> {
    app.pane()
        .entries
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(name))
        .map(|entry| entry.path.as_path())
        .ok_or_else(|| format!("no file named `{name}` in the current folder"))
}

fn filename(args: &str) -> Result<&str, String> {
    let name = args.trim();
    if name.is_empty() {
        Err("expected a filename".into())
    } else {
        Ok(name)
    }
}

fn filename_and_timeout(args: &str) -> Result<(&str, Duration), String> {
    let (name, timeout) = match args.split_once('|') {
        Some((name, seconds)) => {
            let seconds = seconds
                .trim()
                .parse::<u64>()
                .map_err(|_| "thumbnail timeout must be integer seconds")?;
            if !(1..=120).contains(&seconds) {
                return Err("thumbnail timeout must be between 1 and 120 seconds".into());
            }
            (name.trim(), Duration::from_secs(seconds))
        }
        None => (args.trim(), WAIT_LIMIT),
    };
    if name.is_empty() {
        Err("expected a filename".into())
    } else {
        Ok((name, timeout))
    }
}

fn elapsed(st: &mut StepState) -> Duration {
    st.started.get_or_insert_with(Instant::now).elapsed()
}
