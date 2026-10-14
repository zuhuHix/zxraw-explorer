//! Git status badges for the folder views.
//!
//! `git status` runs on one background thread, never on the UI thread. Results are
//! cached per repository and refreshed when the file watcher reports a change, or
//! after a short age. Failures are quiet: a repository that cannot be read simply
//! shows no badges.

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};

/// Largest `git status` output accepted, in bytes.
const MAX_OUTPUT: usize = 8 * 1024 * 1024;
/// Longest a `git status` run may take before it is killed.
const TIMEOUT: Duration = Duration::from_secs(4);
/// How often the worker checks whether `git status` has finished.
const POLL: Duration = Duration::from_millis(10);
/// Age after which a cached status is refetched while its folder is shown.
const FRESH_FOR: Duration = Duration::from_secs(15);
/// Wait before retrying a repository after an unexpected error.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// Folder states kept before entries that are no longer shown are dropped.
const MAX_DIRS: usize = 256;
/// Deepest folder level searched for a `.git` entry.
const MAX_ROOT_DEPTH: usize = 64;

/// Badge shown next to an entry in a Git repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitBadge {
    /// Changed in the work tree or index.
    Modified,
    /// Newly added to the index, or untracked (new to Git).
    Added,
    /// Matched by an ignore rule. Views dim the name instead of drawing a badge.
    Ignored,
    /// Unmerged; needs resolving.
    Conflicted,
}

impl GitBadge {
    /// Short text drawn inside the badge. Empty for `Ignored`.
    pub fn letter(self) -> &'static str {
        match self {
            GitBadge::Modified => "M",
            GitBadge::Added => "+",
            GitBadge::Ignored => "",
            GitBadge::Conflicted => "!",
        }
    }

    /// Colour of the badge letter and, faded, its background.
    pub fn color(self) -> egui::Color32 {
        match self {
            GitBadge::Modified => egui::Color32::from_rgb(0xE0, 0xA0, 0x30),
            GitBadge::Added => egui::Color32::from_rgb(0x4C, 0xB8, 0x5C),
            GitBadge::Ignored => egui::Color32::from_gray(140),
            GitBadge::Conflicted => egui::Color32::from_rgb(0xE0, 0x50, 0x50),
        }
    }

    /// Priority used when rolling entries up into parent folders.
    fn rank(self) -> u8 {
        match self {
            GitBadge::Conflicted => 3,
            GitBadge::Modified => 2,
            GitBadge::Added => 1,
            GitBadge::Ignored => 0,
        }
    }
}

/// Parsed `git status --porcelain=v2 -z --branch` output for one repository.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoStatus {
    /// Repository root folder.
    pub root: PathBuf,
    /// Current branch name, or `None` when detached or unknown.
    pub branch: Option<String>,
    /// Badges keyed by lowercase repo-relative path. Folders reported by Git keep a trailing `/`.
    entries: HashMap<String, GitBadge>,
    /// Strongest badge of the entries below each folder, keyed like `entries` without the `/`.
    rollup: HashMap<String, GitBadge>,
}

impl RepoStatus {
    /// Parses the NUL-separated porcelain v2 output of `git status` for the repository at `root`.
    pub fn parse(root: PathBuf, out: &[u8]) -> RepoStatus {
        let mut status = RepoStatus {
            root,
            branch: None,
            entries: HashMap::new(),
            rollup: HashMap::new(),
        };
        let mut records = out.split(|&b| b == 0);
        while let Some(record) = records.next() {
            if record.is_empty() {
                continue;
            }
            let line = String::from_utf8_lossy(record);
            if let Some(head) = line.strip_prefix("# branch.head ") {
                status.branch = if head == "(detached)" {
                    None
                } else {
                    Some(head.to_string())
                };
            } else if line.starts_with("# ") {
                continue;
            } else if let Some(rest) = line.strip_prefix("1 ") {
                // XY sub mH mI mW hH hI path
                let parts: Vec<&str> = rest.splitn(8, ' ').collect();
                if parts.len() == 8 {
                    let badge = xy_badge(parts[0]);
                    status.add(parts[7], badge);
                }
            } else if let Some(rest) = line.strip_prefix("2 ") {
                // XY sub mH mI mW hH hI Xscore path; the original path follows as its own record
                let parts: Vec<&str> = rest.splitn(9, ' ').collect();
                // Skip the original path record that follows the rename or copy.
                records.next();
                if parts.len() == 9 {
                    let badge = xy_badge(parts[0]);
                    status.add(parts[8], badge);
                }
            } else if let Some(rest) = line.strip_prefix("u ") {
                // XY sub m1 m2 m3 mW h1 h2 h3 path
                let parts: Vec<&str> = rest.splitn(10, ' ').collect();
                if parts.len() == 10 {
                    status.add(parts[9], GitBadge::Conflicted);
                }
            } else if let Some(path) = line.strip_prefix("? ") {
                status.add(path, GitBadge::Added);
            } else if let Some(path) = line.strip_prefix("! ") {
                status.add(path, GitBadge::Ignored);
            }
        }
        status
    }

    /// Records one entry and rolls its badge up into the parent folders.
    fn add(&mut self, path: &str, badge: GitBadge) {
        let key = path.to_lowercase();
        if badge != GitBadge::Ignored {
            rollup_into(&mut self.rollup, &key, badge);
        }
        self.entries.insert(key, badge);
    }

    /// Badge for `path` (absolute, case-insensitive, `\` or `/`). `is_dir` says whether it is a folder.
    pub fn badge(&self, path: &Path, is_dir: bool) -> Option<GitBadge> {
        let root = norm_key(&self.root);
        let abs = norm_key(path);
        let rel = match abs.strip_prefix(root.as_str()) {
            Some(rest) => rest.strip_prefix('/')?,
            None => return None,
        };
        if rel.is_empty() || rel == ".git" || rel.starts_with(".git/") {
            return None;
        }
        if let Some(badge) = self
            .entries
            .get(rel)
            .or_else(|| self.entries.get(&format!("{rel}/")))
        {
            return Some(*badge);
        }
        if is_dir {
            if let Some(badge) = self.rollup.get(rel) {
                return Some(*badge);
            }
        }
        // Everything inside an untracked or ignored folder shares its badge.
        for (idx, _) in rel.match_indices('/') {
            if let Some(badge) = self.entries.get(&rel[..=idx]) {
                if matches!(badge, GitBadge::Ignored | GitBadge::Added) {
                    return Some(*badge);
                }
            }
        }
        None
    }
}

/// Maps the two-letter XY field of a `1` or `2` record to a badge.
fn xy_badge(xy: &str) -> GitBadge {
    if xy.starts_with('A') {
        GitBadge::Added
    } else {
        GitBadge::Modified
    }
}

/// Gives every ancestor folder of `key` the strongest of its own badge and the one it has.
fn rollup_into(map: &mut HashMap<String, GitBadge>, key: &str, badge: GitBadge) {
    let trimmed = key.trim_end_matches('/');
    for (idx, _) in trimmed.match_indices('/') {
        let slot = map.entry(trimmed[..idx].to_string()).or_insert(badge);
        if badge.rank() > slot.rank() {
            *slot = badge;
        }
    }
}

/// Lowercase path with `/` separators and no trailing `/`, for comparisons.
fn norm_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase()
}

/// Walks up from `dir` (at most 64 levels) to the first folder containing `.git`.
///
/// `.git` may be a folder, or a file as used by worktrees and submodules.
/// Returns that folder.
pub fn find_repo_root(dir: &Path) -> Option<PathBuf> {
    let mut current = dir;
    for _ in 0..MAX_ROOT_DEPTH {
        if std::fs::symlink_metadata(current.join(".git")).is_ok() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
    None
}

/// Paints `badge` as a small letter pill whose right edge is at `right_center`.
///
/// `Ignored` paints nothing; views dim the name instead. Returns the width used
/// (0 for `Ignored`).
pub fn paint_badge(painter: &egui::Painter, right_center: egui::Pos2, badge: GitBadge) -> f32 {
    if badge == GitBadge::Ignored {
        return 0.0;
    }
    const WIDTH: f32 = 16.0;
    const HEIGHT: f32 = 14.0;
    let rect = egui::Rect::from_min_size(
        egui::pos2(right_center.x - WIDTH, right_center.y - HEIGHT / 2.0),
        egui::vec2(WIDTH, HEIGHT),
    );
    let color = badge.color();
    painter.rect_filled(rect, 3.0, color.gamma_multiply(0.25));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        badge.letter(),
        egui::FontId::proportional(10.0),
        color,
    );
    WIDTH
}

/// Work sent from the UI thread to the worker.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Request {
    /// Find the repository containing this folder.
    FindRoot(PathBuf),
    /// Run `git status` in this repository.
    Status(PathBuf),
}

/// Result sent from the worker back to the UI thread.
#[derive(Debug)]
enum Reply {
    /// Answer to `FindRoot`.
    Root { dir: PathBuf, root: Option<PathBuf> },
    /// Answer to `Status`.
    Status {
        root: PathBuf,
        result: Result<RepoStatus, Failure>,
    },
}

/// Why a status could not be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Failure {
    /// No `git.exe` was found.
    NoGit,
    /// Output too large, or the run timed out.
    TooBig,
    /// Reading this repository in the background is not allowed.
    Unsafe,
    /// Anything else; retried later.
    Error(String),
}

/// Where a folder the UI shows belongs.
enum DirState {
    /// The worker has not answered yet.
    Pending,
    /// Not inside any repository.
    NoRepo,
    /// Inside the repository with this root.
    Repo(PathBuf),
}

/// Cache entry for one repository.
#[derive(Default)]
struct RepoEntry {
    status: Option<RepoStatus>,
    fetched_at: Option<Instant>,
    in_flight: bool,
    stale: bool,
    given_up: bool,
    retry_after: Option<Instant>,
}

/// UI-thread handle; all I/O happens on one worker thread.
pub struct GitStatus {
    requests: Sender<Request>,
    replies: Receiver<Reply>,
    enabled: bool,
    dirs: HashMap<PathBuf, DirState>,
    repos: HashMap<PathBuf, RepoEntry>,
}

impl GitStatus {
    /// Starts the worker thread. `ctx` is asked to repaint when a reply arrives.
    pub fn new(ctx: egui::Context) -> Self {
        let (requests, request_rx) = unbounded::<Request>();
        let (reply_tx, replies) = unbounded::<Reply>();
        let spawned = thread::Builder::new()
            .name("git-status".into())
            .spawn(move || worker(request_rx, reply_tx, ctx));
        if let Err(err) = spawned {
            tracing::warn!("could not start git-status thread: {err}");
        }
        GitStatus {
            requests,
            replies,
            enabled: true,
            dirs: HashMap::new(),
            repos: HashMap::new(),
        }
    }

    /// Turns badges on or off. Off forgets everything and sends nothing.
    pub fn set_enabled(&mut self, on: bool) {
        if on == self.enabled {
            return;
        }
        self.enabled = on;
        if !on {
            self.dirs.clear();
            self.repos.clear();
        }
    }

    /// Called every frame with the folders panes show. Requests root lookups and (re)fetches as needed.
    pub fn track(&mut self, dirs: &[PathBuf]) {
        if !self.enabled {
            return;
        }
        if self.dirs.len() > MAX_DIRS {
            let keep: HashSet<&PathBuf> = dirs.iter().collect();
            self.dirs.retain(|dir, _| keep.contains(dir));
            let roots: HashSet<PathBuf> = self
                .dirs
                .values()
                .filter_map(|state| match state {
                    DirState::Repo(root) => Some(root.clone()),
                    _ => None,
                })
                .collect();
            self.repos.retain(|root, _| roots.contains(root));
        }

        for dir in dirs {
            if !self.dirs.contains_key(dir) {
                self.dirs.insert(dir.clone(), DirState::Pending);
                let _ = self.requests.send(Request::FindRoot(dir.clone()));
            }
        }

        let now = Instant::now();
        let roots: Vec<PathBuf> = dirs
            .iter()
            .filter_map(|dir| match self.dirs.get(dir) {
                Some(DirState::Repo(root)) => Some(root.clone()),
                _ => None,
            })
            .collect();
        for root in roots {
            let entry = self.repos.entry(root.clone()).or_default();
            let due = !entry.in_flight
                && !entry.given_up
                && (entry.status.is_none()
                    || entry.stale
                    || entry
                        .fetched_at
                        .map_or(true, |t| now.duration_since(t) > FRESH_FOR))
                && entry.retry_after.map_or(true, |t| now >= t);
            if due {
                entry.in_flight = true;
                entry.stale = false;
                let _ = self.requests.send(Request::Status(root));
            }
        }
    }

    /// The watcher saw a change in `dir`: the repo containing it is stale.
    pub fn invalidate(&mut self, dir: &Path) {
        if !self.enabled {
            return;
        }
        let mut stale: Vec<PathBuf> = Vec::new();
        if let Some(DirState::Repo(root)) = self.dirs.get(dir) {
            stale.push(root.clone());
        }
        let dir_key = norm_key(dir);
        for root in self.repos.keys() {
            let root_key = norm_key(root);
            if dir_key == root_key || dir_key.starts_with(&format!("{root_key}/")) {
                stale.push(root.clone());
            }
        }
        for root in stale {
            if let Some(entry) = self.repos.get_mut(&root) {
                entry.stale = true;
            }
        }
    }

    /// Applies worker replies. Returns true when something changed (the caller should repaint).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.replies.try_recv() {
            if !self.enabled {
                continue;
            }
            match reply {
                Reply::Root { dir, root } => {
                    if let Some(state) = self.dirs.get_mut(&dir) {
                        *state = match root {
                            Some(root) => {
                                self.repos.entry(root.clone()).or_default();
                                DirState::Repo(root)
                            }
                            None => DirState::NoRepo,
                        };
                        changed = true;
                    }
                }
                Reply::Status { root, result } => {
                    let entry = self.repos.entry(root.clone()).or_default();
                    entry.in_flight = false;
                    match result {
                        Ok(status) => {
                            entry.status = Some(status);
                            entry.fetched_at = Some(Instant::now());
                            entry.retry_after = None;
                            changed = true;
                        }
                        Err(Failure::Error(message)) => {
                            tracing::debug!("git status failed in {}: {message}", root.display());
                            entry.retry_after = Some(Instant::now() + RETRY_AFTER);
                        }
                        Err(failure) => {
                            if !entry.given_up {
                                tracing::info!(
                                    "git badges off for {}: {failure:?}",
                                    root.display()
                                );
                            }
                            entry.given_up = true;
                            if entry.status.take().is_some() {
                                changed = true;
                            }
                        }
                    }
                }
            }
        }
        changed
    }

    /// Status of the repo containing folder `dir`, if known.
    pub fn repo_for(&self, dir: &Path) -> Option<&RepoStatus> {
        if !self.enabled {
            return None;
        }
        match self.dirs.get(dir)? {
            DirState::Repo(root) => self.repos.get(root)?.status.as_ref(),
            _ => None,
        }
    }

    /// Badge for `path`: the status of its parent folder, then `RepoStatus::badge`.
    pub fn badge(&self, path: &Path, is_dir: bool) -> Option<GitBadge> {
        self.repo_for(path.parent()?)?.badge(path, is_dir)
    }
}

/// Worker thread: answers requests, running each distinct request once per batch.
fn worker(requests: Receiver<Request>, replies: Sender<Reply>, ctx: egui::Context) {
    while let Ok(first) = requests.recv() {
        let mut batch = vec![first];
        batch.extend(requests.try_iter());
        let mut seen = HashSet::new();
        for request in batch {
            if !seen.insert(request.clone()) {
                continue;
            }
            let reply = match request {
                Request::FindRoot(dir) => Reply::Root {
                    root: find_repo_root(&dir),
                    dir,
                },
                Request::Status(root) => {
                    let result = match git_exe() {
                        Some(git) => run_git_status(git, &root)
                            .map(|out| RepoStatus::parse(root.clone(), &out)),
                        None => Err(Failure::NoGit),
                    };
                    Reply::Status { root, result }
                }
            };
            if replies.send(reply).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    }
}

/// Finds `git.exe` once: on PATH, else in `%ProgramFiles%\Git\cmd`.
fn git_exe() -> Option<&'static Path> {
    static GIT: OnceLock<Option<PathBuf>> = OnceLock::new();
    GIT.get_or_init(locate_git).as_deref()
}

fn locate_git() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("git.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let program_files = std::env::var_os("ProgramFiles")?;
    let candidate = PathBuf::from(program_files)
        .join("Git")
        .join("cmd")
        .join("git.exe");
    candidate.is_file().then_some(candidate)
}

/// May `git status` read the repository at `path` in the background? Not when the folder is a
/// cloud placeholder whose data is not on this PC: git would download it.
// TODO: use forge_core::cloud::background_read_allowed once the cloud-placeholder check (#10)
// is on main; this reads the same attributes without opening the folder.
fn background_read_allowed(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS
        const ONLINE_ONLY: u32 = 0x0000_1000 | 0x0004_0000 | 0x0040_0000;
        std::fs::symlink_metadata(path).map_or(true, |md| md.file_attributes() & ONLINE_ONLY == 0)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        true
    }
}

/// Runs `git status` in `root` and returns its raw porcelain v2 output.
fn run_git_status(git: &Path, root: &Path) -> Result<Vec<u8>, Failure> {
    if !background_read_allowed(root) {
        return Err(Failure::Unsafe);
    }
    let mut cmd = Command::new(git);
    cmd.arg("-C")
        .arg(root)
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=normal",
            "--ignored=traditional",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console window flashes up.
        cmd.creation_flags(0x0800_0000);
    }

    let mut child = cmd
        .spawn()
        .map_err(|err| Failure::Error(format!("could not start git: {err}")))?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Failure::Error("git stdout was not piped".into()));
    };

    // The reader stops early once the output passes the limit; the worker then kills git.
    let too_big = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&too_big);
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() + n > MAX_OUTPUT {
                        flag.store(true, Ordering::SeqCst);
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        buf
    });

    let started = Instant::now();
    let status = loop {
        if too_big.load(Ordering::SeqCst) || started.elapsed() >= TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Failure::TooBig);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(POLL),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Error(err.to_string()));
            }
        }
    };

    let out = reader.join().unwrap_or_default();
    if too_big.load(Ordering::SeqCst) {
        return Err(Failure::TooBig);
    }
    if !status.success() {
        return Err(Failure::Error(format!(
            "git status exited with code {}",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `-z` buffer from records, each followed by a NUL.
    fn z(records: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for record in records {
            out.extend_from_slice(record.as_bytes());
            out.push(0);
        }
        out
    }

    fn parse_records(records: &[&str]) -> RepoStatus {
        RepoStatus::parse(PathBuf::from(r"C:\repo"), &z(records))
    }

    fn sample() -> RepoStatus {
        parse_records(&[
            "# branch.oid 1234abcd",
            "# branch.head main",
            "1 .M N... 100644 100644 100644 aaa bbb src/my file.rs",
            "1 A. N... 000000 100644 100644 000 ccc added.rs",
            "2 R. N... 100644 100644 100644 aaa bbb R100 renamed.rs",
            "old name.rs",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.rs",
            "? new dir/",
            "? notes.txt",
            "! target/",
            "! debug.log",
        ])
    }

    #[test]
    fn parses_all_record_kinds() {
        let status = sample();
        assert_eq!(status.branch.as_deref(), Some("main"));
        let p = |s: &str| PathBuf::from(format!(r"C:\repo\{s}"));
        assert_eq!(
            status.badge(&p(r"src\my file.rs"), false),
            Some(GitBadge::Modified)
        );
        assert_eq!(
            status.badge(&p(r"SRC\My File.rs"), false),
            Some(GitBadge::Modified)
        );
        assert_eq!(status.badge(&p("added.rs"), false), Some(GitBadge::Added));
        assert_eq!(
            status.badge(&p("renamed.rs"), false),
            Some(GitBadge::Modified)
        );
        // The original path of the rename is not an entry.
        assert_eq!(status.badge(&p("old name.rs"), false), None);
        assert_eq!(
            status.badge(&p("conflict.rs"), false),
            Some(GitBadge::Conflicted)
        );
        assert_eq!(status.badge(&p("new dir"), true), Some(GitBadge::Added));
        assert_eq!(status.badge(&p("notes.txt"), false), Some(GitBadge::Added));
        assert_eq!(status.badge(&p("target"), true), Some(GitBadge::Ignored));
        assert_eq!(
            status.badge(&p("debug.log"), false),
            Some(GitBadge::Ignored)
        );
    }

    #[test]
    fn rolls_up_to_parent_folders() {
        let status = sample();
        let p = |s: &str| PathBuf::from(format!(r"C:\repo\{s}"));
        assert_eq!(status.badge(&p("src"), true), Some(GitBadge::Modified));

        let status = parse_records(&[
            "1 .M N... 100644 100644 100644 aaa bbb lib/mod.rs",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc lib/conflict.rs",
            "1 A. N... 000000 100644 100644 000 ccc only/new.rs",
        ]);
        assert_eq!(status.badge(&p("lib"), true), Some(GitBadge::Conflicted));
        assert_eq!(status.badge(&p("only"), true), Some(GitBadge::Added));
    }

    #[test]
    fn contents_of_untracked_and_ignored_folders_inherit_badge() {
        let status = sample();
        let p = |s: &str| PathBuf::from(format!(r"C:\repo\{s}"));
        assert_eq!(
            status.badge(&p(r"new dir\x\y.txt"), false),
            Some(GitBadge::Added)
        );
        assert_eq!(
            status.badge(&p(r"target\debug\a.exe"), false),
            Some(GitBadge::Ignored)
        );
        assert_eq!(status.badge(&p("target"), true), Some(GitBadge::Ignored));
    }

    #[test]
    fn edge_cases_have_no_badge() {
        let status = parse_records(&["# branch.head (detached)"]);
        assert_eq!(status.branch, None);

        let empty = RepoStatus::parse(PathBuf::from(r"C:\repo"), b"");
        assert_eq!(empty.branch, None);
        assert_eq!(empty.badge(&PathBuf::from(r"C:\repo\a.txt"), false), None);

        let status = sample();
        let p = |s: &str| PathBuf::from(format!(r"C:\repo\{s}"));
        assert_eq!(status.badge(&p(".git"), true), None);
        assert_eq!(status.badge(&p(r".git\HEAD"), false), None);
        assert_eq!(status.badge(&PathBuf::from(r"D:\other\x.txt"), false), None);
        assert_eq!(
            status.badge(&PathBuf::from(r"C:\repo-other\x.txt"), false),
            None
        );
    }

    #[test]
    fn find_repo_root_walks_up() {
        let tmp = tempfile::tempdir().unwrap();

        let root = tmp.path().join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("a").join("b").join("c")).unwrap();
        assert_eq!(
            find_repo_root(&root.join("a").join("b").join("c")),
            Some(root)
        );

        // A `.git` file, as in worktrees and submodules, also counts.
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(worktree.join("sub")).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: elsewhere\n").unwrap();
        assert_eq!(
            find_repo_root(&worktree.join("sub")),
            Some(worktree.clone())
        );

        let plain = tmp.path().join("plain").join("x");
        std::fs::create_dir_all(&plain).unwrap();
        let ancestor_is_repo = tmp
            .path()
            .ancestors()
            .any(|a| a.join(".git").symlink_metadata().is_ok());
        if !ancestor_is_repo {
            assert_eq!(find_repo_root(&plain), None);
        }
    }

    #[test]
    fn real_git_reports_untracked_file() {
        let Some(git) = git_exe() else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let init = Command::new(git)
            .arg("-C")
            .arg(tmp.path())
            .args(["init", "-q"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !matches!(init, Ok(s) if s.success()) {
            return;
        }
        std::fs::write(tmp.path().join("untracked.txt"), "hi").unwrap();

        let out = run_git_status(git, tmp.path()).expect("git status should succeed");
        let status = RepoStatus::parse(tmp.path().to_path_buf(), &out);
        assert_eq!(
            status.badge(&tmp.path().join("untracked.txt"), false),
            Some(GitBadge::Added)
        );
    }
}
