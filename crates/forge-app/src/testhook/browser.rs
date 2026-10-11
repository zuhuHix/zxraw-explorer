//! `ui browser <verb> [args]` script verbs.
//!
//! Document every verb here (grammar, what it waits for, how it fails). Paths given to
//! verbs that write or delete must be checked with `super::sandboxed`.
//!
//! - `toggle`: run the same per-pane grouping command as the UI.
//! - `wait [seconds]`: wait for capture-time and thumbnail analysis to finish (30 s default,
//!   300 s maximum).
//! - `assert-groups <count>`: assert the number of multi-image groups.
//! - `assert-members <anchor> | <name1>, <name2>`: assert a group's exact member names. Only
//!   files the listing shows count: hidden and filtered members are not part of a stack.
//! - `expand <anchor>` / `collapse <anchor>`: change a stack's display state.
//! - `assert-expanded <anchor>` / `assert-collapsed <anchor>`: verify that display state.
//! - `select-group <anchor>`: select every listed file represented by the group.

use super::{Flow, StepState};
use crate::app::ZxrawApp;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(super) fn step(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    verb: &str,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    match verb {
        "toggle" => {
            no_args(args, "toggle")?;
            crate::similar::toggle(app);
            Ok(Flow::Done)
        }
        "wait" => {
            let timeout = timeout(args)?;
            if !app.pane().similar.enabled() {
                return Err("grouping is off".into());
            }
            let started = *st.started.get_or_insert_with(Instant::now);
            if app.pane().similar.complete() {
                return Ok(Flow::Done);
            }
            if started.elapsed() >= timeout {
                let (phase, done, total) = app.pane().similar.progress();
                return Err(format!(
                    "grouping did not finish within {} s ({phase:?}: {done}/{total})",
                    timeout.as_secs()
                ));
            }
            let (phase, done, total) = app.pane().similar.progress();
            let _ = ctx;
            Ok(Flow::Wait(format!("{phase:?}: {done}/{total}")))
        }
        "assert-groups" => {
            let expected = args
                .parse::<usize>()
                .map_err(|_| "assert-groups takes a non-negative count".to_string())?;
            let grouping = &app.pane().similar;
            if !grouping.complete() {
                return Err("grouping is not complete; use `ui browser wait` first".into());
            }
            let found = grouping.groups().len();
            if expected == found {
                Ok(Flow::Done)
            } else {
                Err(format!("expected {expected} group(s), found {found}"))
            }
        }
        "assert-members" => {
            let (anchor, expected) = args
                .split_once(" | ")
                .ok_or("expected `<anchor> | <member1>, <member2>`")?;
            let expected: Vec<String> = expected
                .split(',')
                .map(|name| name.trim().to_lowercase())
                .filter(|name| !name.is_empty())
                .collect();
            if expected.is_empty() {
                return Err("expected at least one member name".into());
            }
            let (_, members) = group_named(app, anchor.trim())?;
            let mut found: Vec<String> = members
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().to_lowercase())
                .collect();
            found.sort();
            let mut expected = expected;
            expected.sort();
            if found == expected {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "group `{}` has members {found:?}, expected {expected:?}",
                    anchor.trim()
                ))
            }
        }
        "expand" | "collapse" => {
            let anchor = one_arg(args, verb)?;
            let (key, _) = group_named(app, anchor)?;
            let expanded = verb == "expand";
            if app.pane_mut().similar.set_expanded(key, expanded) {
                Ok(Flow::Done)
            } else {
                Err(format!("no group anchored by `{anchor}`"))
            }
        }
        "assert-expanded" | "assert-collapsed" => {
            let anchor = one_arg(args, verb)?;
            let (key, _) = group_named(app, anchor)?;
            let expected = verb == "assert-expanded";
            let found = app.pane().similar.is_expanded(&key);
            if found == expected {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "group `{anchor}` expanded state is {found}, expected {expected}"
                ))
            }
        }
        "select-group" => {
            let anchor = one_arg(args, verb)?;
            let (key, members) = group_named(app, anchor)?;
            let pane = app.pane_mut();
            pane.selected.clear();
            pane.selected.extend(members.iter().cloned());
            pane.cursor = pane
                .visible
                .iter()
                .position(|&index| pane.entries[index].path == key);
            pane.anchor = pane.cursor;
            pane.scroll_to_cursor = pane.cursor.is_some();
            Ok(Flow::Done)
        }
        other => Err(format!("unknown `ui browser` verb `{other}`")),
    }
}

fn no_args(args: &str, verb: &str) -> Result<(), String> {
    if args.trim().is_empty() {
        Ok(())
    } else {
        Err(format!("`{verb}` takes no arguments"))
    }
}

fn one_arg<'a>(args: &'a str, verb: &str) -> Result<&'a str, String> {
    let arg = args.trim();
    if arg.is_empty() {
        Err(format!("`{verb}` requires a group anchor filename"))
    } else {
        Ok(arg)
    }
}

fn timeout(args: &str) -> Result<Duration, String> {
    if args.trim().is_empty() {
        return Ok(Duration::from_secs(30));
    }
    let seconds = args
        .trim()
        .parse::<u64>()
        .map_err(|_| "wait takes an optional timeout in seconds".to_string())?;
    if seconds == 0 || seconds > 300 {
        return Err("wait timeout must be between 1 and 300 seconds".into());
    }
    Ok(Duration::from_secs(seconds))
}

/// The group anchored by, or containing, `name`, with its listed members.
fn group_named(app: &ZxrawApp, name: &str) -> Result<(PathBuf, Vec<PathBuf>), String> {
    let pane = app.pane();
    if !pane.similar.complete() && pane.similar.groups().is_empty() {
        return Err("grouping has not produced any groups yet".into());
    }
    pane.similar
        .groups()
        .iter()
        .map(|(key, members)| (key, pane.visible_group_paths(members)))
        .find(|(key, members)| {
            filename_matches(key, name)
                || members.iter().any(|member| filename_matches(member, name))
        })
        .map(|(key, members)| (key.clone(), members))
        .ok_or_else(|| format!("no group contains or is anchored by `{name}`"))
}

fn filename_matches(path: &std::path::Path, expected: &str) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(expected))
}
