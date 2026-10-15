//! Navigation sidebar: quick access (known folders and pins), collections, network
//! locations, drives.

pub mod network;
mod pins;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use egui::{RichText, Sense, Ui};
use parking_lot::Mutex;

use crate::app::ZxrawApp;
use crate::modal::{Modal, Prompt, PromptKind};
use crate::settings::Pin;
use crate::theme;
use crate::views::DragPaths;
use forge_core::model::EntryKind;
use forge_core::netpath::is_network_path;
use forge_core::paths::paths_equal;

use pins::{PinDrag, PinStatus, PinWatch, add_pins, move_pin, remove_pin, set_label};

#[derive(Clone, Debug)]
pub struct Drive {
    pub root: PathBuf,
    pub label: String,
    pub kind: &'static str,
    pub total: u64,
    pub free: u64,
    pub available: bool,
    /// The share a mapped network drive points at, from Windows' table of connections
    /// (looked up on the drives thread).
    pub remote: Option<PathBuf>,
}

pub struct SidebarState {
    pub drives: Arc<Mutex<Vec<Drive>>>,
    pub known: Vec<(String, PathBuf)>,
    pub cards: CardWatch,
    /// Existence checks for Quick access pins.
    pub pins: PinWatch,
    /// Saved network locations: status checks, the add dialog, "Connect…".
    pub network: network::NetworkState,
}

/// Watches removable drives for inserted cards and decides which ones get an import prompt.
/// The suppression rules live here as plain bookkeeping so they can be tested without the OS;
/// the DCIM/PRIVATE check itself runs on a worker thread (see [`CardWatch::tick`]).
pub struct CardWatch {
    /// Removable roots seen on the previous observation.
    present: HashSet<PathBuf>,
    /// Roots the user dismissed ("Not now" or imported). Cleared when the card is removed.
    dismissed: HashSet<PathBuf>,
    /// Cards whose camera-folder check is running.
    pending: HashMap<PathBuf, (Drive, u64)>,
    next_generation: u64,
    results_tx: Sender<(PathBuf, u64, bool)>,
    results_rx: Receiver<(PathBuf, u64, bool)>,
    /// Cards that have a camera folder and are waiting for an answer.
    pub prompts: Vec<Drive>,
}

impl Default for CardWatch {
    fn default() -> Self {
        let (results_tx, results_rx) = mpsc::channel();
        Self {
            present: HashSet::new(),
            dismissed: HashSet::new(),
            pending: HashMap::new(),
            next_generation: 0,
            results_tx,
            results_rx,
            prompts: Vec::new(),
        }
    }
}

impl CardWatch {
    /// Feed the current removable, available drives. Returns the cards that just arrived and
    /// need a camera-folder check. Removal forgets the card, so re-inserting it prompts again.
    fn observe(&mut self, cards: &[Drive]) -> Vec<Drive> {
        let current: HashSet<PathBuf> = cards.iter().map(|d| d.root.clone()).collect();
        for gone in self.present.difference(&current) {
            self.dismissed.remove(gone);
            self.pending.remove(gone);
            self.prompts.retain(|p| &p.root != gone);
        }
        let arrived = cards
            .iter()
            .filter(|d| !self.present.contains(&d.root))
            .cloned()
            .collect::<Vec<_>>();
        for card in &arrived {
            self.next_generation = self.next_generation.wrapping_add(1);
            self.pending
                .insert(card.root.clone(), (card.clone(), self.next_generation));
        }
        self.present = current;
        arrived
    }

    /// Record a finished check. Only a card that is still present, has a camera folder and was
    /// not dismissed gets a prompt; results for removed cards are dropped.
    fn finish(&mut self, root: &Path, generation: u64, has_camera_folder: bool) {
        if !self
            .pending
            .get(root)
            .is_some_and(|(_, pending_generation)| *pending_generation == generation)
        {
            return;
        }
        let Some((card, _)) = self.pending.remove(root) else {
            return;
        };
        if has_camera_folder && self.present.contains(root) && !self.dismissed.contains(root) {
            self.prompts.push(card);
        }
    }

    /// Hide the prompt for `root` and stay quiet about it until the card is removed.
    pub fn dismiss(&mut self, root: &Path) {
        self.prompts.retain(|p| p.root != root);
        self.dismissed.insert(root.to_path_buf());
    }

    /// Start checks for newly arrived cards and apply finished ones. Never blocks: the
    /// directory reads happen on short-lived worker threads.
    pub fn tick(&mut self, drives: &[Drive], ctx: &egui::Context) {
        let cards: Vec<Drive> = drives
            .iter()
            .filter(|d| d.kind == "Removable" && d.available)
            .cloned()
            .collect();
        for card in self.observe(&cards) {
            let tx = self.results_tx.clone();
            let repaint = ctx.clone();
            let root = card.root.clone();
            let generation = self
                .pending
                .get(&root)
                .map(|(_, generation)| *generation)
                .expect("arrived card has a pending generation");
            let spawned = std::thread::Builder::new()
                .name("card-check".into())
                .spawn(move || {
                    let _ = tx.send((root.clone(), generation, has_camera_folder(&root)));
                    repaint.request_repaint();
                });
            if spawned.is_err() {
                self.pending.remove(&card.root);
            }
        }
        while let Ok((root, generation, has_camera_folder)) = self.results_rx.try_recv() {
            self.finish(&root, generation, has_camera_folder);
        }
    }
}

/// True when the card root holds a DCIM or PRIVATE folder (the usual camera layout).
pub fn has_camera_folder(root: &Path) -> bool {
    ["DCIM", "PRIVATE"]
        .iter()
        .any(|name| crate::ingest_ui::find_child_directory(root, name).is_some())
}

impl SidebarState {
    pub fn new(ctx: egui::Context) -> Self {
        let drives: Arc<Mutex<Vec<Drive>>> = Arc::new(Mutex::new(Vec::new()));
        let d = drives.clone();
        let repaint = ctx.clone();
        // Drive enumeration can stall on disconnected network drives; it lives on its own
        // thread and refreshes periodically to pick up inserted cards / USB drives.
        let _ = std::thread::Builder::new()
            .name("drives".into())
            .spawn(move || {
                loop {
                    let list = crate::platform::drives();
                    // A timed-out drive query leaves its worker thread blocked in the OS;
                    // back off so a dead network share can't accumulate threads.
                    let any_hung = list.iter().any(|d| !d.available && d.kind == "Network");
                    let changed = {
                        let mut g = d.lock();
                        let changed = g.len() != list.len()
                            || g.iter().zip(&list).any(|(a, b)| {
                                a.root != b.root || a.available != b.available || a.free != b.free
                            });
                        *g = list;
                        changed
                    };
                    if changed {
                        ctx.request_repaint();
                    }
                    std::thread::sleep(Duration::from_secs(if any_hung { 60 } else { 3 }));
                }
            });
        Self {
            drives,
            known: crate::platform::known_folders(),
            cards: CardWatch::default(),
            pins: PinWatch::default(),
            network: network::NetworkState::new(repaint),
        }
    }
}

fn nav_item(
    app: &mut ZxrawApp,
    ui: &mut Ui,
    icon: &str,
    label: &str,
    path: &PathBuf,
    sub: Option<String>,
) -> egui::Response {
    let current = &app.pane().path == path;
    let text = match &sub {
        Some(s) => format!("{icon}  {label}   {s}"),
        None => format!("{icon}  {label}"),
    };
    // The current folder gets a quiet fill and an accent bar rather than the full selection
    // colour, so the sidebar stays calm next to the file list's selection.
    let button = if current {
        egui::Button::new(text)
            .selected(true)
            .fill(theme::palette(ui.ctx()).nav_selected)
            .stroke(egui::Stroke::NONE)
    } else {
        egui::Button::new(text)
            .selected(false)
            .frame_when_inactive(false)
    };
    let r = ui.add(button.min_size(egui::vec2(ui.available_width(), 0.0)));
    if current {
        theme::paint_indicator(ui, r.rect, true);
    }
    let r = r.on_hover_text(path.to_string_lossy());
    if r.clicked() {
        app.navigate(path.clone());
    }
    if r.middle_clicked() {
        app.run(crate::commands::Cmd::NewTab, &ui.ctx().clone());
        app.navigate(path.clone());
    }
    // Sidebar entries accept drops (copy/move into that folder).
    if let Some(p) = r.dnd_release_payload::<DragPaths>() {
        let copy = ui.input(|i| i.modifiers.command);
        crate::actions::transfer(app, p.0.clone(), path.clone(), copy);
    }
    r
}

pub fn show(app: &mut ZxrawApp, ui: &mut Ui) {
    let network_roots: Vec<PathBuf> = app
        .sidebar
        .drives
        .lock()
        .iter()
        .filter(|d| d.kind == "Network")
        .map(|d| d.root.clone())
        .collect();
    app.sidebar
        .pins
        .tick(&app.settings.pins, &network_roots, ui.ctx());
    network::tick(app, ui.ctx());
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(4.0);
            quick_access_section(app, ui);

            ui.add_space(6.0);
            crate::layouts::sidebar_section(app, ui);

            ui.add_space(6.0);
            collections_section(app, ui);

            ui.add_space(6.0);
            network::section(app, ui);

            ui.add_space(6.0);
            drives_section(app, ui);
        });
}

/// Known folders, then the user's pins. Files released anywhere in this section that no
/// folder row took are pinned.
fn quick_access_section(app: &mut ZxrawApp, ui: &mut Ui) {
    let area = ui.scope(|ui| {
        section(ui, "Quick access");
        for (name, path) in app.sidebar.known.clone() {
            nav_item(app, ui, "★", &name, &path, None);
        }
        pins_list(app, ui);
    });
    let released = ui.ctx().input(|i| i.pointer.any_released());
    if released && ui.rect_contains_pointer(area.response.rect) {
        if let Some(payload) = egui::DragAndDrop::take_payload::<DragPaths>(ui.ctx()) {
            pin_dropped(app, payload.0.clone(), payload.1.clone());
        }
    }
}

/// A change requested from a pin row this frame. Applied after the list is drawn.
enum PinEdit {
    Open(usize),
    OpenFolder(usize),
    Rename(usize),
    ResetName(usize),
    /// Moves the pin at the first index to the second.
    Move(usize, usize),
    Unpin(usize),
    Locate(usize),
}

fn pins_list(app: &mut ZxrawApp, ui: &mut Ui) {
    let pins = app.settings.pins.clone();
    if pins.is_empty() {
        ui.label(RichText::new("Drop files or folders here to pin them").weak().small());
        return;
    }
    let current = app.pane().path.clone();
    let mut edit = None;
    for (i, pin) in pins.iter().enumerate() {
        let status = pin_status(app, &pin.path);
        let missing = status == PinStatus::Missing;
        let is_current = !pin.file && paths_equal(&current, &pin.path);
        let r = pin_row(ui, i, pin, status, is_current);

        if r.clicked() {
            activate_pin(app, i);
        }
        if r.middle_clicked() && !pin.file && !missing {
            app.run(crate::commands::Cmd::NewTab, &ui.ctx().clone());
            activate_pin(app, i);
        }
        // Folder pins take drops like the known folders (copy or move into that folder).
        if !pin.file && !missing {
            if let Some(p) = r.dnd_release_payload::<DragPaths>() {
                let copy = ui.input(|i| i.modifiers.command);
                crate::actions::transfer(app, p.0.clone(), pin.path.clone(), copy);
            }
        }
        if let Some(p) = r.dnd_release_payload::<PinDrag>() {
            edit = Some(PinEdit::Move(p.0, i));
        }

        r.context_menu(|ui| {
            if missing {
                if ui.button("Locate…").clicked() {
                    edit = Some(PinEdit::Locate(i));
                    ui.close();
                }
                if ui.button("Rename…").clicked() {
                    edit = Some(PinEdit::Rename(i));
                    ui.close();
                }
                if ui.button("Remove").clicked() {
                    edit = Some(PinEdit::Unpin(i));
                    ui.close();
                }
                return;
            }
            if ui.button("Open").clicked() {
                edit = Some(PinEdit::Open(i));
                ui.close();
            }
            if ui.button("Open containing folder").clicked() {
                edit = Some(PinEdit::OpenFolder(i));
                ui.close();
            }
            ui.separator();
            if ui.button("Rename…").clicked() {
                edit = Some(PinEdit::Rename(i));
                ui.close();
            }
            if pin.label.is_some() && ui.button("Reset name").clicked() {
                edit = Some(PinEdit::ResetName(i));
                ui.close();
            }
            ui.separator();
            if i > 0 && ui.button("Move up").clicked() {
                edit = Some(PinEdit::Move(i, i - 1));
                ui.close();
            }
            if i + 1 < pins.len() && ui.button("Move down").clicked() {
                edit = Some(PinEdit::Move(i, i + 1));
                ui.close();
            }
            ui.separator();
            if ui.button("Unpin").clicked() {
                edit = Some(PinEdit::Unpin(i));
                ui.close();
            }
        });
    }
    match edit {
        Some(PinEdit::Open(i)) => activate_pin(app, i),
        Some(PinEdit::OpenFolder(i)) => open_containing_folder(app, i),
        Some(PinEdit::Rename(i)) => start_rename(app, i),
        Some(PinEdit::ResetName(i)) => {
            if let Some(pin) = app.settings.pins.get_mut(i) {
                pin.label = None;
            }
        }
        Some(PinEdit::Move(from, to)) => move_pin(&mut app.settings.pins, from, to),
        Some(PinEdit::Unpin(i)) => remove_pin(&mut app.settings.pins, i),
        Some(PinEdit::Locate(i)) => locate(app, i),
        None => {}
    }
}

/// One pin row: a drag source for reordering, drawn muted when its target is missing.
fn pin_row(ui: &mut Ui, index: usize, pin: &Pin, status: PinStatus, current: bool) -> egui::Response {
    let missing = matches!(status, PinStatus::Missing | PinStatus::Unreachable);
    let icon = if pin.file { "📄" } else { "🔖" };
    let text = RichText::new(format!("{icon}  {}", pin_label(pin)));
    let text = if missing {
        text.color(theme::muted(ui))
    } else {
        text
    };
    let button = if current {
        egui::Button::new(text)
            .selected(true)
            .fill(theme::palette(ui.ctx()).nav_selected)
            .stroke(egui::Stroke::NONE)
    } else {
        egui::Button::new(text)
            .selected(false)
            .frame_when_inactive(false)
    };
    let width = ui.available_width();
    let r = ui
        .dnd_drag_source(egui::Id::new(("sidebar_pin", index)), PinDrag(index), |ui| {
            ui.add(button.min_size(egui::vec2(width, 0.0)))
        })
        .inner;
    if current {
        theme::paint_indicator(ui, r.rect, true);
    }
    let tip = match status {
        PinStatus::Missing => format!("Not found: {}", pin.path.display()),
        PinStatus::Unreachable => format!("Not reachable right now: {}", pin.path.display()),
        _ => pin.path.to_string_lossy().into_owned(),
    };
    r.on_hover_text(tip)
}

fn open_containing_folder(app: &mut ZxrawApp, index: usize) {
    let Some(pin) = app.settings.pins.get(index).cloned() else {
        return;
    };
    // Navigating to a file selects it in its folder.
    if pin.file {
        app.navigate(pin.path);
    } else if let Some(parent) = pin.path.parent() {
        app.navigate(parent.to_path_buf());
    }
}

fn start_rename(app: &mut ZxrawApp, index: usize) {
    let Some(pin) = app.settings.pins.get(index) else {
        return;
    };
    let prompt = Prompt::new(
        PromptKind::RenamePin {
            path: pin.path.clone(),
        },
        "Rename pin",
        pin.label.clone().unwrap_or_default(),
    );
    app.ops.modal = Some(Modal::Prompt(prompt));
}

/// Picks a replacement for a missing pin. The label is kept; the file or folder kind stays as
/// it was, matching the dialog that was shown.
fn locate(app: &mut ZxrawApp, index: usize) {
    let Some(pin) = app.settings.pins.get(index) else {
        return;
    };
    let file = pin.file;
    let picked = if file {
        crate::dialogs::pick_file(rfd::FileDialog::new())
    } else {
        crate::dialogs::pick_folder(rfd::FileDialog::new())
    };
    let Some(path) = picked else {
        return;
    };
    let duplicate = app
        .settings
        .pins
        .iter()
        .enumerate()
        .any(|(j, p)| j != index && paths_equal(&p.path, &path));
    if duplicate {
        app.toast("Already in Quick access");
        return;
    }
    if let Some(pin) = app.settings.pins.get_mut(index) {
        pin.path = path;
    }
}

fn collections_section(app: &mut ZxrawApp, ui: &mut Ui) {
    let collections = app.ops.collections_ui.sidebar_collections().to_vec();
    let open = collapsible_header(
        app,
        ui,
        "collections",
        "Collections",
        !collections.is_empty(),
        "Collection actions",
        |app, ui| {
            menu_explainer(
                ui,
                "A collection is a named group of files you pick from any folders in your \
                 library. Files are never moved or copied: a collection only remembers where \
                 they are. A smart collection fills itself from a search instead. Click one to \
                 open it.",
            );
            if menu_item(
                ui,
                "New collection…",
                "Create an empty collection, one from the selected files, or a smart collection \
                 that fills itself from a search",
            ) {
                crate::collections_ui::open_new_collection(app);
            }
            if menu_item(
                ui,
                "Manage collections…",
                "Open the Collections window: browse, rename, reorder and delete collections \
                 and saved searches",
            ) {
                app.ops.collections_ui.open = true;
            }
        },
    );
    if !open {
        return;
    }
    if collections.is_empty() {
        ui.label(RichText::new("No collections yet").weak().small());
        return;
    }
    for collection in collections.iter().take(8) {
        let marker = match collection.kind {
            forge_db::CollectionKind::Manual => "▤",
            forge_db::CollectionKind::Smart => "⌕",
        };
        if ui
            .selectable_label(false, format!("{marker}  {}", collection.name))
            .clicked()
        {
            crate::collections_ui::open_collection(app, collection.id);
        }
    }
    if collections.len() > 8 {
        ui.label(
            RichText::new(format!("+ {} more", collections.len() - 8))
                .weak()
                .small(),
        );
    }
}

fn drives_section(app: &mut ZxrawApp, ui: &mut Ui) {
    section(ui, "Drives");
    let drives = app.sidebar.drives.lock().clone();
    // Mapped network drives are listed in the Network section, with their status.
    for d in drives.into_iter().filter(|d| d.kind != "Network") {
        let label = if d.label.is_empty() {
            format!("{} ({})", d.kind, d.root.display())
        } else {
            format!(
                "{} ({})",
                d.label,
                d.root.display().to_string().trim_end_matches('\\')
            )
        };
        let icon = match d.kind {
            "Removable" => "💾",
            "Network" => "🖧",
            "Optical" => "💿",
            _ => "🖴",
        };
        if !d.available {
            ui.label(
                RichText::new(format!("{icon}  {label}  (unavailable)")).color(theme::muted(ui)),
            );
            continue;
        }
        nav_item(app, ui, icon, &label, &d.root, None);
        if d.total > 0 {
            let used = 1.0 - d.free as f32 / d.total as f32;
            // A slim track with an accent gradient (red when nearly full).
            let (rect, bar) = ui.allocate_exact_size(
                egui::vec2(ui.available_width() - 8.0, 4.0),
                Sense::hover(),
            );
            let painter = ui.painter();
            painter.rect_filled(rect, 2.0, ui.visuals().widgets.inactive.bg_fill);
            let filled = egui::Rect::from_min_size(
                rect.min,
                egui::vec2(rect.width() * used.clamp(0.0, 1.0), rect.height()),
            );
            let (from, to) = if used > 0.9 {
                let red = ui.visuals().error_fg_color;
                (red, red)
            } else {
                (
                    theme::accent(ui.ctx()),
                    theme::accent_gradient_end(ui.ctx()),
                )
            };
            theme::paint_hgradient(&painter.with_clip_rect(filled), rect, from, to);
            bar.on_hover_text(format!(
                "{} free of {}",
                forge_core::model::format_size(d.free),
                forge_core::model::format_size(d.total)
            ));
        }
    }
}

/// A plain section header (no arrow, no button).
pub(crate) fn section(ui: &mut Ui, title: &str) {
    header_row(ui, title, None, None);
}

/// What was clicked on a section header this frame.
pub(crate) struct Header {
    /// The title (and the disclosure arrow, when there is one).
    pub title: egui::Response,
    /// The small button at the right end of the row, when there is one.
    pub button: Option<egui::Response>,
}

/// Height of a section header row: the height of the ghost button, so headers with and without
/// a button take the same space.
const HEADER_HEIGHT: f32 = 24.0;

/// One section header row, used by every sidebar section so they line up the same way: the
/// title in small muted capitals, a disclosure arrow after it when `open` is given, and an
/// optional ghost `button` (an icon such as "+" or "⋯") at the right end. Everything is centred
/// vertically on the row. The title senses clicks; plain sections ignore them.
pub(crate) fn header_row(
    ui: &mut Ui,
    title: &str,
    open: Option<bool>,
    button: Option<&str>,
) -> Header {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(
        egui::vec2(width, HEADER_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(egui::vec2(width, HEADER_HEIGHT));
            let mut title = ui.add(
                egui::Label::new(
                    RichText::new(title.to_uppercase())
                        .small()
                        .strong()
                        .color(theme::muted(ui)),
                )
                .selectable(false)
                .sense(Sense::click()),
            );
            if let Some(open) = open {
                let (_, arrow) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::click());
                egui::collapsing_header::paint_default_icon(
                    ui,
                    if open { 1.0 } else { 0.0 },
                    &arrow,
                );
                title = title.union(arrow);
                title = title.on_hover_cursor(egui::CursorIcon::PointingHand);
            }
            let button = button.map(|icon| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(theme::ghost_button(icon))
                })
                .inner
            });
            Header { title, button }
        },
    )
    .inner
}

/// Whether the collapsible section `key` is shown open: the user's last choice for it, else
/// open exactly when it has items (so an empty section starts collapsed).
pub(crate) fn section_open(app: &ZxrawApp, key: &str, has_items: bool) -> bool {
    app.settings
        .sidebar_sections
        .get(key)
        .copied()
        .unwrap_or(has_items)
}

/// A collapsible section header with a "⋯" menu button. A click on the title or arrow toggles
/// the section and remembers the choice in the settings; the button opens a popup menu drawn
/// by `menu`. Returns whether the section is open.
pub(crate) fn collapsible_header(
    app: &mut ZxrawApp,
    ui: &mut Ui,
    key: &str,
    title: &str,
    has_items: bool,
    menu_hover: &str,
    menu: impl FnOnce(&mut ZxrawApp, &mut Ui),
) -> bool {
    let open = section_open(app, key, has_items);
    let header = header_row(ui, title, Some(open), Some("⋯"));
    let header_title = header.title.on_hover_text(if open {
        "Click to collapse"
    } else {
        "Click to expand"
    });
    crate::testhook::note_control_rect(ui, &format!("sidebar-{key}"), header_title.rect);
    let open = if header_title.clicked() {
        app.settings.sidebar_sections.insert(key.to_string(), !open);
        !open
    } else {
        open
    };
    if let Some(button) = header.button {
        crate::testhook::note_control_rect(ui, &format!("sidebar-{key}-menu"), button.rect);
        let button = button.on_hover_text(menu_hover);
        egui::Popup::menu(&button).show(|ui| {
            ui.set_max_width(260.0);
            menu(app, ui);
        });
    }
    open
}

/// One action in a section's menu, with its tooltip. Returns whether it was clicked (the menu
/// closes then).
pub(crate) fn menu_item(ui: &mut Ui, label: &str, hover: &str) -> bool {
    let response = ui.button(label);
    crate::testhook::note_menu_rect(ui, label, response.rect);
    let clicked = response.on_hover_text(hover).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

/// The short explanation at the top of a section's menu.
pub(crate) fn menu_explainer(ui: &mut Ui, text: &str) {
    // Menus lay out justified, which would spread the words of a wrapped label apart.
    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.add(egui::Label::new(RichText::new(text).weak().small()).wrap());
    });
    ui.separator();
}

/// The name shown for a pin: its label, else the file or folder name, else the full path.
pub(crate) fn pin_label(pin: &Pin) -> String {
    if let Some(label) = pin
        .label
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        return label.to_string();
    }
    pin.path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| pin.path.to_string_lossy().into_owned())
}

/// Last known existence status of a pinned path (`Unknown` until its first check).
pub(crate) fn pin_status(app: &ZxrawApp, path: &Path) -> PinStatus {
    app.sidebar.pins.status(path)
}

/// What a click on pin `index` does. A missing target is not opened; the user is told instead.
pub(crate) fn activate_pin(app: &mut ZxrawApp, index: usize) {
    let Some(pin) = app.settings.pins.get(index).cloned() else {
        return;
    };
    if pin_status(app, &pin.path) == PinStatus::Missing {
        app.toast(format!("{} is not available", pin_label(&pin)));
        return;
    }
    if pin.file {
        crate::actions::open_file(app, &pin.path);
    } else {
        app.navigate(pin.path);
    }
}

/// The kind a path has in either pane's listing, `None` if neither pane lists it. No disk call.
fn listed_kind(app: &ZxrawApp, path: &Path) -> Option<EntryKind> {
    let tab = app.tab();
    std::iter::once(tab.focused())
        .chain(tab.other())
        .flat_map(|pane| pane.entries.iter())
        .find(|e| e.path.as_path() == path)
        .map(|e| e.kind)
}

/// Whether a pinned path is a file. `carried` is the kind the pane listed when the drag
/// started, `listed` the kind in the current listing. A folder pin is made for an unknown
/// kind on a network path, so `local_probe` (a disk call) only runs for local paths.
fn pin_is_file(
    carried: Option<EntryKind>,
    listed: Option<EntryKind>,
    network: bool,
    local_probe: impl FnOnce() -> bool,
) -> bool {
    match carried.or(listed) {
        Some(kind) => kind == EntryKind::File,
        // A folder pin that is really a file still works: activating it falls back to
        // opening the file's location.
        None if network => false,
        None => local_probe(),
    }
}

/// Files or folders released on the Quick access section, with the kind each had in the
/// pane listing when the drag started.
fn pin_dropped(app: &mut ZxrawApp, paths: Vec<PathBuf>, kinds: Vec<Option<EntryKind>>) {
    let items: Vec<(PathBuf, bool)> = paths
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let carried = kinds.get(i).copied().flatten();
            let listed = listed_kind(app, &p);
            let network = is_network_path(&p);
            let file = pin_is_file(carried, listed, network, || !p.is_dir());
            (p, file)
        })
        .collect();
    pin_paths(app, items);
}

/// Pins paths to Quick access; `true` marks a file. Already pinned paths are skipped.
pub fn pin_paths(app: &mut ZxrawApp, paths: Vec<(PathBuf, bool)>) {
    if paths.is_empty() {
        return;
    }
    if add_pins(&mut app.settings.pins, paths) > 0 {
        app.toast("Pinned to Quick access");
    } else {
        app.toast("Already in Quick access");
    }
}

/// `PinToQuickAccess`: pins the selected entries, or the current folder if none.
pub fn pin_selection(app: &mut ZxrawApp) {
    let (targets, kinds, folder) = {
        let pane = app.pane();
        let targets = pane.action_targets();
        let kinds = pane.kinds_of(&targets);
        (targets, kinds, pane.path.clone())
    };
    let items: Vec<(PathBuf, bool)> = if targets.is_empty() {
        vec![(folder, false)]
    } else {
        targets
            .into_iter()
            .zip(kinds)
            .map(|(p, kind)| {
                let network = is_network_path(&p);
                let file = pin_is_file(None, kind, network, || !p.is_dir());
                (p, file)
            })
            .collect()
    };
    pin_paths(app, items);
}

/// Applies an accepted `RenamePin` prompt. Blank text clears the label.
pub fn accept_label(app: &mut ZxrawApp, p: Prompt) -> Option<Modal> {
    if let PromptKind::RenamePin { path } = &p.kind {
        set_label(&mut app.settings.pins, path, &p.text);
    }
    None
}

#[cfg(test)]
mod card_tests {
    use super::*;

    fn card(root: &str) -> Drive {
        Drive {
            root: PathBuf::from(root),
            label: "CARD".into(),
            kind: "Removable",
            total: 0,
            free: 0,
            available: true,
            remote: None,
        }
    }

    fn roots(cards: &[Drive]) -> Vec<PathBuf> {
        cards.iter().map(|c| c.root.clone()).collect()
    }

    fn finish_current(watch: &mut CardWatch, root: &Path, has_camera_folder: bool) {
        let generation = watch.pending.get(root).unwrap().1;
        watch.finish(root, generation, has_camera_folder);
    }

    #[test]
    fn arrival_requests_one_check() {
        let mut watch = CardWatch::default();
        let e = card(r"E:\");
        assert_eq!(
            roots(&watch.observe(std::slice::from_ref(&e))),
            vec![e.root.clone()]
        );
        assert!(watch.observe(std::slice::from_ref(&e)).is_empty());
    }

    #[test]
    fn camera_folder_prompts_and_plain_card_does_not() {
        let mut watch = CardWatch::default();
        let (e, f) = (card(r"E:\"), card(r"F:\"));
        watch.observe(&[e.clone(), f.clone()]);
        finish_current(&mut watch, &e.root, true);
        finish_current(&mut watch, &f.root, false);
        assert_eq!(roots(&watch.prompts), vec![e.root]);
    }

    #[test]
    fn dismissed_card_stays_quiet_until_removed() {
        let mut watch = CardWatch::default();
        let e = card(r"E:\");
        watch.observe(std::slice::from_ref(&e));
        finish_current(&mut watch, &e.root, true);
        watch.dismiss(&e.root);
        assert!(watch.prompts.is_empty());

        // Still inserted: no new check, so no prompt even if a late result arrives.
        assert!(watch.observe(std::slice::from_ref(&e)).is_empty());
        assert!(watch.prompts.is_empty());

        // Removed, then inserted again: it is a new card and prompts again.
        assert!(watch.observe(&[]).is_empty());
        assert_eq!(
            roots(&watch.observe(std::slice::from_ref(&e))),
            vec![e.root.clone()]
        );
        finish_current(&mut watch, &e.root, true);
        assert_eq!(roots(&watch.prompts), vec![e.root]);
    }

    #[test]
    fn removed_and_reinserted_card_discards_the_old_generation() {
        let mut watch = CardWatch::default();
        let e = card(r"E:\");
        watch.observe(std::slice::from_ref(&e));
        let old_generation = watch.pending.get(&e.root).unwrap().1;

        watch.observe(&[]);
        watch.observe(std::slice::from_ref(&e));
        let new_generation = watch.pending.get(&e.root).unwrap().1;
        assert_ne!(new_generation, old_generation);

        watch.finish(&e.root, old_generation, true);
        assert!(watch.prompts.is_empty());
        assert_eq!(watch.pending.get(&e.root).unwrap().1, new_generation);

        watch.finish(&e.root, new_generation, true);
        assert_eq!(roots(&watch.prompts), vec![e.root]);
    }

    #[test]
    fn removal_before_check_finishes_drops_the_prompt() {
        let mut watch = CardWatch::default();
        let e = card(r"E:\");
        watch.observe(std::slice::from_ref(&e));
        let generation = watch.pending.get(&e.root).unwrap().1;
        assert!(watch.observe(&[]).is_empty());
        watch.finish(&e.root, generation, true);
        assert!(watch.prompts.is_empty());
    }

    #[test]
    fn removal_clears_an_open_prompt() {
        let mut watch = CardWatch::default();
        let e = card(r"E:\");
        watch.observe(std::slice::from_ref(&e));
        finish_current(&mut watch, &e.root, true);
        assert_eq!(watch.prompts.len(), 1);
        watch.observe(&[]);
        assert!(watch.prompts.is_empty());
    }

    #[test]
    fn dismissing_one_card_leaves_others_prompting() {
        let mut watch = CardWatch::default();
        let (e, f) = (card(r"E:\"), card(r"F:\"));
        watch.observe(&[e.clone(), f.clone()]);
        finish_current(&mut watch, &e.root, true);
        finish_current(&mut watch, &f.root, true);
        watch.dismiss(&e.root);
        assert_eq!(roots(&watch.prompts), vec![f.root]);
    }
}

#[cfg(test)]
mod pin_tests {
    use super::*;

    #[test]
    fn carried_kind_decides_without_probing() {
        let never = || -> bool { panic!("disk probed") };
        assert!(pin_is_file(Some(EntryKind::File), None, false, never));
        assert!(!pin_is_file(Some(EntryKind::Dir), None, false, never));
        assert!(!pin_is_file(Some(EntryKind::Link), None, false, never));
        // A carried kind wins over the current listing.
        assert!(!pin_is_file(
            Some(EntryKind::Dir),
            Some(EntryKind::File),
            false,
            never
        ));
    }

    #[test]
    fn listed_kind_is_used_when_nothing_was_carried() {
        let never = || -> bool { panic!("disk probed") };
        assert!(pin_is_file(None, Some(EntryKind::File), false, never));
        assert!(!pin_is_file(None, Some(EntryKind::Dir), true, never));
    }

    #[test]
    fn network_path_with_unknown_kind_never_probes_the_disk() {
        let pinned_as_file = pin_is_file(None, None, true, || -> bool {
            panic!("network path must not be probed")
        });
        assert!(!pinned_as_file);
    }

    #[test]
    fn local_path_with_unknown_kind_asks_the_disk() {
        assert!(pin_is_file(None, None, false, || true));
        assert!(!pin_is_file(None, None, false, || false));
    }
}
