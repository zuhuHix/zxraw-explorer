//! `ui tools <verb> [args]` script verbs.
//!
//! Duplicate finder (`… | …` separates two paths; `key=value` pairs are separated by spaces):
//! - `duplicates open <folder>`: open the finder on the folder and start its scan.
//! - `duplicates wait [secs]`: wait for the scan (default 30 s); fails if the scan reported an error.
//! - `duplicates assert-groups <n>`: fail unless the completed scan has n groups.
//! - `duplicates keep-first`: select every copy except the first of each group (fails if a
//!   group would lose all its copies).
//! - `duplicates recycle`: start moving the selected copies to the Recycle Bin. The removal
//!   runs as a job, so follow it with `wait-jobs`; the app re-verifies each copy first.
//!
//! Folder compare and Update sync:
//! - `compare open <left>` / `compare set-right <right>`: choose the folders.
//! - `compare start`: run the comparison; `compare wait [secs]` waits for it (default 30 s).
//! - `compare assert-entries <key=value>…`: counts of the comparison by state. Keys: `same`,
//!   `left-only`, `right-only`, `different`, `type-mismatch`, `error`.
//! - `compare plan-update`: build the Update preview from the comparison.
//! - `compare assert-plan <key=value>…`: counts of the Update preview. Keys: `copies`,
//!   `deletes`, `skipped`.
//! - `compare assert-copy <rel>` / `compare assert-no-copy <rel>`: the preview does or does
//!   not copy this relative path.
//! - `compare assert-skipped <rel>`: the preview skips this relative path with a reason.
//! - `compare execute-update`: run the previewed Update (fails unless the mode is Update).
//! - `compare wait-sync [secs]`: wait for the sync job's result (default 30 s).
//! - `compare assert-sync <key=value>…`: result counts. Keys: `done`, `failed`, `skipped`.
//!
//! - `wait-listing <n> [secs]`: wait until the focused pane lists exactly n entries.
//! - `wait-entry-size <name> <bytes> [secs]`: wait until the focused pane shows the entry with
//!   that size (the size shown in the details view).
//!
//! ZIP archives (the ZIP browser and "create ZIP" windows, driven like the buttons):
//! - `archive open <zip>` / `archive wait [secs]`: open the browser and wait for the listing.
//! - `archive assert-entry <name>` / `archive assert-count <n>`: check the listed entries.
//! - `archive extract <dest>`: "Extract all" into `dest`; follow it with `wait-jobs`.
//! - `archive create <dir> | <name.zip> | <source> [| <source>…]`: create the archive; follow
//!   it with `wait-jobs`. An existing archive name is never replaced (a number is added).
//!
//! Split and join (the split dialog and join command):
//! - `split <file> <bytes>`: split into parts of that size; follow with `wait-jobs`.
//! - `join <manifest or first .001 part>`: join the parts; follow with `wait-jobs`.
//!
//! Buttons, through real pointer input (the handler is never called directly):
//! - `click-button <label>`: press and release on a button the UI registered under `label`,
//!   such as `duplicates:Scan` or `archive:Extract all`. Waits up to 10 s for the button to
//!   be visible and enabled. Answer any native dialog with `dialog <path>` before the click.
//!
//! External changes, made with `std::fs` from the hook so the pane must notice them through
//! the OS watcher alone (paths are absolute; `|` separates two paths):
//! - `external create <path>`: create a 4 KiB file (`create_new`, never replaces).
//! - `external create-many <dir> <count>`: create `count` small files named `burst-NNNNN.txt`.
//! - `external rename <from> | <to>`: rename without replacing an existing target.
//! - `external delete <path>`: remove a file, or a folder with everything in it.
//! - `external append <path> | <text>`: append text, changing the file size.
//!
//! Every verb fails with a message on bad arguments, and waiting verbs fail after their
//! timeout with the reason they were still waiting.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Flow, StepState};
use crate::app::ZxrawApp;
use crate::{extra_tools_ui, tools_ui};
use forge_core::compare::{EntryState, SyncAction};

const WAIT_LIMIT: Duration = Duration::from_secs(30);
const WAIT_MAX: u64 = 300;
const BURST_LIMIT: usize = 5000;
/// How long `click-button` waits for its widget to be visible and enabled.
const BUTTON_WAIT: Duration = Duration::from_secs(10);

pub(super) fn step(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    verb: &str,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    match verb {
        "duplicates" => duplicates_step(app, args, st),
        "compare" => compare_step(app, args, st),
        "external" => external_step(args),
        "wait-listing" => wait_listing(app, args, st),
        "wait-entry-size" => wait_entry_size(app, args, st),
        "archive" => archive_step(app, args, st),
        "click-button" => click_button(app, ctx, args.trim(), st),
        "split" => {
            let (file, bytes) = args
                .rsplit_once(char::is_whitespace)
                .ok_or("expected `<file> <bytes>`")?;
            let bytes: u64 = bytes
                .trim()
                .parse()
                .map_err(|_| "part size must be a number of bytes")?;
            extra_tools_ui::automation_split(app, inside(file)?, bytes)
                .map(|()| Flow::Done)
        }
        "join" => {
            extra_tools_ui::automation_join(app, inside(one_path(args)?)?);
            Ok(Flow::Done)
        }
        _ => Err(format!("unknown `ui tools` verb `{verb}`")),
    }
}

/// Click a visible tool button by its registered egui widget id. This still goes through
/// egui's pointer input path; the handler is never called directly by the test hook.
fn click_button(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    label: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    if label.is_empty() {
        return Err("expected a registered tool button label".into());
    }
    if st.data.is_none() {
        // The button may appear a frame or two after the step that opens its window, so
        // wait for it, but only up to BUTTON_WAIT.
        let id = ctx.data(|data| {
            data.get_temp::<egui::Id>(egui::Id::new(("testhook-tools-button", label)))
        });
        let reason = match id.and_then(|id| ctx.read_response(id)) {
            Some(response) if response.enabled() && response.interact_rect.is_positive() => {
                st.data = Some(Box::new(response.interact_rect.center()));
                String::new()
            }
            Some(_) => format!("tool button {label:?} is disabled"),
            None => format!("tool button {label:?} is not visible"),
        };
        if st.data.is_none() {
            if st.started.is_some_and(|started| started.elapsed() > BUTTON_WAIT) {
                return Err(format!("{reason} after {} s", BUTTON_WAIT.as_secs()));
            }
            return Ok(Flow::Wait(reason));
        }
    }
    let pos = *st
        .data
        .as_ref()
        .and_then(|data| data.downcast_ref::<egui::Pos2>())
        .ok_or("invalid click-button state")?;
    app.testhook.input.pointer = Some(pos);
    let pressed = st.frames == 0;
    let events = if pressed {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    } else {
        vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]
    };
    match super::inject(app, st, &events) {
        Flow::Done if pressed => {
            st.frames = 1;
            st.phase = 0;
            Ok(Flow::Wait("button pressed".into()))
        }
        other => Ok(other),
    }
}

fn duplicates_step(app: &mut ZxrawApp, args: &str, st: &mut StepState) -> Result<Flow, String> {
    let (sub, rest) = split_verb(args);
    match sub {
        "open" => {
            tools_ui::automation_open_duplicates(app, inside(one_path(rest)?)?);
            Ok(Flow::Done)
        }
        "wait" => {
            let timeout = timeout_arg(rest)?;
            match tools_ui::automation_duplicate_groups(app) {
                Some(Ok(_)) => Ok(Flow::Done),
                Some(Err(error)) => Err(format!("duplicate scan failed: {error}")),
                None => wait_or_fail(st, timeout, "the duplicate scan is still running"),
            }
        }
        "assert-groups" => {
            let expected = count_arg(rest)?;
            match tools_ui::automation_duplicate_groups(app) {
                Some(Ok(groups)) if groups.len() == expected => Ok(Flow::Done),
                Some(Ok(groups)) => Err(format!(
                    "expected {expected} duplicate group(s), found {}",
                    groups.len()
                )),
                Some(Err(error)) => Err(format!("duplicate scan failed: {error}")),
                None => Err("the duplicate scan has not completed (use `duplicates wait`)".into()),
            }
        }
        "keep-first" => {
            no_args(rest)?;
            tools_ui::automation_duplicates_keep_first(app).map(|_| Flow::Done)
        }
        "recycle" => {
            no_args(rest)?;
            tools_ui::automation_duplicates_recycle(app).map(|()| Flow::Done)
        }
        _ => Err(format!("unknown `ui tools duplicates` verb `{sub}`")),
    }
}

fn compare_step(app: &mut ZxrawApp, args: &str, st: &mut StepState) -> Result<Flow, String> {
    let (sub, rest) = split_verb(args);
    match sub {
        "open" => {
            tools_ui::automation_open_compare(app, inside(one_path(rest)?)?);
            Ok(Flow::Done)
        }
        "set-right" => {
            let right = inside(one_path(rest)?)?;
            tools_ui::automation_compare_set_right(app, right).map(|()| Flow::Done)
        }
        "start" => {
            no_args(rest)?;
            tools_ui::automation_compare_start(app).map(|()| Flow::Done)
        }
        "wait" => {
            let timeout = timeout_arg(rest)?;
            match tools_ui::automation_compare_report(app) {
                Some(Ok(_)) => Ok(Flow::Done),
                Some(Err(error)) => Err(format!("folder comparison failed: {error}")),
                None => wait_or_fail(st, timeout, "the folder comparison is still running"),
            }
        }
        "assert-entries" => {
            let counts = key_counts(rest)?;
            let report = tools_ui::automation_compare_report(app)
                .ok_or("the comparison has not completed (use `compare wait`)")?
                .map_err(|error| format!("folder comparison failed: {error}"))?;
            for (key, expected) in counts {
                let found = report.count(|state| entry_key(state) == key);
                if found != expected {
                    return Err(format!(
                        "expected {expected} `{key}` entries, found {found}"
                    ));
                }
            }
            Ok(Flow::Done)
        }
        "plan-update" => {
            no_args(rest)?;
            tools_ui::automation_compare_build_update(app).map(|_| Flow::Done)
        }
        "assert-plan" => {
            let counts = key_counts(rest)?;
            let plan = tools_ui::automation_compare_build_update(app)?;
            for (key, expected) in counts {
                let found = match key {
                    "copies" => plan
                        .actions
                        .iter()
                        .filter(|a| matches!(a, SyncAction::Copy { .. }))
                        .count(),
                    "deletes" => plan.deletes(),
                    "skipped" => plan.skipped.len(),
                    _ => return Err(format!("unknown plan count `{key}`")),
                };
                if found != expected {
                    return Err(format!(
                        "expected {expected} `{key}` in the plan, found {found}"
                    ));
                }
            }
            Ok(Flow::Done)
        }
        "assert-copy" | "assert-no-copy" => {
            let rel = PathBuf::from(one_path(rest)?);
            let plan = tools_ui::automation_compare_build_update(app)?;
            let planned = plan
                .actions
                .iter()
                .any(|a| matches!(a, SyncAction::Copy { rel: r, .. } if *r == rel));
            match (sub == "assert-copy", planned) {
                (true, true) | (false, false) => Ok(Flow::Done),
                (true, false) => Err(format!("the plan does not copy {}", rel.display())),
                (false, true) => Err(format!("the plan copies {}", rel.display())),
            }
        }
        "assert-skipped" => {
            let rel = PathBuf::from(one_path(rest)?);
            let plan = tools_ui::automation_compare_build_update(app)?;
            if plan.skipped.iter().any(|issue| issue.path == rel) {
                Ok(Flow::Done)
            } else {
                Err(format!("the plan does not skip {}", rel.display()))
            }
        }
        "execute-update" => {
            no_args(rest)?;
            tools_ui::automation_compare_execute_update(app).map(|()| Flow::Done)
        }
        "wait-sync" => {
            let timeout = timeout_arg(rest)?;
            match tools_ui::automation_compare_sync_result(app) {
                Some(_) => Ok(Flow::Done),
                None => wait_or_fail(st, timeout, "the sync is still running"),
            }
        }
        "assert-sync" => {
            let counts = key_counts(rest)?;
            let report = tools_ui::automation_compare_sync_result(app)
                .ok_or("the sync has not completed (use `compare wait-sync`)")?;
            for (key, expected) in counts {
                let found = match key {
                    "done" => report.done.len(),
                    "failed" => report.failed.len(),
                    "skipped" => report.skipped.len(),
                    _ => return Err(format!("unknown sync count `{key}`")),
                };
                if found != expected {
                    return Err(format!(
                        "expected {expected} `{key}` in the sync, found {found}"
                    ));
                }
            }
            Ok(Flow::Done)
        }
        _ => Err(format!("unknown `ui tools compare` verb `{sub}`")),
    }
}

/// `wait-listing <n> [secs]`: waits until the focused pane lists exactly n entries.
fn wait_listing(app: &ZxrawApp, args: &str, st: &mut StepState) -> Result<Flow, String> {
    let mut parts = args.split_whitespace();
    let expected: usize = parts
        .next()
        .ok_or("expected an entry count")?
        .parse()
        .map_err(|_| "expected an entry count")?;
    let timeout = timeout_arg(parts.next().unwrap_or(""))?;
    let found = app.pane().entries.len();
    if found == expected {
        Ok(Flow::Done)
    } else {
        wait_or_fail(
            st,
            timeout,
            &format!("the pane lists {found} entries, expected {expected}"),
        )
    }
}

/// `wait-entry-size <name> <bytes> [secs]`: waits until the focused pane lists `name` with
/// that size. Fails at once if the name is not listed after the timeout.
fn wait_entry_size(app: &ZxrawApp, args: &str, st: &mut StepState) -> Result<Flow, String> {
    let mut parts = args.split_whitespace();
    let name = parts.next().ok_or("expected an entry name")?;
    let bytes: u64 = parts
        .next()
        .ok_or("expected a size in bytes")?
        .parse()
        .map_err(|_| "expected a size in bytes")?;
    let timeout = timeout_arg(parts.next().unwrap_or(""))?;
    match app.pane().entries.iter().find(|entry| entry.name == name) {
        Some(entry) if entry.size == bytes => Ok(Flow::Done),
        Some(entry) => wait_or_fail(
            st,
            timeout,
            &format!("`{name}` is {} bytes, expected {bytes}", entry.size),
        ),
        None => wait_or_fail(st, timeout, &format!("`{name}` is not listed yet")),
    }
}

fn archive_step(app: &mut ZxrawApp, args: &str, st: &mut StepState) -> Result<Flow, String> {
    let (sub, rest) = split_verb(args);
    match sub {
        "open" => {
            extra_tools_ui::automation_open_archive(app, inside(one_path(rest)?)?);
            Ok(Flow::Done)
        }
        "wait" => {
            let timeout = timeout_arg(rest)?;
            match extra_tools_ui::automation_archive_listing(app) {
                Some(Ok(_)) => Ok(Flow::Done),
                Some(Err(error)) => Err(format!("listing the archive failed: {error}")),
                None => wait_or_fail(st, timeout, "the archive is still being listed"),
            }
        }
        "assert-entry" => {
            let name = one_path(rest)?;
            if extra_tools_ui::automation_archive_names(app)
                .iter()
                .any(|n| n == name)
            {
                Ok(Flow::Done)
            } else {
                Err(format!("the archive has no entry `{name}`"))
            }
        }
        "assert-count" => {
            let expected = count_arg(rest)?;
            let found = extra_tools_ui::automation_archive_names(app).len();
            if found == expected {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "expected {expected} archive entries, found {found}"
                ))
            }
        }
        "extract" => {
            let destination = inside(one_path(rest)?)?;
            extra_tools_ui::automation_archive_extract(app, destination).map(|()| Flow::Done)
        }
        "create" => {
            // `archive create <dir> | <file name> | <source> [| <source>…]`
            let parts: Vec<&str> = rest.split(" | ").map(str::trim).collect();
            if parts.len() < 3 {
                return Err("expected `<dir> | <name> | <source>…`".into());
            }
            let sources = parts[2..]
                .iter()
                .map(|p| inside(p))
                .collect::<Result<Vec<_>, _>>()?;
            extra_tools_ui::automation_create_archive(
                app,
                sources,
                inside(parts[0])?,
                parts[1].to_string(),
            )
            .map(|()| Flow::Done)
        }
        _ => Err(format!("unknown `ui tools archive` verb `{sub}`")),
    }
}

fn entry_key(state: &EntryState) -> &'static str {
    match state {
        EntryState::Same => "same",
        EntryState::LeftOnly => "left-only",
        EntryState::RightOnly => "right-only",
        EntryState::Different(_) => "different",
        EntryState::TypeMismatch => "type-mismatch",
        EntryState::Error(_) => "error",
    }
}

/// Filesystem changes made outside the UI code path. Each returns at once; the pane has to
/// pick the change up from the OS watcher.
fn external_step(args: &str) -> Result<Flow, String> {
    let (sub, rest) = split_verb(args);
    match sub {
        "create" => {
            let path = entry_inside(one_path(rest)?)?;
            create_new(&path, &[b'x'; 4096])?;
        }
        "create-many" => {
            let (dir, count) = rest
                .rsplit_once(char::is_whitespace)
                .ok_or("expected a folder and a count")?;
            let count: usize = count.trim().parse().map_err(|_| "count must be a number")?;
            if count > BURST_LIMIT {
                return Err(format!("at most {BURST_LIMIT} files per burst"));
            }
            let dir = inside(dir)?;
            fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            for index in 0..count {
                let path = dir.join(format!("burst-{index:05}.txt"));
                create_new(&path, format!("burst {index}\n").as_bytes())?;
            }
        }
        "rename" => {
            let (from, to) = two_paths(rest)?;
            if to.symlink_metadata().is_ok() {
                return Err(format!("{} already exists", to.display()));
            }
            forge_core::paths::rename_no_replace(&from, &to).map_err(|e| e.to_string())?;
        }
        "delete" => {
            let path = entry_inside(one_path(rest)?)?;
            let md = path.symlink_metadata().map_err(|e| e.to_string())?;
            if md.is_dir() {
                fs::remove_dir_all(&path).map_err(|e| e.to_string())?;
            } else {
                fs::remove_file(&path).map_err(|e| e.to_string())?;
            }
        }
        "append" => {
            let (path, text) = rest.split_once(" | ").ok_or("expected `<path> | <text>`")?;
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(entry_inside(path)?)
                .map_err(|e| e.to_string())?;
            file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        }
        _ => return Err(format!("unknown `ui tools external` verb `{sub}`")),
    }
    Ok(Flow::Done)
}

fn create_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(bytes).map_err(|e| e.to_string())
}

/// Shared waiting: keeps the step running until the timeout, then fails with the reason.
fn wait_or_fail(st: &mut StepState, timeout: Duration, reason: &str) -> Result<Flow, String> {
    let waited = st
        .started
        .get_or_insert_with(std::time::Instant::now)
        .elapsed();
    if waited >= timeout {
        Err(format!("timed out after {} s: {reason}", timeout.as_secs()))
    } else {
        Ok(Flow::Wait(reason.into()))
    }
}

fn split_verb(args: &str) -> (&str, &str) {
    let args = args.trim();
    match args.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (args, ""),
    }
}

/// A path argument, confined to the script sandbox.
fn inside(path: &str) -> Result<PathBuf, String> {
    super::sandboxed(Path::new(path.trim()))
}

/// A path argument naming an entry the step itself creates, renames or deletes.
fn entry_inside(path: &str) -> Result<PathBuf, String> {
    super::sandboxed_entry(Path::new(path.trim()))
}

fn one_path(args: &str) -> Result<&str, String> {
    let path = args.trim();
    if path.is_empty() {
        Err("expected a path".into())
    } else {
        Ok(path)
    }
}

fn two_paths(args: &str) -> Result<(PathBuf, PathBuf), String> {
    let (first, second) = args.split_once(" | ").ok_or("expected `<path> | <path>`")?;
    Ok((entry_inside(first)?, entry_inside(second)?))
}

fn no_args(args: &str) -> Result<(), String> {
    if args.trim().is_empty() {
        Ok(())
    } else {
        Err("this verb takes no arguments".into())
    }
}

fn count_arg(args: &str) -> Result<usize, String> {
    args.trim()
        .parse()
        .map_err(|_| "expected a count".to_string())
}

fn timeout_arg(args: &str) -> Result<Duration, String> {
    let args = args.trim();
    if args.is_empty() {
        return Ok(WAIT_LIMIT);
    }
    let secs: u64 = args
        .parse()
        .map_err(|_| "timeout must be an integer number of seconds")?;
    if !(1..=WAIT_MAX).contains(&secs) {
        return Err(format!("timeout must be between 1 and {WAIT_MAX} seconds"));
    }
    Ok(Duration::from_secs(secs))
}

/// Parses `key=value` pairs, separated by spaces.
fn key_counts(args: &str) -> Result<Vec<(&str, usize)>, String> {
    let pairs: Vec<(&str, usize)> = args
        .split_whitespace()
        .map(|pair| {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| format!("expected `key=value`, found `{pair}`"))?;
            let value = value
                .parse()
                .map_err(|_| format!("`{pair}`: the value must be a number"))?;
            Ok((key, value))
        })
        .collect::<Result<_, String>>()?;
    if pairs.is_empty() {
        return Err("expected at least one `key=value` pair".into());
    }
    Ok(pairs)
}
