//! Git status badges for the folder views.
//!
//! Status is read on one background thread, never on the UI thread, and never by running `git`
//! or any other program: a folder from elsewhere can carry a `.git/config` that names programs
//! (fsmonitor, filter drivers), and merely listing it must not start them. Every file is opened
//! through [`safe_fs`], which never follows a link and never leaves the folder, so a hostile
//! repository cannot send the scan to a network share either. Results are cached per
//! repository and refreshed when the file watcher reports a change, or after a short age.
//! Failures are quiet: a repository that cannot be read simply shows no badges.

mod odb;
mod safe_fs;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};

use safe_fs::{Dir, Fs};

/// Longest a status walk may take before it is stopped.
const TIMEOUT: Duration = Duration::from_secs(4);
/// Age after which a cached status is refetched while its folder is shown.
const FRESH_FOR: Duration = Duration::from_secs(15);
/// Wait before retrying a repository after an unexpected error.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// Wait before retrying a repository that was skipped because earlier scans still run.
const RETRY_BUSY: Duration = Duration::from_secs(5);
/// Folder states kept before entries that are no longer shown are dropped.
const MAX_DIRS: usize = 256;
/// Deepest folder level (below a drive's root) that can be in a repository.
const MAX_ROOT_DEPTH: usize = 128;

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

/// The Git status of one repository.
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
    /// An empty status for the repository at `root`.
    fn new(root: PathBuf, branch: Option<String>) -> RepoStatus {
        RepoStatus {
            root,
            branch,
            entries: HashMap::new(),
            rollup: HashMap::new(),
        }
    }

    /// Records a staged change (index against HEAD) next to the worktree one already there.
    /// A file new to HEAD shows as new even if edited since staging; a conflict always wins.
    fn add_staged(&mut self, path: &str, badge: GitBadge) {
        match self.entries.get(&path.to_lowercase()) {
            Some(GitBadge::Conflicted) => {}
            Some(existing) if badge != GitBadge::Added && existing.rank() >= badge.rank() => {}
            _ => self.add(path, badge),
        }
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

/// The deepest folder from `dir` up that contains a `.git` (a folder, or a file as used by
/// worktrees and submodules, or a link: the scan then refuses it).
///
/// Nothing is looked up by path: `dir` and its ancestors are reached through [`locate`], from
/// the root of its local drive one name at a time, and each `.git` is probed relative to its
/// folder's handle. A folder on a share or mapped drive, or below a link, has no repository.
fn find_repo_root(dir: &Path, over: &dyn Fn() -> bool) -> Result<Option<PathBuf>, Failure> {
    let located = locate(dir, false, over)?;
    find_in(&located, over)
}

/// [`find_repo_root`] for a folder already located.
fn find_in(located: &Located, over: &dyn Fn() -> bool) -> Result<Option<PathBuf>, Failure> {
    for (depth, level) in located.levels.iter().enumerate().rev() {
        if over() {
            return Err(Failure::TooBig);
        }
        let found = match located.fs.dir(level, ".git") {
            Ok(_) => true,
            Err(safe_fs::Error::NotFound | safe_fs::Error::Io(_)) => false,
            // A file, link or placeholder named `.git`: a repository the scan will refuse.
            Err(_) => true,
        };
        if found {
            let mut root = located.path.clone();
            for _ in depth..located.levels.len() - 1 {
                root.pop();
            }
            return Ok(Some(root));
        }
    }
    Ok(None)
}

/// A local folder reached from the root of its drive one name at a time, refusing links.
struct Located {
    fs: Fs,
    /// The drive's root, then each folder down to the one asked for.
    levels: Vec<Dir>,
    /// The folder asked for, in [`lexical`] form.
    path: PathBuf,
}

/// Opens `dir` without following any link on the way: refuses a share, a mapped network drive
/// or a device path before opening anything; opens the drive's root (the only open by path);
/// then each folder down to `dir` through [`safe_fs`], so a junction or symlink anywhere in
/// the path is refused rather than followed. Refuses a folder inside a cloud sync root (whose
/// files could be online-only), judged from the attributes of the handles just opened.
fn locate(dir: &Path, record: bool, over: &dyn Fn() -> bool) -> Result<Located, Failure> {
    let path = lexical(dir).ok_or(Failure::Unsafe)?;
    let letter = safe_fs::local_drive(&path).ok_or(Failure::Unsafe)?;
    let names: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if names.len() > MAX_ROOT_DEPTH {
        return Err(Failure::TooBig);
    }
    let (fs, top) = Fs::open_volume(letter, record).map_err(meta)?;
    let mut levels = vec![top];
    let mut attributes = HashMap::new();
    let mut at = PathBuf::from(format!("{letter}:\\"));
    for name in &names {
        if over() {
            return Err(Failure::TooBig);
        }
        let next = fs.dir(levels.last().unwrap(), name).map_err(meta)?;
        at.push(name);
        attributes.insert(at.clone(), (next.info.attributes, 0));
        levels.push(next);
    }
    let known = |p: &Path| attributes.get(p).copied();
    if forge_core::cloud::in_cloud_sync_root_with(&path, &known) {
        return Err(Failure::Unsafe);
    }
    Ok(Located { fs, levels, path })
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
    /// Read the status of this repository.
    Status(PathBuf),
}

/// Result sent from the worker back to the UI thread.
#[derive(Debug)]
enum Reply {
    /// Answer to `FindRoot`. `busy`: earlier work still runs; ask again shortly.
    Root {
        dir: PathBuf,
        root: Option<PathBuf>,
        busy: bool,
    },
    /// Answer to `Status`.
    Status {
        root: PathBuf,
        result: Result<RepoStatus, Failure>,
    },
}

/// Why a status could not be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Failure {
    /// Too many entries, or the walk took too long.
    TooBig,
    /// Reading this repository in the background is not allowed.
    Unsafe,
    /// Earlier scans that ran out of time are still running; retried shortly.
    Busy,
    /// Anything else; retried later.
    Error(String),
}

/// Where a folder the UI shows belongs.
enum DirState {
    /// The worker has not answered yet.
    Pending,
    /// The worker was busy; ask again after this time.
    Retry(Instant),
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
    /// Requests handed to the worker; every scan starts with one.
    sent: usize,
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
            enabled: false,
            sent: 0,
            dirs: HashMap::new(),
            repos: HashMap::new(),
        }
    }

    /// Turns badges on or off (off until turned on). Off forgets everything and sends nothing.
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

        let now = Instant::now();
        for dir in dirs {
            let due = match self.dirs.get(dir) {
                None => true,
                Some(DirState::Retry(at)) => now >= *at,
                Some(_) => false,
            };
            if due {
                self.dirs.insert(dir.clone(), DirState::Pending);
                self.sent += 1;
                let _ = self.requests.send(Request::FindRoot(dir.clone()));
            }
        }

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
                self.sent += 1;
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
                Reply::Root { dir, root, busy } => {
                    if let Some(state) = self.dirs.get_mut(&dir) {
                        *state = match root {
                            Some(root) => {
                                self.repos.entry(root.clone()).or_default();
                                DirState::Repo(root)
                            }
                            None if busy => DirState::Retry(Instant::now() + RETRY_BUSY),
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
                        Err(Failure::Busy) => {
                            entry.retry_after = Some(Instant::now() + RETRY_BUSY);
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
                Request::FindRoot(dir) => {
                    let found = {
                        let dir = dir.clone();
                        on_thread(TIMEOUT, &ABANDONED, move |over| find_repo_root(&dir, over))
                    };
                    Reply::Root {
                        busy: found == Err(Failure::Busy),
                        root: found.ok().flatten(),
                        dir,
                    }
                }
                Request::Status(root) => {
                    let result = read_status(&root);
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

/// What one status read may cost before the repository is skipped for the session.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// Wall time for the whole read, from before the repository is opened.
    time: Duration,
    /// Size of `.git/index` on disk, checked before it is read.
    index_bytes: u64,
    /// Entries in the index (each is compared with its file).
    index_entries: usize,
    /// Folders and files the work-tree walk looks at, reported or not.
    visited: usize,
    /// Size of one ignore file (`.gitignore`, `info/exclude`, `core.excludesFile`).
    ignore_file_bytes: u64,
    /// Patterns in one ignore file.
    ignore_file_patterns: usize,
    /// Size of all ignore files read together.
    ignore_bytes: u64,
    /// Patterns of all ignore files read together.
    ignore_patterns: usize,
    /// Size of `packed-refs`.
    packed_refs_bytes: u64,
    /// Staged changes (paths that differ between the index and `HEAD`).
    staged_entries: usize,
    /// Bytes of the paths of all staged changes.
    staged_bytes: usize,
    /// Longest path in `HEAD`'s tree.
    path_bytes: usize,
    /// Object reading: object sizes, packs, delta chains.
    odb: odb::OdbLimits,
}

const LIMITS: Limits = Limits {
    time: TIMEOUT,
    index_bytes: 64 * 1024 * 1024,
    index_entries: 100_000,
    visited: 50_000,
    ignore_file_bytes: 256 * 1024,
    ignore_file_patterns: 4_096,
    ignore_bytes: 4 * 1024 * 1024,
    ignore_patterns: 20_000,
    packed_refs_bytes: 16 * 1024 * 1024,
    staged_entries: 200_000,
    staged_bytes: 64 * 1024 * 1024,
    path_bytes: 4096,
    odb: odb::OdbLimits {
        object_bytes: 32 * 1024 * 1024,
        total_bytes: 512 * 1024 * 1024,
        packs: 256,
        idx_bytes: 256 * 1024 * 1024,
        pack_bytes: 16 * 1024 * 1024 * 1024,
        compressed_bytes: 512 * 1024 * 1024,
        delta_depth: 512,
    },
};

/// Reads the status of the repository at `root`, in this process.
fn read_status(root: &Path) -> Result<RepoStatus, Failure> {
    read_status_with(root, LIMITS)
}

/// Scans that ran past their deadline and are still running. Bounded so that a folder full of
/// slow repositories cannot pile up threads.
static ABANDONED: AtomicUsize = AtomicUsize::new(0);
/// Most abandoned scans at once; with the one being waited for, at most two run.
const MAX_ABANDONED: usize = 1;

/// [`read_status`] with explicit limits.
///
/// Nothing in the folder is trusted, and nothing in it is opened by path: every file and
/// folder is opened through [`safe_fs`], one name at a time relative to a checked parent
/// handle, never following a symlink, junction or other reparse point, and never leaving the
/// root's folder. gix only parses and hashes bytes read that way (the index, config, refs,
/// objects and ignore files); it never touches the file system itself. So no program runs (no
/// fsmonitor, no filter driver), no network path is opened, and every read has a cap:
///
/// * `.git` must be a real folder in the root. A `.git` file (linked worktree, submodule)
///   could name any place, so those repositories show no badges;
/// * the config may not move the work tree, name an excludes file outside the folder, or ask
///   for SHA-256 objects; `objects/info/alternates` must be empty; split indexes are refused;
/// * tracked files are compared with the index from folder listings; a tracked path below a
///   link counts as modified (Git, too, never follows a link inside a work tree);
/// * staged changes compare the index with `HEAD`'s tree, read from loose objects and packs.
fn read_status_with(root: &Path, limits: Limits) -> Result<RepoStatus, Failure> {
    let root = root.to_path_buf();
    on_thread(limits.time, &ABANDONED, move |over| {
        read_within(&root, limits, over)
    })
}

/// Runs `work` on its own thread and waits at most `time` for it; `work` is given a check
/// that turns true at the deadline. Work still running then is told to stop and left to finish
/// on its own (an open of a slow file system cannot be interrupted); it counts in `abandoned`
/// until it does, and no new work starts while `abandoned` is full.
fn on_thread<T: Send + 'static>(
    time: Duration,
    abandoned: &'static AtomicUsize,
    work: impl FnOnce(&dyn Fn() -> bool) -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    if abandoned.load(Ordering::SeqCst) >= MAX_ABANDONED {
        return Err(Failure::Busy);
    }
    // The budget covers everything from here on, including opening the repository.
    let deadline = Instant::now() + time;
    let interrupt = Arc::new(AtomicBool::new(false));
    // Set by the first of "the work finished" and "the caller gave up"; the second one to
    // arrive settles the count.
    let settled = Arc::new(AtomicBool::new(false));
    let (done, result) = crossbeam_channel::bounded(1);
    let spawned = {
        let interrupt = Arc::clone(&interrupt);
        let settled = Arc::clone(&settled);
        thread::Builder::new()
            .name("git-status-scan".into())
            .spawn(move || {
                let over = || interrupt.load(Ordering::SeqCst) || Instant::now() >= deadline;
                let _ = done.send(work(&over));
                if settled.swap(true, Ordering::SeqCst) {
                    abandoned.fetch_sub(1, Ordering::SeqCst);
                }
            })
    };
    if let Err(e) = spawned {
        return Err(Failure::Error(e.to_string()));
    }
    let over = || interrupt.load(Ordering::SeqCst) || Instant::now() >= deadline;
    match result.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(_) if over() => Err(Failure::TooBig),
        Ok(result) => result,
        // The work panicked: it is not running any more.
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
            Err(Failure::Error("git status scan stopped".into()))
        }
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
            interrupt.store(true, Ordering::SeqCst);
            abandoned.fetch_add(1, Ordering::SeqCst);
            if settled.swap(true, Ordering::SeqCst) {
                // It finished after all; nothing is left running.
                abandoned.fetch_sub(1, Ordering::SeqCst);
            }
            Err(Failure::TooBig)
        }
    }
}

fn read_within(
    root: &Path,
    limits: Limits,
    over: &dyn Fn() -> bool,
) -> Result<RepoStatus, Failure> {
    let (fs, top, root) = open_root(root, false, over)?;
    scan(&fs, &top, &root, limits, over)
}

/// Opens the repository folder `root` through [`locate`] and makes it the folder the scan may
/// not leave.
fn open_root(
    root: &Path,
    record: bool,
    over: &dyn Fn() -> bool,
) -> Result<(Fs, Dir, PathBuf), Failure> {
    let Located {
        mut fs,
        mut levels,
        path,
    } = locate(root, record, over)?;
    let top = levels.pop().ok_or(Failure::Unsafe)?;
    fs.narrow(&top).map_err(meta)?;
    Ok((fs, top, path))
}

/// A safe-layer error while reading Git metadata. Links and unexpected kinds mean the
/// repository is unsafe to read.
fn meta(e: safe_fs::Error) -> Failure {
    match e {
        safe_fs::Error::Link
        | safe_fs::Error::WrongKind
        | safe_fs::Error::BadName
        | safe_fs::Error::OnlineOnly => Failure::Unsafe,
        safe_fs::Error::TooBig => Failure::TooBig,
        safe_fs::Error::NotFound => Failure::Error("not found".into()),
        safe_fs::Error::Io(message) => Failure::Error(message),
    }
}

fn odb_failure(e: odb::OdbError) -> Failure {
    match e {
        odb::OdbError::Link => Failure::Unsafe,
        odb::OdbError::TooBig => Failure::TooBig,
        odb::OdbError::Missing => Failure::Error("object missing".into()),
        odb::OdbError::Bad(message) => Failure::Error(message),
    }
}

/// Largest `HEAD`, config, loose ref, `commondir` or alternates file read. Real ones are a few
/// kilobytes at most.
const MAX_META_BYTES: u64 = 1024 * 1024;
/// Deepest folder level walked, and deepest tree read.
const MAX_DEPTH: usize = 256;
/// Most symbolic refs followed from `HEAD`.
const MAX_SYMREF_DEPTH: usize = 5;

/// The scan proper, with every file opened through `fs` below `top`, the root folder.
fn scan(
    fs: &Fs,
    top: &Dir,
    root: &Path,
    limits: Limits,
    over: &dyn Fn() -> bool,
) -> Result<RepoStatus, Failure> {
    let git = fs.dir(top, ".git").map_err(meta)?;
    let read = |rel: &str| fs.read_opt(&git, rel, MAX_META_BYTES).map_err(meta);
    if read("commondir")?.is_some() {
        return Err(Failure::Unsafe);
    }
    let excludes_file = match read("config")? {
        Some(bytes) => check_config(&bytes, root)?,
        None => None,
    };
    if let Some(bytes) = read("objects/info/alternates")? {
        if String::from_utf8_lossy(&bytes)
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            return Err(Failure::Unsafe);
        }
    }
    let head = read("HEAD")?
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        .unwrap_or_default();
    let branch = head
        .strip_prefix("ref:")
        .map(|name| name.trim())
        .map(|name| name.strip_prefix("refs/heads/").unwrap_or(name).to_string());

    let (index, index_mtime) = match fs.file(&git, "index") {
        Ok(file) => {
            if file.info.size > limits.index_bytes {
                return Err(Failure::TooBig);
            }
            let mtime = file.info.mtime;
            let bytes = file.read_capped(limits.index_bytes).map_err(meta)?;
            let options = gix::index::decode::Options {
                thread_limit: Some(1),
                alloc_limit_bytes: Some(4 * limits.index_bytes as usize),
                ..Default::default()
            };
            let (state, _) = gix::index::State::from_bytes(
                &bytes,
                filetime::FileTime::zero(),
                gix::hash::Kind::Sha1,
                options,
            )
            .map_err(|e| Failure::Error(e.to_string()))?;
            (state, mtime)
        }
        Err(safe_fs::Error::NotFound) => (gix::index::State::new(gix::hash::Kind::Sha1), 0),
        Err(e) => return Err(meta(e)),
    };
    // A split index keeps its entries in another file, named by the index.
    if index.link().is_some() {
        return Err(Failure::Unsafe);
    }
    if index.entries().len() > limits.index_entries || over() {
        return Err(Failure::TooBig);
    }

    let mut status = RepoStatus::new(root.to_path_buf(), branch);

    // Tracked, untracked and ignored entries, in one walk of the work tree.
    let mut ignores = Ignores::new(limits);
    if let Some(rel) = &excludes_file {
        match fs.file_at(top, rel) {
            Ok(file) => ignores.add_global(file)?,
            Err(safe_fs::Error::NotFound) => {}
            Err(e) => return Err(meta(e)),
        }
    }
    match fs.file_at(&git, "info/exclude") {
        Ok(file) => ignores.add_global(file)?,
        Err(safe_fs::Error::NotFound) => {}
        Err(e) => return Err(meta(e)),
    }
    let mut walker = Walker::new(
        fs,
        root,
        &index,
        index_mtime,
        ignores,
        &mut status,
        limits,
        over,
    );
    walker.walk(top, "", false, 0)?;
    walker.finish();
    if over() {
        return Err(Failure::TooBig);
    }

    // Staged changes: the index against HEAD's tree.
    let objects = match fs.dir(&git, "objects") {
        Ok(dir) => Some(dir),
        Err(safe_fs::Error::NotFound) => None,
        Err(e) => return Err(meta(e)),
    };
    match head_commit(fs, &git, &head, limits)? {
        Some(commit) => {
            let objects = objects.ok_or_else(|| Failure::Error("no objects".into()))?;
            let mut odb = odb::Odb::new(fs, objects, limits.odb, over);
            match staged_changes(&mut odb, &commit, &index, limits, over) {
                Ok(changes) => {
                    for (path, badge) in changes {
                        status.add_staged(&path, badge);
                    }
                }
                // A shallow or partial clone: no staged badges.
                Err(odb::OdbError::Missing) => {}
                Err(e) => return Err(odb_failure(e)),
            }
        }
        // Unborn branch: everything in the index is new.
        None => {
            for entry in index.entries() {
                if over() {
                    break;
                }
                if entry.flags.stage_raw() == 0 {
                    status.add_staged(&entry.path(&index).to_string(), GitBadge::Added);
                }
            }
        }
    }
    if over() {
        return Err(Failure::TooBig);
    }
    Ok(status)
}

/// Checks the repository's config. Returns the root-relative path of `core.excludesFile`.
///
/// Refused: a work tree other than the root, an excludes file outside it (or relative, which
/// gix would resolve against the current folder), and objects other than SHA-1. Includes are
/// never followed, and keys naming other files (`indexFile`, `attributesFile`, ...) are never
/// used, so they need no check.
fn check_config(bytes: &[u8], root: &Path) -> Result<Option<String>, Failure> {
    let file = gix::config::File::from_bytes_no_includes(
        bytes,
        gix::config::file::Metadata::from(gix::config::Source::Local),
        Default::default(),
    )
    .map_err(|e| Failure::Error(e.to_string()))?;
    let git_dir = root.join(".git");
    let mut excludes = None;
    for section in file.sections() {
        let name = section.header().name().to_string().to_ascii_lowercase();
        let values = |key: &str| {
            section
                .body()
                .values(key)
                .into_iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
        };
        match name.as_str() {
            "core" => {
                for v in values("worktree") {
                    if !resolve(&git_dir, &v, root).is_some_and(|w| path_key(&w) == path_key(root))
                    {
                        return Err(Failure::Unsafe);
                    }
                }
                for v in values("excludesFile") {
                    let path = absolute(v.strip_prefix(":(optional)").unwrap_or(&v), root)
                        .filter(|p| inside(p, root) && path_key(p) != path_key(root))
                        .ok_or(Failure::Unsafe)?;
                    let rel = path_key(&path)[path_key(root).len() + 1..].replace('\\', "/");
                    excludes = Some(rel);
                }
            }
            "extensions" => {
                if values("objectFormat")
                    .iter()
                    .any(|v| !v.eq_ignore_ascii_case("sha1"))
                {
                    return Err(Failure::Unsafe);
                }
            }
            _ => {}
        }
    }
    Ok(excludes)
}

/// The commit `HEAD` points at, following at most a few symbolic refs, each opened through the
/// safe layer. `None` for an unborn branch.
fn head_commit(
    fs: &Fs,
    git: &Dir,
    head: &str,
    limits: Limits,
) -> Result<Option<gix::ObjectId>, Failure> {
    let mut text = head.to_string();
    let mut packed: Option<Vec<u8>> = None;
    for _ in 0..MAX_SYMREF_DEPTH {
        let Some(name) = text.strip_prefix("ref:") else {
            return gix::ObjectId::from_hex(text.trim().as_bytes())
                .map(Some)
                .map_err(|e| Failure::Error(e.to_string()));
        };
        let name = name.trim().to_string();
        if !name.starts_with("refs/")
            || !name
                .split('/')
                .all(|part| safe_fs::plain_component(part) && plain_name(part.as_ref()))
        {
            return Err(Failure::Unsafe);
        }
        match fs.read_opt(git, &name, MAX_META_BYTES).map_err(meta)? {
            Some(bytes) => text = String::from_utf8_lossy(&bytes).trim().to_string(),
            None => {
                let packed = match &mut packed {
                    Some(p) => p,
                    None => packed.insert(
                        fs.read_opt(git, "packed-refs", limits.packed_refs_bytes)
                            .map_err(meta)?
                            .unwrap_or_default(),
                    ),
                };
                return Ok(String::from_utf8_lossy(packed).lines().find_map(|line| {
                    let (id, refname) = line.split_once(' ')?;
                    (refname.trim() == name)
                        .then(|| gix::ObjectId::from_hex(id.as_bytes()).ok())
                        .flatten()
                }));
            }
        }
    }
    Err(Failure::Unsafe)
}

/// Paths whose index entry differs from `commit`'s tree: changed or deleted ones as modified,
/// ones new to the index as added. Subtrees the index's cache records as unchanged are skipped.
fn staged_changes(
    odb: &mut odb::Odb<'_>,
    commit: &gix::ObjectId,
    index: &gix::index::State,
    limits: Limits,
    over: &dyn Fn() -> bool,
) -> Result<Vec<(String, GitBadge)>, odb::OdbError> {
    let data = odb.read(commit, gix::objs::Kind::Commit)?;
    let tree = gix::objs::CommitRefIter::from_bytes(&data, gix::hash::Kind::Sha1)
        .tree_id()
        .map_err(|e| odb::OdbError::Bad(e.to_string()))?;
    let entries = index.entries();
    let mut by_path = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if entry.flags.stage_raw() == 0 && entry.mode != gix::index::entry::Mode::DIR {
            by_path.insert(entry.path(index).to_vec(), i);
        }
    }
    let mut cached = HashMap::new();
    if let Some(root) = index.tree() {
        let mut stack = vec![(Vec::new(), root)];
        while let Some((prefix, node)) = stack.pop() {
            if node.num_entries.is_some() {
                cached.insert(prefix.clone(), node.id);
            }
            for child in &node.children {
                let mut path = prefix.clone();
                path.extend_from_slice(&child.name);
                path.push(b'/');
                stack.push((path, child));
            }
        }
    }
    let mut diff = Staged {
        odb,
        index,
        by_path,
        cached,
        seen: vec![false; entries.len()],
        changes: Vec::new(),
        bytes: 0,
        limits,
        over,
    };
    diff.tree(tree, &mut Vec::new(), 0)?;
    let added: Vec<Vec<u8>> = diff
        .by_path
        .iter()
        .filter(|(_, i)| !diff.seen[**i])
        .map(|(path, _)| path.clone())
        .collect();
    for path in added {
        diff.push(&path, GitBadge::Added)?;
    }
    Ok(diff.changes)
}

struct Staged<'a, 'o> {
    odb: &'a mut odb::Odb<'o>,
    index: &'a gix::index::State,
    by_path: HashMap<Vec<u8>, usize>,
    /// Tree ids the index's cache records, by folder path with a trailing `/` (root: empty).
    cached: HashMap<Vec<u8>, gix::ObjectId>,
    seen: Vec<bool>,
    changes: Vec<(String, GitBadge)>,
    /// Bytes of the paths in `changes`.
    bytes: usize,
    limits: Limits,
    over: &'a dyn Fn() -> bool,
}

impl Staged<'_, '_> {
    /// Records a change. A small tree can name a huge number of paths (the same subtree under
    /// many names, nested), so the count and the bytes of the paths are capped.
    fn push(&mut self, path: &[u8], badge: GitBadge) -> Result<(), odb::OdbError> {
        self.bytes += path.len();
        if self.changes.len() >= self.limits.staged_entries || self.bytes > self.limits.staged_bytes
        {
            return Err(odb::OdbError::TooBig);
        }
        self.changes
            .push((String::from_utf8_lossy(path).into_owned(), badge));
        Ok(())
    }

    fn tree(
        &mut self,
        id: gix::ObjectId,
        prefix: &mut Vec<u8>,
        depth: usize,
    ) -> Result<(), odb::OdbError> {
        if depth > MAX_DEPTH || (self.over)() {
            return Err(odb::OdbError::TooBig);
        }
        if self.cached.get(prefix.as_slice()) == Some(&id) {
            // Unchanged: every index entry below is as in HEAD.
            let entries = self.index.entries();
            let start = entries.partition_point(|e| &e.path(self.index)[..] < prefix.as_slice());
            for (i, entry) in entries.iter().enumerate().skip(start) {
                if !entry.path(self.index).starts_with(prefix) {
                    break;
                }
                self.seen[i] = true;
            }
            return Ok(());
        }
        let data = self.odb.read(&id, gix::objs::Kind::Tree)?;
        for entry in gix::objs::TreeRefIter::from_bytes(&data, gix::hash::Kind::Sha1) {
            if (self.over)() {
                return Err(odb::OdbError::TooBig);
            }
            let entry = entry.map_err(|e| odb::OdbError::Bad(e.to_string()))?;
            let len = prefix.len();
            prefix.extend_from_slice(entry.filename);
            if prefix.len() > self.limits.path_bytes {
                return Err(odb::OdbError::TooBig);
            }
            if entry.mode.is_tree() {
                prefix.push(b'/');
                self.tree(entry.oid.to_owned(), prefix, depth + 1)?;
            } else {
                let changed = match self.by_path.get(prefix.as_slice()) {
                    Some(&i) => {
                        self.seen[i] = true;
                        let e = &self.index.entries()[i];
                        e.id != entry.oid
                            || e.mode.to_tree_entry_mode().map(|m| m.kind())
                                != Some(entry.mode.kind())
                    }
                    // Deleted from the index.
                    None => true,
                };
                if changed {
                    let path = prefix.clone();
                    self.push(&path, GitBadge::Modified)?;
                }
            }
            prefix.truncate(len);
        }
        Ok(())
    }
}

/// Ignore patterns: the global files (`core.excludesFile`, `info/exclude`) and a stack of
/// per-folder `.gitignore` files along the walk, with caps on their sizes and pattern counts.
struct Ignores {
    globals: Vec<IgnoreList>,
    dirs: Vec<IgnoreList>,
    bytes: u64,
    patterns: usize,
    limits: Limits,
}

struct IgnoreList {
    /// Folder the file is in, relative to the root, with a trailing `/` (root: empty).
    base: String,
    patterns: Vec<gix::glob::Pattern>,
}

impl Ignores {
    fn new(limits: Limits) -> Self {
        Ignores {
            globals: Vec::new(),
            dirs: Vec::new(),
            bytes: 0,
            patterns: 0,
            limits,
        }
    }

    /// Parses an ignore file within the caps. Online-only files are never read.
    fn parse(&mut self, file: safe_fs::File, base: &str) -> Result<Option<IgnoreList>, Failure> {
        if !forge_core::cloud::is_online_only_attrs(file.info.attributes) {
            if file.info.size > self.limits.ignore_file_bytes {
                return Err(Failure::TooBig);
            }
            self.bytes += file.info.size;
            if self.bytes > self.limits.ignore_bytes {
                return Err(Failure::TooBig);
            }
            let bytes = file
                .read_capped(self.limits.ignore_file_bytes)
                .map_err(meta)?;
            let patterns: Vec<_> = gix::ignore::parse(&bytes, false)
                .map(|(pattern, _, _)| pattern)
                .take(self.limits.ignore_file_patterns + 1)
                .collect();
            self.patterns += patterns.len();
            if patterns.len() > self.limits.ignore_file_patterns
                || self.patterns > self.limits.ignore_patterns
            {
                return Err(Failure::TooBig);
            }
            return Ok(Some(IgnoreList {
                base: base.to_string(),
                patterns,
            }));
        }
        Ok(None)
    }

    /// Adds `core.excludesFile` or `info/exclude`; the one added later wins.
    fn add_global(&mut self, file: safe_fs::File) -> Result<(), Failure> {
        if let Some(list) = self.parse(file, "")? {
            self.globals.insert(0, list);
        }
        Ok(())
    }

    /// Pushes the `.gitignore` of folder `dir` (at `rel`), if it has one. Returns whether it
    /// did. A linked `.gitignore` is not followed, as Git does not.
    fn push(&mut self, fs: &Fs, dir: &Dir, rel: &str) -> Result<bool, Failure> {
        match fs.file(dir, ".gitignore") {
            Ok(file) => match self.parse(file, rel)? {
                Some(list) => {
                    self.dirs.push(list);
                    Ok(true)
                }
                None => Ok(false),
            },
            Err(safe_fs::Error::TooBig) => Err(Failure::TooBig),
            Err(safe_fs::Error::Io(message)) => Err(Failure::Error(message)),
            Err(_) => Ok(false),
        }
    }

    /// Is root-relative `path` ignored? Deeper `.gitignore` files win over shallower ones,
    /// which win over the global files; within a file the last matching pattern wins.
    fn ignored(&self, path: &str, is_dir: bool) -> bool {
        use gix::bstr::ByteSlice;
        for list in self.dirs.iter().rev().chain(self.globals.iter()) {
            let Some(sub) = path.strip_prefix(&list.base) else {
                continue;
            };
            let basename = sub.rfind('/').map(|p| p + 1);
            let hit = list.patterns.iter().rev().find(|p| {
                p.matches_repo_relative_path(
                    sub.as_bytes().as_bstr(),
                    basename,
                    Some(is_dir),
                    gix::glob::pattern::Case::Fold,
                    gix::glob::wildmatch::Mode::NO_MATCH_SLASH_LITERAL,
                )
            });
            if let Some(p) = hit {
                return !p.is_negative();
            }
        }
        false
    }
}

/// Lets [`forge_core::cloud::background_read_allowed_with`] check attributes read from an
/// open handle instead of looking the path up again.
struct HandleAttributes(u32);

impl forge_core::cloud::AttributeSource for HandleAttributes {
    fn attributes(&self, _path: &Path) -> Option<u32> {
        Some(self.0)
    }
}

/// Most same-size files whose contents are hashed to tell a real change from a touched or
/// racily-clean file (one written in the same second as the index).
const MAX_HASHED_FILES: usize = 256;
/// Largest file hashed for that check; bigger ones count as modified.
const MAX_HASHED_BYTES: u64 = 16 * 1024 * 1024;

/// Does file `name` in `dir` still hash to `id` as a blob? Reads the raw bytes itself, with
/// no filter or line-ending conversion, so a filtered file may count as changed, never the
/// reverse. An online-only file is never read, which would download it; it counts as changed.
/// `path` is only used to look up simulated placeholders in tests.
fn same_as_blob(fs: &Fs, dir: &Dir, name: &str, path: &Path, id: &gix::ObjectId) -> bool {
    let Ok(file) = fs.file(dir, name) else {
        return false;
    };
    if file.info.size > MAX_HASHED_BYTES
        || !forge_core::cloud::background_read_allowed_with(
            path,
            &HandleAttributes(file.info.attributes),
        )
    {
        return false;
    }
    file.read_capped(MAX_HASHED_BYTES).is_ok_and(|bytes| {
        gix::objs::compute_hash(id.kind(), gix::objs::Kind::Blob, &bytes).is_ok_and(|h| h == *id)
    })
}

/// Windows `FILETIME` as Unix seconds and nanoseconds, as Git for Windows stores them.
fn unix_time(filetime: i64) -> (u32, u32) {
    const EPOCH: i64 = 116_444_736_000_000_000;
    let since = filetime - EPOCH;
    (
        since.div_euclid(10_000_000) as u32,
        (since.rem_euclid(10_000_000) * 100) as u32,
    )
}

/// One walk of the work tree: compares tracked files with the index from folder listings, and
/// reports untracked and ignored entries. Links are listed but never entered or read.
struct Walker<'a> {
    fs: &'a Fs,
    root: &'a Path,
    index: &'a gix::index::State,
    /// Write time of the index; files written since may be racily clean.
    index_mtime: i64,
    /// Index entries by lowercase path (several when conflicted).
    tracked: HashMap<String, Vec<usize>>,
    /// Lowercase folders, without a trailing `/`, that hold tracked entries.
    tracked_dirs: HashSet<String>,
    /// Index entries whose file was found and compared.
    checked: Vec<bool>,
    ignores: Ignores,
    status: &'a mut RepoStatus,
    limits: Limits,
    visited: usize,
    hashed: usize,
    over: &'a dyn Fn() -> bool,
}

impl<'a> Walker<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        fs: &'a Fs,
        root: &'a Path,
        index: &'a gix::index::State,
        index_mtime: i64,
        ignores: Ignores,
        status: &'a mut RepoStatus,
        limits: Limits,
        over: &'a dyn Fn() -> bool,
    ) -> Self {
        let mut tracked: HashMap<String, Vec<usize>> = HashMap::new();
        let mut tracked_dirs = HashSet::new();
        for (i, entry) in index.entries().iter().enumerate() {
            let path = entry.path(index).to_string().to_lowercase();
            for (at, _) in path.match_indices('/') {
                tracked_dirs.insert(path[..at].to_string());
            }
            tracked.entry(path).or_default().push(i);
        }
        Walker {
            fs,
            root,
            index,
            index_mtime,
            tracked,
            tracked_dirs,
            checked: vec![false; index.entries().len()],
            ignores,
            status,
            limits,
            visited: 0,
            hashed: 0,
            over,
        }
    }

    /// Walks folder `dir` at root-relative `rel` (empty, or ending in `/`). Inside an ignored
    /// folder (`ignored`) only tracked files are looked at; everything else is ignored too.
    fn walk(&mut self, dir: &Dir, rel: &str, ignored: bool, depth: usize) -> Result<(), Failure> {
        if depth > MAX_DEPTH || (self.over)() {
            return Err(Failure::TooBig);
        }
        let mut entries = Vec::new();
        let (visited, max) = (&mut self.visited, self.limits.visited);
        dir.list(|entry| {
            *visited += 1;
            entries.push(entry);
            *visited <= max
        })
        .map_err(meta)?;
        if self.visited > max {
            return Err(Failure::TooBig);
        }
        // Only a plain, local `.gitignore` is read: never a link (Git does not follow one
        // either), and never an online-only file, which opening could download.
        let gitignore = entries
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(".gitignore"));
        let pushed = !ignored
            && gitignore.is_some_and(|e| {
                !e.info.is_dir()
                    && !e.info.is_link()
                    && !forge_core::cloud::is_online_only_attrs(e.info.attributes)
            })
            && self.ignores.push(self.fs, dir, rel)?;
        for entry in entries {
            if entry.name.eq_ignore_ascii_case(".git") {
                continue;
            }
            let path = format!("{rel}{}", entry.name);
            let key = path.to_lowercase();
            if let Some(ids) = self.tracked.get(&key).cloned() {
                self.compare(dir, &entry, &ids, &path);
                continue;
            }
            let link = entry.info.is_link();
            if entry.info.is_dir() && forge_core::cloud::is_online_only_attrs(entry.info.attributes)
            {
                // An online-only folder is never opened; anything tracked below counts as
                // modified.
                continue;
            }
            if entry.info.is_dir() && !link {
                let has_tracked = self.tracked_dirs.contains(&key);
                let is_ignored = ignored || self.ignores.ignored(&path, true);
                if !has_tracked && is_ignored {
                    self.status.add(&format!("{path}/"), GitBadge::Ignored);
                    continue;
                }
                // Changed since listed, not openable (access denied) or not safely: skipped,
                // and whatever is tracked below counts as modified.
                if let Ok(sub) = self.fs.dir(dir, &entry.name) {
                    self.walk(&sub, &format!("{path}/"), is_ignored, depth + 1)?;
                }
            } else {
                // A file, or a link of any kind (reported, never followed). Git matches links
                // as files.
                let is_ignored = ignored || self.ignores.ignored(&path, false);
                let badge = if is_ignored {
                    GitBadge::Ignored
                } else {
                    GitBadge::Added
                };
                self.status.add(&path, badge);
            }
            if (self.over)() {
                return Err(Failure::TooBig);
            }
        }
        if pushed {
            self.ignores.dirs.pop();
        }
        Ok(())
    }

    /// Compares the file listed as `entry` in `dir` with its index entries `ids`.
    fn compare(&mut self, dir: &Dir, entry: &safe_fs::Entry, ids: &[usize], path: &str) {
        use gix::index::entry::{Flags, Mode};
        for &i in ids {
            self.checked[i] = true;
        }
        let entries = self.index.entries();
        if ids.iter().any(|&i| entries[i].flags.stage_raw() != 0) {
            self.status.add(path, GitBadge::Conflicted);
            return;
        }
        let e = &entries[ids[0]];
        if e.flags
            .intersects(Flags::SKIP_WORKTREE | Flags::ASSUME_VALID)
            || e.mode == Mode::COMMIT
            || e.mode == Mode::DIR
        {
            return;
        }
        if e.flags.contains(Flags::INTENT_TO_ADD) {
            self.status.add(path, GitBadge::Added);
            return;
        }
        let info = entry.info;
        if e.mode == Mode::SYMLINK && info.is_link() {
            return;
        }
        if info.is_dir() || info.size as u32 != e.stat.size {
            self.status.add(path, GitBadge::Modified);
            return;
        }
        let (secs, nsecs) = unix_time(info.mtime);
        let same_time =
            e.stat.mtime.secs == secs && (e.stat.mtime.nsecs == 0 || e.stat.mtime.nsecs == nsecs);
        if same_time && info.mtime < self.index_mtime {
            return;
        }
        // Same size but a new time (or written with the index): hash the bytes, as git would
        // after a touch or right after a commit. Past the cap a file simply counts as modified.
        self.hashed += 1;
        let abs = self.root.join(path.replace('/', "\\"));
        if info.is_link()
            || forge_core::cloud::is_online_only_attrs(info.attributes)
            || self.hashed > MAX_HASHED_FILES
            || !same_as_blob(self.fs, dir, &entry.name, &abs, &e.id)
        {
            self.status.add(path, GitBadge::Modified);
        }
    }

    /// Tracked entries whose file was never found: deleted, below a link, or not openable.
    fn finish(&mut self) {
        use gix::index::entry::{Flags, Mode};
        for (i, e) in self.index.entries().iter().enumerate() {
            if self.checked[i]
                || e.flags
                    .intersects(Flags::SKIP_WORKTREE | Flags::ASSUME_VALID)
                || e.mode == Mode::COMMIT
                || e.mode == Mode::DIR
            {
                continue;
            }
            let badge = if e.flags.stage_raw() != 0 {
                GitBadge::Conflicted
            } else if e.flags.contains(Flags::INTENT_TO_ADD) {
                GitBadge::Added
            } else {
                GitBadge::Modified
            };
            self.status.add(&e.path(self.index).to_string(), badge);
        }
    }
}

/// `value` (a path from a repository file) resolved against `base` by text alone, as Windows
/// would, without touching the file system. `None` unless the result is on `root`'s volume
/// (same drive, or same server and share). Device paths, drive-relative and rooted paths
/// (`C:x`, `\x`) and names Windows would silently change are refused.
fn resolve(base: &Path, value: &str, root: &Path) -> Option<PathBuf> {
    let value = Path::new(value);
    let joined = match value.components().next()? {
        Component::Prefix(_) => value.to_path_buf(),
        Component::RootDir => return None,
        _ => base.join(value),
    };
    let path = lexical(&joined)?;
    (volume(&path) == volume(root)).then_some(path)
}

/// Like [`resolve`], but `value` must already be absolute. Used for keys gix would resolve
/// against the process's current directory.
fn absolute(value: &str, root: &Path) -> Option<PathBuf> {
    match Path::new(value).components().next()? {
        Component::Prefix(_) => resolve(root, value, root),
        _ => None,
    }
}

/// `path` with `.` and `..` resolved and a plain drive (`C:\`) or share (`\\server\share\`)
/// prefix, by text alone. `None` for relative paths, device paths (`\\.\`, `\\?\` other than
/// a drive or share) and names Windows would change on open (trailing dots or spaces, `:`,
/// reserved device names).
fn lexical(path: &Path) -> Option<PathBuf> {
    let mut parts = path.components();
    let mut out = match parts.next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                PathBuf::from(format!("{}:\\", (d as char).to_ascii_uppercase()))
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                let (server, share) = (server.to_str()?, share.to_str()?);
                if server.is_empty() || share.is_empty() || server == "." || server == "?" {
                    return None;
                }
                PathBuf::from(format!(r"\\{server}\{share}\"))
            }
            Prefix::Verbatim(_) | Prefix::DeviceNS(_) => return None,
        },
        _ => return None,
    };
    if parts.next() != Some(Component::RootDir) {
        return None;
    }
    for part in parts {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) if plain_name(name) => out.push(name),
            _ => return None,
        }
    }
    Some(out)
}

/// A file name Windows opens as written: not empty, no trailing dot or space (stripped on
/// open, so `.. ` would mean the parent), no `:` (streams), no wildcards, not a device.
fn plain_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name.contains([':', '*', '?', '"', '<', '>', '|', '\\', '/', '\0'])
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.as_bytes()[3].is_ascii_digit());
    !device
}

/// Lowercase text form of a [`lexical`] path without a trailing `\`, for comparisons.
fn path_key(path: &Path) -> String {
    path.to_string_lossy().trim_end_matches('\\').to_lowercase()
}

/// The drive or share of a [`lexical`] path, lowercase.
fn volume(path: &Path) -> String {
    match path.components().next() {
        Some(Component::Prefix(p)) => p.as_os_str().to_string_lossy().to_lowercase(),
        _ => String::new(),
    }
}

/// Is [`lexical`] path `path` `dir` itself or below it?
fn inside(path: &Path, dir: &Path) -> bool {
    let (path, dir) = (path_key(path), path_key(dir));
    path == dir
        || path
            .strip_prefix(&dir)
            .is_some_and(|rest| rest.starts_with('\\'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_of(entries: &[(&str, GitBadge)]) -> RepoStatus {
        let mut status = RepoStatus::new(PathBuf::from(r"C:\repo"), Some("main".into()));
        for (path, badge) in entries {
            status.add(path, *badge);
        }
        status
    }

    fn sample() -> RepoStatus {
        status_of(&[
            ("src/my file.rs", GitBadge::Modified),
            ("added.rs", GitBadge::Added),
            ("conflict.rs", GitBadge::Conflicted),
            ("new dir/", GitBadge::Added),
            ("notes.txt", GitBadge::Added),
            ("target/", GitBadge::Ignored),
            ("debug.log", GitBadge::Ignored),
        ])
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(format!(r"C:\repo\{s}"))
    }

    #[test]
    fn looks_up_badges_case_insensitively() {
        let status = sample();
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
        assert_eq!(status.badge(&p("clean.rs"), false), None);
    }

    #[test]
    fn rolls_up_to_parent_folders() {
        assert_eq!(sample().badge(&p("src"), true), Some(GitBadge::Modified));
        let status = status_of(&[
            ("lib/mod.rs", GitBadge::Modified),
            ("lib/conflict.rs", GitBadge::Conflicted),
            ("only/new.rs", GitBadge::Added),
            ("logs/x.log", GitBadge::Ignored),
        ]);
        assert_eq!(status.badge(&p("lib"), true), Some(GitBadge::Conflicted));
        assert_eq!(status.badge(&p("only"), true), Some(GitBadge::Added));
        assert_eq!(
            status.badge(&p("logs"), true),
            None,
            "ignored files are not rolled up"
        );
    }

    #[test]
    fn staged_changes_combine_with_worktree_badges() {
        let mut status = status_of(&[
            ("c.rs", GitBadge::Conflicted),
            ("m.rs", GitBadge::Modified),
            ("edited-new.rs", GitBadge::Modified),
        ]);
        status.add_staged("c.rs", GitBadge::Added);
        status.add_staged("m.rs", GitBadge::Modified);
        status.add_staged("edited-new.rs", GitBadge::Added);
        status.add_staged("new.rs", GitBadge::Added);
        assert_eq!(status.badge(&p("c.rs"), false), Some(GitBadge::Conflicted));
        assert_eq!(status.badge(&p("m.rs"), false), Some(GitBadge::Modified));
        assert_eq!(
            status.badge(&p("edited-new.rs"), false),
            Some(GitBadge::Added)
        );
        assert_eq!(status.badge(&p("new.rs"), false), Some(GitBadge::Added));
    }

    #[test]
    fn contents_of_untracked_and_ignored_folders_inherit_badge() {
        let status = sample();
        assert_eq!(
            status.badge(&p(r"new dir\x\y.txt"), false),
            Some(GitBadge::Added)
        );
        assert_eq!(
            status.badge(&p(r"target\debug\a.exe"), false),
            Some(GitBadge::Ignored)
        );
    }

    #[test]
    fn edge_cases_have_no_badge() {
        let status = sample();
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
        assert_eq!(find(&root.join("a").join("b").join("c")), Some(root));

        // A `.git` file, as in worktrees and submodules, also counts.
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(worktree.join("sub")).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: elsewhere\n").unwrap();
        assert_eq!(find(&worktree.join("sub")), Some(worktree.clone()));

        let plain = tmp.path().join("plain").join("x");
        std::fs::create_dir_all(&plain).unwrap();
        let ancestor_is_repo = tmp
            .path()
            .ancestors()
            .any(|a| a.join(".git").symlink_metadata().is_ok());
        if !ancestor_is_repo {
            assert_eq!(find(&plain), None);
        }
    }

    #[test]
    fn nothing_is_scanned_until_turned_on() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let dirs = vec![root.clone()];

        let mut git = GitStatus::new(egui::Context::default());
        for _ in 0..3 {
            git.track(&dirs);
            git.invalidate(&root);
            assert!(!git.poll());
        }
        assert_eq!(git.sent, 0, "off by default: no request reaches the worker");
        assert!(git.repo_for(&root).is_none());

        git.set_enabled(true);
        git.track(&dirs);
        assert_eq!(git.sent, 1, "on: the folder's repository is looked up");

        git.set_enabled(false);
        git.track(&dirs);
        git.invalidate(&root);
        assert_eq!(git.sent, 1, "turned off again: nothing more is sent");
        assert!(git.repo_for(&root).is_none());
    }

    /// Runs the git CLI to build a fixture. Tests that need it are skipped without git.
    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// A committed repository: `a.txt` modified, `new.txt` staged, an untracked file and
    /// folder, and an ignored log.
    fn fixture_repo() -> Option<tempfile::TempDir> {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        if !git(root, &["init", "-q", "-b", "main"]) {
            eprintln!("skipped: git is not installed");
            return None;
        }
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        assert!(git(root, &["add", "."]));
        assert!(git(root, &["commit", "-q", "-m", "one"]));
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        assert!(git(root, &["add", "new.txt"]));
        std::fs::write(root.join("a.txt"), "hello, changed\n").unwrap();
        std::fs::write(root.join("untracked.txt"), "u").unwrap();
        std::fs::create_dir_all(root.join("ud").join("x")).unwrap();
        std::fs::write(root.join("ud").join("x").join("f"), "f").unwrap();
        std::fs::write(root.join("b.log"), "log").unwrap();
        Some(tmp)
    }

    #[test]
    fn reads_a_real_repository_without_git() {
        let Some(tmp) = fixture_repo() else { return };
        let root = tmp.path();
        let status = read_status(root).expect("status");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(
            status.badge(&root.join("a.txt"), false),
            Some(GitBadge::Modified)
        );
        assert_eq!(
            status.badge(&root.join("new.txt"), false),
            Some(GitBadge::Added)
        );
        assert_eq!(
            status.badge(&root.join("untracked.txt"), false),
            Some(GitBadge::Added)
        );
        assert_eq!(status.badge(&root.join("ud"), true), Some(GitBadge::Added));
        assert_eq!(
            status.badge(&root.join("b.log"), false),
            Some(GitBadge::Ignored)
        );
        assert_eq!(status.badge(&root.join(".gitignore"), false), None);
    }

    /// A downloaded folder can name programs in its own `.git/config`. Listing it must not
    /// run them: neither `core.fsmonitor` nor a filter driver applied by `.gitattributes`.
    #[test]
    fn repository_config_cannot_run_programs() {
        let Some(tmp) = fixture_repo() else { return };
        let root = tmp.path();
        let marker = root.join("MARKER");
        let script = root.join("evil.bat");
        std::fs::write(
            &script,
            format!("@echo off\r\necho ran > \"{}\"\r\n", marker.display()),
        )
        .unwrap();
        let script = script.to_string_lossy().replace('\\', "/");
        for (key, value) in [
            ("core.fsmonitor", script.as_str()),
            ("filter.evil.clean", script.as_str()),
            ("filter.evil.smudge", script.as_str()),
            ("filter.evil.process", script.as_str()),
            ("filter.evil.required", "true"),
            ("core.hooksPath", "."),
        ] {
            assert!(git(root, &["config", key, value]), "git config {key}");
        }
        std::fs::write(root.join(".gitattributes"), "* filter=evil\n").unwrap();
        // Same size, new content and time: the file is stat-dirty, which is where git would
        // re-read it through the filter.
        std::fs::write(root.join("a.txt"), "HELLO, CHANGED\n").unwrap();

        let status = read_status(root).expect("status");
        assert!(
            !marker.exists(),
            "a program from the repository's config was run"
        );
        assert_eq!(
            status.badge(&root.join("a.txt"), false),
            Some(GitBadge::Modified)
        );
    }

    #[test]
    fn a_config_that_moves_the_work_tree_is_refused() {
        let Some(tmp) = fixture_repo() else { return };
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("secret.txt"), "s").unwrap();
        let target = elsewhere.path().to_string_lossy().replace('\\', "/");
        assert!(git(tmp.path(), &["config", "core.worktree", &target]));
        assert_eq!(read_status(tmp.path()).unwrap_err(), Failure::Unsafe);
    }

    /// An address reserved for documentation (TEST-NET-1): nothing answers there, so a scan
    /// that tried to reach it would stall for seconds instead of returning at once.
    const FAKE_SHARE: &str = "//192.0.2.1/share";

    /// Asserts that `root` is refused before anything is opened, and quickly.
    fn assert_refused(root: &Path, what: &str) {
        let started = Instant::now();
        assert_eq!(
            read_status(root).unwrap_err(),
            Failure::Unsafe,
            "{what}: scan"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{what}: took {:?}",
            started.elapsed()
        );
        let (result, opened, root_final) = scan_recorded(root, LIMITS);
        assert_eq!(
            result.unwrap_err(),
            Failure::Unsafe,
            "{what}: recorded scan"
        );
        assert_contained(&opened, &root_final, what);
    }

    /// [`find_repo_root`] without a deadline, failures as `None`, as the worker reports them.
    fn find(dir: &Path) -> Option<PathBuf> {
        find_repo_root(dir, &|| false).ok().flatten()
    }

    /// The scan on its own thread, with `abandoned` as the count of abandoned scans.
    fn scan_on_thread(
        root: &Path,
        limits: Limits,
        abandoned: &'static AtomicUsize,
    ) -> Result<RepoStatus, Failure> {
        let root = root.to_path_buf();
        on_thread(limits.time, abandoned, move |over| {
            read_within(&root, limits, over)
        })
    }

    /// The safe layer narrowed to folder `dir`, reached as the scan reaches a root.
    fn fs_at(dir: &Path, record: bool) -> (Fs, Dir) {
        let (fs, top, _) = open_root(dir, record, &|| false).expect("folder opens");
        (fs, top)
    }

    /// Runs the scan on this thread, recording the final path of every handle it opens.
    fn scan_recorded(
        root: &Path,
        limits: Limits,
    ) -> (Result<RepoStatus, Failure>, Vec<String>, String) {
        let (fs, top, root) = match open_root(root, true, &|| false) {
            Ok(opened) => opened,
            Err(e) => return (Err(e), vec![String::new()], String::new()),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        let result = scan(&fs, &top, &root, limits, &|| Instant::now() >= deadline);
        (result, fs.opened(), fs.root_final().to_string())
    }

    /// Every opened handle was the root, inside it, or one of the folders on the way down to
    /// it from its drive's root.
    fn assert_contained(opened: &[String], root_final: &str, what: &str) {
        assert!(!opened.is_empty(), "{what}: nothing recorded");
        for path in opened {
            let key = path.to_lowercase();
            let key = key.trim_end_matches('\\');
            assert!(
                key == root_final
                    || key.starts_with(&format!("{root_final}\\"))
                    || root_final.starts_with(&format!("{key}\\")),
                "{what}: opened {path} outside {root_final}"
            );
        }
    }

    /// Makes `link` a junction to `target`. Needs no rights; false if it failed.
    fn junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Makes `link` a directory or file symlink to `target`. Needs developer mode or admin
    /// rights; tests skip their symlink part without them.
    fn symlink(link: &Path, target: &str, dir: bool) -> bool {
        let made = if dir {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        };
        if made.is_err() {
            eprintln!("skipped: symlinks cannot be created here");
        }
        made.is_ok()
    }

    /// Makes `link` a new folder that is a mount point (junction) to the unroutable share
    /// `\\192.0.2.1\x`, by writing the reparse data directly: `mklink /J` refuses network
    /// targets, but the file system stores any. Needs no rights; false if it failed. (Windows
    /// itself refuses to follow a mount point to a share; symlinks, which need developer mode,
    /// are what reach one. Both are refused unfollowed.)
    fn net_junction(link: &Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE};
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_MODE,
            OPEN_EXISTING,
        };
        if std::fs::create_dir(link).is_err() {
            return false;
        }
        let wide = |s: &str| {
            s.encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<u8>>()
        };
        let substitute = wide(r"\??\UNC\192.0.2.1\x");
        let print = wide(r"\\192.0.2.1\x");
        let mut path = substitute.clone();
        path.extend([0, 0]);
        path.extend(&print);
        path.extend([0, 0]);
        let mut buf = Vec::new();
        buf.extend(0xA000_0003u32.to_le_bytes()); // IO_REPARSE_TAG_MOUNT_POINT
        buf.extend(((8 + path.len()) as u16).to_le_bytes());
        buf.extend(0u16.to_le_bytes());
        buf.extend(0u16.to_le_bytes());
        buf.extend((substitute.len() as u16).to_le_bytes());
        buf.extend(((substitute.len() + 2) as u16).to_le_bytes());
        buf.extend((print.len() as u16).to_le_bytes());
        buf.extend(path);
        let name: Vec<u16> = link.as_os_str().encode_wide().chain([0]).collect();
        unsafe {
            let Ok(handle) = CreateFileW(
                windows::core::PCWSTR(name.as_ptr()),
                GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            ) else {
                return false;
            };
            let set = windows::Win32::System::IO::DeviceIoControl(
                handle,
                windows::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT,
                Some(buf.as_ptr().cast()),
                buf.len() as u32,
                None,
                0,
                None,
                None,
            );
            let _ = CloseHandle(handle);
            if set.is_err() {
                eprintln!("skipped: could not create a network junction: {set:?}");
            }
            set.is_ok()
        }
    }

    /// The unroutable share used as the target of network links.
    const NET: &str = r"\\192.0.2.1\x";

    /// Asserts the scan of `root` stays inside it and returns quickly.
    fn assert_fast_and_contained(root: &Path, what: &str) -> Result<RepoStatus, Failure> {
        let started = Instant::now();
        let (result, opened, root_final) = scan_recorded(root, LIMITS);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{what}: took {:?}",
            started.elapsed()
        );
        assert_contained(&opened, &root_final, what);
        result
    }

    /// Id of the commit `HEAD` names, read with git.
    fn head_id(root: &Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Appends raw text to the repository's `.git/config`, without running git (which could
    /// itself follow what is written).
    fn append_config(root: &Path, text: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join(".git").join("config"))
            .unwrap();
        writeln!(file, "{text}").unwrap();
    }

    /// `path` as a config value: forward slashes, quoted.
    fn config_path(path: &Path) -> String {
        format!("\"{}\"", path.to_string_lossy().replace('\\', "/"))
    }

    #[test]
    fn paths_are_resolved_by_text_alone() {
        let root = Path::new(r"C:\work\repo");
        let git_dir = Path::new(r"C:\work\repo\.git");
        let ok = |base: &Path, v: &str| resolve(base, v, root).map(|p| path_key(&p));
        assert_eq!(
            ok(root, "../main/.git").as_deref(),
            Some(r"c:\work\main\.git")
        );
        assert_eq!(ok(git_dir, "..").as_deref(), Some(r"c:\work\repo"));
        assert_eq!(
            ok(git_dir, "C:/work/repo/./x/../").as_deref(),
            Some(r"c:\work\repo")
        );
        assert_eq!(
            ok(git_dir, r"\\?\C:\work\repo").as_deref(),
            Some(r"c:\work\repo")
        );
        for bad in [
            r"\\server\share\x",
            "//server/share/x",
            r"\\?\UNC\server\share\x",
            r"\\.\pipe\x",
            "//./pipe/x",
            "//?/C:/x",
            r"\\?\GLOBALROOT\Device\x",
            "D:/other/volume",
            "C:relative",
            r"\rooted",
            "/rooted",
            r".. \x",
            "x./y",
            "file.txt:stream",
            "NUL",
            "sub/con.txt",
            "COM1",
            "*",
        ] {
            assert_eq!(ok(git_dir, bad), None, "{bad}");
        }
        // Relative values for keys gix would resolve against the current directory.
        assert_eq!(absolute("info/exclude", root), None);
        assert!(absolute("C:/work/repo/.git/info/x", root).is_some());
        // A repository on a share may point within that share, never at another one.
        let share_root = Path::new(r"\\nas\code\repo");
        assert!(resolve(share_root, "../main/.git", share_root).is_some());
        assert_eq!(resolve(share_root, "//other/code/x", share_root), None);
        assert!(inside(Path::new(r"C:\work\repo\.git\x"), git_dir));
        assert!(!inside(Path::new(r"C:\work\repo\.gitx"), git_dir));
    }

    #[test]
    fn a_git_file_pointing_elsewhere_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dot_git = tmp.path().join(".git");
        for target in [
            r"\\192.0.2.1\share\repo.git",
            FAKE_SHARE,
            r"\\?\UNC\192.0.2.1\share",
            r"\\.\pipe\repo",
            "Q:/repo.git",
            "relative-but-.. /x",
        ] {
            std::fs::write(&dot_git, format!("gitdir: {target}\n")).unwrap();
            assert_refused(tmp.path(), target);
        }
        std::fs::write(&dot_git, "not a git file\n").unwrap();
        assert_refused(tmp.path(), "no gitdir line");
    }

    /// Gap 3: a `.git` file names its git directory, anywhere. Linked worktrees and submodules
    /// use one, so they show no badges, and nothing where it points is opened.
    #[test]
    fn linked_worktrees_submodules_and_external_git_dirs_show_no_badges() {
        let Some(main) = fixture_repo() else { return };
        let wt_parent = tempfile::tempdir_in(main.path().parent().unwrap()).unwrap();
        let wt = wt_parent.path().join("wt");
        assert!(git(
            main.path(),
            &["worktree", "add", "-q", "-b", "side", &wt.to_string_lossy()]
        ));
        assert_refused(&wt, "linked worktree");

        let Some(lib) = fixture_repo() else { return };
        let lib_url = lib.path().to_string_lossy().replace('\\', "/");
        assert!(git(
            main.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &lib_url,
                "sub"
            ]
        ));
        assert_refused(&main.path().join("sub"), "submodule");
        // The repository holding them still works.
        assert!(read_status(main.path()).is_ok());

        // A `.git` file, or a `.git` junction, into another repository on the same drive.
        let Some(other) = fixture_repo() else { return };
        let plain = tempfile::tempdir().unwrap();
        let target = other.path().join(".git");
        std::fs::write(
            plain.path().join(".git"),
            format!("gitdir: {}\n", target.display()),
        )
        .unwrap();
        assert_refused(plain.path(), "same-drive .git file");
        std::fs::remove_file(plain.path().join(".git")).unwrap();
        if junction(&plain.path().join(".git"), &target) {
            assert_refused(plain.path(), ".git junction");
            std::fs::remove_dir(plain.path().join(".git")).unwrap();
        }
        if net_junction(&plain.path().join(".git")) {
            assert_refused(plain.path(), ".git junction to a share");
        }
    }

    #[test]
    fn config_paths_outside_the_repository_are_refused() {
        let outside = tempfile::tempdir().unwrap();
        let elsewhere = outside.path().join("never-opened");
        std::fs::write(&elsewhere, "[core]\n\tbare = true\n").unwrap();
        let elsewhere = config_path(&elsewhere);
        let share = format!("\"{FAKE_SHARE}/x\"");
        let relative = "\"relative/x\"".to_string();
        for section in ["[core]\n\tworktree", "[core]\n\texcludesFile"] {
            for value in [&share, &elsewhere, &relative] {
                let Some(tmp) = fixture_repo() else { return };
                append_config(tmp.path(), &format!("{section} = {value}"));
                assert_refused(tmp.path(), &format!("{section} = {value}"));
            }
        }
        // Keys naming other files are never used: the scan works and opens nothing there.
        for section in [
            "[core]\n\tattributesFile",
            "[gitoxide \"core\"]\n\tindexFile",
            "[gitoxide \"core\"]\n\tshallowFile",
            "[include]\n\tpath",
            "[includeIf \"gitdir:/\"]\n\tpath",
        ] {
            for value in [&share, &elsewhere] {
                let Some(tmp) = fixture_repo() else { return };
                append_config(tmp.path(), &format!("{section} = {value}"));
                let what = format!("{section} = {value}");
                assert!(
                    assert_fast_and_contained(tmp.path(), &what).is_ok(),
                    "{what}"
                );
            }
        }
        // The same keys pointing inside the repository are fine.
        let Some(tmp) = fixture_repo() else { return };
        let root = tmp.path();
        let info = root.join(".git").join("info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("more-excludes"), "*.tmp\n").unwrap();
        std::fs::write(root.join("x.tmp"), "t").unwrap();
        append_config(
            root,
            &format!(
                "[core]\n\tworktree = {}\n\texcludesFile = {}",
                config_path(root),
                config_path(&info.join("more-excludes"))
            ),
        );
        let status = read_status(root).expect("in-repository config paths");
        assert_eq!(
            status.badge(&root.join("x.tmp"), false),
            Some(GitBadge::Ignored)
        );
    }

    /// Gap 3: a configured path is opened one component at a time, so a junction anywhere
    /// along it is refused rather than followed.
    #[test]
    fn junctions_inside_configured_paths_are_refused() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("excl"), "*.txt\n").unwrap();
        std::fs::write(outside.path().join("exclude"), "*.txt\n").unwrap();
        std::fs::write(outside.path().join("index"), "not an index").unwrap();
        let Some(tmp) = fixture_repo() else { return };
        let root = tmp.path();
        if !junction(&root.join(".git").join("j"), outside.path()) {
            eprintln!("skipped: could not create a junction");
            return;
        }
        // indexFile is never used; excludesFile through the junction is refused.
        append_config(
            root,
            &format!(
                "[gitoxide \"core\"]\n\tindexFile = {}",
                config_path(&root.join(".git").join("j").join("index"))
            ),
        );
        assert!(assert_fast_and_contained(root, "indexFile via junction").is_ok());
        append_config(
            root,
            &format!(
                "[core]\n\texcludesFile = {}",
                config_path(&root.join(".git").join("j").join("excl"))
            ),
        );
        assert_refused(root, "excludesFile via junction");

        // `info` itself a junction.
        let Some(tmp) = fixture_repo() else { return };
        let info = tmp.path().join(".git").join("info");
        let _ = std::fs::remove_dir_all(&info);
        assert!(junction(&info, outside.path()));
        assert_refused(tmp.path(), "info junction");
        let _ = std::fs::remove_dir(&info);
        if net_junction(&info) {
            assert_refused(tmp.path(), "info junction to a share");
        }
    }

    #[test]
    fn alternates_are_refused() {
        let Some(tmp) = fixture_repo() else { return };
        let info = tmp.path().join(".git").join("objects").join("info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("alternates"), "# comment only\n\n").unwrap();
        assert!(read_status(tmp.path()).is_ok());
        std::fs::write(info.join("alternates"), format!("{FAKE_SHARE}/objects\n")).unwrap();
        assert_refused(tmp.path(), "alternates");
    }

    /// Gap 4: the object files read for `HEAD` are opened link-safe, loose or packed.
    #[test]
    fn links_among_object_files_are_refused() {
        // A junction nobody needs is never touched.
        let Some(tmp) = fixture_repo() else { return };
        let target = tempfile::tempdir().unwrap();
        if !junction(
            &tmp.path().join(".git").join("objects").join("zz"),
            target.path(),
        ) {
            eprintln!("skipped: could not create a junction");
            return;
        }
        assert!(assert_fast_and_contained(tmp.path(), "unused junction").is_ok());

        // The loose folder holding HEAD's commit, moved out and linked back.
        let Some(tmp) = fixture_repo() else { return };
        let id = head_id(tmp.path());
        let objects = tmp.path().join(".git").join("objects");
        let outside = tempfile::tempdir().unwrap();
        let moved = outside.path().join(&id[..2]);
        std::fs::rename(objects.join(&id[..2]), &moved).unwrap();
        assert!(junction(&objects.join(&id[..2]), &moved));
        assert_refused(tmp.path(), "loose object folder junction");

        // Packed objects: read correctly, then refused once `pack` is a junction.
        let Some(tmp) = fixture_repo() else { return };
        assert!(git(tmp.path(), &["gc", "-q"]));
        let status = read_status(tmp.path()).expect("packed");
        assert_eq!(
            status.badge(&tmp.path().join("new.txt"), false),
            Some(GitBadge::Added)
        );
        assert_eq!(status.badge(&tmp.path().join(".gitignore"), false), None);
        let pack = tmp.path().join(".git").join("objects").join("pack");
        let outside = tempfile::tempdir().unwrap();
        let moved = outside.path().join("pack");
        std::fs::rename(&pack, &moved).unwrap();
        assert!(junction(&pack, &moved));
        assert_refused(tmp.path(), "pack folder junction");
        let _ = std::fs::remove_dir(&pack);
        if net_junction(&pack) {
            assert_refused(tmp.path(), "pack folder junction to a share");
        }

        // A pack index that is a link to a share.
        let _ = std::fs::remove_dir(&pack);
        std::fs::rename(&moved, &pack).unwrap();
        let idx = std::fs::read_dir(&pack)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|x| x == "idx"))
            .unwrap();
        std::fs::remove_file(&idx).unwrap();
        if net_junction(&idx) {
            assert_refused(tmp.path(), "pack index junction to a share");
            std::fs::remove_dir(&idx).unwrap();
        }
        if symlink(&idx, &format!("{NET}\\pack.idx"), false) {
            assert_refused(tmp.path(), "pack index symlink to a share");
        }
    }

    /// Staged changes read from packs with deltas match what git reports.
    #[test]
    fn staged_changes_are_read_from_deltified_packs() {
        let Some(tmp) = fixture_repo() else { return };
        let root = tmp.path();
        assert!(git(root, &["commit", "-q", "-m", "two"]));
        for round in 0..5 {
            for dir in ["d1", "d2"] {
                std::fs::create_dir_all(root.join(dir)).unwrap();
                for i in 0..30 {
                    let text = format!("{round} {i}\n");
                    std::fs::write(root.join(dir).join(format!("f{i}.txt")), text).unwrap();
                }
            }
            assert!(git(root, &["add", "."]));
            assert!(git(root, &["commit", "-q", "-m", "more"]));
        }
        assert!(git(root, &["gc", "-q", "--aggressive"]));
        std::fs::write(root.join("d1").join("f3.txt"), "staged change\n").unwrap();
        std::fs::write(root.join("d2").join("added.txt"), "x").unwrap();
        assert!(git(root, &["add", "d1/f3.txt", "d2/added.txt"]));
        assert!(git(root, &["rm", "-q", "--cached", "d2/f7.txt"]));
        let status = read_status(root).expect("status");
        let badge = |p: &str| status.badge(&root.join(p), false);
        assert_eq!(badge(r"d1\f3.txt"), Some(GitBadge::Modified));
        assert_eq!(badge(r"d2\added.txt"), Some(GitBadge::Added));
        // Removed from the index: a staged deletion, which outranks "untracked".
        assert_eq!(badge(r"d2\f7.txt"), Some(GitBadge::Modified));
        assert_eq!(badge(r"d1\f4.txt"), None);
        assert_eq!(badge(r"d2\f8.txt"), None);
    }

    #[test]
    fn a_nested_git_file_pointing_at_a_share_is_never_followed() {
        let Some(tmp) = fixture_repo() else { return };
        let root = tmp.path();
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(".git"), format!("gitdir: {FAKE_SHARE}/x\n")).unwrap();
        std::fs::write(nested.join("f.txt"), "f").unwrap();
        let started = Instant::now();
        let status = read_status(root).expect("status");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(status.badge(&nested, true), Some(GitBadge::Added));
    }

    #[test]
    fn oversized_metadata_is_not_read() {
        let big = "#".repeat(MAX_META_BYTES as usize + 1);
        for file in ["config", "HEAD"] {
            let Some(tmp) = fixture_repo() else { return };
            let path = tmp.path().join(".git").join(file);
            let mut text = std::fs::read_to_string(&path).unwrap();
            if file == "HEAD" {
                text = format!("{text}\n{big}");
            } else {
                text.push_str(&big);
            }
            std::fs::write(&path, text).unwrap();
            assert_eq!(
                read_status(tmp.path()).unwrap_err(),
                Failure::TooBig,
                "{file}"
            );
        }
    }

    #[test]
    fn abandoned_scans_are_bounded() {
        let Some(tmp) = fixture_repo() else { return };
        static FULL: AtomicUsize = AtomicUsize::new(MAX_ABANDONED);
        assert_eq!(
            scan_on_thread(tmp.path(), LIMITS, &FULL).unwrap_err(),
            Failure::Busy
        );
        // A scan given no time is abandoned, and settles the count once it ends.
        static OWN: AtomicUsize = AtomicUsize::new(0);
        let mut limits = LIMITS;
        limits.time = Duration::ZERO;
        assert_eq!(
            scan_on_thread(tmp.path(), limits, &OWN).unwrap_err(),
            Failure::TooBig
        );
        let started = Instant::now();
        while OWN.load(Ordering::SeqCst) != 0 {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "abandoned scan never ended"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn online_only_files_are_not_hashed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        std::fs::write(&path, "same\n").unwrap();
        let id = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, b"same\n")
            .unwrap();
        let (fs, top) = fs_at(tmp.path(), false);
        assert!(same_as_blob(&fs, &top, "f.txt", &path, &id));
        forge_core::cloud::simulate_online_only(path.clone());
        assert!(
            !same_as_blob(&fs, &top, "f.txt", &path, &id),
            "an online-only file was read"
        );
        forge_core::cloud::clear_simulated_online_only(&path);
    }

    #[test]
    fn every_part_of_the_scan_is_bounded() {
        let Some(tmp) = fixture_repo() else { return };
        let root = tmp.path();
        let many = root.join("many");
        std::fs::create_dir_all(&many).unwrap();
        for i in 0..20 {
            std::fs::write(many.join(format!("{i}.txt")), "x").unwrap();
        }
        assert!(read_status_with(root, LIMITS).is_ok());
        let small = |f: fn(&mut Limits)| {
            let mut limits = LIMITS;
            f(&mut limits);
            read_status_with(root, limits)
        };
        // The fixture's index has three entries.
        assert_eq!(small(|l| l.index_entries = 2).unwrap_err(), Failure::TooBig);
        assert_eq!(small(|l| l.index_bytes = 10).unwrap_err(), Failure::TooBig);
        // A single untracked folder is reported as one entry, but all its files are visited.
        assert_eq!(small(|l| l.visited = 10).unwrap_err(), Failure::TooBig);
        // No time at all is covered by `abandoned_scans_are_bounded`, with its own count.
    }

    #[test]
    fn a_rewritten_file_with_the_same_contents_is_clean() {
        let Some(tmp) = fixture_repo() else { return };
        let root = tmp.path();
        // The committed `.gitignore`, rewritten byte for byte: new time, same size and content.
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        let status = read_status(root).expect("status");
        assert_eq!(status.badge(&root.join(".gitignore"), false), None);
        assert_eq!(
            status.badge(&root.join("a.txt"), false),
            Some(GitBadge::Modified)
        );
    }

    /// Gap 1: a tracked path below a junction or symlink is never followed; it counts as
    /// modified, as Git counts it.
    #[test]
    fn links_on_tracked_paths_are_never_followed() {
        let Some(tmp) = fixture_repo() else { return };
        let root = tmp.path();
        for dir in ["folder", "net"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("file.txt"), "tracked\n").unwrap();
        }
        assert!(git(root, &["add", "."]));
        assert!(git(root, &["commit", "-q", "-m", "dirs"]));
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("folder")).unwrap();
        std::fs::write(outside.path().join("folder").join("file.txt"), "tracked\n").unwrap();
        std::fs::remove_dir_all(root.join("folder")).unwrap();
        if !junction(&root.join("folder"), &outside.path().join("folder")) {
            eprintln!("skipped: could not create a junction");
            return;
        }
        let status = assert_fast_and_contained(root, "junction on a tracked path").expect("status");
        assert_eq!(
            status.badge(&root.join(r"folder\file.txt"), false),
            Some(GitBadge::Modified)
        );
        // The junction itself is an untracked entry, as Git lists it.
        assert_eq!(
            status.badge(&root.join("folder"), true),
            Some(GitBadge::Added)
        );

        std::fs::remove_dir_all(root.join("net")).unwrap();
        if net_junction(&root.join("net")) || symlink(&root.join("net"), NET, true) {
            let status = assert_fast_and_contained(root, "symlink to a share").expect("status");
            assert_eq!(
                status.badge(&root.join(r"net\file.txt"), false),
                Some(GitBadge::Modified)
            );
        }
    }

    /// Gap 2: each ref along a chain of symbolic refs is opened link-safe, and the chain is
    /// bounded.
    #[test]
    fn symbolic_ref_chains_are_link_safe_and_bounded() {
        let Some(tmp) = fixture_repo() else { return };
        let root = tmp.path();
        let id = head_id(root);
        let refs = root.join(".git").join("refs");
        let main = refs.join("heads").join("main");
        let origin = refs.join("remotes").join("origin");
        // A real chain resolves: the staged file still shows as new, nothing else does.
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::write(origin.join("HEAD"), format!("{id}\n")).unwrap();
        std::fs::write(&main, "ref: refs/remotes/origin/HEAD\n").unwrap();
        let status = assert_fast_and_contained(root, "chain").expect("status");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(
            status.badge(&root.join("new.txt"), false),
            Some(GitBadge::Added)
        );
        assert_eq!(status.badge(&root.join(".gitignore"), false), None);

        // The end of the chain behind a junction.
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("origin")).unwrap();
        std::fs::write(
            outside.path().join("origin").join("HEAD"),
            format!("{id}\n"),
        )
        .unwrap();
        std::fs::remove_dir_all(refs.join("remotes")).unwrap();
        if !junction(&refs.join("remotes"), outside.path()) {
            eprintln!("skipped: could not create a junction");
            return;
        }
        assert_refused(root, "ref chain through a junction");
        std::fs::remove_dir(refs.join("remotes")).unwrap();
        std::fs::create_dir_all(refs.join("remotes")).unwrap();
        if net_junction(&origin) {
            assert_refused(root, "ref chain through a junction to a share");
            std::fs::remove_dir(&origin).unwrap();
        }

        // The end of the chain a symlink to a share.
        std::fs::create_dir_all(&origin).unwrap();
        if symlink(&origin.join("HEAD"), &format!("{NET}\\HEAD"), false) {
            assert_refused(root, "ref chain to a share");
        }

        // A loop ends.
        std::fs::write(&main, "ref: refs/heads/main\n").unwrap();
        assert_refused(root, "ref loop");
    }

    /// Gap 5: ignore files, `packed-refs` and the pack folder are read within caps.
    #[test]
    fn ignore_files_packed_refs_and_packs_are_bounded() {
        let Some(tmp) = fixture_repo() else { return };
        let root = tmp.path();
        let with = |f: fn(&mut Limits)| {
            let mut limits = LIMITS;
            f(&mut limits);
            scan_recorded(root, limits).0
        };
        assert!(with(|_| {}).is_ok());
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join(".gitignore"), "a\nb\nc\nd\n").unwrap();
        // The root's `.gitignore` is `*.log\n`: 6 bytes, one pattern.
        assert_eq!(
            with(|l| l.ignore_file_bytes = 7).unwrap_err(),
            Failure::TooBig
        );
        assert_eq!(
            with(|l| l.ignore_file_patterns = 3).unwrap_err(),
            Failure::TooBig
        );
        assert_eq!(with(|l| l.ignore_bytes = 10).unwrap_err(), Failure::TooBig);
        assert_eq!(
            with(|l| l.ignore_patterns = 4).unwrap_err(),
            Failure::TooBig
        );
        assert!(with(|l| l.ignore_patterns = 5).is_ok());

        assert!(git(root, &["gc", "-q"]));
        assert!(
            !root
                .join(".git")
                .join("refs")
                .join("heads")
                .join("main")
                .exists(),
            "gc packed the branch"
        );
        assert!(with(|_| {}).is_ok());
        assert_eq!(
            with(|l| l.packed_refs_bytes = 10).unwrap_err(),
            Failure::TooBig
        );
        assert_eq!(with(|l| l.odb.packs = 0).unwrap_err(), Failure::TooBig);
        assert_eq!(
            with(|l| l.odb.idx_bytes = 100).unwrap_err(),
            Failure::TooBig
        );
        assert_eq!(
            with(|l| l.odb.object_bytes = 10).unwrap_err(),
            Failure::TooBig
        );
        assert_eq!(
            with(|l| l.odb.pack_bytes = 10).unwrap_err(),
            Failure::TooBig
        );
        assert_eq!(
            with(|l| l.odb.compressed_bytes = 10).unwrap_err(),
            Failure::TooBig
        );
    }

    /// Gap 6: files are opened relative to handles already held, so a folder swapped for a
    /// junction after it was opened changes nothing, and the junction itself is refused.
    #[test]
    fn opens_go_through_held_handles() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a").join("f.txt"), "original").unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("f.txt"), "outside").unwrap();

        let (fs, top) = fs_at(&root, true);
        let a = fs.dir(&top, "a").unwrap();
        std::fs::rename(root.join("a"), root.join("moved")).unwrap();
        if !junction(&root.join("a"), outside.path()) {
            eprintln!("skipped: could not create a junction");
            return;
        }
        let read = fs.file(&a, "f.txt").unwrap().read_capped(100).unwrap();
        assert_eq!(read, b"original");
        assert_eq!(fs.dir(&top, "a").err(), Some(safe_fs::Error::Link));
        assert_eq!(fs.file(&top, "a").err(), Some(safe_fs::Error::WrongKind));
        assert_eq!(
            fs.file_at(&top, "a/f.txt").err(),
            Some(safe_fs::Error::Link)
        );
        assert_contained(&fs.opened(), fs.root_final(), "held handles");

        // A held folder moved out of the root is refused from then on.
        std::fs::rename(root.join("moved"), tmp.path().join("gone")).unwrap();
        assert_eq!(fs.file(&a, "f.txt").err(), Some(safe_fs::Error::Link));

        // Names that are not one plain component are never passed to the system.
        for bad in ["..", ".", "a/b", r"a\b", "f.txt:stream", ""] {
            assert_eq!(
                fs.file(&top, bad).err(),
                Some(safe_fs::Error::BadName),
                "{bad}"
            );
        }
    }

    /// Review item 1: repository discovery never looks anything up by path below a drive's
    /// root, so a junction among the browsed folder's ancestors is refused, not followed.
    #[test]
    fn a_junction_among_the_ancestors_is_not_followed() {
        let Some(repo) = fixture_repo() else { return };
        std::fs::create_dir_all(repo.path().join("sub")).unwrap();
        assert_eq!(
            find(&repo.path().join("sub")),
            Some(lexical(repo.path()).unwrap())
        );

        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("j");
        if !junction(&link, repo.path()) {
            eprintln!("skipped: could not create a junction");
            return;
        }
        let never = || false;
        for dir in [link.clone(), link.join("sub")] {
            assert_eq!(
                find_repo_root(&dir, &never).unwrap_err(),
                Failure::Unsafe,
                "{}",
                dir.display()
            );
            assert_eq!(read_status(&dir).unwrap_err(), Failure::Unsafe);
        }
        // A `.git` that is itself a junction is found (so the folder counts as a repository)
        // but never entered.
        let plain = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plain.path().join("sub")).unwrap();
        assert!(junction(
            &plain.path().join(".git"),
            &repo.path().join(".git")
        ));
        assert_eq!(
            find(&plain.path().join("sub")),
            Some(lexical(plain.path()).unwrap())
        );
        assert_refused(plain.path(), ".git junction found by discovery");
    }

    /// Review item 1: a folder on a share, or on no local drive, is refused before anything
    /// is opened.
    #[test]
    fn remote_roots_are_refused_before_any_probe() {
        let never = || false;
        let before = safe_fs::PATH_OPENS.with(|n| n.get());
        let started = Instant::now();
        for path in [
            r"\\192.0.2.1\x\repo",
            "//192.0.2.1/x/repo",
            r"\\?\UNC\192.0.2.1\x\repo",
            r"\\.\pipe\x",
            r"\\?\GLOBALROOT\Device\Mup\192.0.2.1\x",
        ] {
            let path = Path::new(path);
            assert_eq!(
                find_repo_root(path, &never).unwrap_err(),
                Failure::Unsafe,
                "{path:?}"
            );
            assert_eq!(
                open_root(path, true, &never).err(),
                Some(Failure::Unsafe),
                "{path:?}"
            );
            assert_eq!(read_status(path).unwrap_err(), Failure::Unsafe, "{path:?}");
        }
        // A drive letter that is not a local drive (here: none at all).
        let unused = ('D'..='Z').rev().find(|d| {
            safe_fs::local_drive(Path::new(&format!(r"{d}:\"))).is_none()
                && !Path::new(&format!(r"{d}:\")).exists()
        });
        if let Some(d) = unused {
            let path = PathBuf::from(format!(r"{d}:\repo"));
            assert_eq!(find_repo_root(&path, &never).unwrap_err(), Failure::Unsafe);
        }
        assert_eq!(
            safe_fs::PATH_OPENS.with(|n| n.get()),
            before,
            "something was opened"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    /// Review item 2: a tiny tree that names one subtree under many names, nested, would
    /// expand to millions of paths; the staged diff stops at its caps.
    #[test]
    fn staged_changes_are_capped() {
        let Some(tmp) = fixture_repo() else { return };
        let root = tmp.path();
        let with = |f: fn(&mut Limits)| {
            let mut limits = LIMITS;
            f(&mut limits);
            scan_recorded(root, limits).0
        };
        // The fixture stages `new.txt`.
        assert!(with(|_| {}).is_ok());
        assert_eq!(with(|l| l.staged_entries = 0).unwrap_err(), Failure::TooBig);
        assert_eq!(with(|l| l.staged_bytes = 3).unwrap_err(), Failure::TooBig);
        assert_eq!(with(|l| l.path_bytes = 3).unwrap_err(), Failure::TooBig);

        // 16 names for the same subtree, six levels deep: 16.7 million paths from 7 trees.
        let run = |args: &[&str], input: &str| -> String {
            use std::io::Write;
            let mut child = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let blob = run(&["hash-object", "-w", "--stdin"], "x");
        let mut tree = run(&["mktree"], &format!("100644 blob {blob}\tf\n"));
        for _ in 0..6 {
            let listing: String = (0..16)
                .map(|i| format!("040000 tree {tree}\td{i:02}\n"))
                .collect();
            tree = run(&["mktree"], &listing);
        }
        let commit = run(&["commit-tree", &tree, "-m", "bomb"], "");
        assert!(git(root, &["update-ref", "refs/heads/main", &commit]));
        let started = Instant::now();
        assert_eq!(read_status(root).unwrap_err(), Failure::TooBig);
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{:?}",
            started.elapsed()
        );
    }

    /// Review item 3: a file with another name (a hard link) could be a file outside the
    /// folder; it is refused.
    #[test]
    fn hard_linked_files_are_refused() {
        let Some(tmp) = fixture_repo() else { return };
        let outside = tempfile::tempdir().unwrap();
        let head = tmp.path().join(".git").join("HEAD");
        let theirs = outside.path().join("HEAD");
        std::fs::copy(&head, &theirs).unwrap();
        std::fs::remove_file(&head).unwrap();
        std::fs::hard_link(&theirs, &head).unwrap();
        assert_refused(tmp.path(), "hard-linked HEAD");

        let (fs, top) = fs_at(tmp.path(), false);
        std::fs::hard_link(&theirs, tmp.path().join("linked.txt")).unwrap();
        assert_eq!(
            fs.file(&top, "linked.txt").err(),
            Some(safe_fs::Error::Link)
        );
        assert!(fs.file(&top, "a.txt").is_ok());
    }

    /// Review item 5: every relative open asks not to follow links, and every file open not
    /// to recall a cloud placeholder.
    #[test]
    fn every_open_asks_for_no_recall_and_no_link_following() {
        use windows::Wdk::Storage::FileSystem::{FILE_OPEN_NO_RECALL, FILE_OPEN_REPARSE_POINT};
        let file = safe_fs::create_options(false).0;
        assert_ne!(file & FILE_OPEN_NO_RECALL.0, 0);
        assert_ne!(file & FILE_OPEN_REPARSE_POINT.0, 0);
        assert_ne!(
            safe_fs::create_options(true).0 & FILE_OPEN_REPARSE_POINT.0,
            0
        );
    }
}
