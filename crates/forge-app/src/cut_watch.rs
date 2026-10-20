//! Notices when the items of a pending cut leave their folders: pasted in Explorer or on the
//! desktop, moved by another app, or replaced by a different item under the same name.
//! Nothing tells the app that cut from about such a paste, so the items are checked.
//!
//! One thread serves the whole app and watches only the newest cut. Each check runs on a
//! helper thread that is given up on after [`PASS_TIMEOUT`] (a dead share can block a file
//! system call for a long time). A cut has at most one check out at a time, the watcher's or a
//! paste's, and there are at most [`MAX_HELPERS`] in all, so blocked helpers never pile up and
//! a newer cut is still watched. A check that was skipped or given up on changes nothing.
//!
//! Everything fails closed: an item whose identity could not be read is never moved by a
//! paste (see [`screen`]), and an item that could not be checked stays cut.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender};

/// How often a pending cut checks its items.
pub const POLL: Duration = Duration::from_millis(750);
/// How long one check pass may take before it counts as "could not tell".
pub const PASS_TIMEOUT: Duration = Duration::from_secs(5);
/// Check passes still running after their timeout, at most, in all.
const MAX_HELPERS: usize = 8;

/// What a cut item is. Two different items at one path differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// Volume serial and file index (Windows file ID).
    Id { dir: bool, id: (u32, u64) },
    /// Without a file ID: every one of these must be known (see [`identity_of`]).
    Meta {
        dir: bool,
        len: u64,
        created: SystemTime,
        modified: SystemTime,
    },
}

/// What is known of a cut item's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdState {
    /// Not read yet: right after the cut, or after this app moved or renamed it.
    Pending,
    Known(Identity),
    /// Could not be read: the item is never moved by a paste.
    Unknown,
}

/// One check of a cut item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    Present(Identity),
    /// Not there, and the nearest folder above it that exists could be read, so the item
    /// itself is gone rather than its drive or share.
    Missing,
    /// Could not tell: a removed drive, a network error, access denied, a timeout, or an
    /// item whose identity cannot be read.
    Unknown,
}

/// Checks `path` without following a link at its end. May block on a slow drive.
pub fn observe(path: &Path) -> Observation {
    match std::fs::symlink_metadata(path) {
        Ok(md) => identity_of(path, &md).map_or(Observation::Unknown, Observation::Present),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut ancestor = path.parent();
            while let Some(dir) = ancestor {
                match std::fs::metadata(dir) {
                    Ok(md) if md.is_dir() => return Observation::Missing,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        ancestor = dir.parent();
                    }
                    _ => return Observation::Unknown,
                }
            }
            Observation::Unknown
        }
        Err(_) => Observation::Unknown,
    }
}

/// The file ID, or else kind, size, creation and modified time. `None` if neither is
/// complete: a missing field never matches.
fn identity_of(path: &Path, md: &std::fs::Metadata) -> Option<Identity> {
    let dir = md.is_dir();
    match forge_win::file_id(path) {
        // Some network file systems report index 0 for everything.
        Ok(id) if id.1 != 0 => Some(Identity::Id { dir, id }),
        _ => meta_identity(dir, md.len(), md.created().ok(), md.modified().ok()),
    }
}

fn meta_identity(
    dir: bool,
    len: u64,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
) -> Option<Identity> {
    Some(Identity::Meta {
        dir,
        len,
        created: created?,
        modified: modified?,
    })
}

/// Cuts with a check running, the watcher's or a paste's: `(watcher, generation)`.
static CHECKS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// Starts `observe` over `paths` on a helper thread for the cut `key`, unless that cut
/// already has a check running or [`MAX_HELPERS`] are running in all. The helper calls
/// `done` with the result, after freeing its slot.
fn spawn_check(
    key: (u64, u64),
    paths: Vec<PathBuf>,
    observe: fn(&Path) -> Observation,
    done: impl FnOnce(Vec<Observation>) + Send + 'static,
) -> bool {
    {
        let mut checks = CHECKS.lock().unwrap_or_else(|e| e.into_inner());
        if checks.contains(&key) || checks.len() >= MAX_HELPERS {
            return false;
        }
        checks.push(key);
    }
    let free = move || {
        let mut checks = CHECKS.lock().unwrap_or_else(|e| e.into_inner());
        checks.retain(|k| *k != key);
    };
    let spawned = std::thread::Builder::new()
        .name("forge-cut-check".into())
        .spawn(move || {
            let seen = paths.iter().map(|p| observe(p)).collect();
            free();
            done(seen);
        });
    if spawned.is_err() {
        let mut checks = CHECKS.lock().unwrap_or_else(|e| e.into_inner());
        checks.retain(|k| *k != key);
        return false;
    }
    true
}

/// Checks a cut's items for a paste, off the UI thread, sharing the watcher's slots: it
/// waits for the cut's running check to end, and gives up after `timeout` in all. `None` if
/// no check could be done in time.
#[derive(Clone, Copy)]
pub struct Checker {
    watcher: u64,
    observe: fn(&Path) -> Observation,
}

impl Checker {
    pub fn check(
        &self,
        generation: u64,
        paths: Vec<PathBuf>,
        timeout: Duration,
    ) -> Option<Vec<Observation>> {
        let deadline = Instant::now() + timeout;
        let (tx, rx) = crossbeam_channel::bounded(1);
        loop {
            let tx = tx.clone();
            let send = move |seen| {
                let _ = tx.send(seen);
            };
            if spawn_check((self.watcher, generation), paths.clone(), self.observe, send) {
                break;
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
    }
}

/// What the watcher found out about the cut `generation` at `revision`.
#[derive(Debug, PartialEq)]
pub enum Report {
    /// Identities read for items that were [`IdState::Pending`] (`None`: could not be read).
    Identities {
        generation: u64,
        revision: u64,
        items: Vec<(PathBuf, Option<Identity>)>,
    },
    /// These items are no longer the cut items at their paths: gone on two checks in a row,
    /// or replaced by a different item.
    Gone {
        generation: u64,
        revision: u64,
        paths: Vec<PathBuf>,
    },
}

/// A cut item being watched.
#[derive(Debug)]
struct Item {
    path: PathBuf,
    id: IdState,
    missing: u8,
    gone: bool,
}

impl Item {
    /// Applies one check; `true` once the item counts as gone.
    fn check(&mut self, seen: Observation) -> bool {
        match (self.id, seen) {
            (IdState::Pending, Observation::Present(now)) => {
                self.id = IdState::Known(now);
                false
            }
            (IdState::Pending, _) => {
                self.id = IdState::Unknown;
                self.count_missing(seen)
            }
            (IdState::Known(then), Observation::Present(now)) if then != now => true,
            _ => self.count_missing(seen),
        }
    }

    /// A file saved by replacing it can be absent for a moment: gone only when missing on
    /// two checks in a row.
    fn count_missing(&mut self, seen: Observation) -> bool {
        if seen == Observation::Missing {
            self.missing += 1;
        } else {
            self.missing = 0;
        }
        self.missing >= 2
    }
}

/// The watched cut.
struct Job {
    generation: u64,
    revision: u64,
    items: Vec<Item>,
}

impl Job {
    /// Applies one pass (one observation per item, in order) and reports what changed.
    fn apply(&mut self, seen: &[Observation], report: &dyn Fn(Report)) {
        let (mut learned, mut gone) = (Vec::new(), Vec::new());
        for (item, seen) in self.items.iter_mut().zip(seen) {
            if item.gone {
                continue;
            }
            let was_pending = item.id == IdState::Pending;
            if item.check(*seen) {
                item.gone = true;
                gone.push(item.path.clone());
            }
            if was_pending {
                let id = match item.id {
                    IdState::Known(id) => Some(id),
                    _ => None,
                };
                learned.push((item.path.clone(), id));
            }
        }
        let (generation, revision) = (self.generation, self.revision);
        if !learned.is_empty() {
            report(Report::Identities {
                generation,
                revision,
                items: learned,
            });
        }
        if !gone.is_empty() {
            report(Report::Gone {
                generation,
                revision,
                paths: gone,
            });
        }
    }

    fn done(&self) -> bool {
        self.items.iter().all(|item| item.gone)
    }
}

enum Msg {
    Watch {
        generation: u64,
        revision: u64,
        items: Vec<(PathBuf, IdState)>,
    },
    Forget {
        generation: u64,
    },
}

/// Handle to the watcher thread. Dropping it (and every [`WatchGuard`]) ends the thread.
pub struct CutWatcher {
    tx: Sender<Msg>,
    checker: Checker,
}

/// Timings and the check itself, replaceable in tests.
#[derive(Clone, Copy)]
struct Config {
    poll: Duration,
    timeout: Duration,
    observe: fn(&Path) -> Observation,
}

impl CutWatcher {
    pub fn start(report: impl Fn(Report) + Send + 'static) -> Self {
        Self::start_with(
            Config {
                poll: POLL,
                timeout: PASS_TIMEOUT,
                observe,
            },
            report,
        )
    }

    fn start_with(config: Config, report: impl Fn(Report) + Send + 'static) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let checker = Checker {
            watcher: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            observe: config.observe,
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("forge-cut-watch".into())
            .spawn(move || run(&rx, config, checker, &report))
            .expect("spawn the cut watcher");
        Self { tx, checker }
    }

    /// Checks for pastes, sharing this watcher's slots.
    pub fn checker(&self) -> Checker {
        self.checker
    }

    /// Watches the cut `generation` at `revision` instead of whatever was watched before,
    /// with a first check at once. [`IdState::Pending`] items get their identity there.
    pub fn watch(&self, generation: u64, revision: u64, items: Vec<(PathBuf, IdState)>) {
        let _ = self.tx.send(Msg::Watch {
            generation,
            revision,
            items,
        });
    }

    /// Held by the cut `generation`: dropping it stops the watch, if that cut is the one
    /// watched.
    pub fn guard(&self, generation: u64) -> WatchGuard {
        WatchGuard {
            tx: self.tx.clone(),
            generation,
        }
    }
}

/// See [`CutWatcher::guard`].
pub struct WatchGuard {
    tx: Sender<Msg>,
    generation: u64,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Forget {
            generation: self.generation,
        });
    }
}

/// A pass's result from its helper: `(generation, revision, observations)`.
type PassResult = (u64, u64, Vec<Observation>);

fn run(rx: &Receiver<Msg>, config: Config, checker: Checker, report: &dyn Fn(Report)) {
    let (done_tx, done_rx) = crossbeam_channel::unbounded::<PassResult>();
    let mut job: Option<Job> = None;
    // The pass waited for, until its deadline.
    let mut in_time: Option<(u64, u64, Instant)> = None;
    let mut next_pass = Instant::now();
    loop {
        let wake = match (&job, in_time) {
            (_, Some((_, _, deadline))) => Some(deadline),
            (Some(_), None) => Some(next_pass),
            (None, None) => None,
        };
        let timeout = wake.map_or(Duration::from_secs(3600), |at| {
            at.saturating_duration_since(Instant::now())
        });
        crossbeam_channel::select! {
            recv(rx) -> msg => match msg {
                Ok(Msg::Watch { generation, revision, items }) => {
                    let items = items
                        .into_iter()
                        .map(|(path, id)| Item { path, id, missing: 0, gone: false })
                        .collect();
                    job = Some(Job { generation, revision, items });
                    in_time = None;
                    next_pass = Instant::now();
                }
                Ok(Msg::Forget { generation }) => {
                    if job.as_ref().is_some_and(|j| j.generation == generation) {
                        job = None;
                        in_time = None;
                    }
                }
                Err(_) => return,
            },
            recv(done_rx) -> result => {
                let Ok((generation, revision, seen)) = result else { continue };
                if in_time.is_some_and(|(g, r, _)| g == generation && r == revision) {
                    in_time = None;
                    if let Some(j) = job.as_mut().filter(|j| j.revision == revision) {
                        j.apply(&seen, report);
                    }
                    next_pass = Instant::now() + config.poll;
                }
            },
            default(timeout) => {
                let now = Instant::now();
                if let Some((_, _, deadline)) = in_time {
                    if now >= deadline {
                        // Given up on: nothing was learned, so nothing changes. The helper
                        // keeps its slot until it ends, so this cut gets no second one.
                        in_time = None;
                        next_pass = now + config.poll;
                    }
                } else if let Some(j) = job.as_ref().filter(|_| now >= next_pass) {
                    let (generation, revision) = (j.generation, j.revision);
                    let paths = j.items.iter().map(|item| item.path.clone()).collect();
                    let tx = done_tx.clone();
                    let send = move |seen| {
                        let _ = tx.send((generation, revision, seen));
                    };
                    if spawn_check((checker.watcher, generation), paths, config.observe, send) {
                        in_time = Some((generation, revision, now + config.timeout));
                    } else {
                        // A check of this cut is still out (or too many are): skipped, and
                        // tried again next time. Nothing changes.
                        next_pass = now + config.poll;
                    }
                }
            },
        }
        if job.as_ref().is_some_and(Job::done) {
            job = None;
            in_time = None;
        }
    }
}

/// The cut items after this app moved or renamed `pairs` (`from`, `to`), and the items it
/// used up. An item renamed in place, or carried along by a renamed or moved folder above
/// it, stays cut under its new path; its identity is read again (a move to another volume
/// changes the file ID). An item moved into another folder (a paste here) is used up.
pub fn follow(
    items: &[(PathBuf, IdState)],
    pairs: &[(PathBuf, PathBuf)],
) -> (Vec<(PathBuf, IdState)>, Vec<PathBuf>) {
    let (mut kept, mut used) = (Vec::new(), Vec::new());
    'items: for (old, id) in items {
        let mut path = old.clone();
        for (from, to) in pairs {
            if forge_core::paths::paths_equal(&path, from) {
                let renamed = match (from.parent(), to.parent()) {
                    (Some(a), Some(b)) => forge_core::paths::paths_equal(a, b),
                    _ => false,
                };
                if !renamed {
                    used.push(old.clone());
                    continue 'items;
                }
                path = to.clone();
            } else if forge_core::paths::is_strictly_within(from, &path) {
                // Component-wise and case-insensitive, as Windows compares paths.
                let rest: PathBuf = path.components().skip(from.components().count()).collect();
                path = to.join(rest);
            }
        }
        let id = if path == *old { *id } else { IdState::Pending };
        kept.push((path, id));
    }
    (kept, used)
}

/// Which cut items a paste may move, given what is at their paths now.
#[derive(Debug, Default, PartialEq)]
pub struct Screened {
    /// Still the item that was cut.
    pub keep: Vec<PathBuf>,
    /// A different item has taken the path.
    pub replaced: Vec<PathBuf>,
    /// Its identity was never read, or it could not be checked now: not moved.
    pub unverified: Vec<PathBuf>,
    /// No longer there.
    pub missing: Vec<PathBuf>,
}

/// Sorts the cut `items` by what is at their paths now (`seen`, one per item, in order).
pub fn screen(items: &[(PathBuf, IdState)], seen: &[Observation]) -> Screened {
    let mut out = Screened::default();
    for ((path, id), seen) in items.iter().zip(seen) {
        let list = match (id, seen) {
            (IdState::Known(then), Observation::Present(now)) if then == now => &mut out.keep,
            (IdState::Known(_), Observation::Present(_)) => &mut out.replaced,
            (_, Observation::Missing) => &mut out.missing,
            _ => &mut out.unverified,
        };
        list.push(path.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const A: Identity = Identity::Id {
        dir: false,
        id: (1, 10),
    };
    const B: Identity = Identity::Id {
        dir: false,
        id: (1, 11),
    };

    fn item(id: IdState) -> Item {
        Item {
            path: PathBuf::from(r"C:\x\a.jpg"),
            id,
            missing: 0,
            gone: false,
        }
    }

    #[test]
    fn an_item_is_gone_after_two_missing_checks_in_a_row() {
        let mut it = item(IdState::Known(A));
        assert!(!it.check(Observation::Missing));
        assert!(it.check(Observation::Missing));
    }

    #[test]
    fn an_item_back_or_unknown_between_misses_is_not_gone() {
        let mut it = item(IdState::Known(A));
        assert!(!it.check(Observation::Missing));
        assert!(!it.check(Observation::Present(A)), "came back");
        assert!(!it.check(Observation::Missing));
        assert!(!it.check(Observation::Unknown), "drive unreachable");
        assert!(!it.check(Observation::Missing));
        assert!(!it.check(Observation::Unknown));
    }

    #[test]
    fn a_different_item_at_the_path_is_gone_at_once() {
        let mut it = item(IdState::Known(A));
        assert!(it.check(Observation::Present(B)));
    }

    #[test]
    fn a_pending_item_binds_at_its_first_check_or_stays_unknown() {
        let mut it = item(IdState::Pending);
        assert!(!it.check(Observation::Present(A)));
        assert_eq!(it.id, IdState::Known(A));

        let mut it = item(IdState::Pending);
        assert!(!it.check(Observation::Unknown));
        assert_eq!(it.id, IdState::Unknown, "never bound later");
        assert!(!it.check(Observation::Present(B)));
        assert_eq!(it.id, IdState::Unknown);
        assert!(!it.check(Observation::Missing));
        assert!(
            it.check(Observation::Missing),
            "an unknown item can still go"
        );
    }

    #[test]
    fn the_fallback_identity_needs_every_field() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        assert!(meta_identity(false, 5, None, Some(t)).is_none());
        assert!(meta_identity(false, 5, Some(t), None).is_none());
        assert!(meta_identity(false, 5, None, None).is_none());
        let full = meta_identity(false, 5, Some(t), Some(t)).unwrap();
        assert_ne!(
            Some(full),
            meta_identity(false, 6, Some(t), Some(t)),
            "size"
        );
        assert_ne!(
            Some(full),
            meta_identity(false, 5, Some(t), Some(t + Duration::from_secs(1))),
            "modified"
        );
        assert_ne!(Some(full), meta_identity(true, 5, Some(t), Some(t)), "kind");
    }

    #[test]
    fn observe_tells_a_gone_item_from_an_unreachable_drive() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("a.txt");
        std::fs::write(&file, b"a").unwrap();
        let Observation::Present(first) = observe(&file) else {
            panic!("the file is there");
        };
        std::fs::remove_file(&file).unwrap();
        assert_eq!(observe(&file), Observation::Missing);
        assert_eq!(
            observe(&d.path().join("gone").join("deeper").join("a.txt")),
            Observation::Missing,
            "a removed folder above it still has a readable folder above that"
        );
        // A new file under the same name is a different item.
        std::fs::write(&file, b"b").unwrap();
        assert!(matches!(observe(&file), Observation::Present(now) if now != first));
        // No drive with this letter.
        let free = (b'D'..=b'Z')
            .map(|l| format!("{}:\\", l as char))
            .find(|root| std::fs::metadata(root).is_err())
            .expect("a free drive letter");
        assert_eq!(
            observe(&Path::new(&free).join("Shoot").join("a.jpg")),
            Observation::Unknown
        );
    }

    #[test]
    fn follow_rebinds_renamed_items_and_uses_up_moved_ones() {
        let p = |s: &str| PathBuf::from(s);
        let k = IdState::Known(A);
        let cut = [
            (p(r"C:\a\x.jpg"), k),
            (p(r"C:\a\y.jpg"), k),
            (p(r"c:\B\sub\z.jpg"), k),
            (p(r"C:\c\w.jpg"), k),
            (p(r"C:\bb\v.jpg"), k),
        ];
        // Windows paths compare without case, and by whole components (`C:\b` does not
        // hold `C:\bb`).
        let pairs = [
            (p(r"C:\a\x.jpg"), p(r"C:\a\x2.jpg")),
            (p(r"C:\a\y.jpg"), p(r"D:\pasted\y.jpg")),
            (p(r"C:\b"), p(r"E:\b")),
        ];
        assert_eq!(
            follow(&cut, &pairs),
            (
                vec![
                    (p(r"C:\a\x2.jpg"), IdState::Pending),
                    (p(r"E:\b\sub\z.jpg"), IdState::Pending),
                    (p(r"C:\c\w.jpg"), k),
                    (p(r"C:\bb\v.jpg"), k),
                ],
                vec![p(r"C:\a\y.jpg")]
            )
        );
        assert_eq!(follow(&cut, &[]), (cut.to_vec(), Vec::new()));
    }

    #[test]
    fn a_paste_moves_only_items_still_known_to_be_the_cut_ones() {
        let p = |s: &str| PathBuf::from(s);
        let items = [
            (p("same"), IdState::Known(A)),
            (p("replaced"), IdState::Known(A)),
            (p("never-read"), IdState::Unknown),
            (p("unreachable"), IdState::Known(A)),
            (p("gone"), IdState::Known(A)),
        ];
        let seen = [
            Observation::Present(A),
            Observation::Present(B),
            Observation::Present(A),
            Observation::Unknown,
            Observation::Missing,
        ];
        assert_eq!(
            screen(&items, &seen),
            Screened {
                keep: vec![p("same")],
                replaced: vec![p("replaced")],
                unverified: vec![p("never-read"), p("unreachable")],
                missing: vec![p("gone")],
            }
        );
    }

    #[test]
    fn a_paste_check_shares_the_cuts_slot_and_gives_up_in_time() {
        set(r"W:\paste", Observation::Present(A));
        let checker = Checker {
            watcher: 9_000,
            observe: scripted,
        };
        assert_eq!(
            checker.check(1, vec![PathBuf::from(r"W:\paste")], Duration::from_secs(10)),
            Some(vec![Observation::Present(A)])
        );
        // A check of cut 1 hangs on a share: a second one for cut 1 waits for its slot and
        // gives up in time; another cut is checked meanwhile.
        let hung = std::thread::spawn(move || {
            checker.check(1, vec![PathBuf::from(r"B:\block\p")], Duration::from_millis(300))
        });
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        assert_eq!(
            checker.check(1, vec![PathBuf::from(r"W:\paste")], Duration::from_millis(300)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            checker.check(2, vec![PathBuf::from(r"W:\paste")], Duration::from_secs(2)),
            Some(vec![Observation::Present(A)])
        );
        assert_eq!(hung.join().unwrap(), None, "the hung check itself gave up");
    }

    static SCRIPT: Mutex<Vec<(PathBuf, Observation)>> = Mutex::new(Vec::new());

    /// Answers from `SCRIPT`; a path under `B:\block` hangs well past the test's timeout.
    fn scripted(path: &Path) -> Observation {
        if path.starts_with(r"B:\block") {
            std::thread::sleep(Duration::from_secs(3));
        }
        SCRIPT
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == path)
            .map_or(Observation::Unknown, |(_, o)| *o)
    }

    fn set(path: &str, seen: Observation) {
        let mut script = SCRIPT.lock().unwrap();
        script.retain(|(p, _)| p != Path::new(path));
        script.push((PathBuf::from(path), seen));
    }

    fn wait_for(rx: &Receiver<Report>, want: &Report) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(got) = rx.recv_timeout(Duration::from_millis(50))
                && &got == want
            {
                return;
            }
        }
        panic!("no {want:?} within 10 s");
    }

    /// One watcher: identities are read at once, items are reported one at a time as
    /// they go, a blocked check counts as "could not tell" without holding up a newer cut,
    /// and an older cut is no longer reported on.
    #[test]
    fn the_watcher_reports_items_as_they_go_and_is_not_held_up_by_a_blocked_check() {
        let p = PathBuf::from;
        set(r"W:\one", Observation::Present(A));
        set(r"W:\two", Observation::Present(B));
        let (tx, rx) = crossbeam_channel::unbounded();
        let config = Config {
            poll: Duration::from_millis(10),
            timeout: Duration::from_millis(200),
            observe: scripted,
        };
        let watcher = CutWatcher::start_with(config, move |r| {
            let _ = tx.send(r);
        });
        let two = (p(r"W:\two"), IdState::Known(B));
        watcher.watch(1, 0, vec![(p(r"W:\one"), IdState::Pending), two]);
        wait_for(
            &rx,
            &Report::Identities {
                generation: 1,
                revision: 0,
                items: vec![(p(r"W:\one"), Some(A))],
            },
        );
        set(r"W:\one", Observation::Missing);
        wait_for(
            &rx,
            &Report::Gone {
                generation: 1,
                revision: 0,
                paths: vec![p(r"W:\one")],
            },
        );

        // A cut on a share that hangs: its check is given up on, which teaches nothing, so
        // the item stays pending (it is not taken as "could not be read").
        watcher.watch(2, 0, vec![(p(r"B:\block\x"), IdState::Pending)]);
        std::thread::sleep(Duration::from_millis(600));
        assert!(
            rx.try_iter()
                .all(|r| !matches!(r, Report::Identities { generation: 2, .. })),
            "a check that never finished must not decide anything"
        );
        // A newer cut is still watched while that check hangs.
        set(r"W:\three", Observation::Present(B));
        watcher.watch(3, 0, vec![(p(r"W:\three"), IdState::Known(A))]);
        wait_for(
            &rx,
            &Report::Gone {
                generation: 3,
                revision: 0,
                paths: vec![p(r"W:\three")],
            },
        );
        // Generation 1 is no longer watched: its replaced item is not reported.
        set(r"W:\two", Observation::Present(A));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            rx.try_iter()
                .all(|r| !matches!(r, Report::Gone { generation: 1, .. }))
        );
    }
}
