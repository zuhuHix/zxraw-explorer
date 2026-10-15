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
//! - `assert-visible <name1>, <name2>`: assert the focused pane shows exactly these names, in
//!   this order (after the filter and hidden-file rules).
//! - `assert-filter [text]`: assert the focused pane's filter text (empty when omitted).
//! - `assert-sticky on|off`: assert whether sticky selection is on in the focused pane.
//! - `click-arrow <name>`: real pointer click on a folder's disclosure arrow (details view).
//!   Waits until the arrow is on screen.
//! - `assert-no-dialog`: no dialog (prompt, confirmation, report) is open.
//! - `assert-prompt-error <text>`: a name prompt is open and its error contains `<text>`.
//! - `hold-name-lane <path> | <ms>`: keep the rename/new-item lane of `<path>` busy for
//!   `<ms>` milliseconds, as a rename stuck on an unresponsive share would.
//! - `click-control <name>`: real pointer click on a named control (`group-similar`: the grid's
//!   "Group bursts & similar" toggle). Waits until it is on screen.
//! - `assert-grouping on|off`: whether burst grouping is on in the focused pane.
//! - `assert-cursor <name>`: the focused pane's cursor is on this entry.
//! - `assert-depths <name>=<depth>, …`: assert the tree depth of listed rows.
//! - `assert-expanded-folder <name>` / `assert-collapsed-folder <name>`: whether a listed
//!   folder is open in place.
//! - `offline rename <path> | <name>`: accept a Rename prompt for `<path>`;
//!   `offline undo-rename <path> | <name>`: undo a recorded rename of `<path>` to `<name>`;
//!   `offline new-folder <name>` / `offline new-file <name>`: run New folder / New file in the
//!   focused pane and accept the prompt with `<name>`. Not sandboxed: the paths (and the
//!   focused pane's folder) must be on the unroutable TEST-NET-1 share `\\192.0.2.x`, so
//!   nothing is ever touched. Fails if a UI call took longer than 250 ms or the prompt stayed
//!   open; it does not wait for the background work (which hangs until the network gives up).

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
        "assert-visible" => {
            let expected: Vec<&str> = args
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .collect();
            let pane = app.pane();
            let shown: Vec<&str> = pane
                .visible
                .iter()
                .map(|&index| pane.entries[index].name.as_str())
                .collect();
            if shown == expected {
                Ok(Flow::Done)
            } else {
                Err(format!("the list shows {shown:?}, expected {expected:?}"))
            }
        }
        "assert-filter" => {
            let filter = &app.pane().filter_text;
            if filter == args.trim() {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "the filter is `{filter}`, expected `{}`",
                    args.trim()
                ))
            }
        }
        "assert-sticky" => {
            let expected = match args.trim() {
                "on" => true,
                "off" => false,
                _ => return Err("expected `on` or `off`".into()),
            };
            let sticky = app.pane().sticky;
            if sticky == expected {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "sticky selection is {}",
                    if sticky { "on" } else { "off" }
                ))
            }
        }
        "click-arrow" => {
            if st.data.is_none() {
                let name = args.trim();
                let Some(path) = entry_path(app, name) else {
                    return Err(format!("`{name}` is not listed"));
                };
                let Some(rect) = super::drawn_rect(ctx, super::arrow_rect_id(&path)) else {
                    return Ok(Flow::Wait(format!(
                        "the arrow of `{name}` is not on screen"
                    )));
                };
                st.data = Some(Box::new(rect.center()));
            }
            let pos = *st
                .data
                .as_ref()
                .unwrap()
                .downcast_ref::<egui::Pos2>()
                .unwrap();
            super::pointer_clicks(app, st, pos, 1, egui::PointerButton::Primary)
        }
        "click-control" => {
            if st.data.is_none() {
                let name = args.trim();
                let Some(rect) = super::drawn_rect(ctx, super::control_rect_id(name)) else {
                    return Ok(Flow::Wait(format!("the control `{name}` is not on screen")));
                };
                st.data = Some(Box::new(rect.center()));
            }
            let pos = *st
                .data
                .as_ref()
                .unwrap()
                .downcast_ref::<egui::Pos2>()
                .unwrap();
            super::pointer_clicks(app, st, pos, 1, egui::PointerButton::Primary)
        }
        "assert-grouping" => {
            let expected = match args.trim() {
                "on" => true,
                "off" => false,
                _ => return Err("expected `on` or `off`".into()),
            };
            let on = app.pane().similar.enabled();
            if on == expected {
                Ok(Flow::Done)
            } else {
                Err(format!("grouping is {}", if on { "on" } else { "off" }))
            }
        }
        "assert-cursor" => {
            let name = args.trim();
            match app.pane().cursor_entry() {
                Some(e) if e.name.eq_ignore_ascii_case(name) => Ok(Flow::Done),
                Some(e) => Err(format!("the cursor is on `{}`, expected `{name}`", e.name)),
                None => Err(format!("there is no cursor, expected `{name}`")),
            }
        }
        "assert-no-dialog" => {
            no_args(args, "assert-no-dialog")?;
            if app.ops.modal.is_none() {
                Ok(Flow::Done)
            } else {
                let what = match &app.ops.modal {
                    Some(crate::modal::Modal::Prompt(p)) => format!("a prompt with `{}`", p.text),
                    _ => "a dialog".to_string(),
                };
                Err(format!("{what} is open"))
            }
        }
        "hold-name-lane" => {
            let (path, ms) = args
                .rsplit_once('|')
                .ok_or("expected `hold-name-lane <path> | <ms>`")?;
            let ms: u64 = ms
                .trim()
                .parse()
                .map_err(|_| format!("expected milliseconds, got `{}`", ms.trim()))?;
            crate::actions::hold_name_lane(
                app,
                std::path::Path::new(path.trim()),
                Duration::from_millis(ms),
            );
            Ok(Flow::Done)
        }
        "assert-prompt-error" => {
            let expected = args.trim();
            match &app.ops.modal {
                Some(crate::modal::Modal::Prompt(p))
                    if p.error.as_deref().is_some_and(|e| e.contains(expected)) =>
                {
                    Ok(Flow::Done)
                }
                Some(crate::modal::Modal::Prompt(p)) => {
                    Err(format!("the prompt's error is {:?}", p.error))
                }
                _ => Err("no prompt is open".into()),
            }
        }
        "assert-depths" => {
            let pane = app.pane();
            for pair in args.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                let Some((name, depth)) = pair.split_once('=') else {
                    return Err(format!("expected `<name>=<depth>`, got `{pair}`"));
                };
                let (name, depth) = (name.trim(), depth.trim());
                let depth: usize = depth
                    .parse()
                    .map_err(|_| format!("`{depth}` is not a depth"))?;
                let Some(vis) = pane
                    .visible
                    .iter()
                    .position(|&i| pane.entries[i].name.eq_ignore_ascii_case(name))
                else {
                    return Err(format!("`{name}` is not shown"));
                };
                if pane.depth_at(vis) != depth {
                    return Err(format!(
                        "`{name}` is at depth {}, expected {depth}",
                        pane.depth_at(vis)
                    ));
                }
            }
            Ok(Flow::Done)
        }
        "assert-expanded-folder" | "assert-collapsed-folder" => {
            let name = args.trim();
            let Some(path) = entry_path(app, name) else {
                return Err(format!("`{name}` is not listed"));
            };
            let expected = verb == "assert-expanded-folder";
            let open = app.pane().is_expanded(&path);
            if open == expected {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "folder `{name}` open in place is {open}, expected {expected}"
                ))
            }
        }
        "offline" => offline(app, ctx, args),
        other => Err(format!("unknown `ui browser` verb `{other}`")),
    }
}

/// A path on the unroutable TEST-NET-1 range (RFC 5737): no file call on it ever succeeds.
fn on_test_net(path: &std::path::Path) -> bool {
    forge_core::netpath::is_network_path(path)
        && path
            .to_string_lossy()
            .replace('/', "\\")
            .starts_with(r"\\192.0.2.")
}

/// `ui browser offline …`: the UI side of a rename / undo / create on an offline share
/// must return at once and leave the disk work to a worker.
fn offline(app: &mut ZxrawApp, ctx: &egui::Context, args: &str) -> Result<Flow, String> {
    use crate::actions::UndoEntry;
    use crate::commands::Cmd;
    use crate::modal::{Modal, Prompt, PromptKind};
    let (action, rest) = args
        .trim()
        .split_once(char::is_whitespace)
        .ok_or("expected `offline <action> …`")?;
    let rest = rest.trim();
    let started = Instant::now();
    match action {
        "rename" | "undo-rename" => {
            let (path, name) = rest.split_once('|').ok_or("expected `<path> | <name>`")?;
            let (path, name) = (PathBuf::from(path.trim()), name.trim().to_string());
            if !on_test_net(&path) {
                return Err("the path must be on the TEST-NET-1 share \\\\192.0.2.x".into());
            }
            if action == "rename" {
                let prompt = Prompt::new(PromptKind::Rename { path }, "Rename", name);
                if let Some(m) = crate::actions::accept(app, Modal::Prompt(prompt)) {
                    app.ops.modal = Some(m);
                    return Err("the rename prompt stayed open".into());
                }
            } else {
                let renamed = path.with_file_name(&name);
                app.ops.undo.push(UndoEntry::Rename(vec![(renamed, path)]));
                app.run(Cmd::Undo, ctx);
            }
        }
        "new-folder" | "new-file" => {
            if !on_test_net(&app.pane().path) {
                return Err(
                    "the focused pane must show a TEST-NET-1 share (`ui sidebar go`)".into(),
                );
            }
            let cmd = if action == "new-folder" {
                Cmd::NewFolder
            } else {
                Cmd::NewFile
            };
            app.run(cmd, ctx);
            let Some(Modal::Prompt(mut prompt)) = app.ops.modal.take() else {
                return Err(format!("{cmd:?} opened no prompt"));
            };
            prompt.text = rest.to_string();
            if let Some(m) = crate::actions::accept(app, Modal::Prompt(prompt)) {
                app.ops.modal = Some(m);
                return Err("the prompt stayed open".into());
            }
        }
        other => return Err(format!("unknown offline action `{other}`")),
    }
    let took = started.elapsed();
    if took > Duration::from_millis(250) {
        Err(format!(
            "`{action}` blocked the UI for {} ms",
            took.as_millis()
        ))
    } else {
        Ok(Flow::Done)
    }
}

fn entry_path(app: &ZxrawApp, name: &str) -> Option<PathBuf> {
    app.pane()
        .entries
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(name))
        .map(|e| e.path.clone())
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
