//! `ui related <verb> [args]` script verbs.
//!
//! - `proxies-locate`: open the folder picker (answer with the script `dialog` step).
//! - `assert-proxies <name>[, <name>...]`: wait for local or located proxy matches.
//! - `assert-no-proxies <name>[, <name>...]`: wait for the proxy scan, then require that none of the names match.
//! - `assert-originals <name>[, <name>...]` and `assert-exports ...`: wait for filename-rule matches.
//! - `select-originals`, `select-exports`: start the pane selection commands. They match on a worker thread.
//! - `wait-selection`: wait until the last selection command has been applied.
//! - `assert-selection <name>[, <name>...]`: wait for a pending selection, then check the selected pane entries.
//! - `settings-edit-rule <1-based row> <rule>`: edit through the Settings UI validator.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::{Flow, StepState};
use crate::app::ZxrawApp;

const ASSERT_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn step(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    verb: &str,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    match verb {
        "proxies-locate" => {
            crate::video_ui::locate_proxies(app);
            Ok(Flow::Done)
        }
        "assert-proxies" => assert_proxies(app, ctx, args, st, true),
        "assert-no-proxies" => assert_proxies(app, ctx, args, st, false),
        "assert-originals" => assert_associations(app, ctx, args, st, false),
        "assert-exports" => assert_associations(app, ctx, args, st, true),
        "select-originals" => {
            crate::export_assoc::select_originals(app);
            Ok(Flow::Done)
        }
        "select-exports" => {
            crate::export_assoc::select_exports(app);
            Ok(Flow::Done)
        }
        "wait-selection" => wait_selection(st),
        "assert-selection" => assert_selection(app, args, st),
        "settings-edit-rule" => edit_rule(app, args),
        _ => Err(format!("unknown `ui related` verb `{verb}`")),
    }
}

fn names(args: &str) -> Result<HashSet<String>, String> {
    let names = args
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_lowercase)
        .collect::<HashSet<_>>();
    if names.is_empty() {
        return Err("expected one or more comma-separated filenames".into());
    }
    Ok(names)
}

fn names_in_paths(paths: impl IntoIterator<Item = std::path::PathBuf>) -> HashSet<String> {
    paths
        .into_iter()
        .filter_map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_lowercase())
        })
        .collect()
}

fn wait_timed_out(st: &mut StepState, what: &str) -> Result<bool, String> {
    let started = st.started.get_or_insert_with(Instant::now);
    if started.elapsed() >= ASSERT_TIMEOUT {
        return Err(format!(
            "timed out after {} s waiting for {what}",
            ASSERT_TIMEOUT.as_secs()
        ));
    }
    Ok(false)
}

fn assert_proxies(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    args: &str,
    st: &mut StepState,
    present: bool,
) -> Result<Flow, String> {
    let expected = names(args)?;
    if app.pane().is_loading() {
        wait_timed_out(st, "the pane listing before checking proxies")?;
        return Ok(Flow::Wait("the pane listing is still loading".into()));
    }
    let Some(entry) = app.pane().cursor_entry().cloned() else {
        return Err("select a video clip before asserting proxies".into());
    };
    let ffmpeg_dir = app.settings.ffmpeg_dir.clone();
    app.ops
        .video_ui
        .ensure_related_scan(&entry, ffmpeg_dir.as_deref(), ctx);
    let ui = &app.ops.video_ui;
    if ui.relation_scan_pending() || ui.proxy_scan_pending() {
        wait_timed_out(st, "proxy discovery")?;
        return Ok(Flow::Wait("proxy discovery is still running".into()));
    }
    let actual = names_in_paths(
        ui.related_results()
            .iter()
            .chain(ui.located_proxy_results())
            .map(|related| related.path.clone()),
    );
    if present {
        if !expected.is_subset(&actual) {
            return Err(format!(
                "expected proxies {expected:?}; found {actual:?}; scan error: {}",
                ui.proxy_scan_error().unwrap_or("none")
            ));
        }
        let drawn = ui
            .related_results()
            .iter()
            .chain(ui.located_proxy_results())
            .filter(|related| expected.contains(&path_name(&related.path)))
            .filter(|related| related_row_was_drawn(ctx, &related.path))
            .map(|related| path_name(&related.path))
            .collect::<HashSet<_>>();
        if !expected.is_subset(&drawn) {
            wait_timed_out(st, "expected proxy rows to be drawn in the preview")?;
            return Ok(Flow::Wait(
                "expected proxy rows are not visible in the preview yet".into(),
            ));
        }
    }
    if !present && !expected.is_disjoint(&actual) {
        return Err(format!(
            "expected no proxies {expected:?}; found {actual:?}"
        ));
    }
    Ok(Flow::Done)
}

fn path_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn related_row_was_drawn(ctx: &egui::Context, path: &std::path::Path) -> bool {
    let id = egui::Id::new(("testhook-entry-rect", related_row_test_key(path)));
    ctx.data(|data| data.get_temp::<(u64, egui::Rect)>(id))
        .is_some_and(|(frame, rect)| {
            rect.is_positive() && ctx.cumulative_frame_nr().saturating_sub(frame) <= 2
        })
}

fn related_row_test_key(path: &std::path::Path) -> std::path::PathBuf {
    let mut key = path.as_os_str().to_os_string();
    key.push("::related-row");
    std::path::PathBuf::from(key)
}

fn assert_associations(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    args: &str,
    st: &mut StepState,
    exports: bool,
) -> Result<Flow, String> {
    let expected = names(args)?;
    let Some(entry) = app.pane().cursor_entry() else {
        return Err("select a media file before asserting related exports".into());
    };
    let path = entry.path.clone();
    let rules = crate::export_assoc::active_rules(&app.settings.export_rules);
    crate::export_assoc::request_for(&path, &rules, ctx);
    match crate::export_assoc::cached_for(&path, &rules) {
        Some(crate::export_assoc::QueryState::Pending) | None => {
            wait_timed_out(st, "export associations")?;
            Ok(Flow::Wait(
                "export association scan is still running".into(),
            ))
        }
        Some(crate::export_assoc::QueryState::Failed(error)) => {
            Err(format!("export association scan failed: {error}"))
        }
        Some(crate::export_assoc::QueryState::Complete(associations)) => {
            let paths = if exports {
                associations.exports
            } else {
                associations.originals
            };
            let actual = names_in_paths(paths);
            if !expected.is_subset(&actual) {
                let kind = if exports { "exports" } else { "originals" };
                return Err(format!("expected {kind} {expected:?}; found {actual:?}"));
            }
            Ok(Flow::Done)
        }
    }
}

/// The select commands apply their result a frame or more after they start, so waiting here keeps
/// scripts from reading the selection too early.
fn wait_selection(st: &mut StepState) -> Result<Flow, String> {
    if crate::export_assoc::selection_pending() {
        wait_timed_out(st, "the pane selection")?;
        return Ok(Flow::Wait("pane selection is still being matched".into()));
    }
    Ok(Flow::Done)
}

fn assert_selection(app: &ZxrawApp, args: &str, st: &mut StepState) -> Result<Flow, String> {
    let expected = names(args)?;
    if let Flow::Wait(reason) = wait_selection(st)? {
        return Ok(Flow::Wait(reason));
    }
    let actual = names_in_paths(app.pane().selected.iter().cloned());
    if actual != expected {
        return Err(format!(
            "expected pane selection {expected:?}; found {actual:?}"
        ));
    }
    Ok(Flow::Done)
}

fn edit_rule(app: &mut ZxrawApp, args: &str) -> Result<Flow, String> {
    let (row, rule) = args
        .trim()
        .split_once(char::is_whitespace)
        .ok_or_else(|| "usage: settings-edit-rule <1-based row> <rule>".to_string())?;
    let row = row
        .parse::<usize>()
        .map_err(|_| "rule row must be a positive integer".to_string())?;
    let index = row
        .checked_sub(1)
        .ok_or_else(|| "rule rows start at 1".to_string())?;
    crate::settings_ui::open_media_links(app);
    crate::settings_ui::apply_export_rule(&mut app.settings, index, rule.trim().to_string())
        .map_err(|error| format!("cannot edit export rule: {error}"))?;
    Ok(Flow::Done)
}
