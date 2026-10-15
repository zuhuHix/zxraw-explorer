//! The sidebar's Network section: saved network locations (`\\server\share[\folder]`) and
//! mapped network drives, each with a status dot.
//!
//! Status comes from checks on the shared probe service's network lane (see `path_probe`),
//! scheduled by a [`PinWatch`] of its own: the UI thread never touches a share. Locations are
//! re-checked every 30 s and when the section header is clicked.
//!
//! Signing in is left to Windows: "Connect…" calls `WNetAddConnection3W` on a worker thread
//! and Windows shows its own credential dialog. The app never asks for, sees or stores a
//! password.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{Key, RichText, Sense, Ui};
use forge_core::netpath;
use forge_core::paths::paths_equal;
use forge_win::network::Connect;

use super::pins::PinWatch;
use crate::app::ZxrawApp;
use crate::modal::{Modal, Prompt, PromptKind};
use crate::path_probe::Found;
use crate::settings::NetworkLocation;
use crate::theme;

/// What the Network section shows for a location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetStatus {
    /// No check has answered yet.
    Checking,
    Online,
    /// The share answered, but the folder inside it is not there.
    NotFound,
    /// The server refused the current user (no or wrong credentials).
    NeedsSignIn(String),
    /// The server or share did not answer, or answered with a network error.
    Offline(String),
}

impl NetStatus {
    /// Short words for tooltips and test scripts.
    pub fn word(&self) -> &'static str {
        match self {
            Self::Checking => "checking",
            Self::Online => "online",
            Self::NotFound => "notfound",
            Self::NeedsSignIn(_) => "signin",
            Self::Offline(_) => "offline",
        }
    }

    /// Connecting may help: the share wants a sign-in, or it is unreachable (a share
    /// that dropped its connection after the PC slept answers "network path not found").
    fn can_connect(&self) -> bool {
        matches!(self, Self::NeedsSignIn(_) | Self::Offline(_))
    }
}

/// Turns the last check of a location into its status. A check still running after the
/// watch's timeout counts as offline, even if an earlier check had found the share.
pub fn classify(found: Option<&Found>, timed_out: bool) -> NetStatus {
    if timed_out {
        return NetStatus::Offline("It did not answer in time.".into());
    }
    match found {
        None => NetStatus::Checking,
        Some(Found::Folder | Found::File) => NetStatus::Online,
        Some(Found::Missing) => NetStatus::NotFound,
        Some(Found::Unknown(f)) if f.code.is_some_and(netpath::is_login_error_code) => {
            NetStatus::NeedsSignIn(f.message.clone())
        }
        Some(Found::Unknown(f)) => NetStatus::Offline(f.message.clone()),
    }
}

/// One row of the section.
#[derive(Debug, Clone)]
pub struct Entry {
    /// What is checked and opened: the saved path, or the drive root.
    pub path: PathBuf,
    pub label: String,
    /// Index in `Settings::network_locations`; `None` for a mapped drive.
    pub saved: Option<usize>,
}

/// State of the "Add network location" dialog.
#[derive(Default)]
pub struct AddDialog {
    pub path: String,
    pub name: String,
    pub error: Option<String>,
    focused: bool,
}

/// Network section state, owned by `SidebarState`.
pub struct NetworkState {
    watch: PinWatch,
    pub dialog: Option<AddDialog>,
    /// Share roots with a "Connect…" in progress.
    connecting: HashSet<PathBuf>,
    connect_tx: Sender<(PathBuf, Result<Connect, String>)>,
    connect_rx: Receiver<(PathBuf, Result<Connect, String>)>,
}

impl Default for NetworkState {
    fn default() -> Self {
        let (connect_tx, connect_rx) = unbounded();
        Self {
            watch: PinWatch::default(),
            dialog: None,
            connecting: HashSet::new(),
            connect_tx,
            connect_rx,
        }
    }
}

impl NetworkState {
    pub fn status(&self, path: &Path) -> NetStatus {
        classify(self.watch.found(path), self.watch.timed_out(path))
    }
}

/// The name shown for a saved location: its name, else "folder on server".
pub fn location_label(location: &NetworkLocation) -> String {
    if let Some(name) = location
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        return name.to_string();
    }
    default_label(&location.path)
}

fn default_label(path: &Path) -> String {
    // `Path::file_name` is `None` for a share root (`\\nas\photos` is all prefix).
    let text = path.to_string_lossy();
    let folder = text.split(['\\', '/']).rfind(|c| !c.is_empty());
    match (folder, netpath::unc_parts(path)) {
        (Some(folder), Some((server, _))) => format!("{folder} on {server}"),
        _ => text.into_owned(),
    }
}

fn network_drives(app: &ZxrawApp) -> Vec<crate::sidebar::Drive> {
    app.sidebar
        .drives
        .lock()
        .iter()
        .filter(|d| d.kind == "Network")
        .cloned()
        .collect()
}

/// Saved locations, then mapped network drives.
pub fn entries(app: &ZxrawApp) -> Vec<Entry> {
    let mut out: Vec<Entry> = app
        .settings
        .network_locations
        .iter()
        .enumerate()
        .map(|(i, l)| Entry {
            path: l.path.clone(),
            label: location_label(l),
            saved: Some(i),
        })
        .collect();
    for d in network_drives(app) {
        let letter = d.root.display().to_string();
        let letter = letter.trim_end_matches('\\');
        let label = if d.label.is_empty() {
            format!("Network drive ({letter})")
        } else {
            format!("{} ({letter})", d.label)
        };
        out.push(Entry {
            path: d.root,
            label,
            saved: None,
        });
    }
    out
}

/// Schedules due checks and applies finished checks and connections. Never blocks.
pub fn tick(app: &mut ZxrawApp, ctx: &egui::Context) {
    let drives: Vec<PathBuf> = network_drives(app).into_iter().map(|d| d.root).collect();
    let paths: Vec<PathBuf> = app
        .settings
        .network_locations
        .iter()
        .map(|l| l.path.clone())
        .chain(drives.iter().cloned())
        .collect();
    app.sidebar.network.watch.tick_paths(&paths, &drives, Some(ctx));
    while let Ok((path, result)) = app.sidebar.network.connect_rx.try_recv() {
        let share = netpath::share_root(&path).unwrap_or_else(|| path.clone());
        app.sidebar.network.connecting.remove(&share);
        match result {
            Ok(Connect::Connected) => {
                app.sidebar.network.watch.recheck(&path);
                app.toast(format!("Connected to {}", share.display()));
            }
            Ok(Connect::Cancelled) => {}
            Err(e) => app.toast_err(format!("Could not connect to {}: {e}", share.display())),
        }
    }
}

/// Checks every entry again on the next tick.
fn recheck_all(app: &mut ZxrawApp) {
    for e in entries(app) {
        app.sidebar.network.watch.recheck(&e.path);
    }
}

pub fn status_of(app: &ZxrawApp, path: &Path) -> NetStatus {
    app.sidebar.network.status(path)
}

/// A change requested from a row this frame. Applied after the list is drawn.
enum Edit {
    Open(usize),
    Connect(usize),
    Recheck(usize),
    Rename(usize),
    Remove(usize),
}

/// The section: a header that re-checks on click, then one row per entry.
pub fn section(app: &mut ZxrawApp, ui: &mut Ui) {
    let header = ui
        .horizontal(|ui| {
            let title = ui
                .add(
                    egui::Label::new(
                        RichText::new("NETWORK")
                            .small()
                            .strong()
                            .color(theme::muted(ui)),
                    )
                    .sense(Sense::click()),
                )
                .on_hover_text("Click to check the network locations again");
            let add = ui
                .add(theme::ghost_button("+"))
                .on_hover_text("Add network location…");
            (title.clicked(), add.clicked())
        })
        .inner;
    if header.0 {
        recheck_all(app);
    }
    if header.1 {
        open_add_dialog(app, String::new());
    }
    let list = entries(app);
    if list.is_empty() {
        ui.label(
            RichText::new("Add a NAS or shared folder with +")
                .weak()
                .small(),
        );
        return;
    }
    let current = app.pane().path.clone();
    let mut edit = None;
    for (i, entry) in list.iter().enumerate() {
        let status = status_of(app, &entry.path);
        let connecting = is_connecting(app, &entry.path);
        let r = row(ui, entry, &status, connecting, paths_equal(&current, &entry.path));
        if r.clicked() {
            edit = Some(Edit::Open(i));
        }
        r.context_menu(|ui| {
            if ui.button("Open").clicked() {
                edit = Some(Edit::Open(i));
                ui.close();
            }
            if status.can_connect()
                && !connecting
                && ui.button("Connect…").on_hover_text("Sign in with Windows").clicked()
            {
                edit = Some(Edit::Connect(i));
                ui.close();
            }
            if ui.button("Check again").clicked() {
                edit = Some(Edit::Recheck(i));
                ui.close();
            }
            if entry.saved.is_some() {
                ui.separator();
                if ui.button("Rename…").clicked() {
                    edit = Some(Edit::Rename(i));
                    ui.close();
                }
                if ui.button("Remove").clicked() {
                    edit = Some(Edit::Remove(i));
                    ui.close();
                }
            }
        });
    }
    let Some(edit) = edit else {
        return;
    };
    let index = match edit {
        Edit::Open(i) | Edit::Connect(i) | Edit::Recheck(i) | Edit::Rename(i) | Edit::Remove(i) => i,
    };
    let Some(entry) = list.get(index).cloned() else {
        return;
    };
    match edit {
        Edit::Open(_) => activate(app, &entry),
        Edit::Connect(_) => start_connect(app, &entry.path),
        Edit::Recheck(_) => app.sidebar.network.watch.recheck(&entry.path),
        Edit::Rename(_) => start_rename(app, &entry),
        Edit::Remove(_) => {
            if let Some(i) = entry.saved {
                remove_location(app, i);
            }
        }
    }
}

fn is_connecting(app: &ZxrawApp, path: &Path) -> bool {
    let share = netpath::share_root(path).unwrap_or_else(|| path.to_path_buf());
    app.sidebar.network.connecting.contains(&share)
}

/// One row: a status dot, then the label.
fn row(ui: &mut Ui, entry: &Entry, status: &NetStatus, connecting: bool, current: bool) -> egui::Response {
    let icon = if entry.saved.is_some() { "🖧" } else { "🖴" };
    let text = RichText::new(format!("   {icon}  {}", entry.label));
    let text = match status {
        NetStatus::Online | NetStatus::Checking => text,
        _ => text.color(theme::muted(ui)),
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
    let r = ui.add(button.min_size(egui::vec2(ui.available_width(), 0.0)));
    if current {
        theme::paint_indicator(ui, r.rect, true);
    }
    let colour = match status {
        NetStatus::Online => egui::Color32::from_rgb(90, 180, 100),
        NetStatus::NeedsSignIn(_) => egui::Color32::from_rgb(230, 190, 60),
        NetStatus::Offline(_) | NetStatus::NotFound => egui::Color32::from_rgb(224, 82, 82),
        NetStatus::Checking => theme::muted(ui),
    };
    let centre = egui::pos2(r.rect.left() + 9.0, r.rect.center().y);
    if matches!(status, NetStatus::Checking) || connecting {
        ui.painter()
            .circle_stroke(centre, 3.5, egui::Stroke::new(1.2, colour));
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    } else {
        ui.painter().circle_filled(centre, 3.5, colour);
    }
    let state = if connecting {
        "Connecting…".to_string()
    } else {
        match status {
            NetStatus::Checking => "Checking…".into(),
            NetStatus::Online => "Online".into(),
            NetStatus::NotFound => "Online, but this folder was not found".into(),
            NetStatus::NeedsSignIn(_) => "Needs sign-in: click to connect".into(),
            NetStatus::Offline(why) => format!("Offline or unreachable: {why}"),
        }
    };
    r.on_hover_text(format!("{}\n{state}", entry.path.display()))
}

/// What a click on an entry does. Only a location that answered (or is still being checked)
/// is opened; the folder listing runs off the UI thread. An offline one says so instead.
pub fn activate(app: &mut ZxrawApp, entry: &Entry) {
    match status_of(app, &entry.path) {
        NetStatus::Online | NetStatus::Checking => {
            app.pane_mut().navigate(entry.path.clone());
        }
        NetStatus::NotFound => {
            app.toast_err(format!(
                "{} was not found on the server. It may have been moved or renamed.",
                entry.label
            ));
            app.sidebar.network.watch.recheck(&entry.path);
        }
        NetStatus::NeedsSignIn(_) => {
            app.toast(format!("{} needs you to sign in", entry.label));
            start_connect(app, &entry.path);
        }
        NetStatus::Offline(why) => {
            app.toast_err(format!(
                "{} is offline or unreachable ({why}). Check that it is turned on and on \
                 this network, or right-click it and choose Connect…",
                entry.label
            ));
            app.sidebar.network.watch.recheck(&entry.path);
        }
    }
}

/// Starts "Connect…" for `path` on a worker thread. Windows shows its own sign-in dialog,
/// owned by the app window. A second request for the same share while one runs is ignored.
pub fn start_connect(app: &mut ZxrawApp, path: &Path) {
    let share = netpath::share_root(path).unwrap_or_else(|| path.to_path_buf());
    if !app.sidebar.network.connecting.insert(share.clone()) {
        return;
    }
    let owner = forge_win::dragout::active_window().unwrap_or(0);
    let tx = app.sidebar.network.connect_tx.clone();
    let path = path.to_path_buf();
    let repaint = app.sidebar.ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("net-connect".into())
        .spawn(move || {
            let result = remote_of(&path)
                .ok_or_else(|| "this drive is not connected to a network share".to_string())
                .and_then(|remote| {
                    forge_win::network::connect_share(owner, &remote).map_err(|e| match e {
                        forge_win::WinError::Api { message, .. } => message,
                        other => other.to_string(),
                    })
                });
            let _ = tx.send((path, result));
            repaint.request_repaint();
        });
    if spawned.is_err() {
        app.sidebar.network.connecting.remove(&share);
        app.toast_err("Could not start connecting");
    }
}

/// The share to connect for `path`: its UNC share root, or the share a mapped drive points
/// at (looked up on the worker thread; Windows answers from its own table).
fn remote_of(path: &Path) -> Option<PathBuf> {
    if let Some(root) = netpath::share_root(path) {
        return Some(root);
    }
    let letter = path.to_string_lossy().chars().next()?;
    forge_win::network::mapped_share(letter)
}

fn start_rename(app: &mut ZxrawApp, entry: &Entry) {
    let Some(location) = entry.saved.and_then(|i| app.settings.network_locations.get(i)) else {
        return;
    };
    let prompt = Prompt::new(
        PromptKind::RenameNetworkLocation {
            path: location.path.clone(),
        },
        "Rename network location",
        location_label(location),
    );
    app.ops.modal = Some(Modal::Prompt(prompt));
}

/// Applies an accepted `RenameNetworkLocation` prompt. Blank text goes back to the default
/// name.
pub fn accept_label(app: &mut ZxrawApp, p: Prompt) -> Option<Modal> {
    if let PromptKind::RenameNetworkLocation { path } = &p.kind {
        rename_location(&mut app.settings.network_locations, path, &p.text);
    }
    None
}

/// Sets the name of the saved location at `path`. Blank text (or the default name) clears
/// it. Returns false when no location matches.
pub fn rename_location(locations: &mut [NetworkLocation], path: &Path, text: &str) -> bool {
    let Some(location) = locations.iter_mut().find(|l| paths_equal(&l.path, path)) else {
        return false;
    };
    let text = text.trim();
    location.name =
        (!text.is_empty() && text != default_label(&location.path)).then(|| text.to_string());
    true
}

pub fn remove_location(app: &mut ZxrawApp, index: usize) {
    if index < app.settings.network_locations.len() {
        app.settings.network_locations.remove(index);
    }
}

/// Validates the typed path and name and saves the location. Returns the error to show in
/// the dialog otherwise.
pub fn add_location(
    locations: &mut Vec<NetworkLocation>,
    path: &str,
    name: &str,
) -> Result<PathBuf, String> {
    let path = netpath::validate_share_path(path)?;
    if locations.iter().any(|l| paths_equal(&l.path, &path)) {
        return Err("This location is already in Network.".into());
    }
    let name = name.trim();
    locations.push(NetworkLocation {
        name: (!name.is_empty() && name != default_label(&path)).then(|| name.to_string()),
        path: path.clone(),
        extra: Default::default(),
    });
    Ok(path)
}

/// Opens the "Add network location" dialog with `path` filled in.
pub fn open_add_dialog(app: &mut ZxrawApp, path: String) {
    let name = netpath::validate_share_path(&path)
        .map(|p| default_label(&p))
        .unwrap_or_default();
    app.sidebar.network.dialog = Some(AddDialog {
        path,
        name,
        ..Default::default()
    });
}

/// `AddToNetwork`: the selected folder if it is on a share, else the current folder if it
/// is, else an empty dialog.
pub fn add_from_selection(app: &mut ZxrawApp) {
    let pane = app.pane();
    let selected = pane.action_targets();
    let candidate = match selected.as_slice() {
        [one] if netpath::is_unc(one) && is_listed_folder(app, one) => Some(one.clone()),
        _ => netpath::is_unc(&pane.path).then(|| pane.path.clone()),
    };
    let text = candidate
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    open_add_dialog(app, text);
}

/// Whether `path` is listed as a folder in the focused pane (no disk access).
fn is_listed_folder(app: &ZxrawApp, path: &Path) -> bool {
    app.pane()
        .entries
        .iter()
        .any(|e| e.path == path && e.kind == forge_core::model::EntryKind::Dir)
}

/// Accepts the add dialog: saves the location or keeps the dialog open with an error.
pub fn accept_dialog(app: &mut ZxrawApp) -> Result<(), String> {
    let Some(dialog) = app.sidebar.network.dialog.as_mut() else {
        return Err("the dialog is not open".into());
    };
    match add_location(&mut app.settings.network_locations, &dialog.path, &dialog.name) {
        Ok(_) => {
            app.sidebar.network.dialog = None;
            app.toast("Added to Network");
            Ok(())
        }
        Err(e) => {
            dialog.error = Some(e.clone());
            Err(e)
        }
    }
}

/// Draws the "Add network location" dialog when it is open. Nothing here contacts the
/// server: the path is checked lexically and its status shows once saved.
pub fn dialog(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(dialog) = app.sidebar.network.dialog.as_mut() else {
        return;
    };
    let enter = ctx.input(|i| i.key_pressed(Key::Enter));
    let esc = ctx.input(|i| i.key_pressed(Key::Escape));
    let mut accept = false;
    let mut cancel = false;
    let resp = egui::Modal::new(egui::Id::new("network-add")).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.heading("Add network location");
        ui.add_space(6.0);
        ui.label("Folder on a NAS or shared computer");
        let r = ui.add(
            egui::TextEdit::singleline(&mut dialog.path)
                .hint_text(r"\\nas\photos")
                .desired_width(f32::INFINITY),
        );
        if !dialog.focused {
            r.request_focus();
            dialog.focused = true;
        }
        if r.changed() {
            dialog.error = None;
            if let Ok(p) = netpath::validate_share_path(&dialog.path) {
                dialog.name = default_label(&p);
            }
        }
        ui.add_space(4.0);
        ui.label("Name");
        ui.add(egui::TextEdit::singleline(&mut dialog.name).desired_width(f32::INFINITY));
        ui.label(
            RichText::new(
                "If the share needs a password, Windows asks for it when you connect. \
                 ZXraw Explorer never sees or stores it.",
            )
            .weak()
            .small(),
        );
        if let Some(e) = &dialog.error {
            ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Add").clicked() || (enter && !dialog.path.trim().is_empty()) {
                accept = true;
            }
            if ui.button("Cancel").clicked() || esc {
                cancel = true;
            }
        });
    });
    if cancel || (resp.should_close() && !accept) {
        app.sidebar.network.dialog = None;
    } else if accept {
        let _ = accept_dialog(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_probe::{Client, Failure, Service};
    use std::time::{Duration, Instant};

    #[test]
    fn statuses_follow_the_last_check() {
        assert_eq!(classify(None, false), NetStatus::Checking);
        assert_eq!(classify(Some(&Found::Folder), false), NetStatus::Online);
        assert_eq!(classify(Some(&Found::Missing), false), NetStatus::NotFound);
        let failed = |code| {
            Found::Unknown(Failure {
                message: "x".into(),
                code: Some(code),
            })
        };
        for code in [5, 86, 1326, 1244] {
            assert_eq!(classify(Some(&failed(code)), false), NetStatus::NeedsSignIn("x".into()), "{code}");
        }
        for code in [53, 64, 67, 121, 1231] {
            assert_eq!(classify(Some(&failed(code)), false), NetStatus::Offline("x".into()), "{code}");
        }
        assert!(matches!(classify(Some(&Found::Unknown("odd".into())), false), NetStatus::Offline(_)));
        // A hung check is offline even if the share answered before.
        assert!(matches!(classify(Some(&Found::Folder), true), NetStatus::Offline(_)));
        assert!(matches!(classify(None, true), NetStatus::Offline(_)));
    }

    /// `\\login\…` refuses the user, `\\down\…` is unreachable, `\\hung\…` never answers in
    /// time, everything else is there.
    fn fake_check(path: &Path) -> Found {
        let text = path.to_string_lossy();
        let failed = |code| {
            Found::Unknown(Failure {
                message: std::io::Error::from_raw_os_error(code).to_string(),
                code: Some(code),
            })
        };
        if text.starts_with(r"\\login") {
            failed(1326)
        } else if text.starts_with(r"\\down") {
            failed(53)
        } else if text.starts_with(r"\\hung") {
            std::thread::sleep(Duration::from_secs(60));
            failed(121)
        } else {
            Found::Folder
        }
    }

    #[test]
    fn statuses_come_from_the_injected_probe() {
        let service = Service::new(1, 3, fake_check);
        let mut watch = PinWatch::with_client(Client::on(&service, None));
        let paths: Vec<PathBuf> = [r"\\nas\photos", r"\\login\share", r"\\down\share", r"\\hung\share"]
            .map(PathBuf::from)
            .to_vec();
        let status = |watch: &PinWatch, p: &str| {
            let p = Path::new(p);
            classify(watch.found(p), watch.timed_out(p))
        };
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            watch.tick_paths(&paths, &[], None);
            let done = [r"\\nas\photos", r"\\login\share", r"\\down\share"]
                .iter()
                .all(|p| status(&watch, p) != NetStatus::Checking);
            if done || Instant::now() > until {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(status(&watch, r"\\nas\photos"), NetStatus::Online);
        assert!(matches!(status(&watch, r"\\login\share"), NetStatus::NeedsSignIn(_)));
        assert!(matches!(status(&watch, r"\\down\share"), NetStatus::Offline(_)));
        // Still running, not yet past the timeout.
        assert_eq!(status(&watch, r"\\hung\share"), NetStatus::Checking);
    }

    #[test]
    fn locations_are_added_validated_renamed_and_deduplicated() {
        let mut locations = Vec::new();
        assert_eq!(
            add_location(&mut locations, "//nas/photos/", "").unwrap(),
            PathBuf::from(r"\\nas\photos")
        );
        assert_eq!(location_label(&locations[0]), "photos on nas");
        assert!(add_location(&mut locations, r"\\NAS\Photos", "x").is_err(), "duplicate");
        assert!(add_location(&mut locations, r"\\nas", "x").is_err(), "no share");
        assert!(add_location(&mut locations, r"C:\photos", "x").is_err(), "not a share");
        add_location(&mut locations, r"\\192.168.1.10\share\sub", " Studio ").unwrap();
        assert_eq!(location_label(&locations[1]), "Studio");

        assert!(rename_location(&mut locations, Path::new(r"\\nas\photos"), "Archive"));
        assert_eq!(location_label(&locations[0]), "Archive");
        assert!(rename_location(&mut locations, Path::new(r"\\nas\photos"), "  "));
        assert_eq!(locations[0].name, None);
        assert!(!rename_location(&mut locations, Path::new(r"\\other\x"), "y"));
    }
}
