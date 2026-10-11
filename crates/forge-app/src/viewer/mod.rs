//! Full-window image viewer with culling and compare modes.
//!
//! The host gives the viewer the whole central area and an [`AnnotationStore`]; the viewer
//! decodes in the background (own bounded worker pool), never touches the filesystem on
//! the UI thread, and never writes to image files — every rating/label/pick edit goes
//! through [`AnnotationStore::set`].
//!
//! Modes: [`ViewerMode::Single`] (inspect), [`ViewerMode::Cull`] (keyboard rating with
//! filters, auto-advance and burst badges) and [`ViewerMode::Compare`] (2–4 panels with
//! synchronized zoom/pan, or a before/after split slider for exactly two items).
//!
//! Keyboard map (also shown in-app with `?` / `F1`): see [`HELP`].

mod cache;
mod canvas;
mod chrome;
mod cull;
mod decode;
mod loader;
mod meta;
mod style;
mod zoom;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use egui::{Key, Pos2, Rect, Ui};
use forge_jobs::Priority;

#[allow(unused_imports)] // re-exported for the host app
pub use cull::{Annotation, AnnotationChange, AnnotationStore, CullAction, Filter, MemoryStore};
pub use loader::LoaderConfig as ViewerConfig;
pub use zoom::{ViewState, Zoom};

use loader::{Level, Loader};
use meta::MetaScanner;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewerMode {
    Single,
    Cull,
    Compare,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerOutcome {
    Open,
    Close { last_index: usize },
}

/// Keyboard reference shown in the help overlay: (keys, action, section).
pub const HELP: &[(&str, &str, &str)] = &[
    ("← / →, PgUp / PgDn", "Previous / next photo", "Navigate"),
    ("Home / End", "First / last photo", "Navigate"),
    ("Ctrl+← / Ctrl+→", "Previous / next unrated", "Navigate"),
    ("Esc", "Close help, else exit the viewer", "Navigate"),
    ("Ctrl+1 / 2 / 3", "Single / Cull / Compare mode", "Navigate"),
    ("Z, double-click", "Toggle fit ↔ 100 % (at cursor)", "Zoom"),
    ("Space", "Toggle 100 % at cursor / centre", "Zoom"),
    ("Wheel, + / −", "Zoom around cursor / centre", "Zoom"),
    ("F", "Fit to screen", "Zoom"),
    ("Drag", "Pan", "Zoom"),
    ("0 – 5", "Set star rating", "Cull"),
    (
        "6 / 7 / 8 / 9",
        "Toggle red / yellow / green / blue label",
        "Cull",
    ),
    ("Shift+6 or V", "Toggle purple label", "Cull"),
    ("P / X / U", "Pick / reject / unflag", "Cull"),
    ("A", "Auto-advance after rating on/off", "Cull"),
    ("Tab / Shift+Tab", "Next / previous active panel", "Compare"),
    (
        "← / →",
        "Replace active panel with previous / next photo",
        "Compare",
    ),
    ("S", "Synchronized zoom & pan on/off", "Compare"),
    (
        "B",
        "Before/after split slider (exactly two photos)",
        "Compare",
    ),
    ("I", "Info overlay (EXIF)", "View"),
    ("G", "Sharpness guidance badge", "View"),
    ("T", "Filmstrip on/off", "View"),
    ("? / F1", "This help", "View"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nav {
    Next,
    Prev,
    First,
    Last,
    NextUnrated,
    PrevUnrated,
    To(usize),
}

struct CompareState {
    /// Item indices shown in the panels.
    panels: Vec<usize>,
    views: Vec<ViewState>,
    active: usize,
    sync: bool,
    shared: ViewState,
    split: bool,
    /// Divider position, 0..1 of the canvas width.
    split_x: f32,
}

pub struct Viewer {
    items: Arc<Vec<PathBuf>>,
    mode: ViewerMode,
    /// Filtered item indices (sorted).
    visible: Vec<usize>,
    /// Current item (index into `items`).
    cur: usize,
    filter: Filter,
    filter_dirty: bool,
    auto_advance: bool,
    show_info: bool,
    show_help: bool,
    show_sharpness: bool,
    show_filmstrip: bool,
    view: ViewState,
    compare: CompareState,
    loader: Loader,
    meta: MetaScanner,
    bursts: Vec<Option<(u32, u32)>>,
    bursts_at: usize,
    scroll_strip: bool,
    need_full: Vec<usize>,
    strip_range: std::ops::Range<usize>,
    last_canvas: Rect,
    panel_rects: Vec<Rect>,
    edge_init: bool,
    ppp: f32,
    toast: Option<(String, Instant)>,
    close: bool,
    input_blocked: bool,
}

impl Viewer {
    #[allow(dead_code)] // Standalone viewer API used by the demo and embedders.
    pub fn new(items: Vec<PathBuf>, start: usize, mode: ViewerMode, ctx: &egui::Context) -> Self {
        Self::with_config(items, start, mode, ctx, ViewerConfig::default())
    }

    /// Like [`Viewer::new`] with explicit decode threads / cache budget / FFmpeg tools.
    pub fn with_config(
        items: Vec<PathBuf>,
        start: usize,
        mode: ViewerMode,
        ctx: &egui::Context,
        cfg: ViewerConfig,
    ) -> Self {
        let n = items.len();
        let start = start.min(n.saturating_sub(1));
        let items = Arc::new(items);
        let ff = cfg.ff.clone();
        let mut v = Self {
            loader: Loader::new(items.clone(), cfg, ctx),
            meta: MetaScanner::new(items.clone(), start, ff, ctx.clone()),
            items,
            mode,
            visible: (0..n).collect(),
            cur: start,
            filter: Filter::All,
            filter_dirty: false,
            auto_advance: true,
            show_info: false,
            show_help: false,
            show_sharpness: false,
            show_filmstrip: true,
            view: ViewState::default(),
            compare: CompareState {
                panels: Vec::new(),
                views: Vec::new(),
                active: 0,
                sync: true,
                shared: ViewState::default(),
                split: false,
                split_x: 0.5,
            },
            bursts: vec![None; n],
            bursts_at: 0,
            scroll_strip: true,
            need_full: Vec::new(),
            strip_range: 0..0,
            last_canvas: Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 800.0)),
            panel_rects: Vec::new(),
            edge_init: false,
            ppp: ctx.pixels_per_point(),
            toast: None,
            close: false,
            input_blocked: false,
        };
        if mode == ViewerMode::Compare {
            v.init_compare(None);
        }
        v
    }

    #[allow(dead_code)] // host API
    pub fn mode(&self) -> ViewerMode {
        self.mode
    }

    /// Index (into the original item list) of the photo being viewed.
    #[allow(dead_code)] // host API
    /// The full item list the viewer was opened with (indices match `ViewerOutcome::Close`).
    pub fn items(&self) -> &[PathBuf] {
        &self.items
    }

    #[allow(dead_code)] // Host API.
    pub fn current_index(&self) -> usize {
        self.cur
    }

    /// Whether the current item has a decoded preview or a reported decode failure.
    #[allow(dead_code)] // Used by the script hook only; the standalone demo does not run it.
    pub fn current_decode_complete(&self) -> bool {
        !self.items.is_empty()
            && (self.current_decode_error().is_some()
                || self.loader.has_level(self.cur, Level::Quick)
                || self.loader.has_level(self.cur, Level::Screen))
    }

    /// Decode error currently shown in the empty canvas for the current item.
    #[allow(dead_code)] // Used by the script hook only; the standalone demo does not run it.
    pub fn current_decode_error(&self) -> Option<String> {
        self.loader
            .error((self.cur, Level::Screen))
            .or_else(|| self.loader.error((self.cur, Level::Quick)))
            .map(ToString::to_string)
    }

    /// Prevents viewer keyboard and pointer controls from acting behind an app modal.
    #[allow(dead_code)] // Host API; the standalone demo has no app modals.
    pub fn set_input_blocked(&mut self, blocked: bool) {
        self.input_blocked = blocked;
    }

    /// Changes the decoded-texture memory budget (bytes; default 512 MB).
    #[allow(dead_code)] // host API
    pub fn set_memory_budget(&mut self, bytes: usize) {
        self.loader.set_budget(bytes);
    }

    pub fn set_mode(&mut self, mode: ViewerMode) {
        if mode == self.mode {
            return;
        }
        if mode == ViewerMode::Compare {
            self.init_compare(None);
        } else if self.mode == ViewerMode::Compare {
            if let Some(&i) = self.compare.panels.get(self.compare.active) {
                self.cur = i;
            }
            self.view = ViewState::default();
        }
        self.mode = mode;
        self.scroll_strip = true;
        self.loader.bump_generation();
    }

    /// Draw full-window (caller gives the whole central area). Returns `Close` when the
    /// user exits.
    pub fn show(&mut self, ui: &mut Ui, store: &mut dyn AnnotationStore) -> ViewerOutcome {
        let ctx = ui.ctx().clone();
        if self.input_blocked {
            ui.disable();
        }
        self.ppp = ctx.pixels_per_point();
        self.loader.pump();
        if self.meta.pump() {
            self.update_bursts();
        }
        // The viewer is keyboard-driven: keep egui focus navigation (arrows/Tab/Space on
        // focused buttons) from stealing our keys.
        if !self.input_blocked {
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
        }
        if self.filter_dirty && self.mode != ViewerMode::Cull {
            self.refilter(store);
        }
        if !self.input_blocked {
            self.handle_keys(&ctx, store);
        }

        let full = ui.available_rect_before_wrap();
        ui.painter().rect_filled(full, 0.0, style::CANVAS_BG);
        let mut rest = full;
        let header = Rect::from_min_size(rest.min, egui::vec2(rest.width(), 40.0));
        rest.min.y = header.max.y;
        let sub = (self.mode != ViewerMode::Single).then(|| {
            let r = Rect::from_min_size(rest.min, egui::vec2(rest.width(), 32.0));
            rest.min.y = r.max.y;
            r
        });
        let strip = (self.show_filmstrip && rest.height() > 260.0).then(|| {
            let r = Rect::from_min_max(egui::pos2(rest.min.x, rest.max.y - 92.0), rest.max);
            rest.max.y = r.min.y;
            r
        });
        let canvas = rest;
        self.update_screen_edge(canvas, ctx.pixels_per_point());
        self.last_canvas = canvas;

        self.header(ui, header, store);
        if let Some(r) = sub {
            self.sub_bar(ui, r, store);
        }
        if let Some(r) = strip {
            self.filmstrip(ui, r, store);
        }
        self.need_full.clear();
        match self.mode {
            ViewerMode::Single | ViewerMode::Cull => self.single_canvas(ui, canvas, store),
            ViewerMode::Compare => self.compare_canvas(ui, canvas, store),
        }
        self.toast_overlay(ui, canvas);
        if self.show_help {
            self.help_overlay(&ctx);
        }
        self.issue_requests();

        if self.close {
            self.close = false;
            return ViewerOutcome::Close {
                last_index: self.cur,
            };
        }
        ViewerOutcome::Open
    }

    // -----------------------------------------------------------------------
    // State helpers
    // -----------------------------------------------------------------------

    fn path(&self, i: usize) -> &std::path::Path {
        &self.items[i]
    }

    fn file_name(&self, i: usize) -> String {
        self.items
            .get(i)
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn toast(&mut self, s: impl Into<String>) {
        self.toast = Some((s.into(), Instant::now()));
    }

    fn update_screen_edge(&mut self, canvas: Rect, ppp: f32) {
        let want = ((canvas.width().max(canvas.height()) * ppp / 256.0).ceil() * 256.0) as u32;
        let want = want.clamp(1024, 4096);
        // Set once from the real canvas, then only grow (shrinking would just re-decode).
        if !self.edge_init || want > self.loader.screen_edge + 256 {
            self.edge_init = true;
            self.loader.screen_edge = want;
        }
    }

    fn update_bursts(&mut self) {
        let done = self.meta.done;
        if done >= self.bursts_at + 128 || (self.meta.complete() && done != self.bursts_at) {
            self.bursts = meta::burst_positions(&self.meta.metas, 1.0);
            self.bursts_at = done;
        }
    }

    fn refilter(&mut self, store: &dyn AnnotationStore) {
        self.filter_dirty = false;
        let items = self.items.clone();
        self.visible = cull::filter_items(items.len(), self.filter, |i| store.get(&items[i]));
        if let Some(p) = cull::reselect(&self.visible, self.cur)
            && self.visible[p] != self.cur
        {
            self.cur = self.visible[p];
            self.view = ViewState::default();
            self.loader.bump_generation();
        }
        self.scroll_strip = true;
    }

    fn set_filter(&mut self, f: Filter, store: &dyn AnnotationStore) {
        self.filter = f;
        self.refilter(store);
        if self.visible.is_empty() {
            self.toast(format!("No photos match “{}”", f.label()));
        }
    }

    /// Resolves a navigation request to a target item (in the filtered list).
    fn nav_target(&self, nav: Nav, from: usize, store: &dyn AnnotationStore) -> Option<usize> {
        let vis = &self.visible;
        if vis.is_empty() {
            return None;
        }
        let len = vis.len();
        let at = vis.binary_search(&from);
        let unrated = |i: usize| store.get(&self.items[i]).rating == 0;
        let p = match nav {
            Nav::Next => match at {
                Ok(p) => (p + 1).min(len - 1),
                Err(p) => p.min(len - 1),
            },
            Nav::Prev => match at {
                Ok(p) | Err(p) => p.saturating_sub(1),
            },
            Nav::First => 0,
            Nav::Last => len - 1,
            Nav::NextUnrated => match at {
                Ok(p) => cull::next_matching(vis, p, true, unrated)?,
                Err(p) if p < len && unrated(vis[p]) => p,
                Err(p) => cull::next_matching(vis, p, true, unrated)?,
            },
            Nav::PrevUnrated => match at {
                Ok(p) | Err(p) => cull::next_matching(vis, p, false, unrated)?,
            },
            Nav::To(i) => return vis.binary_search(&i).ok().map(|_| i),
        };
        Some(vis[p])
    }

    fn go(&mut self, nav: Nav, store: &dyn AnnotationStore) {
        if self.filter_dirty {
            self.refilter(store);
        }
        if self.mode == ViewerMode::Compare {
            let a = self.compare.active;
            let Some(&from) = self.compare.panels.get(a) else {
                return;
            };
            match self.nav_target(nav, from, store) {
                Some(t) if t != from => {
                    self.compare.panels[a] = t;
                    self.cur = t;
                    if !self.compare.sync {
                        self.compare.views[a] = ViewState::default();
                    }
                    self.scroll_strip = true;
                    self.loader.bump_generation();
                }
                Some(_) => {}
                None if matches!(nav, Nav::NextUnrated | Nav::PrevUnrated) => {
                    self.toast("No more unrated photos in this direction")
                }
                None => {}
            }
            return;
        }
        match self.nav_target(nav, self.cur, store) {
            Some(t) if t != self.cur => {
                self.cur = t;
                self.view = ViewState::default();
                self.scroll_strip = true;
                self.loader.bump_generation();
            }
            Some(_) => {}
            None if matches!(nav, Nav::NextUnrated | Nav::PrevUnrated) => {
                self.toast("No more unrated photos in this direction")
            }
            None => {}
        }
    }

    fn init_compare(&mut self, count: Option<usize>) {
        let vis = &self.visible;
        let n = count
            .unwrap_or(if self.items.len() <= 4 {
                self.items.len()
            } else {
                2
            })
            .clamp(1, 4)
            .min(vis.len().max(1));
        let p = cull::reselect(vis, self.cur).unwrap_or(0);
        let start = p.min(vis.len().saturating_sub(n));
        self.compare.panels = vis.iter().skip(start).take(n).copied().collect();
        if self.compare.panels.is_empty() && !self.items.is_empty() {
            self.compare.panels.push(self.cur);
        }
        self.compare.views = vec![ViewState::default(); self.compare.panels.len()];
        self.compare.shared = ViewState::default();
        self.compare.active = self
            .compare
            .panels
            .iter()
            .position(|&i| i == self.cur)
            .unwrap_or(0);
        self.compare.split = self.items.len() == 2 && self.compare.split;
    }

    fn set_panel_count(&mut self, n: usize) {
        let n = n.clamp(1, 4);
        let cur_n = self.compare.panels.len();
        if n == cur_n {
            return;
        }
        if n < cur_n {
            self.compare.panels.truncate(n);
            self.compare.views.truncate(n);
            self.compare.active = self.compare.active.min(n - 1);
        } else {
            for _ in cur_n..n {
                let last = self.compare.panels.last().copied().unwrap_or(self.cur);
                let next = self
                    .visible
                    .iter()
                    .copied()
                    .find(|&i| i > last && !self.compare.panels.contains(&i))
                    .or_else(|| {
                        self.visible
                            .iter()
                            .copied()
                            .find(|i| !self.compare.panels.contains(i))
                    })
                    .unwrap_or(last);
                self.compare.panels.push(next);
                self.compare.views.push(ViewState::default());
            }
        }
        self.compare.split = false;
        self.loader.bump_generation();
    }

    /// Item(s) the culling keys act on.
    fn target_item(&self) -> Option<usize> {
        match self.mode {
            ViewerMode::Compare => self.compare.panels.get(self.compare.active).copied(),
            _ if self.items.is_empty() => None,
            _ => Some(self.cur),
        }
    }

    fn apply_cull(&mut self, action: CullAction, store: &mut dyn AnnotationStore) {
        let Some(item) = self.target_item() else {
            return;
        };
        let path = self.items[item].clone();
        let change = cull::change_for(action, store.get(&path));
        store.set(std::slice::from_ref(&path), change);
        self.filter_dirty = self.filter != Filter::All;
        if self.mode == ViewerMode::Cull && self.auto_advance && cull::advances(action) {
            self.go(Nav::Next, store);
        }
    }

    // -----------------------------------------------------------------------
    // Keyboard
    // -----------------------------------------------------------------------

    fn handle_keys(&mut self, ctx: &egui::Context, store: &mut dyn AnnotationStore) {
        if self.input_blocked || ctx.egui_wants_keyboard_input() {
            return;
        }
        let events = ctx.input(|i| i.events.clone());
        let hover = ctx.input(|i| i.pointer.hover_pos());
        for e in events {
            let egui::Event::Key {
                key,
                physical_key,
                pressed: true,
                modifiers: m,
                ..
            } = e
            else {
                continue;
            };
            // Digits by physical position so AZERTY / Portuguese layouts and Shift+6 work.
            let key = match physical_key {
                Some(pk) if is_digit(pk) => pk,
                _ => key,
            };
            if self.show_help {
                if matches!(key, Key::Escape | Key::F1 | Key::Questionmark) {
                    self.show_help = false;
                }
                continue;
            }
            let ctrl = m.ctrl || m.command;
            match key {
                Key::Escape => self.close = true,
                Key::F1 | Key::Questionmark => self.show_help = true,
                Key::Slash if m.shift => self.show_help = true,
                Key::ArrowRight | Key::PageDown if ctrl => self.go(Nav::NextUnrated, store),
                Key::ArrowLeft | Key::PageUp if ctrl => self.go(Nav::PrevUnrated, store),
                Key::ArrowRight | Key::PageDown => self.go(Nav::Next, store),
                Key::ArrowLeft | Key::PageUp => self.go(Nav::Prev, store),
                Key::Home => self.go(Nav::First, store),
                Key::End => self.go(Nav::Last, store),
                Key::Num1 if ctrl => self.set_mode(ViewerMode::Single),
                Key::Num2 if ctrl => self.set_mode(ViewerMode::Cull),
                Key::Num3 if ctrl => self.set_mode(ViewerMode::Compare),
                Key::Tab if self.mode == ViewerMode::Compare => {
                    let n = self.compare.panels.len().max(1);
                    self.compare.active = if m.shift {
                        (self.compare.active + n - 1) % n
                    } else {
                        (self.compare.active + 1) % n
                    };
                    if let Some(&i) = self.compare.panels.get(self.compare.active) {
                        self.cur = i;
                        self.scroll_strip = true;
                    }
                }
                Key::S if self.mode == ViewerMode::Compare && !ctrl => {
                    self.compare.sync = !self.compare.sync;
                    self.toast(if self.compare.sync {
                        "Sync zoom & pan: on"
                    } else {
                        "Sync zoom & pan: off"
                    });
                }
                Key::B if self.mode == ViewerMode::Compare && !ctrl => {
                    if self.items.len() == 2 && self.compare.panels.len() == 2 {
                        self.compare.split = !self.compare.split;
                    } else {
                        self.toast("Before/after split needs exactly two photos");
                    }
                }
                Key::Z | Key::Space if !ctrl => self.zoom_key(ZoomKey::Toggle, hover),
                Key::F if !ctrl => self.zoom_key(ZoomKey::Fit, hover),
                Key::Plus | Key::Equals => self.zoom_key(ZoomKey::In, hover),
                Key::Minus => self.zoom_key(ZoomKey::Out, hover),
                Key::I if !ctrl => self.show_info = !self.show_info,
                Key::G if !ctrl => {
                    self.show_sharpness = !self.show_sharpness;
                    self.toast(if self.show_sharpness {
                        "Sharpness guidance: on (estimate)"
                    } else {
                        "Sharpness guidance: off"
                    });
                }
                Key::T if !ctrl => self.show_filmstrip = !self.show_filmstrip,
                Key::A if !ctrl && self.mode == ViewerMode::Cull => {
                    self.auto_advance = !self.auto_advance;
                    self.toast(if self.auto_advance {
                        "Auto-advance: on"
                    } else {
                        "Auto-advance: off"
                    });
                }
                k if !ctrl && self.mode != ViewerMode::Single => {
                    if let Some(a) = cull::action_for_key(k, m.shift) {
                        self.apply_cull(a, store);
                    }
                }
                k if !ctrl
                    && self.mode == ViewerMode::Single
                    && cull::action_for_key(k, m.shift).is_some() =>
                {
                    self.toast("Switch to Cull mode (Ctrl+2) to rate and flag");
                }
                _ => {}
            }
        }
    }

    // -----------------------------------------------------------------------
    // Background requests
    // -----------------------------------------------------------------------

    fn issue_requests(&mut self) {
        let mut req: Vec<((usize, Level), Priority)> = Vec::new();
        let current: Vec<usize> = match self.mode {
            ViewerMode::Compare => self.compare.panels.clone(),
            _ if self.items.is_empty() => Vec::new(),
            _ => vec![self.cur],
        };
        for &i in &current {
            self.meta.want(i);
            if !self.loader.has_level(i, Level::Screen) {
                req.push(((i, Level::Quick), Priority::High));
            }
            req.push(((i, Level::Screen), Priority::High));
            if self.need_full.contains(&i) {
                req.push(((i, Level::Full), Priority::High));
            }
        }
        // Neighbours in the filtered sequence (±1 first, then ±2).
        if let Some(&anchor) = current.first() {
            let p = cull::reselect(&self.visible, anchor).unwrap_or(0) as isize;
            let last = *current.last().unwrap_or(&anchor);
            let pl = cull::reselect(&self.visible, last).unwrap_or(0) as isize;
            for d in [1isize, -1, 2, -2] {
                let q = if d > 0 { pl + d } else { p + d };
                if q >= 0 && (q as usize) < self.visible.len() {
                    let i = self.visible[q as usize];
                    if !current.contains(&i) {
                        req.push(((i, Level::Screen), Priority::Low));
                    }
                }
            }
        }
        // Filmstrip thumbnails in view.
        for p in self.strip_range.clone() {
            if let Some(&i) = self.visible.get(p) {
                req.push(((i, Level::Small), Priority::Normal));
            }
        }
        self.loader.update(&req);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZoomKey {
    Toggle,
    Fit,
    In,
    Out,
}

fn is_digit(k: Key) -> bool {
    matches!(
        k,
        Key::Num0
            | Key::Num1
            | Key::Num2
            | Key::Num3
            | Key::Num4
            | Key::Num5
            | Key::Num6
            | Key::Num7
            | Key::Num8
            | Key::Num9
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_input_preserves_focus_annotations_and_close_state_with_empty_filter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.png");
        let ctx = egui::Context::default();
        let focus = egui::Id::new("modal-test-focus");

        let mut viewer = Viewer::new(vec![path.clone()], 0, ViewerMode::Cull, &ctx);
        let mut store = MemoryStore::default();
        viewer.set_filter(Filter::Picks, &store);
        viewer.set_input_blocked(true);

        let input = egui::RawInput {
            events: vec![
                egui::Event::Key {
                    key: egui::Key::Num5,
                    physical_key: Some(egui::Key::Num5),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: Some(egui::Key::Escape),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let mut outcome = ViewerOutcome::Open;
        let mut focus_after = None;
        let mut output = ctx.run_ui(input, |ui| {
            ctx.memory_mut(|memory| memory.request_focus(focus));
            outcome = viewer.show(ui, &mut store);
            focus_after = ctx.memory(|memory| memory.focused());
        });
        output.textures_delta.clear();

        assert!(viewer.visible.is_empty());
        assert_eq!(outcome, ViewerOutcome::Open);
        assert_eq!(store.get(&path), Annotation::default());
        assert_eq!(focus_after, Some(focus));
    }
}
