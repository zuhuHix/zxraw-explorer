//! Notices when the items of a pending cut leave their folders: pasted in Explorer or on the
//! desktop, moved by another app, or replaced by a different item under the same name.
//! Nothing tells the app that cut from about such a paste, so the items are checked.
//!
//! One thread serves the whole app and watches only the newest cut: a new cut replaces the
//! work, so checks on a slow or unreachable drive delay the next check rather than piling
//! up threads, and the UI thread never waits on the file system.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

/// How often a pending cut checks its items.
pub const POLL: Duration = Duration::from_millis(750);

/// What a cut item was when it was cut. Two different items at one path differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// Volume serial and file index (Windows file ID).
    Id { dir: bool, id: (u32, u64) },
    /// Fallback when the file ID cannot be read. Unlike the modified time, the creation time
    /// stays when the file is edited.
    Created {
        dir: bool,
        created: Option<SystemTime>,
    },
}

/// One check of a cut item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    Present(Identity),
    /// Not there, and the nearest folder above it that exists could be read, so the item
    /// itself is gone rather than its drive or share.
    Missing,
    /// Could not tell: a removed drive, a network error, access denied.
    Unknown,
}

/// Checks `path` without following a link at its end. May block on a slow drive.
pub fn observe(path: &Path) -> Observation {
    match std::fs::symlink_metadata(path) {
        Ok(md) => Observation::Present(identity_of(path, &md)),
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

fn identity_of(path: &Path, md: &std::fs::Metadata) -> Identity {
    let dir = md.is_dir();
    match forge_win::file_id(path) {
        Ok(id) => Identity::Id { dir, id },
        Err(_) => Identity::Created {
            dir,
            created: md.created().ok(),
        },
    }
}

/// What the watcher found out about the cut `generation`.
#[derive(Debug, PartialEq)]
pub enum Report {
    /// Identities read for items that had none yet.
    Identities {
        generation: u64,
        items: Vec<(PathBuf, Identity)>,
    },
    /// These items are no longer the cut items at their paths: gone on two checks in a row,
    /// or replaced by a different item.
    Gone {
        generation: u64,
        paths: Vec<PathBuf>,
    },
}

/// A cut item being watched.
#[derive(Debug)]
struct Item {
    path: PathBuf,
    identity: Option<Identity>,
    missing: u8,
    gone: bool,
}

impl Item {
    /// Applies one check; `true` once the item counts as gone.
    fn check(&mut self, seen: Observation) -> bool {
        match seen {
            Observation::Present(now) => match self.identity {
                None => {
                    self.identity = Some(now);
                    self.missing = 0;
                    false
                }
                Some(then) if then == now => {
                    self.missing = 0;
                    false
                }
                Some(_) => true,
            },
            // A file saved by replacing it can be absent for a moment.
            Observation::Missing => {
                self.missing += 1;
                self.missing >= 2
            }
            Observation::Unknown => {
                self.missing = 0;
                false
            }
        }
    }
}

enum Msg {
    Watch {
        generation: u64,
        items: Vec<(PathBuf, Option<Identity>)>,
    },
    Forget {
        generation: u64,
    },
}

/// Handle to the watcher thread. Dropping it ends the thread after its current check.
pub struct CutWatcher {
    tx: Sender<Msg>,
}

impl CutWatcher {
    pub fn start(report: impl Fn(Report) + Send + 'static) -> Self {
        Self::start_with(POLL, observe, report)
    }

    fn start_with(
        poll: Duration,
        observe: fn(&Path) -> Observation,
        report: impl Fn(Report) + Send + 'static,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("forge-cut-watch".into())
            .spawn(move || run(&rx, poll, observe, &report))
            .expect("spawn the cut watcher");
        Self { tx }
    }

    /// Watches the cut `generation` instead of whatever was watched before. An item with no
    /// identity takes the one it has at the first check.
    pub fn watch(&self, generation: u64, items: Vec<(PathBuf, Option<Identity>)>) {
        let _ = self.tx.send(Msg::Watch { generation, items });
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

fn run(
    rx: &Receiver<Msg>,
    poll: Duration,
    observe: fn(&Path) -> Observation,
    report: &dyn Fn(Report),
) {
    let mut watched: Option<(u64, Vec<Item>)> = None;
    loop {
        let first = if watched.is_some() {
            match rx.recv_timeout(poll) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(msg) => Some(msg),
                Err(_) => return,
            }
        };
        // Only the newest request counts; requests that queued up during a slow check are
        // replaced, not worked through.
        for msg in first.into_iter().chain(rx.try_iter()) {
            match msg {
                Msg::Watch { generation, items } => {
                    let items = items
                        .into_iter()
                        .map(|(path, identity)| Item {
                            path,
                            identity,
                            missing: 0,
                            gone: false,
                        })
                        .collect();
                    watched = Some((generation, items));
                }
                Msg::Forget { generation } => {
                    if watched.as_ref().is_some_and(|(g, _)| *g == generation) {
                        watched = None;
                    }
                }
            }
        }
        let Some((generation, items)) = watched.as_mut() else {
            continue;
        };
        let (mut learned, mut gone) = (Vec::new(), Vec::new());
        for item in items.iter_mut().filter(|item| !item.gone) {
            let had_identity = item.identity.is_some();
            if item.check(observe(&item.path)) {
                item.gone = true;
                gone.push(item.path.clone());
            } else if let (false, Some(identity)) = (had_identity, item.identity) {
                learned.push((item.path.clone(), identity));
            }
        }
        let generation = *generation;
        if items.iter().all(|item| item.gone) {
            watched = None;
        }
        if !learned.is_empty() {
            report(Report::Identities {
                generation,
                items: learned,
            });
        }
        if !gone.is_empty() {
            report(Report::Gone {
                generation,
                paths: gone,
            });
        }
    }
}

/// Where the cut items are after the app itself moved or renamed `pairs` (`from`, `to`):
/// `None` for an item moved into another folder (a paste used it up), its new path for an
/// item renamed in place or carried along by a renamed or moved folder above it.
pub fn follow(paths: &[PathBuf], pairs: &[(PathBuf, PathBuf)]) -> Vec<Option<PathBuf>> {
    paths
        .iter()
        .map(|path| {
            let mut path = path.clone();
            for (from, to) in pairs {
                if forge_core::paths::paths_equal(&path, from) {
                    let renamed = match (from.parent(), to.parent()) {
                        (Some(a), Some(b)) => forge_core::paths::paths_equal(a, b),
                        _ => false,
                    };
                    if !renamed {
                        return None;
                    }
                    path = to.clone();
                } else if let Ok(rest) = path.strip_prefix(from) {
                    path = to.join(rest);
                }
            }
            Some(path)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Instant;

    use super::*;

    const A: Identity = Identity::Id {
        dir: false,
        id: (1, 10),
    };
    const B: Identity = Identity::Id {
        dir: false,
        id: (1, 11),
    };

    fn item(identity: Option<Identity>) -> Item {
        Item {
            path: PathBuf::from(r"C:\x\a.jpg"),
            identity,
            missing: 0,
            gone: false,
        }
    }

    #[test]
    fn an_item_is_gone_after_two_missing_checks_in_a_row() {
        let mut it = item(Some(A));
        assert!(!it.check(Observation::Missing));
        assert!(it.check(Observation::Missing));
    }

    #[test]
    fn an_item_back_or_unknown_between_misses_is_not_gone() {
        let mut it = item(Some(A));
        assert!(!it.check(Observation::Missing));
        assert!(!it.check(Observation::Present(A)), "came back");
        assert!(!it.check(Observation::Missing));
        assert!(!it.check(Observation::Unknown), "drive unreachable");
        assert!(!it.check(Observation::Missing));
        assert!(!it.check(Observation::Unknown));
    }

    #[test]
    fn a_different_item_at_the_path_is_gone_at_once() {
        let mut it = item(Some(A));
        assert!(it.check(Observation::Present(B)));
    }

    #[test]
    fn an_item_takes_its_identity_at_the_first_check() {
        let mut it = item(None);
        assert!(!it.check(Observation::Present(A)));
        assert_eq!(it.identity, Some(A));
        assert!(it.check(Observation::Present(B)));
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
    fn follow_keeps_renames_and_drops_moves_into_other_folders() {
        let p = |s: &str| PathBuf::from(s);
        let cut = [p(r"C:\a\x.jpg"), p(r"C:\a\y.jpg"), p(r"C:\b\sub\z.jpg")];
        let pairs = [
            (p(r"C:\a\x.jpg"), p(r"C:\a\x2.jpg")),
            (p(r"C:\a\y.jpg"), p(r"D:\pasted\y.jpg")),
            (p(r"C:\b"), p(r"E:\b")),
        ];
        assert_eq!(
            follow(&cut, &pairs),
            vec![Some(p(r"C:\a\x2.jpg")), None, Some(p(r"E:\b\sub\z.jpg"))]
        );
        assert_eq!(follow(&cut, &[]), cut.iter().cloned().map(Some).collect::<Vec<_>>());
    }

    static OBSERVED: Mutex<Vec<(PathBuf, Observation)>> = Mutex::new(Vec::new());

    fn scripted(path: &Path) -> Observation {
        OBSERVED
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, o)| *o)
            .unwrap_or(Observation::Unknown)
    }

    fn set(path: &str, seen: Observation) {
        let mut observed = OBSERVED.lock().unwrap();
        observed.retain(|(p, _)| p != Path::new(path));
        observed.push((PathBuf::from(path), seen));
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

    /// One thread: identities are learned, items are reported one at a time as they go,
    /// and a newer cut replaces the old one, whose items are then no longer reported.
    #[test]
    fn the_watcher_reports_items_as_they_go_and_follows_the_newest_cut() {
        set(r"W:\one", Observation::Present(A));
        set(r"W:\two", Observation::Present(B));
        let (tx, rx) = crossbeam_channel::unbounded();
        let watcher = CutWatcher::start_with(Duration::from_millis(10), scripted, move |r| {
            let _ = tx.send(r);
        });
        watcher.watch(1, vec![(PathBuf::from(r"W:\one"), None), (PathBuf::from(r"W:\two"), Some(B))]);
        wait_for(
            &rx,
            &Report::Identities {
                generation: 1,
                items: vec![(PathBuf::from(r"W:\one"), A)],
            },
        );
        set(r"W:\one", Observation::Missing);
        wait_for(
            &rx,
            &Report::Gone {
                generation: 1,
                paths: vec![PathBuf::from(r"W:\one")],
            },
        );

        watcher.watch(2, vec![(PathBuf::from(r"W:\three"), Some(A))]);
        set(r"W:\two", Observation::Present(A));
        set(r"W:\three", Observation::Present(B));
        wait_for(
            &rx,
            &Report::Gone {
                generation: 2,
                paths: vec![PathBuf::from(r"W:\three")],
            },
        );
        // Generation 1 is no longer watched: its replaced item is not reported.
        std::thread::sleep(Duration::from_millis(100));
        assert!(rx.try_iter().all(|r| !matches!(r, Report::Gone { generation: 1, .. })));
    }
}
