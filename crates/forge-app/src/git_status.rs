//! Git status badges for the folder views.
//!
//! Status is read with `gix` on one background thread, never on the UI thread, and never by
//! running `git` or any other program: a folder from elsewhere can carry a `.git/config` that
//! names programs (fsmonitor, filter drivers), and merely listing it must not start them. Results
//! are cached per repository and refreshed when the file watcher reports a change, or after a
//! short age. Failures are quiet: a repository that cannot be read simply shows no badges.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use gix::status::plumbing::index_as_worktree::traits::{CompareBlobs, ReadData, SubmoduleStatus};
use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};
use gix::status::plumbing::index_as_worktree_with_renames::{Entry, VisitEntry};

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
    /// Read the status of this repository.
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
                Request::FindRoot(dir) => Reply::Root {
                    root: find_repo_root(&dir),
                    dir,
                },
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
    /// Size of `.git/index` on disk, checked before it is loaded.
    index_bytes: u64,
    /// Entries in the index (each is compared with its file).
    index_entries: usize,
    /// Folders and files the directory walk looks at, emitted or not.
    visited: usize,
}

const LIMITS: Limits = Limits {
    time: TIMEOUT,
    index_bytes: 64 * 1024 * 1024,
    index_entries: 100_000,
    visited: 50_000,
};

/// Reads the status of the repository at `root` with `gix`, in this process.
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
/// No external program ever runs: gix implements no fsmonitor, configuration is loaded in
/// isolation (the repository's own config only, never `git` itself, system or user config,
/// and no includes), and gix never reads file contents, so no filter driver from
/// `.gitattributes` can start. A tracked file whose size differs from the index counts as
/// modified; one whose time alone changed is hashed raw by [`same_as_blob`].
///
/// Refused before gix opens anything (see [`preflight`]): a repository inside a cloud sync
/// root, one whose `.git` file, `commondir`, config or alternates name a path elsewhere, and
/// one with links where its metadata should be.
fn read_status_with(root: &Path, limits: Limits) -> Result<RepoStatus, Failure> {
    scan_on_thread(root, limits, &ABANDONED)
}

/// Runs the scan on its own thread and waits at most `limits.time` for it. A scan still
/// running then is told to stop and left to finish on its own (gix cannot be interrupted
/// while it opens a repository); it counts in `abandoned` until it does.
fn scan_on_thread(
    root: &Path,
    limits: Limits,
    abandoned: &'static AtomicUsize,
) -> Result<RepoStatus, Failure> {
    if abandoned.load(Ordering::SeqCst) >= MAX_ABANDONED {
        return Err(Failure::Busy);
    }
    // The budget covers everything from here on, including opening and loading the index.
    let deadline = Instant::now() + limits.time;
    let interrupt = Arc::new(AtomicBool::new(false));
    // Set by the first of "the scan finished" and "the caller gave up"; the second one to
    // arrive settles the count.
    let settled = Arc::new(AtomicBool::new(false));
    let (done, result) = crossbeam_channel::bounded(1);
    let spawned = {
        let root = root.to_path_buf();
        let interrupt = Arc::clone(&interrupt);
        let settled = Arc::clone(&settled);
        thread::Builder::new()
            .name("git-status-scan".into())
            .spawn(move || {
                let _ = done.send(read_within(&root, limits, &interrupt, deadline));
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
        // The scan panicked: it is not running any more.
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
    interrupt: &AtomicBool,
    deadline: Instant,
) -> Result<RepoStatus, Failure> {
    // The caller raises `interrupt` (which gix polls) at the deadline; the clock check makes
    // it exact between steps.
    let over = || interrupt.load(Ordering::SeqCst) || Instant::now() >= deadline;
    let err = |e: &dyn std::fmt::Display| Failure::Error(e.to_string());
    let layout = preflight(root)?;
    // Opening `.git` itself, never `root`: gix would otherwise fall back to reading `root` as
    // a bare repository whose config `preflight` has not checked.
    let repo = gix::open_opts(
        root.join(".git"),
        gix::open::Options::isolated().strict_config(false),
    )
    .map_err(|e| err(&e))?;
    // What gix resolved must be what was checked. Text comparisons only.
    let same = |a: &Path, b: &Path| lexical(a).is_some_and(|a| path_key(&a) == path_key(b));
    let index_file = repo.index_path();
    let as_checked = same(repo.git_dir(), &layout.git_dir)
        && same(repo.common_dir(), &layout.common_dir)
        && repo.workdir().is_some_and(|w| same(w, &layout.root))
        && lexical(&index_file).is_some_and(|i| inside(&i, &layout.git_dir));
    if !as_checked {
        return Err(Failure::Unsafe);
    }
    let branch = repo
        .head_name()
        .ok()
        .flatten()
        .map(|name| name.shorten().to_string());
    if std::fs::metadata(&index_file).is_ok_and(|m| m.len() > limits.index_bytes) {
        return Err(Failure::TooBig);
    }
    let index = repo.index_or_empty().map_err(|e| err(&e))?;
    if index.entries().len() > limits.index_entries || over() {
        return Err(Failure::TooBig);
    }

    let mut status = RepoStatus::new(root.to_path_buf(), branch);

    // Tracked files: stat data against the index.
    let mut tracked = Tracked {
        status: &mut status,
        suspects: Vec::new(),
    };
    repo.index_worktree_status(
        &index,
        None::<&str>,
        &mut tracked,
        StatOnly,
        NoSubmodules,
        &mut gix::progress::Discard,
        interrupt,
        gix::status::index_worktree::Options {
            sorting: None,
            dirwalk_options: None,
            rewrites: None,
            thread_limit: Some(1),
        },
    )
    .map_err(|e| err(&e))?;
    // Same size but a new time: hash the bytes, as git would after a touch or right after a
    // commit. Past the caps a file simply counts as modified.
    let suspects = std::mem::take(&mut tracked.suspects);
    for (n, (rela_path, id)) in suspects.into_iter().enumerate() {
        if over() {
            return Err(Failure::TooBig);
        }
        if n >= MAX_HASHED_FILES || !same_as_blob(&root.join(&rela_path), &id) {
            status.add(&rela_path, GitBadge::Modified);
        }
    }
    if over() {
        return Err(Failure::TooBig);
    }

    // Untracked and ignored entries. Untracked files are emitted one by one so every one
    // counts towards the limit; ignored folders are reported whole, as git does.
    let options = repo
        .dirwalk_options()
        .map_err(|e| err(&e))?
        .emit_untracked(gix::dir::walk::EmissionMode::Matching)
        .emit_ignored(Some(gix::dir::walk::EmissionMode::CollapseDirectory))
        // Telling a nested repository apart means following its `.git` file, which may name
        // any path; walk into it like a plain folder instead. Symlinks are never followed.
        .recurse_repositories(true)
        .classify_untracked_bare_repositories(false)
        .symlinks_to_directories_are_ignored_like_directories(false);
    let mut walk = Walk {
        status: &mut status,
        visited: 0,
        max: limits.visited,
        too_many: false,
    };
    let walked = repo.dirwalk(&index, None::<&str>, interrupt, options, &mut walk);
    if walk.too_many || over() {
        return Err(Failure::TooBig);
    }
    walked.map_err(|e| err(&e))?;

    // Staged changes: the index against HEAD's tree. Compares object ids only.
    match repo.head_tree_id() {
        Ok(tree) => {
            repo.tree_index_status(
                &tree,
                &index,
                None,
                gix::status::tree_index::TrackRenames::Disabled,
                |change, _, _| {
                    if over() {
                        return Ok(std::ops::ControlFlow::Break(()));
                    }
                    use gix::diff::index::ChangeRef;
                    let (location, badge) = match &change {
                        ChangeRef::Addition { location, .. } => (location, GitBadge::Added),
                        ChangeRef::Deletion { location, .. }
                        | ChangeRef::Modification { location, .. }
                        | ChangeRef::Rewrite { location, .. } => (location, GitBadge::Modified),
                    };
                    status.add_staged(&location.to_string(), badge);
                    Ok(std::ops::ControlFlow::Continue(()))
                },
            )
            .map_err(|e| err(&e))?;
        }
        // Unborn branch: everything in the index is new.
        Err(_) => {
            for entry in index.entries() {
                if over() {
                    break;
                }
                status.add_staged(&entry.path(&index).to_string(), GitBadge::Added);
            }
        }
    }
    if over() {
        return Err(Failure::TooBig);
    }
    Ok(status)
}

/// Largest `.git` file, `HEAD`, `commondir`, config or alternates file read before opening a
/// repository. Real ones are a few kilobytes at most.
const MAX_META_BYTES: u64 = 1024 * 1024;

/// Children of `objects` checked for links (loose object folders, `pack`, `info`).
const MAX_OBJECT_DIRS: usize = 512;

/// Where a repository's parts are, as checked by [`preflight`].
#[derive(Debug)]
struct Layout {
    /// The work tree: the folder being browsed, in [`lexical`] form.
    root: PathBuf,
    /// The repository's own git directory (`root/.git`, or where a `.git` file points).
    git_dir: PathBuf,
    /// Shared git directory of a linked worktree (`commondir`), else `git_dir`.
    common_dir: PathBuf,
}

/// Checks everything the repository's own files say about where its parts are, before gix
/// (or anything else) touches those places.
///
/// A folder from elsewhere can carry a `.git` whose files name any path. Even checking that
/// a network path exists makes Windows connect to the server, which leaks the user's NTLM
/// hash and can stall for many seconds. So every path read from the repository is first
/// resolved and compared as text ([`resolve`]), and only touched once it is known to be on
/// the root's volume and inside the place it belongs:
///
/// * the `.git` file (`gitdir:`) and `commondir`: on the root's volume (linked worktrees
///   and submodules keep their git directory elsewhere on the same disk);
/// * `core.worktree`: exactly `root`;
/// * `gitoxide.core.indexFile` and `gitoxide.core.shallowFile`: inside the git directory;
/// * `core.excludesFile` and `core.attributesFile`: inside `root` or the git directory;
/// * `include.path` and `includeIf.*.path`: inside the git directory (gix does not follow
///   includes here anyway);
/// * `objects/info/alternates`: refused if it lists anything.
///
/// The fixed metadata files (`HEAD`, `config`, `index`, `refs`, `objects`, `info/exclude`, ...)
/// must not be links (symlinks or junctions), which could lead anywhere. Metadata files over
/// [`MAX_META_BYTES`] make the repository too big rather than being read.
fn preflight(root: &Path) -> Result<Layout, Failure> {
    let root = lexical(root).ok_or(Failure::Unsafe)?;
    if forge_core::cloud::in_cloud_sync_root(&root) {
        return Err(Failure::Unsafe);
    }
    let dot_git = root.join(".git");
    let md = std::fs::symlink_metadata(&dot_git).map_err(|e| Failure::Error(e.to_string()))?;
    if is_link(&md) {
        return Err(Failure::Unsafe);
    }
    let git_dir = if md.is_file() {
        let text = read_meta(&dot_git)?.ok_or(Failure::Unsafe)?;
        let target = text.trim().strip_prefix("gitdir:").ok_or(Failure::Unsafe)?;
        let git_dir = resolve(&root, target.trim(), &root).ok_or(Failure::Unsafe)?;
        not_linked_below(&root, &git_dir)?;
        git_dir
    } else {
        dot_git
    };
    let common_dir = match read_meta(&git_dir.join("commondir"))? {
        Some(text) => resolve(&git_dir, text.trim(), &root).ok_or(Failure::Unsafe)?,
        None => git_dir.clone(),
    };
    not_linked_below(&root, &common_dir)?;
    not_a_link(&git_dir)?;
    not_a_link(&common_dir)?;
    for dir in [&git_dir, &common_dir] {
        if !inside(dir, &root) && forge_core::cloud::in_cloud_sync_root(dir) {
            return Err(Failure::Unsafe);
        }
    }

    // The fixed places gix reads. Missing ones are fine; links are not.
    for base in [&git_dir, &common_dir] {
        for rel in [
            "HEAD",
            "config",
            "config.worktree",
            "index",
            "commondir",
            "packed-refs",
            "shallow",
            "info",
            r"info\exclude",
            r"info\attributes",
            r"info\sparse-checkout",
            "objects",
            r"objects\info",
            r"objects\info\alternates",
            r"objects\pack",
            "refs",
            r"refs\heads",
        ] {
            not_a_link(&base.join(rel))?;
        }
    }
    if let Ok(children) = std::fs::read_dir(common_dir.join("objects")) {
        for (n, child) in children.enumerate() {
            if n >= MAX_OBJECT_DIRS {
                return Err(Failure::TooBig);
            }
            let child = child.map_err(|e| Failure::Error(e.to_string()))?;
            if child.file_type().map_or(true, |t| t.is_symlink()) {
                return Err(Failure::Unsafe);
            }
            not_a_link(&child.path())?;
        }
    }

    // HEAD and the branch it names.
    if let Some(head) = read_meta(&git_dir.join("HEAD"))? {
        if let Some(target) = head.trim().strip_prefix("ref:") {
            let mut path = common_dir.clone();
            for part in target.trim().split('/') {
                if !plain_name(std::ffi::OsStr::new(part)) {
                    return Err(Failure::Unsafe);
                }
                path.push(part);
                not_a_link(&path)?;
            }
        }
    }

    // The repository's config, and the per-worktree one gix may add to it.
    let configs = [common_dir.join("config"), git_dir.join("config.worktree")];
    for config in &configs {
        let Some(text) = read_meta(config)? else {
            continue;
        };
        let file = gix::config::File::from_bytes_no_includes(
            text.as_bytes(),
            gix::config::file::Metadata::from(gix::config::Source::Local),
            Default::default(),
        )
        .map_err(|e| Failure::Error(e.to_string()))?;
        let config_dir = config.parent().unwrap_or(&git_dir);
        let in_git_dir = |p: &Path| inside(p, &git_dir) || inside(p, &common_dir);
        for section in file.sections() {
            let name = section.header().name().to_string();
            let sub = section.header().subsection_name().map(|s| s.to_string());
            let values = |key: &str| {
                section
                    .body()
                    .values(key)
                    .into_iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
            };
            let ok = match (name.to_ascii_lowercase().as_str(), sub.as_deref()) {
                ("core", _) => {
                    values("worktree").iter().all(|v| {
                        resolve(&git_dir, v, &root).is_some_and(|w| path_key(&w) == path_key(&root))
                    }) && ["excludesFile", "attributesFile"]
                        .iter()
                        .flat_map(|key| values(key))
                        .all(|v| {
                            absolute(v.strip_prefix(":(optional)").unwrap_or(&v), &root)
                                .is_some_and(|p| inside(&p, &root) || in_git_dir(&p))
                        })
                }
                ("gitoxide", Some(sub)) if sub.eq_ignore_ascii_case("core") => {
                    ["indexFile", "shallowFile"]
                        .iter()
                        .flat_map(|key| values(key))
                        .all(|v| absolute(&v, &root).is_some_and(|p| in_git_dir(&p)))
                }
                ("include", None) | ("includeif", Some(_)) => values("path")
                    .iter()
                    .all(|v| resolve(config_dir, v, &root).is_some_and(|p| in_git_dir(&p))),
                _ => true,
            };
            if !ok {
                return Err(Failure::Unsafe);
            }
        }
    }

    // Alternates add object folders anywhere, each with alternates of its own.
    if let Some(text) = read_meta(&common_dir.join(r"objects\info\alternates"))? {
        if text
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            return Err(Failure::Unsafe);
        }
    }
    Ok(Layout {
        root,
        git_dir,
        common_dir,
    })
}

/// Contents of the metadata file at `path` (already known to be local), or `None` if there is
/// none. Links are refused and files over [`MAX_META_BYTES`] make the repository too big.
fn read_meta(path: &Path) -> Result<Option<String>, Failure> {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return Ok(None);
    };
    if is_link(&md) || !md.is_file() {
        return Err(Failure::Unsafe);
    }
    if md.len() > MAX_META_BYTES {
        return Err(Failure::TooBig);
    }
    let bytes = std::fs::read(path).map_err(|e| Failure::Error(e.to_string()))?;
    if bytes.len() as u64 > MAX_META_BYTES {
        return Err(Failure::TooBig);
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Is this a symlink, junction or other reparse point? Read with `symlink_metadata`, so the
/// link itself is never followed.
fn is_link(md: &std::fs::Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    md.file_type().is_symlink()
        || forge_core::cloud::metadata_attributes(md) & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Refuses `path` (already known to be local) if it is a link. A missing path is fine.
fn not_a_link(path: &Path) -> Result<(), Failure> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if is_link(&md) => Err(Failure::Unsafe),
        _ => Ok(()),
    }
}

/// Refuses `path` if it or a folder between `base` and it is a link. Parts outside `base`
/// are the user's own and not checked.
fn not_linked_below(base: &Path, path: &Path) -> Result<(), Failure> {
    let base_key = path_key(base);
    for ancestor in path.ancestors() {
        let key = path_key(ancestor);
        if key.len() <= base_key.len() || !inside(ancestor, base) {
            break;
        }
        not_a_link(ancestor)?;
    }
    Ok(())
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

/// Compares tracked files by their index stat data only, reporting whether the size changed.
/// It never reads a file through gix: that is what keeps filter drivers
/// (`filter.<name>.clean` / `.process`) from running.
#[derive(Clone)]
struct StatOnly;

impl CompareBlobs for StatOnly {
    /// `true` when the size differs from the index (certainly modified).
    type Output = bool;
    fn compare_blobs<'a, 'b>(
        &mut self,
        entry: &gix::index::Entry,
        worktree_blob_size: u64,
        _data: impl ReadData<'a>,
        _buf: &mut Vec<u8>,
    ) -> gix::Result<Option<bool>> {
        Ok(Some(worktree_blob_size != u64::from(entry.stat.size)))
    }
}

/// Most same-size files whose contents are hashed to tell a real change from a touched or
/// racily-clean file (one written in the same second as the index).
const MAX_HASHED_FILES: usize = 256;
/// Largest file hashed for that check; bigger ones count as modified.
const MAX_HASHED_BYTES: u64 = 16 * 1024 * 1024;

/// Does the file at `path` still hash to `id` as a blob? Reads the raw bytes itself, with no
/// filter or line-ending conversion, so a filtered file may count as changed, never the
/// reverse. An online-only file (say, in a sync folder nested in the repository) is never
/// read, which would download it; it counts as changed.
fn same_as_blob(path: &Path, id: &gix::ObjectId) -> bool {
    if std::fs::symlink_metadata(path).map_or(true, |m| !m.is_file() || m.len() > MAX_HASHED_BYTES)
    {
        return false;
    }
    if !forge_core::cloud::background_read_allowed(path) {
        return false;
    }
    std::fs::read(path).is_ok_and(|bytes| {
        gix::objs::compute_hash(id.kind(), gix::objs::Kind::Blob, &bytes).is_ok_and(|h| h == *id)
    })
}

/// Submodules are listed as folders without looking inside them.
#[derive(Clone)]
struct NoSubmodules;

impl SubmoduleStatus for NoSubmodules {
    type Output = ();
    fn status(
        &mut self,
        _entry: &gix::index::Entry,
        _path: &gix::bstr::BStr,
    ) -> gix::Result<Option<()>> {
        Ok(None)
    }
}

/// Badges for tracked files that differ from the index. Same-size changes go to `suspects`
/// for a content check.
struct Tracked<'s> {
    status: &'s mut RepoStatus,
    suspects: Vec<(String, gix::ObjectId)>,
}

impl<'index> VisitEntry<'index> for Tracked<'_> {
    type ContentChange = bool;
    type SubmoduleStatus = ();

    fn visit_entry(&mut self, entry: Entry<'index, bool, ()>) {
        if let Entry::Modification {
            rela_path,
            status,
            entry,
            ..
        } = entry
        {
            let badge = match status {
                EntryStatus::Conflict { .. } => GitBadge::Conflicted,
                EntryStatus::IntentToAdd => GitBadge::Added,
                EntryStatus::NeedsUpdate(_) => return,
                EntryStatus::Change(Change::Modification {
                    content_change: Some(false),
                    ..
                }) => {
                    self.suspects.push((rela_path.to_string(), entry.id));
                    return;
                }
                EntryStatus::Change(_) => GitBadge::Modified,
            };
            self.status.add(&rela_path.to_string(), badge);
        }
    }
}

/// Collects untracked and ignored entries, stopping once `max` folders and files were seen.
struct Walk<'s> {
    status: &'s mut RepoStatus,
    visited: usize,
    max: usize,
    too_many: bool,
}

impl Walk<'_> {
    fn count(&mut self) -> bool {
        self.visited += 1;
        if self.visited > self.max {
            self.too_many = true;
        }
        !self.too_many
    }
}

impl gix::dir::walk::Delegate for Walk<'_> {
    fn emit(
        &mut self,
        entry: gix::dir::EntryRef<'_>,
        _collapsed_directory_status: Option<gix::dir::entry::Status>,
    ) -> gix::dir::walk::Action {
        if !self.count() {
            return std::ops::ControlFlow::Break(());
        }
        let badge = match entry.status {
            gix::dir::entry::Status::Untracked => GitBadge::Added,
            gix::dir::entry::Status::Ignored(_) => GitBadge::Ignored,
            _ => return std::ops::ControlFlow::Continue(()),
        };
        let dir = matches!(entry.disk_kind, Some(gix::dir::entry::Kind::Directory));
        let slash = if dir { "/" } else { "" };
        self.status
            .add(&format!("{}{slash}", entry.rela_path), badge);
        std::ops::ControlFlow::Continue(())
    }

    fn can_recurse(
        &mut self,
        entry: gix::dir::EntryRef<'_>,
        for_deletion: Option<gix::dir::walk::ForDeletionMode>,
        worktree_root_is_repository: bool,
    ) -> bool {
        // Every folder considered counts, whether or not it is entered or emitted.
        self.count()
            && entry.status.can_recurse(
                entry.disk_kind,
                entry.pathspec_match,
                for_deletion,
                worktree_root_is_repository,
            )
    }
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
            preflight(root).unwrap_err(),
            Failure::Unsafe,
            "{what}: preflight"
        );
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

    #[test]
    fn linked_worktrees_and_submodules_still_work() {
        let Some(main) = fixture_repo() else { return };
        let wt_parent = tempfile::tempdir_in(main.path().parent().unwrap()).unwrap();
        let wt = wt_parent.path().join("wt");
        assert!(git(
            main.path(),
            &["worktree", "add", "-q", "-b", "side", &wt.to_string_lossy()]
        ));
        std::fs::write(wt.join("a.txt"), "worktree change, longer\n").unwrap();
        let status = read_status(&wt).expect("linked worktree");
        assert_eq!(status.branch.as_deref(), Some("side"));
        assert_eq!(
            status.badge(&wt.join("a.txt"), false),
            Some(GitBadge::Modified)
        );

        // A `commondir` that leaves the volume is refused like a `.git` file would be.
        let git_dir = main.path().join(".git").join("worktrees").join("wt");
        std::fs::write(git_dir.join("commondir"), format!("{FAKE_SHARE}/x\n")).unwrap();
        assert_refused(&wt, "commondir");

        // A submodule: `.git` is a file into the parent's `.git/modules`, whose config sets a
        // relative `core.worktree` back to the submodule folder.
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
        let sub = main.path().join("sub");
        std::fs::write(sub.join("a.txt"), "submodule change, longer\n").unwrap();
        let status = read_status(&sub).expect("submodule");
        assert_eq!(
            status.badge(&sub.join("a.txt"), false),
            Some(GitBadge::Modified)
        );
    }

    #[test]
    fn config_paths_outside_the_repository_are_refused() {
        let outside = tempfile::tempdir().unwrap();
        let elsewhere = outside.path().join("never-opened");
        std::fs::write(&elsewhere, "[core]\n\tbare = true\n").unwrap();
        let elsewhere = config_path(&elsewhere);
        let share = format!("\"{FAKE_SHARE}/x\"");
        for section in [
            "[core]\n\tworktree",
            "[core]\n\texcludesFile",
            "[core]\n\tattributesFile",
            "[gitoxide \"core\"]\n\tindexFile",
            "[gitoxide \"core\"]\n\tshallowFile",
            "[include]\n\tpath",
            "[includeIf \"gitdir:/\"]\n\tpath",
        ] {
            // A relative include would stay in the git directory, so only the others are tried.
            let relative = "\"relative/x\"".to_string();
            let values = if section.contains("include") {
                vec![&share, &elsewhere]
            } else {
                vec![&share, &elsewhere, &relative]
            };
            for value in values {
                let Some(tmp) = fixture_repo() else { return };
                append_config(tmp.path(), &format!("{section} = {value}"));
                assert_refused(tmp.path(), &format!("{section} = {value}"));
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

    #[test]
    fn links_in_the_git_directory_are_refused() {
        let Some(tmp) = fixture_repo() else { return };
        let target = tempfile::tempdir().unwrap();
        let link = tmp.path().join(".git").join("objects").join("zz");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(target.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            eprintln!("skipped: could not create a junction");
            return;
        }
        assert_refused(tmp.path(), "junction in objects");
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
        assert!(same_as_blob(&path, &id));
        forge_core::cloud::simulate_online_only(path.clone());
        assert!(!same_as_blob(&path, &id), "an online-only file was read");
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
}
