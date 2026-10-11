//! `ui media <verb> [args]` script verbs.
//!
//! - `gps assert none|invalid|<lat>,<lon>` waits for the selected image metadata and checks
//!   the GPS panel state.
//! - `open-maps <url>` computes the selected image's map URL and records a dry-run toast;
//!   it never opens a browser.
//! - `export start <csv|json> <selection|folder> [recursive]` starts an export after taking
//!   the next scripted `dialog <path>` answer.
//! - `export wait [seconds]` waits for the export job to finish (default 30 seconds).
//! - `export assert success|failed|cancelled|path <path>` checks its result.

use std::path::Path;
use std::time::{Duration, Instant};

use forge_jobs::JobState;

use super::{Flow, StepState};
use crate::app::ZxrawApp;
use crate::gps_ui::{GpsState, map_url};
use crate::media_meta::MediaMeta;

const WAIT_LIMIT: Duration = Duration::from_secs(30);

pub(super) fn step(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    verb: &str,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    match verb {
        "gps" => gps_step(app, ctx, args, st),
        "open-maps" => open_maps_step(app, ctx, args, st),
        "export" => export_step(app, args, st),
        _ => Err(format!("unknown `ui media` verb `{verb}`")),
    }
}

fn gps_step(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    let expected = args
        .strip_prefix("assert ")
        .ok_or("expected `gps assert none|invalid|<lat>,<lon>`")?
        .trim();
    let entry = app
        .pane()
        .cursor_entry()
        .cloned()
        .ok_or("select a file before asserting GPS state")?;
    let Some(result) = app.ops.meta.get(&entry, ctx) else {
        return wait_for_metadata(st, "selected image metadata");
    };
    let metadata = match result {
        Ok(MediaMeta::Image(metadata)) => metadata,
        Ok(MediaMeta::Video(_)) => return assert_gps(expected, GpsState::None),
        Err(error) => return Err(format!("could not read selected media metadata: {error}")),
    };
    assert_gps(expected, crate::gps_ui::state(&metadata))
}

fn assert_gps(expected: &str, found: GpsState) -> Result<Flow, String> {
    match (expected, found) {
        ("none", GpsState::None) | ("invalid", GpsState::Invalid) => Ok(Flow::Done),
        (
            expected,
            GpsState::Coordinates {
                latitude,
                longitude,
                ..
            },
        ) => {
            let (lat, lon) = parse_pair(expected)?;
            if (latitude - lat).abs() <= 0.000001 && (longitude - lon).abs() <= 0.000001 {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "expected GPS {lat},{lon}, found {latitude},{longitude}"
                ))
            }
        }
        (expected, found) => Err(format!("expected GPS state `{expected}`, found {found:?}")),
    }
}

fn open_maps_step(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    expected_url: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    let expected_url = expected_url.trim();
    if expected_url.is_empty() {
        return Err("expected the URL that would be opened".into());
    }
    let entry = app
        .pane()
        .cursor_entry()
        .cloned()
        .ok_or("select a file before asking for its map URL")?;
    let Some(result) = app.ops.meta.get(&entry, ctx) else {
        return wait_for_metadata(st, "selected image metadata");
    };
    let metadata = match result {
        Ok(MediaMeta::Image(metadata)) => metadata,
        _ => return Err("selected item has no valid GPS coordinates".into()),
    };
    let GpsState::Coordinates {
        latitude,
        longitude,
        ..
    } = crate::gps_ui::state(&metadata)
    else {
        return Err("selected item has no valid GPS coordinates".into());
    };
    let url = map_url(&app.settings.map_url_template, latitude, longitude)?;
    if url != expected_url {
        return Err(format!("expected map URL `{expected_url}`, got `{url}`"));
    }
    // The script receives the computed value as a toast it can assert. This path deliberately
    // does not invoke ShellExecute or any other browser-launch mechanism.
    app.toast(format!("Would open in maps: {url}"));
    Ok(Flow::Done)
}

fn wait_for_metadata(st: &mut StepState, what: &str) -> Result<Flow, String> {
    let started = *st.started.get_or_insert_with(Instant::now);
    if started.elapsed() >= WAIT_LIMIT {
        Err(format!("timed out waiting for {what}"))
    } else {
        Ok(Flow::Wait(format!("waiting for {what}")))
    }
}

fn export_step(app: &mut ZxrawApp, args: &str, st: &mut StepState) -> Result<Flow, String> {
    let mut words = args.split_whitespace();
    match words.next() {
        Some("start") => {
            if app.ops.library.db.is_none() {
                if let Some(error) = &app.ops.library.open_error {
                    return Err(format!("library database failed to open: {error}"));
                }
                return wait_for_metadata(st, "library database");
            }
            let format = words
                .next()
                .ok_or("expected export format `csv` or `json`")?;
            let scope = words
                .next()
                .ok_or("expected export scope `selection` or `folder`")?;
            let recursive = match words.next() {
                None => false,
                Some("recursive") if scope == "folder" => true,
                Some("flat") if scope == "folder" => false,
                Some(value) => return Err(format!("unknown export option `{value}`")),
            };
            if words.next().is_some() {
                return Err("too many export start arguments".into());
            }
            crate::meta_export::test_start(app, format, scope, recursive)?;
            Ok(Flow::Done)
        }
        Some("wait") => {
            let timeout = match words.next() {
                Some(value) => {
                    let seconds = value
                        .parse::<u64>()
                        .map_err(|_| "export wait expects an integer timeout in seconds")?;
                    if seconds > 300 {
                        return Err("export wait timeout cannot exceed 300 seconds".into());
                    }
                    Duration::from_secs(seconds)
                }
                None => WAIT_LIMIT,
            };
            if words.next().is_some() {
                return Err("too many export wait arguments".into());
            }
            let Some((state, _, _)) = crate::meta_export::test_status() else {
                return Err("no metadata export has started".into());
            };
            if state.is_finished() {
                Ok(Flow::Done)
            } else {
                let started = *st.started.get_or_insert_with(Instant::now);
                if started.elapsed() >= timeout {
                    Err(format!(
                        "metadata export did not finish within {} seconds",
                        timeout.as_secs()
                    ))
                } else {
                    Ok(Flow::Wait(format!(
                        "waiting up to {} seconds for export",
                        timeout.as_secs()
                    )))
                }
            }
        }
        Some("assert") => {
            let assertion = words.next().ok_or("expected an export assertion")?;
            let status =
                crate::meta_export::test_status().ok_or("no metadata export has started")?;
            match assertion {
                "success" => {
                    if status.0 == JobState::Completed && status.2 == Some(Ok(())) {
                        Ok(Flow::Done)
                    } else {
                        Err(format!(
                            "expected successful export, found {:?}: {:?}",
                            status.0, status.2
                        ))
                    }
                }
                "failed" => {
                    if status.0 == JobState::Failed {
                        Ok(Flow::Done)
                    } else {
                        Err(format!("expected failed export, found {:?}", status.0))
                    }
                }
                "cancelled" => {
                    if status.0 == JobState::Cancelled {
                        Ok(Flow::Done)
                    } else {
                        Err(format!("expected cancelled export, found {:?}", status.0))
                    }
                }
                "path" => {
                    let expected = words.collect::<Vec<_>>().join(" ");
                    if expected.is_empty() {
                        return Err("expected a path after `export assert path`".into());
                    }
                    let expected = super::sandboxed(Path::new(&expected))?;
                    if status.1 == expected && expected.is_file() {
                        Ok(Flow::Done)
                    } else {
                        Err(format!(
                            "expected export at {}, found {}",
                            expected.display(),
                            status.1.display()
                        ))
                    }
                }
                other => Err(format!("unknown export assertion `{other}`")),
            }
        }
        _ => Err("expected `export start`, `export wait`, or `export assert`".into()),
    }
}

fn parse_pair(value: &str) -> Result<(f64, f64), String> {
    let (latitude, longitude) = value
        .split_once(',')
        .ok_or("expected coordinates as `<lat>,<lon>`")?;
    let latitude = latitude
        .parse::<f64>()
        .map_err(|_| "expected a decimal latitude")?;
    let longitude = longitude
        .parse::<f64>()
        .map_err(|_| "expected a decimal longitude")?;
    Ok((latitude, longitude))
}
