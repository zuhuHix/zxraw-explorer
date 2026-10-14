//! Saved layouts (workspaces): "Save layout…" stores the open tabs, "Load layout…" reopens
//! them and "Manage layouts…" renames or deletes them. Layouts live in
//! `Settings::layouts`; each tab is the same [`TabSession`] the session file uses.

use std::path::{Path, PathBuf};

use crate::app::ZxrawApp;
use crate::commands::Cmd;
use crate::modal::{Modal, Prompt, PromptKind};
use crate::settings::{Layout, TabSession};

/// Shown in the sidebar before the list gets too long.
const SIDEBAR_LIMIT: usize = 8;

/// State of the "Load layout" and "Manage layouts" windows.
#[derive(Default)]
pub struct LayoutsUi {
    load_open: bool,
    manage_open: bool,
}

/// A row action in the layout lists.
#[derive(Clone, Copy)]
enum Action {
    Load,
    Rename,
    Up,
    Down,
    Delete,
}

/// What loading a layout would open, given which folders still exist.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub tabs: Vec<TabSession>,
    pub active: usize,
    /// Folders that were not found, each listed once, in layout order.
    pub missing: Vec<PathBuf>,
}

/// Runs `SaveLayout`, `LoadLayout` and `ManageLayouts`.
pub fn run(app: &mut ZxrawApp, cmd: Cmd) {
    match cmd {
        Cmd::SaveLayout => open_save_prompt(app),
        Cmd::LoadLayout => {
            if app.settings.layouts.is_empty() {
                app.toast("No saved layouts yet");
            } else {
                app.layouts.load_open = true;
            }
        }
        Cmd::ManageLayouts => app.layouts.manage_open = true,
        _ => {}
    }
}

/// Draws the layout windows; called every frame.
pub fn show(app: &mut ZxrawApp, ctx: &egui::Context) {
    if app.layouts.load_open {
        show_load(app, ctx);
    }
    if app.layouts.manage_open {
        show_manage(app, ctx);
    }
}

/// Applies an accepted `SaveLayout` or `RenameLayout` prompt. Returns the prompt again
/// (with an error) if it must stay open.
pub fn accept_label(app: &mut ZxrawApp, mut p: Prompt) -> Option<Modal> {
    let name = p.text.trim().to_string();
    match p.kind {
        PromptKind::SaveLayout => {
            if name.is_empty() {
                p.error = Some("Enter a name".into());
                return Some(Modal::Prompt(p));
            }
            let layout = capture(app, &name);
            if upsert(&mut app.settings.layouts, layout) {
                app.toast(format!("Layout “{name}” updated"));
            } else {
                app.toast(format!("Layout “{name}” saved"));
            }
            None
        }
        PromptKind::RenameLayout { index } => {
            match rename(&mut app.settings.layouts, index, &name) {
                Ok(()) => None,
                Err(e) => {
                    p.error = Some(e);
                    Some(Modal::Prompt(p))
                }
            }
        }
        _ => None,
    }
}

/// The sidebar's "Layouts" section: saved layouts (a click loads one) and the save and
/// manage actions. Called by `sidebar::show`.
pub fn sidebar_section(app: &mut ZxrawApp, ui: &mut egui::Ui) {
    crate::sidebar::section(ui, "Layouts");
    let count = app.settings.layouts.len();
    let mut chosen: Option<(usize, Action)> = None;
    if count == 0 {
        ui.weak("No saved layouts yet");
    }
    for (i, layout) in app.settings.layouts.iter().enumerate().take(SIDEBAR_LIMIT) {
        let response = ui.selectable_label(false, format!("▦  {}", layout.name));
        if response.clicked() {
            chosen = Some((i, Action::Load));
        }
        response.context_menu(|ui| {
            if ui.button("Load").clicked() {
                chosen = Some((i, Action::Load));
                ui.close();
            }
            if ui.button("Rename…").clicked() {
                chosen = Some((i, Action::Rename));
                ui.close();
            }
            if ui.button("Delete").clicked() {
                chosen = Some((i, Action::Delete));
                ui.close();
            }
        });
    }
    if count > SIDEBAR_LIMIT {
        ui.weak(format!("+ {} more", count - SIDEBAR_LIMIT));
    }
    ui.horizontal(|ui| {
        if ui.small_button("Save layout…").clicked() {
            open_save_prompt(app);
        }
        if ui.small_button("Manage…").clicked() {
            app.layouts.manage_open = true;
        }
    });
    if let Some((i, action)) = chosen {
        apply_action(app, i, action);
    }
}

/// Snapshot of the open tabs under `name`.
pub fn capture(app: &ZxrawApp, name: &str) -> Layout {
    let session = app.session();
    Layout {
        name: name.to_string(),
        tabs: session.tabs,
        active_tab: session.active_tab,
    }
}

/// Stores `layout` under its name. A layout with the same name (case-insensitive) is
/// replaced in place; otherwise it is appended. Returns true when one was replaced.
pub fn upsert(layouts: &mut Vec<Layout>, layout: Layout) -> bool {
    match layouts.iter_mut().find(|l| same_name(&l.name, &layout.name)) {
        Some(slot) => {
            *slot = layout;
            true
        }
        None => {
            layouts.push(layout);
            false
        }
    }
}

/// Renames the layout at `index`. The name must be non-empty and must not match another
/// layout's name (case-insensitive). An index out of range changes nothing.
pub fn rename(layouts: &mut [Layout], index: usize, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter a name".into());
    }
    if index >= layouts.len() {
        return Ok(());
    }
    let taken = layouts
        .iter()
        .enumerate()
        .any(|(i, l)| i != index && same_name(&l.name, name));
    if taken {
        return Err("A layout with that name already exists".into());
    }
    layouts[index].name = name.to_string();
    Ok(())
}

/// Works out which tabs of `layout` can open. A missing right pane is split off; a
/// missing left pane is replaced by the right one (with its view and sort); a tab with
/// both panes missing is dropped. `active` follows its tab, or the nearest earlier tab
/// that survives, or 0.
pub fn plan(layout: &Layout, exists: impl Fn(&Path) -> bool) -> Plan {
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut check = |path: &Path| {
        let ok = exists(path);
        if !ok && !missing.iter().any(|m| m == path) {
            missing.push(path.to_path_buf());
        }
        ok
    };

    let mut tabs = Vec::new();
    // New index of each layout tab, or None when it is dropped.
    let mut new_index = Vec::with_capacity(layout.tabs.len());
    for t in &layout.tabs {
        let left_ok = check(&t.left);
        let right_ok = t.right.as_deref().is_some_and(&mut check);
        let kept = match (left_ok, &t.right) {
            (true, _) => Some(TabSession {
                right: t.right.clone().filter(|_| right_ok),
                right_view: t.right_view.filter(|_| right_ok),
                right_sort: t.right_sort.filter(|_| right_ok),
                ..t.clone()
            }),
            (false, Some(right)) if right_ok => Some(TabSession {
                left: right.clone(),
                right: None,
                view: t.right_view.unwrap_or(t.view),
                sort: t.right_sort.or(t.sort),
                right_view: None,
                right_sort: None,
            }),
            _ => None,
        };
        new_index.push(kept.is_some().then_some(tabs.len()));
        tabs.extend(kept);
    }

    let active = if layout.tabs.is_empty() {
        0
    } else {
        let a = layout.active_tab.min(layout.tabs.len() - 1);
        (0..=a).rev().find_map(|i| new_index[i]).unwrap_or(0)
    };
    Plan {
        tabs,
        active,
        missing,
    }
}

/// Opens the layout at `index` (its position in `Settings::layouts`).
pub fn load(app: &mut ZxrawApp, index: usize) {
    if let Some(layout) = app.settings.layouts.get(index).cloned() {
        open_layout(app, &layout);
    }
}

/// Opens the layout called `name` (case-insensitive). Returns false if there is none.
pub fn load_by_name(app: &mut ZxrawApp, name: &str) -> bool {
    let Some(layout) = app
        .settings
        .layouts
        .iter()
        .find(|l| same_name(&l.name, name))
        .cloned()
    else {
        return false;
    };
    open_layout(app, &layout);
    true
}

/// Replaces the open tabs with the ones of `layout` that still exist. If none survive,
/// the current tabs stay.
fn open_layout(app: &mut ZxrawApp, layout: &Layout) {
    let plan = plan(layout, Path::is_dir);
    if plan.tabs.is_empty() {
        app.toast_err(format!(
            "Layout “{}”: none of its folders were found",
            layout.name
        ));
        return;
    }
    let fallback = app.start_folder();
    app.tabs = plan
        .tabs
        .iter()
        .map(|t| app.tab_from_session(t, &fallback).0)
        .collect();
    app.active = plan.active.min(app.tabs.len() - 1);
    if plan.missing.is_empty() {
        app.toast(format!("Layout “{}” loaded", layout.name));
    } else {
        let shown: Vec<String> = plan
            .missing
            .iter()
            .take(3)
            .map(|p| p.display().to_string())
            .collect();
        let more = if plan.missing.len() > 3 { "…" } else { "" };
        app.toast_err(format!(
            "Layout “{}”: {} folder(s) not found: {}{}",
            layout.name,
            plan.missing.len(),
            shown.join(", "),
            more
        ));
    }
}

fn open_save_prompt(app: &mut ZxrawApp) {
    let default = format!("Layout {}", app.settings.layouts.len() + 1);
    app.ops.modal = Some(Modal::Prompt(Prompt::new(
        PromptKind::SaveLayout,
        "Save layout",
        default,
    )));
}

fn open_rename_prompt(app: &mut ZxrawApp, index: usize) {
    let current = app.settings.layouts[index].name.clone();
    app.ops.modal = Some(Modal::Prompt(Prompt::new(
        PromptKind::RenameLayout { index },
        "Rename layout",
        current,
    )));
}

fn apply_action(app: &mut ZxrawApp, index: usize, action: Action) {
    if index >= app.settings.layouts.len() {
        return;
    }
    match action {
        Action::Load => load(app, index),
        Action::Rename => open_rename_prompt(app, index),
        Action::Up => {
            if index > 0 {
                app.settings.layouts.swap(index, index - 1);
            }
        }
        Action::Down => {
            if index + 1 < app.settings.layouts.len() {
                app.settings.layouts.swap(index, index + 1);
            }
        }
        Action::Delete => {
            let removed = app.settings.layouts.remove(index);
            app.toast(format!("Layout “{}” deleted", removed.name));
        }
    }
}

fn show_load(app: &mut ZxrawApp, ctx: &egui::Context) {
    let mut open = true;
    let mut close = false;
    let mut chosen = None;
    egui::Window::new("Load layout")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(320.0)
        .show(ctx, |ui| {
            if app.settings.layouts.is_empty() {
                ui.weak("No saved layouts yet");
            }
            for (i, layout) in app.settings.layouts.iter().enumerate() {
                let label = format!("{}  ·  {}", layout.name, tab_count(layout.tabs.len()));
                if ui.button(label).clicked() {
                    chosen = Some(i);
                }
            }
            ui.add_space(6.0);
            close = ui.button("Close").clicked();
        });
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        close = true;
    }
    if let Some(i) = chosen {
        load(app, i);
        close = true;
    }
    app.layouts.load_open = open && !close;
}

fn show_manage(app: &mut ZxrawApp, ctx: &egui::Context) {
    let mut open = true;
    let mut chosen: Option<(usize, Action)> = None;
    egui::Window::new("Manage layouts")
        .open(&mut open)
        .collapsible(false)
        .default_width(420.0)
        .show(ctx, |ui| {
            if app.settings.layouts.is_empty() {
                ui.weak("No saved layouts yet");
            }
            let count = app.settings.layouts.len();
            for (i, layout) in app.settings.layouts.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(&layout.name);
                    ui.weak(tab_count(layout.tabs.len()));
                    if ui.small_button("Load").clicked() {
                        chosen = Some((i, Action::Load));
                    }
                    if ui.small_button("Rename…").clicked() {
                        chosen = Some((i, Action::Rename));
                    }
                    if ui.add_enabled(i > 0, egui::Button::new("Move up").small()).clicked() {
                        chosen = Some((i, Action::Up));
                    }
                    if ui
                        .add_enabled(i + 1 < count, egui::Button::new("Move down").small())
                        .clicked()
                    {
                        chosen = Some((i, Action::Down));
                    }
                    if ui.small_button("Delete").clicked() {
                        chosen = Some((i, Action::Delete));
                    }
                });
            }
        });
    if let Some((i, action)) = chosen {
        apply_action(app, i, action);
    }
    app.layouts.manage_open = open;
}

fn tab_count(n: usize) -> String {
    if n == 1 {
        "1 tab".into()
    } else {
        format!("{n} tabs")
    }
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ViewMode;
    use forge_core::listing::SortSpec;
    use std::collections::HashSet;

    fn tab(left: &str, right: Option<&str>) -> TabSession {
        TabSession {
            left: left.into(),
            right: right.map(Into::into),
            ..TabSession::default()
        }
    }

    fn layout(tabs: Vec<TabSession>, active_tab: usize) -> Layout {
        Layout {
            name: "Work".into(),
            tabs,
            active_tab,
        }
    }

    fn existing(paths: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |p: &Path| set.contains(p)
    }

    #[test]
    fn plan_keeps_everything_when_all_present() {
        let l = layout(vec![tab("A", None), tab("B", Some("C"))], 1);
        let p = plan(&l, existing(&["A", "B", "C"]));
        assert_eq!(p.tabs, l.tabs);
        assert_eq!(p.active, 1);
        assert!(p.missing.is_empty());
    }

    #[test]
    fn plan_splits_off_missing_right_pane() {
        let mut t = tab("A", Some("C"));
        t.right_view = Some(ViewMode::Grid);
        t.right_sort = Some(SortSpec::default());
        let p = plan(&layout(vec![t], 0), existing(&["A"]));
        assert_eq!(p.tabs, vec![tab("A", None)]);
        assert_eq!(p.missing, vec![PathBuf::from("C")]);
    }

    #[test]
    fn plan_promotes_right_pane_when_left_missing() {
        let mut t = tab("A", Some("C"));
        t.view = ViewMode::Details;
        t.right_view = Some(ViewMode::Grid);
        let sort = SortSpec {
            descending: true,
            ..SortSpec::default()
        };
        t.right_sort = Some(sort);
        let p = plan(&layout(vec![t], 0), existing(&["C"]));
        assert_eq!(
            p.tabs,
            vec![TabSession {
                left: "C".into(),
                right: None,
                view: ViewMode::Grid,
                sort: Some(sort),
                right_view: None,
                right_sort: None,
            }]
        );
        assert_eq!(p.missing, vec![PathBuf::from("A")]);
    }

    #[test]
    fn plan_drops_tab_with_both_panes_missing() {
        let l = layout(vec![tab("A", Some("B")), tab("C", None)], 0);
        let p = plan(&l, existing(&["C"]));
        assert_eq!(p.tabs, vec![tab("C", None)]);
        assert_eq!(p.missing, vec![PathBuf::from("A"), PathBuf::from("B")]);
    }

    #[test]
    fn plan_active_follows_its_tab_or_nearest_earlier() {
        // Active tab (index 2) is dropped: the nearest earlier surviving tab (index 1).
        let l = layout(vec![tab("A", None), tab("B", None), tab("X", None)], 2);
        let p = plan(&l, existing(&["A", "B"]));
        assert_eq!(p.active, 1);

        // First tab is dropped and active was 0: falls back to 0.
        let l = layout(vec![tab("X", None), tab("A", None)], 0);
        let p = plan(&l, existing(&["A"]));
        assert_eq!(p.tabs, vec![tab("A", None)]);
        assert_eq!(p.active, 0);

        // Active tab survives but earlier ones are dropped: its new index is used.
        let l = layout(vec![tab("X", None), tab("A", None), tab("B", None)], 2);
        let p = plan(&l, existing(&["A", "B"]));
        assert_eq!(p.active, 1);
    }

    #[test]
    fn plan_with_everything_missing_has_no_tabs() {
        let l = layout(vec![tab("A", None), tab("B", Some("C"))], 0);
        let p = plan(&l, existing(&[]));
        assert!(p.tabs.is_empty());
        assert_eq!(p.active, 0);
        assert_eq!(p.missing.len(), 3);
    }

    #[test]
    fn plan_lists_duplicate_missing_paths_once() {
        let l = layout(vec![tab("A", None), tab("A", Some("B")), tab("B", None)], 0);
        let p = plan(&l, existing(&[]));
        assert_eq!(p.missing, vec![PathBuf::from("A"), PathBuf::from("B")]);
    }

    #[test]
    fn upsert_adds_new_and_overwrites_same_name_in_place() {
        let mut list = vec![
            Layout { name: "Alpha".into(), ..Layout::default() },
            Layout { name: "Work".into(), ..Layout::default() },
        ];
        let updated = upsert(&mut list, Layout { name: "Beta".into(), ..Layout::default() });
        assert!(!updated);
        assert_eq!(list.len(), 3);
        assert_eq!(list[2].name, "Beta");

        let replacement = Layout {
            name: "WORK".into(),
            tabs: vec![tab("Z", None)],
            active_tab: 0,
        };
        let updated = upsert(&mut list, replacement.clone());
        assert!(updated);
        assert_eq!(list.len(), 3);
        assert_eq!(list[1], replacement);
    }

    #[test]
    fn rename_checks_name() {
        let mut list = vec![
            Layout { name: "Alpha".into(), ..Layout::default() },
            Layout { name: "Beta".into(), ..Layout::default() },
        ];
        assert!(rename(&mut list, 0, "  Gamma  ").is_ok());
        assert_eq!(list[0].name, "Gamma");

        assert_eq!(rename(&mut list, 0, "   "), Err("Enter a name".into()));
        assert_eq!(
            rename(&mut list, 0, "beta"),
            Err("A layout with that name already exists".into())
        );
        assert!(rename(&mut list, 1, "BETA").is_ok());
        assert_eq!(list[1].name, "BETA");
        assert!(rename(&mut list, 9, "Anything").is_ok());
    }

    #[test]
    fn layout_round_trips_through_json() {
        let mut t = tab("C:\\A", Some("C:\\B"));
        t.view = ViewMode::Grid;
        t.sort = Some(SortSpec {
            descending: true,
            ..SortSpec::default()
        });
        t.right_view = Some(ViewMode::Details);
        let l = layout(vec![t], 0);
        let json = serde_json::to_string(&l).unwrap();
        let back: Layout = serde_json::from_str(&json).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn layout_loads_old_tab_shape() {
        let json = r#"{"name":"Old","tabs":[{"left":"C:\\A","right":null,"view":"Grid"}],"active_tab":0}"#;
        let l: Layout = serde_json::from_str(json).unwrap();
        assert_eq!(l.name, "Old");
        assert_eq!(l.tabs.len(), 1);
        assert_eq!(l.tabs[0].left, PathBuf::from("C:\\A"));
        assert_eq!(l.tabs[0].view, ViewMode::Grid);
        assert_eq!(l.tabs[0].sort, None);
        assert_eq!(l.tabs[0].right_view, None);
        assert_eq!(l.tabs[0].right_sort, None);
    }
}
