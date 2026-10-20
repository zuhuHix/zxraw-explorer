//! Copy/move/delete jobs, the interactive conflict dialog and the operations panel.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};
use egui::{RichText, Ui};
use forge_core::model::format_size;
use forge_core::ops::{
    self, ConflictDecision, ConflictInfo, ConflictPolicy, JobCtx, OpKind, OpOptions, OpReport,
    Resolution,
};
use forge_jobs::{CancelToken, JobHandle, JobState};

use crate::actions::{Event, UndoEntry};
use crate::app::ZxrawApp;
use crate::modal::{self, Confirm, ConfirmKind, Modal};
use crate::views;

/// Online-only sources at or above this count, or this total size, ask before a transfer
/// because reading them downloads everything from the cloud.
const CLOUD_WARN_COUNT: usize = 10;
const CLOUD_WARN_BYTES: u64 = 1 << 30;
/// Limits on the metadata-only walk into selected folders that may hold cloud files. Hitting
/// either one stops the walk and makes the check incomplete (it then asks).
const CLOUD_WALK_MAX_ENTRIES: usize = 20_000;
const CLOUD_WALK_TIME_LIMIT: Duration = Duration::from_millis(300);
/// Limits on checking the selected files one by one (one attribute query each). Larger than
/// the walk's: a shoot of several thousand selected files must not ask just for its size.
const CLOUD_FILES_MAX: usize = 100_000;
const CLOUD_FILES_TIME_LIMIT: Duration = Duration::from_secs(3);
/// Move-source identities are read by this many shared workers per lane (local, network), so
/// metadata calls blocked on an unreachable share tie up at most the network lane's threads,
/// however many moves are queued, and local moves never wait behind them.
const IDENTITY_WORKERS: usize = 2;
/// How long one identity read may run before its move fails closed. Time spent queued
/// behind other moves does not count.
const IDENTITY_WAIT: Duration = Duration::from_secs(10);
/// A scan that finishes this quickly needs no "Checking" dialog (local folders, nearly always).
const CLOUD_CHECK_GRACE: Duration = Duration::from_millis(150);
/// Ids for online-only checks, so a result is matched to the dialog that asked for it.
static NEXT_CLOUD_CHECK: AtomicU64 = AtomicU64::new(1);

/// A conflict waiting for the user, with the channel the job thread is blocked on.
pub struct ConflictQuestion {
    pub info: ConflictInfo,
    pub job_title: String,
    pub reply: Sender<Resolution>,
    pub apply_all: bool,
}

struct ClipboardMoveCompletion {
    sources: Vec<PathBuf>,
    generation: Option<u64>,
    expected_sequence: Option<u32>,
    /// For a paste of this app's cut: what each item was when it was cut. The job moves a
    /// source only if it is still that item (see [`screen_cut_sources`]).
    cut_ids: Option<Vec<(PathBuf, crate::cut_watch::Identity)>>,
    /// Reads the sources' identities for that, bounded and sharing the cut watcher's slots.
    checker: Option<crate::cut_watch::Checker>,
}

/// Start a copy or move of `sources` into `dest` as a background job, after asking first
/// when it would download online-only files.
pub fn start_transfer(app: &mut ZxrawApp, sources: Vec<PathBuf>, dest: PathBuf, kind: OpKind) {
    if refused_while_a_dialog_is_open(app, kind) {
        return;
    }
    if scan_needed(&sources, &dest, kind) {
        ask_after_cloud_check(app, sources.clone(), sources, dest, kind, None);
        return;
    }
    start_transfer_confirmed(app, sources, dest, kind);
}

/// [`start_transfer`] without the cloud check. Used when the user already accepted it.
pub fn start_transfer_confirmed(
    app: &mut ZxrawApp,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    kind: OpKind,
) {
    // Items already in `dest`: moving them there is a no-op, and copying them there must
    // never be offered as "replace the file with itself" — Explorer makes a renamed copy.
    let in_dest = |p: &PathBuf| {
        p.parent()
            .is_some_and(|d| forge_core::paths::paths_equal(d, &dest))
    };
    let (same, sources): (Vec<PathBuf>, Vec<PathBuf>) = sources.into_iter().partition(in_dest);
    if !same.is_empty() && kind == OpKind::Copy {
        start_transfer_with(
            app,
            same,
            dest.clone(),
            kind,
            ConflictPolicy::KeepBoth,
            None,
        );
    }
    if sources.is_empty() {
        return;
    }
    start_transfer_with(app, sources, dest, kind, ConflictPolicy::Ask, None);
}

/// Start a move originating from the system or in-app file clipboard. The clipboard is
/// consumed only after this job reports a complete, uncancelled move.
pub fn start_clipboard_move(
    app: &mut ZxrawApp,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    generation: Option<u64>,
    expected_sequence: Option<u32>,
) -> bool {
    if refused_while_a_dialog_is_open(app, OpKind::Move) {
        // Handled: the refusal has its own message.
        return true;
    }
    clipboard_move(app, sources, dest, generation, expected_sequence, true)
}

/// There is one confirmation-dialog slot (`app.ops.modal`), and every new copy or move may
/// need it (the online-only check and its warning). While a confirmation dialog is open a new
/// transfer is refused with a message:
/// never queued out of sight, never dropped silently, and never run past a confirmation the
/// user is still reading. Checked at the public entry points (copy, move, paste, drag and
/// drop, to the other pane) before anything else.
fn refused_while_a_dialog_is_open(app: &mut ZxrawApp, kind: OpKind) -> bool {
    if app.ops.modal.is_none() {
        return false;
    }
    app.toast_err(busy_transfer_message(kind));
    true
}

/// [`start_clipboard_move`] without the cloud check. Used when the user already accepted it.
pub fn start_clipboard_move_confirmed(
    app: &mut ZxrawApp,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    generation: Option<u64>,
    expected_sequence: Option<u32>,
) -> bool {
    clipboard_move(app, sources, dest, generation, expected_sequence, false)
}

fn clipboard_move(
    app: &mut ZxrawApp,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    generation: Option<u64>,
    expected_sequence: Option<u32>,
    check_cloud: bool,
) -> bool {
    let clipboard_sources = sources.clone();
    let in_dest = |path: &PathBuf| {
        path.parent()
            .is_some_and(|parent| forge_core::paths::paths_equal(parent, &dest))
    };
    let sources: Vec<PathBuf> = sources.into_iter().filter(|path| !in_dest(path)).collect();
    if sources.is_empty() {
        return false;
    }
    if check_cloud && scan_needed(&sources, &dest, OpKind::Move) {
        ask_after_cloud_check(
            app,
            sources,
            clipboard_sources,
            dest,
            OpKind::Move,
            Some((generation, expected_sequence)),
        );
        return true;
    }
    // Looked up now, also after a cloud confirmation: if the cut changed meanwhile, an item
    // no longer in it has no identity and is not moved.
    let cut_ids = generation.map(|g| crate::actions::cut_identities(app, g));
    let completion = ClipboardMoveCompletion {
        sources: clipboard_sources,
        generation,
        expected_sequence,
        cut_ids,
        checker: crate::actions::cut_checker(app),
    };
    start_transfer_with(
        app,
        sources,
        dest,
        OpKind::Move,
        ConflictPolicy::Ask,
        Some(completion),
    );
    true
}

fn ask_cloud_download(app: &mut ZxrawApp, body: String, kind: OpKind, pending: ConfirmKind) {
    let verb = if kind == OpKind::Copy { "copy" } else { "move" };
    app.ops.modal = Some(Modal::Confirm(Confirm {
        kind: pending,
        title: "Download from the cloud?".into(),
        body,
        confirm_label: format!("Download and {verb}"),
        danger: false,
    }));
}

/// Cancels the online-only scan of a "Checking" dialog when the dialog goes away: Cancel,
/// Continue anyway, or the result arriving (the scan is finished then, so it is a no-op).
pub struct CloudScanGuard(CancelToken);

impl Drop for CloudScanGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Shows the "Checking" dialog while the online-only scan runs on a background thread.
/// Cancel drops the dialog, which stops the scan and discards its result. Continue anyway
/// starts the transfer.
///
/// Only reached from the public entry points, which refuse while a dialog is open.
fn ask_after_cloud_check(
    app: &mut ZxrawApp,
    scan_sources: Vec<PathBuf>,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    kind: OpKind,
    clipboard: Option<(Option<u64>, Option<u32>)>,
) {
    let id = NEXT_CLOUD_CHECK.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = bounded(1);
    let ctx = app.ops.ctx.clone();
    let scan_dest = dest.clone();
    let cancel = CancelToken::new();
    let guard = CloudScanGuard(cancel.clone());
    std::thread::spawn(move || {
        let warning = cloud_warning_for(&scan_sources, &scan_dest, kind, &cancel);
        let _ = tx.send((id, warning));
        ctx.request_repaint();
    });
    // Bounded wait: a slow enumeration on cloud or network storage gets the dialog instead.
    if let Ok((finished, warning)) = rx.recv_timeout(CLOUD_CHECK_GRACE) {
        match cloud_check_action(Some(id), finished, warning) {
            CloudCheckAction::Warn(body) => {
                let pending = ConfirmKind::DownloadCloudFiles {
                    sources,
                    dest,
                    kind,
                    clipboard,
                };
                ask_cloud_download(app, body, kind, pending);
            }
            CloudCheckAction::Start => start_pending_transfer(app, sources, dest, kind, clipboard),
            // Cannot happen (the result is for this check), but never drop a transfer silently.
            CloudCheckAction::Ignore => app.toast_err("The transfer did not start; try again"),
        }
        return;
    }
    app.ops.modal = Some(Modal::Confirm(Confirm {
        kind: ConfirmKind::CheckingCloudFiles {
            id,
            result: rx,
            sources,
            dest,
            kind,
            clipboard,
            _scan: guard,
        },
        title: "Checking for online-only files…".into(),
        body: "Looking for files that would be downloaded from the cloud.".into(),
        confirm_label: "Continue anyway".into(),
        danger: false,
    }));
}

fn busy_transfer_message(kind: OpKind) -> String {
    let verb = if kind == OpKind::Copy { "copy" } else { "move" };
    format!("The {verb} did not start: close the open dialog first, then try again")
}

/// Applies a finished online-only check to the dialog that is waiting for it.
pub fn poll_cloud_check(app: &mut ZxrawApp) {
    let Some(Modal::Confirm(Confirm {
        kind: ConfirmKind::CheckingCloudFiles { id, result, .. },
        ..
    })) = &app.ops.modal
    else {
        return;
    };
    let Ok((finished, warning)) = result.try_recv() else {
        return;
    };
    let action = cloud_check_action(Some(*id), finished, warning);
    if action == CloudCheckAction::Ignore {
        return;
    }
    let Some(Modal::Confirm(Confirm {
        kind:
            ConfirmKind::CheckingCloudFiles {
                sources,
                dest,
                kind,
                clipboard,
                ..
            },
        ..
    })) = app.ops.modal.take()
    else {
        return;
    };
    match action {
        CloudCheckAction::Warn(body) => {
            let pending = ConfirmKind::DownloadCloudFiles {
                sources,
                dest,
                kind,
                clipboard,
            };
            ask_cloud_download(app, body, kind, pending);
        }
        CloudCheckAction::Start => start_pending_transfer(app, sources, dest, kind, clipboard),
        CloudCheckAction::Ignore => {}
    }
}

/// Starts a transfer the user has already checked or accepted, without asking again.
pub fn start_pending_transfer(
    app: &mut ZxrawApp,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    kind: OpKind,
    clipboard: Option<(Option<u64>, Option<u32>)>,
) {
    match clipboard {
        None => start_transfer_confirmed(app, sources, dest, kind),
        Some((generation, expected_sequence)) => {
            if !start_clipboard_move_confirmed(app, sources, dest, generation, expected_sequence) {
                app.toast("Nothing to move: the items are already in this folder");
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum CloudCheckAction {
    Start,
    Warn(String),
    /// The result is not for the awaited check.
    Ignore,
}

/// Pure decision for a finished online-only check: `awaited` is the id of the check the
/// dialog shows, `finished` the id the result carries. A mismatch means the result is stale.
fn cloud_check_action(
    awaited: Option<u64>,
    finished: u64,
    warning: Option<String>,
) -> CloudCheckAction {
    if awaited != Some(finished) {
        return CloudCheckAction::Ignore;
    }
    warning.map_or(CloudCheckAction::Start, CloudCheckAction::Warn)
}

/// Does the transfer need the online-only check? It always runs on the background worker,
/// for files as for folders: even one attribute query can stall on network or cloud storage.
/// A move within one volume is a rename that downloads nothing. Path text only, no I/O.
fn scan_needed(sources: &[PathBuf], dest: &Path, kind: OpKind) -> bool {
    !(kind == OpKind::Move && sources.iter().all(|path| same_volume(path, dest)))
}

/// The confirmation text when a transfer of `sources` to `dest` would download online-only
/// files, or could not be fully checked; `None` when it can start at once.
fn cloud_warning_for(
    sources: &[PathBuf],
    dest: &Path,
    kind: OpKind,
    cancel: &CancelToken,
) -> Option<String> {
    let same = sources.iter().all(|path| same_volume(path, dest));
    if kind == OpKind::Move && same {
        return None;
    }
    let roots = forge_core::cloud::known_sync_roots();
    let may_hold =
        |dir: &Path| forge_core::cloud::folder_may_hold_cloud_files(dir, roots.as_deref());
    let scan = online_only_totals(sources, dest, kind, cancel, &may_hold);
    cloud_download_warning(scan.count, scan.bytes, kind, same, scan.incomplete)
}

/// Online-only files that the transfer would read, and their logical size. Metadata only:
/// placeholders report their full size without being opened.
#[derive(Debug, Default, PartialEq)]
struct OnlineOnlyScan {
    count: usize,
    bytes: u64,
    /// Not every item was checked: the entry or time limit was hit, a folder or an item's
    /// metadata could not be read, or the scan was cancelled. `count` and `bytes` are lower
    /// bounds, and zero proves nothing.
    incomplete: bool,
}

/// `may_hold(folder)`: could the folder hold cloud files? Only those folders are walked; see
/// [`forge_core::cloud::folder_may_hold_cloud_files`].
fn online_only_totals(
    sources: &[PathBuf],
    dest: &Path,
    kind: OpKind,
    cancel: &CancelToken,
    may_hold: &dyn Fn(&Path) -> bool,
) -> OnlineOnlyScan {
    online_only_totals_with(
        sources,
        dest,
        kind,
        ScanLimits {
            files: (CLOUD_FILES_MAX, CLOUD_FILES_TIME_LIMIT),
            walk: (CLOUD_WALK_MAX_ENTRIES, CLOUD_WALK_TIME_LIMIT),
        },
        cancel,
        may_hold,
    )
}

/// `(entries, time)` limits for the selected files and for the folder walk.
#[derive(Clone, Copy)]
struct ScanLimits {
    files: (usize, Duration),
    walk: (usize, Duration),
}

impl ScanLimits {
    /// The same limits for both parts (tests).
    #[cfg(test)]
    fn both(entries: usize, time: Duration) -> Self {
        Self {
            files: (entries, time),
            walk: (entries, time),
        }
    }
}

/// Shared limits of one scan: entries visited, a deadline, and cancellation.
struct ScanBudget<'a> {
    visited: usize,
    max_entries: usize,
    deadline: Instant,
    cancel: &'a CancelToken,
}

impl<'a> ScanBudget<'a> {
    fn new((max_entries, time): (usize, Duration), cancel: &'a CancelToken) -> Self {
        Self {
            visited: 0,
            max_entries,
            deadline: Instant::now() + time,
            cancel,
        }
    }

    /// Takes one entry from the budget; `false` (and the scan is incomplete) when spent.
    fn take(&mut self) -> bool {
        if self.visited >= self.max_entries
            || Instant::now() >= self.deadline
            || self.cancel.is_cancelled()
        {
            return false;
        }
        self.visited += 1;
        true
    }
}

/// [`online_only_totals`] with injectable limits.
///
/// Selected files are always checked one by one (an online-only file need not sit under a
/// marked sync root), under their own limits. Selected folders are walked only when they may
/// hold cloud files; a folder positively outside every cloud location (a local shoot, the
/// common case) is not walked at all, so its size never makes the check incomplete.
fn online_only_totals_with(
    sources: &[PathBuf],
    dest: &Path,
    kind: OpKind,
    limits: ScanLimits,
    cancel: &CancelToken,
    may_hold: &dyn Fn(&Path) -> bool,
) -> OnlineOnlyScan {
    let mut budget = ScanBudget::new(limits.files, cancel);
    let mut scan = OnlineOnlyScan::default();
    let mut folders = Vec::new();
    // Selected files first, so a large folder cannot hide an explicitly selected file. A huge
    // selection is bounded too, and stopping makes the scan incomplete, never clean.
    for path in sources {
        if kind == OpKind::Move && same_volume(path, dest) {
            continue;
        }
        if !budget.take() {
            scan.incomplete = true;
            return scan;
        }
        match std::fs::symlink_metadata(path) {
            Ok(md) if md.is_file() => {
                if !forge_core::cloud::background_read_allowed(path) {
                    scan.count += 1;
                    scan.bytes += md.len();
                }
            }
            Ok(md) if md.is_dir() => folders.push(path),
            // Links are not followed by the transfer; nothing there downloads.
            Ok(_) => {}
            // A source that is gone fails in the transfer itself; anything else is unknown.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => scan.incomplete = true,
        }
    }
    let mut budget = ScanBudget::new(limits.walk, cancel);
    for folder in folders {
        if cancel.is_cancelled() {
            scan.incomplete = true;
            break;
        }
        if !may_hold(folder) {
            continue;
        }
        if !walk_online_only(folder, &mut budget, &mut scan) {
            scan.incomplete = true;
            break;
        }
    }
    scan
}

/// Walks the folder `root` and adds its online-only files to `scan`. Listing metadata only:
/// `DirEntry::metadata` comes from the directory enumeration, so no file is opened. Symlinks
/// and junctions are not followed. A folder or entry that cannot be read marks the scan
/// incomplete. Returns `false` when the budget ran out (the walk stopped early).
fn walk_online_only(root: &Path, budget: &mut ScanBudget, scan: &mut OnlineOnlyScan) -> bool {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            scan.incomplete = true;
            continue;
        };
        for entry in entries {
            if !budget.take() {
                return false;
            }
            let Ok(entry) = entry else {
                scan.incomplete = true;
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                scan.incomplete = true;
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                match entry.metadata() {
                    Ok(md) if forge_core::cloud::is_online_only_metadata(&path, &md) => {
                        scan.count += 1;
                        scan.bytes += md.len();
                    }
                    Ok(_) => {}
                    Err(_) => scan.incomplete = true,
                }
            }
        }
    }
    true
}

/// Are both paths on the same drive (or share the same UNC share / root)?
fn same_volume(a: &Path, b: &Path) -> bool {
    let root = |p: &Path| {
        p.components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
    };
    root(a) == root(b)
}

/// Pure decision: the body text of the download warning, or `None` when the transfer is
/// cheap enough to start at once. A move within one volume is a rename and downloads nothing.
///
/// An `incomplete` scan (limits hit, unreadable items, cancelled) always asks: its totals are
/// lower bounds, and finding nothing proves nothing. With online-only files found it says
/// "at least"; with none it says the files could not all be checked.
fn cloud_download_warning(
    count: usize,
    bytes: u64,
    kind: OpKind,
    same_volume: bool,
    incomplete: bool,
) -> Option<String> {
    if kind == OpKind::Move && same_volume {
        return None;
    }
    let verb = if kind == OpKind::Copy {
        "Copying"
    } else {
        "Moving"
    };
    if incomplete && count == 0 {
        return Some(format!(
            "Couldn't check all files for cloud placeholders. {verb} may download some of them \
             from the cloud first."
        ));
    }
    let below_threshold = count < CLOUD_WARN_COUNT && bytes < CLOUD_WARN_BYTES;
    if count == 0 || (below_threshold && !incomplete) {
        return None;
    }
    let size = format_size(bytes);
    if incomplete {
        // The scan saw only part of the selection: the totals are lower bounds.
        let (files, are) = if count == 1 {
            ("1 file".to_owned(), "is")
        } else {
            (format!("{count} files"), "are")
        };
        return Some(format!(
            "At least {files} ({size}) {are} online-only; not every file could be checked, so \
             there may be more. {verb} them downloads them from the cloud first."
        ));
    }
    let (files, are, them) = if count == 1 {
        ("1 file".to_owned(), "is", "it")
    } else {
        (format!("{count} files"), "are", "them")
    };
    Some(format!(
        "{files} ({size}) {are} online-only. {verb} {them} downloads {them} from the cloud first."
    ))
}

fn start_transfer_with(
    app: &mut ZxrawApp,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    kind: OpKind,
    policy: ConflictPolicy,
    completion: Option<ClipboardMoveCompletion>,
) {
    if sources.is_empty() {
        return;
    }
    if kind == OpKind::Move {
        // Queue the note being edited while its file is still where it is being edited.
        crate::tags_ui::save_note_now(app);
    }
    let verify = app.settings.verify_copies;
    let verb = if kind == OpKind::Copy {
        "Copying"
    } else {
        "Moving"
    };
    let title = format!(
        "{verb} {} item(s) to {}",
        sources.len(),
        dest.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dest.display().to_string())
    );
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    let title2 = title.clone();
    // Automation can explicitly arm a read fault (only script steps arm it). Force the
    // move-copy path in that case so tests exercise the same verified fallback used for
    // cross-volume moves. Not `testhook.active()`: it is false while a script step runs.
    let force_copy_for_move = kind == OpKind::Move && forge_core::fault::is_armed();
    app.ops
        .jobs
        .transfers()
        .set_serial(app.settings.transfer_queue);
    let disks = transfer_disks(&sources, &dest);
    if kind == OpKind::Move {
        // The sources leave their folders once the queued job runs.
        app.expect_removal(&sources);
    }
    // A move deletes its sources, so it must act on the items the user picked: what is
    // at a source path when the job starts must still be the same item. The identities are
    // read now, on a helper thread (one metadata query per source can stall on a network
    // path), and the job waits for them before it looks at any source.
    let identities = (kind == OpKind::Move).then(|| IdentityPool::global().capture(&sources));
    // Taken by whichever runs: the job, or the callback for a job cancelled while queued.
    let completion = std::sync::Arc::new(parking_lot::Mutex::new(completion));
    let unstarted = {
        let (completion, tx, ctx) = (completion.clone(), tx.clone(), ctx.clone());
        let title = title.clone();
        move || {
            let _ = tx.send(Event::Toast(format!(
                "{title} — cancelled before it started"
            )));
            if let Some(completion) = completion.lock().take() {
                let _ = tx.send(Event::MoveFinished {
                    sources: completion.sources,
                    ok: false,
                    clipboard_generation: completion.generation,
                    expected_sequence: completion.expected_sequence,
                });
            }
            ctx.request_repaint();
        }
    };
    let job_body = move |job: &JobHandle| {
        let completion = completion.lock().take();
        // A queued job starts later: anything that vanished meanwhile is skipped, not
        // failed, and a moved source replaced by another item is left alone. A move fails
        // closed: a source whose identity was not captured when queued is not moved
        // either. Conflicts are looked at only now, when the job runs.
        if identities.is_some() {
            job.progress
                .set_current("Checking that the sources are still the items you picked");
        }
        let identities = match &identities {
            Some(pending) => match pending.wait(&job.cancel, IDENTITY_WAIT) {
                Ok(identities) => identities,
                Err(failure) => {
                    let message = match failure {
                        IdentityWait::Cancelled => "cancelled".to_string(),
                        IdentityWait::TimedOut(path) => {
                            let message = format!(
                                "Nothing was moved: {} did not answer within {} s, so it \
                                 could not be checked that it is still the item you picked",
                                path.display(),
                                IDENTITY_WAIT.as_secs()
                            );
                            let _ = tx.send(Event::Error(message.clone()));
                            message
                        }
                    };
                    if let Some(completion) = completion {
                        let _ = tx.send(Event::MoveFinished {
                            sources: completion.sources,
                            ok: false,
                            clipboard_generation: completion.generation,
                            expected_sequence: completion.expected_sequence,
                        });
                    }
                    ctx.request_repaint();
                    return Err(message);
                }
            },
            None => Vec::new(),
        };
        let (sources, changed, unverified) =
            screen_queued_sources(sources, &identities, source_identity);
        // A paste of this app's cut checks against the identity from the cut itself, not
        // only the one read when the move was queued (a replacement made before that would
        // otherwise pass).
        let (sources, changed, unverified) = match completion.as_ref() {
            Some(c) if c.cut_ids.is_some() => {
                // Bounded, and given up on when the job is cancelled: an item that could not be
                // checked in time is not moved.
                let cut = c.cut_ids.as_deref().unwrap_or_default();
                let seen = c
                    .checker
                    .zip(c.generation)
                    .and_then(|(checker, generation)| {
                        checker.check_unless(
                            generation,
                            sources.clone(),
                            crate::cut_watch::PASS_TIMEOUT,
                            || job.cancel.is_cancelled(),
                        )
                    })
                    .unwrap_or_else(|| {
                        vec![crate::cut_watch::Observation::Unknown; sources.len()]
                    });
                screen_cut_sources(sources, (changed, unverified), cut, &seen)
            }
            _ => (sources, changed, unverified),
        };
        let (sources, mut vanished): (Vec<PathBuf>, Vec<PathBuf>) = sources
            .into_iter()
            .partition(|s| forge_core::paths::exists_no_follow(s));
        let opts = OpOptions {
            policy,
            verify,
            force_copy_for_move,
        };
        let plan = match ops::plan(kind, &sources, &dest, opts, &job.cancel) {
            Ok(mut plan) => {
                // A source that vanished after the check above is not a failure either.
                plan.issues.retain(|issue| {
                    let gone = sources.contains(&issue.path)
                        && !forge_core::paths::exists_no_follow(&issue.path);
                    if gone {
                        vanished.push(issue.path.clone());
                    }
                    !gone
                });
                plan
            }
            Err(e) => {
                let _ = tx.send(Event::Error(e.to_string()));
                if let Some(completion) = completion {
                    let _ = tx.send(Event::MoveFinished {
                        sources: completion.sources,
                        ok: false,
                        clipboard_generation: completion.generation,
                        expected_sequence: completion.expected_sequence,
                    });
                }
                ctx.request_repaint();
                return Err(e.to_string());
            }
        };
        job.progress.add_total_items(plan.files + plan.links);
        job.progress.add_total_bytes(plan.bytes);

        // Conflicts are forwarded to the UI; this thread blocks until the user answers
        // (or the job is cancelled).
        let ui_tx = tx.clone();
        let ui_ctx = ctx.clone();
        let cancel = job.cancel.clone();
        let mut resolver = move |info: &ConflictInfo| -> Resolution {
            let (rtx, rrx) = bounded(1);
            let _ = ui_tx.send(Event::Conflict(ConflictQuestion {
                info: info.clone(),
                job_title: title2.clone(),
                reply: rtx,
                apply_all: false,
            }));
            ui_ctx.request_repaint();
            loop {
                match rrx.recv_timeout(Duration::from_millis(200)) {
                    Ok(r) => return r,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) if !cancel.is_cancelled() => {
                        continue;
                    }
                    _ => return Resolution::once(ConflictDecision::Cancel),
                }
            }
        };
        let mut report = ops::execute(&plan, job, &mut resolver);
        for path in vanished {
            report
                .skipped
                .push(ops::OpIssue::new(path, "no longer exists"));
        }
        for path in changed {
            report
                .skipped
                .push(ops::OpIssue::new(path, "changed while queued, not moved"));
        }
        for path in unverified {
            report.skipped.push(ops::OpIssue::new(
                path,
                "could not verify it's the same item, not moved",
            ));
        }
        if kind == OpKind::Move {
            let moved = uncovered_moves(&sources, &dest, &report);
            if !moved.is_empty() {
                let _ = tx.send(Event::FilesMoved(moved));
            }
        }
        let move_ok = report.failed.is_empty() && report.skipped.is_empty() && !report.cancelled;
        finish_report(
            job,
            &report,
            &tx,
            &[dest.clone()],
            sources
                .iter()
                .filter_map(|s| s.parent().map(Path::to_path_buf))
                .collect(),
        );
        if let Some(completion) = completion {
            let _ = tx.send(Event::MoveFinished {
                sources: completion.sources,
                ok: move_ok,
                clipboard_generation: completion.generation,
                expected_sequence: completion.expected_sequence,
            });
        }
        ctx.request_repaint();
        Ok(())
    };
    app.ops
        .jobs
        .spawn_queued("transfer", title, disks, job_body, unstarted);
}

/// What a source path pointed at when a move was queued.
#[derive(Debug, PartialEq)]
enum SourceIdentity {
    /// Volume serial and file index (Windows file ID).
    Id {
        kind: std::fs::FileType,
        id: (u32, u64),
    },
    /// Fallback when the file ID cannot be read.
    Meta {
        kind: std::fs::FileType,
        len: u64,
        modified: Option<std::time::SystemTime>,
    },
}

/// Splits a queued job's sources into those to act on, those replaced since they were
/// queued, and those with no identity recorded at queue time that exist now (a move fails
/// closed on them). `identities` is empty for copies, which keep every source. Sources
/// that are gone are kept here; the caller reports them as no longer existing.
fn screen_queued_sources(
    sources: Vec<PathBuf>,
    identities: &[Option<SourceIdentity>],
    current: impl Fn(&Path) -> Option<SourceIdentity>,
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let (mut keep, mut changed, mut unverified) = (Vec::new(), Vec::new(), Vec::new());
    for (i, source) in sources.into_iter().enumerate() {
        let now = current(&source);
        match (identities.get(i), now) {
            (Some(Some(then)), Some(now)) if now != *then => changed.push(source),
            (Some(None), Some(_)) => unverified.push(source),
            _ => keep.push(source),
        }
    }
    (keep, changed, unverified)
}

/// Splits the sources of a cut-paste by the identity each item had when it was cut, given
/// what is at each source now (`seen`, in order): kept if still that item (or gone, which the
/// job reports as gone), changed if another item has taken the path, unverified if there is
/// no cut identity or it could not be read now. Adds to the `changed` and `unverified` lists
/// from [`screen_queued_sources`].
fn screen_cut_sources(
    sources: Vec<PathBuf>,
    (mut changed, mut unverified): (Vec<PathBuf>, Vec<PathBuf>),
    cut: &[(PathBuf, crate::cut_watch::Identity)],
    seen: &[crate::cut_watch::Observation],
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    use crate::cut_watch::Observation;
    let mut keep = Vec::new();
    for (i, source) in sources.into_iter().enumerate() {
        let then = cut
            .iter()
            .find(|(p, _)| forge_core::paths::paths_equal(p, &source))
            .map(|(_, id)| *id);
        let now = seen.get(i).copied().unwrap_or(Observation::Unknown);
        match (then, now) {
            (Some(then), Observation::Present(now)) if now == then => keep.push(source),
            (Some(_), Observation::Present(_)) => changed.push(source),
            (_, Observation::Missing) => keep.push(source),
            _ => unverified.push(source),
        }
    }
    (keep, changed, unverified)
}

/// Progress of one move's identity reads, from the pool to the waiting job.
enum IdentityEvent {
    /// A worker started reading this source (the per-read deadline starts now).
    Started(usize),
    Done(usize, Option<SourceIdentity>),
}

/// The identity reads one move still needs, queued in a lane.
struct MoveReads {
    paths: parking_lot::Mutex<std::collections::VecDeque<(usize, PathBuf)>>,
    /// Set when the move no longer needs them (finished, failed, cancelled): skipped.
    cancel: CancelToken,
    reply: Sender<IdentityEvent>,
}

/// One lane of identity workers. Moves take turns, one read each, so a small move queued
/// behind a large one is served after at most one read per worker, not after all of them.
struct IdentityLane {
    moves: parking_lot::Mutex<std::collections::VecDeque<std::sync::Arc<MoveReads>>>,
    ready: parking_lot::Condvar,
    /// Workers that actually started (a thread can fail to spawn).
    workers: std::sync::atomic::AtomicUsize,
}

impl IdentityLane {
    fn start(name: &str, workers: usize, read: fn(&Path) -> Option<SourceIdentity>) -> Arc<Self> {
        let lane = Arc::new(Self {
            moves: Default::default(),
            ready: parking_lot::Condvar::new(),
            workers: Default::default(),
        });
        for _ in 0..workers.max(1) {
            let worker = lane.clone();
            let spawned = std::thread::Builder::new()
                .name(name.into())
                .spawn(move || worker.serve(read));
            if spawned.is_ok() {
                lane.workers.fetch_add(1, Ordering::Relaxed);
            }
        }
        lane
    }

    /// The next read: the first move with work left, which then goes to the back.
    fn next(&self) -> (Arc<MoveReads>, (usize, PathBuf)) {
        let mut moves = self.moves.lock();
        loop {
            while let Some(reads) = moves.pop_front() {
                if reads.cancel.is_cancelled() {
                    continue;
                }
                let mut paths = reads.paths.lock();
                let Some(item) = paths.pop_front() else {
                    continue;
                };
                let more = !paths.is_empty();
                drop(paths);
                if more {
                    moves.push_back(reads.clone());
                }
                return (reads, item);
            }
            self.ready.wait(&mut moves);
        }
    }

    fn serve(&self, read: fn(&Path) -> Option<SourceIdentity>) {
        loop {
            let (reads, (index, path)) = self.next();
            if reads.cancel.is_cancelled() {
                continue;
            }
            let _ = reads.reply.send(IdentityEvent::Started(index));
            let identity = read(&path);
            let _ = reads.reply.send(IdentityEvent::Done(index, identity));
        }
    }
}

/// The shared workers that read move-source identities off the UI thread, when a move is
/// queued. Fixed in number and alive for the whole process (as `path_probe`'s lanes), so a
/// metadata call stuck on an unreachable share can never pile up threads.
struct IdentityPool {
    local: Arc<IdentityLane>,
    network: Arc<IdentityLane>,
}

impl IdentityPool {
    fn global() -> &'static IdentityPool {
        static POOL: std::sync::OnceLock<IdentityPool> = std::sync::OnceLock::new();
        POOL.get_or_init(|| IdentityPool::new(IDENTITY_WORKERS, source_identity))
    }

    fn new(workers: usize, read: fn(&Path) -> Option<SourceIdentity>) -> Self {
        Self {
            local: IdentityLane::start("move-identity", workers, read),
            network: IdentityLane::start("move-identity-net", workers, read),
        }
    }

    /// Queues the identities of `sources` in the network lane if any source is on a share
    /// (no file call: UNC text, or the drive type, which Windows answers locally).
    fn capture(&self, sources: &[PathBuf]) -> PendingIdentities {
        let network = sources
            .iter()
            .any(|path| forge_core::netpath::is_network_path(path));
        let lane = if network { &self.network } else { &self.local };
        let cancel = CancelToken::new();
        let (reply, events) = crossbeam_channel::unbounded();
        let pending = PendingIdentities {
            events,
            paths: sources.to_vec(),
            cancel: cancel.clone(),
        };
        if lane.workers.load(Ordering::Relaxed) == 0 || sources.is_empty() {
            // No worker to read them (or nothing to read): the reply side is dropped now, so
            // every identity is unknown and the move fails closed.
            return pending;
        }
        let reads = Arc::new(MoveReads {
            paths: parking_lot::Mutex::new(sources.iter().cloned().enumerate().collect()),
            cancel,
            reply,
        });
        lane.moves.lock().push_back(reads);
        lane.ready.notify_all();
        pending
    }
}

/// The identities of one move, arriving from the pool. Dropping it (the move finished or was
/// cancelled, even while still queued) skips its reads that have not started. It holds no
/// reply sender, so once the pool has dropped the move's reads the channel disconnects.
struct PendingIdentities {
    events: Receiver<IdentityEvent>,
    paths: Vec<PathBuf>,
    cancel: CancelToken,
}

impl Drop for PendingIdentities {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[derive(Debug, PartialEq)]
enum IdentityWait {
    Cancelled,
    /// A read of this source ran longer than the limit (an unreachable share, usually).
    TimedOut(PathBuf),
}

impl PendingIdentities {
    /// Waits for every identity, checking `cancel` in between. Reads still queued behind
    /// other moves wait without a limit; a read that has started may take `per_read`. An
    /// identity that never arrives because the pool dropped it is unknown, so the move fails
    /// closed for that source.
    fn wait(
        &self,
        cancel: &CancelToken,
        per_read: Duration,
    ) -> Result<Vec<Option<SourceIdentity>>, IdentityWait> {
        let mut found: Vec<Option<Option<SourceIdentity>>> =
            (0..self.paths.len()).map(|_| None).collect();
        let mut remaining = self.paths.len();
        let mut running: std::collections::HashMap<usize, Instant> = Default::default();
        while remaining > 0 {
            if cancel.is_cancelled() {
                return Err(IdentityWait::Cancelled);
            }
            match self.events.recv_timeout(Duration::from_millis(50)) {
                Ok(IdentityEvent::Started(index)) => {
                    running.insert(index, Instant::now());
                }
                Ok(IdentityEvent::Done(index, identity)) => {
                    running.remove(&index);
                    if found[index].is_none() {
                        remaining -= 1;
                    }
                    found[index] = Some(identity);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
            if let Some((&index, _)) = running.iter().find(|(_, at)| at.elapsed() >= per_read) {
                return Err(IdentityWait::TimedOut(self.paths[index].clone()));
            }
        }
        Ok(found.into_iter().map(Option::flatten).collect())
    }
}

/// `None` if nothing is at `path`. Does not follow a link at the end.
fn source_identity(path: &Path) -> Option<SourceIdentity> {
    let md = std::fs::symlink_metadata(path).ok()?;
    let kind = md.file_type();
    Some(match forge_win::file_id(path) {
        Ok(id) => SourceIdentity::Id { kind, id },
        Err(_) => SourceIdentity::Meta {
            kind,
            len: md.len(),
            modified: md.modified().ok(),
        },
    })
}

/// Physical disks a transfer reads and writes, or `None` if any of them is unknown
/// (network paths). Looked up per drive letter and cached briefly, because a drive
/// letter can be given to another card reader or USB disk.
fn transfer_disks(sources: &[PathBuf], dest: &Path) -> forge_jobs::DiskSet {
    use std::collections::HashMap;
    use std::path::Component;
    use std::sync::LazyLock;
    use std::time::Instant;

    static CACHE: LazyLock<parking_lot::Mutex<HashMap<String, (Instant, Option<Vec<u32>>)>>> =
        LazyLock::new(Default::default);
    let mut disks = Vec::new();
    for path in sources.iter().map(PathBuf::as_path).chain([dest]) {
        // A network location (mapped drive letters included) has no known disk: it counts
        // as sharing one with every other transfer, so in queue mode a copy to or from a
        // NAS never runs side by side with another. This also skips the disk query, which
        // can stall on an offline share.
        if forge_core::netpath::is_network_path(path) {
            return None;
        }
        let Some(Component::Prefix(prefix)) = path.components().next() else {
            return None;
        };
        let key = prefix.as_os_str().to_string_lossy().to_lowercase();
        let found = {
            let mut cache = CACHE.lock();
            match cache.get(&key) {
                Some((at, found)) if at.elapsed() < Duration::from_secs(30) => found.clone(),
                _ => {
                    let found = forge_win::physical_disks(path);
                    cache.insert(key, (Instant::now(), found.clone()));
                    found
                }
            }
        };
        disks.extend(found?);
    }
    disks.sort_unstable();
    disks.dedup();
    Some(disks)
}

/// The `(source, destination)` pairs of moved items that have no undo record. A cross-volume
/// move is a copy plus a delete and records no undo entry, so its pairs are read from the
/// report. A source that is gone moved as a whole when `report.done` has its destination under
/// the source's own name, or anything inside it (a folder copy reports its files, not the
/// folder): the source is relinked as one prefix, so everything inside follows. A source that
/// still partly exists (a partial move) relinks only the reported items whose original is gone.
/// A keep-both copy (renamed on arrival) is not matched, so its library row stays at the old
/// path.
fn uncovered_moves(sources: &[PathBuf], dest: &Path, report: &OpReport) -> Vec<(PathBuf, PathBuf)> {
    use crate::library::moved_path;
    let mut pairs = Vec::new();
    for src in sources {
        let Some(name) = src.file_name() else {
            continue;
        };
        let target = dest.join(name);
        let covered = report
            .undo
            .iter()
            .any(|u| matches!(u, ops::UndoRecord::Moved { from, .. } if from == src));
        if covered {
            continue;
        }
        // Each reported destination at or under `target`, with the source path it came from.
        let arrived: Vec<(PathBuf, PathBuf)> = report
            .done
            .iter()
            .filter_map(|done| moved_path(done, &target, src).map(|from| (from, done.clone())))
            .collect();
        if !src.exists() {
            if !arrived.is_empty() {
                pairs.push((src.clone(), target));
            }
            continue;
        }
        let mut partial: Vec<(PathBuf, PathBuf)> = arrived
            .into_iter()
            .filter(|(from, _)| !from.exists())
            .collect();
        // A moved subfolder already carries the files reported inside it.
        partial.sort_by_key(|(from, _)| from.components().count());
        let mut kept: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (from, to) in partial {
            if !kept.iter().any(|(k, _)| moved_path(&from, k, k).is_some()) {
                kept.push((from, to));
            }
        }
        pairs.extend(kept);
    }
    pairs
}

fn finish_report(
    job: &JobHandle,
    report: &OpReport,
    tx: &Sender<Event>,
    dests: &[PathBuf],
    mut sources: Vec<PathBuf>,
) {
    sources.sort();
    sources.dedup();
    for d in dests.iter().chain(sources.iter()) {
        let _ = tx.send(Event::Changed(d.clone()));
    }
    if !report.undo.is_empty() {
        let _ = tx.send(Event::Undo(UndoEntry::Ops(report.undo.clone())));
    }
    // Warnings are not repeated: the engine logs each one as it happens.
    for l in report_lines(report) {
        job.log(l);
    }
    for _ in 0..report.failed.len() {
        job.progress.add_error();
    }
    let _ = tx.send(report_event(&job.title, report));
}

/// One-line result of a copy/move/delete: "12 done (3 MB), 0 skipped, 0 failed, 1 warning".
fn report_summary(report: &OpReport) -> String {
    format!(
        "{} done ({}), {} skipped, {} failed{}{}",
        report.done.len(),
        format_size(report.bytes),
        report.skipped.len(),
        report.failed.len(),
        warnings_suffix(report.warnings.len()),
        if report.cancelled { ", cancelled" } else { "" }
    )
}

/// ", 1 warning" / ", 3 warnings" after a result summary; empty without warnings.
pub(crate) fn warnings_suffix(n: usize) -> String {
    match n {
        0 => String::new(),
        1 => ", 1 warning".into(),
        n => format!(", {n} warnings"),
    }
}

/// The summary, then each failed and skipped item.
fn report_lines(report: &OpReport) -> Vec<String> {
    let mut lines = vec![report_summary(report)];
    for f in &report.failed {
        lines.push(format!("FAILED  {}: {}", f.path.display(), f.reason));
    }
    for s in &report.skipped {
        lines.push(format!("skipped {}: {}", s.path.display(), s.reason));
    }
    lines
}

/// How a finished job tells the user: a toast when all went well, otherwise a report that
/// lists failures and warnings (a warning is not an error, but the user must learn of it).
fn report_event(title: &str, report: &OpReport) -> Event {
    let heading = if !report.failed.is_empty() {
        "completed with errors"
    } else if !report.warnings.is_empty() {
        "completed with warnings"
    } else if report.cancelled {
        return Event::Toast(format!("{title} — cancelled"));
    } else {
        return Event::Toast(report_summary(report));
    };
    Event::ReportWithWarnings {
        title: format!("{title} — {heading}"),
        lines: report_lines(report),
        warnings: report.warnings.clone(),
    }
}

pub fn delete_permanently(app: &mut ZxrawApp, targets: Vec<PathBuf>) {
    app.expect_removal(&targets);
    let n = targets.len();
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops.jobs.spawn(
        "delete",
        format!("Permanently deleting {n} item(s)"),
        move |job| {
            let report = ops::delete_permanently(&targets, &JobCtx::from_job(job));
            let parents: Vec<PathBuf> = targets
                .iter()
                .filter_map(|t| t.parent().map(Path::to_path_buf))
                .collect();
            finish_report(job, &report, &tx, &[], parents);
            // Protected folders (C:\Program Files and the like) refuse an in-app delete.
            // Windows can do it after asking for administrator permission.
            let denied: Vec<PathBuf> = targets
                .iter()
                .filter(|t| {
                    report
                        .failed
                        .iter()
                        .any(|f| f.path.starts_with(t) && is_access_denied(&f.reason))
                })
                .cloned()
                .collect();
            if !denied.is_empty() && !report.cancelled {
                let _ = tx.send(Event::AskConfirm(Confirm {
                    title: "Administrator permission needed".into(),
                    body: format!(
                        "Access was denied to {} item(s):\n\n{}\n\nThey are probably in a \
                         protected folder. Windows can delete them after asking for \
                         administrator permission.",
                        denied.len(),
                        modal::describe_items(&denied)
                    ),
                    kind: ConfirmKind::DeleteWithWindows(DeleteTarget::capture_all(&denied)),
                    confirm_label: "Delete as administrator".into(),
                    danger: true,
                }));
            }
            ctx.request_repaint();
            Ok(())
        },
    );
}

/// Whether an in-app delete failed because access was denied (ERROR_ACCESS_DENIED). The
/// system message is localised; its "(os error 5)" suffix is not.
fn is_access_denied(reason: &str) -> bool {
    reason.contains("(os error 5)")
}

/// An item the user is asked to delete permanently, with what it was when the question was
/// asked. The delete acts on it only if it is still the same object: an item moved away or
/// replaced while the question was open is left alone.
pub struct DeleteTarget {
    path: PathBuf,
    /// `None` when it could not be read when the question was asked; such an item is
    /// never deleted.
    identity: Option<forge_win::ItemIdentity>,
}

impl DeleteTarget {
    fn capture_all(paths: &[PathBuf]) -> Vec<Self> {
        paths
            .iter()
            .map(|path| Self {
                path: path.clone(),
                identity: forge_win::ItemIdentity::of(path),
            })
            .collect()
    }
}

/// Splits `targets` into the items that are still the object the user was asked about
/// (with that identity, for the shell to check again as it reaches each one), and report
/// lines for the rest. An identity missing then or now never matches.
fn screen_delete_targets<I: PartialEq>(
    targets: Vec<(PathBuf, Option<I>)>,
    current: impl Fn(&Path) -> Option<I>,
) -> (Vec<(PathBuf, I)>, Vec<String>) {
    let mut same = Vec::new();
    let mut changed = Vec::new();
    for (path, then) in targets {
        let now = current(&path);
        match then {
            Some(then) if now.as_ref() == Some(&then) => same.push((path, then)),
            _ if std::fs::symlink_metadata(&path).is_err() => {
                changed.push(format!("{}: it no longer exists", path.display()))
            }
            None => changed.push(format!(
                "{}: it could not be identified when you were asked, so it was not deleted",
                path.display()
            )),
            Some(_) => changed.push(format!(
                "{}: {}, so it was not deleted",
                path.display(),
                forge_win::DeleteFailure::Changed
            )),
        }
    }
    (same, changed)
}

/// The job's result for a shell delete. A cancelled one (by the user, in the app or in a
/// Windows dialog, or by the system) ends as cancelled, not completed.
fn delete_result(job: &JobHandle, report: &forge_win::DeleteReport, skipped: usize) -> Result<(), String> {
    if report.cancelled {
        job.cancel.cancel();
        return Ok(());
    }
    match report.failed.len() + skipped {
        0 => Ok(()),
        n => Err(format!("{n} item(s) not deleted")),
    }
}

/// Questions from finished deletes, and the progress popup's speed meters.
#[derive(Default)]
pub struct DeleteUi {
    /// Questions waiting for the open dialog to close.
    pending: std::collections::VecDeque<Confirm>,
    /// Speed of each job the popup shows.
    meters: std::collections::HashMap<forge_jobs::JobId, RateMeter>,
}

/// Queues a question from a job; it opens once no other dialog is open.
pub fn ask(app: &mut ZxrawApp, confirm: Confirm) {
    app.ops.delete_ui.pending.push_back(confirm);
}

/// Opens the next waiting question if no dialog is open. Returns whether one opened.
pub fn show_pending_question(app: &mut ZxrawApp) -> bool {
    if app.ops.modal.is_some() || app.ops.batch_rename.is_some() {
        return false;
    }
    let Some(confirm) = app.ops.delete_ui.pending.pop_front() else {
        return false;
    };
    app.ops.modal = Some(Modal::Confirm(confirm));
    true
}

/// Moves `targets` to the Recycle Bin. Never deletes permanently: items that cannot be
/// recycled stay, and the app asks whether to delete them permanently instead.
pub fn recycle(app: &mut ZxrawApp, targets: Vec<PathBuf>) {
    app.expect_removal(&targets);
    let n = targets.len();
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops
        .jobs
        .spawn("delete", recycle_title(n), move |job| {
            let report = forge_win::recycle_items(&targets, shell_observer(job));
            finish_shell_delete(job, &targets, &report, true, Vec::new(), &tx);
            ctx.request_repaint();
            delete_result(job, &report, 0)
        });
}

/// Deletes `targets` permanently through Windows, which can ask for administrator
/// permission (the in-app delete cannot).
pub fn delete_with_windows(app: &mut ZxrawApp, targets: Vec<DeleteTarget>) {
    let paths: Vec<PathBuf> = targets.iter().map(|t| t.path.clone()).collect();
    app.expect_removal(&paths);
    let n = targets.len();
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops.jobs.spawn(
        "delete",
        format!("Permanently deleting {n} item(s)"),
        move |job| {
            // Checked here, before anything is deleted, and again by the shell as it reaches
            // each item: the question may have been open a while, and a big batch takes time.
            let targets = targets.into_iter().map(|t| (t.path, t.identity)).collect();
            let (same, changed) = screen_delete_targets(targets, forge_win::ItemIdentity::of);
            for _ in &changed {
                job.progress.add_error();
            }
            let skipped = changed.len();
            let report = forge_win::delete_confirmed_items(&same, shell_observer(job));
            finish_shell_delete(job, &paths, &report, false, changed, &tx);
            ctx.request_repaint();
            delete_result(job, &report, skipped)
        },
    );
}

/// The title of a job that moves `n` items to the Recycle Bin.
fn recycle_title(n: usize) -> String {
    match n {
        1 => "Moving to Recycle Bin".into(),
        n => format!("Moving {n} items to Recycle Bin"),
    }
}

/// Shows a shell delete's progress in the job's: the files and folders inside the items
/// once the shell has counted them, otherwise the top-level items. While the shell counts,
/// or moves a whole folder in one step, there is nothing to measure: the job gets a note
/// and no counts, so its bar slides instead of standing at "0 of 1".
fn show_delete_progress(progress: &forge_jobs::Progress, p: &forge_win::DeleteProgress) {
    use crate::viewer::cull::format_count;
    use forge_win::DeleteStage;
    let (done, total, note) = match p.stage {
        DeleteStage::Starting => (0, 0, "Getting ready…".to_string()),
        DeleteStage::Counting => (
            0,
            0,
            format!("Counting items… {} found", format_count(p.found as usize)),
        ),
        DeleteStage::MovingWhole if p.work_total > 1 => (
            0,
            0,
            format!(
                "Moving {} items to the Recycle Bin…",
                format_count(p.work_total as usize)
            ),
        ),
        DeleteStage::MovingWhole => (0, 0, "Moving to the Recycle Bin…".to_string()),
        DeleteStage::Deleting if p.work_total > 0 => (p.work_done, p.work_total, String::new()),
        DeleteStage::Deleting => (p.items_done as u64, p.items_total as u64, String::new()),
    };
    progress.items_total.store(total, Ordering::Relaxed);
    progress.items_done.store(done.min(total), Ordering::Relaxed);
    progress.set_note(note);
    // No callbacks come until that step ends, so nothing can stop or pause it.
    progress
        .in_one_step
        .store(p.stage == DeleteStage::MovingWhole, Ordering::Relaxed);
    if let Some(current) = &p.current {
        progress.set_current(current.display().to_string());
    }
}

/// Feeds a shell delete's progress into the job, and the job's pause and cancel back.
fn shell_observer(job: &JobHandle) -> forge_win::DeleteObserver {
    let progress = job.progress.clone();
    let cancel = job.cancel.clone();
    Box::new(move |p| {
        show_delete_progress(&progress, p);
        while cancel.is_paused() && !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(100));
        }
        !cancel.is_cancelled()
    })
}

/// Why an item was not deleted, for people. Names the programs holding it open when they
/// can be found.
fn failure_text(path: &Path, why: &forge_win::DeleteFailure) -> String {
    use forge_win::DeleteFailure as F;
    match why {
        F::NotRecyclable => "too big for the Recycle Bin, or its drive has no Recycle Bin".into(),
        F::NeedsAdmin => "access denied: needs administrator permission".into(),
        F::InUse => match forge_win::processes_using(path, 5_000).as_slice() {
            [] => "in use by another program; close it and try again".into(),
            names => format!("in use by {}; close it and try again", names.join(", ")),
        },
        F::Missing => "it no longer exists".into(),
        F::Changed => format!("{why}, so it was not deleted"),
        F::Other(message) => message.clone(),
    }
}

/// Reports a shell delete: refreshes the folders, logs every item that stayed, and tells the
/// user. Items that cannot be recycled lead to the "delete permanently instead?" question.
fn finish_shell_delete(
    job: &JobHandle,
    targets: &[PathBuf],
    report: &forge_win::DeleteReport,
    recycled: bool,
    skipped: Vec<String>,
    tx: &Sender<Event>,
) {
    job.progress.set_note("");
    job.progress.in_one_step.store(false, Ordering::Relaxed);
    let mut seen = std::collections::HashSet::new();
    for d in targets.iter().filter_map(|p| p.parent()) {
        if seen.insert(d) {
            let _ = tx.send(Event::Changed(d.to_path_buf()));
        }
    }
    let not_recyclable: Vec<PathBuf> = report
        .failed
        .iter()
        .filter(|(_, why)| *why == forge_win::DeleteFailure::NotRecyclable)
        .map(|(p, _)| p.clone())
        .collect();
    let mut other: Vec<String> = report
        .failed
        .iter()
        .filter(|(_, why)| *why != forge_win::DeleteFailure::NotRecyclable)
        .map(|(p, why)| format!("{}: {}", p.display(), failure_text(p, why)))
        .collect();
    for line in &skipped {
        job.log(format!("NOT DELETED  {line}"));
    }
    other.extend(skipped);
    // A cancel or abort that stopped inside a folder: not "left as it was".
    for p in &report.partly_deleted {
        let line = format!(
            "{}: partly deleted, some of its contents may already be gone",
            p.display()
        );
        job.log(format!("STOPPED  {line}"));
        other.push(line);
    }
    let n = report.done.len();
    job.log(format!(
        "{n} deleted, {} not deleted, {} not started{}",
        report.failed.len(),
        report.not_started.len(),
        if report.cancelled { ", cancelled" } else { "" }
    ));
    for (p, why) in &report.failed {
        job.log(format!("NOT DELETED  {}: {why}", p.display()));
        job.progress.add_error();
    }
    if report.cancelled {
        // No question after a cancel: the items that could not be recycled are listed.
        other.extend(not_recyclable.iter().map(|p| {
            let why = failure_text(p, &forge_win::DeleteFailure::NotRecyclable);
            format!("{}: {why}", p.display())
        }));
    }
    if !not_recyclable.is_empty() && !report.cancelled {
        let mut body = format!(
            "{} item(s) cannot go to the Recycle Bin: they are too big for it, or their drive \
             has no Recycle Bin. Nothing in them was deleted.\n\n{}\n\nDelete them \
             permanently instead? This cannot be undone.",
            not_recyclable.len(),
            modal::describe_items(&not_recyclable)
        );
        if !other.is_empty() {
            body.push_str(&format!("\n\nAlso not deleted:\n{}", other.join("\n")));
        }
        let _ = tx.send(Event::AskConfirm(Confirm {
            title: "Too big for the Recycle Bin".into(),
            body,
            kind: ConfirmKind::DeleteWithWindows(DeleteTarget::capture_all(&not_recyclable)),
            confirm_label: "Delete permanently".into(),
            danger: true,
        }));
    } else if !other.is_empty() {
        let left = other.len();
        let mut lines = vec![format!("{n} deleted, {left} not deleted.")];
        if report.cancelled {
            lines.push("Cancelled.".into());
        }
        lines.extend(other);
        if report.cancelled && !report.not_started.is_empty() {
            lines.push(format!(
                "Cancelled before {} more item(s).",
                report.not_started.len()
            ));
        }
        let _ = tx.send(Event::Report {
            title: format!("{} — {left} item(s) not deleted", job.title),
            lines,
        });
    } else if report.cancelled {
        let _ = tx.send(Event::Toast(format!(
            "Delete cancelled: {n} item(s) deleted, {} left as they were",
            report.not_started.len()
        )));
    } else if recycled {
        let _ = tx.send(Event::Toast(format!("Moved {n} item(s) to the Recycle Bin")));
    } else {
        let _ = tx.send(Event::Toast(format!("Permanently deleted {n} item(s)")));
    }
}

/// Undoes `records` in a job, last first. An undo runs to the end: it has no Pause or
/// Cancel.
pub fn undo_ops(app: &mut ZxrawApp, records: Vec<ops::UndoRecord>) {
    crate::tags_ui::save_note_now(app);
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops.jobs.spawn(
        "undo",
        format!("Undoing {} change(s)", records.len()),
        move |job| {
            undo_records(job, &records, &tx);
            ctx.request_repaint();
            Ok(())
        },
    );
}

/// The job kinds that undo something: operations and renames ("undo") and text
/// replacements ("undo-replace").
pub const UNDO_KINDS: [&str; 2] = ["undo", "undo-replace"];

/// Whether a job of this kind undoes something.
pub fn is_undo_kind(kind: &str) -> bool {
    UNDO_KINDS.contains(&kind)
}

/// Whether an undo of any kind is still running. Only one runs at a time.
pub fn undo_running(jobs: &forge_jobs::JobRegistry) -> bool {
    jobs.list()
        .iter()
        .any(|j| is_undo_kind(&j.kind) && !j.state().is_finished())
}

/// The work of [`undo_ops`]'s job.
fn undo_records(job: &JobHandle, records: &[ops::UndoRecord], tx: &Sender<Event>) {
    job.progress
        .items_total
        .store(records.len() as u64, Ordering::Relaxed);
    let mut errors = Vec::new();
    for (i, r) in records.iter().enumerate().rev() {
        job.progress.set_current(r.describe());
        match ops::undo(r) {
            Ok(()) => {
                // The undo moved the item back: its library rows follow it.
                if let ops::UndoRecord::Renamed { from, to }
                | ops::UndoRecord::Moved { from, to } = r
                {
                    let _ = tx.send(Event::FilesMoved(vec![(to.clone(), from.clone())]));
                }
            }
            Err(e) => {
                errors.push(format!("{}: {e}", r.describe()));
                job.progress.add_error();
            }
        }
        let touched: Vec<PathBuf> = match r {
            ops::UndoRecord::Renamed { from, to } | ops::UndoRecord::Moved { from, to } => {
                vec![from.clone(), to.clone()]
            }
            ops::UndoRecord::CreatedFolder { path }
            | ops::UndoRecord::CreatedFile { path }
            | ops::UndoRecord::Copied { path, .. } => vec![path.clone()],
        };
        for p in touched {
            if let Some(d) = p.parent() {
                let _ = tx.send(Event::Changed(d.to_path_buf()));
            }
        }
        job.progress
            .items_done
            .store((records.len() - i) as u64, Ordering::Relaxed);
    }
    if errors.is_empty() {
        let _ = tx.send(Event::Toast(format!("Undid {} change(s)", records.len())));
    } else {
        let _ = tx.send(Event::Report {
            title: "Undo was incomplete".into(),
            lines: errors,
        });
    }
}

/// Conflict dialog. Returns true while a question is being shown.
pub fn conflict_dialog(app: &mut ZxrawApp, ctx: &egui::Context) -> bool {
    let Some(q) = app.ops.conflicts.front_mut() else {
        return false;
    };
    let mut answer = None;
    egui::Modal::new(egui::Id::new("conflict")).show(ctx, |ui| {
        ui.set_width(520.0);
        ui.heading("File already exists");
        ui.label(RichText::new(&q.job_title).weak());
        ui.add_space(6.0);
        let name = q
            .info
            .dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        ui.label(format!("The destination already contains “{name}”."));
        ui.add_space(4.0);
        egui::Grid::new("cmp")
            .num_columns(3)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                ui.label("");
                ui.label(RichText::new("Size").strong());
                ui.label(RichText::new("Modified").strong());
                ui.end_row();
                let newer = |a: Option<std::time::SystemTime>, b: Option<std::time::SystemTime>| {
                    a.zip(b).is_some_and(|(a, b)| a > b)
                };
                ui.label("Incoming");
                ui.label(format_size(q.info.source_size));
                ui.label(format!(
                    "{}{}",
                    views::format_time(q.info.source_modified),
                    if newer(q.info.source_modified, q.info.dest_modified) {
                        "  (newer)"
                    } else {
                        ""
                    }
                ));
                ui.end_row();
                ui.label("Existing");
                ui.label(format_size(q.info.dest_size));
                ui.label(format!(
                    "{}{}",
                    views::format_time(q.info.dest_modified),
                    if newer(q.info.dest_modified, q.info.source_modified) {
                        "  (newer)"
                    } else {
                        ""
                    }
                ));
                ui.end_row();
            });
        ui.add_space(8.0);
        ui.checkbox(&mut q.apply_all, "Do this for all remaining conflicts");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .button("Keep both")
                .on_hover_text("The incoming file gets a new name like “name (2)”")
                .clicked()
            {
                answer = Some(ConflictDecision::KeepBoth);
            }
            if ui.button("Skip").clicked() {
                answer = Some(ConflictDecision::Skip);
            }
            if ui.button("Replace if newer").clicked() {
                answer = Some(ConflictDecision::OverwriteIfNewer);
            }
            let replace = egui::Button::new(RichText::new("Replace").color(egui::Color32::WHITE))
                .fill(egui::Color32::from_rgb(170, 60, 50));
            if ui
                .add(replace)
                .on_hover_text("The existing file is overwritten. This cannot be undone.")
                .clicked()
            {
                answer = Some(ConflictDecision::Overwrite);
            }
            if ui.button("Cancel operation").clicked() {
                answer = Some(ConflictDecision::Cancel);
            }
        });
    });
    if let Some(decision) = answer {
        let q = app.ops.conflicts.pop_front().expect("front exists");
        let _ = q.reply.send(Resolution {
            decision,
            apply_to_all: q.apply_all,
        });
    }
    true
}

/// What the user asked for in a queued row.
enum QueueAction {
    Up,
    Down,
    StartNow,
}

pub fn jobs_window(app: &mut ZxrawApp, ctx: &egui::Context) {
    close_guard(app, ctx);
    let queue = app.ops.jobs.transfers().clone();
    queue.set_serial(app.settings.transfer_queue);
    let mut open = app.ops.show_jobs;
    egui::Window::new("Operations")
        .open(&mut open)
        .default_width(520.0)
        .resizable(true)
        .show(ctx, |ui| {
            let jobs = app.ops.jobs.list();
            if jobs.is_empty() {
                ui.label(RichText::new("No operations yet.").weak());
            }
            let waiting = queue.waiting();
            let paused = queue.is_paused();
            if !waiting.is_empty() || paused || app.ops.jobs.pending_transfers() > 0 {
                ui.horizontal(|ui| {
                    let text = match (waiting.len(), paused) {
                        (0, true) => "Queue paused".to_string(),
                        (n, true) => format!("Queue paused · {n} waiting"),
                        (n, false) => format!("{n} waiting in the queue"),
                    };
                    ui.label(RichText::new(text).strong());
                    let label = if paused {
                        "Resume queue"
                    } else {
                        "Pause queue"
                    };
                    if ui
                        .button(label)
                        .on_hover_text(
                            "Pausing holds waiting copies and moves and pauses running ones",
                        )
                        .clicked()
                    {
                        queue.set_paused(!paused);
                    }
                });
                ui.separator();
            }
            let mut action = None;
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .show(ui, |ui| {
                    // Waiting transfers first, in the order they will run.
                    for (i, id) in waiting.iter().enumerate() {
                        let Some(j) = jobs.iter().find(|j| j.id == *id) else {
                            continue;
                        };
                        let place = (i + 1, waiting.len());
                        if let Some(a) = job_row(ui, &app.ops.jobs, j, Some(place)) {
                            action = Some((j.id, a));
                        }
                        ui.separator();
                    }
                    for j in jobs.iter().rev().filter(|j| !waiting.contains(&j.id)) {
                        job_row(ui, &app.ops.jobs, j, None);
                        ui.separator();
                    }
                });
            match action {
                Some((id, QueueAction::Up)) => _ = queue.move_by(id, true),
                Some((id, QueueAction::Down)) => _ = queue.move_by(id, false),
                Some((id, QueueAction::StartNow)) => _ = queue.start_now(id),
                None => {}
            }
            if ui.button("Clear finished").clicked() {
                app.ops.jobs.clear_finished();
            }
        });
    app.ops.show_jobs = open;
}

/// How long a copy, move or delete runs before its progress popup appears.
const POPUP_DELAY: Duration = Duration::from_millis(700);
/// The popup lists at most this many jobs.
const POPUP_MAX_JOBS: usize = 3;

/// Whether a job of this kind gets the progress popup: the ones that change files.
fn popup_kind(kind: &str) -> bool {
    matches!(kind, "delete" | "transfer" | "undo")
}

/// A small window in front of the app with the progress of copies, moves and deletes that
/// have run longer than [`POPUP_DELAY`]: per job its title, a thin bar, the item it is on,
/// one line of counts, and Pause and Cancel. It does not block the app, and it is not
/// shown while the Operations window (status bar) is open.
pub fn progress_popup(app: &mut ZxrawApp, ctx: &egui::Context) {
    let jobs: Vec<JobHandle> = app
        .ops
        .jobs
        .list()
        .into_iter()
        .filter(|j| popup_kind(&j.kind))
        .filter(|j| matches!(j.state(), JobState::Running | JobState::Paused))
        .collect();
    if app.ops.show_jobs || jobs.is_empty() {
        return;
    }
    let (due, waiting): (Vec<&JobHandle>, Vec<&JobHandle>) =
        jobs.iter().partition(|j| j.elapsed() >= POPUP_DELAY);
    if let Some(wait) = waiting.iter().map(|j| POPUP_DELAY.saturating_sub(j.elapsed())).min() {
        ctx.request_repaint_after(wait);
    }
    if due.is_empty() {
        return;
    }
    ctx.request_repaint_after(Duration::from_millis(100));
    let meters = &mut app.ops.delete_ui.meters;
    meters.retain(|id, _| due.iter().any(|j| j.id == *id));
    let mut clicks = Vec::new();
    egui::Window::new("Operation progress")
        .id(egui::Id::new("operation-progress-popup"))
        .title_bar(false)
        .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -40.0])
        .collapsible(false)
        .resizable(false)
        .frame(egui::Frame::window(&ctx.global_style()).inner_margin(egui::Margin::symmetric(14, 12)))
        .show(ctx, |ui| clicks = popup_contents(ui, &due, meters));
    for click in clicks {
        match click {
            PopupClick::Cancel(id) => {
                if let Some(j) = due.iter().find(|j| j.id == id) {
                    app.ops.jobs.cancel(j);
                }
            }
            PopupClick::Pause(id) => {
                if let Some(j) = due.iter().find(|j| j.id == id) {
                    j.cancel.set_paused(!j.cancel.is_paused_itself());
                }
            }
        }
    }
}

/// A button pressed in the progress popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupClick {
    Cancel(forge_jobs::JobId),
    /// Pause, or Resume when paused.
    Pause(forge_jobs::JobId),
}

/// Width of the progress popup's contents.
const POPUP_WIDTH: f32 = 340.0;

/// The progress popup's contents for `jobs` (the first [`POPUP_MAX_JOBS`]).
fn popup_contents(
    ui: &mut Ui,
    jobs: &[&JobHandle],
    meters: &mut std::collections::HashMap<forge_jobs::JobId, RateMeter>,
) -> Vec<PopupClick> {
    let mut clicks = Vec::new();
    ui.set_width(POPUP_WIDTH);
    ui.spacing_mut().item_spacing.y = 6.0;
    for (i, j) in jobs.iter().take(POPUP_MAX_JOBS).enumerate() {
        if i > 0 {
            ui.add_space(8.0);
        }
        let snap = j.progress.snapshot();
        let paused = j.cancel.is_paused_itself();
        let stopping = j.cancel.is_cancelled();
        // Pause and Cancel wait for a step that reports nothing until it ends.
        let in_one_step = j.progress.in_one_step.load(Ordering::Relaxed);
        let meter = meters.entry(j.id).or_default();
        let measured = if snap.bytes_total > 0 {
            snap.bytes_done
        } else {
            snap.items_done
        };
        meter.sample(
            Instant::now(),
            measured,
            // A pause of the whole transfer queue counts too.
            !j.cancel.is_paused() && !stopping && snap.note.is_empty(),
        );
        ui.add(egui::Label::new(RichText::new(j.title.as_ref()).strong()).truncate());
        let held = paused && !in_one_step;
        thin_bar(ui, if snap.note.is_empty() { snap.fraction() } else { None }, held);
        // The name, which a long path would push out of sight; the path is on hover.
        let name = Path::new(&snap.current)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| snap.current.clone());
        let name = if name.is_empty() { " ".to_string() } else { name };
        ui.add(egui::Label::new(RichText::new(name).small().weak()).truncate())
            .on_hover_text(&snap.current);
        let counts = status_text(&snap, meter.rate(), paused, stopping, in_one_step);
        // An undo runs to the end: progress only.
        if is_undo_kind(&j.kind) {
            ui.add(egui::Label::new(RichText::new(counts).small()).truncate());
            continue;
        }
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // A cancel cannot be taken back: until the job ends both buttons rest.
                if ui.add_enabled(!stopping, egui::Button::new("Cancel")).clicked() {
                    clicks.push(PopupClick::Cancel(j.id));
                }
                // A pause still waiting for the step to end rests as "Pausing…".
                let label = match (paused, in_one_step) {
                    (false, _) => "Pause",
                    (true, false) => "Resume",
                    (true, true) => "Pausing…",
                };
                let can = !stopping && !(paused && in_one_step);
                if ui.add_enabled(can, egui::Button::new(label)).clicked() {
                    clicks.push(PopupClick::Pause(j.id));
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(counts).small()).truncate());
                });
            });
        });
    }
    if jobs.len() > POPUP_MAX_JOBS {
        ui.label(
            RichText::new(format!("and {} more in Operations", jobs.len() - POPUP_MAX_JOBS))
                .small()
                .weak(),
        );
    }
    clicks
}

/// Height of the popup's progress bar.
const BAR_HEIGHT: f32 = 4.0;

/// A thin rounded bar in the accent colour, full width. `None` draws a short segment
/// sliding along it, for work that cannot be measured yet. Paused, it is greyed and still.
fn thin_bar(ui: &mut Ui, fraction: Option<f32>, paused: bool) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), BAR_HEIGHT),
        egui::Sense::hover(),
    );
    let radius = BAR_HEIGHT / 2.0;
    let accent = if paused {
        ui.visuals().weak_text_color()
    } else {
        crate::theme::accent(ui.ctx())
    };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, radius, accent.gamma_multiply(0.22));
    let fill = match fraction {
        Some(f) => {
            let w = rect.width() * f.clamp(0.0, 1.0);
            (w > 0.0).then(|| {
                egui::Rect::from_min_size(rect.min, egui::vec2(w.max(BAR_HEIGHT), BAR_HEIGHT))
            })
        }
        None => {
            let seg = rect.width() * 0.3;
            let t = if paused { 0.0 } else { ui.input(|i| i.time) };
            if !paused {
                ui.ctx().request_repaint();
            }
            // Slides from just off the left edge to just off the right one every 1.4 s.
            let x = rect.left() - seg + (rect.width() + seg) * ((t / 1.4).fract() as f32);
            Some(egui::Rect::from_min_size(
                egui::pos2(x, rect.top()),
                egui::vec2(seg, BAR_HEIGHT),
            ))
        }
    };
    if let Some(fill) = fill {
        painter.rect_filled(fill.intersect(rect), radius, accent);
    }
}

/// "45 s left" style estimate, or nothing when there is no sensible one.
fn time_left(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.5 {
        return String::new();
    }
    let secs = secs.round() as u64;
    match secs {
        0..60 => format!("{secs} s left"),
        60..3600 => format!("{} min left", secs.div_ceil(60)),
        _ => format!("{} h {} min left", secs / 3600, (secs % 3600) / 60),
    }
}

/// The popup's line under the bar: what a pending Pause or Cancel waits for, "Paused", or
/// the counts.
fn status_text(
    snap: &forge_jobs::ProgressSnapshot,
    rate: Option<f64>,
    paused: bool,
    stopping: bool,
    in_one_step: bool,
) -> String {
    match (stopping, paused, in_one_step) {
        (true, _, true) => "Stopping after the current folder…".into(),
        (true, _, false) => "Stopping…".into(),
        (false, true, true) => "Pausing after the current folder…".into(),
        (false, true, false) => "Paused".into(),
        (false, false, _) => counts_text(snap, rate),
    }
}

/// How fast a job goes, counting only time it was running and measurably moving: not
/// while paused or counting, nor across a counter that started over (a new pass).
#[derive(Debug, Default)]
struct RateMeter {
    last: Option<(Instant, u64)>,
    active: Duration,
    gained: u64,
}

impl RateMeter {
    /// The shortest running time a speed is given for.
    const SETTLE: Duration = Duration::from_millis(500);

    /// Takes a reading of the counter at `now`. Time while not `running` does not count.
    fn sample(&mut self, now: Instant, done: u64, running: bool) {
        if !running {
            self.last = None;
            return;
        }
        if let Some((then, before)) = self.last
            && done >= before
        {
            self.active += now.saturating_duration_since(then);
            self.gained += done - before;
        }
        self.last = Some((now, done));
    }

    /// Units per second while running, once there has been enough of it.
    fn rate(&self) -> Option<f64> {
        (self.active >= Self::SETTLE && self.gained > 0)
            .then(|| self.gained as f64 / self.active.as_secs_f64())
    }
}

/// The counts line of a running job: its note when it has one ("Counting items…");
/// otherwise done of total with the speed (`rate`, in the unit counted) and the time left.
fn counts_text(snap: &forge_jobs::ProgressSnapshot, rate: Option<f64>) -> String {
    use crate::viewer::cull::format_count;
    if !snap.note.is_empty() {
        return snap.note.clone();
    }
    let rate = rate.unwrap_or(0.0);
    let mut parts = Vec::new();
    let (done, total) = if snap.bytes_total > 0 {
        parts.push(format!(
            "{} of {}",
            format_size(snap.bytes_done),
            format_size(snap.bytes_total)
        ));
        if rate > 0.0 {
            parts.push(format!("{}/s", format_size(rate as u64)));
        }
        (snap.bytes_done, snap.bytes_total)
    } else if snap.items_total > 0 {
        parts.push(format!(
            "{} of {} items",
            format_count(snap.items_done as usize),
            format_count(snap.items_total as usize)
        ));
        if rate >= 1.0 && snap.items_total > 1 {
            parts.push(format!("{}/s", format_count(rate as usize)));
        }
        (snap.items_done, snap.items_total)
    } else {
        parts.push(format!("{} items", format_count(snap.items_done as usize)));
        (0, 0)
    };
    if rate > 0.0 && total > done {
        let left = time_left(total.saturating_sub(done) as f64 / rate);
        if !left.is_empty() {
            parts.push(left);
        }
    }
    parts.join(" · ")
}

/// Lets the close guard run in automation (scripts that test it turn it on).
pub static CLOSE_GUARD_IN_AUTOMATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn close_warning_id() -> egui::Id {
    egui::Id::new("close-with-transfers")
}

/// True while the "copies still waiting" warning is shown.
pub fn close_warning_open(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(close_warning_id()))
        .unwrap_or(false)
}

/// Warns before the window closes while copies or moves are queued or running: closing
/// cancels them (running copies remove their partial files).
fn close_guard(app: &mut ZxrawApp, ctx: &egui::Context) {
    let guarded = !app.testhook.active()
        || CLOSE_GUARD_IN_AUTOMATION.load(std::sync::atomic::Ordering::Relaxed);
    let pending = app.ops.jobs.pending_transfers();
    let confirmed_id = egui::Id::new("close-with-transfers-confirmed");
    let confirmed = ctx
        .data(|d| d.get_temp::<bool>(confirmed_id))
        .unwrap_or(false);
    if guarded && pending > 0 && !confirmed && ctx.input(|i| i.viewport().close_requested()) {
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.data_mut(|d| d.insert_temp(close_warning_id(), true));
    }
    if !close_warning_open(ctx) {
        return;
    }
    if pending == 0 {
        // Everything finished while the warning was open.
        ctx.data_mut(|d| d.insert_temp(close_warning_id(), false));
        return;
    }
    let waiting = app.ops.jobs.transfers().waiting().len();
    let mut choice = None;
    egui::Modal::new(close_warning_id()).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading("Copies are not finished");
        ui.add_space(4.0);
        let running = pending - waiting;
        ui.label(format!(
            "{running} copy or move operation(s) running and {waiting} waiting in the queue. \
             Closing cancels them; files already copied stay where they are."
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Keep open").clicked() {
                choice = Some(false);
            }
            if ui.button("Show operations").clicked() {
                app.ops.show_jobs = true;
                choice = Some(false);
            }
            let close =
                egui::Button::new(RichText::new("Close anyway").color(egui::Color32::WHITE))
                    .fill(egui::Color32::from_rgb(170, 60, 50));
            if ui.add(close).clicked() {
                choice = Some(true);
            }
        });
    });
    if let Some(close) = choice {
        ctx.data_mut(|d| d.insert_temp(close_warning_id(), false));
        if close {
            ctx.data_mut(|d| d.insert_temp(confirmed_id, true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

fn job_row(
    ui: &mut Ui,
    jobs: &forge_jobs::JobRegistry,
    j: &JobHandle,
    place: Option<(usize, usize)>,
) -> Option<QueueAction> {
    let snap = j.progress.snapshot();
    let state = j.state();
    let mut action = None;
    ui.horizontal(|ui| {
        if let Some((n, _)) = place {
            ui.label(RichText::new(format!("{n}.")).weak());
        }
        ui.label(RichText::new(j.title.as_ref()).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !state.is_finished() {
                if ui.small_button("Cancel").clicked() {
                    jobs.cancel(j);
                }
                if let Some((n, len)) = place {
                    if ui
                        .small_button("Start now")
                        .on_hover_text("Start this one without waiting for its turn")
                        .clicked()
                    {
                        action = Some(QueueAction::StartNow);
                    }
                    if ui
                        .add_enabled(n < len, egui::Button::new("⏷").small())
                        .on_hover_text("Run later")
                        .clicked()
                    {
                        action = Some(QueueAction::Down);
                    }
                    if ui
                        .add_enabled(n > 1, egui::Button::new("⏶").small())
                        .on_hover_text("Run sooner")
                        .clicked()
                    {
                        action = Some(QueueAction::Up);
                    }
                } else if state != JobState::Queued {
                    // The user's own pause; a job held only by the paused queue offers
                    // "Pause" and resumes with the queue.
                    let paused = j.cancel.is_paused_itself();
                    if ui
                        .small_button(if paused { "Resume" } else { "Pause" })
                        .clicked()
                    {
                        j.cancel.set_paused(!paused);
                    }
                }
            }
            ui.label(RichText::new(format!("{state:?}")).weak());
        });
    });
    if state == JobState::Queued {
        ui.label(
            RichText::new("Waiting for its turn. Conflicts are checked when it starts.")
                .small()
                .weak(),
        );
        return action;
    }
    job_progress(ui, j, &snap, state);
    if snap.errors > 0 {
        ui.label(
            RichText::new(format!("{} error(s)", snap.errors)).color(ui.visuals().error_fg_color),
        );
    }
    let log = j.log_lines();
    if !log.is_empty() {
        ui.collapsing(format!("Report ({} lines)", log.len()), |ui| {
            for l in log.iter().take(500) {
                ui.label(RichText::new(l).small().monospace());
            }
        });
    }
    action
}

/// A job's progress bar (or spinner) with its counts, and the item it is working on.
fn job_progress(ui: &mut Ui, j: &JobHandle, snap: &forge_jobs::ProgressSnapshot, state: JobState) {
    let frac = snap.fraction();
    let secs = j.elapsed().as_secs_f64().max(0.001);
    let rate = snap.bytes_done as f64 / secs;
    let text = if !snap.note.is_empty() {
        snap.note.clone()
    } else if snap.bytes_total > 0 {
        let eta = if rate > 0.0 && !state.is_finished() {
            let remain = (snap.bytes_total.saturating_sub(snap.bytes_done)) as f64 / rate;
            format!(", ~{}s left", remain.round() as u64)
        } else {
            String::new()
        };
        format!(
            "{} / {}  ({}/s{eta})",
            format_size(snap.bytes_done),
            format_size(snap.bytes_total),
            format_size(rate as u64)
        )
    } else if snap.items_total > 0 {
        format!("{} / {} items", snap.items_done, snap.items_total)
    } else {
        format!("{} items", snap.items_done)
    };
    match frac {
        Some(f) if !state.is_finished() => {
            ui.add(egui::ProgressBar::new(f).text(text));
        }
        None if !state.is_finished() => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(text);
            });
        }
        _ => {
            ui.label(RichText::new(text).weak());
        }
    }
    if !snap.current.is_empty() && !state.is_finished() {
        ui.add(egui::Label::new(RichText::new(&snap.current).small().weak()).truncate());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved_report(done: &[&Path]) -> OpReport {
        OpReport {
            done: done.iter().map(|p| p.to_path_buf()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_permanent_delete_skips_items_changed_since_the_question() {
        let d = tempfile::tempdir().unwrap();
        let (same, replaced, unread, gone) = (
            d.path().join("same"),
            d.path().join("replaced"),
            d.path().join("unread"),
            d.path().join("gone"),
        );
        for p in [&same, &replaced, &unread] {
            std::fs::write(p, b"x").unwrap();
        }
        let targets = vec![
            (same.clone(), Some(1)),
            (replaced.clone(), Some(2)),
            (unread.clone(), None),
            (gone.clone(), Some(4)),
        ];
        let now = |p: &Path| match p.file_name()?.to_str()? {
            "same" => Some(1),
            "replaced" => Some(9),
            "unread" => Some(3),
            _ => None,
        };
        let (keep, skipped) = screen_delete_targets(targets, now);
        assert_eq!(keep, vec![(same, 1)]);
        assert_eq!(skipped.len(), 3, "{skipped:?}");
        assert!(skipped[0].contains("replaced") && skipped[0].contains("changed after"));
        assert!(skipped[1].contains("unread") && skipped[1].contains("could not be identified"));
        assert!(skipped[2].contains("gone") && skipped[2].contains("no longer exists"));
    }

    #[test]
    fn a_shell_delete_cancelled_in_a_windows_dialog_ends_the_job_as_cancelled() {
        let jobs = forge_jobs::JobRegistry::new();
        let job = jobs.spawn("delete", "test", |job| {
            let report = forge_win::DeleteReport {
                cancelled: true,
                ..Default::default()
            };
            delete_result(job, &report, 0)
        });
        let t0 = Instant::now();
        while !job.state().is_finished() && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(job.state(), JobState::Cancelled);
    }

    #[test]
    fn a_cancel_inside_a_folder_reports_it_as_partly_deleted() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let jobs = forge_jobs::JobRegistry::new();
        let job = jobs.spawn("delete", "Deleting 3 item(s)", move |job| {
            let report = forge_win::DeleteReport {
                done: vec![PathBuf::from(r"C:\x\a")],
                partly_deleted: vec![PathBuf::from(r"C:\x\big")],
                not_started: vec![PathBuf::from(r"C:\x\c")],
                cancelled: true,
                ..Default::default()
            };
            finish_shell_delete(job, &[], &report, false, Vec::new(), &tx);
            Ok(())
        });
        let t0 = Instant::now();
        while !job.state().is_finished() && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let lines = rx
            .try_iter()
            .find_map(|e| match e {
                Event::Report { lines, .. } => Some(lines),
                _ => None,
            })
            .expect("a report, not a toast");
        let text = lines.join("\n");
        assert!(text.contains(r"C:\x\big: partly deleted"), "{text}");
        assert!(text.contains("Cancelled before 1 more item(s)"), "{text}");
        assert!(!text.contains(r"C:\x\a"), "{text}");
    }

    #[test]
    fn warnings_are_counted_and_reported_not_toasted() {
        let kept = ops::OpIssue::new(
            r"C:\x\a.jpg",
            r"replaced, but the previous version could not be deleted; it was kept as C:\x\a.old.jpg",
        );
        let mut report = OpReport {
            done: vec![PathBuf::from(r"C:\x\a.jpg"), PathBuf::from(r"C:\x\b.jpg")],
            warnings: vec![kept.clone()],
            ..Default::default()
        };
        let zero = format_size(0);
        assert_eq!(
            report_summary(&report),
            format!("2 done ({zero}), 0 skipped, 0 failed, 1 warning")
        );
        match report_event("Copying 2 item(s)", &report) {
            Event::ReportWithWarnings {
                title,
                lines,
                warnings,
            } => {
                assert_eq!(title, "Copying 2 item(s) — completed with warnings");
                assert_eq!(lines, vec![report_summary(&report)]);
                assert_eq!(warnings, vec![kept.clone()]);
            }
            _ => panic!("warnings must open a report, not a toast"),
        }

        report.warnings.push(kept);
        report.failed.push(ops::OpIssue::new(r"C:\x\c.jpg", "denied"));
        assert!(report_summary(&report).ends_with("1 failed, 2 warnings"));
        assert!(matches!(
            report_event("Copy", &report),
            Event::ReportWithWarnings { title, warnings, .. }
                if title == "Copy — completed with errors" && warnings.len() == 2
        ));

        let clean = OpReport {
            done: vec![PathBuf::from(r"C:\x\a.jpg")],
            ..Default::default()
        };
        assert!(matches!(
            report_event("Copy", &clean),
            Event::Toast(t) if t == format!("1 done ({zero}), 0 skipped, 0 failed")
        ));
        let cancelled = OpReport {
            cancelled: true,
            warnings: vec![ops::OpIssue::new(r"C:\x\a.jpg", "kept as x")],
            ..Default::default()
        };
        assert!(matches!(
            report_event("Copy", &cancelled),
            Event::ReportWithWarnings { lines, .. } if lines[0].ends_with("1 warning, cancelled")
        ));
    }

    #[test]
    fn access_denied_is_recognised_in_any_language() {
        let denied = std::io::Error::from_raw_os_error(5).to_string();
        assert!(is_access_denied(&format!("D:/x: {denied}")));
        let missing = std::io::Error::from_raw_os_error(2).to_string();
        assert!(!is_access_denied(&missing));
        assert!(!is_access_denied("(os error 50)"));
    }

    /// A delete job that runs until it is cancelled.
    fn running_delete(jobs: &forge_jobs::JobRegistry) -> JobHandle {
        jobs.spawn("delete", recycle_title(1), |job| {
            while !job.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        })
    }

    /// The buttons the progress popup shows for `jobs`, by label.
    fn popup_buttons(jobs: &[&JobHandle]) -> Vec<String> {
        popup_button_states(jobs)
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    /// The progress popup's buttons for `jobs`: label and whether it can be pressed.
    fn popup_button_states(jobs: &[&JobHandle]) -> Vec<(String, bool)> {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(600.0, 400.0),
                )),
                ..Default::default()
            },
            |ui| {
                popup_contents(ui, jobs, &mut Default::default());
            },
        );
        output.textures_delta.clear();
        let update = output.platform_output.accesskit_update.take().unwrap();
        update
            .nodes
            .iter()
            .filter(|(_, n)| n.role() == egui::accesskit::Role::Button)
            .filter_map(|(_, n)| n.label().map(|l| (l.to_string(), !n.is_disabled())))
            .collect()
    }

    #[test]
    fn a_pause_or_cancel_that_waits_for_a_folder_move_says_so_and_rests() {
        let jobs = forge_jobs::JobRegistry::new();
        let job = running_delete(&jobs);
        job.progress.in_one_step.store(true, Ordering::Relaxed);
        job.cancel.set_paused(true);
        let mut buttons = popup_button_states(&[&job]);
        buttons.sort();
        assert_eq!(
            buttons,
            [("Cancel".to_string(), true), ("Pausing…".to_string(), false)]
        );
        job.cancel.cancel();
        let buttons = popup_button_states(&[&job]);
        assert!(buttons.iter().all(|(_, can)| !can), "{buttons:?}");
        let snap = forge_jobs::ProgressSnapshot::default();
        assert_eq!(
            status_text(&snap, None, true, true, true),
            "Stopping after the current folder…"
        );
        assert_eq!(status_text(&snap, None, false, true, false), "Stopping…");
        assert_eq!(
            status_text(&snap, None, true, false, true),
            "Pausing after the current folder…"
        );
        assert_eq!(status_text(&snap, None, true, false, false), "Paused");
        // Once the step ends the pause holds, and Resume is offered.
        let jobs = forge_jobs::JobRegistry::new();
        let job = running_delete(&jobs);
        job.cancel.set_paused(true);
        let mut buttons = popup_button_states(&[&job]);
        buttons.sort();
        assert_eq!(
            buttons,
            [("Cancel".to_string(), true), ("Resume".to_string(), true)]
        );
        jobs.cancel(&job);
    }

    #[test]
    fn speed_counts_only_running_time_and_survives_a_new_pass() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut m = RateMeter::default();
        m.sample(at(0), 0, true);
        m.sample(at(1_000), 100, true);
        assert_eq!(m.rate(), Some(100.0));
        // A minute paused: neither the time nor a jump across it counts.
        m.sample(at(1_500), 150, false);
        m.sample(at(61_000), 150, true);
        m.sample(at(62_000), 250, true);
        assert_eq!(m.rate(), Some(100.0));
        // A new pass starts its counter over; the speed carries on from there.
        m.sample(at(63_000), 10, true);
        m.sample(at(64_000), 110, true);
        assert_eq!(m.rate(), Some(100.0));
        assert_eq!(RateMeter::default().rate(), None);
    }

    fn undo_folders(n: usize) -> (tempfile::TempDir, Vec<ops::UndoRecord>) {
        let root = tempfile::tempdir().unwrap();
        let records = (0..n)
            .map(|i| {
                let path = root.path().join(format!("new{i}"));
                std::fs::create_dir(&path).unwrap();
                ops::UndoRecord::CreatedFolder { path }
            })
            .collect();
        (root, records)
    }

    #[test]
    fn undo_reports_its_progress_and_shows_no_pause_or_cancel() {
        let (root, records) = undo_folders(3);
        let (tx, _rx) = crossbeam_channel::unbounded();
        let jobs = forge_jobs::JobRegistry::new();
        let job = jobs.spawn("undo", "Undoing 3 change(s)", move |job| {
            undo_records(job, &records, &tx);
            Ok(())
        });
        wait_finished(&job);
        assert_eq!(job.state(), JobState::Completed);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let snap = job.progress.snapshot();
        assert_eq!((snap.items_done, snap.items_total), (3, 3));
        for kind in UNDO_KINDS {
            let held = jobs.spawn(kind, "Undoing", |job| {
                while !job.cancel.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(())
            });
            assert!(popup_buttons(&[&held]).is_empty(), "{kind}");
            jobs.cancel(&held);
        }
    }

    #[test]
    fn only_one_undo_of_any_kind_runs_at_a_time() {
        let jobs = forge_jobs::JobRegistry::new();
        assert!(!undo_running(&jobs));
        let other = running_delete(&jobs);
        assert!(!undo_running(&jobs), "a delete is not an undo");
        for kind in UNDO_KINDS {
            let job = jobs.spawn(kind, "Undoing", |job| {
                while !job.cancel.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(())
            });
            assert!(undo_running(&jobs), "{kind}");
            jobs.cancel(&job);
            wait_finished(&job);
            assert!(!undo_running(&jobs), "{kind}");
        }
        jobs.cancel(&other);
    }

    fn wait_finished(job: &JobHandle) {
        let t0 = Instant::now();
        while !job.state().is_finished() && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_progress_popup_has_only_pause_and_cancel() {
        let jobs = forge_jobs::JobRegistry::new();
        let job = running_delete(&jobs);
        let mut buttons = popup_buttons(&[&job]);
        buttons.sort();
        assert_eq!(buttons, ["Cancel", "Pause"]);
        job.cancel.set_paused(true);
        let mut buttons = popup_buttons(&[&job]);
        buttons.sort();
        assert_eq!(buttons, ["Cancel", "Resume"]);
        jobs.cancel(&job);
    }

    fn delete_progress(stage: forge_win::DeleteStage) -> forge_win::DeleteProgress {
        forge_win::DeleteProgress {
            stage,
            items_total: 1,
            ..Default::default()
        }
    }

    #[test]
    fn a_big_folder_shows_what_happens_inside_it_instead_of_0_of_1() {
        use forge_win::DeleteStage;
        let progress = forge_jobs::Progress::new();
        let mut p = delete_progress(DeleteStage::Counting);
        p.found = 41_230;
        show_delete_progress(&progress, &p);
        let snap = progress.snapshot();
        assert_eq!(snap.note, "Counting items… 41,230 found");
        assert_eq!(snap.fraction(), None, "nothing to measure yet: the bar slides");

        p.stage = DeleteStage::MovingWhole;
        p.work_total = 100_405;
        p.current = Some(PathBuf::from(r"C:\x\big"));
        show_delete_progress(&progress, &p);
        let snap = progress.snapshot();
        assert_eq!(snap.note, "Moving 100,405 items to the Recycle Bin…");
        assert_eq!(snap.fraction(), None);
        assert_eq!(snap.current, r"C:\x\big");

        // A permanent delete goes file by file: the files inside are counted.
        let mut p = delete_progress(DeleteStage::Deleting);
        p.work_done = 250;
        p.work_total = 1_000;
        p.current = Some(PathBuf::from(r"C:\x\big\in\f.txt"));
        show_delete_progress(&progress, &p);
        let snap = progress.snapshot();
        assert_eq!(snap.note, "");
        assert_eq!((snap.items_done, snap.items_total), (250, 1_000));
        assert_eq!(snap.current, r"C:\x\big\in\f.txt");

        // Without work units (the sandbox), the top-level items count.
        let mut p = delete_progress(DeleteStage::Deleting);
        p.items_done = 1;
        p.items_total = 3;
        show_delete_progress(&progress, &p);
        assert_eq!(progress.snapshot().items_total, 3);
    }

    #[test]
    fn the_counts_line_gives_progress_speed_and_time_left() {
        let snap = forge_jobs::ProgressSnapshot {
            items_done: 2_500,
            items_total: 10_000,
            ..Default::default()
        };
        assert_eq!(
            counts_text(&snap, Some(500.0)),
            "2,500 of 10,000 items · 500/s · 15 s left"
        );
        assert_eq!(counts_text(&snap, None), "2,500 of 10,000 items");
        let snap = forge_jobs::ProgressSnapshot {
            note: "Counting items… 12 found".into(),
            ..Default::default()
        };
        assert_eq!(counts_text(&snap, Some(5.0)), "Counting items… 12 found");
        assert_eq!(time_left(0.2), "");
        assert_eq!(time_left(150.0), "3 min left");
        assert_eq!(time_left(3_700.0), "1 h 1 min left");
    }

    #[test]
    fn the_progress_popup_is_for_jobs_that_change_files() {
        for kind in ["delete", "transfer", "undo"] {
            assert!(popup_kind(kind), "{kind}");
        }
        for kind in ["index", "checksums", "update", "folder-size"] {
            assert!(!popup_kind(kind), "{kind}");
        }
    }

    #[test]
    fn a_cross_volume_folder_move_relinks_the_notes_inside_it() {
        let root = tempfile::tempdir().unwrap();
        let (src, dest) = (root.path().join("trip"), root.path().join("other"));
        std::fs::create_dir_all(src.join("day1")).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        let photo = src.join("day1").join("a.jpg");
        std::fs::write(&photo, b"photo").unwrap();
        let db = std::sync::Arc::new(forge_db::Db::open_in_memory().unwrap());
        db.upsert_entries(&[forge_core::listing::entry_for_path(&photo).unwrap()])
            .unwrap();
        let mut library = crate::library::test_library(db.clone());
        let answer = library.annotate_confirmed(
            vec![photo.clone()],
            forge_db::AnnotationChange {
                notes: Some(Some("day one".into())),
                ..Default::default()
            },
        );
        assert_eq!(answer.recv_timeout(Duration::from_secs(5)).unwrap(), Ok(()));

        // Copy plus delete: the report lists the copied files only, and has no undo record.
        let moved = dest.join("trip").join("day1").join("a.jpg");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::rename(&photo, &moved).unwrap();
        std::fs::remove_dir_all(&src).unwrap();
        let pairs = uncovered_moves(std::slice::from_ref(&src), &dest, &moved_report(&[&moved]));
        assert_eq!(pairs, vec![(src.clone(), dest.join("trip"))]);

        library.follow_moves(pairs);
        assert!(library.flush(Duration::from_secs(5)));
        let id = db
            .file_id(&moved)
            .unwrap()
            .expect("the row followed its folder");
        assert_eq!(db.annotation(id).unwrap().notes.as_deref(), Some("day one"));
        assert!(db.file_id(&photo).unwrap().is_none());
        drop(library);
    }

    #[test]
    fn a_partial_folder_move_relinks_only_the_files_that_moved() {
        let root = tempfile::tempdir().unwrap();
        let (src, dest) = (root.path().join("trip"), root.path().join("other"));
        std::fs::create_dir_all(src.join("sub")).unwrap();
        let target = dest.join("trip");
        std::fs::create_dir_all(target.join("sub")).unwrap();
        // a.jpg and sub/c.jpg moved; b.jpg failed and is still at the source.
        std::fs::write(src.join("b.jpg"), b"b").unwrap();
        for name in ["a.jpg", "b.jpg", "sub/c.jpg"] {
            std::fs::write(target.join(name), b"x").unwrap();
        }
        let report = moved_report(&[
            &target.join("a.jpg"),
            &target.join("b.jpg"),
            &target.join("sub").join("c.jpg"),
        ]);
        let mut pairs = uncovered_moves(std::slice::from_ref(&src), &dest, &report);
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (src.join("a.jpg"), target.join("a.jpg")),
                (
                    src.join("sub").join("c.jpg"),
                    target.join("sub").join("c.jpg")
                ),
            ]
        );
        // A recorded move is left to its undo record.
        let mut covered = report.clone();
        covered.undo.push(ops::UndoRecord::Moved {
            from: src.clone(),
            to: target.clone(),
        });
        assert!(uncovered_moves(std::slice::from_ref(&src), &dest, &covered).is_empty());
    }

    #[test]
    fn a_cut_paste_moves_only_the_items_that_were_cut() {
        use crate::cut_watch::{Identity, Observation};
        let id = |n| Identity::Id {
            dir: false,
            id: (1, n),
        };
        let p = |s: &str| PathBuf::from(s);
        let cut = [
            (p(r"C:\a\same.jpg"), id(1)),
            (p(r"C:\a\replaced.jpg"), id(2)),
            (p(r"C:\a\gone.jpg"), id(3)),
            (p(r"C:\a\unreachable.jpg"), id(4)),
        ];
        let now = |path: &Path| match path.file_name().and_then(|n| n.to_str()) {
            Some("same.jpg") => Observation::Present(id(1)),
            Some("replaced.jpg") => Observation::Present(id(9)),
            Some("gone.jpg") => Observation::Missing,
            Some("new.jpg") => Observation::Present(id(5)),
            _ => Observation::Unknown,
        };
        let sources = [
            "same.jpg",
            "replaced.jpg",
            "gone.jpg",
            "unreachable.jpg",
            "new.jpg",
        ]
        .map(|n| p(r"C:\a").join(n))
        .to_vec();
        let earlier = (vec![p("x")], vec![p("y")]);
        let seen: Vec<_> = sources.iter().map(|s| now(s)).collect();
        let (keep, changed, unverified) = screen_cut_sources(sources, earlier, &cut, &seen);
        assert_eq!(keep, vec![p(r"C:\a\same.jpg"), p(r"C:\a\gone.jpg")]);
        assert_eq!(changed, vec![p("x"), p(r"C:\a\replaced.jpg")]);
        assert_eq!(
            unverified,
            vec![p("y"), p(r"C:\a\unreachable.jpg"), p(r"C:\a\new.jpg")],
            "an item that was not in the cut has no identity to match"
        );
    }

    fn meta(len: u64) -> SourceIdentity {
        let dir = tempfile::tempdir().unwrap();
        SourceIdentity::Meta {
            kind: std::fs::symlink_metadata(dir.path()).unwrap().file_type(),
            len,
            modified: None,
        }
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn queued_move_screening_fails_closed() {
        let sources = paths(&[
            "same",
            "replaced",
            "no-baseline",
            "no-baseline-gone",
            "gone",
        ]);
        let identities = vec![Some(meta(1)), Some(meta(2)), None, None, Some(meta(5))];
        let now = |p: &Path| match p.to_str().unwrap() {
            "same" => Some(meta(1)),
            "replaced" => Some(meta(99)),
            "no-baseline" => Some(meta(3)),
            _ => None,
        };
        let (keep, changed, unverified) = screen_queued_sources(sources, &identities, now);
        // Gone sources stay in `keep`; the job reports them as no longer existing.
        assert_eq!(keep, paths(&["same", "no-baseline-gone", "gone"]));
        assert_eq!(changed, paths(&["replaced"]));
        assert_eq!(unverified, paths(&["no-baseline"]));
    }

    #[test]
    fn queued_copy_screening_keeps_every_source() {
        let sources = paths(&["a", "b"]);
        let (keep, changed, unverified) = screen_queued_sources(sources, &[], |_| Some(meta(1)));
        assert_eq!(keep, paths(&["a", "b"]));
        assert!(changed.is_empty() && unverified.is_empty());
    }
}

#[cfg(test)]
mod cloud_warning_tests {
    use super::*;

    /// Treats every folder as one that may hold cloud files, so the walk itself is tested.
    fn cloud_folder(_: &Path) -> bool {
        true
    }

    /// The production rule for a folder outside every known sync root.
    fn local_folder(dir: &Path) -> bool {
        forge_core::cloud::folder_may_hold_cloud_files(dir, Some(&[]))
    }

    #[test]
    fn small_transfers_do_not_warn() {
        assert_eq!(
            cloud_download_warning(0, 0, OpKind::Copy, false, false),
            None
        );
        assert_eq!(
            cloud_download_warning(CLOUD_WARN_COUNT - 1, 1024, OpKind::Copy, false, false),
            None
        );
        assert_eq!(
            cloud_download_warning(1, CLOUD_WARN_BYTES - 1, OpKind::Copy, false, false),
            None
        );
    }

    #[test]
    fn count_or_size_threshold_warns() {
        let body = cloud_download_warning(CLOUD_WARN_COUNT, 3, OpKind::Copy, false, false).unwrap();
        assert_eq!(
            body,
            "10 files (3 B) are online-only. Copying them downloads them from the cloud first."
        );
        assert!(cloud_download_warning(1, CLOUD_WARN_BYTES, OpKind::Copy, false, false).is_some());
        let single =
            cloud_download_warning(1, CLOUD_WARN_BYTES, OpKind::Move, false, false).unwrap();
        assert!(single.starts_with(&format!(
            "1 file ({}) is online-only. Moving it downloads it",
            format_size(CLOUD_WARN_BYTES)
        )));
    }

    #[test]
    fn capped_walk_totals_are_lower_bounds() {
        let body = cloud_download_warning(CLOUD_WARN_COUNT, 3, OpKind::Copy, false, true).unwrap();
        assert_eq!(
            body,
            "At least 10 files (3 B) are online-only; not every file could be checked, so there \
             may be more. Copying them downloads them from the cloud first."
        );
        // Below the thresholds an incomplete scan still warns: the real total is unknown.
        let one = cloud_download_warning(1, 3, OpKind::Move, false, true).unwrap();
        assert!(
            one.starts_with("At least 1 file (3 B) is online-only; not every file"),
            "{one}"
        );
        // Finding nothing proves nothing when the scan was incomplete.
        assert_eq!(
            cloud_download_warning(0, 0, OpKind::Copy, false, true).as_deref(),
            Some(
                "Couldn't check all files for cloud placeholders. Copying may download some of \
                 them from the cloud first."
            )
        );
        assert_eq!(
            cloud_download_warning(0, 0, OpKind::Copy, false, false),
            None
        );
    }

    #[test]
    fn move_within_one_volume_never_warns() {
        assert_eq!(
            cloud_download_warning(50, CLOUD_WARN_BYTES * 4, OpKind::Move, true, true),
            None
        );
        assert!(cloud_download_warning(50, 0, OpKind::Move, false, false).is_some());
        assert!(cloud_download_warning(50, 0, OpKind::Copy, true, false).is_some());
    }

    #[test]
    fn same_volume_move_skips_the_walk() {
        let dir = tempfile::tempdir().expect("temp directory");
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let mut placeholders = Vec::new();
        for i in 0..CLOUD_WARN_COUNT {
            let path = folder.join(format!("cloud{i}.bin"));
            std::fs::write(&path, b"abc").unwrap();
            forge_core::cloud::simulate_online_only(path.clone());
            placeholders.push(path);
        }
        let dest = dir.path().join("dest");
        // Inside a sync root, so the copy walks it.
        forge_core::cloud::simulate_online_only(folder.clone());
        let sources = std::slice::from_ref(&folder);
        assert_eq!(cloud_warning_for(sources, &dest, OpKind::Move, &CancelToken::new()), None);
        // The same folder copied would warn, so the Move result comes from the rule.
        assert!(cloud_warning_for(sources, &dest, OpKind::Copy, &CancelToken::new()).is_some());
        forge_core::cloud::clear_simulated_online_only(&folder);
        for path in &placeholders {
            forge_core::cloud::clear_simulated_online_only(path);
        }
    }

    #[test]
    fn nested_online_only_files_are_counted() {
        let dir = tempfile::tempdir().expect("temp directory");
        let root = dir.path().join("root");
        let deep = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir(root.join("a").join("empty")).unwrap();
        let folders = [root.clone(), root.join("a"), root.join("a").join("b"), deep];
        let mut placeholders = Vec::new();
        for i in 0..10 {
            let path = folders[i % folders.len()].join(format!("cloud{i}.bin"));
            std::fs::write(&path, b"abc").unwrap();
            forge_core::cloud::simulate_online_only(path.clone());
            placeholders.push(path);
        }
        let local = root.join("local.bin");
        std::fs::write(&local, b"hello").unwrap();

        let elsewhere = dir.path().join("elsewhere");
        let scan = online_only_totals(
            std::slice::from_ref(&root),
            &elsewhere,
            OpKind::Copy,
            &CancelToken::new(),
            &cloud_folder,
        );
        assert_eq!(
            scan,
            OnlineOnlyScan {
                count: 10,
                bytes: 30,
                incomplete: false,
            }
        );
        forge_core::cloud::simulate_online_only(root.clone());
        let body = cloud_warning_for(
            std::slice::from_ref(&root),
            &elsewhere,
            OpKind::Copy,
            &CancelToken::new(),
        )
            .expect("ten online-only files warn");
        assert!(body.starts_with("10 files (30 B) are online-only."));
        forge_core::cloud::clear_simulated_online_only(&root);
        for path in &placeholders {
            forge_core::cloud::clear_simulated_online_only(path);
        }
    }

    #[test]
    fn entry_cap_with_online_only_files_gives_a_lower_bound() {
        let dir = tempfile::tempdir().expect("temp directory");
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let mut placeholders = Vec::new();
        for i in 0..5 {
            let path = root.join(format!("cloud{i}.bin"));
            std::fs::write(&path, b"abc").unwrap();
            forge_core::cloud::simulate_online_only(path.clone());
            placeholders.push(path);
        }
        let elsewhere = dir.path().join("elsewhere");
        // Every entry inside is online-only, so the first two visited are seen before the cap.
        let scan = online_only_totals_with(
            std::slice::from_ref(&root),
            &elsewhere,
            OpKind::Copy,
            ScanLimits::both(2, Duration::from_secs(60)),
            &CancelToken::new(),
            &cloud_folder,
        );
        assert!(scan.incomplete);
        assert_eq!(scan.count, 2);
        let body =
            cloud_download_warning(scan.count, scan.bytes, OpKind::Copy, false, scan.incomplete);
        assert_eq!(
            body.as_deref(),
            Some(
                "At least 2 files (6 B) are online-only; not every file could be checked, so \
                 there may be more. Copying them downloads them from the cloud first."
            )
        );
        for path in &placeholders {
            forge_core::cloud::clear_simulated_online_only(path);
        }
    }

    #[test]
    fn entry_cap_without_online_only_file_still_asks() {
        let dir = tempfile::tempdir().expect("temp directory");
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        for i in 0..5 {
            std::fs::write(root.join(format!("local{i}.bin")), b"abc").unwrap();
        }
        let elsewhere = dir.path().join("elsewhere");
        let scan = online_only_totals_with(
            std::slice::from_ref(&root),
            &elsewhere,
            OpKind::Copy,
            ScanLimits::both(2, Duration::from_secs(60)),
            &CancelToken::new(),
            &cloud_folder,
        );
        assert!(scan.incomplete);
        assert_eq!(scan.count, 0);
        let body =
            cloud_download_warning(scan.count, scan.bytes, OpKind::Copy, false, scan.incomplete)
                .expect("an incomplete scan asks even when it found nothing");
        assert!(body.starts_with("Couldn't check all files"), "{body}");
    }

    #[test]
    fn every_transfer_but_a_same_volume_move_is_checked_on_the_worker() {
        let files = [
            PathBuf::from(r"C:\shoot\a.cr3"),
            PathBuf::from(r"C:\shoot\b.cr3"),
        ];
        // Paths that do not exist: deciding must not touch the filesystem.
        assert!(scan_needed(&files, Path::new(r"D:\backup"), OpKind::Copy));
        assert!(scan_needed(&files, Path::new(r"C:\other"), OpKind::Copy));
        assert!(scan_needed(&files, Path::new(r"D:\backup"), OpKind::Move));
        assert!(!scan_needed(&files, Path::new(r"c:\other"), OpKind::Move));
    }

    #[test]
    fn selected_files_are_checked_before_folders_use_the_budget() {
        let dir = tempfile::tempdir().expect("temp directory");
        let big = dir.path().join("big");
        std::fs::create_dir(&big).unwrap();
        for i in 0..20 {
            std::fs::write(big.join(format!("local{i}.bin")), b"abc").unwrap();
        }
        let mut sources = vec![big];
        for i in 0..12 {
            let path = dir.path().join(format!("f{i}.bin"));
            std::fs::write(&path, b"abc").unwrap();
            forge_core::cloud::simulate_online_only(path.clone());
            sources.push(path);
        }
        let elsewhere = dir.path().join("elsewhere");
        // The folder is listed first, but the 12 files are checked before it is walked.
        let scan = online_only_totals_with(
            &sources,
            &elsewhere,
            OpKind::Copy,
            ScanLimits::both(14, Duration::from_secs(60)),
            &CancelToken::new(),
            &cloud_folder,
        );
        assert_eq!(
            scan,
            OnlineOnlyScan {
                count: 12,
                bytes: 36,
                incomplete: true,
            }
        );
        for path in &sources[1..] {
            forge_core::cloud::clear_simulated_online_only(path);
        }
    }

    #[test]
    fn a_large_file_selection_is_capped_and_incomplete() {
        let dir = tempfile::tempdir().expect("temp directory");
        let files: Vec<PathBuf> = (0..12)
            .map(|i| {
                let path = dir.path().join(format!("f{i}.bin"));
                std::fs::write(&path, b"abc").unwrap();
                path
            })
            .collect();
        let elsewhere = dir.path().join("elsewhere");
        let scan = online_only_totals_with(
            &files,
            &elsewhere,
            OpKind::Copy,
            ScanLimits::both(4, Duration::from_secs(60)),
            &CancelToken::new(),
            &cloud_folder,
        );
        assert_eq!(
            scan,
            OnlineOnlyScan {
                count: 0,
                bytes: 0,
                incomplete: true,
            }
        );
    }

    #[test]
    fn unreadable_folders_make_the_scan_incomplete() {
        let dir = tempfile::tempdir().expect("temp directory");
        // A "folder" that read_dir cannot list: a file posing as the walk root.
        let not_a_dir = dir.path().join("plain.bin");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let mut budget = ScanBudget {
            visited: 0,
            max_entries: 100,
            deadline: Instant::now() + Duration::from_secs(60),
            cancel: &CancelToken::new(),
        };
        let mut scan = OnlineOnlyScan::default();
        assert!(walk_online_only(&not_a_dir, &mut budget, &mut scan));
        assert!(scan.incomplete);
    }

    #[test]
    fn a_cancelled_scan_stops_and_is_incomplete() {
        let dir = tempfile::tempdir().expect("temp directory");
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        let cancel = CancelToken::new();
        let guard = CloudScanGuard(cancel.clone());
        drop(guard);
        assert!(cancel.is_cancelled(), "dismissing the dialog cancels its scan");
        let scan = online_only_totals_with(
            std::slice::from_ref(&file),
            &dir.path().join("elsewhere"),
            OpKind::Copy,
            ScanLimits::both(100, Duration::from_secs(60)),
            &cancel,
            &cloud_folder,
        );
        assert!(scan.incomplete);
    }

    #[test]
    fn a_large_local_folder_starts_without_asking_but_a_cloud_one_asks() {
        let dir = tempfile::tempdir().expect("temp directory");
        let shoot = dir.path().join("shoot");
        std::fs::create_dir(&shoot).unwrap();
        for i in 0..30 {
            std::fs::write(shoot.join(format!("IMG_{i:04}.CR3")), b"raw").unwrap();
        }
        let elsewhere = dir.path().join("backup");
        let limits = ScanLimits::both(5, Duration::from_secs(60));
        // Far over the walk cap, but positively outside every cloud location: not walked,
        // complete, no prompt.
        let local = online_only_totals_with(
            std::slice::from_ref(&shoot),
            &elsewhere,
            OpKind::Copy,
            limits,
            &CancelToken::new(),
            &local_folder,
        );
        assert_eq!(local, OnlineOnlyScan::default());
        assert_eq!(
            cloud_download_warning(local.count, local.bytes, OpKind::Copy, false, local.incomplete),
            None
        );
        // The same tree marked as a cloud folder is walked, hits the cap and asks.
        forge_core::cloud::simulate_online_only(shoot.clone());
        let cloud = online_only_totals_with(
            std::slice::from_ref(&shoot),
            &elsewhere,
            OpKind::Copy,
            limits,
            &CancelToken::new(),
            &local_folder,
        );
        forge_core::cloud::clear_simulated_online_only(&shoot);
        assert!(cloud.incomplete);
        assert!(
            cloud_download_warning(cloud.count, cloud.bytes, OpKind::Copy, false, cloud.incomplete)
                .is_some()
        );
        // A folder holding a known sync root (the profile folder holding OneDrive) is walked.
        let holds_root = |d: &Path| {
            forge_core::cloud::folder_may_hold_cloud_files(d, Some(&[shoot.join("OneDrive")]))
        };
        assert!(
            online_only_totals_with(
                std::slice::from_ref(&shoot),
                &elsewhere,
                OpKind::Copy,
                limits,
                &CancelToken::new(),
                &holds_root,
            )
            .incomplete
        );
    }

    #[test]
    fn selected_files_in_a_local_folder_are_still_checked_one_by_one() {
        let dir = tempfile::tempdir().expect("temp directory");
        let file = dir.path().join("offline.mov");
        std::fs::write(&file, b"abc").unwrap();
        forge_core::cloud::simulate_online_only(file.clone());
        let scan = online_only_totals_with(
            std::slice::from_ref(&file),
            &dir.path().join("backup"),
            OpKind::Copy,
            ScanLimits::both(100, Duration::from_secs(60)),
            &CancelToken::new(),
            &local_folder,
        );
        forge_core::cloud::clear_simulated_online_only(&file);
        assert_eq!((scan.count, scan.bytes), (1, 3));
    }

    fn slow_identity(_: &Path) -> Option<SourceIdentity> {
        std::thread::sleep(Duration::from_millis(400));
        None
    }

    #[test]
    fn identities_come_from_a_fixed_pool_and_a_stuck_one_fails_closed() {
        let dir = tempfile::tempdir().expect("temp directory");
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        let pool = IdentityPool::new(1, source_identity);
        let identities = pool
            .capture(std::slice::from_ref(&file))
            .wait(&CancelToken::new(), Duration::from_secs(10))
            .unwrap();
        assert!(identities[0].is_some());

        // A source that does not answer in time fails the move closed, naming it.
        let stuck = IdentityPool::new(1, slow_identity);
        let pending = stuck.capture(std::slice::from_ref(&file));
        assert_eq!(
            pending.wait(&CancelToken::new(), Duration::from_millis(50)),
            Err(IdentityWait::TimedOut(file.clone()))
        );
        // Cancelled while waiting.
        let cancel = CancelToken::new();
        cancel.cancel();
        assert_eq!(
            stuck
                .capture(std::slice::from_ref(&file))
                .wait(&cancel, Duration::from_secs(10)),
            Err(IdentityWait::Cancelled)
        );
    }

    static READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn counted_identity(_: &Path) -> Option<SourceIdentity> {
        READS.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        None
    }

    fn paced_identity(_: &Path) -> Option<SourceIdentity> {
        std::thread::sleep(Duration::from_millis(30));
        None
    }

    #[test]
    fn a_small_move_is_not_failed_by_a_large_one_queued_ahead() {
        // One worker, 30 ms a read: the large move's 200 reads take 6 s in all. The small
        // move takes turns with it, and the time it spends queued does not count.
        let pool = IdentityPool::new(1, paced_identity);
        let large: Vec<PathBuf> = (0..200).map(|i| PathBuf::from(format!("big{i}"))).collect();
        let large = pool.capture(&large);
        let small = pool.capture(&[PathBuf::from("small")]);
        let started = Instant::now();
        let identities = small
            .wait(&CancelToken::new(), Duration::from_millis(500))
            .expect("served in turn, well within the per-read limit");
        assert_eq!(identities.len(), 1);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        drop(large);
    }

    static RUNNING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static PEAK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn tracked_identity(_: &Path) -> Option<SourceIdentity> {
        let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
        PEAK.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        None
    }

    #[test]
    fn identity_reads_never_exceed_the_lane_workers() {
        let pool = IdentityPool::new(2, tracked_identity);
        let moves: Vec<PendingIdentities> = (0..6)
            .map(|m| {
                let paths: Vec<PathBuf> =
                    (0..5).map(|i| PathBuf::from(format!("m{m}-{i}"))).collect();
                pool.capture(&paths)
            })
            .collect();
        for pending in &moves {
            pending
                .wait(&CancelToken::new(), Duration::from_secs(5))
                .unwrap();
        }
        assert!(PEAK.load(Ordering::SeqCst) <= 2, "at most the lane's two workers");
    }

    #[test]
    fn a_hung_network_read_does_not_block_local_moves() {
        let pool = IdentityPool::new(1, slow_identity);
        let share = pool.capture(&[PathBuf::from(r"\\nas\photos\a.cr3")]);
        // The network lane's only worker is busy for 400 ms; the local lane is free.
        let local = IdentityPool {
            local: IdentityLane::start("test-local", 1, source_identity),
            network: pool.network.clone(),
        };
        let dir = tempfile::tempdir().expect("temp directory");
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        let started = Instant::now();
        local
            .capture(std::slice::from_ref(&file))
            .wait(&CancelToken::new(), Duration::from_secs(5))
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(300));
        drop(share);
    }

    #[test]
    fn a_dropped_move_skips_its_queued_identity_reads() {
        let pool = IdentityPool::new(1, counted_identity);
        let paths: Vec<PathBuf> = (0..10).map(|i| PathBuf::from(format!("x{i}"))).collect();
        drop(pool.capture(&paths));
        std::thread::sleep(Duration::from_millis(600));
        assert!(READS.load(Ordering::SeqCst) <= 1, "queued reads must be skipped");
    }

    #[test]
    fn identities_that_never_arrive_fail_closed() {
        let dir = tempfile::tempdir().expect("temp directory");
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        let (reply, events) = crossbeam_channel::unbounded();
        reply
            .send(IdentityEvent::Done(0, source_identity(&file)))
            .unwrap();
        drop(reply);
        let pending = PendingIdentities {
            events,
            paths: vec![file.clone(), file.clone()],
            cancel: CancelToken::new(),
        };
        let identities = pending
            .wait(&CancelToken::new(), Duration::from_secs(10))
            .unwrap();
        assert!(identities[0].is_some() && identities[1].is_none());
        let (keep, _, unverified) =
            screen_queued_sources(vec![file.clone(), file.clone()], &identities, source_identity);
        assert_eq!((keep.len(), unverified.len()), (1, 1));
    }

    #[test]
    fn a_transfer_is_refused_with_a_message_while_a_dialog_is_open() {
        assert_eq!(
            busy_transfer_message(OpKind::Copy),
            "The copy did not start: close the open dialog first, then try again"
        );
    }

    #[test]
    fn cloud_check_result_is_applied_only_to_its_own_check() {
        assert_eq!(
            cloud_check_action(Some(3), 3, None),
            CloudCheckAction::Start
        );
        assert_eq!(
            cloud_check_action(Some(3), 3, Some("warn".into())),
            CloudCheckAction::Warn("warn".into())
        );
        // A result for an earlier check, or for a dialog already cancelled or continued.
        assert_eq!(
            cloud_check_action(Some(3), 2, Some("warn".into())),
            CloudCheckAction::Ignore
        );
        assert_eq!(cloud_check_action(None, 3, None), CloudCheckAction::Ignore);
    }

    #[test]
    fn files_and_folders_both_need_the_async_scan() {
        let dir = tempfile::tempdir().expect("temp directory");
        let file = dir.path().join("file.bin");
        let folder = dir.path().join("folder");
        std::fs::write(&file, b"abc").unwrap();
        std::fs::create_dir(&folder).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        for source in [&file, &folder] {
            let sources = std::slice::from_ref(source);
            assert!(scan_needed(sources, &elsewhere, OpKind::Copy));
            // Moved within its volume it is a rename: no scan at all.
            assert!(!scan_needed(sources, &elsewhere, OpKind::Move));
        }
    }

    #[test]
    fn same_volume_compares_roots_case_insensitively() {
        assert!(same_volume(Path::new("C:/a/x.jpg"), Path::new("c:/b")));
        assert!(!same_volume(Path::new("C:/a/x.jpg"), Path::new("D:/b")));
    }

    #[test]
    fn totals_count_online_only_files_at_top_level_and_under_folders() {
        let dir = tempfile::tempdir().expect("temp directory");
        let cloud = dir.path().join("cloud.bin");
        let local = dir.path().join("local.bin");
        let folder = dir.path().join("folder");
        let nested = folder.join("nested.bin");
        std::fs::write(&cloud, b"abc").unwrap();
        std::fs::write(&local, b"hello").unwrap();
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(&nested, b"xy").unwrap();
        forge_core::cloud::simulate_online_only(cloud.clone());
        forge_core::cloud::simulate_online_only(folder.clone());
        forge_core::cloud::simulate_online_only(nested.clone());
        let sources = [cloud.clone(), local, folder.clone()];
        let elsewhere = dir.path().join("elsewhere");
        assert_eq!(
            online_only_totals(
                &sources,
                &elsewhere,
                OpKind::Copy,
                &CancelToken::new(),
                &cloud_folder,
            ),
            OnlineOnlyScan {
                count: 2,
                bytes: 5,
                incomplete: false,
            }
        );
        // A move within the same volume is a rename: nothing is read.
        assert_eq!(
            online_only_totals(
                &sources,
                dir.path(),
                OpKind::Move,
                &CancelToken::new(),
                &cloud_folder,
            ),
            OnlineOnlyScan::default()
        );
        forge_core::cloud::clear_simulated_online_only(&cloud);
        forge_core::cloud::clear_simulated_online_only(&folder);
        forge_core::cloud::clear_simulated_online_only(&nested);
    }
}
