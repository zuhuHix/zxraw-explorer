//! Quick access pins: list edits (add, reorder, rename, remove) and the background check that
//! tells whether a pinned file or folder still exists. The checks run on the shared probe
//! service; the scheduling bookkeeping takes explicit times so it can be tested without it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use forge_core::paths::paths_equal;

use crate::path_probe::{Client, Event, Found, is_network};
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
    /// The check finished and found nothing there.
    Missing,
    /// The check did not answer in time or failed without saying the path is gone (an offline
    /// share, access denied, a drive that isn't ready). Shown greyed out, but a click still
    /// tries to open it.
    Unreachable,
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
    if is_network(path, network_roots) {
        NETWORK_INTERVAL
    } else {
        LOCAL_INTERVAL
    }
}

#[derive(Default)]
struct Slot {
    status: PinStatus,
    last_checked: Option<Instant>,
    /// When the check was submitted, while it waits for a worker. At most one queued check
    /// per path, so a dead share can't grow the queue.
    queued_since: Option<Instant>,
    /// When a worker picked the check up; `None` once it answered.
    running_since: Option<Instant>,
    pinned: bool,
    /// What the last finished check found.
    last: Option<Found>,
}

impl Slot {
    /// A worker has run the check for `limit` without an answer.
    fn timed_out(&self, now: Instant, limit: Duration) -> bool {
        self.running_since
            .is_some_and(|started| now.saturating_duration_since(started) >= limit)
    }

    /// No answer within `limit`: the check ran that long, or waited that long for a worker
    /// (all of them busy with hung checks of other paths). A stalled check that is still
    /// queued is not submitted again; it runs when a worker frees up.
    fn stalled(&self, now: Instant, limit: Duration) -> bool {
        self.timed_out(now, limit)
            || self
                .queued_since
                .is_some_and(|queued| now.saturating_duration_since(queued) >= limit)
    }

    fn queued(&self) -> bool {
        self.queued_since.is_some()
    }
}

/// Keeps the existence status of pinned paths up to date without touching the disk on the UI
/// thread. Checks go through the shared probe service (see `path_probe`), whose local and
/// network lanes keep local pins answering while a network share hangs. The Network section
/// uses a second watch for its locations (see `network`).
pub struct PinWatch {
    slots: HashMap<PathBuf, Slot>,
    /// Connected on the first tick that has something to check.
    probe: Option<Client>,
    /// How long a check may go unanswered (see [`CHECK_TIMEOUT`]).
    timeout: Duration,
}

impl Default for PinWatch {
    fn default() -> Self {
        Self {
            slots: HashMap::new(),
            probe: None,
            timeout: CHECK_TIMEOUT,
        }
    }
}

/// A check that has gone this long without an answer (running, or queued behind other
/// checks) shows its pin as unreachable. A running one is retried at its next interval.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

impl PinWatch {
    /// The last known status of a pinned path.
    pub fn status(&self, path: &Path) -> PinStatus {
        self.slots.get(path).map_or(PinStatus::Unknown, |s| s.status)
    }

    /// What the last finished check of `path` found, if one has finished.
    pub fn found(&self, path: &Path) -> Option<&Found> {
        self.slots.get(path).and_then(|s| s.last.as_ref())
    }

    /// The check of `path` has gone unanswered longer than the timeout, running or queued.
    pub fn stalled(&self, path: &Path) -> bool {
        self.slots
            .get(path)
            .is_some_and(|s| s.stalled(Instant::now(), self.timeout))
    }

    /// Checks `path` again on the next tick, unless a check of it is already queued or
    /// running.
    pub fn recheck(&mut self, path: &Path) {
        if let Some(slot) = self.slots.get_mut(path) {
            slot.last_checked = None;
        }
    }

    /// A watch whose checks go to `client` instead of the shared service.
    #[cfg(test)]
    pub(crate) fn with_client(client: Client, timeout: Duration) -> Self {
        Self {
            slots: HashMap::new(),
            probe: Some(client),
            timeout,
        }
    }

    /// Start due checks and apply finished ones. Called once per frame; never blocks.
    pub fn tick(&mut self, pins: &[Pin], network_roots: &[PathBuf], ctx: &egui::Context) {
        self.sync(pins);
        self.run(network_roots, Some(ctx));
    }

    /// [`Self::tick`] for a plain list of paths.
    pub fn tick_paths(&mut self, paths: &[PathBuf], network_roots: &[PathBuf], ctx: Option<&egui::Context>) {
        self.sync_paths(paths.iter());
        self.run(network_roots, ctx);
    }

    fn run(&mut self, network_roots: &[PathBuf], ctx: Option<&egui::Context>) {
        let now = Instant::now();
        for path in self.due(now, network_roots) {
            self.start(&path, now);
            let network = is_network(&path, network_roots);
            self.probe
                .get_or_insert_with(|| Client::new(ctx.cloned()))
                .submit(path, network);
        }
        while let Some(event) = self.probe.as_ref().and_then(Client::try_recv) {
            match event {
                Event::Started(path) => self.started(&path, now),
                Event::Done(path, found) => self.finish(&path, &found),
            }
        }
        self.expire(now);
        if let Some(ctx) = ctx
            && !self.slots.is_empty()
        {
            // Wake up for the next due check even when nothing else repaints.
            ctx.request_repaint_after(LOCAL_INTERVAL);
        }
    }

    /// Follows the pin list: every pinned path gets a slot. Slots of unpinned paths are dropped,
    /// except while a check for them is still pending (its result is then ignored).
    fn sync(&mut self, pins: &[Pin]) {
        self.sync_paths(pins.iter().map(|p| &p.path));
    }

    fn sync_paths<'a>(&mut self, paths: impl Iterator<Item = &'a PathBuf>) {
        for slot in self.slots.values_mut() {
            slot.pinned = false;
        }
        for path in paths {
            self.slots.entry(path.clone()).or_default().pinned = true;
        }
        self.slots
            .retain(|_, slot| slot.pinned || slot.queued() || slot.running_since.is_some());
    }

    /// Pinned paths whose interval has passed and that have no check queued or running (a
    /// running check that timed out does not hold the pin back: it is retried). Sorted, so the
    /// order is stable.
    fn due(&self, now: Instant, network_roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut due: Vec<PathBuf> = self
            .slots
            .iter()
            .filter(|(path, slot)| {
                slot.pinned
                    && !slot.queued()
                    && (slot.running_since.is_none() || slot.timed_out(now, self.timeout))
                    && slot.last_checked.is_none_or(|checked| {
                        now.saturating_duration_since(checked) >= check_interval(path, network_roots)
                    })
            })
            .map(|(path, _)| path.clone())
            .collect();
        due.sort();
        due
    }

    /// Marks a check of `path` as submitted at `now`.
    fn start(&mut self, path: &Path, now: Instant) {
        if let Some(slot) = self.slots.get_mut(path) {
            slot.queued_since = Some(now);
            slot.last_checked = Some(now);
        }
    }

    /// A worker picked up the check of `path`; its timeout counts from here.
    fn started(&mut self, path: &Path, now: Instant) {
        if let Some(slot) = self.slots.get_mut(path) {
            slot.queued_since = None;
            slot.running_since = Some(now);
        }
    }

    /// Pins whose check has gone unanswered longer than the timeout, running or still
    /// queued, show as unreachable.
    fn expire(&mut self, now: Instant) {
        for slot in self.slots.values_mut() {
            if slot.pinned && slot.stalled(now, self.timeout) {
                slot.status = PinStatus::Unreachable;
            }
        }
    }

    /// Records a finished check. A result for a path that is no longer pinned is dropped.
    fn finish(&mut self, path: &Path, found: &Found) {
        let Some(slot) = self.slots.get_mut(path) else {
            return;
        };
        slot.queued_since = None;
        slot.running_since = None;
        if slot.pinned {
            slot.last = Some(found.clone());
            slot.status = match found {
                Found::Folder | Found::File => PinStatus::Present,
                Found::Missing => PinStatus::Missing,
                Found::Unknown(_) => PinStatus::Unreachable,
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
        watch.finish(Path::new(r"C:\a"), &Found::Folder);
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
            watch.finish(&path, &Found::Folder);
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
    fn a_check_that_fails_without_not_found_is_unreachable_not_missing() {
        let mut watch = PinWatch::default();
        let path = PathBuf::from(r"\\nas\photos");
        watch.sync(&[Pin { path: path.clone(), ..Default::default() }]);
        watch.start(&path, Instant::now());
        watch.finish(&path, &Found::Unknown("The network path was not found.".into()));
        assert_eq!(watch.status(&path), PinStatus::Unreachable);
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
        watch.finish(&path, &Found::Missing);
        assert_eq!(watch.status(&path), PinStatus::Missing);

        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(watch.due(t1, &[]), vec![path.clone()]);
        watch.start(&path, t1);
        watch.finish(&path, &Found::Folder);
        assert_eq!(watch.status(&path), PinStatus::Present);
    }

    #[test]
    fn unpinned_paths_are_forgotten() {
        let mut watch = PinWatch::default();
        let path = PathBuf::from(r"C:\a");
        watch.sync(&pins_of(&[r"C:\a"]));
        watch.start(&path, Instant::now());
        watch.finish(&path, &Found::Folder);
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
        watch.finish(&path, &Found::Missing);
        assert_eq!(watch.status(&path), PinStatus::Unknown);
        assert!(watch.slots.is_empty());
        // A result that arrives for a path with no slot at all is also dropped.
        watch.finish(&path, &Found::Folder);
        assert_eq!(watch.status(&path), PinStatus::Unknown);
    }

    #[test]
    fn a_running_check_that_never_answers_shows_unreachable_and_is_retried() {
        let mut watch = PinWatch::default();
        let path = PathBuf::from(r"\\offline\share");
        let pins = vec![Pin { path: path.clone(), ..Default::default() }];
        let t0 = Instant::now();
        watch.sync(&pins);
        watch.start(&path, t0);
        watch.started(&path, t0);
        watch.expire(t0 + CHECK_TIMEOUT / 2);
        assert_eq!(watch.status(&path), PinStatus::Unknown);
        let t1 = t0 + CHECK_TIMEOUT;
        watch.expire(t1);
        assert_eq!(watch.status(&path), PinStatus::Unreachable);
        // Retried at its next interval, even though the first check still holds a worker.
        assert_eq!(watch.due(t0 + NETWORK_INTERVAL, &[]), vec![path.clone()]);
        watch.start(&path, t0 + NETWORK_INTERVAL);
        assert!(watch.due(t0 + NETWORK_INTERVAL * 3, &[]).is_empty(), "one queued retry at most");
        watch.finish(&path, &Found::Folder);
        assert_eq!(watch.status(&path), PinStatus::Present);
    }

    #[test]
    fn a_check_queued_past_the_timeout_shows_unreachable_but_is_not_resubmitted() {
        let mut watch = PinWatch::default();
        let path = PathBuf::from(r"\\nas\photos");
        watch.sync(&pins_of(&[r"\\nas\photos"]));
        let t0 = Instant::now();
        watch.start(&path, t0);
        watch.expire(t0 + CHECK_TIMEOUT / 2);
        assert_eq!(watch.status(&path), PinStatus::Unknown);
        // Every worker is busy with hung checks: no answer in time is unreachable.
        watch.expire(t0 + CHECK_TIMEOUT);
        assert_eq!(watch.status(&path), PinStatus::Unreachable);
        assert!(watch.due(t0 + NETWORK_INTERVAL * 3, &[]).is_empty(), "still queued once");
        // It answers when a worker frees up.
        let t1 = t0 + CHECK_TIMEOUT * 10;
        watch.started(&path, t1);
        watch.finish(&path, &Found::Folder);
        assert_eq!(watch.status(&path), PinStatus::Present);
    }
}
