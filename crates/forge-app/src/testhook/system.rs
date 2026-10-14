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
//!
//! Copy and move queue (the transfer queue; "waiting" means queued, not running):
//! - `pause-queue` / `resume-queue`: hold or release the queue. Waiting transfers stay queued
//!   while it is paused. Resume it before `wait-jobs`, which also waits for queued jobs.
//! - `assert-queued <n>`: exactly n transfers are waiting.
//! - `queue-up <k>` / `queue-down <k>`: move the k-th waiting transfer (1-based, queue order)
//!   one place earlier or later. Fails if out of range or if it cannot move.
//! - `queue-start-now <k>`: let the k-th waiting transfer start at once, past the queue.
//! - `queue-cancel <k>`: cancel the k-th waiting transfer; it never runs.
//! - `assert-queue-order <text>, <text>, …`: the waiting transfers, in queue order, number
//!   the same as the texts, and each job title contains its text. Titles read
//!   `Copying N item(s) to <folder>`, so match on the item count, e.g. `2 item(s), 1 item(s)`.
//!
//! Closing with transfers pending:
//! - `request-close`: arm the close guard for automation and ask the window to close.
//! - `assert-close-warning`: wait up to 5 s for the "Copies are not finished" warning.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::{Flow, StepState};
use crate::app::ZxrawApp;
use crate::ops_ui;

const WAIT_LIMIT: Duration = Duration::from_secs(30);
const CLOSE_WARNING_WAIT: Duration = Duration::from_secs(5);

pub(super) fn step(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    verb: &str,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    match verb {
        "pause-queue" => {
            app.ops.jobs.transfers().set_paused(true);
            Ok(Flow::Done)
        }
        "resume-queue" => {
            app.ops.jobs.transfers().set_paused(false);
            Ok(Flow::Done)
        }
        "assert-queued" => {
            let want = position(args, "a count")?;
            let have = app.ops.jobs.transfers().waiting().len();
            if have == want {
                Ok(Flow::Done)
            } else {
                Err(format!("{have} transfer(s) waiting, expected {want}"))
            }
        }
        "queue-up" | "queue-down" | "queue-start-now" | "queue-cancel" => {
            queue_action(app, verb, args)
        }
        "assert-queue-order" => {
            let expected: Vec<&str> = args.split(',').map(str::trim).collect();
            if expected.iter().any(|text| text.is_empty()) {
                return Err("expected a comma-separated list of title texts".into());
            }
            let waiting = app.ops.jobs.transfers().waiting();
            let jobs = app.ops.jobs.list();
            let titles: Vec<String> = waiting
                .iter()
                .map(|id| {
                    jobs.iter()
                        .find(|job| job.id == *id)
                        .map(|job| job.title.to_string())
                        .unwrap_or_default()
                })
                .collect();
            let matches = titles.len() == expected.len()
                && titles.iter().zip(&expected).all(|(t, e)| t.contains(e));
            if matches {
                Ok(Flow::Done)
            } else {
                Err(format!("queue is {titles:?}, expected {expected:?}"))
            }
        }
        "request-close" => {
            ops_ui::CLOSE_GUARD_IN_AUTOMATION.store(true, Ordering::SeqCst);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            Ok(Flow::Done)
        }
        "assert-close-warning" => {
            if ops_ui::close_warning_open(ctx) {
                Ok(Flow::Done)
            } else if elapsed(st) >= CLOSE_WARNING_WAIT {
                Err("the copies-not-finished warning did not open within 5 s".into())
            } else {
                Ok(Flow::Wait("waiting for the close warning".into()))
            }
        }
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

/// Acts on the k-th waiting transfer (1-based); `k` outside the queue is an error.
fn queue_action(app: &mut ZxrawApp, verb: &str, args: &str) -> Result<Flow, String> {
    let k = position(args, "a queue position")?;
    let waiting = app.ops.jobs.transfers().waiting();
    let Some(&id) = k.checked_sub(1).and_then(|at| waiting.get(at)) else {
        return Err(format!(
            "queue position {k} is out of range: {} transfer(s) waiting",
            waiting.len()
        ));
    };
    let queue = app.ops.jobs.transfers();
    let done = match verb {
        "queue-up" => queue.move_by(id, true),
        "queue-down" => queue.move_by(id, false),
        "queue-start-now" => queue.start_now(id),
        _ => {
            let handle = app.ops.jobs.list().into_iter().find(|job| job.id == id);
            match handle {
                Some(handle) => {
                    app.ops.jobs.cancel(&handle);
                    true
                }
                None => false,
            }
        }
    };
    if done {
        Ok(Flow::Done)
    } else {
        Err(format!("{verb} {k} had no effect (the transfer cannot move or is gone)"))
    }
}

/// A non-negative integer argument such as a count or a 1-based queue position.
fn position(args: &str, what: &str) -> Result<usize, String> {
    let text = args.trim();
    text.parse()
        .map_err(|_| format!("expected {what}, got `{text}`"))
}

fn elapsed(st: &mut StepState) -> Duration {
    st.started.get_or_insert_with(Instant::now).elapsed()
}
