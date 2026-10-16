//! `ui library <verb> [args]` script verbs.
//!
//! Each verb drives the same code the window's buttons and the command palette reach, then
//! waits for it. Waits give up after 10 s and fail with the last observed value. Names in
//! lists are file names (exact sets, order ignored); `none` stands for the empty set.
//!
//! Search (the search window):
//! - `search <query>`: open the window, set the query and run it; waits for the results.
//! - `assert-results <names> | none`: the results must be exactly these names (offline rows
//!   included). Fails while a search is still running.
//! - `assert-search-error <text>`: the last search must have failed with an error message
//!   containing the text (and shown no results).
//! - `assert-result-count <n>`.
//! - `index <folder>`: start an index job for a folder, as the window's "Re-index" button
//!   does (first waiting for the library database to open). Use `wait-jobs` to wait for it.
//! - `wait-files <min> <max>`: wait until the number of indexed rows is in `min..max`
//!   (handy for an index job caught part-way).
//!
//! Collections (the collections window):
//! - `collection-new <name>`: a manual collection holding the current selection; waits for
//!   the save.
//! - `collection-open <name>`: show it in the window; waits for its rows.
//! - `collection-shown <name> | <names>`: the rows the open window lists (exact set).
//! - `collection-assert <name> | <names>`: exact member set read from the database,
//!   offline members included.
//! - `collection-assert-offline <name> | <names>`: the members that are offline.
//! - `collection-window-size <min-width> <min-height>`: the window is at least this big
//!   (waits up to 10 s for it to grow to that size).
//! - `collection-resize <dx> <dy>`: drags the window's bottom-right grip by the offset in
//!   points.
//! - `choose import|export <name>|relink-old|relink-new`: the window's dialog buttons. A
//!   `dialog <path>` step queued before answers them. Waits for the operation.
//! - `relink-preview <count>`: preview the relink of the chosen folders; `count` must match.
//! - `relink-confirm`: confirm the relink; waits for it to finish.
//!
//! Tags and notes (the current selection):
//! - `tag add|remove <tag>[, <tag>…]`: applied as the tag editor applies it.
//! - `note <text>` or `note clear`.
//! - `rate <0-5>`.
//! - `assert-tags <path> | <tags>`, `assert-note <path> | <text>` and
//!   `assert-rating <path> | <0-5>`: read from the database, so they see persisted values.
//!
//! Settings:
//! - `setting <key> <value>`: sets a field of the in-memory settings. They are saved when
//!   the app exits. Keys: `theme` (dark|light|system), `show_hidden`, `show_system`,
//!   `default_view` (details|grid), `thumb_size`, `xmp_sidecar_sync`.
//! - `assert-setting <key> <value>`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use forge_db::{AnnotationChange, Db, FileId};

use super::{Flow, StepState, inject};
use crate::actions::Event;
use crate::app::ZxrawApp;
use crate::collections_ui::{self, UiCommand};
use crate::settings::{ThemeChoice, ViewMode};

const WAIT: Duration = Duration::from_secs(10);

pub(super) fn step(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    verb: &str,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    match verb {
        "search" => search(app, args, st),
        "assert-results" => {
            let expected = name_set(args)?;
            let search = &app.ops.library.search;
            if search.running {
                return wait(st, "a search is still running");
            }
            if let Some(error) = &search.error {
                return Err(format!("search failed: {error}"));
            }
            let found = names_of(search.results.iter().map(|r| r.name.as_str()));
            check_set("search results", &expected, &found, st)
        }
        "assert-search-error" => {
            let expected = non_empty(args, "expected part of the error message")?;
            let search = &app.ops.library.search;
            if search.running {
                return wait(st, "a search is still running");
            }
            match &search.error {
                Some(error) if error.contains(expected.as_str()) && search.results.is_empty() => {
                    Ok(Flow::Done)
                }
                Some(error) => Err(format!(
                    "search error `{error}` does not contain `{expected}` or results were shown"
                )),
                None => Err("the search did not report an error".into()),
            }
        }
        "assert-result-count" => {
            let expected = parse_count(args)?;
            let search = &app.ops.library.search;
            if search.running {
                return wait(st, "a search is still running");
            }
            let found = search.results.len();
            if found == expected {
                Ok(Flow::Done)
            } else {
                Err(format!(
                    "expected {expected} search result(s), found {found}"
                ))
            }
        }
        "index" => {
            let folder = super::sandboxed(Path::new(&non_empty(args, "expected a folder")?))?;
            // The database opens (and migrates) on a background thread; a script's first step
            // can come sooner on a busy machine.
            if app.ops.library.db.is_none() {
                return wait(st, "the library database is still opening");
            }
            let db = library_db(app)?;
            let tx = app.ops.tx.clone();
            let c = ctx.clone();
            crate::library::start_index_job(&app.ops.jobs, db, folder, move |m| {
                let _ = tx.send(Event::Toast(m));
                c.request_repaint();
            });
            Ok(Flow::Done)
        }
        "wait-files" => {
            let (min, max) = two_numbers(args)?;
            if app.ops.library.db.is_none() {
                return wait(st, "the library database is still opening");
            }
            let db = library_db(app)?;
            let files = db.stats().map_err(|e| e.to_string())?.files as usize;
            if (min..max).contains(&files) {
                Ok(Flow::Done)
            } else {
                wait(st, &format!("{files} indexed row(s), want {min}..{max}"))
            }
        }
        "collection-new" => {
            let name = non_empty(args, "expected a collection name")?;
            match st.phase {
                0 => {
                    collections_ui::create_manual_from_selection(app, ctx, &name);
                    st.phase = 1;
                    wait(st, "saving the collection")
                }
                _ if !collections_ui::testhook_idle(app) => wait(st, "saving the collection"),
                _ => {
                    let db = library_db(app)?;
                    match db.collection_by_name(&name).map_err(|e| e.to_string())? {
                        Some(_) => Ok(Flow::Done),
                        None => Err(format!("collection `{name}` was not created")),
                    }
                }
            }
        }
        "collection-open" => {
            let name = non_empty(args, "expected a collection name")?;
            let id = collection_id(app, &name)?;
            match st.phase {
                0 => {
                    crate::collections_ui::open_collection(app, id);
                    st.phase = 1;
                    wait(st, "loading the collection")
                }
                _ if collections_ui::testhook_shown_rows(app, id).is_some() => Ok(Flow::Done),
                _ => wait(st, "loading the collection"),
            }
        }
        "collection-shown" => {
            let (name, expected) = name_and_set(args)?;
            let id = collection_id(app, &name)?;
            match collections_ui::testhook_shown_rows(app, id) {
                Some(rows) => {
                    let found = rows.iter().cloned().collect::<BTreeSet<_>>();
                    check_set(&format!("rows shown for `{name}`"), &expected, &found, st)
                }
                None => wait(st, "the collection is not open yet"),
            }
        }
        "collection-assert" | "collection-assert-offline" => {
            let offline_only = verb == "collection-assert-offline";
            let (name, expected) = name_and_set(args)?;
            let db = library_db(app)?;
            let id = collection_id(app, &name)?;
            let rows = db
                .collection_files(
                    id,
                    &forge_db::SearchOptions {
                        include_offline: true,
                        limit: None,
                        ..Default::default()
                    },
                )
                .map_err(|e| e.to_string())?;
            let found = names_of(
                rows.iter()
                    .filter(|r| !offline_only || !r.present)
                    .map(|r| r.name.as_str()),
            );
            check_set(&format!("members of `{name}`"), &expected, &found, st)
        }
        "collection-window-size" => collection_window_size(app, args, st),
        "collection-resize" => collection_resize(app, args, st),
        "choose" => choose(app, ctx, args, st),
        "relink-preview" => {
            let expected = parse_count(args)?;
            let (old, new) = relink_paths(app)?;
            if st.phase == 0 {
                collections_ui::execute_command(app, ctx, UiCommand::PreviewRelink { old, new });
                st.phase = 1;
                return wait(st, "previewing the relink");
            }
            match collections_ui::testhook_relink_preview(app)? {
                None => wait(st, "previewing the relink"),
                Some(count) if count == expected as u64 => Ok(Flow::Done),
                Some(count) => Err(format!(
                    "relink preview found {count} item(s), expected {expected}"
                )),
            }
        }
        "relink-confirm" => {
            let (old, new) = relink_paths(app)?;
            if st.phase == 0 {
                collections_ui::execute_command(app, ctx, UiCommand::ConfirmRelink { old, new });
                st.phase = 1;
                return wait(st, "relinking");
            }
            if collections_ui::testhook_idle(app) {
                Ok(Flow::Done)
            } else {
                wait(st, "relinking")
            }
        }
        "tag" => tag(app, args),
        "note" => {
            let text = non_empty(args, "expected note text or `clear`")?;
            let notes = if text == "clear" { None } else { Some(text) };
            annotate_selection(
                app,
                AnnotationChange {
                    notes: Some(notes),
                    ..Default::default()
                },
            )?;
            Ok(Flow::Done)
        }
        "rate" => {
            let rating = parse_count(args)?;
            if rating > 5 {
                return Err("rating must be 0-5".into());
            }
            annotate_selection(
                app,
                AnnotationChange {
                    rating: Some(rating as u8),
                    ..Default::default()
                },
            )?;
            Ok(Flow::Done)
        }
        "assert-tags" => {
            let (path, expected) = path_and_set(args)?;
            let db = library_db(app)?;
            let found = persisted(&db, &path)?.tags;
            let found = found.into_iter().collect::<BTreeSet<_>>();
            check_set(
                &format!("tags of {}", path.display()),
                &expected,
                &found,
                st,
            )
        }
        "assert-rating" => {
            let (path, expected) = path_and_text(args)?;
            let rating = expected
                .parse::<u8>()
                .map_err(|_| format!("expected a rating 0-5, got `{expected}`"))?;
            let db = library_db(app)?;
            let found = persisted(&db, &path)?.rating;
            if found == rating {
                Ok(Flow::Done)
            } else {
                wait(st, &format!("rating is {found}, want {rating}"))
            }
        }
        "assert-note" => {
            let (path, expected) = path_and_text(args)?;
            let db = library_db(app)?;
            let found = persisted(&db, &path)?.notes.unwrap_or_default();
            if found == expected {
                Ok(Flow::Done)
            } else {
                wait(st, &format!("notes are `{found}`, want `{expected}`"))
            }
        }
        "setting" => {
            let (key, value) = key_and_value(args)?;
            set_setting(app, key, value)?;
            Ok(Flow::Done)
        }
        "assert-setting" => {
            let (key, value) = key_and_value(args)?;
            let found = setting(app, key)?;
            if found == value {
                Ok(Flow::Done)
            } else {
                Err(format!("setting `{key}` is `{found}`, expected `{value}`"))
            }
        }
        other => Err(format!("unknown `ui library` verb `{other}`")),
    }
}

// ---------------------------------------------------------------------------
// Verbs
// ---------------------------------------------------------------------------

/// `collection-window-size <min-width> <min-height>`: the collections window is at least this
/// big. Waits for it to appear, or to grow to the size.
fn collection_window_size(app: &ZxrawApp, args: &str, st: &mut StepState) -> Result<Flow, String> {
    let (min_w, min_h) = float_pair(args)?;
    match collections_ui::testhook_window_size(app) {
        Some(size) if size.x >= min_w && size.y >= min_h => Ok(Flow::Done),
        Some(size) => wait(st, &format!("window is {:.0} x {:.0}", size.x, size.y)),
        None => wait(st, "collections window is not shown"),
    }
}

/// `collection-resize <dx> <dy>`: drags the collections window's bottom-right grip by the
/// offset in points. Press, move and release land in three frames, like a real drag.
fn collection_resize(app: &mut ZxrawApp, args: &str, st: &mut StepState) -> Result<Flow, String> {
    let (dx, dy) = float_pair(args)?;
    if st.data.is_none() {
        let Some(grip) = collections_ui::testhook_window_grip(app) else {
            return wait(st, "collections window is not shown");
        };
        st.data = Some(Box::new((grip, grip + egui::vec2(dx, dy))));
    }
    let (from, to) = *st
        .data
        .as_ref()
        .and_then(|data| data.downcast_ref::<(egui::Pos2, egui::Pos2)>())
        .ok_or("invalid collection-resize state")?;
    let press = |pos, pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    let (pos, events) = match st.frames {
        0 => (
            from,
            vec![egui::Event::PointerMoved(from), press(from, true)],
        ),
        1 => (to, vec![egui::Event::PointerMoved(to)]),
        _ => (to, vec![press(to, false)]),
    };
    app.testhook.input.pointer = Some(pos);
    match inject(app, st, &events) {
        Flow::Done if st.frames < 2 => {
            st.frames += 1;
            st.phase = 0;
            Ok(Flow::Wait("drag in progress".into()))
        }
        other => Ok(other),
    }
}

fn search(app: &mut ZxrawApp, args: &str, st: &mut StepState) -> Result<Flow, String> {
    let query = non_empty(args, "expected a search query")?;
    if st.phase == 0 {
        let lib = &mut app.ops.library;
        lib.search.open = true;
        lib.search.query = query;
        lib.run_search();
        st.phase = 1;
        return wait(st, "searching");
    }
    let search = &app.ops.library.search;
    if search.running {
        wait(st, "searching")
    } else {
        Ok(Flow::Done)
    }
}

fn choose(
    app: &mut ZxrawApp,
    ctx: &egui::Context,
    args: &str,
    st: &mut StepState,
) -> Result<Flow, String> {
    if st.phase == 0 {
        let mut parts = args.splitn(2, char::is_whitespace);
        let what = parts.next().unwrap_or_default();
        let command = match what {
            "import" => UiCommand::ChooseImport,
            "relink-old" => UiCommand::ChooseRelinkOld,
            "relink-new" => UiCommand::ChooseRelinkNew,
            "export" => {
                let name = non_empty(
                    parts.next().unwrap_or_default().trim(),
                    "expected a collection name",
                )?;
                let collection_id = collection_id(app, &name)?;
                UiCommand::ChooseExport {
                    collection_id,
                    name,
                }
            }
            other => return Err(format!("unknown dialog `{other}`")),
        };
        collections_ui::execute_command(app, ctx, command);
        st.phase = 1;
    }
    if collections_ui::testhook_idle(app) {
        Ok(Flow::Done)
    } else {
        wait(st, "waiting for the operation")
    }
}

fn tag(app: &mut ZxrawApp, args: &str) -> Result<Flow, String> {
    let (mode, tags) = args
        .split_once(char::is_whitespace)
        .ok_or("expected `add <tags>` or `remove <tags>`")?;
    let tags: Vec<String> = tags
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect();
    let change = match mode {
        "add" => AnnotationChange {
            add_tags: tags,
            ..Default::default()
        },
        "remove" => AnnotationChange {
            remove_tags: tags,
            ..Default::default()
        },
        other => return Err(format!("expected `add` or `remove`, got `{other}`")),
    };
    annotate_selection(app, change)?;
    Ok(Flow::Done)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Waits for another frame, failing once the step has waited longer than [`WAIT`].
fn wait(st: &mut StepState, reason: &str) -> Result<Flow, String> {
    let started = st.started.get_or_insert_with(std::time::Instant::now);
    if started.elapsed() > WAIT {
        Err(format!("timed out after {} s: {reason}", WAIT.as_secs()))
    } else {
        Ok(Flow::Wait(reason.to_string()))
    }
}

/// Exact set comparison: `Done` on a match, a wait while it is still different.
fn check_set(
    what: &str,
    expected: &BTreeSet<String>,
    found: &BTreeSet<String>,
    st: &mut StepState,
) -> Result<Flow, String> {
    if found == expected {
        Ok(Flow::Done)
    } else {
        wait(
            st,
            &format!("{what}: found {found:?}, expected {expected:?}"),
        )
    }
}

fn library_db(app: &ZxrawApp) -> Result<Arc<Db>, String> {
    app.ops
        .library
        .db
        .clone()
        .ok_or_else(|| "the library database is not open".to_string())
}

fn collection_id(app: &ZxrawApp, name: &str) -> Result<forge_db::CollectionId, String> {
    let db = library_db(app)?;
    db.collection_by_name(name)
        .map_err(|e| e.to_string())?
        .map(|c| c.id)
        .ok_or_else(|| format!("no collection named `{name}`"))
}

fn persisted(db: &Db, path: &Path) -> Result<forge_db::Annotation, String> {
    let id: FileId = db
        .file_id(path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{} is not in the library", path.display()))?;
    db.annotation(id).map_err(|e| e.to_string())
}

/// Applies a change to the selected files through the library's annotation path.
fn annotate_selection(app: &mut ZxrawApp, change: AnnotationChange) -> Result<(), String> {
    let paths = app.pane().action_targets();
    if paths.is_empty() {
        return Err("select files first".into());
    }
    app.ops.library.annotate(paths, change);
    Ok(())
}

fn relink_paths(app: &ZxrawApp) -> Result<(PathBuf, PathBuf), String> {
    collections_ui::testhook_relink_paths(app).ok_or_else(|| {
        "choose the old and new folders first (`choose relink-old|relink-new`)".to_string()
    })
}

fn setting(app: &ZxrawApp, key: &str) -> Result<String, String> {
    let s = &app.settings;
    Ok(match key {
        "theme" => format!("{:?}", *s.theme).to_lowercase(),
        "show_hidden" => s.show_hidden.to_string(),
        "show_system" => s.show_system.to_string(),
        "default_view" => format!("{:?}", *s.default_view).to_lowercase(),
        "thumb_size" => s.thumb_size.to_string(),
        "xmp_sidecar_sync" => s.xmp_sidecar_sync.to_string(),
        other => return Err(format!("unknown setting `{other}`")),
    })
}

fn set_setting(app: &mut ZxrawApp, key: &str, value: &str) -> Result<(), String> {
    let bool_value = || {
        value
            .parse::<bool>()
            .map_err(|_| format!("`{key}` takes true or false"))
    };
    match key {
        "theme" => {
            app.settings.theme.set(match value {
                "dark" => ThemeChoice::Dark,
                "light" => ThemeChoice::Light,
                "system" => ThemeChoice::System,
                _ => return Err("theme is dark, light or system".into()),
            })
        }
        "show_hidden" => app.settings.show_hidden = bool_value()?,
        "show_system" => app.settings.show_system = bool_value()?,
        "default_view" => {
            app.settings.default_view.set(match value {
                "details" => ViewMode::Details,
                "grid" => ViewMode::Grid,
                _ => return Err("default_view is details or grid".into()),
            })
        }
        "thumb_size" => {
            app.settings.thumb_size = value
                .parse::<f32>()
                .map_err(|_| "thumb_size takes a number".to_string())?
        }
        "xmp_sidecar_sync" => app.settings.xmp_sidecar_sync = bool_value()?,
        other => return Err(format!("unknown setting `{other}`")),
    }
    Ok(())
}

fn key_and_value(args: &str) -> Result<(&str, &str), String> {
    let mut parts = args.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some(key), Some(value), None) => Ok((key, value)),
        _ => Err("expected `<key> <value>`".into()),
    }
}

fn name_set(args: &str) -> Result<BTreeSet<String>, String> {
    let args = args.trim();
    if args == "none" {
        return Ok(BTreeSet::new());
    }
    Ok(args
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

/// `<name> | <names>`.
fn name_and_set(args: &str) -> Result<(String, BTreeSet<String>), String> {
    let (name, names) = two_parts(args)?;
    Ok((name.to_string(), name_set(names)?))
}

/// `<path> | <names or tags>`.
fn path_and_set(args: &str) -> Result<(PathBuf, BTreeSet<String>), String> {
    let (path, rest) = two_parts(args)?;
    let set = if rest == "none" {
        BTreeSet::new()
    } else {
        name_set(rest)?
    };
    Ok((super::sandboxed(Path::new(path))?, set))
}

/// `<path> | <text>`; `none` stands for the empty text.
fn path_and_text(args: &str) -> Result<(PathBuf, String), String> {
    let (path, text) = two_parts(args)?;
    Ok((
        super::sandboxed(Path::new(path))?,
        if text == "none" {
            String::new()
        } else {
            text.to_string()
        },
    ))
}

fn two_parts(args: &str) -> Result<(&str, &str), String> {
    match args.split_once(" | ") {
        Some((a, b)) if !a.trim().is_empty() => Ok((a.trim(), b.trim())),
        _ => Err("expected `<first> | <second>`".into()),
    }
}

fn float_pair(args: &str) -> Result<(f32, f32), String> {
    let mut parts = args.split_whitespace().map(str::parse::<f32>);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(x)), Some(Ok(y)), None) => Ok((x, y)),
        _ => Err(format!("expected `<x> <y>` numbers, got `{args}`")),
    }
}

fn two_numbers(args: &str) -> Result<(usize, usize), String> {
    let mut parts = args.split_whitespace().map(str::parse::<usize>);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(min)), Some(Ok(max)), None) if min < max => Ok((min, max)),
        _ => Err("expected `<min> <max>` with min below max".into()),
    }
}

fn parse_count(args: &str) -> Result<usize, String> {
    args.trim()
        .parse::<usize>()
        .map_err(|_| format!("expected a number, got `{}`", args.trim()))
}

fn non_empty(args: &str, error: &str) -> Result<String, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        Err(error.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn names_of<'a>(names: impl Iterator<Item = &'a str>) -> BTreeSet<String> {
    names.map(String::from).collect()
}
