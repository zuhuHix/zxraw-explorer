//! Quick access pins: list edits (add, reorder, rename, remove) and the background check that
//! tells whether a pinned file or folder still exists. The checks run on short-lived worker
//! threads; the scheduling bookkeeping takes explicit times so it can be tested without them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use forge_core::paths::paths_equal;

use crate::path_probe::Probe;
use crate::settings::Pin;

/// How often a local pinned path is checked.
const LOCAL_INTERVAL: Duration = Duration::from_secs(5);
/// How often a network path (UNC or a network drive) is checked. Each check can stall on a dead
/// share, so they are spaced out.
const NETWORK_INTERVAL: Duration = Duration::from_secs(30);

/// Whether a pinned path currently exists. `Unknown` until its first check finishes; it renders
/// like `Present`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PinStatus {
    #[default]
    Unknown,
    Present,
    Missing,
}

/// Drag payload of a pin row: the index of the pin being moved.
pub struct PinDrag(pub usize);

/// Appends `items` (path, is_file) in order. Paths already pinned are skipped, including
/// case-only differences and duplicates within `items`. Returns how many were added.
pub fn add_pins(pins: &mut Vec<Pin>, items: Vec<(PathBuf, bool)>) -> usize {
    let mut added = 0;
    for (path, file) in items {
        if pins.iter().any(|p| paths_equal(&p.path, &path)) {
            continue;
        }
        pins.push(Pin {
            extra: Default::default(),
            path,
            label: None,
            file,
        });
        added += 1;
    }
    added
}

/// Moves the pin at `from` so it ends up at index `to`. Out-of-range indices do nothing.
pub fn move_pin(pins: &mut Vec<Pin>, from: usize, to: usize) {
    if from == to || from >= pins.len() || to >= pins.len() {
        return;
    }
    let pin = pins.remove(from);
    pins.insert(to, pin);
}

/// Removes the pin at `index`. Out-of-range indices do nothing.
pub fn remove_pin(pins: &mut Vec<Pin>, index: usize) {
    if index < pins.len() {
        pins.remove(index);
    }
}

/// Sets the label of the pin for `path`. Blank text clears the label. Returns false when no pin
/// matches.
pub fn set_label(pins: &mut [Pin], path: &Path, text: &str) -> bool {
    let Some(pin) = pins.iter_mut().find(|p| paths_equal(&p.path, path)) else {
        return false;
    };
    let text = text.trim();
    pin.label = (!text.is_empty()).then(|| text.to_string());
    true
}

/// How long to wait between checks of `path`: network paths (UNC, or under a network drive
/// root) get the longer interval.
pub fn check_interval(path: &Path, network_roots: &[PathBuf]) -> Duration {
    let text = path.to_string_lossy().to_lowercase();
    let is_unc = text.starts_with(r"\\");
    let on_network_drive = network_roots
        .iter()
        .any(|root| text.starts_with(&root.to_string_lossy().to_lowercase()));
    if is_unc || on_network_drive {
        NETWORK_INTERVAL
    } else {
        LOCAL_INTERVAL
    }
}

#[derive(Default)]
struct Slot {
    status: PinStatus,
    last_checked: Option<Instant>,
    /// A check was started and its result has not arrived yet. Never cleared by timeout, so a
    /// hung check blocks further checks of that path instead of piling up threads.
    in_flight: bool,
    pinned: bool,
}

/// Keeps the existence status of pinned paths up to date without touching the disk on the UI
/// thread. Checks queue on a small worker pool (see `path_probe`), so pins on dead network
/// shares can't pile up blocked threads.
#[derive(Default)]
pub struct PinWatch {
    slots: HashMap<PathBuf, Slot>,
    /// Started on the first tick that has something to check.
    probe: Option<Probe>,
}

/// Workers checking pins. A check stuck on an offline share holds one of them.
const PROBE_WORKERS: usize = 3;

/// A check that has not answered by then shows its pin as unavailable. The check itself keeps
/// its worker until the OS gives up, and the path is not queued again meanwhile.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

impl PinWatch {
    /// The last known status of a pinned path.
    pub fn status(&self, path: &Path) -> PinStatus {
        self.slots.get(path).map_or(PinStatus::Unknown, |s| s.status)
    }

    /// Start due checks and apply finished ones. Called once per frame; never blocks.
    pub fn tick(&mut self, pins: &[Pin], network_roots: &[PathBuf], ctx: &egui::Context) {
        self.sync(pins);
        let now = Instant::now();
        for path in self.due(now, network_roots) {
            self.start(&path, now);
            self.probe
                .get_or_insert_with(|| Probe::new(PROBE_WORKERS, Some(ctx.clone())))
                .submit(path);
        }
        while let Some((path, found)) = self.probe.as_ref().and_then(Probe::try_recv) {
            self.finish(&path, found.is_some());
        }
        self.expire(now);
        if !self.slots.is_empty() {
            // Wake up for the next due check even when nothing else repaints.
            ctx.request_repaint_after(LOCAL_INTERVAL);
        }
    }

    /// Follows the pin list: every pinned path gets a slot. Slots of unpinned paths are dropped,
    /// except while a check for them is still running (its result is then ignored).
    fn sync(&mut self, pins: &[Pin]) {
        for slot in self.slots.values_mut() {
            slot.pinned = false;
        }
        for pin in pins {
            self.slots.entry(pin.path.clone()).or_default().pinned = true;
        }
        self.slots.retain(|_, slot| slot.pinned || slot.in_flight);
    }

    /// Pinned paths that have no check in flight and whose interval has passed. Sorted, so the
    /// order is stable.
    fn due(&self, now: Instant, network_roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut due: Vec<PathBuf> = self
            .slots
            .iter()
            .filter(|(path, slot)| {
                slot.pinned
                    && !slot.in_flight
                    && slot.last_checked.is_none_or(|checked| {
                        now.saturating_duration_since(checked) >= check_interval(path, network_roots)
                    })
            })
            .map(|(path, _)| path.clone())
            .collect();
        due.sort();
        due
    }

    /// Marks a check of `path` as started at `now`.
    fn start(&mut self, path: &Path, now: Instant) {
        if let Some(slot) = self.slots.get_mut(path) {
            slot.in_flight = true;
            slot.last_checked = Some(now);
        }
    }

    /// Pins whose check has run longer than [`CHECK_TIMEOUT`] show as missing.
    fn expire(&mut self, now: Instant) {
        for slot in self.slots.values_mut() {
            let overdue = slot
                .last_checked
                .is_some_and(|started| now.saturating_duration_since(started) >= CHECK_TIMEOUT);
            if slot.pinned && slot.in_flight && overdue {
                slot.status = PinStatus::Missing;
            }
        }
    }

    /// Records a finished check. A result for a path that is no longer pinned is dropped.
    fn finish(&mut self, path: &Path, present: bool) {
        let Some(slot) = self.slots.get_mut(path) else {
            return;
        };
        slot.in_flight = false;
        if slot.pinned {
            slot.status = if present {
                PinStatus::Present
            } else {
                PinStatus::Missing
            };
        } else {
            self.slots.remove(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(path: &str) -> Pin {
        Pin {
            extra: Default::default(),
            path: PathBuf::from(path),
            label: None,
            file: false,
        }
    }

    fn paths(pins: &[Pin]) -> Vec<PathBuf> {
        pins.iter().map(|p| p.path.clone()).collect()
    }

    fn pins_of(pins: &[&str]) -> Vec<Pin> {
        pins.iter().map(|p| pin(p)).collect()
    }

    #[test]
    fn add_appends_in_order_and_skips_duplicates() {
        let mut pins = Vec::new();
        let added = add_pins(
            &mut pins,
            vec![
                (PathBuf::from(r"C:\A"), false),
                (PathBuf::from(r"C:\B"), true),
                (PathBuf::from(r"c:\a"), false),
                (PathBuf::from(r"C:\B"), false),
                (PathBuf::from(r"C:\C"), false),
            ],
        );
        assert_eq!(added, 3);
        assert_eq!(
            paths(&pins),
            vec![
                PathBuf::from(r"C:\A"),
                PathBuf::from(r"C:\B"),
                PathBuf::from(r"C:\C")
            ]
        );
        assert!(pins[1].file);
        assert!(!pins[0].file);
        assert_eq!(add_pins(&mut pins, vec![(PathBuf::from(r"C:\C"), false)]), 0);
    }

    #[test]
    fn move_pin_forward_backward_same_and_out_of_range() {
        let mut pins = pins_of(&[r"C:\a", r"C:\b", r"C:\c"]);
        move_pin(&mut pins, 0, 2);
        assert_eq!(
            paths(&pins),
            vec![
                PathBuf::from(r"C:\b"),
                PathBuf::from(r"C:\c"),
                PathBuf::from(r"C:\a")
            ]
        );
        move_pin(&mut pins, 2, 0);
        assert_eq!(
            paths(&pins),
            vec![
                PathBuf::from(r"C:\a"),
                PathBuf::from(r"C:\b"),
                PathBuf::from(r"C:\c")
            ]
        );
        let before = pins.clone();
        move_pin(&mut pins, 1, 1);
        assert_eq!(pins, before);
        move_pin(&mut pins, 0, 3);
        move_pin(&mut pins, 3, 0);
        assert_eq!(pins, before);
    }

    #[test]
    fn remove_pin_drops_only_that_entry() {
        let mut pins = pins_of(&[r"C:\a", r"C:\b"]);
        remove_pin(&mut pins, 0);
        assert_eq!(paths(&pins), vec![PathBuf::from(r"C:\b")]);
        remove_pin(&mut pins, 5);
        assert_eq!(pins.len(), 1);
    }

    #[test]
    fn rename_sets_and_clears_label() {
        let mut pins = pins_of(&[r"C:\a"]);
        assert!(set_label(&mut pins, Path::new(r"c:\A"), "  Work  "));
        assert_eq!(pins[0].label.as_deref(), Some("Work"));
        assert!(set_label(&mut pins, Path::new(r"C:\a"), "   "));
        assert_eq!(pins[0].label, None);
        assert!(!set_label(&mut pins, Path::new(r"C:\gone"), "x"));
    }

    #[test]
    fn interval_is_longer_for_network_paths() {
        let roots = vec![PathBuf::from(r"Z:\")];
        assert_eq!(check_interval(Path::new(r"C:\x"), &roots), LOCAL_INTERVAL);
        assert_eq!(check_interval(Path::new(r"Z:\docs"), &roots), NETWORK_INTERVAL);
        assert_eq!(
            check_interval(Path::new(r"\\server\share\x"), &[]),
            NETWORK_INTERVAL
        );
    }

    #[test]
    fn first_tick_checks_every_pin() {
        let mut watch = PinWatch::default();
        let pins = pins_of(&[r"C:\a", r"C:\b"]);
        watch.sync(&pins);
        let now = Instant::now();
        assert_eq!(watch.due(now, &[]).len(), 2);
    }

    #[test]
    fn nothing_is_rechecked_before_the_interval() {
        let mut watch = PinWatch::default();
        let pins = pins_of(&[r"C:\a"]);
        watch.sync(&pins);
        let t0 = Instant::now();
        watch.start(Path::new(r"C:\a"), t0);
        watch.finish(Path::new(r"C:\a"), true);
        assert!(watch.due(t0 + Duration::from_secs(4), &[]).is_empty());
        assert_eq!(watch.due(t0 + Duration::from_secs(5), &[]).len(), 1);
    }

    #[test]
    fn network_paths_wait_thirty_seconds() {
        let mut watch = PinWatch::default();
        let pins = pins_of(&[r"Z:\docs", r"\\server\share", r"C:\local"]);
        watch.sync(&pins);
        let roots = vec![PathBuf::from(r"Z:\")];
        let t0 = Instant::now();
        for path in watch.due(t0, &roots) {
            watch.start(&path, t0);
            watch.finish(&path, true);
        }
        let at_10 = watch.due(t0 + Duration::from_secs(10), &roots);
        assert_eq!(at_10, vec![PathBuf::from(r"C:\local")]);
        let at_30 = watch.due(t0 + Duration::from_secs(30), &roots);
        assert_eq!(at_30.len(), 3);
    }

    #[test]
    fn hung_check_is_never_duplicated() {
        let mut watch = PinWatch::default();
        let pins = pins_of(&[r"\\dead\share\x"]);
        watch.sync(&pins);
        let t0 = Instant::now();
        let path = PathBuf::from(r"\\dead\share\x");
        assert_eq!(watch.due(t0, &[]).len(), 1);
        watch.start(&path, t0);
        assert!(watch.due(t0 + Duration::from_secs(600), &[]).is_empty());
        assert_eq!(watch.status(&path), PinStatus::Unknown);
    }

    #[test]
    fn missing_then_present_after_a_later_result() {
        let mut watch = PinWatch::default();
        let path = PathBuf::from(r"E:\photos");
        watch.sync(&[Pin {
            extra: Default::default(),
            path: path.clone(),
            label: None,
            file: false,
        }]);
        let t0 = Instant::now();
        watch.start(&path, t0);
        watch.finish(&path, false);
        assert_eq!(watch.status(&path), PinStatus::Missing);

        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(watch.due(t1, &[]), vec![path.clone()]);
        watch.start(&path, t1);
        watch.finish(&path, true);
        assert_eq!(watch.status(&path), PinStatus::Present);
    }

    #[test]
    fn unpinned_paths_are_forgotten() {
        let mut watch = PinWatch::default();
        let path = PathBuf::from(r"C:\a");
        watch.sync(&pins_of(&[r"C:\a"]));
        watch.start(&path, Instant::now());
        watch.finish(&path, true);
        assert_eq!(watch.status(&path), PinStatus::Present);
        watch.sync(&[]);
        assert_eq!(watch.status(&path), PinStatus::Unknown);
        assert!(watch.slots.is_empty());
    }

    #[test]
    fn stale_result_for_unpinned_path_is_ignored() {
        let mut watch = PinWatch::default();
        let path = PathBuf::from(r"C:\a");
        watch.sync(&pins_of(&[r"C:\a"]));
        let t0 = Instant::now();
        watch.start(&path, t0);
        // Unpinned while the check runs; the late result must not resurrect the slot.
        watch.sync(&[]);
        watch.finish(&path, false);
        assert_eq!(watch.status(&path), PinStatus::Unknown);
        assert!(watch.slots.is_empty());
        // A result that arrives for a path with no slot at all is also dropped.
        watch.finish(&path, true);
        assert_eq!(watch.status(&path), PinStatus::Unknown);
    }

    #[test]
    fn a_check_that_never_answers_shows_missing_after_the_timeout() {
        let mut watch = PinWatch::default();
        let path = PathBuf::from(r"\\offline\share");
        let pins = vec![Pin { path: path.clone(), ..Default::default() }];
        let t0 = Instant::now();
        watch.sync(&pins);
        watch.start(&path, t0);
        watch.expire(t0 + CHECK_TIMEOUT / 2);
        assert_eq!(watch.status(&path), PinStatus::Unknown);
        watch.expire(t0 + CHECK_TIMEOUT);
        assert_eq!(watch.status(&path), PinStatus::Missing);
        assert!(watch.due(t0 + CHECK_TIMEOUT * 10, &[]).is_empty(), "still in flight");
        watch.finish(&path, true);
        assert_eq!(watch.status(&path), PinStatus::Present);
    }
}
