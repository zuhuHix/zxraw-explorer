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
//! - `compare plan-two-way`: build the Two-way preview (copies missing items both ways and the
//!   newer file over the older; never deletes). The `assert-plan`, `assert-copy`, and
//!   `assert-skipped` verbs rebuild the Update preview, so check a Two-way preview before them.
//! - `compare execute-two-way`: run the previewed Two-way sync (fails unless the mode is
//!   Two-way).
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
//! Cloud placeholders:
//! - `simulate-online-only <path>`: mark the file as an online-only cloud file for this process,
//!   so find-and-replace and the backup check skip it the way they skip real placeholders.
//!
//! Find and replace (`… | …` separates paths; `replace set` takes the rest of the line as the
//! value):
//! - `replace open <path> [| <path>…]`: open the window on these files or folders.
//! - `replace set <key> <value>`: set one form field. Keys: `find`, `replace`, `regex`, `case`,
//!   `whole-word`, `recursive` (on/off), `include`, `exclude`, `max-mib`.
//! - `replace preview`: start the preview; `replace wait-preview [secs]` waits for it (default
//!   30 s).
//! - `replace assert-preview <key=value>…`: counts of the preview. Keys: `files`, `matches`,
//!   `skipped`.
//! - `replace assert-skipped <file name> <reason>`: the preview skips that file. Reasons:
//!   `binary-extension`, `unknown-type`, `binary`, `too-large`, `online-only`, `read-only`,
//!   `changed`, `path-changed`, `hard-linked`, `unreadable`.
//! - `replace untick <file name>`: leave a previewed file out of the replace.
//! - `replace run`: start the replace of the ticked files; `replace wait-apply [secs]` waits for
//!   it (default 30 s).
//! - `replace assert-apply <key=value>…`: counts of the replace. Keys: `files` (undo records),
//!   `replacements`, `skipped`, `failed`.
//! - `replace undo`: the window's "Undo replace" action; follow it with `wait-jobs`.
//!
//! Backup check (the Verify backup window, one folder per verb):
//! - `verify open <original>`: open the check for this folder.
//! - `verify set-backup <backup>`: choose the backup folder.
//! - `verify options full=on|off moves=on|off`: both settings are required. Full hashes every
//!   pair of equal size; moves finds renamed files by hash.
//! - `verify start`: run the check; `verify cancel` cancels a running check.
//! - `verify wait [secs]`: wait for the report (default 30 s); fails if the check ended with an
//!   error.
//! - `verify wait-cancelled [secs]`: wait for the check to end, and succeed only if it ended
//!   cancelled.
//! - `verify assert <key=value>…`: counts of the report. Keys: `match`, `missing`, `differs`,
//!   `extra`, `moved`, `not-checked`, `errors`.
//! - `verify assert-status <rel path> <status>`: one entry's status label (`match`, `missing`,
//!   `differs`, `extra`, `moved`, `not-checked`, `error`).
//! - `verify export <path> csv|text`: write the report to a new file; follow with `assert-contains`.
//!
//! Every verb fails with a message on bad arguments, and waiting verbs fail after their
//! timeout with the reason they were still waiting.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Flow, StepState};
use crate::app::ZxrawApp;
use crate::{extra_tools_ui, replace_ui, tools_ui};
use forge_core::backup_verify::BackupReport;
use forge_core::compare::{EntryState, SyncAction};
use forge_core::text_replace::{ApplyReport, ScanReport, SkipReason};

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
        "replace" => replace_step(app, args, st),
        "verify" => verify_step(app, args, st),
        "simulate-online-only" => {
            // Only inside the sandbox; the path is matched exactly by the engines.
            forge_core::cloud::simulate_online_only(inside(one_path(args)?)?);
            Ok(Flow::Done)
        }
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
            extra_tools_ui::automation_split(app, inside(file)?, bytes).map(|()| Flow::Done)
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
            if st
                .started
                .is_some_and(|started| started.elapsed() > BUTTON_WAIT)
            {
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
        "plan-two-way" => {
            no_args(rest)?;
            tools_ui::automation_compare_build_two_way(app).map(|_| Flow::Done)
        }
        "execute-two-way" => {
            no_args(rest)?;
            tools_ui::automation_compare_execute_two_way(app).map(|()| Flow::Done)
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

fn replace_step(app: &mut ZxrawApp, args: &str, st: &mut StepState) -> Result<Flow, String> {
    let (sub, rest) = split_verb(args);
    match sub {
        "open" => {
            let roots = rest
                .split(" | ")
                .map(|path| one_path(path).and_then(inside))
                .collect::<Result<Vec<_>, _>>()?;
            replace_ui::automation_open(app, roots);
            Ok(Flow::Done)
        }
        "set" => {
            let (key, value) = split_verb(rest);
            replace_ui::automation_set(app, key, value).map(|()| Flow::Done)
        }
        "preview" => {
            no_args(rest)?;
            replace_ui::automation_preview(app).map(|()| Flow::Done)
        }
        "wait-preview" => {
            let timeout = timeout_arg(rest)?;
            match replace_ui::automation_preview_result(app) {
                Some(Ok(_)) => Ok(Flow::Done),
                Some(Err(error)) => Err(format!("preview failed: {error}")),
                None => wait_or_fail(st, timeout, "the preview is still running"),
            }
        }
        "assert-preview" => {
            let counts = key_counts(rest)?;
            let report = finished_preview(app)?;
            for (key, expected) in counts {
                let found = match key {
                    "files" => report.files.len(),
                    "matches" => report.total_matches(),
                    "skipped" => report.skipped.len(),
                    _ => return Err(format!("unknown preview count `{key}`")),
                };
                if found != expected {
                    return Err(format!(
                        "expected {expected} `{key}` in the preview, found {found}"
                    ));
                }
            }
            Ok(Flow::Done)
        }
        "assert-skipped" => {
            let (name, reason) = split_verb(rest);
            if name.is_empty() || reason.is_empty() {
                return Err("expected `<file name> <reason>`".into());
            }
            let report = finished_preview(app)?;
            match report
                .skipped
                .iter()
                .find(|(path, _)| path.file_name().and_then(|n| n.to_str()) == Some(name))
            {
                Some((_, found)) if skip_key(found) == reason => Ok(Flow::Done),
                Some((_, found)) => Err(format!(
                    "{name} is skipped as `{}`, expected `{reason}`",
                    skip_key(found)
                )),
                None => Err(format!("the preview does not skip {name}")),
            }
        }
        "untick" => {
            let name = one_path(rest)?;
            replace_ui::automation_untick(app, name).map(|()| Flow::Done)
        }
        "run" => {
            no_args(rest)?;
            replace_ui::automation_replace(app).map(|()| Flow::Done)
        }
        "wait-apply" => {
            let timeout = timeout_arg(rest)?;
            match replace_ui::automation_apply_result(app) {
                Some(Ok(_)) => Ok(Flow::Done),
                Some(Err(error)) => Err(format!("replace failed: {error}")),
                None => wait_or_fail(st, timeout, "the replace is still running"),
            }
        }
        "assert-apply" => {
            let counts = key_counts(rest)?;
            let report = finished_apply(app)?;
            for (key, expected) in counts {
                let found = match key {
                    "files" => report.undo.len(),
                    "replacements" => report.replacements(),
                    "skipped" => report.skipped.len(),
                    "failed" => report.failed.len(),
                    _ => return Err(format!("unknown replace count `{key}`")),
                };
                if found != expected {
                    return Err(format!(
                        "expected {expected} `{key}` in the replace, found {found}"
                    ));
                }
            }
            Ok(Flow::Done)
        }
        "undo" => {
            no_args(rest)?;
            replace_ui::automation_undo(app).map(|()| Flow::Done)
        }
        _ => Err(format!("unknown `ui tools replace` verb `{sub}`")),
    }
}

/// The finished preview, or an error saying what to wait for.
fn finished_preview(app: &mut ZxrawApp) -> Result<ScanReport, String> {
    match replace_ui::automation_preview_result(app) {
        Some(Ok(report)) => Ok(report),
        Some(Err(error)) => Err(format!("preview failed: {error}")),
        None => Err("the preview has not completed (use `replace wait-preview`)".into()),
    }
}

/// The finished replace, or an error saying what to wait for.
fn finished_apply(app: &mut ZxrawApp) -> Result<ApplyReport, String> {
    match replace_ui::automation_apply_result(app) {
        Some(Ok(report)) => Ok(report),
        Some(Err(error)) => Err(format!("replace failed: {error}")),
        None => Err("the replace has not completed (use `replace wait-apply`)".into()),
    }
}

/// The script name of a skip reason, as used by `replace assert-skipped`.
fn skip_key(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::BinaryExtension => "binary-extension",
        SkipReason::UnknownType => "unknown-type",
        SkipReason::Binary => "binary",
        SkipReason::TooLarge(_) => "too-large",
        SkipReason::OnlineOnly => "online-only",
        SkipReason::ReadOnly => "read-only",
        SkipReason::Changed => "changed",
        SkipReason::PathChanged => "path-changed",
        SkipReason::HardLinked => "hard-linked",
        SkipReason::Unreadable(_) => "unreadable",
    }
}

fn verify_step(app: &mut ZxrawApp, args: &str, st: &mut StepState) -> Result<Flow, String> {
    let (sub, rest) = split_verb(args);
    match sub {
        "open" => {
            tools_ui::automation_open_verify(app, inside(one_path(rest)?)?);
            Ok(Flow::Done)
        }
        "set-backup" => tools_ui::automation_verify_set_backup(app, inside(one_path(rest)?)?)
            .map(|()| Flow::Done),
        "options" => {
            let mut full = None;
            let mut moves = None;
            for pair in rest.split_whitespace() {
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("expected `key=on|off`, found `{pair}`"))?;
                let on = match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err(format!("`{pair}`: expected on or off")),
                };
                match key {
                    "full" => full = Some(on),
                    "moves" => moves = Some(on),
                    _ => return Err(format!("unknown verify option `{key}`")),
                }
            }
            let (Some(full), Some(moves)) = (full, moves) else {
                return Err("expected both `full=on|off` and `moves=on|off`".into());
            };
            tools_ui::automation_verify_set_options(app, full, moves).map(|()| Flow::Done)
        }
        "start" => {
            no_args(rest)?;
            tools_ui::automation_verify_start(app).map(|()| Flow::Done)
        }
        "cancel" => {
            no_args(rest)?;
            tools_ui::automation_verify_cancel(app).map(|()| Flow::Done)
        }
        "wait" => {
            let timeout = timeout_arg(rest)?;
            match tools_ui::automation_verify_report(app) {
                Some(Ok(_)) => Ok(Flow::Done),
                Some(Err(error)) => Err(format!("backup check failed: {error}")),
                None => wait_or_fail(st, timeout, "the backup check is still running"),
            }
        }
        "wait-cancelled" => {
            let timeout = timeout_arg(rest)?;
            match tools_ui::automation_verify_report(app) {
                Some(Err(error)) if error.contains("cancel") => Ok(Flow::Done),
                Some(Err(error)) => Err(format!(
                    "the backup check ended with an error, not a cancel: {error}"
                )),
                Some(Ok(_)) => Err("the backup check finished without being cancelled".into()),
                None => wait_or_fail(st, timeout, "the backup check is still running"),
            }
        }
        "assert" => {
            let counts = key_counts(rest)?;
            let report = finished_verify(app)?;
            let totals = report.counts();
            for (key, expected) in counts {
                let found = match key {
                    "match" => totals.matched,
                    "missing" => totals.missing,
                    "differs" => totals.differs,
                    "extra" => totals.extra,
                    "moved" => totals.moved,
                    "not-checked" => totals.not_checked,
                    "errors" => totals.errors,
                    _ => return Err(format!("unknown verify count `{key}`")),
                };
                if found != expected {
                    return Err(format!(
                        "expected {expected} `{key}` in the backup check, found {found}"
                    ));
                }
            }
            Ok(Flow::Done)
        }
        "assert-status" => {
            let (rel, label) = split_verb(rest);
            if rel.is_empty() || label.is_empty() {
                return Err("expected `<rel path> <status>`".into());
            }
            let want = if label == "not-checked" {
                "not checked"
            } else {
                label
            };
            let report = finished_verify(app)?;
            let entry = report
                .entries
                .iter()
                .find(|entry| entry.rel == Path::new(rel))
                .ok_or_else(|| format!("the backup check has no entry for {rel}"))?;
            let found = entry.status.label();
            if found == want {
                Ok(Flow::Done)
            } else {
                Err(format!("{rel} is `{found}`, expected `{want}`"))
            }
        }
        "export" => {
            // `verify export <path> csv|text`; the path comes first so it may hold no spaces.
            let (path, format) = rest
                .rsplit_once(char::is_whitespace)
                .ok_or("expected `<path> csv|text`")?;
            let csv = match format.trim() {
                "csv" => true,
                "text" => false,
                other => return Err(format!("expected csv or text, found `{other}`")),
            };
            let path = entry_inside(one_path(path)?)?;
            tools_ui::automation_verify_export(app, path, csv).map(|()| Flow::Done)
        }
        _ => Err(format!("unknown `ui tools verify` verb `{sub}`")),
    }
}

/// The finished backup check report, or an error saying what to wait for.
fn finished_verify(app: &mut ZxrawApp) -> Result<BackupReport, String> {
    match tools_ui::automation_verify_report(app) {
        Some(Ok(report)) => Ok(report),
        Some(Err(error)) => Err(format!("backup check failed: {error}")),
        None => Err("the backup check has not completed (use `verify wait`)".into()),
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
        EntryState::NotCompared(_) => "not-compared",
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
