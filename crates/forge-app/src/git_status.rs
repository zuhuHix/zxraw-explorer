//! Git status badges for the folder views.
//!
//! Status is read with `gix` on one background thread, never on the UI thread, and never by
//! running `git` or any other program: a folder from elsewhere can carry a `.git/config` that
//! names programs (fsmonitor, filter drivers), and merely listing it must not start them. Results
//! are cached per repository and refreshed when the file watcher reports a change, or after a
//! short age. Failures are quiet: a repository that cannot be read simply shows no badges.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use gix::status::plumbing::index_as_worktree::EntryStatus;
use gix::status::plumbing::index_as_worktree::traits::{CompareBlobs, ReadData, SubmoduleStatus};
use gix::status::plumbing::index_as_worktree_with_renames::{Entry, VisitEntry};

/// Longest a status walk may take before it is stopped.
const TIMEOUT: Duration = Duration::from_secs(4);
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

/// [`read_status`] with explicit limits.
///
/// No external program ever runs: gix implements no fsmonitor, configuration is loaded in
/// isolation (the repository's own config only, never `git` itself, system or user config),
/// and file contents are never read, so no filter driver from `.gitattributes` can start. A
/// tracked file whose size or time differs from the index counts as modified.
///
/// Refused without reading anything: a repository inside a cloud sync root (the walk would
/// read `.gitignore` files that may be online-only), a `.git` file pointing at a network
/// path, and a repository whose config moves its work tree (`core.worktree`) away from `root`.
fn read_status_with(root: &Path, limits: Limits) -> Result<RepoStatus, Failure> {
    // The budget covers everything from here on, including opening and loading the index.
    let deadline = Instant::now() + limits.time;
    let interrupt = Arc::new(AtomicBool::new(false));
    let (done, finished) = crossbeam_channel::bounded::<()>(0);
    let flag = Arc::clone(&interrupt);
    let time = limits.time;
    thread::spawn(move || {
        if let Err(crossbeam_channel::RecvTimeoutError::Timeout) = finished.recv_timeout(time) {
            flag.store(true, Ordering::SeqCst);
        }
    });
    let over = || interrupt.load(Ordering::SeqCst) || Instant::now() >= deadline;
    let result = read_within(root, limits, &interrupt, deadline);
    drop(done);
    match result {
        _ if over() => Err(Failure::TooBig),
        result => result,
    }
}

fn read_within(
    root: &Path,
    limits: Limits,
    interrupt: &AtomicBool,
    deadline: Instant,
) -> Result<RepoStatus, Failure> {
    // The timer thread raises `interrupt` (which gix polls); the clock check makes the
    // deadline exact between steps.
    let over = || interrupt.load(Ordering::SeqCst) || Instant::now() >= deadline;
    let err = |e: &dyn std::fmt::Display| Failure::Error(e.to_string());
    if forge_core::cloud::in_cloud_sync_root(root) {
        return Err(Failure::Unsafe);
    }
    if git_file_points_to_network(root) {
        return Err(Failure::Unsafe);
    }
    let repo = gix::open_opts(root, gix::open::Options::isolated().strict_config(false))
        .map_err(|e| err(&e))?;
    // `core.worktree` in the repository's own config could point the walk anywhere.
    let same_tree = match (repo.workdir(), std::fs::canonicalize(root)) {
        (Some(workdir), Ok(root)) => std::fs::canonicalize(workdir).is_ok_and(|w| w == root),
        _ => false,
    };
    if !same_tree {
        return Err(Failure::Unsafe);
    }
    let branch = repo
        .head_name()
        .ok()
        .flatten()
        .map(|name| name.shorten().to_string());
    let index_file = repo.index_path();
    if std::fs::metadata(&index_file).is_ok_and(|m| m.len() > limits.index_bytes) {
        return Err(Failure::TooBig);
    }
    let index = repo.index_or_empty().map_err(|e| err(&e))?;
    if index.entries().len() > limits.index_entries || over() {
        return Err(Failure::TooBig);
    }

    let mut status = RepoStatus::new(root.to_path_buf(), branch);

    // Tracked files: stat data against the index.
    repo.index_worktree_status(
        &index,
        None::<&str>,
        &mut Tracked(&mut status),
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
    if over() {
        return Err(Failure::TooBig);
    }

    // Untracked and ignored entries. Untracked files are emitted one by one so every one
    // counts towards the limit; ignored folders are reported whole, as git does.
    let options = repo
        .dirwalk_options()
        .map_err(|e| err(&e))?
        .emit_untracked(gix::dir::walk::EmissionMode::Matching)
        .emit_ignored(Some(gix::dir::walk::EmissionMode::CollapseDirectory));
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

/// Does `root/.git` name a git directory on a network path (`gitdir: \\server\...`)? Opening it
/// would contact that server just because a folder was listed.
fn git_file_points_to_network(root: &Path) -> bool {
    let dot_git = root.join(".git");
    if !std::fs::symlink_metadata(&dot_git).is_ok_and(|m| m.is_file()) {
        return false;
    }
    // A real `.git` file is one short line; anything large is not one.
    if std::fs::metadata(&dot_git).map_or(true, |m| m.len() > 4096) {
        return true;
    }
    let Ok(text) = std::fs::read_to_string(&dot_git) else {
        return true;
    };
    let target = text.trim().strip_prefix("gitdir:").unwrap_or("").trim();
    target.starts_with(r"\\") || target.starts_with("//")
}

/// Compares tracked files by their index stat data only. Never reading a file is what keeps
/// filter drivers (`filter.<name>.clean` / `.process`) from running.
#[derive(Clone)]
struct StatOnly;

impl CompareBlobs for StatOnly {
    type Output = ();
    fn compare_blobs<'a, 'b>(
        &mut self,
        _entry: &gix::index::Entry,
        _worktree_blob_size: u64,
        _data: impl ReadData<'a>,
        _buf: &mut Vec<u8>,
    ) -> gix::Result<Option<()>> {
        Ok(Some(()))
    }
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

/// Badges for tracked files that differ from the index.
struct Tracked<'s>(&'s mut RepoStatus);

impl<'index> VisitEntry<'index> for Tracked<'_> {
    type ContentChange = ();
    type SubmoduleStatus = ();

    fn visit_entry(&mut self, entry: Entry<'index, (), ()>) {
        if let Entry::Modification {
            rela_path, status, ..
        } = entry
        {
            let badge = match status {
                EntryStatus::Conflict { .. } => GitBadge::Conflicted,
                EntryStatus::IntentToAdd => GitBadge::Added,
                EntryStatus::NeedsUpdate(_) => return,
                EntryStatus::Change(_) => GitBadge::Modified,
            };
            self.0.add(&rela_path.to_string(), badge);
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

    #[test]
    fn a_git_file_pointing_at_a_network_path_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dot_git = tmp.path().join(".git");
        std::fs::write(&dot_git, "gitdir: \\\\server\\share\\repo.git\n").unwrap();
        assert!(git_file_points_to_network(tmp.path()));
        assert_eq!(read_status(tmp.path()).unwrap_err(), Failure::Unsafe);
        std::fs::write(&dot_git, "gitdir: C:/code/main/.git/worktrees/x\n").unwrap();
        assert!(!git_file_points_to_network(tmp.path()));
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
        assert_eq!(
            small(|l| l.time = Duration::ZERO).unwrap_err(),
            Failure::TooBig
        );
    }
}
