//! `ui ingest <verb> [args]` script verbs: drive the Media ingest window without native dialogs.
//!
//! Source and destination folders are given as paths, or picked through the `dialog` seam
//! (queue the answer with `dialog <path>` first). Arguments that hold two values are split on
//! ` | `. Every wait gives up after its own limit (default 30 s) and fails with the last state.
//!
//! - `open [<source> | <destination>]`: open the window; with paths, also set the source
//!   (card detection runs in the background) and the destination.
//! - `pick-source` / `pick-destination`: take the folder from the next `dialog` answer.
//! - `template <folder> | <file>`: set the folder and filename templates.
//! - `option <filter|verify|project|client|shoot> <value>`: set one option. `filter` takes
//!   `all`, `photos` or `videos`. `verify on` is accepted; `verify off` fails because ingest
//!   always verifies every copy.
//! - `wait-preview [secs]`: wait until the plan for the current settings is ready. Fails when
//!   the preview reports an error.
//! - `assert-plan <items> [<errors>]`: check the planned item count and, optionally, the
//!   number of planning errors (for example unreadable sources).
//! - `start [secs]`: wait until the import can start (plan ready, no errors, enough space),
//!   then start it and wait until it is running. Fails if a finished result is still shown
//!   (run `retry` first).
//! - `wait-done [secs]`: wait until the running import has produced its report. Fails if the
//!   ingest worker itself failed (not a per-file failure) or on timeout.
//! - `assert-counts <key>=<n> ...`: check result counts. Keys: `copied`, `verified`,
//!   `skipped`, `failed`, `missing`. Only the keys given are checked.
//! - `assert-safety safe|not-safe|incomplete`: check the safe-to-format state shown for the
//!   finished run. `incomplete` covers a missing or failed check.
//! - `assert-issue <file name>`: fail unless a failed, missing or unverified path in the
//!   finished run has that file name.
//! - `cancel [secs]`: cancel a running import (or preview) and wait until it stops.
//! - `retry`: clear the finished result and plan again, for a rerun after `fault off`.
//! - `fault source-gone <prefix> <bytes>`: after `bytes` have been read under `prefix`, every
//!   read fails with ERROR_DEVICE_NOT_CONNECTED, as if the card were pulled. The source root
//!   stays unavailable until `fault off`.
//! - `fault corrupt <prefix>`: flip one byte of the next temporary copy whose source is under
//!   `prefix`, before verification. The verification then fails and the destination is never
//!   placed.
//! - `fault off`: disarm all fault rules (the card is back).
//!
//! Faults are armed only from script steps, and script steps only run in automated runs
//! (`ZXRAW_SCRIPT`), so a normal session cannot arm them.

use std::path::Path;
use std::time::{Duration, Instant};

use forge_core::fault;

use super::{Flow, StepState};
use crate::app::ZxrawApp;
use crate::ingest_ui::{AutomationSafety, AutomationSummary};

const WAIT_LIMIT: Duration = Duration::from_secs(30);
const WAIT_MAX: u64 = 300;

pub(super) fn step(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    verb: &str,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    let state = &mut app.ops.ingest;
    match verb {
        "open" => {
            match split_pair(args) {
                None if args.is_empty() => state.automation_open(),
                Some((source, destination)) => {
                    let source = super::sandboxed(Path::new(source))?;
                    let destination = super::sandboxed(Path::new(destination))?;
                    state.automation_open_with(&source, &destination, ctx);
                }
                None => return Err("expected nothing, or `<source> | <destination>`".into()),
            }
            Ok(Flow::Done)
        }
        "pick-source" => {
            if state.automation_pick_source(ctx) {
                Ok(Flow::Done)
            } else {
                Err("no folder was queued for the source dialog (use `dialog <path>` first)".into())
            }
        }
        "pick-destination" => {
            if state.automation_pick_destination() {
                Ok(Flow::Done)
            } else {
                Err(
                    "no folder was queued for the destination dialog (use `dialog <path>` first)"
                        .into(),
                )
            }
        }
        "template" => {
            let (folder, file) = split_pair(args).ok_or("expected `<folder> | <file>`")?;
            state.automation_set_template(folder, file);
            Ok(Flow::Done)
        }
        "option" => {
            let (name, value) = args
                .split_once(char::is_whitespace)
                .ok_or("expected `<name> <value>`")?;
            state.automation_set_option(name, value.trim())?;
            Ok(Flow::Done)
        }
        "wait-preview" => {
            let summary = state.automation_summary();
            if let Some(error) = summary.preview_error {
                return Err(format!("the preview failed: {error}"));
            }
            if summary.preview_ready {
                return Ok(Flow::Done);
            }
            wait(st, args, "the preview is not ready yet")
        }
        "assert-plan" => {
            let summary = state.automation_summary();
            let (items, errors) = match args.split_once(char::is_whitespace) {
                Some((items, errors)) => (items, Some(errors.trim())),
                None => (args, None),
            };
            let items = parse_count(items, "an item count")?;
            if summary.plan_items != items {
                return Err(format!(
                    "expected {items} planned item(s), found {}",
                    summary.plan_items
                ));
            }
            if let Some(errors) = errors {
                let errors = parse_count(errors, "an error count")?;
                if summary.plan_errors != errors {
                    return Err(format!(
                        "expected {errors} planning error(s), found {}",
                        summary.plan_errors
                    ));
                }
            }
            Ok(Flow::Done)
        }
        "start" => {
            let summary = state.automation_summary();
            if summary.running {
                return Ok(Flow::Done);
            }
            if st.phase == 0 {
                if !summary.can_start {
                    return wait(st, args, "the import cannot start yet");
                }
                if summary.result_ready {
                    return Err("a finished ingest result is still shown; run `retry` first".into());
                }
                state.automation_start();
                st.phase = 1;
            }
            wait(st, args, "the import has not started yet")
        }
        "wait-done" => {
            let summary = state.automation_summary();
            if let Some(error) = summary.result_error {
                return Err(format!("the ingest worker failed: {error}"));
            }
            if summary.result_ready && !summary.running {
                return Ok(Flow::Done);
            }
            wait(st, args, "the ingest is still running")
        }
        "assert-counts" => {
            let summary = state.automation_summary();
            if !summary.result_ready {
                return Err(
                    "no ingest result is available (run `start` and `wait-done` first)".into(),
                );
            }
            for pair in args.split_whitespace() {
                let (key, expected) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("expected `<key>=<n>`, got `{pair}`"))?;
                let expected = parse_count(expected, "a count")?;
                let found = match key {
                    "copied" => summary.copied,
                    "verified" => summary.verified,
                    "skipped" => summary.skipped,
                    "failed" => summary.failed,
                    "missing" => summary.missing,
                    other => return Err(format!("unknown count `{other}`")),
                };
                if found != expected {
                    return Err(format!("expected {key}={expected}, found {found}"));
                }
            }
            Ok(Flow::Done)
        }
        "assert-safety" => {
            let summary = state.automation_summary();
            let expected = match args {
                "safe" => AutomationSafety::Safe,
                "not-safe" => AutomationSafety::NotSafe,
                "incomplete" => AutomationSafety::Incomplete,
                _ => return Err("expected `safe`, `not-safe` or `incomplete`".into()),
            };
            if !summary.result_ready {
                return Err("no ingest result is available".into());
            }
            if summary.safety == expected {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "expected safety {expected:?}, found {:?}",
                    summary.safety
                ))
            }
        }
        "assert-issue" => {
            let summary: AutomationSummary = state.automation_summary();
            if args.is_empty() {
                return Err("expected a file name".into());
            }
            let found = summary.issue_paths.iter().any(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy() == args)
            });
            if found {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "`{args}` is not reported as failed, missing or unverified: {:?}",
                    summary.issue_paths
                ))
            }
        }
        "cancel" => {
            if st.phase == 0 {
                state.automation_cancel();
                st.phase = 1;
            }
            if state.automation_summary().running {
                wait(st, args, "the ingest is still stopping")
            } else {
                Ok(Flow::Done)
            }
        }
        "retry" => {
            state.automation_retry()?;
            Ok(Flow::Done)
        }
        "fault" => fault_step(args),
        other => Err(format!("unknown `ui ingest` verb `{other}`")),
    }
}

/// Waits until `Flow::Done` is possible, honouring the optional `[secs]` argument.
fn wait(st: &mut StepState, args: &str, reason: &str) -> Result<Flow, String> {
    let limit = if args.is_empty() {
        WAIT_LIMIT
    } else {
        let secs = parse_count(args, "a timeout in seconds")?;
        if !(1..=WAIT_MAX).contains(&(secs as u64)) {
            return Err(format!("the timeout must be 1..={WAIT_MAX} seconds"));
        }
        Duration::from_secs(secs as u64)
    };
    let started = *st.started.get_or_insert_with(Instant::now);
    if started.elapsed() >= limit {
        return Err(format!("timed out after {} s: {reason}", limit.as_secs()));
    }
    Ok(Flow::Wait(reason.into()))
}

fn fault_step(args: &str) -> Result<Flow, String> {
    let (rule, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    match rule {
        "source-gone" => {
            let (prefix, bytes) = rest
                .rsplit_once(char::is_whitespace)
                .ok_or("expected `source-gone <prefix> <bytes>`")?;
            let bytes = parse_count(bytes.trim(), "a byte count")?;
            fault::arm_source_gone_after(&super::sandboxed(Path::new(prefix.trim()))?, bytes as u64)?;
        }
        "corrupt" => {
            if rest.is_empty() {
                return Err("expected `corrupt <prefix>`".into());
            }
            fault::arm_corrupt_before_verify(&super::sandboxed(Path::new(rest))?)?;
        }
        "off" => fault::disarm(),
        other => return Err(format!("unknown fault rule `{other}`")),
    }
    Ok(Flow::Done)
}

/// `<a> | <b>` with both parts non-empty.
fn split_pair(args: &str) -> Option<(&str, &str)> {
    match args.split_once(" | ") {
        Some((a, b)) if !a.trim().is_empty() && !b.trim().is_empty() => Some((a.trim(), b.trim())),
        _ => None,
    }
}

fn parse_count(text: &str, what: &str) -> Result<usize, String> {
    text.trim()
        .parse::<usize>()
        .map_err(|_| format!("expected {what}, got `{text}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_two_part_arguments() {
        assert_eq!(
            split_pair(r"C:\card | D:\out"),
            Some((r"C:\card", r"D:\out"))
        );
        assert_eq!(split_pair(r"C:\card |"), None);
        assert_eq!(split_pair("one part"), None);
    }

    #[test]
    fn counts_must_be_plain_numbers() {
        assert_eq!(parse_count(" 12 ", "n").unwrap(), 12);
        assert!(parse_count("-1", "n").unwrap_err().contains("expected n"));
    }
}
