//! Application shell: tabs, panes, sidebar, toolbar, status bar, command dispatch.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Align, Key, Layout, RichText, Ui};
use forge_core::model::format_size;

use crate::commands::Cmd;
use crate::pane::{LoadState, Pane};
use crate::settings::{self, Paths, Session, Settings, TabSession, ThemeChoice, ViewMode};
use crate::theme;
use crate::views::{self, ViewEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

pub struct Tab {
    pub left: Pane,
    pub right: Option<Pane>,
    pub focus: Side,
}

impl Tab {
    pub fn focused(&self) -> &Pane {
        match (self.focus, &self.right) {
            (Side::Right, Some(r)) => r,
            _ => &self.left,
        }
    }
    pub fn focused_mut(&mut self) -> &mut Pane {
        match (self.focus, &mut self.right) {
            (Side::Right, Some(r)) => r,
            _ => &mut self.left,
        }
    }
    /// The non-focused pane in dual-pane mode.
    pub fn other(&self) -> Option<&Pane> {
        match self.focus {
            Side::Left => self.right.as_ref(),
            Side::Right => Some(&self.left),
        }
    }
    pub fn other_mut(&mut self) -> Option<&mut Pane> {
        match self.focus {
            Side::Left => self.right.as_mut(),
            Side::Right => Some(&mut self.left),
        }
    }
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    pub until: Instant,
}

pub struct ZxrawApp {
    pub paths: Paths,
    pub settings: Settings,
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub show_sidebar: bool,
    pub show_preview: bool,
    pub toasts: Vec<Toast>,
    pub palette: Option<crate::palette::Palette>,
    pub sidebar: crate::sidebar::SidebarState,
    /// "Load layout" and "Manage layouts" windows.
    pub layouts: crate::layouts::LayoutsUi,
    pub focus_address: bool,
    pub focus_filter: bool,
    pub ops: crate::actions::Actions,
    pub testhook: crate::testhook::TestHook,
    pub viewer: Option<crate::viewer::Viewer>,
    pub notices: crate::notices::Notices,
    /// A photo passed on the command line (a double-click when ZXraw Explorer is the default
    /// app): shown in the viewer once its folder has loaded.
    launch_photo: Option<PathBuf>,
    applied_theme: Option<(bool, settings::Accent, f32)>,
    /// Icon the window was opened with or last switched to (`main.rs` sets it at startup).
    applied_icon: settings::AppIcon,
    /// Whether the restored window size has been checked (see [`window_size_fix`]).
    window_size_checked: bool,
    type_ahead: (String, Instant),
    last_session_save: Instant,
    selection_summary: SelectionSummary,
}

/// The status bar's "N selected (size)". Walking every listed entry each frame cost
/// ~200 ms per frame in a 50,000-entry folder (debug build) while anything was selected,
/// so the visible entries are indexed once per listing revision and a selection change
/// costs time proportional to the selection, not the folder.
#[derive(Default)]
struct SelectionSummary {
    listing: Option<u64>,
    /// Visible entry path → size, modification time, and whether it's a video or audio file.
    index: std::collections::HashMap<PathBuf, (u64, Option<std::time::SystemTime>, bool)>,
    selection: Option<(usize, u64)>,
    count: usize,
    size: u64,
    /// Selected video and audio files, for the running-time total.
    media: Vec<(PathBuf, Option<std::time::SystemTime>)>,
    durations: crate::media_durations::MediaDurations,
}

impl SelectionSummary {
    fn get(&mut self, p: &crate::pane::Pane) -> (usize, u64) {
        let listing = p.listing_rev();
        if self.listing != Some(listing) {
            self.index = p
                .visible
                .iter()
                .map(|&i| {
                    let e = &p.entries[i];
                    let media = !e.is_dir()
                        && matches!(
                            forge_core::model::media_kind_for_ext(&e.ext),
                            forge_core::model::MediaKind::Video | forge_core::model::MediaKind::Audio
                        );
                    (e.path.clone(), (e.size, e.modified, media))
                })
                .collect();
            self.listing = Some(listing);
            self.selection = None;
        }
        // Fingerprint the selection by its length and its first 64 paths in the set's own
        // iteration order: the whole content for small selections (selecting another single
        // file keeps the length), and for large ones a same-size swap to different files
        // almost surely changes which paths come first. At most 64 hashes per frame.
        let hasher = std::hash::BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default();
        let fingerprint = p
            .selected
            .iter()
            .take(64)
            .enumerate()
            .fold(0u64, |acc, (i, path)| {
                acc.rotate_left(7) ^ std::hash::BuildHasher::hash_one(&hasher, (i, path))
            });
        let selection = (p.selected.len(), fingerprint);
        if self.selection != Some(selection) {
            let mut count = 0;
            let mut size = 0;
            self.media.clear();
            for path in &p.selected {
                if let Some(&(bytes, modified, media)) = self.index.get(path) {
                    count += 1;
                    size += bytes;
                    if media {
                        self.media.push((path.clone(), modified));
                    }
                }
            }
            self.media.sort();
            (self.count, self.size) = (count, size);
            self.selection = Some(selection);
        }
        (self.count, self.size)
    }
}

impl ZxrawApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        paths: Paths,
        pending_crash: Option<PathBuf>,
    ) -> Self {
        crate::fonts::install(&cc.egui_ctx);
        let mut settings: Settings = settings::load_json(&paths.config.join("settings.json"));
        settings.migrate();
        let session: Session = if settings.restore_session {
            settings::load_json(&paths.config.join("session.json"))
        } else {
            Session::default()
        };
        let mut app = Self {
            ops: crate::actions::Actions::new(&paths, &settings, cc.egui_ctx.clone()),
            paths,
            tabs: Vec::new(),
            active: 0,
            show_sidebar: true,
            show_preview: false,
            toasts: Vec::new(),
            palette: None,
            sidebar: crate::sidebar::SidebarState::new(cc.egui_ctx.clone()),
            layouts: Default::default(),
            focus_address: false,
            focus_filter: false,
            applied_theme: None,
            applied_icon: *settings.app_icon,
            window_size_checked: false,
            type_ahead: (String::new(), Instant::now()),
            last_session_save: Instant::now(),
            selection_summary: SelectionSummary::default(),
            testhook: crate::testhook::TestHook::from_env(),
            viewer: None,
            notices: Default::default(),
            launch_photo: None,
            settings,
        };
        app.notices = crate::notices::Notices::new(
            &app.paths.config,
            pending_crash,
            app.testhook.active(),
        );
        if app.testhook.active() {
            // Automated runs never show native dialogs or touch the real Recycle Bin.
            crate::dialogs::set_automated();
            forge_win::set_recycle_sandbox(Some(app.paths.data.join("recycle-sandbox")));
        }
        app.restore_session(session);
        if app.applied_icon != Default::default() && !app.testhook.active() {
            // Re-applied each start: reinstalling recreates the shortcuts with the default.
            crate::app_icon::apply_to_shortcuts(*app.settings.app_icon);
        }
        // `zxraw.exe <folder>` opens that folder in a new tab; `zxraw.exe <photo>` opens its
        // folder with the photo selected and shows it in the viewer.
        if let Some(arg) = std::env::args_os().nth(1) {
            let p = forge_core::paths::normalize_user_path(&arg.to_string_lossy());
            let folder = if p.is_file() { p.parent().map(Path::to_path_buf) } else { Some(p.clone()) };
            if let Some(folder) = folder.filter(|folder| folder.exists()) {
                // Reuse a restored tab already showing this folder instead of piling up tabs.
                let existing = app
                    .tabs
                    .iter()
                    .position(|t| forge_core::paths::paths_equal(&t.left.path, &folder));
                match existing {
                    Some(i) => app.active = i,
                    None => app.run(Cmd::NewTab, &cc.egui_ctx),
                }
                if p.is_file() {
                    app.tab_mut().focus = Side::Left;
                    let kind = forge_core::model::media_kind_for_ext(
                        &p.extension()
                            .map(|e| e.to_string_lossy().to_ascii_lowercase())
                            .unwrap_or_default(),
                    );
                    if matches!(kind, forge_core::MediaKind::Image | forge_core::MediaKind::Raw) {
                        app.launch_photo = Some(p.clone());
                    }
                    app.navigate(p);
                } else if existing.is_none() {
                    app.navigate(p);
                }
            }
        }
        app
    }

    fn home() -> PathBuf {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\"))
    }

    /// Folder for a new window without a restored session. Automated runs start in an
    /// empty folder inside their profile so they never list the user's real home folder.
    pub(crate) fn start_folder(&self) -> PathBuf {
        if self.testhook.active() {
            let dir = self.paths.data.join("start");
            if std::fs::create_dir_all(&dir).is_ok() {
                return dir;
            }
        }
        Self::home()
    }

    pub(crate) fn new_pane(&self, path: PathBuf, view: ViewMode) -> Pane {
        let mut p = Pane::new(path, *self.settings.default_sort, view);
        p.show_hidden = self.settings.show_hidden;
        p.show_system = self.settings.show_system;
        p
    }

    /// Builds a tab from a saved session or layout. Never touches the disk: listings load in
    /// the background, and the caller checks which folders still exist (see `layouts`).
    pub(crate) fn tab_from_session(&self, t: &TabSession) -> Tab {
        let pane = |path: &PathBuf, view: ViewMode, sort: Option<forge_core::listing::SortSpec>| {
            let mut p = self.new_pane(path.clone(), view);
            if let Some(sort) = sort {
                p.sort = sort;
            }
            p
        };
        let left = pane(&t.left, *t.view, *t.sort);
        let right = t.right.as_ref().map(|r| {
            pane(r, t.right_view.unwrap_or(*t.view), t.right_sort.or(*t.sort))
        });
        Tab {
            left,
            right,
            focus: Side::Left,
        }
    }

    fn restore_session(&mut self, s: Session) {
        // The tabs open at once; folders that have vanished or are offline are switched to the
        // start folder when the background check answers (see `layouts::tick`).
        for t in &s.tabs {
            let tab = self.tab_from_session(t);
            self.tabs.push(tab);
        }
        if self.tabs.is_empty() {
            let left = self.new_pane(self.start_folder(), *self.settings.default_view);
            self.tabs.push(Tab {
                left,
                right: None,
                focus: Side::Left,
            });
            self.show_sidebar = true;
        } else {
            self.show_sidebar = s.show_sidebar;
            self.show_preview = s.show_preview;
            crate::layouts::check_restored(self);
        }
        self.active = s.active_tab.min(self.tabs.len() - 1);
    }

    pub fn session(&self) -> Session {
        Session {
            tabs: self
                .tabs
                .iter()
                .map(|t| TabSession {
                    extra: Default::default(),
                    left: t.left.path.clone(),
                    right: t.right.as_ref().map(|r| r.path.clone()),
                    view: t.left.view.into(),
                    sort: Some(t.left.sort).into(),
                    right_view: t.right.as_ref().map(|r| r.view).into(),
                    right_sort: t.right.as_ref().map(|r| r.sort).into(),
                })
                .collect(),
            active_tab: self.active,
            show_preview: self.show_preview,
            show_sidebar: self.show_sidebar,
        }
    }

    pub fn save_all(&self) {
        if let Err(e) = settings::save_json(&self.paths.config.join("session.json"), &self.session()) {
            tracing::warn!("saving session failed: {e}");
        }
        if let Err(e) = settings::save_json(&self.paths.config.join("settings.json"), &self.settings) {
            tracing::warn!("saving settings failed: {e}");
        }
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }
    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }
    pub fn pane(&self) -> &Pane {
        self.tab().focused()
    }
    pub fn pane_mut(&mut self) -> &mut Pane {
        self.tab_mut().focused_mut()
    }

    /// Number of selected entries the focused pane still lists, from the same cache as the
    /// status bar: cheap enough to call every frame, even in a 50,000-entry folder.
    pub(crate) fn selected_listed_count(&mut self) -> usize {
        let mut summary = std::mem::take(&mut self.selection_summary);
        let count = if self.pane().selected.is_empty() {
            0
        } else {
            summary.get(self.pane()).0
        };
        self.selection_summary = summary;
        count
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.testhook.record_toast(&text, false);
        self.toasts.push(Toast {
            text,
            error: false,
            until: Instant::now() + Duration::from_secs(4),
        });
    }
    pub fn toast_err(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.testhook.record_toast(&text, true);
        self.toasts.push(Toast {
            text,
            error: true,
            until: Instant::now() + Duration::from_secs(8),
        });
    }

    pub fn navigate(&mut self, path: PathBuf) {
        let path = forge_core::paths::normalize_user_path(&path.to_string_lossy());
        if path.is_file() {
            // Navigating to a file opens its folder and selects it.
            if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                let name = name.to_string_lossy().into_owned();
                let pane = self.pane_mut();
                pane.navigate(parent.to_path_buf());
                pane.editing_path = None;
                pane.set_pending_select(name);
            }
            return;
        }
        self.pane_mut().navigate(path);
    }

    pub fn refresh_all_showing(&mut self, dir: &Path) {
        // Exports may have appeared or gone; their associations are re-scanned on demand.
        crate::export_assoc::invalidate_cache();
        for t in &mut self.tabs {
            if t.left.path == dir {
                t.left.refresh();
            }
            if let Some(r) = &mut t.right {
                if r.path == dir {
                    r.refresh();
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Command dispatch
    // -----------------------------------------------------------------------

    pub fn run(&mut self, cmd: Cmd, ctx: &egui::Context) {
        use Cmd::*;
        match cmd {
            Back => self.pane_mut().back(),
            Forward => self.pane_mut().forward(),
            Up => self.pane_mut().up(),
            Refresh => self.pane_mut().refresh(),
            Open => self.open_cursor(),
            NewTab => {
                let p = self.pane().path.clone();
                let view = self.pane().view;
                let pane = self.new_pane(p, view);
                self.tabs.insert(
                    self.active + 1,
                    Tab {
                        left: pane,
                        right: None,
                        focus: Side::Left,
                    },
                );
                self.active += 1;
            }
            CloseTab => self.close_tab(self.active),
            NextTab => self.active = (self.active + 1) % self.tabs.len(),
            PrevTab => self.active = (self.active + self.tabs.len() - 1) % self.tabs.len(),
            ToggleDualPane => {
                let (p, v) = (self.pane().path.clone(), self.pane().view);
                let fresh = self.new_pane(p, v);
                let t = self.tab_mut();
                if t.right.is_some() {
                    t.right = None;
                    t.focus = Side::Left;
                } else {
                    t.right = Some(fresh);
                }
            }
            SwitchPane => {
                let t = self.tab_mut();
                if t.right.is_some() {
                    t.focus = if t.focus == Side::Left { Side::Right } else { Side::Left };
                }
            }
            FocusAddress => self.focus_address = true,
            FocusFilter => self.focus_filter = true,
            SelectAll => self.pane_mut().select_all(),
            SelectNone => self.pane_mut().clear_selection(),
            InvertSelection => self.pane_mut().invert_selection(),
            ToggleHidden => {
                self.settings.show_hidden = !self.settings.show_hidden;
                let h = self.settings.show_hidden;
                for t in &mut self.tabs {
                    for p in std::iter::once(&mut t.left).chain(t.right.as_mut()) {
                        p.show_hidden = h;
                        p.rebuild_visible();
                    }
                }
            }
            TogglePreview => self.show_preview = !self.show_preview,
            ToggleSidebar => self.show_sidebar = !self.show_sidebar,
            ViewDetails => self.pane_mut().view = ViewMode::Details,
            ViewGrid => self.pane_mut().view = ViewMode::Grid,
            ThumbsBigger => self.settings.thumb_size = (self.settings.thumb_size * 1.2).min(512.0),
            ThumbsSmaller => self.settings.thumb_size = (self.settings.thumb_size / 1.2).max(64.0),
            CommandPalette => self.palette = Some(crate::palette::Palette::default()),
            CullMode => crate::viewer_host::open(self, crate::viewer::ViewerMode::Cull),
            CompareImages => crate::viewer_host::open(self, crate::viewer::ViewerMode::Compare),
            BookmarkFolder => {
                let p = self.pane().path.clone();
                crate::sidebar::pin_paths(self, vec![(p, false)]);
            }
            PinToQuickAccess => crate::sidebar::pin_selection(self),
            SaveLayout | LoadLayout | ManageLayouts => crate::layouts::run(self, cmd),
            CopyPath => {
                let t: Vec<String> = self
                    .pane()
                    .action_targets()
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                let text = if t.is_empty() {
                    self.pane().path.to_string_lossy().into_owned()
                } else {
                    t.join("\r\n")
                };
                ctx.copy_text(text);
                self.toast("Path copied");
            }
            ToggleTheme => {
                *self.settings.theme = match *self.settings.theme {
                    ThemeChoice::Dark => ThemeChoice::Light,
                    _ => ThemeChoice::Dark,
                };
            }
            ZoomIn => self.settings.ui_scale = (self.settings.ui_scale + 0.1).min(3.0),
            ZoomOut => self.settings.ui_scale = (self.settings.ui_scale - 0.1).max(0.5),
            ZoomReset => self.settings.ui_scale = 1.0,
            other => crate::actions::run(self, other, ctx),
        }
    }

    fn close_tab(&mut self, i: usize) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.tabs.remove(i);
        if self.active >= self.tabs.len() || self.active > i {
            self.active = self.active.saturating_sub(1);
        }
    }

    pub fn open_cursor(&mut self) {
        let Some(e) = self.pane().cursor_entry().cloned() else {
            return;
        };
        self.open_entry(&e);
    }

    pub fn open_entry(&mut self, e: &forge_core::FileEntry) {
        use forge_core::EntryKind;
        match e.kind {
            EntryKind::Dir => self.pane_mut().navigate(e.path.clone()),
            EntryKind::Link if forge_core::listing::link_targets_dir(&e.path) => {
                // Explicit user action: following a junction/symlink is fine here.
                self.pane_mut().navigate(e.path.clone())
            }
            // Photos open in the built-in viewer (Esc returns); other files use their
            // default Windows app. "Open with…" is always available for photos too.
            _ if matches!(e.media_kind(), forge_core::MediaKind::Image | forge_core::MediaKind::Raw) => {
                self.pane_mut().set_pending_select(e.name.clone());
                crate::viewer_host::open(self, crate::viewer::ViewerMode::Single);
            }
            _ => crate::actions::open_file(self, &e.path),
        }
    }

    // -----------------------------------------------------------------------
    // Keyboard
    // -----------------------------------------------------------------------

    fn handle_keys(&mut self, ctx: &egui::Context) {
        // While the settings page is capturing a new shortcut, the keys belong to it.
        if self.viewer.is_some()
            || self.palette.is_some()
            || crate::actions::modal_open(self)
            || self.ops.settings_ui.capture.is_some()
        {
            return;
        }
        let text_focus = ctx.memory(|m| m.focused().is_some());
        let mut cmds = Vec::new();
        let overrides = &self.settings.keybindings;
        ctx.input_mut(|i| {
            for &c in Cmd::ALL {
                if let Some(sc) = c.binding(overrides) {
                    // While typing, keys belong to the text field: only commands that cannot
                    // touch files or the selection are allowed through.
                    if text_focus && !allowed_while_typing(c) {
                        continue;
                    }
                    if consume_exact(i, &sc) {
                        cmds.push(c);
                    }
                }
            }
        });
        // egui translates Ctrl+C/X/V into Copy/Cut/Paste events rather than key presses.
        if !text_focus {
            ctx.input(|i| {
                for ev in &i.events {
                    match ev {
                        egui::Event::Copy => cmds.push(Cmd::Copy),
                        egui::Event::Cut => cmds.push(Cmd::Cut),
                        egui::Event::Paste(_) => cmds.push(Cmd::Paste),
                        _ => {}
                    }
                }
            });
        }
        // Ctrl+C etc. can arrive both as a key shortcut and as a clipboard event.
        let mut seen = Vec::new();
        cmds.retain(|c| {
            let dup = seen.contains(c);
            seen.push(*c);
            !dup
        });
        for c in cmds {
            self.run(c, ctx);
        }
        if text_focus {
            return;
        }

        // Cursor navigation.
        let grid_cols = if self.pane().view == ViewMode::Grid {
            views::grid_columns(ctx, self.pane_view_id()) as isize
        } else {
            1
        };
        let (m, keys, text) = ctx.input(|i| {
            let keys: Vec<Key> = [
                Key::ArrowUp,
                Key::ArrowDown,
                Key::ArrowLeft,
                Key::ArrowRight,
                Key::PageUp,
                Key::PageDown,
                Key::Home,
                Key::End,
                Key::Enter,
                Key::Backspace,
                Key::Space,
                Key::Escape,
            ]
            .into_iter()
            .filter(|k| i.key_pressed(*k))
            .collect();
            let text: String = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            (i.modifiers, keys, text)
        });
        if m.alt {
            return;
        }
        let page = 20;
        for k in keys {
            let pane = self.pane_mut();
            match k {
                Key::ArrowDown => pane.move_cursor(grid_cols, m.shift, m.command),
                Key::ArrowUp => pane.move_cursor(-grid_cols, m.shift, m.command),
                Key::ArrowRight if grid_cols > 1 => pane.move_cursor(1, m.shift, m.command),
                Key::ArrowLeft if grid_cols > 1 => pane.move_cursor(-1, m.shift, m.command),
                Key::PageDown => pane.move_cursor(page, m.shift, m.command),
                Key::PageUp => pane.move_cursor(-page, m.shift, m.command),
                Key::Home => pane.move_cursor(-(1 << 30), m.shift, m.command),
                Key::End => pane.move_cursor(1 << 30, m.shift, m.command),
                Key::Space if m.command => {
                    if let Some(c) = pane.cursor {
                        pane.click(c, true, false);
                    }
                }
                Key::Enter => self.open_cursor(),
                Key::Backspace => self.pane_mut().up(),
                Key::Escape => {
                    let p = self.pane_mut();
                    if !p.filter_text.is_empty() {
                        p.set_filter(String::new());
                    } else {
                        p.clear_selection();
                    }
                }
                _ => {}
            }
        }
        // Type-ahead find.
        let text: String = text.chars().filter(|c| !c.is_control() && *c != ' ').collect();
        if !text.is_empty() && !m.command {
            let (buf, at) = &mut self.type_ahead;
            if at.elapsed() > Duration::from_millis(900) {
                buf.clear();
            }
            buf.push_str(&text);
            *at = Instant::now();
            let prefix = buf.clone();
            self.pane_mut().type_ahead(&prefix);
        }
    }

    pub fn pane_view_id(&self) -> egui::Id {
        egui::Id::new(("pane", self.active, self.tab().focus == Side::Right))
    }

    // -----------------------------------------------------------------------
    // Layout
    // -----------------------------------------------------------------------

    fn tab_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let mut close = None;
            let mut select = None;
            for (i, t) in self.tabs.iter().enumerate() {
                let title = match &t.right {
                    Some(r) => format!("{} | {}", t.left.title(), r.title()),
                    None => t.left.title(),
                };
                let title: String = if title.chars().count() > 28 {
                    title.chars().take(27).chain(std::iter::once('…')).collect()
                } else {
                    title
                };
                let selected = i == self.active;
                // The active tab is a raised chip in the card colour with a glowing accent
                // bar beneath; the others are recessed plain text until hovered.
                let r = if selected {
                    let p = theme::palette(ui.ctx());
                    let shadow = ui.painter().add(egui::Shape::Noop);
                    let text = RichText::new(title).color(ui.visuals().strong_text_color());
                    let button = egui::Button::new(text).selected(true).fill(p.content).stroke(egui::Stroke::new(1.0, p.card_border));
                    let r = ui.add(button.corner_radius(8).min_size(egui::vec2(0.0, 26.0)));
                    let lift = egui::Shadow { offset: [0, 2], blur: 8, spread: 0, color: egui::Color32::from_black_alpha(if p.dark { 90 } else { 18 }) };
                    ui.painter().set(shadow, lift.as_shape(r.rect, egui::CornerRadius::same(8)));
                    theme::paint_indicator(ui, r.rect, false);
                    r
                } else {
                    let text = RichText::new(title).color(theme::muted(ui));
                    ui.add(egui::Button::new(text).selected(false).frame_when_inactive(false).corner_radius(8).min_size(egui::vec2(0.0, 26.0)))
                };
                if r.clicked() {
                    select = Some(i);
                }
                if r.middle_clicked() {
                    close = Some(i);
                }
                r.context_menu(|ui| {
                    if ui.button("Close tab").clicked() {
                        close = Some(i);
                        ui.close();
                    }
                });
                if selected && self.tabs.len() > 1 && ui.small_button("×").on_hover_text("Close tab").clicked() {
                    close = Some(i);
                }
            }
            if let Some(i) = select {
                self.active = i;
            }
            if let Some(i) = close {
                self.close_tab(i);
            }
            if ui.add(theme::ghost_button("+")).on_hover_text(crate::commands::tooltip("New tab", Cmd::NewTab, &self.settings.keybindings)).clicked() {
                self.run(Cmd::NewTab, &ui.ctx().clone());
            }
        });
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            let p = self.pane();
            let (cb, cf) = (p.can_back(), p.can_forward());
            ui.spacing_mut().item_spacing.x = 2.0;
            if ui.add_enabled(cb, theme::ghost_button("⏴")).on_hover_text(crate::commands::tooltip("Back", Cmd::Back, &self.settings.keybindings)).clicked() {
                self.run(Cmd::Back, &ctx);
            }
            if ui.add_enabled(cf, theme::ghost_button("⏵")).on_hover_text(crate::commands::tooltip("Forward", Cmd::Forward, &self.settings.keybindings)).clicked() {
                self.run(Cmd::Forward, &ctx);
            }
            if ui.add(theme::ghost_button("⏶")).on_hover_text(crate::commands::tooltip("Up", Cmd::Up, &self.settings.keybindings)).clicked() {
                self.run(Cmd::Up, &ctx);
            }
            if ui.add(theme::ghost_button("⟳")).on_hover_text(crate::commands::tooltip("Refresh", Cmd::Refresh, &self.settings.keybindings)).clicked() {
                self.run(Cmd::Refresh, &ctx);
            }
            ui.add_space(4.0);

            // Right-aligned controls first so the address bar gets the remaining width.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(theme::ghost_button("⚙")).on_hover_text(crate::commands::tooltip("Settings", Cmd::Settings, &self.settings.keybindings)).clicked() {
                    self.run(Cmd::Settings, &ctx);
                }
                if ui.add(theme::ghost_button("☰")).on_hover_text(crate::commands::tooltip("Command palette", Cmd::CommandPalette, &self.settings.keybindings)).clicked() {
                    self.run(Cmd::CommandPalette, &ctx);
                }
                ui.add_space(4.0);
                let view = self.pane().view;
                if theme::pill_toggle(ui, view == ViewMode::Grid, "▦").on_hover_text("Thumbnails").clicked() {
                    self.run(Cmd::ViewGrid, &ctx);
                }
                if theme::pill_toggle(ui, view == ViewMode::Details, "☷").on_hover_text("Details").clicked() {
                    self.run(Cmd::ViewDetails, &ctx);
                }
                let dual = self.tab().right.is_some();
                if theme::pill_toggle(ui, dual, "◫").on_hover_text(crate::commands::tooltip("Dual pane", Cmd::ToggleDualPane, &self.settings.keybindings)).clicked() {
                    self.run(Cmd::ToggleDualPane, &ctx);
                }
                if theme::pill_toggle(ui, self.show_preview, "▣").on_hover_text(crate::commands::tooltip("Preview panel", Cmd::TogglePreview, &self.settings.keybindings)).clicked() {
                    self.run(Cmd::TogglePreview, &ctx);
                }
                ui.add_space(6.0);
                // Filter box: a rounded pill with a search icon.
                let mut filter = self.pane().filter_text.clone();
                let err = self.pane().filter_error.clone();
                let fr = ui
                    .scope(|ui| {
                        theme::pill_field_style(ui);
                        ui.add(
                            egui::TextEdit::singleline(&mut filter)
                                .prefix(RichText::new("🔍").color(theme::muted(ui)))
                                .hint_text("Filter  (e.g. ext:cr3 size:>20mb)")
                                .background_color(theme::palette(ui.ctx()).field)
                                .margin(egui::Margin::symmetric(10, 4))
                                .desired_width(240.0),
                        )
                    })
                    .inner;
                ui.add_space(4.0);
                let fr = match &err {
                    Some(e) => fr.on_hover_text(format!("Query error — using plain text match:\n{e}")),
                    None => fr,
                };
                if self.focus_filter {
                    fr.request_focus();
                    self.focus_filter = false;
                }
                if fr.changed() {
                    self.pane_mut().set_filter(filter);
                }
                if fr.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    self.pane_mut().set_filter(String::new());
                    fr.surrender_focus();
                }
                // Address / breadcrumb takes the rest.
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    // A long path must not paint over the filter and view buttons.
                    ui.shrink_clip_rect(ui.max_rect());
                    crate::breadcrumb::address_bar(self, ui);
                });
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let mut summary = std::mem::take(&mut self.selection_summary);
        ui.horizontal(|ui| {
            let p = self.pane();
            let n = p.visible.len();
            let total = p.entries.len();
            let mut s = if n == total {
                format!("{n} items")
            } else {
                format!("{n} of {total} items")
            };
            // Count only selected entries that are still listed: a move or an outside
            // change can leave names in the selection set that are no longer there.
            if !p.selected.is_empty() {
                let (sel, size) = summary.get(p);
                if sel > 0 {
                    s += &format!("   ·   {sel} selected ({})", format_size(size));
                    if !summary.media.is_empty() {
                        let total = summary.durations.total(
                            &summary.media,
                            self.settings.ffmpeg_dir.as_deref(),
                            ui.ctx(),
                        );
                        s += &crate::media_durations::status_text(&total);
                    }
                }
            }
            ui.label(s);
            match &p.load {
                LoadState::Loading { since } if since.elapsed() > Duration::from_millis(200) => {
                    ui.spinner();
                    ui.label(RichText::new("Loading…").color(theme::muted(ui)));
                }
                LoadState::Ready { errors } if *errors > 0 => {
                    ui.label(
                        RichText::new(format!("⚠ {errors} entries could not be read"))
                            .color(ui.visuals().warn_fg_color),
                    );
                }
                LoadState::Failed(e) => {
                    ui.label(RichText::new(format!("⚠ {e}")).color(ui.visuals().error_fg_color));
                }
                _ => {}
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                crate::actions::status_widgets(self, ui);
            });
        });
        self.selection_summary = summary;
    }

    fn pane_ui(&mut self, ui: &mut Ui, side: Side) {
        let dual = self.tab().right.is_some();
        let focused = !dual || self.tab().focus == side;
        let id = egui::Id::new(("pane", self.active, side == Side::Right));
        let thumb_size = self.settings.thumb_size;
        let events = {
            let Self { tabs, active, ops, .. } = self;
            let tab = &mut tabs[*active];
            let pane = match side {
                Side::Left => &mut tab.left,
                Side::Right => tab.right.as_mut().expect("right pane"),
            };
            if dual {
                // Mini header for each pane so it's obvious which one has focus.
                let (txt, col) = if focused {
                    (pane.path.to_string_lossy().into_owned(), theme::accent(ui.ctx()))
                } else {
                    (pane.path.to_string_lossy().into_owned(), ui.visuals().weak_text_color())
                };
                ui.label(RichText::new(txt).small().color(col));
            }
            let failed = match &pane.load {
                LoadState::Failed(e) if pane.entries.is_empty() => Some(e.clone()),
                _ => None,
            };
            match failed {
                Some(e) => {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("This folder can't be opened").heading());
                        ui.label(e);
                        ui.add_space(8.0);
                        if ui.button("Retry").clicked() {
                            pane.refresh();
                        }
                    });
                    Vec::new()
                }
                None => match pane.view {
                    ViewMode::Details => views::details(ui, pane, focused, id, &ops.grid_provider()),
                    ViewMode::Grid => views::grid(ui, pane, focused, id, thumb_size, &mut ops.grid_provider()),
                },
            }
        };
        for ev in events {
            match ev {
                ViewEvent::Focus => self.tab_mut().focus = side,
                ViewEvent::Activate(vis) => {
                    self.tab_mut().focus = side;
                    if let Some(e) = self.pane().entry_at(vis).cloned() {
                        self.open_entry(&e);
                    }
                }
                ViewEvent::Command(c) => {
                    self.tab_mut().focus = side;
                    self.run(c, &ui.ctx().clone());
                }
                ViewEvent::Drop { paths, target, copy } => {
                    crate::actions::transfer(self, paths, target, copy);
                }
            }
        }
    }
}

impl eframe::App for ZxrawApp {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        crate::testhook::raw_input(self, raw_input);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        crate::commands::publish_key_state(
            ctx,
            &self.settings.keybindings,
            self.ops.settings_ui.capture.is_some(),
        );
        let mut changed = false;
        for t in &mut self.tabs {
            changed |= t.left.poll();
            if let Some(r) = &mut t.right {
                changed |= r.poll();
            }
        }
        changed |= crate::actions::poll(self);
        crate::updater::tick(self);
        crate::layouts::tick(self, ctx);
        if !self.pane().is_loading()
            && let Some(photo) = self.launch_photo.take()
        {
            crate::viewer_host::open_launched(self, photo);
        }
        if changed || self.ops.thumbs.busy() || self.tabs.iter().any(|t| t.left.is_loading()) {
            ctx.request_repaint_after(Duration::from_millis(30));
        }
        if self.last_session_save.elapsed() > Duration::from_secs(30) {
            self.save_all();
            self.last_session_save = Instant::now();
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let dark = match *self.settings.theme {
            ThemeChoice::Dark => true,
            ThemeChoice::Light => false,
            ThemeChoice::System => ctx.system_theme().is_none_or(|t| t == egui::Theme::Dark),
        };
        let look = (dark, *self.settings.accent, self.settings.ui_scale);
        if self.applied_theme != Some(look) {
            theme::apply(&ctx, dark, *self.settings.accent, self.settings.ui_scale);
            self.applied_theme = Some(look);
        }
        if !self.window_size_checked {
            let viewport = ctx.input(|i| i.viewport().clone());
            if self.testhook.active() || viewport.minimized == Some(true) {
                self.window_size_checked = true;
            } else if let (Some(inner), Some(monitor)) = (viewport.inner_rect, viewport.monitor_size) {
                self.window_size_checked = true;
                if let Some(size) = window_size_fix(inner.size(), monitor) {
                    tracing::info!("restored window was {:?}; resizing to {size:?}", inner.size());
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                        ((monitor - size) / 2.0).to_pos2(),
                    ));
                }
            }
        }
        let icon = *self.settings.app_icon;
        if self.applied_icon != icon {
            self.applied_icon = icon;
            crate::app_icon::apply_to_window(&ctx, icon);
            if !self.testhook.active() {
                crate::app_icon::apply_to_shortcuts(icon);
            }
        }

        // Files dropped from other applications go into the focused pane's folder.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if !dropped.is_empty() {
            let target = self.pane().path.clone();
            crate::actions::transfer(self, dropped, target, true);
        }

        self.handle_keys(&ctx);
        crate::dragout::tick(self, &ctx);

        if crate::viewer_host::show(self, ui) {
            crate::actions::modals(self, ui);
            // The palette must be drawn here too: an open palette blocks the viewer's keys,
            // so if it were not shown it could never be closed.
            self.palette_ui(&ctx);
            self.toasts_ui(&ctx);
            crate::testhook::tick(self, &ctx);
            return;
        }
        let title = format!("{} — ZXraw Explorer", self.pane().path.display());
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));

        // The tab strip, toolbar, sidebar and status bar are one continuous chrome surface;
        // the listing and the preview float on it as raised cards.
        egui::Panel::top("tabs")
            .frame(theme::chrome_frame(&ctx, egui::Margin { left: 8, right: 8, top: 6, bottom: 2 }))
            .show_separator_line(false)
            .show(ui, |ui| self.tab_bar(ui));
        egui::Panel::top("toolbar")
            .frame(theme::chrome_frame(&ctx, egui::Margin::symmetric(8, 5)))
            .show_separator_line(false)
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status")
            .frame(theme::chrome_frame(&ctx, egui::Margin { left: 12, right: 12, top: 0, bottom: 4 }))
            .show_separator_line(false)
            .show(ui, |ui| self.status_bar(ui));
        if self.show_sidebar {
            egui::Panel::left("sidebar")
                .frame(theme::sidebar_frame(&ctx))
                .show_separator_line(false)
                .resizable(true)
                .default_size(210.0)
                .size_range(140.0..=420.0)
                .show(ui, |ui| crate::sidebar::show(self, ui));
        }
        if self.show_preview {
            egui::Panel::right("preview")
                .frame(theme::content_side_frame(&ctx))
                .show_separator_line(false)
                .resizable(true)
                .default_size(340.0)
                .size_range(220.0..=900.0)
                .show(ui, |ui| theme::card(ui, false, |ui| crate::actions::preview_panel(self, ui)));
        }
        egui::CentralPanel::default().frame(theme::content_frame(&ctx)).show(ui, |ui| {
            if self.tab().right.is_some() {
                let gap = 8.0;
                let w = ui.available_width();
                let h = ui.available_height();
                let focus = self.tab().focus;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    ui.allocate_ui_with_layout(egui::vec2((w - gap) / 2.0, h), Layout::top_down(Align::Min), |ui| {
                        ui.set_min_size(egui::vec2((w - gap) / 2.0, h));
                        theme::card(ui, focus == Side::Left, |ui| self.pane_ui(ui, Side::Left));
                    });
                    ui.allocate_ui_with_layout(ui.available_size(), Layout::top_down(Align::Min), |ui| {
                        theme::card(ui, focus == Side::Right, |ui| self.pane_ui(ui, Side::Right));
                    });
                });
            } else {
                theme::card(ui, false, |ui| self.pane_ui(ui, Side::Left));
            }
        });

        crate::layouts::show(self, &ctx);
        crate::actions::modals(self, ui);
        crate::notices::show(self, &ctx);
        self.palette_ui(&ctx);
        self.toasts_ui(&ctx);
        crate::testhook::tick(self, &ctx);
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.save_all();
    }

    fn on_exit(&mut self) {
        self.save_all();
        crate::actions::shutdown(self);
    }
}

impl ZxrawApp {
    fn palette_ui(&mut self, ctx: &egui::Context) {
        if let Some(mut pal) = self.palette.take() {
            if let Some(cmd) = pal.show(ctx, &self.settings.keybindings) {
                self.run(cmd, ctx);
            } else if pal.open {
                self.palette = Some(pal);
            }
        }
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.until > Instant::now());
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -36.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                for t in self.toasts.iter().rev().take(5) {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(420.0);
                        let col = if t.error { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                        ui.label(RichText::new(&t.text).color(col));
                    });
                }
            });
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

/// Commands that stay active while a text field has keyboard focus. Everything else
/// (ratings, delete, copy/paste of files, selection) would act on the file list behind the
/// field the user is typing in.
fn allowed_while_typing(c: Cmd) -> bool {
    matches!(
        c,
        Cmd::CommandPalette
            | Cmd::NewTab
            | Cmd::CloseTab
            | Cmd::NextTab
            | Cmd::PrevTab
            | Cmd::Refresh
            | Cmd::FocusAddress
            | Cmd::FocusFilter
            | Cmd::Search
            | Cmd::Settings
            | Cmd::ShowJobs
            | Cmd::ToggleDualPane
            | Cmd::TogglePreview
            | Cmd::ZoomIn
            | Cmd::ZoomOut
            | Cmd::ZoomReset
    )
}

/// Like `InputState::consume_shortcut`, but Shift/Alt must match exactly: egui's logical
/// matching would let Shift+F6 (move) trigger F6 (copy), or Shift+Delete trigger Delete.
/// Keys that need Shift on common layouts (`+`) keep logical matching.
fn consume_exact(i: &mut egui::InputState, sc: &egui::KeyboardShortcut) -> bool {
    if matches!(sc.logical_key, Key::Plus | Key::Equals | Key::Minus) {
        return i.consume_shortcut(sc);
    }
    let mut hit = false;
    i.events.retain(|e| match e {
        egui::Event::Key { key, pressed: true, modifiers, .. }
            if !hit && *key == sc.logical_key && modifiers.matches_exact(sc.modifiers) =>
        {
            hit = true;
            false
        }
        _ => true,
    });
    hit
}

#[cfg(test)]
mod key_tests {
    use super::*;

    fn press(key: Key, modifiers: egui::Modifiers) -> egui::InputState {
        let mut i = egui::InputState::default();
        i.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        });
        i.modifiers = modifiers;
        i
    }

    #[test]
    fn shift_variants_do_not_trigger_plain_shortcuts() {
        let copy = Cmd::CopyToOtherPane.shortcut().unwrap();
        let mv = Cmd::MoveToOtherPane.shortcut().unwrap();
        let mut i = press(Key::F6, egui::Modifiers::SHIFT);
        assert!(!consume_exact(&mut i, &copy));
        assert!(consume_exact(&mut i, &mv));

        let recycle = Cmd::Recycle.shortcut().unwrap();
        let perm = Cmd::DeletePermanently.shortcut().unwrap();
        let mut i = press(Key::Delete, egui::Modifiers::SHIFT);
        assert!(!consume_exact(&mut i, &recycle));
        assert!(consume_exact(&mut i, &perm));

        let reject = Cmd::MarkReject.shortcut().unwrap();
        let mut i = press(Key::Delete, egui::Modifiers::COMMAND);
        assert!(!consume_exact(&mut i, &recycle));
        assert!(consume_exact(&mut i, &reject));
    }

    #[test]
    fn file_commands_are_blocked_while_typing() {
        for c in [Cmd::Rate1, Cmd::Recycle, Cmd::DeletePermanently, Cmd::Copy, Cmd::Paste, Cmd::SelectAll, Cmd::MarkReject, Cmd::Undo, Cmd::Rename] {
            assert!(!allowed_while_typing(c), "{c:?} must not fire while typing");
        }
        assert!(allowed_while_typing(Cmd::CommandPalette));
    }

    #[test]
    fn shortcuts_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in Cmd::ALL {
            if let Some(sc) = c.shortcut() {
                assert!(seen.insert((sc.logical_key, sc.modifiers.shift, sc.modifiers.alt, sc.modifiers.command)), "duplicate shortcut for {c:?}");
            }
        }
    }
}

#[cfg(test)]
mod selection_summary_tests {
    use super::*;
    use forge_core::listing::SortSpec;
    use forge_core::model::{EntryKind, FileEntry};

    fn file(dir: &Path, name: &str, size: u64) -> FileEntry {
        FileEntry {
            path: dir.join(name),
            name: name.into(),
            kind: EntryKind::File,
            size,
            modified: None,
            created: None,
            hidden: false,
            readonly: false,
            system: false,
            online_only: false,
            ext: "txt".into(),
        }
    }

    fn pane(names: &[(&str, u64)]) -> Pane {
        let dir = Path::new(r"C:\fixture");
        let mut p = Pane::new(dir.to_path_buf(), SortSpec::default(), ViewMode::Details);
        p.entries = names.iter().map(|(n, s)| file(dir, n, *s)).collect();
        p.load = LoadState::Ready { errors: 0 };
        p.rebuild_visible();
        p
    }

    #[test]
    fn counts_only_selected_entries_that_are_still_listed() {
        let mut p = pane(&[("a.txt", 10), ("b.txt", 20)]);
        p.selected.insert(p.path.join("a.txt"));
        p.selected.insert(p.path.join("gone.txt"));
        let mut summary = SelectionSummary::default();
        assert_eq!(summary.get(&p), (1, 10));
    }

    #[test]
    fn follows_selection_changes_of_the_same_length_and_listing_changes() {
        let mut p = pane(&[("a.txt", 10), ("b.txt", 20)]);
        let mut summary = SelectionSummary::default();
        p.selected.insert(p.path.join("a.txt"));
        assert_eq!(summary.get(&p), (1, 10));
        // Another single file: same selection length, different content.
        p.selected.clear();
        p.selected.insert(p.path.join("b.txt"));
        assert_eq!(summary.get(&p), (1, 20));
        // The selected file disappears from the listing.
        p.entries.retain(|e| e.name != "b.txt");
        p.rebuild_visible();
        assert_eq!(summary.get(&p), (0, 0));
    }

    #[test]
    fn a_same_size_swap_of_a_large_selection_is_noticed() {
        let names: Vec<(String, u64)> = (0..600).map(|i| (format!("f{i:03}.txt"), i)).collect();
        let refs: Vec<(&str, u64)> = names.iter().map(|(n, s)| (n.as_str(), *s)).collect();
        let mut p = pane(&refs);
        let mut summary = SelectionSummary::default();
        p.selected = (0..300).map(|i| p.path.join(format!("f{i:03}.txt"))).collect();
        let first = summary.get(&p);
        p.selected = (300..600).map(|i| p.path.join(format!("f{i:03}.txt"))).collect();
        let second = summary.get(&p);
        assert_eq!(first, (300, (0..300).sum()));
        assert_eq!(second, (300, (300..600).sum()));
    }

    #[test]
    fn collects_selected_video_and_audio_files_for_the_duration_total() {
        let mut p = pane(&[("a.txt", 10), ("clip.mp4", 20), ("song.mp3", 30)]);
        for e in &mut p.entries {
            e.ext = e.name.rsplit('.').next().unwrap().into();
        }
        p.rebuild_visible();
        let mut summary = SelectionSummary::default();
        p.selected.insert(p.path.join("a.txt"));
        p.selected.insert(p.path.join("clip.mp4"));
        p.selected.insert(p.path.join("song.mp3"));
        assert_eq!(summary.get(&p), (3, 60));
        let names: Vec<_> = summary.media.iter().map(|(path, _)| path.file_name().unwrap()).collect();
        assert_eq!(names, ["clip.mp4", "song.mp3"]);
        p.selected.remove(&p.path.join("clip.mp4"));
        summary.get(&p);
        assert_eq!(summary.media.len(), 1);
    }

    #[test]
    fn sorting_in_place_keeps_sizes_attached_to_their_files() {
        let mut p = pane(&[("a.txt", 10), ("b.txt", 20), ("c.txt", 30)]);
        let mut summary = SelectionSummary::default();
        p.selected.insert(p.path.join("c.txt"));
        assert_eq!(summary.get(&p), (1, 30));
        p.entries.reverse();
        p.rebuild_visible();
        p.selected.insert(p.path.join("a.txt"));
        assert_eq!(summary.get(&p), (2, 40));
    }
}

/// The window size to switch to when the size restored from the last session is too small to
/// use (it can come back tiny after closing while minimised, or after a DPI change), or `None`
/// to keep it. The default is 1360x840, shrunk to fit 90% of the monitor.
fn window_size_fix(inner: egui::Vec2, monitor: egui::Vec2) -> Option<egui::Vec2> {
    let fit = (monitor * 0.9).min(egui::vec2(1360.0, 840.0));
    let usable = egui::vec2(900.0, 560.0).min(fit);
    (inner.x < usable.x || inner.y < usable.y).then_some(fit)
}

#[cfg(test)]
mod window_size_tests {
    use super::window_size_fix;
    use egui::vec2;

    #[test]
    fn tiny_restored_windows_are_enlarged_to_fit_the_monitor() {
        let monitor = vec2(1920.0, 1080.0);
        assert_eq!(window_size_fix(vec2(320.0, 200.0), monitor), Some(vec2(1360.0, 840.0)));
        assert_eq!(window_size_fix(vec2(1200.0, 400.0), monitor), Some(vec2(1360.0, 840.0)));
        assert_eq!(window_size_fix(vec2(1000.0, 700.0), monitor), None);
        // A small screen gets a window that fits it, and is not "fixed" again every launch.
        let small = vec2(1024.0, 600.0);
        let fixed = window_size_fix(vec2(300.0, 200.0), small).unwrap();
        assert_eq!(fixed, vec2(921.6, 540.0));
        assert_eq!(window_size_fix(fixed, small), None);
    }
}
