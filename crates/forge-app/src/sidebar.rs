//! Navigation sidebar: quick access, bookmarks, drives.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use egui::{RichText, Ui};
use parking_lot::Mutex;

use crate::app::ZxrawApp;
use crate::theme;
use crate::views::DragPaths;

#[derive(Clone, Debug)]
pub struct Drive {
    pub root: PathBuf,
    pub label: String,
    pub kind: &'static str,
    pub total: u64,
    pub free: u64,
    pub available: bool,
}

pub struct SidebarState {
    pub drives: Arc<Mutex<Vec<Drive>>>,
    pub known: Vec<(String, PathBuf)>,
    pub cards: CardWatch,
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
    let r = ui.add(
        egui::Button::selectable(current, text)
            .frame_when_inactive(false)
            .min_size(egui::vec2(ui.available_width(), 0.0)),
    );
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
    let collections = app.ops.collections_ui.sidebar_collections().to_vec();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(4.0);
            section(ui, "Quick access");
            for (name, path) in app.sidebar.known.clone() {
                nav_item(app, ui, "★", &name, &path, None);
            }

            if !app.settings.bookmarks.is_empty() {
                ui.add_space(6.0);
                section(ui, "Bookmarks");
                let mut remove = None;
                for (i, path) in app.settings.bookmarks.clone().iter().enumerate() {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.to_string_lossy().into_owned());
                    let r = nav_item(app, ui, "🔖", &name, path, None);
                    r.context_menu(|ui| {
                        if ui.button("Remove bookmark").clicked() {
                            remove = Some(i);
                            ui.close();
                        }
                        if i > 0 && ui.button("Move up").clicked() {
                            app.settings.bookmarks.swap(i, i - 1);
                            ui.close();
                        }
                    });
                }
                if let Some(i) = remove {
                    app.settings.bookmarks.remove(i);
                }
            }

            ui.add_space(6.0);
            section(ui, "Collections");
            if collections.is_empty() {
                ui.label(RichText::new("No collections yet").weak());
            } else {
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
            if ui.small_button("Manage collections…").clicked() {
                app.ops.collections_ui.open = true;
            }

            ui.add_space(6.0);
            section(ui, "Drives");
            let drives = app.sidebar.drives.lock().clone();
            for d in drives {
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
                        RichText::new(format!("{icon}  {label}  (unavailable)"))
                            .color(theme::muted(ui)),
                    );
                    continue;
                }
                nav_item(app, ui, icon, &label, &d.root, None);
                if d.total > 0 {
                    let used = 1.0 - d.free as f32 / d.total as f32;
                    let bar = egui::ProgressBar::new(used)
                        .desired_height(3.0)
                        .desired_width(ui.available_width() - 8.0)
                        .fill(if used > 0.9 {
                            ui.visuals().error_fg_color
                        } else {
                            theme::ACCENT
                        });
                    ui.add(bar).on_hover_text(format!(
                        "{} free of {}",
                        forge_core::model::format_size(d.free),
                        forge_core::model::format_size(d.total)
                    ));
                }
            }
        });
}

fn section(ui: &mut Ui, title: &str) {
    ui.label(
        RichText::new(title.to_uppercase())
            .small()
            .strong()
            .color(theme::muted(ui)),
    );
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
