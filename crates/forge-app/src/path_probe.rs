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

/// What a finished check found. Only `Missing` says the path is gone: any other error (an
/// offline share, a VPN that isn't up, access denied, a drive that isn't ready) is `Unknown`,
/// and callers keep the path so it can show its own error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Folder,
    File,
    Missing,
    Unknown(Failure),
}

/// Why a check could not tell whether a path exists. `code` is the Win32 error when the OS
/// gave one; the network section uses it to tell a server that wants a sign-in from one that
/// is offline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub code: Option<i32>,
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self {
            message: message.into(),
            code: None,
        }
    }
}

impl From<&std::io::Error> for Failure {
    fn from(e: &std::io::Error) -> Self {
        Self {
            message: e.to_string(),
            code: e.raw_os_error(),
        }
    }
}

/// Progress of one submitted check, reported to the client that submitted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A worker picked the check up (until then it is only queued).
    Started(PathBuf),
    Done(PathBuf, Found),
}

fn check(path: &Path) -> Found {
    classify(path, |p| std::fs::metadata(p).map(|m| m.is_dir()))
}

/// Turns a metadata lookup (`Ok(is_dir)`) into a [`Found`]. "Not found" counts only when the
/// path's volume root answers: an unplugged USB drive or a disconnected mapped drive also
/// reports its paths as not found, and those must stay. A path without a drive or share prefix
/// (relative or root-relative) has no volume to ask, so it counts as unknown.
fn classify(path: &Path, metadata: impl Fn(&Path) -> std::io::Result<bool>) -> Found {
    match metadata(path) {
        Ok(true) => Found::Folder,
        Ok(false) => Found::File,
        Err(e) if is_not_found(&e) => match volume_root(path) {
            Some(root) if root == path => Found::Unknown((&e).into()),
            Some(root) => match metadata(&root) {
                Ok(_) => Found::Missing,
                Err(e) => Found::Unknown(Failure {
                    message: format!("{}: {e}", root.display()),
                    code: e.raw_os_error(),
                }),
            },
            None => Found::Unknown(e.to_string()),
        },
        Err(e) => Found::Unknown((&e).into()),
    }
}

/// ERROR_FILE_NOT_FOUND or ERROR_PATH_NOT_FOUND. Other OS errors that Rust may also report as
/// `ErrorKind::NotFound` (a bad network path or share name) say nothing about whether the
/// folder exists.
fn is_not_found(error: &std::io::Error) -> bool {
    match error.raw_os_error() {
        Some(code) => code == 2 || code == 3,
        None => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// The drive or share root of `path` (`C:\`, `\\server\share\`), if it has one.
fn volume_root(path: &Path) -> Option<PathBuf> {
    let Some(std::path::Component::Prefix(prefix)) = path.components().next() else {
        return None;
    };
    let mut root = PathBuf::from(prefix.as_os_str());
    root.push(std::path::MAIN_SEPARATOR_STR);
    Some(root)
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

    pub(crate) fn new(local: usize, network: usize, check: fn(&Path) -> Found) -> Self {
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

    pub(crate) fn on(service: &Service, repaint: Option<egui::Context>) -> Self {
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Queued,
    Running,
    Done(Found),
}

/// A one-off batch of checks (the folders of a layout or a restored session) with a deadline.
/// Only a check that finished with "not found" (or found a file) makes a path missing; a path
/// whose check failed some other way, or was still queued or running at the deadline, is
/// unknown, and callers open it anyway so its tab can show its own error.
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

    /// A batch whose checks have already finished with `results`.
    #[cfg(test)]
    pub(crate) fn finished(results: impl IntoIterator<Item = (PathBuf, Found)>) -> Self {
        Self {
            client: Client::new(None),
            state: results.into_iter().map(|(p, f)| (p, State::Done(f))).collect(),
            deadline: Instant::now(),
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

    /// The check finished and found no folder at `path`: nothing there, or a file.
    pub fn is_missing(&self, path: &Path) -> bool {
        matches!(
            self.state.get(path),
            Some(State::Done(Found::Missing | Found::File))
        )
    }

    /// Paths whose check failed without a clear answer or had not finished when the batch
    /// ended (usually offline shares).
    pub fn unchecked(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .state
            .iter()
            .filter(|(_, s)| !matches!(s, State::Done(found) if !matches!(found, Found::Unknown(_))))
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
    /// (`missing` in the name means not found, `denied` an access error).
    fn fake_check(path: &Path) -> Found {
        let text = path.to_string_lossy();
        if text.starts_with(r"\\hung") {
            std::thread::sleep(Duration::from_secs(60));
            return Found::Unknown("timed out".into());
        }
        if text.contains("missing") {
            Found::Missing
        } else if text.contains("denied") {
            Found::Unknown("Access is denied.".into())
        } else {
            Found::Folder
        }
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
        assert!(wait_for(&client, |e| *e == Event::Done(PathBuf::from(r"C:\photos"), Found::Folder)));
    }

    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);

    fn counting_check(_: &Path) -> Found {
        let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
        PEAK.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        Found::Folder
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
    fn only_not_found_on_a_reachable_volume_counts_as_missing() {
        use std::io::{Error, ErrorKind};
        let disk = |p: &Path| match p.to_str() {
            Some(r"C:\" | r"C:\dir") => Ok(true),
            Some(r"C:\a.txt") => Ok(false),
            _ => Err(Error::from(ErrorKind::NotFound)),
        };
        assert_eq!(classify(Path::new(r"C:\dir"), disk), Found::Folder);
        assert_eq!(classify(Path::new(r"C:\a.txt"), disk), Found::File);
        assert_eq!(classify(Path::new(r"C:\gone"), disk), Found::Missing);
        // The volume itself is gone: an unplugged card, a disconnected drive or share.
        for path in [r"E:\Card\DCIM", r"E:\", r"\\nas\photos\2024"] {
            assert!(matches!(classify(Path::new(path), disk), Found::Unknown(_)), "{path}");
        }
        // Network errors, access denied and the like never mean missing: 5 access denied,
        // 21 not ready, 53 bad network path, 67 bad share name, 1231 network unreachable.
        for kind in [ErrorKind::PermissionDenied, ErrorKind::NetworkUnreachable] {
            let failing = |_: &Path| Err(Error::from(kind));
            assert!(matches!(classify(Path::new(r"C:\dir"), failing), Found::Unknown(_)), "{kind:?}");
        }
        for code in [5, 21, 53, 67, 1231] {
            // Only the folder fails; its volume answers.
            let failing = |p: &Path| match p.to_str() {
                Some(r"C:\") => Ok(true),
                _ => Err(Error::from_raw_os_error(code)),
            };
            assert!(matches!(classify(Path::new(r"C:\dir"), failing), Found::Unknown(_)), "{code}");
        }
        for code in [2, 3] {
            let gone = |p: &Path| match p.to_str() {
                Some(r"C:\") => Ok(true),
                _ => Err(Error::from_raw_os_error(code)),
            };
            assert_eq!(classify(Path::new(r"C:\dir"), gone), Found::Missing, "{code}");
        }
    }

    #[test]
    fn a_path_without_a_volume_is_unknown_not_missing() {
        // No drive or share to ask whether it answers, so "not found" proves nothing.
        for code in [2, 3] {
            let gone = |_: &Path| Err(std::io::Error::from_raw_os_error(code));
            for path in ["relative", r"photos\2024", r"\rooted\dir"] {
                assert!(matches!(classify(Path::new(path), gone), Found::Unknown(_)), "{path} {code}");
            }
        }
    }

    #[test]
    fn a_failed_check_is_unknown_not_missing() {
        let service = Service::new(1, 1, fake_check);
        let paths = [
            (PathBuf::from(r"\\nas\denied"), true),
            (PathBuf::from(r"C:\missing"), false),
        ];
        let mut batch = Batch::start_with(
            Client::on(&service, None),
            paths,
            Instant::now() + Duration::from_secs(10),
        );
        while !batch.poll(Instant::now()) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!batch.is_missing(Path::new(r"\\nas\denied")));
        assert!(batch.is_missing(Path::new(r"C:\missing")));
        assert_eq!(batch.unchecked(), vec![PathBuf::from(r"\\nas\denied")]);
    }

    #[test]
    fn volume_roots() {
        assert_eq!(volume_root(Path::new(r"C:\a\b")), Some(PathBuf::from(r"C:\")));
        assert_eq!(
            volume_root(Path::new(r"\\nas\photos\x")),
            Some(PathBuf::from(r"\\nas\photos\"))
        );
        assert_eq!(volume_root(Path::new("relative")), None);
    }

    #[test]
    fn network_paths_are_recognised() {
        let roots = vec![PathBuf::from(r"Z:\")];
        assert!(is_network(Path::new(r"\\server\share"), &[]));
        assert!(is_network(Path::new(r"z:\docs"), &roots));
        assert!(!is_network(Path::new(r"C:\docs"), &roots));
    }
}
