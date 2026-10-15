//! Copy/move/delete jobs, the interactive conflict dialog and the operations panel.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded};
use egui::{RichText, Ui};
use forge_core::model::format_size;
use forge_core::ops::{
    self, ConflictDecision, ConflictInfo, ConflictPolicy, JobCtx, OpKind, OpOptions, OpReport,
    Resolution,
};
use forge_jobs::{JobHandle, JobState};

use crate::actions::{Event, UndoEntry};
use crate::app::ZxrawApp;
use crate::modal::{Confirm, ConfirmKind, Modal};
use crate::views;

/// Online-only sources at or above this count, or this total size, ask before a transfer
/// because reading them downloads everything from the cloud.
const CLOUD_WARN_COUNT: usize = 10;
const CLOUD_WARN_BYTES: u64 = 1 << 30;
/// Limits on the metadata-only walk into selected folders. Hitting either one stops the walk
/// and marks its totals as a lower bound.
const CLOUD_WALK_MAX_ENTRIES: usize = 20_000;
const CLOUD_WALK_TIME_LIMIT: Duration = Duration::from_millis(300);
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
}

/// Start a copy or move of `sources` into `dest` as a background job, after asking first
/// when it would download online-only files.
pub fn start_transfer(app: &mut ZxrawApp, sources: Vec<PathBuf>, dest: PathBuf, kind: OpKind) {
    if scan_needed(&sources, &dest, kind) {
        ask_after_cloud_check(app, sources.clone(), sources, dest, kind, None);
        return;
    }
    if let Some(body) = cloud_warning_for(&sources, &dest, kind) {
        let pending = ConfirmKind::DownloadCloudFiles {
            sources,
            dest,
            kind,
            clipboard: None,
        };
        ask_cloud_download(app, body, kind, pending);
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
    clipboard_move(app, sources, dest, generation, expected_sequence, true)
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
    if check_cloud && let Some(body) = cloud_warning_for(&sources, &dest, OpKind::Move) {
        let pending = ConfirmKind::DownloadCloudFiles {
            sources: clipboard_sources,
            dest,
            kind: OpKind::Move,
            clipboard: Some((generation, expected_sequence)),
        };
        ask_cloud_download(app, body, OpKind::Move, pending);
        return true;
    }
    let completion = ClipboardMoveCompletion {
        sources: clipboard_sources,
        generation,
        expected_sequence,
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

/// Shows the "Checking" dialog while the online-only scan runs on a background thread.
/// Cancel drops the dialog, which discards the result. Continue anyway starts the transfer.
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
    std::thread::spawn(move || {
        let warning = cloud_warning_for(&scan_sources, &scan_dest, kind);
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
        },
        title: "Checking for online-only files…".into(),
        body: "Looking for files that would be downloaded from the cloud.".into(),
        confirm_label: "Continue anyway".into(),
        danger: false,
    }));
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

/// Does the transfer need the folder walk? Top-level files are checked synchronously, and a
/// move within one volume is a rename that downloads nothing.
fn scan_needed(sources: &[PathBuf], dest: &Path, kind: OpKind) -> bool {
    if kind == OpKind::Move && sources.iter().all(|path| same_volume(path, dest)) {
        return false;
    }
    sources
        .iter()
        .any(|path| std::fs::symlink_metadata(path).is_ok_and(|md| md.is_dir()))
}

/// The confirmation text when a transfer of `sources` to `dest` would download online-only
/// files, or `None`.
fn cloud_warning_for(sources: &[PathBuf], dest: &Path, kind: OpKind) -> Option<String> {
    let same = sources.iter().all(|path| same_volume(path, dest));
    if kind == OpKind::Move && same {
        return None;
    }
    let scan = online_only_totals(sources, dest, kind);
    cloud_download_warning(scan.count, scan.bytes, kind, same, scan.truncated)
}

/// Online-only files that the transfer would read, and their logical size. Metadata only:
/// placeholders report their full size without being opened.
#[derive(Debug, Default, PartialEq)]
struct OnlineOnlyScan {
    count: usize,
    bytes: u64,
    /// The folder walk stopped at its limits, so `count` and `bytes` are lower bounds.
    truncated: bool,
}

fn online_only_totals(sources: &[PathBuf], dest: &Path, kind: OpKind) -> OnlineOnlyScan {
    online_only_totals_with(
        sources,
        dest,
        kind,
        CLOUD_WALK_MAX_ENTRIES,
        CLOUD_WALK_TIME_LIMIT,
    )
}

/// [`online_only_totals`] with injectable walk limits.
fn online_only_totals_with(
    sources: &[PathBuf],
    dest: &Path,
    kind: OpKind,
    max_entries: usize,
    time_limit: Duration,
) -> OnlineOnlyScan {
    let deadline = Instant::now() + time_limit;
    let mut scan = OnlineOnlyScan::default();
    let mut visited = 0;
    for path in sources {
        if kind == OpKind::Move && same_volume(path, dest) {
            continue;
        }
        let Ok(md) = std::fs::symlink_metadata(path) else {
            continue;
        };
        if md.is_file() {
            if !forge_core::cloud::background_read_allowed(path) {
                scan.count += 1;
                scan.bytes += md.len();
            }
        } else if md.is_dir() {
            walk_online_only(path, max_entries, deadline, &mut visited, &mut scan);
        }
    }
    scan
}

/// Walks the folder `root` and adds its online-only files to `scan`. Listing metadata only:
/// `DirEntry::metadata` comes from the directory enumeration, so no file is opened. Symlinks
/// and junctions are not followed. The walk stops, marking `scan.truncated`, when it has
/// visited `max_entries` entries or passed `deadline`.
fn walk_online_only(
    root: &Path,
    max_entries: usize,
    deadline: Instant,
    visited: &mut usize,
    scan: &mut OnlineOnlyScan,
) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if *visited >= max_entries || Instant::now() >= deadline {
                scan.truncated = true;
                return;
            }
            *visited += 1;
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && let Ok(md) = entry.metadata()
                && forge_core::cloud::is_online_only_metadata(&path, &md)
            {
                scan.count += 1;
                scan.bytes += md.len();
            }
        }
    }
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
/// When the folder walk was `truncated` and it had already seen online-only files, the real
/// total is unknown but not zero, so a generic warning is shown instead of silence. An
/// online-only file seen before the cap proves the selection holds more of them. A walk that
/// stopped before seeing any proves nothing, so it stays silent.
fn cloud_download_warning(
    count: usize,
    bytes: u64,
    kind: OpKind,
    same_volume: bool,
    truncated: bool,
) -> Option<String> {
    if kind == OpKind::Move && same_volume {
        return None;
    }
    let verb = if kind == OpKind::Copy {
        "Copying"
    } else {
        "Moving"
    };
    if count < CLOUD_WARN_COUNT && bytes < CLOUD_WARN_BYTES {
        if truncated && count > 0 {
            return Some(format!(
                "This folder may contain online-only files. {verb} downloads them from the cloud first."
            ));
        }
        return None;
    }
    let (files, are, them) = if count == 1 && !truncated {
        ("1 file".to_owned(), "is", "it")
    } else {
        (format!("{count} files"), "are", "them")
    };
    // A capped walk saw only part of the selection: the totals are lower bounds.
    let files = if truncated {
        format!("At least {files}")
    } else {
        files
    };
    Some(format!(
        "{files} ({}) {are} online-only. {verb} {them} downloads {them} from the cloud first.",
        format_size(bytes)
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
    // A move deletes its sources, so it must act on the items the user picked: what is
    // at a source path when the job starts must still be the same item.
    let identities: Vec<Option<SourceIdentity>> = if kind == OpKind::Move {
        sources.iter().map(|s| source_identity(s)).collect()
    } else {
        Vec::new()
    };
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
        let (sources, changed, unverified) =
            screen_queued_sources(sources, &identities, source_identity);
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
#[derive(PartialEq)]
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
    let mut lines = Vec::new();
    lines.push(format!(
        "{} done ({}), {} skipped, {} failed{}",
        report.done.len(),
        format_size(report.bytes),
        report.skipped.len(),
        report.failed.len(),
        if report.cancelled { ", cancelled" } else { "" }
    ));
    for f in &report.failed {
        lines.push(format!("FAILED  {}: {}", f.path.display(), f.reason));
    }
    for s in &report.skipped {
        lines.push(format!("skipped {}: {}", s.path.display(), s.reason));
    }
    for l in &lines {
        job.log(l.clone());
    }
    for _ in 0..report.failed.len() {
        job.progress.add_error();
    }
    if !report.failed.is_empty() {
        let _ = tx.send(Event::Report {
            title: format!("{} — completed with errors", job.title),
            lines,
        });
    } else if report.cancelled {
        let _ = tx.send(Event::Toast(format!("{} — cancelled", job.title)));
    } else {
        let _ = tx.send(Event::Toast(lines.remove(0)));
    }
}

pub fn delete_permanently(app: &mut ZxrawApp, targets: Vec<PathBuf>) {
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
            ctx.request_repaint();
            Ok(())
        },
    );
}

pub fn undo_ops(app: &mut ZxrawApp, records: Vec<ops::UndoRecord>) {
    crate::tags_ui::save_note_now(app);
    let tx = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops.jobs.spawn(
        "undo",
        format!("Undoing {} change(s)", records.len()),
        move |job| {
            let mut errors = Vec::new();
            for r in records.iter().rev() {
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
            }
            if errors.is_empty() {
                let _ = tx.send(Event::Toast(format!("Undid {} change(s)", records.len())));
            } else {
                let _ = tx.send(Event::Report {
                    title: "Undo was incomplete".into(),
                    lines: errors,
                });
            }
            ctx.request_repaint();
            Ok(())
        },
    );
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
    let frac = snap.fraction();
    let secs = j.elapsed().as_secs_f64().max(0.001);
    let rate = snap.bytes_done as f64 / secs;
    let text = if snap.bytes_total > 0 {
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
        ui.label(RichText::new(&snap.current).small().weak());
    }
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
        assert!(
            body.starts_with("At least 10 files (3 B) are online-only."),
            "{body}"
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
        let sources = std::slice::from_ref(&folder);
        assert_eq!(cloud_warning_for(sources, &dest, OpKind::Move), None);
        // The same folder copied would warn, so the Move result comes from the rule.
        assert!(cloud_warning_for(sources, &dest, OpKind::Copy).is_some());
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
        let scan = online_only_totals(std::slice::from_ref(&root), &elsewhere, OpKind::Copy);
        assert_eq!(
            scan,
            OnlineOnlyScan {
                count: 10,
                bytes: 30,
                truncated: false,
            }
        );
        let body = cloud_warning_for(std::slice::from_ref(&root), &elsewhere, OpKind::Copy)
            .expect("ten online-only files warn");
        assert!(body.starts_with("10 files (30 B) are online-only."));
        for path in &placeholders {
            forge_core::cloud::clear_simulated_online_only(path);
        }
    }

    #[test]
    fn entry_cap_with_online_only_file_gives_generic_warning() {
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
        // Every entry is online-only, so the first two visited are seen before the cap.
        let scan = online_only_totals_with(
            std::slice::from_ref(&root),
            &elsewhere,
            OpKind::Copy,
            2,
            Duration::from_secs(60),
        );
        assert!(scan.truncated);
        assert_eq!(scan.count, 2);
        let body =
            cloud_download_warning(scan.count, scan.bytes, OpKind::Copy, false, scan.truncated);
        assert_eq!(
            body.as_deref(),
            Some(
                "This folder may contain online-only files. Copying downloads them from the cloud first."
            )
        );
        for path in &placeholders {
            forge_core::cloud::clear_simulated_online_only(path);
        }
    }

    #[test]
    fn entry_cap_without_online_only_file_stays_silent() {
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
            2,
            Duration::from_secs(60),
        );
        assert!(scan.truncated);
        assert_eq!(scan.count, 0);
        assert_eq!(
            cloud_download_warning(scan.count, scan.bytes, OpKind::Copy, false, scan.truncated),
            None
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
    fn folders_need_the_async_scan_but_top_level_files_do_not() {
        let dir = tempfile::tempdir().expect("temp directory");
        let file = dir.path().join("file.bin");
        let folder = dir.path().join("folder");
        std::fs::write(&file, b"abc").unwrap();
        std::fs::create_dir(&folder).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        assert!(!scan_needed(
            std::slice::from_ref(&file),
            &elsewhere,
            OpKind::Copy
        ));
        assert!(scan_needed(
            std::slice::from_ref(&folder),
            &elsewhere,
            OpKind::Copy
        ));
        // A folder moved within its volume is a rename: no scan at all.
        assert!(!scan_needed(
            std::slice::from_ref(&folder),
            &elsewhere,
            OpKind::Move
        ));
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
            online_only_totals(&sources, &elsewhere, OpKind::Copy),
            OnlineOnlyScan {
                count: 2,
                bytes: 5,
                truncated: false,
            }
        );
        // A move within the same volume is a rename: nothing is read.
        assert_eq!(
            online_only_totals(&sources, dir.path(), OpKind::Move),
            OnlineOnlyScan::default()
        );
        forge_core::cloud::clear_simulated_online_only(&cloud);
        forge_core::cloud::clear_simulated_online_only(&folder);
        forge_core::cloud::clear_simulated_online_only(&nested);
    }
}
