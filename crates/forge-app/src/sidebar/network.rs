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

use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{Key, RichText, Ui};
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

/// Shown for a share that is not mounted on Linux (see [`unmounted_if_missing`]).
const NOT_MOUNTED: &str = "not mounted, or this folder is not on the share. Right-click it and \
    choose Connect… to mount the share";

/// On Linux a location is checked at its GVfs mount folder, which exists only while the share
/// is mounted. Without a mount the check finds nothing there and reports the folder as missing
/// (the volume root `/` still answers), so "not found" here means the share may not be
/// connected.
fn unmounted_if_missing(status: NetStatus) -> NetStatus {
    match status {
        NetStatus::NotFound if !cfg!(windows) => NetStatus::Offline(NOT_MOUNTED.into()),
        other => other,
    }
}

/// Whether two paths name the same location. Server and share names are case-insensitive on
/// both systems; Windows compares paths as it always did.
fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        paths_equal(a, b)
    } else {
        a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
    }
}

/// The path a saved location is checked and opened at. On Linux that is its GVfs mount folder;
/// without `XDG_RUNTIME_DIR` there is none, and the saved path is used as it is.
#[cfg(target_os = "linux")]
pub fn local_path(saved: &Path) -> PathBuf {
    runtime_dir()
        .and_then(|runtime| gvfs::mount_path(&runtime, saved))
        .unwrap_or_else(|| saved.to_path_buf())
}

#[cfg(not(target_os = "linux"))]
pub fn local_path(saved: &Path) -> PathBuf {
    saved.to_path_buf()
}

/// The `\\server\share[\folder]` path of a folder in a listing, for "Add to Network". On Linux
/// a folder under a GVfs mount folder maps back to its share.
#[cfg(target_os = "linux")]
fn share_path_of(path: &Path) -> Option<PathBuf> {
    runtime_dir()
        .and_then(|runtime| gvfs::share_path(&runtime, path))
        .or_else(|| netpath::is_unc(path).then(|| path.to_path_buf()))
}

#[cfg(not(target_os = "linux"))]
fn share_path_of(path: &Path) -> Option<PathBuf> {
    netpath::is_unc(path).then(|| path.to_path_buf())
}

/// Adds the GVfs folder to the network roots on Linux: checks under it can wait on a hung
/// share, so they go to the network lane (see `path_probe::is_network`).
#[cfg(target_os = "linux")]
fn with_mount_root(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    if let Some(runtime) = runtime_dir() {
        roots.push(runtime.join(gvfs::DIR));
    }
    roots
}

#[cfg(not(target_os = "linux"))]
fn with_mount_root(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots
}

#[cfg(target_os = "linux")]
fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// What "Connect…" does, for its tooltip.
fn connect_hint() -> &'static str {
    if cfg!(windows) {
        "Sign in with Windows"
    } else {
        "Mount the share (GVfs)"
    }
}

/// Linux: where GVfs mounts SMB shares. `connect_share` mounts with `gio mount` and does not
/// return the folder, so it is rebuilt here as `$XDG_RUNTIME_DIR/gvfs/smb-share:server=HOST,
/// share=NAME`. Pure path mapping: nothing is read from disk.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod gvfs {
    use std::path::{Path, PathBuf};

    /// The folder under `$XDG_RUNTIME_DIR` that GVfs mounts shares in.
    pub const DIR: &str = "gvfs";

    /// `\\server\share\a\b` as (`server`, `share`, `["a", "b"]`). Forward slashes count as
    /// separators and empty parts are skipped. `None` unless a server and a share are there.
    fn split(saved: &Path) -> Option<(String, String, Vec<String>)> {
        let text = saved.to_string_lossy().replace('/', "\\");
        let rest = text.strip_prefix(r"\\")?;
        let mut parts = rest
            .split('\\')
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        let server = parts.next()?;
        let share = parts.next()?;
        Some((server, share, parts.collect()))
    }

    /// The mount folder of the saved path: `\\nas\photos\2024` becomes
    /// `<runtime>/gvfs/smb-share:server=nas,share=photos/2024`.
    pub fn mount_path(runtime: &Path, saved: &Path) -> Option<PathBuf> {
        let (server, share, rest) = split(saved)?;
        let mut path = runtime
            .join(DIR)
            .join(format!("smb-share:server={server},share={share}"));
        path.extend(rest);
        Some(path)
    }

    /// The saved path of a folder under a mount folder (the inverse of [`mount_path`]). `None`
    /// for anything else, including the GVfs folder itself.
    pub fn share_path(runtime: &Path, mount: &Path) -> Option<PathBuf> {
        let rel = mount.strip_prefix(runtime.join(DIR)).ok()?;
        let parts: Vec<&str> = rel.iter().map(|p| p.to_str()).collect::<Option<_>>()?;
        let (first, rest) = parts.split_first()?;
        let names = first.strip_prefix("smb-share:server=")?;
        let (server, share) = names.split_once(",share=")?;
        if server.is_empty() || share.is_empty() || rest.iter().any(|p| matches!(*p, "." | "..")) {
            return None;
        }
        let mut text = format!(r"\\{server}\{share}");
        for part in rest {
            text.push('\\');
            text.push_str(part);
        }
        Some(PathBuf::from(text))
    }
}

/// One row of the section.
#[derive(Debug, Clone)]
pub struct Entry {
    /// What is checked and opened: the saved path (on Linux its GVfs mount folder), or the
    /// drive root.
    pub path: PathBuf,
    pub label: String,
    /// Index in `Settings::network_locations`; `None` for a mapped drive.
    pub saved: Option<usize>,
    /// The share "Connect…" signs in to (`\\server\share`). A mapped drive and a saved
    /// location on the same share have the same one. `None` for a drive whose share is
    /// unknown.
    pub share: Option<PathBuf>,
}

/// Signs in to one share; blocks until the user answers Windows' dialog.
pub type ConnectFn = fn(isize, &Path) -> Result<Connect, String>;

fn connect_with_windows(owner: isize, share: &Path) -> Result<Connect, String> {
    forge_win::network::connect_share(owner, share).map_err(|e| match e {
        forge_win::WinError::Api { message, .. } => message,
        other => other.to_string(),
    })
}

/// What [`Connector::request`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Started,
    /// This share's sign-in is already on screen.
    AlreadyRunning,
    /// Another share's sign-in is on screen; only one runs at a time.
    Busy(PathBuf),
}

/// Runs "Connect…" requests on one long-lived worker, one at a time: at most one Windows
/// sign-in dialog is open and at most one thread can be blocked in `WNetAddConnection3W`.
/// Requests are keyed by share, so a mapped drive and a saved location on the same share
/// don't open two dialogs.
pub struct Connector {
    connect: ConnectFn,
    repaint: Option<egui::Context>,
    /// Started on the first request.
    jobs: Option<Sender<(isize, PathBuf)>>,
    results_tx: Sender<(PathBuf, Result<Connect, String>)>,
    results: Receiver<(PathBuf, Result<Connect, String>)>,
    busy: Option<PathBuf>,
}

impl Connector {
    pub fn new(connect: ConnectFn, repaint: Option<egui::Context>) -> Self {
        let (results_tx, results) = unbounded();
        Self {
            connect,
            repaint,
            jobs: None,
            results_tx,
            results,
            busy: None,
        }
    }

    /// The share whose sign-in is in progress.
    pub fn busy(&self) -> Option<&Path> {
        self.busy.as_deref()
    }

    /// Starts signing in to `share` unless a sign-in is already running.
    pub fn request(&mut self, owner: isize, share: PathBuf) -> Request {
        if let Some(busy) = &self.busy {
            return if same_path(busy, &share) {
                Request::AlreadyRunning
            } else {
                Request::Busy(busy.clone())
            };
        }
        if self.jobs.is_none() {
            let (tx, rx) = unbounded::<(isize, PathBuf)>();
            let connect = self.connect;
            let results = self.results_tx.clone();
            let repaint = self.repaint.clone();
            let spawned = std::thread::Builder::new()
                .name("net-connect".into())
                .spawn(move || {
                    for (owner, share) in rx {
                        let result = connect(owner, &share);
                        let _ = results.send((share, result));
                        if let Some(ctx) = &repaint {
                            ctx.request_repaint();
                        }
                    }
                });
            if spawned.is_err() {
                return Request::Busy(share);
            }
            self.jobs = Some(tx);
        }
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send((owner, share.clone()));
        }
        self.busy = Some(share);
        Request::Started
    }

    /// A finished sign-in, if one finished.
    pub fn poll(&mut self) -> Option<(PathBuf, Result<Connect, String>)> {
        let done = self.results.try_recv().ok()?;
        self.busy = None;
        Some(done)
    }
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
    connector: Connector,
}

impl NetworkState {
    pub fn new(repaint: egui::Context) -> Self {
        Self {
            watch: PinWatch::default(),
            dialog: None,
            connector: Connector::new(connect_with_windows, Some(repaint)),
        }
    }

    pub fn status(&self, path: &Path) -> NetStatus {
        unmounted_if_missing(classify(self.watch.found(path), self.watch.stalled(path)))
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
            path: local_path(&l.path),
            label: location_label(l),
            saved: Some(i),
            share: netpath::share_root(&l.path),
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
            share: d.remote.as_deref().and_then(netpath::share_root),
        });
    }
    out
}

/// Schedules due checks and applies finished checks and connections. Never blocks.
pub fn tick(app: &mut ZxrawApp, ctx: &egui::Context) {
    let drives: Vec<PathBuf> = network_drives(app).into_iter().map(|d| d.root).collect();
    let roots = with_mount_root(drives.clone());
    let paths: Vec<PathBuf> = app
        .settings
        .network_locations
        .iter()
        .map(|l| local_path(&l.path))
        .chain(drives.iter().cloned())
        .collect();
    app.sidebar.network.watch.tick_paths(&paths, &roots, Some(ctx));
    while let Some((share, result)) = app.sidebar.network.connector.poll() {
        match result {
            Ok(Connect::Connected) => {
                // Every entry on this share: saved locations and mapped drives alike.
                for e in entries(app) {
                    if e.share.as_deref().is_some_and(|s| same_path(s, &share)) {
                        app.sidebar.network.watch.recheck(&e.path);
                    }
                }
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
    let header = crate::sidebar::header_row(ui, "Network", None, Some("+"));
    let recheck = header
        .title
        .on_hover_text("Click to check the network locations again")
        .clicked();
    let add = header
        .button
        .is_some_and(|b| b.on_hover_text("Add network location…").clicked());
    if recheck {
        recheck_all(app);
    }
    if add {
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
        let connecting = is_connecting(app, entry);
        let r = row(ui, entry, &status, connecting, same_path(&current, &entry.path));
        // A location the last check found online takes drops like the other sidebar folders.
        if matches!(status, NetStatus::Online) {
            super::folder_drop(app, ui, &r, &entry.path);
        }
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
                && ui.button("Connect…").on_hover_text(connect_hint()).clicked()
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
        Edit::Connect(_) => start_connect(app, &entry),
        Edit::Recheck(_) => app.sidebar.network.watch.recheck(&entry.path),
        Edit::Rename(_) => start_rename(app, &entry),
        Edit::Remove(_) => {
            if let Some(i) = entry.saved {
                remove_location(app, i);
            }
        }
    }
}

fn is_connecting(app: &ZxrawApp, entry: &Entry) -> bool {
    match (app.sidebar.network.connector.busy(), &entry.share) {
        (Some(busy), Some(share)) => same_path(busy, share),
        _ => false,
    }
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
            start_connect(app, entry);
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

/// Starts "Connect…" for the entry's share on the connect worker. Windows shows its own
/// sign-in dialog, owned by the app window. Only one sign-in runs at a time.
pub fn start_connect(app: &mut ZxrawApp, entry: &Entry) {
    let Some(share) = entry.share.clone() else {
        app.toast_err(format!("{} is not connected to a known network share", entry.label));
        return;
    };
    let owner = forge_win::dragout::active_window().unwrap_or(0);
    match app.sidebar.network.connector.request(owner, share) {
        Request::Started | Request::AlreadyRunning => {}
        Request::Busy(other) => app.toast(format!(
            "Finish signing in to {} first",
            other.display()
        )),
    }
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
    let Some(location) = locations.iter_mut().find(|l| same_path(&l.path, path)) else {
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
    if locations.iter().any(|l| same_path(&l.path, &path)) {
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
        [one] if is_listed_folder(app, one) && share_path_of(one).is_some() => share_path_of(one),
        _ => share_path_of(&pane.path),
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
        let hint = if cfg!(windows) {
            r"\\nas\photos"
        } else {
            "smb://nas/photos"
        };
        let r = ui.add(
            egui::TextEdit::singleline(&mut dialog.path)
                .hint_text(hint)
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
        let note = if cfg!(windows) {
            "If the share needs a password, Windows asks for it when you connect. \
             ZXraw Explorer never sees or stores it."
        } else {
            "If the share needs a password, connect to it once from the file manager. \
             ZXraw Explorer cannot ask for it and never stores it."
        };
        ui.label(RichText::new(note).weak().small());
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
        let mut watch = PinWatch::with_client(Client::on(&service, None), Duration::from_secs(30));
        let paths: Vec<PathBuf> = [r"\\nas\photos", r"\\login\share", r"\\down\share", r"\\hung\share"]
            .map(PathBuf::from)
            .to_vec();
        let status = |watch: &PinWatch, p: &str| {
            let p = Path::new(p);
            classify(watch.found(p), watch.stalled(p))
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
    fn a_location_queued_behind_hung_checks_goes_offline_at_the_deadline() {
        // Every network worker hangs on another share; the last location only ever queues.
        let service = Service::new(1, 2, fake_check);
        let mut watch = PinWatch::with_client(Client::on(&service, None), Duration::from_millis(300));
        let paths: Vec<PathBuf> = [r"\\hung\a", r"\\hung\b", r"\\nas\photos"]
            .map(PathBuf::from)
            .to_vec();
        let started = Instant::now();
        watch.tick_paths(&paths, &[], None);
        let nas = Path::new(r"\\nas\photos");
        assert_eq!(classify(watch.found(nas), watch.stalled(nas)), NetStatus::Checking);
        while started.elapsed() < Duration::from_millis(400) {
            watch.tick_paths(&paths, &[], None);
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = classify(watch.found(nas), watch.stalled(nas));
        assert!(matches!(status, NetStatus::Offline(_)), "{status:?}");
        assert!(status.can_connect(), "an offline location offers Connect…");
    }

    #[cfg(windows)]
    static CONNECT_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[cfg(windows)]
    fn slow_connect(_owner: isize, _share: &Path) -> Result<Connect, String> {
        CONNECT_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(200));
        Ok(Connect::Connected)
    }

    #[cfg(windows)]
    #[test]
    fn one_sign_in_at_a_time_and_aliases_of_a_share_share_it() {
        let mut connector = Connector::new(slow_connect, None);
        let share = PathBuf::from(r"\\nas\photos");
        assert_eq!(connector.request(0, share.clone()), Request::Started);
        // The same share through a mapped drive (its resolved share) or another case.
        assert_eq!(connector.request(0, PathBuf::from(r"\\NAS\Photos")), Request::AlreadyRunning);
        assert_eq!(
            connector.request(0, PathBuf::from(r"\\other\x")),
            Request::Busy(share.clone())
        );
        let until = Instant::now() + Duration::from_secs(10);
        let done = loop {
            if let Some(done) = connector.poll() {
                break done;
            }
            assert!(Instant::now() < until, "the sign-in never finished");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(done, (share, Ok(Connect::Connected)));
        assert_eq!(connector.busy(), None);
        assert_eq!(CONNECT_CALLS.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(connector.request(0, PathBuf::from(r"\\other\x")), Request::Started);
    }

    #[cfg(windows)]
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

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    #[test]
    fn typed_share_forms_are_stored_as_unc_paths() {
        // The same share however it is typed on Linux; the stored form is the UNC one.
        for typed in ["smb://nas/photos", "SMB://nas/photos/", "//nas/photos", r"\\nas\photos"] {
            assert_eq!(
                netpath::validate_share_path(typed),
                Ok(PathBuf::from(r"\\nas\photos")),
                "{typed}"
            );
        }
        assert!(netpath::validate_share_path("/home/user").is_err());
    }

    #[test]
    fn the_same_share_in_another_case_is_a_duplicate() {
        let mut locations = Vec::new();
        add_location(&mut locations, "smb://nas/photos", "").unwrap();
        assert!(add_location(&mut locations, "//NAS/Photos", "").is_err());
        assert!(same_path(Path::new(r"\\NAS\Photos"), Path::new(r"\\nas\photos")));
        assert!(!same_path(Path::new(r"\\nas\photos"), Path::new(r"\\nas\other")));
    }

    #[test]
    fn saved_paths_map_to_gvfs_mount_folders() {
        let runtime = Path::new("/run/user/1000");
        let mount = |saved: &str| gvfs::mount_path(runtime, Path::new(saved));
        assert_eq!(
            mount(r"\\nas\photos"),
            Some(PathBuf::from("/run/user/1000/gvfs/smb-share:server=nas,share=photos"))
        );
        assert_eq!(
            mount(r"\\nas\photos\2024\x"),
            Some(PathBuf::from(
                "/run/user/1000/gvfs/smb-share:server=nas,share=photos/2024/x"
            ))
        );
        assert_eq!(
            mount(r"\\nas\My Share\"),
            Some(PathBuf::from("/run/user/1000/gvfs/smb-share:server=nas,share=My Share"))
        );
        assert_eq!(mount("/home/user"), None);
        assert_eq!(mount(r"\\nas"), None);
    }

    #[test]
    fn mount_folders_map_back_to_saved_paths() {
        let runtime = Path::new("/run/user/1000");
        let back = |mount: &str| gvfs::share_path(runtime, Path::new(mount));
        assert_eq!(
            back("/run/user/1000/gvfs/smb-share:server=nas,share=photos/2024"),
            Some(PathBuf::from(r"\\nas\photos\2024"))
        );
        assert_eq!(
            back("/run/user/1000/gvfs/smb-share:server=nas,share=photos"),
            Some(PathBuf::from(r"\\nas\photos"))
        );
        // Not a share: the GVfs folder itself, another GVfs backend, another folder.
        assert_eq!(back("/run/user/1000/gvfs"), None);
        assert_eq!(back("/run/user/1000/gvfs/sftp:host=nas"), None);
        assert_eq!(back("/home/user/gvfs/smb-share:server=nas,share=x"), None);
        assert_eq!(back("/run/user/1000/gvfs/smb-share:server=,share=x"), None);
        assert_eq!(
            back("/run/user/1000/gvfs/smb-share:server=nas,share=x/../y"),
            None
        );
    }

    #[test]
    fn saved_paths_round_trip_through_their_mount_folder() {
        let runtime = Path::new("/run/user/1000");
        for saved in [r"\\nas\photos", r"\\192.168.1.5\C$\temp\a b"] {
            let mount = gvfs::mount_path(runtime, Path::new(saved)).unwrap();
            assert_eq!(gvfs::share_path(runtime, &mount), Some(PathBuf::from(saved)));
        }
    }

    #[test]
    fn a_share_that_is_not_mounted_is_offline_not_missing() {
        // On Linux "not found" at the mount folder means nothing is mounted there.
        assert_eq!(
            unmounted_if_missing(NetStatus::NotFound),
            NetStatus::Offline(NOT_MOUNTED.into())
        );
        assert_eq!(unmounted_if_missing(NetStatus::Online), NetStatus::Online);
        assert_eq!(unmounted_if_missing(NetStatus::Checking), NetStatus::Checking);
    }
}
