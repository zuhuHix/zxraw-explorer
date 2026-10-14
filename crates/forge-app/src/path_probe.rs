//! Existence checks off the UI thread. A check on an unreachable network share can block in
//! the OS for a long time, so checks run on a small fixed pool of workers: a dead share ties
//! up at most that many threads, and the UI thread never waits on the disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};

/// What a check found: `None` when the path could not be read, else whether it is a folder.
pub type Found = Option<bool>;

fn check(path: &Path) -> Found {
    std::fs::metadata(path).ok().map(|m| m.is_dir())
}

/// A queue of existence checks served by a fixed number of worker threads. Dropping the
/// probe lets idle workers exit; a worker blocked in a check exits once the check returns.
pub struct Probe {
    jobs: Sender<PathBuf>,
    results: Receiver<(PathBuf, Found)>,
}

impl Probe {
    pub fn new(workers: usize, repaint: Option<egui::Context>) -> Self {
        Self::with_check(workers, repaint, check)
    }

    fn with_check(
        workers: usize,
        repaint: Option<egui::Context>,
        check: fn(&Path) -> Found,
    ) -> Self {
        let (jobs, job_rx) = unbounded::<PathBuf>();
        let (result_tx, results) = unbounded();
        for _ in 0..workers.max(1) {
            let job_rx = job_rx.clone();
            let result_tx = result_tx.clone();
            let repaint = repaint.clone();
            let _ = std::thread::Builder::new()
                .name("path-probe".into())
                .spawn(move || {
                    for path in job_rx {
                        let found = check(&path);
                        if result_tx.send((path, found)).is_err() {
                            break;
                        }
                        if let Some(ctx) = &repaint {
                            ctx.request_repaint();
                        }
                    }
                });
        }
        Self { jobs, results }
    }

    pub fn submit(&self, path: PathBuf) {
        let _ = self.jobs.send(path);
    }

    pub fn try_recv(&self) -> Option<(PathBuf, Found)> {
        self.results.try_recv().ok()
    }
}

/// A one-off batch of checks (the folders of a layout or a restored session) with a
/// deadline: a path with no answer by then counts as unreachable.
pub struct Batch {
    probe: Probe,
    /// `None` until the path's check answers.
    found: HashMap<PathBuf, Option<Found>>,
    waiting: usize,
    deadline: Instant,
}

/// How long a batch waits for slow (usually offline network) paths.
pub const BATCH_LIMIT: Duration = Duration::from_secs(8);

impl Batch {
    pub fn start(paths: impl IntoIterator<Item = PathBuf>, repaint: Option<egui::Context>) -> Self {
        Self::start_with(Probe::new(3, repaint), paths, Instant::now() + BATCH_LIMIT)
    }

    fn start_with(probe: Probe, paths: impl IntoIterator<Item = PathBuf>, deadline: Instant) -> Self {
        let mut found = HashMap::new();
        for path in paths {
            if !found.contains_key(&path) {
                probe.submit(path.clone());
                found.insert(path, None);
            }
        }
        Self {
            probe,
            waiting: found.len(),
            found,
            deadline,
        }
    }

    /// Takes in finished checks. True once every path has an answer or the deadline passed.
    pub fn poll(&mut self, now: Instant) -> bool {
        while let Some((path, found)) = self.probe.try_recv() {
            if let Some(slot @ None) = self.found.get_mut(&path) {
                *slot = Some(found);
                self.waiting -= 1;
            }
        }
        self.waiting == 0 || now >= self.deadline
    }

    /// Whether `path` was found to be a folder. Unanswered paths are not.
    pub fn is_dir(&self, path: &Path) -> bool {
        self.found.get(path).copied().flatten().flatten() == Some(true)
    }

    /// Whether `path` is one of the batch's paths.
    pub fn knows(&self, path: &Path) -> bool {
        self.found.contains_key(path)
    }

    /// Paths whose check had not answered when the batch ended (usually offline shares).
    pub fn unanswered(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .found
            .iter()
            .filter(|(_, f)| f.is_none())
            .map(|(p, _)| p.clone())
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);

    fn slow_check(_: &Path) -> Found {
        let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
        PEAK.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(30));
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        Some(true)
    }

    #[test]
    fn many_checks_never_run_on_more_threads_than_the_pool() {
        let probe = Probe::with_check(2, None, slow_check);
        for i in 0..12 {
            probe.submit(PathBuf::from(format!(r"\\offline\share{i}")));
        }
        let mut got = 0;
        let until = Instant::now() + Duration::from_secs(10);
        while got < 12 && Instant::now() < until {
            if probe.try_recv().is_some() {
                got += 1;
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(got, 12);
        assert!(PEAK.load(Ordering::SeqCst) <= 2, "peak {}", PEAK.load(Ordering::SeqCst));
    }

    #[test]
    fn batch_reports_folders_files_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let gone = dir.path().join("gone");
        let mut batch = Batch::start([dir.path().to_path_buf(), file.clone(), gone.clone()], None);
        let until = Instant::now() + Duration::from_secs(10);
        while !batch.poll(Instant::now()) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(batch.is_dir(dir.path()));
        assert!(!batch.is_dir(&file));
        assert!(!batch.is_dir(&gone));
        assert!(batch.unanswered().is_empty());
    }

    fn never(_: &Path) -> Found {
        std::thread::sleep(Duration::from_secs(3600));
        None
    }

    #[test]
    fn a_hung_check_ends_the_batch_at_its_deadline() {
        let now = Instant::now();
        let probe = Probe::with_check(1, None, never);
        let mut batch = Batch::start_with(probe, [PathBuf::from(r"\\dead\share")], now + Duration::from_millis(50));
        assert!(!batch.poll(now));
        assert!(batch.poll(now + Duration::from_millis(60)));
        assert!(!batch.is_dir(Path::new(r"\\dead\share")));
        assert_eq!(batch.unanswered(), vec![PathBuf::from(r"\\dead\share")]);
    }
}
