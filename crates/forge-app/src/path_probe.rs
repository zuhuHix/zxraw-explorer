//! Existence checks off the UI thread. A check on an unreachable network share can block in
//! the OS for a long time, so every check in the process goes through one service with a fixed
//! number of workers in two lanes: network paths can tie up only the network lane, and local
//! paths never queue behind them. The workers live for the whole process, so repeated layout
//! loads or pin checks never pile up threads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};

/// What a finished check found: `None` when the path could not be read, else whether it is a
/// folder.
pub type Found = Option<bool>;

/// Progress of one submitted check, reported to the client that submitted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A worker picked the check up (until then it is only queued).
    Started(PathBuf),
    Done(PathBuf, Found),
}

fn check(path: &Path) -> Found {
    std::fs::metadata(path).ok().map(|m| m.is_dir())
}

/// True for UNC paths and paths on a mapped network drive (`network_roots`).
pub fn is_network(path: &Path, network_roots: &[PathBuf]) -> bool {
    let text = path.to_string_lossy().to_lowercase();
    text.starts_with(r"\\")
        || network_roots
            .iter()
            .any(|root| text.starts_with(&root.to_string_lossy().to_lowercase()))
}

struct Job {
    path: PathBuf,
    reply: Sender<Event>,
    repaint: Option<egui::Context>,
}

/// The worker lanes. [`Service::global`] is the one the app uses.
pub struct Service {
    local: Sender<Job>,
    network: Sender<Job>,
}

const LOCAL_WORKERS: usize = 2;
const NETWORK_WORKERS: usize = 3;

impl Service {
    pub fn global() -> &'static Service {
        static SERVICE: OnceLock<Service> = OnceLock::new();
        SERVICE.get_or_init(|| Service::new(LOCAL_WORKERS, NETWORK_WORKERS, check))
    }

    fn new(local: usize, network: usize, check: fn(&Path) -> Found) -> Self {
        Self {
            local: lane("probe-local", local, check),
            network: lane("probe-network", network, check),
        }
    }
}

fn lane(name: &str, workers: usize, check: fn(&Path) -> Found) -> Sender<Job> {
    let (jobs, job_rx) = unbounded::<Job>();
    for _ in 0..workers.max(1) {
        let job_rx = job_rx.clone();
        let _ = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                for job in job_rx {
                    let _ = job.reply.send(Event::Started(job.path.clone()));
                    let found = check(&job.path);
                    let _ = job.reply.send(Event::Done(job.path, found));
                    if let Some(ctx) = &job.repaint {
                        ctx.request_repaint();
                    }
                }
            });
    }
    jobs
}

/// One user of the service (the pin watcher, a layout load). Results come back only to the
/// client that asked; dropping it simply discards later results.
pub struct Client {
    local: Sender<Job>,
    network: Sender<Job>,
    reply: Sender<Event>,
    events: Receiver<Event>,
    repaint: Option<egui::Context>,
}

impl Client {
    pub fn new(repaint: Option<egui::Context>) -> Self {
        Self::on(Service::global(), repaint)
    }

    fn on(service: &Service, repaint: Option<egui::Context>) -> Self {
        let (reply, events) = unbounded();
        Self {
            local: service.local.clone(),
            network: service.network.clone(),
            reply,
            events,
            repaint,
        }
    }

    pub fn submit(&self, path: PathBuf, network: bool) {
        let lane = if network { &self.network } else { &self.local };
        let _ = lane.send(Job {
            path,
            reply: self.reply.clone(),
            repaint: self.repaint.clone(),
        });
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Queued,
    Running,
    Done(Found),
}

/// A one-off batch of checks (the folders of a layout or a restored session) with a deadline.
/// Only a finished negative check makes a path missing; a path still queued or running at the
/// deadline is unknown, and callers open it anyway so its tab can show its own error.
pub struct Batch {
    client: Client,
    state: HashMap<PathBuf, State>,
    deadline: Instant,
}

/// How long a batch waits for slow (usually offline network) paths.
pub const BATCH_LIMIT: Duration = Duration::from_secs(8);

impl Batch {
    /// `paths` pairs each path with whether it is on the network.
    pub fn start(paths: impl IntoIterator<Item = (PathBuf, bool)>, repaint: Option<egui::Context>) -> Self {
        Self::start_with(Client::new(repaint), paths, Instant::now() + BATCH_LIMIT)
    }

    fn start_with(
        client: Client,
        paths: impl IntoIterator<Item = (PathBuf, bool)>,
        deadline: Instant,
    ) -> Self {
        let mut state = HashMap::new();
        for (path, network) in paths {
            state.entry(path).or_insert_with_key(|path| {
                client.submit(path.clone(), network);
                State::Queued
            });
        }
        Self {
            client,
            state,
            deadline,
        }
    }

    /// Takes in progress. True once every check has finished or the deadline passed.
    pub fn poll(&mut self, now: Instant) -> bool {
        while let Some(event) = self.client.try_recv() {
            let (path, next) = match event {
                Event::Started(path) => (path, State::Running),
                Event::Done(path, found) => (path, State::Done(found)),
            };
            if let Some(state) = self.state.get_mut(&path)
                && !matches!(state, State::Done(_))
            {
                *state = next;
            }
        }
        self.state.values().all(|s| matches!(s, State::Done(_))) || now >= self.deadline
    }

    /// The check finished and found no folder at `path`.
    pub fn is_missing(&self, path: &Path) -> bool {
        matches!(self.state.get(path), Some(State::Done(found)) if *found != Some(true))
    }

    /// Paths whose check had not finished when the batch ended (usually offline shares).
    pub fn unchecked(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .state
            .iter()
            .filter(|(_, s)| !matches!(s, State::Done(_)))
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

    /// Paths under `\\hung` block for a minute, like an offline share; others answer at once
    /// (`missing` in the name means not found).
    fn fake_check(path: &Path) -> Found {
        let text = path.to_string_lossy();
        if text.starts_with(r"\\hung") {
            std::thread::sleep(Duration::from_secs(60));
            return None;
        }
        (!text.contains("missing")).then_some(true)
    }

    fn wait_for(client: &Client, want: impl Fn(&Event) -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            match client.try_recv() {
                Some(event) if want(&event) => return true,
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        false
    }

    #[test]
    fn local_checks_are_not_stuck_behind_hung_network_checks() {
        let service = Service::new(1, 2, fake_check);
        let client = Client::on(&service, None);
        for i in 0..5 {
            client.submit(PathBuf::from(format!(r"\\hung\share{i}")), true);
        }
        client.submit(PathBuf::from(r"C:\photos"), false);
        assert!(wait_for(&client, |e| *e == Event::Done(PathBuf::from(r"C:\photos"), Some(true))));
    }

    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);

    fn counting_check(_: &Path) -> Found {
        let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
        PEAK.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        Some(true)
    }

    #[test]
    fn many_checks_never_run_on_more_threads_than_the_lane() {
        let service = Service::new(1, 2, counting_check);
        let client = Client::on(&service, None);
        for i in 0..12 {
            client.submit(PathBuf::from(format!(r"\\nas\share{i}")), true);
        }
        let mut done = 0;
        let until = Instant::now() + Duration::from_secs(10);
        while done < 12 && Instant::now() < until {
            match client.try_recv() {
                Some(Event::Done(..)) => done += 1,
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        assert_eq!(done, 12);
        assert!(PEAK.load(Ordering::SeqCst) <= 2, "peak {}", PEAK.load(Ordering::SeqCst));
    }

    #[test]
    fn batch_reports_folders_files_and_missing_paths_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let gone = dir.path().join("gone");
        let paths = [dir.path().to_path_buf(), file.clone(), gone.clone()].map(|p| (p, false));
        let mut batch = Batch::start(paths, None);
        let until = Instant::now() + Duration::from_secs(10);
        while !batch.poll(Instant::now()) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!batch.is_missing(dir.path()));
        assert!(batch.is_missing(&file), "a file is not a folder");
        assert!(batch.is_missing(&gone));
        assert!(batch.unchecked().is_empty());
    }

    #[test]
    fn at_the_deadline_unfinished_checks_are_unknown_not_missing() {
        // One network worker: the first hung path runs, the second only ever queues.
        let service = Service::new(1, 1, fake_check);
        let start = Instant::now();
        let paths = [
            (PathBuf::from(r"\\hung\a"), true),
            (PathBuf::from(r"\\hung\b"), true),
            (PathBuf::from(r"C:\missing"), false),
            (PathBuf::from(r"C:\here"), false),
        ];
        let mut batch =
            Batch::start_with(Client::on(&service, None), paths, start + Duration::from_millis(300));
        while !batch.poll(Instant::now()) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(batch.is_missing(Path::new(r"C:\missing")));
        assert!(!batch.is_missing(Path::new(r"C:\here")));
        assert!(!batch.is_missing(Path::new(r"\\hung\a")));
        assert!(!batch.is_missing(Path::new(r"\\hung\b")));
        assert_eq!(
            batch.unchecked(),
            vec![PathBuf::from(r"\\hung\a"), PathBuf::from(r"\\hung\b")]
        );
    }

    #[test]
    fn network_paths_are_recognised() {
        let roots = vec![PathBuf::from(r"Z:\")];
        assert!(is_network(Path::new(r"\\server\share"), &[]));
        assert!(is_network(Path::new(r"z:\docs"), &roots));
        assert!(!is_network(Path::new(r"C:\docs"), &roots));
    }
}
