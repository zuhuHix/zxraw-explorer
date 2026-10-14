//! Saved layouts (workspaces): "Save layout…" stores the open tabs, "Load layout…" reopens
//! them and "Manage layouts…" renames or deletes them. Layouts live in
//! `Settings::layouts`; each tab is the same [`TabSession`] the session file uses.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::app::ZxrawApp;
use crate::commands::Cmd;
use crate::modal::{Modal, Prompt, PromptKind};
use crate::path_probe::Batch;
use crate::settings::{Layout, TabSession};

/// Shown in the sidebar before the list gets too long.
const SIDEBAR_LIMIT: usize = 8;

/// State of the "Load layout" and "Manage layouts" windows.
#[derive(Default)]
pub struct LayoutsUi {
    load_open: bool,
    manage_open: bool,
    /// A layout waiting for its folder checks before it opens.
    loading: Option<Loading>,
    /// Folder checks for the tabs restored from the last session.
    restored: Option<Batch>,
}

struct Loading {
    layout: Layout,
    check: Batch,
    /// The open tabs when loading started. If the user changes them meanwhile, the load is
    /// cancelled rather than overwriting their work.
    started_from: Workspace,
}

/// The open tabs (folders, views, sorting) and which one is active.
type Workspace = (Vec<TabSession>, usize);

fn workspace(app: &ZxrawApp) -> Workspace {
    (app.session().tabs, app.active)
}

/// What a pending load does this frame.
#[derive(Debug, PartialEq, Eq)]
enum LoadStep {
    Wait,
    Apply,
    Cancel,
}

fn load_step(started_from: &Workspace, current: &Workspace, checks_done: bool) -> LoadStep {
    if started_from != current {
        LoadStep::Cancel
    } else if checks_done {
        LoadStep::Apply
    } else {
        LoadStep::Wait
    }
}

/// Every folder a set of tabs opens, paired with whether it is on the network (so the probe
/// can keep local checks out of the network lane).
fn probe_paths(app: &ZxrawApp, tabs: &[TabSession]) -> Vec<(PathBuf, bool)> {
    let network_roots: Vec<PathBuf> = app
        .sidebar
        .drives
        .lock()
        .iter()
        .filter(|d| d.kind == "Network")
        .map(|d| d.root.clone())
        .collect();
    tabs.iter()
        .flat_map(|t| std::iter::once(t.left.clone()).chain(t.right.clone()))
        .map(|p| {
            let network = crate::path_probe::is_network(&p, &network_roots);
            (p, network)
        })
        .collect()
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
    if let Some(loading) = &app.layouts.loading {
        let mut cancel = false;
        egui::Window::new("Loading layout")
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 80.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Loading layout “{}”…", loading.layout.name));
                    cancel = ui.small_button("Cancel").clicked();
                });
            });
        if cancel {
            app.layouts.loading = None;
        }
    }
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
        extra: Default::default(),
        name: name.to_string(),
        tabs: session.tabs,
        active_tab: session.active_tab,
    }
}

/// Stores `layout` under its name. A layout with the same name (case-insensitive) is
/// replaced in place, keeping the fields a newer version stored on it (and on each tab that
/// still opens the same folder at the same position); otherwise it is appended. Returns true
/// when one was replaced.
pub fn upsert(layouts: &mut Vec<Layout>, mut layout: Layout) -> bool {
    match layouts.iter_mut().find(|l| same_name(&l.name, &layout.name)) {
        Some(slot) => {
            for (key, value) in std::mem::take(&mut slot.extra) {
                layout.extra.entry(key).or_insert(value);
            }
            for (new, old) in layout.tabs.iter_mut().zip(&mut slot.tabs) {
                if new.left == old.left {
                    for (key, value) in std::mem::take(&mut old.extra) {
                        new.extra.entry(key).or_insert(value);
                    }
                }
            }
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
                right_view: t.right_view.filter(|_| right_ok).into(),
                right_sort: t.right_sort.filter(|_| right_ok).into(),
                ..t.clone()
            }),
            (false, Some(right)) if right_ok => Some(TabSession {
                extra: Default::default(),
                left: right.clone(),
                right: None,
                view: t.right_view.unwrap_or(*t.view).into(),
                sort: t.right_sort.or(*t.sort).into(),
                right_view: None.into(),
                right_sort: None.into(),
                expanded: t.expanded.clone(),
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

/// Starts loading `layout`: its folders are checked off the UI thread (an offline share can
/// take the OS a long time to give up on), and [`tick`] opens it when they have answered.
fn open_layout(app: &mut ZxrawApp, layout: &Layout) {
    let check = Batch::start(probe_paths(app, &layout.tabs), None);
    app.layouts.loading = Some(Loading {
        layout: layout.clone(),
        check,
        started_from: workspace(app),
    });
}

/// True while a layout's folder checks are running.
#[allow(dead_code)] // used by the test hook
pub fn is_loading(app: &ZxrawApp) -> bool {
    app.layouts.loading.is_some()
}

/// Checks the folders of the tabs restored from the last session; see [`tick`].
pub fn check_restored(app: &mut ZxrawApp) {
    let tabs = app.session().tabs;
    app.layouts.restored = Some(Batch::start(probe_paths(app, &tabs), None));
}

/// Applies finished folder checks. Called every frame from `logic`.
pub fn tick(app: &mut ZxrawApp, ctx: &egui::Context) {
    let now = Instant::now();
    let mut pending = false;
    if app.layouts.loading.is_some() {
        let current = workspace(app);
        let loading = app.layouts.loading.as_mut().expect("checked above");
        let done = loading.check.poll(now);
        match load_step(&loading.started_from, &current, done) {
            LoadStep::Wait => pending = true,
            LoadStep::Apply => {
                let loading = app.layouts.loading.take().expect("checked above");
                finish_load(app, &loading.layout, &loading.check);
            }
            LoadStep::Cancel => {
                let loading = app.layouts.loading.take().expect("checked above");
                app.toast(format!(
                    "Stopped loading layout “{}”: the tabs were changed",
                    loading.layout.name
                ));
            }
        }
    }
    if let Some(check) = &mut app.layouts.restored {
        if check.poll(now) {
            let check = app.layouts.restored.take().expect("checked above");
            fix_restored(app, &check);
        } else {
            pending = true;
        }
    }
    if pending {
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

/// Replaces the open tabs with the ones of `layout` whose folders exist. Only a check that
/// found nothing (or a file) counts as missing: a folder whose check did not answer in time or
/// failed some other way (an offline share, an unplugged drive) opens anyway and its tab shows
/// its own error. If no tab survives, the current tabs stay.
fn finish_load(app: &mut ZxrawApp, layout: &Layout, check: &Batch) {
    let plan = plan(layout, |p| !check.is_missing(p));
    let slow: Vec<PathBuf> = check
        .unchecked()
        .into_iter()
        .filter(|p| plan.tabs.iter().any(|t| &t.left == p || t.right.as_ref() == Some(p)))
        .collect();
    if plan.tabs.is_empty() {
        app.toast_err(format!(
            "Layout “{}”: none of its folders were found",
            layout.name
        ));
        return;
    }
    app.tabs = plan.tabs.iter().map(|t| app.tab_from_session(t)).collect();
    app.active = plan.active.min(app.tabs.len() - 1);
    if plan.missing.is_empty() && slow.is_empty() {
        app.toast(format!("Layout “{}” loaded", layout.name));
        return;
    }
    let mut parts = Vec::new();
    if !plan.missing.is_empty() {
        parts.push(format!(
            "{} folder(s) not found: {}",
            plan.missing.len(),
            path_list(&plan.missing)
        ));
    }
    if !slow.is_empty() {
        parts.push(format!(
            "{} could not be checked, opened anyway: {}",
            slow.len(),
            path_list(&slow)
        ));
    }
    app.toast_err(format!("Layout “{}”: {}", layout.name, parts.join("; ")));
}

fn path_list(paths: &[PathBuf]) -> String {
    let shown: Vec<String> = paths.iter().take(3).map(|p| p.display().to_string()).collect();
    let more = if paths.len() > 3 { "…" } else { "" };
    format!("{}{more}", shown.join(", "))
}

/// Restored panes whose folder was found to be gone switch to the start folder, unless the user
/// has already navigated away. A folder whose check did not answer or failed some other way (an
/// offline share, a sleeping NAS, an unplugged drive) stays, and its pane shows its own error.
fn fix_restored(app: &mut ZxrawApp, check: &Batch) {
    let fallback = app.start_folder();
    let panes = app.tabs.iter().enumerate().flat_map(|(t, tab)| {
        std::iter::once((t, false, &tab.left))
            .chain(tab.right.iter().map(move |r| (t, true, r)))
            .map(|(t, right, pane)| (t, right, pane.path.as_path(), pane.can_back()))
    });
    for (t, right) in stale_panes(panes, check) {
        let old = if right { app.tabs[t].right.as_ref() } else { Some(&app.tabs[t].left) };
        let Some(old) = old else { continue };
        let mut pane = app.new_pane(fallback.clone(), old.view);
        pane.sort = old.sort;
        match right {
            false => app.tabs[t].left = pane,
            true => app.tabs[t].right = Some(pane),
        }
    }
}

/// The restored panes (tab index, right pane?) to switch to the start folder: each given as
/// (tab, right, folder, navigated since), it goes when its folder is confirmed missing and the
/// user has not navigated away.
fn stale_panes<'a>(
    panes: impl IntoIterator<Item = (usize, bool, &'a Path, bool)>,
    check: &Batch,
) -> Vec<(usize, bool)> {
    panes
        .into_iter()
        .filter(|&(_, _, path, navigated)| !navigated && check.is_missing(path))
        .map(|(t, right, ..)| (t, right))
        .collect()
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
            extra: Default::default(),
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
        *t.right_view = Some(ViewMode::Grid);
        *t.right_sort = Some(SortSpec::default());
        let p = plan(&layout(vec![t], 0), existing(&["A"]));
        assert_eq!(p.tabs, vec![tab("A", None)]);
        assert_eq!(p.missing, vec![PathBuf::from("C")]);
    }

    #[test]
    fn plan_promotes_right_pane_when_left_missing() {
        let mut t = tab("A", Some("C"));
        *t.view = ViewMode::Details;
        *t.right_view = Some(ViewMode::Grid);
        let sort = SortSpec {
            descending: true,
            ..SortSpec::default()
        };
        *t.right_sort = Some(sort);
        let p = plan(&layout(vec![t], 0), existing(&["C"]));
        assert_eq!(
            p.tabs,
            vec![TabSession {
                extra: Default::default(),
                left: "C".into(),
                right: None,
                view: ViewMode::Grid.into(),
                sort: Some(sort).into(),
                right_view: None.into(),
                right_sort: None.into(),
                expanded: Vec::new(),
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
            extra: Default::default(),
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
    fn overwriting_a_layout_keeps_fields_from_a_newer_version() {
        let json = r#"{"name": "Work", "pinned": true, "tabs": [
            {"left": "A", "zoom": 2},
            {"left": "B", "zoom": 3},
            {"left": "C", "zoom": 4}
        ]}"#;
        let mut list = vec![serde_json::from_str::<Layout>(json).unwrap()];
        let mut moved = tab("X", None);
        moved.extra.insert("zoom".into(), 9.into());
        let replacement = layout(vec![tab("A", Some("D")), moved, tab("C", None), tab("E", None)], 0);
        assert!(upsert(&mut list, replacement));
        let saved = serde_json::to_value(&list[0]).unwrap();
        assert_eq!(saved["pinned"], true);
        assert_eq!(saved["tabs"][0]["zoom"], 2, "same folder at the same place");
        assert_eq!(saved["tabs"][1]["zoom"], 9, "a different folder keeps its own fields");
        assert_eq!(saved["tabs"][2]["zoom"], 4);
        assert!(saved["tabs"][3].get("zoom").is_none());
        assert_eq!(saved["tabs"][0]["right"], "D");
    }

    #[test]
    fn restore_keeps_panes_whose_check_failed_without_not_found() {
        use crate::path_probe::Found;
        let check = Batch::finished([
            (PathBuf::from(r"C:\gone"), Found::Missing),
            (PathBuf::from(r"\\nas\asleep"), Found::Unknown("The network path was not found.".into())),
            (PathBuf::from(r"C:\here"), Found::Folder),
            (PathBuf::from(r"C:\file.txt"), Found::File),
        ]);
        let panes = [
            (0, false, Path::new(r"C:\gone"), false),
            (0, true, Path::new(r"\\nas\asleep"), false),
            (1, false, Path::new(r"C:\here"), false),
            (2, false, Path::new(r"C:\file.txt"), false),
            (3, false, Path::new(r"C:\gone"), true),
            (4, false, Path::new(r"E:\never-checked"), false),
        ];
        assert_eq!(stale_panes(panes, &check), vec![(0, false), (2, false)]);
        // Loading a layout follows the same rule.
        let l = layout(vec![tab(r"C:\gone", None), tab(r"\\nas\asleep", None)], 0);
        let p = plan(&l, |p| !check.is_missing(p));
        assert_eq!(p.missing, vec![PathBuf::from(r"C:\gone")]);
        assert_eq!(p.tabs, vec![tab(r"\\nas\asleep", None)]);
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
        *t.view = ViewMode::Grid;
        *t.sort = Some(SortSpec {
            descending: true,
            ..SortSpec::default()
        });
        *t.right_view = Some(ViewMode::Details);
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
        assert_eq!(*l.tabs[0].view, ViewMode::Grid);
        assert_eq!(*l.tabs[0].sort, None);
        assert_eq!(*l.tabs[0].right_view, None);
        assert_eq!(*l.tabs[0].right_sort, None);
    }

    #[test]
    fn a_pending_load_is_cancelled_when_the_tabs_change() {
        let before: Workspace = (vec![tab("C:/a", None)], 0);
        assert_eq!(load_step(&before, &before.clone(), false), LoadStep::Wait);
        assert_eq!(load_step(&before, &before.clone(), true), LoadStep::Apply);
        let navigated: Workspace = (vec![tab("C:/b", None)], 0);
        assert_eq!(load_step(&before, &navigated, false), LoadStep::Cancel);
        assert_eq!(load_step(&before, &navigated, true), LoadStep::Cancel);
        let new_tab: Workspace = (vec![tab("C:/a", None), tab("C:/a", None)], 1);
        assert_eq!(load_step(&before, &new_tab, true), LoadStep::Cancel);
        let split: Workspace = (vec![tab("C:/a", Some("C:/b"))], 0);
        assert_eq!(load_step(&before, &split, true), LoadStep::Cancel);
    }
}
