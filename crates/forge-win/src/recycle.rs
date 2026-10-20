//! Shell deletes (Recycle Bin or permanent) with progress, owned dialogs and a reason for
//! every item that was not deleted.
//!
//! The shell's own progress window is hidden; progress comes back through an
//! `IFileOperationProgressSink` and the caller shows it. Error dialogs are off. The one
//! shell dialog that can still appear is the "You'll need to provide administrator
//! permission" prompt for protected folders such as `C:\Program Files`, owned by the window
//! set with [`set_dialog_owner`] so it opens in front of the app instead of behind it.
//!
//! [`recycle_items`] never deletes permanently. When the shell finds an item it cannot
//! recycle (too big for the Recycle Bin, or a drive without one), it would normally ask
//! "Do you want to permanently delete it?" in its own window. Instead the sink refuses the
//! permanent delete, the item is reported as [`DeleteFailure::NotRecyclable`] and the app
//! asks in its own dialog.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

static DIALOG_OWNER: AtomicIsize = AtomicIsize::new(0);

/// Sets the window (`HWND`) that owns the shell dialogs of [`recycle_items`] and
/// [`delete_items_permanently`]. 0 means no owner.
pub fn set_dialog_owner(hwnd: isize) {
    DIALOG_OWNER.store(hwnd, Ordering::Relaxed);
}

/// Why an item was not deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteFailure {
    /// It cannot go to the Recycle Bin: too big for it, or its drive has no Recycle Bin.
    /// Nothing of it was deleted.
    NotRecyclable,
    /// Windows needs administrator permission to delete it, and that was not given.
    NeedsAdmin,
    /// A program has it, or a file inside it, open.
    InUse,
    /// It no longer exists.
    Missing,
    /// It is no longer the object the user was asked about (moved away or replaced).
    Changed,
    /// Anything else, with the system's message.
    Other(String),
}

impl std::fmt::Display for DeleteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRecyclable => {
                f.write_str("too big for the Recycle Bin, or its drive has none")
            }
            Self::NeedsAdmin => f.write_str("needs administrator permission"),
            Self::InUse => f.write_str("in use by another program"),
            Self::Missing => f.write_str("no longer exists"),
            Self::Changed => f.write_str("changed after you were asked (moved or replaced)"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

/// The outcome of [`recycle_items`] or [`delete_items_permanently`].
#[derive(Debug, Default, Clone)]
pub struct DeleteReport {
    /// Items that are gone.
    pub done: Vec<PathBuf>,
    /// Items that are still there, with the reason.
    pub failed: Vec<(PathBuf, DeleteFailure)>,
    /// Items not tried because the operation was cancelled.
    pub not_started: Vec<PathBuf>,
    /// Items a cancel or abort stopped midway: they are still there, but some of their
    /// contents may already be gone.
    pub partly_deleted: Vec<PathBuf>,
    /// The caller or the user (in a shell dialog) cancelled.
    pub cancelled: bool,
}

/// What a shell delete is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeleteStage {
    /// Nothing reported yet.
    #[default]
    Starting,
    /// The shell is counting the files and folders inside the items (`found` so far).
    /// For a big folder this is most of the wait before anything moves.
    Counting,
    /// Deleting: `work_done` of `work_total`, or the top-level counts.
    Deleting,
    /// The shell is moving a whole item into the Recycle Bin in one step. It reports
    /// nothing until that step ends, which for a folder with many files takes a while.
    MovingWhole,
}

/// Progress passed to the observer of [`recycle_items`] and [`delete_items_permanently`].
#[derive(Debug, Clone, Default)]
pub struct DeleteProgress {
    pub stage: DeleteStage,
    /// Files and folders the shell has found so far while counting.
    pub found: u64,
    /// Top-level items finished so far, and how many there are.
    pub items_done: usize,
    pub items_total: usize,
    /// The shell's own work units for the current pass (about one per file or folder in
    /// the tree). 0 of 0 until the shell has counted.
    pub work_done: u64,
    pub work_total: u64,
    /// The file or folder being deleted.
    pub current: Option<PathBuf>,
}

/// Called with progress; returning `false` cancels the operation. It hears about
/// progress inside folders at most [`TELL_EVERY`] apart; a new stage, a finished item and
/// the end of a pass always reach it.
pub type DeleteObserver = Box<dyn FnMut(&DeleteProgress) -> bool>;

/// The shortest gap between two progress reports that change only counts or the current
/// item (20 a second). The shell reports every file of a big folder, and the observer has
/// no need to hear about each one.
pub const TELL_EVERY: Duration = Duration::from_millis(50);

/// Lets a report through at most once per [`TELL_EVERY`], unless it is forced.
#[derive(Debug, Default)]
struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    /// Whether a report at `now` goes through; a forced one always does.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn due(&mut self, now: Instant, force: bool) -> bool {
        let due = force
            || self
                .last
                .is_none_or(|last| now.saturating_duration_since(last) >= TELL_EVERY);
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// What an item is, so a later check can tell whether its path still names the same
/// object: type and creation time always, the file ID where the file system has a stable
/// one, and for files the size and last write time. Every part must be present: an item
/// whose creation or write time cannot be read has no identity, and nothing missing ever
/// counts as equal. A folder's size and write time change with its contents, so they are
/// not part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemIdentity {
    dir: bool,
    /// Volume serial and file index. `None` where the file system reports none (index 0).
    id: Option<(u32, u64)>,
    created: std::time::SystemTime,
    /// Size and last write time, for files.
    file: Option<(u64, std::time::SystemTime)>,
}

impl ItemIdentity {
    /// The identity of `path` (not following links), or `None` when it is missing or a
    /// required part cannot be read.
    pub fn of(path: &Path) -> Option<Self> {
        let md = std::fs::symlink_metadata(path).ok()?;
        let dir = md.is_dir();
        let file = if dir {
            None
        } else {
            Some((md.len(), md.modified().ok()?))
        };
        Some(Self {
            dir,
            id: crate::file_id(path).ok().filter(|(_, index)| *index != 0),
            created: md.created().ok()?,
            file,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Recycle,
    Permanent,
}

/// Sends `paths` to the Recycle Bin (or the sandbox set by
/// [`crate::set_recycle_sandbox`]). Never deletes anything permanently: items that cannot
/// be recycled stay in place as [`DeleteFailure::NotRecyclable`]. Runs on the calling
/// thread, which gets its own COM initialisation.
pub fn recycle_items(paths: &[PathBuf], observer: DeleteObserver) -> DeleteReport {
    match crate::shell::recycle_sandbox_dir() {
        Some(dir) => recycle_into_sandbox(&dir, paths, observer),
        None => run_shell(paths, &[], Mode::Recycle, observer),
    }
}

/// Deletes `paths` permanently through the shell, which can ask for administrator
/// permission for protected folders (unlike an in-process delete).
pub fn delete_items_permanently(paths: &[PathBuf], observer: DeleteObserver) -> DeleteReport {
    run_shell(paths, &[], Mode::Permanent, observer)
}

/// Like [`delete_items_permanently`], for items the user confirmed: each is deleted only if
/// it is still the object it was when asked. That is checked again as the shell reaches
/// each item; one that was moved away or replaced is left and reported as
/// [`DeleteFailure::Changed`], and the rest go on.
pub fn delete_confirmed_items(
    items: &[(PathBuf, ItemIdentity)],
    observer: DeleteObserver,
) -> DeleteReport {
    let paths: Vec<PathBuf> = items.iter().map(|(p, _)| p.clone()).collect();
    let expected: Vec<Option<ItemIdentity>> =
        items.iter().map(|(_, id)| Some(id.clone())).collect();
    if paths.is_empty() {
        return DeleteReport::default();
    }
    run_shell(&paths, &expected, Mode::Permanent, observer)
}

/// Name prefix that the recycle sandbox treats as "cannot be recycled", so automated runs
/// can reach the app's "delete permanently instead?" dialog.
pub const SANDBOX_NOT_RECYCLABLE_PREFIX: &str = "not-recyclable";

fn recycle_into_sandbox(
    dir: &Path,
    paths: &[PathBuf],
    mut observer: DeleteObserver,
) -> DeleteReport {
    let mut report = DeleteReport::default();
    let mut progress = DeleteProgress {
        stage: DeleteStage::Deleting,
        items_total: paths.len(),
        ..Default::default()
    };
    for (i, p) in paths.iter().enumerate() {
        progress.current = Some(p.clone());
        if !observer(&progress) {
            report.cancelled = true;
            report.not_started.extend(paths[i..].iter().cloned());
            break;
        }
        let refused = p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(SANDBOX_NOT_RECYCLABLE_PREFIX));
        if std::fs::symlink_metadata(p).is_err() {
            report.failed.push((p.clone(), DeleteFailure::Missing));
        } else if refused {
            report
                .failed
                .push((p.clone(), DeleteFailure::NotRecyclable));
        } else {
            match crate::shell::recycle_to_sandbox(dir, std::slice::from_ref(p)) {
                Ok(()) => report.done.push(p.clone()),
                Err(e) => report
                    .failed
                    .push((p.clone(), DeleteFailure::Other(e.to_string()))),
            }
        }
        progress.items_done = i + 1;
        progress.current = None;
        if !observer(&progress) {
            report.cancelled = true;
            report.not_started.extend(paths[i + 1..].iter().cloned());
            break;
        }
    }
    report
}

/// Whether `path` is certainly gone. Any error other than "not found" counts as present.
fn is_gone(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(windows)]
fn run_shell(
    paths: &[PathBuf],
    expected: &[Option<ItemIdentity>],
    mode: Mode,
    observer: DeleteObserver,
) -> DeleteReport {
    imp::run(paths, expected, mode, observer)
}

#[cfg(not(windows))]
fn run_shell(
    paths: &[PathBuf],
    _expected: &[Option<ItemIdentity>],
    _mode: Mode,
    _observer: DeleteObserver,
) -> DeleteReport {
    DeleteReport {
        failed: paths
            .iter()
            .map(|p| {
                (
                    p.clone(),
                    DeleteFailure::Other("not supported on this platform".into()),
                )
            })
            .collect(),
        ..Default::default()
    }
}

/// HRESULTs the shell reports per item, classified.
#[cfg_attr(not(windows), allow(dead_code))]
mod codes {
    use super::DeleteFailure;

    pub const S_NOT_HANDLED: u32 = 0x0027_0003;
    pub const E_USER_CANCELLED: u32 = 0x8027_0000;
    pub const E_CANCELLED: u32 = 0x8027_0001;
    pub const E_REQUIRES_ELEVATION: u32 = 0x8027_0002;
    pub const E_ACCESS_DENIED_SRC: u32 = 0x8027_0021;
    pub const E_PATH_NOT_FOUND_SRC: u32 = 0x8027_0023;
    pub const E_SHARING_VIOLATION_SRC: u32 = 0x8027_0027;
    pub const E_SHARING_VIOLATION_DEST: u32 = 0x8027_0028;
    pub const E_ACCESSDENIED_READONLY: u32 = 0x8027_003F;
    pub const E_RECYCLE_UNKNOWN_ERROR: u32 = 0x8027_0035;
    pub const E_RECYCLE_FORCE_NUKE: u32 = 0x8027_0036;
    pub const E_RECYCLE_SIZE_TOO_BIG: u32 = 0x8027_0037;
    pub const E_RECYCLE_PATH_TOO_LONG: u32 = 0x8027_0038;
    pub const E_RECYCLE_BIN_NOT_FOUND: u32 = 0x8027_003A;
    pub const E_ACCESSDENIED: u32 = 0x8007_0005;
    pub const E_FILE_NOT_FOUND: u32 = 0x8007_0002;
    pub const E_PATH_NOT_FOUND: u32 = 0x8007_0003;
    pub const E_SHARING_VIOLATION: u32 = 0x8007_0020;
    pub const E_LOCK_VIOLATION: u32 = 0x8007_0021;
    /// HRESULT_FROM_WIN32(ERROR_CANCELLED): what the sink returns to stop the shell.
    pub const E_ERROR_CANCELLED: u32 = 0x8007_04C7;

    /// A cancel, by the user in a shell dialog or by the sink.
    pub fn is_cancel(hr: u32) -> bool {
        matches!(hr, E_USER_CANCELLED | E_CANCELLED | E_ERROR_CANCELLED)
    }

    pub fn classify(hr: u32, message: impl FnOnce() -> String) -> DeleteFailure {
        match hr {
            E_REQUIRES_ELEVATION | E_ACCESS_DENIED_SRC | E_ACCESSDENIED => {
                DeleteFailure::NeedsAdmin
            }
            E_SHARING_VIOLATION_SRC
            | E_SHARING_VIOLATION_DEST
            | E_SHARING_VIOLATION
            | E_LOCK_VIOLATION => DeleteFailure::InUse,
            E_PATH_NOT_FOUND_SRC | E_FILE_NOT_FOUND | E_PATH_NOT_FOUND => DeleteFailure::Missing,
            E_RECYCLE_FORCE_NUKE
            | E_RECYCLE_SIZE_TOO_BIG
            | E_RECYCLE_BIN_NOT_FOUND
            | E_RECYCLE_PATH_TOO_LONG
            | E_RECYCLE_UNKNOWN_ERROR => DeleteFailure::NotRecyclable,
            E_ACCESSDENIED_READONLY => DeleteFailure::Other("it is read-only".into()),
            _ => DeleteFailure::Other(message()),
        }
    }
}

/// `TSF_DELETE_RECYCLE_IF_POSSIBLE`: the shell sets it on a delete it will send to the
/// Recycle Bin, and clears it for an item it is about to destroy.
const RECYCLE_IF_POSSIBLE: u32 = 0x80;

/// Whether the sink must refuse a delete with these flags: during a recycle, anything the
/// shell would destroy instead of recycling. Decided from `mode` and `flags` alone, so it
/// holds even when nothing else about the call can be checked.
fn refuses_permanent_delete(mode: Mode, flags: u32) -> bool {
    mode == Mode::Recycle && flags & RECYCLE_IF_POSSIBLE == 0
}

/// Which of a pass's items (lowercase keys) `path` is, or lies inside. `None` when the path
/// belongs to none of them.
#[cfg(test)]
fn item_of(keys: &[String], path: &str) -> Option<usize> {
    item_of_lower(keys, &path.to_lowercase())
}

/// [`item_of`] for a path already in lowercase.
#[cfg_attr(not(windows), allow(dead_code))]
fn item_of_lower(keys: &[String], path: &str) -> Option<usize> {
    if let Some(i) = keys.iter().position(|k| k == path) {
        return Some(i);
    }
    keys.iter()
        .enumerate()
        .filter(|(_, k)| {
            path.strip_prefix(k.trim_end_matches('\\'))
                .is_some_and(|rest| rest.starts_with('\\'))
        })
        .max_by_key(|(_, k)| k.len())
        .map(|(i, _)| i)
}

/// What the sink refused in a pass. A refusal stops the pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
enum Refusal {
    #[default]
    None,
    /// The permanent delete of this item (or of something inside it) during a recycle.
    Item(usize),
    /// This confirmed item is no longer the object the user was asked about.
    Changed(usize),
    /// A delete the sink could not tie to an item, or two different refusals.
    Unknown,
}

impl Refusal {
    /// `self` after another refusal `next` in the same pass.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn and(self, next: Refusal) -> Refusal {
        if self == Refusal::None || self == next {
            next
        } else {
            Refusal::Unknown
        }
    }
}

/// Reason given for items left after a refusal the sink could not tie to an item.
const UNKNOWN_REFUSAL: &str = "Windows was about to delete something ZXraw could not \
     identify, so it stopped. Nothing more was deleted";

/// How one shell pass ended, as seen after it returned.
#[cfg_attr(not(windows), allow(dead_code))]
struct Pass<'a> {
    items: &'a [PathBuf],
    /// Whether each item is gone from disk now.
    gone: &'a [bool],
    refusal: Refusal,
    /// The first error the shell reported for each item.
    errors: &'a [Option<u32>],
    /// Whether the shell had deleted, or begun deleting, something inside each item.
    started: &'a [bool],
    /// The caller cancelled, or the user did in a shell dialog.
    cancelled: bool,
    /// What `GetAnyOperationsAborted` answered after the pass (`false` when the call
    /// failed; its error is then in `run_error`).
    aborted: bool,
    /// What `PerformOperations` (or the abort query) returned, if it failed: the HRESULT
    /// and its message.
    run_error: Option<(u32, String)>,
}

/// Adds a pass's outcome to `report`. Returns the indices of the items to try again in a
/// new pass: only after the sink refused a known item, and only the items not reached.
///
/// A pass the shell reports as aborted, with items left, was cancelled by the user or the
/// system when nothing else in the pass explains the abort: no refusal of ours, no error
/// for any item, no failure of the call itself.
#[cfg_attr(not(windows), allow(dead_code))]
fn settle_pass(
    pass: Pass<'_>,
    message: impl Fn(u32) -> String,
    report: &mut DeleteReport,
) -> Vec<usize> {
    let mut left = Vec::new();
    for (i, p) in pass.items.iter().enumerate() {
        if pass.gone[i] {
            report.done.push(p.clone());
        } else if pass.refusal == Refusal::Item(i) {
            report
                .failed
                .push((p.clone(), DeleteFailure::NotRecyclable));
        } else if pass.refusal == Refusal::Changed(i) {
            report.failed.push((p.clone(), DeleteFailure::Changed));
        } else if let Some(hr) = pass.errors[i].filter(|hr| !codes::is_cancel(*hr)) {
            report
                .failed
                .push((p.clone(), codes::classify(hr, || message(hr))));
        } else {
            left.push(i);
        }
    }
    let item_errors = pass
        .errors
        .iter()
        .flatten()
        .any(|hr| !codes::is_cancel(*hr));
    let unexplained_abort = pass.aborted
        && pass.refusal == Refusal::None
        && !item_errors
        && pass
            .run_error
            .as_ref()
            .is_none_or(|(hr, _)| codes::is_cancel(*hr));
    if pass.cancelled || (unexplained_abort && !left.is_empty()) {
        report.cancelled = true;
        for &i in &left {
            let item = pass.items[i].clone();
            if pass.started[i] {
                report.partly_deleted.push(item);
            } else {
                report.not_started.push(item);
            }
        }
        return Vec::new();
    }
    let why = match pass.refusal {
        Refusal::Item(_) | Refusal::Changed(_) => return left,
        Refusal::Unknown => DeleteFailure::Other(UNKNOWN_REFUSAL.into()),
        Refusal::None => DeleteFailure::Other(
            pass.run_error
                .map(|(_, m)| m)
                .unwrap_or_else(|| "Windows did not delete it".into()),
        ),
    };
    report.failed.extend(
        left.iter()
            .map(|&i| (pass.items[i].clone(), why.clone())),
    );
    Vec::new()
}

#[cfg(windows)]
mod imp {
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;
    use std::rc::Rc;

    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        FILEOPERATION_FLAGS, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
        FOFX_RECYCLEONDELETE, FOFX_SHOWELEVATIONPROMPT, FileOperation, IFileOperation,
        IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
        SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
    };
    use windows::core::{HRESULT, PCWSTR, Ref, implement};

    use super::{
        DeleteFailure, DeleteObserver, DeleteProgress, DeleteReport, DeleteStage, ItemIdentity,
        Mode, Pass, Refusal, Throttle, codes, is_gone, item_of_lower, refuses_permanent_delete,
        settle_pass,
    };

    /// Progress and the observer. Only progress depends on it: every decision to refuse a
    /// delete is made without it, so a busy state (a reentrant call) never lets one through.
    struct State {
        /// The first error the shell reported for each item (a cancel only when nothing
        /// more specific came).
        errors: Vec<Option<u32>>,
        /// The observer asked to stop.
        stopped: bool,
        /// A cancel came from the shell that the sink did not cause (the user, in a
        /// shell dialog).
        user_cancelled: bool,
        progress: DeleteProgress,
        /// Items finished in earlier passes.
        done_before: usize,
        observer: DeleteObserver,
        throttle: Throttle,
    }

    impl State {
        fn record(&mut self, item: Option<usize>, hr: HRESULT, refused: bool) {
            let Some(i) = item else { return };
            let hr = hr.0 as u32;
            if hr & 0x8000_0000 == 0 {
                return;
            }
            if codes::is_cancel(hr) && !refused && !self.stopped {
                self.user_cancelled = true;
            }
            let slot = &mut self.errors[i];
            match slot {
                None => *slot = Some(hr),
                Some(old) if codes::is_cancel(*old) && !codes::is_cancel(hr) => *slot = Some(hr),
                _ => {}
            }
        }

        /// Tells the observer; false when it asked to stop (now or before).
        fn tell(&mut self) -> bool {
            self.tell_if(true)
        }

        /// Tells the observer when `force` is set or the throttle lets it through; false
        /// when it asked to stop (now or before).
        fn tell_if(&mut self, force: bool) -> bool {
            if !self.stopped
                && self.throttle.due(std::time::Instant::now(), force)
                && !(self.observer)(&self.progress)
            {
                self.stopped = true;
            }
            !self.stopped
        }

        /// Moves to `stage`; true when that is a change.
        fn enter(&mut self, stage: DeleteStage) -> bool {
            let changed = self.progress.stage != stage;
            self.progress.stage = stage;
            changed
        }
    }

    fn cancel_hr() -> windows::core::Error {
        HRESULT(codes::E_ERROR_CANCELLED as i32).into()
    }

    fn path_of(item: Ref<IShellItem>) -> Option<String> {
        let item = item.as_ref()?;
        unsafe {
            let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let s = p.to_string().ok();
            windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
            s
        }
    }

    /// The items of one pass, fixed for the sink's life.
    struct PassItems {
        items: Vec<PathBuf>,
        /// Lowercase keys of `items`, for matching the shell's paths.
        keys: Vec<String>,
        /// For a confirmed delete, what each item was when the user was asked (`None`: no
        /// check). Empty when nothing is checked.
        expected: Vec<Option<ItemIdentity>>,
        /// Items whose identity was checked as the shell reached them.
        checked: Vec<Cell<bool>>,
        /// Items the shell had deleted, or begun deleting, something inside of. A file, or
        /// a folder sent to the Recycle Bin, goes in one step and is never marked.
        started: Vec<Cell<bool>>,
    }

    impl PassItems {
        fn new(items: Vec<PathBuf>, expected: Vec<Option<ItemIdentity>>) -> Self {
            let keys = items
                .iter()
                .map(|p| p.to_string_lossy().to_lowercase())
                .collect();
            let checked = items.iter().map(|_| Cell::new(false)).collect();
            let started = items.iter().map(|_| Cell::new(false)).collect();
            Self {
                items,
                keys,
                expected,
                checked,
                started,
            }
        }

        /// Which item a shell path is in, and whether it is that item itself.
        fn locate(&self, path: Option<&str>) -> Option<(usize, bool)> {
            let lower = path?.to_lowercase();
            item_of_lower(&self.keys, &lower).map(|i| (i, self.keys[i] == lower))
        }

        /// Marks the item a path lies strictly inside (see [`Self::locate`]) as started.
        fn mark_inside(&self, at: Option<(usize, bool)>) {
            if let Some((i, false)) = at {
                self.started[i].set(true);
            }
        }

        /// Whether item `i` is still what the user was asked about. Checked once per pass,
        /// when the shell first reaches the item or anything inside it.
        fn still_same(&self, i: usize) -> bool {
            if self.expected.is_empty() || self.checked[i].get() {
                return true;
            }
            let same = match &self.expected[i] {
                Some(then) => ItemIdentity::of(&self.items[i]).as_ref() == Some(then),
                None => false,
            };
            self.checked[i].set(same);
            same
        }
    }

    #[implement(IFileOperationProgressSink)]
    struct Sink {
        /// Read without the state, so the recycle check never depends on it.
        mode: Mode,
        pass: Rc<PassItems>,
        /// What the sink refused in this pass. A Cell, so a refusal is recorded even while
        /// the state is busy.
        refusal: Rc<Cell<Refusal>>,
        state: Rc<RefCell<State>>,
    }

    impl Sink {
        /// The refusal for a delete with `flags` of something in `item` (`None`: in no
        /// item of the pass), if it must be refused.
        fn judge(&self, flags: u32, item: Option<usize>) -> Option<Refusal> {
            if refuses_permanent_delete(self.mode, flags) {
                return Some(item.map_or(Refusal::Unknown, Refusal::Item));
            }
            if self.pass.expected.is_empty() {
                return None;
            }
            // A confirmed delete: everything must belong to an item that is still the
            // object the user was asked about.
            match item {
                None => Some(Refusal::Unknown),
                Some(i) if !self.pass.still_same(i) => Some(Refusal::Changed(i)),
                Some(_) => None,
            }
        }
    }

    #[allow(non_snake_case)]
    impl IFileOperationProgressSink_Impl for Sink_Impl {
        fn StartOperations(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn FinishOperations(&self, _: HRESULT) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreRenameItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostRenameItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreMoveItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostMoveItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreCopyItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostCopyItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreDeleteItem(&self, flags: u32, item: Ref<IShellItem>) -> windows::core::Result<()> {
            let path = path_of(item);
            let at = self.pass.locate(path.as_deref());
            // Decided before the state is touched, so it holds even when the state is busy.
            if let Some(refusal) = self.judge(flags, at.map(|(i, _)| i)) {
                self.refusal.set(self.refusal.get().and(refusal));
                if let Ok(mut s) = self.state.try_borrow_mut() {
                    // A cancel that came at the same time still counts.
                    s.tell();
                }
                return Err(cancel_hr());
            }
            self.pass.mark_inside(at);
            // Progress only from here: best effort when the state is busy.
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return Ok(());
            };
            let top = at.is_some_and(|(_, top)| top);
            // A top-level item recycled goes in one step (a folder with everything in it).
            let changed = s.enter(if self.mode == Mode::Recycle && top {
                DeleteStage::MovingWhole
            } else {
                DeleteStage::Deleting
            });
            s.progress.current = path.map(PathBuf::from);
            if s.tell_if(changed || top) { Ok(()) } else { Err(cancel_hr()) }
        }
        fn PostDeleteItem(
            &self,
            _: u32,
            item: Ref<IShellItem>,
            hr: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            let path = path_of(item);
            let at = self.pass.locate(path.as_deref());
            if hr.is_ok() {
                self.pass.mark_inside(at);
            }
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return Ok(());
            };
            let top = at.is_some_and(|(_, top)| top);
            s.record(at.map(|(i, _)| i), hr, self.refusal.get() != Refusal::None);
            if top && hr.is_ok() && hr.0 as u32 != codes::S_NOT_HANDLED {
                s.progress.items_done += 1;
            }
            if top {
                s.enter(DeleteStage::Deleting);
            }
            if s.tell_if(top) { Ok(()) } else { Err(cancel_hr()) }
        }
        fn PreNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostNewItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: &PCWSTR,
            _: u32,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn UpdateProgress(&self, total: u32, done: u32) -> windows::core::Result<()> {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return Ok(());
            };
            let mut changed = false;
            if total == 0 {
                // While counting, the shell reports once for each file or folder it finds.
                if matches!(s.progress.stage, DeleteStage::Starting | DeleteStage::Counting) {
                    changed = s.enter(DeleteStage::Counting);
                    s.progress.found += 1;
                }
            } else if matches!(s.progress.stage, DeleteStage::Starting | DeleteStage::Counting) {
                changed = s.enter(DeleteStage::Deleting);
            }
            s.progress.work_total = total.into();
            s.progress.work_done = done.into();
            if s.tell_if(changed) { Ok(()) } else { Err(cancel_hr()) }
        }
        fn ResetTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn PauseTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResumeTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
    }

    fn flags(mode: Mode) -> FILEOPERATION_FLAGS {
        // No FOF_WANTNUKEWARNING: the sink refuses permanent deletes itself (see
        // PreDeleteItem), and the shell's question would open its own window.
        let base = FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOFX_SHOWELEVATIONPROMPT;
        match mode {
            Mode::Recycle => base | FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE,
            Mode::Permanent => base,
        }
    }

    fn new_state(observer: DeleteObserver, items_total: usize, done: usize) -> State {
        State {
            errors: Vec::new(),
            stopped: false,
            user_cancelled: false,
            progress: DeleteProgress {
                items_total,
                items_done: done,
                ..Default::default()
            },
            done_before: done,
            observer,
            throttle: Throttle::default(),
        }
    }

    /// Runs the shell over `paths`. `expected` is empty, or holds for each path what it must
    /// still be when the shell reaches it (a confirmed permanent delete).
    pub(super) fn run(
        paths: &[PathBuf],
        expected: &[Option<ItemIdentity>],
        mode: Mode,
        observer: DeleteObserver,
    ) -> DeleteReport {
        let checked = !expected.is_empty();
        let mut report = DeleteReport::default();
        let mut pending: Vec<(PathBuf, Option<ItemIdentity>)> = Vec::new();
        for (i, p) in paths.iter().enumerate() {
            if is_gone(p) {
                report.failed.push((p.clone(), DeleteFailure::Missing));
            } else {
                pending.push((p.clone(), expected.get(i).cloned().flatten()));
            }
        }
        let state = Rc::new(RefCell::new(new_state(
            observer,
            paths.len(),
            report.failed.len(),
        )));
        let _com = match crate::shell::com::ComGuard::apartment_threaded() {
            Ok(g) => g,
            Err(e) => {
                let why = DeleteFailure::Other(e.to_string());
                report
                    .failed
                    .extend(pending.into_iter().map(|(p, _)| (p, why.clone())));
                return report;
            }
        };
        while !pending.is_empty() {
            let items: Vec<PathBuf> = pending.iter().map(|(p, _)| p.clone()).collect();
            let wanted = if checked {
                pending.iter().map(|(_, id)| id.clone()).collect()
            } else {
                Vec::new()
            };
            let pass = Rc::new(PassItems::new(items.clone(), wanted));
            {
                let mut s = state.borrow_mut();
                s.errors = vec![None; items.len()];
                s.progress.items_done = s.done_before;
                s.progress.work_done = 0;
                s.progress.work_total = 0;
                s.progress.stage = DeleteStage::Starting;
                s.progress.found = 0;
            }
            let refusal = Rc::new(Cell::new(Refusal::None));
            let (run, aborted) = unsafe { perform(mode, &pass, &refusal, &state) };
            let (aborted, run_error) = match aborted {
                Ok(aborted) => (aborted, run.err()),
                Err(e) => (false, run.err().or(Some(e))),
            };
            let started: Vec<bool> = pass.started.iter().map(Cell::get).collect();
            let refusal = refusal.get();
            let mut s = state.borrow_mut();
            let gone: Vec<bool> = items.iter().map(|p| is_gone(p)).collect();
            s.progress.items_done = s.done_before + gone.iter().filter(|g| **g).count();
            s.progress.current = None;
            // The last word on cancelling: a cancel at the end of the pass counts too.
            s.tell();
            let cancelled = s.stopped || (s.user_cancelled && refusal == Refusal::None);
            let before = report.done.len() + report.failed.len();
            let retry = settle_pass(
                Pass {
                    items: &items,
                    gone: &gone,
                    refusal,
                    errors: &s.errors,
                    started: &started,
                    cancelled,
                    aborted,
                    run_error,
                },
                |hr| windows::core::Error::from(HRESULT(hr as i32)).message(),
                &mut report,
            );
            s.done_before += report.done.len() + report.failed.len() - before;
            pending = retry.into_iter().map(|i| pending[i].clone()).collect();
        }
        report
    }

    /// One `IFileOperation` pass. Returns how `PerformOperations` (or the setup before it)
    /// ended, and whether the shell reports any operation aborted (or why it could not say).
    unsafe fn perform(
        mode: Mode,
        pass: &Rc<PassItems>,
        refusal: &Rc<Cell<Refusal>>,
        state: &Rc<RefCell<State>>,
    ) -> (Result<(), (u32, String)>, Result<bool, (u32, String)>) {
        let err = |op: &str, e: windows::core::Error| (e.code().0 as u32, format!("{op}: {}", e.message()));
        unsafe {
            let op: IFileOperation = match CoCreateInstance(&FileOperation, None, CLSCTX_ALL) {
                Ok(op) => op,
                Err(e) => return (Err(err("IFileOperation", e)), Ok(false)),
            };
            let setup = (|| {
                op.SetOperationFlags(flags(mode))
                    .map_err(|e| err("SetOperationFlags", e))?;
                let owner = super::DIALOG_OWNER.load(std::sync::atomic::Ordering::Relaxed);
                if owner != 0 {
                    // A stale handle only costs the owner; the dialog still shows.
                    let _ = op.SetOwnerWindow(HWND(owner as *mut _));
                }
                let sink: IFileOperationProgressSink = Sink {
                    mode,
                    pass: pass.clone(),
                    refusal: refusal.clone(),
                    state: state.clone(),
                }
                .into();
                let cookie = op.Advise(&sink).map_err(|e| err("Advise", e))?;
                for p in &pass.items {
                    let wide = crate::to_wide(p);
                    let item: IShellItem =
                        SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)
                            .map_err(|e| err("open item", e))?;
                    op.DeleteItem(&item, None)
                        .map_err(|e| err("DeleteItem", e))?;
                }
                Ok(cookie)
            })();
            let cookie = match setup {
                Ok(cookie) => cookie,
                Err(e) => return (Err(e), Ok(false)),
            };
            let result = op
                .PerformOperations()
                .map_err(|e| (e.code().0 as u32, e.message()));
            // PerformOperations can succeed when the user or the system cancelled; this is
            // the shell's own word on it.
            let aborted = op
                .GetAnyOperationsAborted()
                .map(|b| b.as_bool())
                .map_err(|e| err("GetAnyOperationsAborted", e));
            let _ = op.Unadvise(cookie);
            (result, aborted)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::Path;

        struct Probe {
            sink: IFileOperationProgressSink,
            refusal: Rc<Cell<Refusal>>,
            state: Rc<RefCell<State>>,
        }

        fn probe(mode: Mode, items: &[&Path], expected: Vec<Option<ItemIdentity>>) -> Probe {
            probe_with(mode, items, expected, Box::new(|_| true))
        }

        fn probe_with(
            mode: Mode,
            items: &[&Path],
            expected: Vec<Option<ItemIdentity>>,
            observer: DeleteObserver,
        ) -> Probe {
            let mut state = new_state(observer, items.len(), 0);
            state.errors = vec![None; items.len()];
            let state = Rc::new(RefCell::new(state));
            let refusal = Rc::new(Cell::new(Refusal::None));
            let pass = Rc::new(PassItems::new(
                items.iter().map(|p| p.to_path_buf()).collect(),
                expected,
            ));
            let sink = Sink {
                mode,
                pass,
                refusal: refusal.clone(),
                state: state.clone(),
            }
            .into();
            Probe {
                sink,
                refusal,
                state,
            }
        }

        fn shell_item(path: &Path) -> IShellItem {
            let wide = crate::to_wide(path);
            unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).unwrap() }
        }

        const RECYCLE: u32 = 0x282;
        const DESTROY: u32 = 0x202;

        #[test]
        fn a_permanent_delete_is_refused_even_while_the_state_is_busy() {
            let _com = crate::shell::com::ComGuard::apartment_threaded().unwrap();
            let d = tempfile::tempdir().unwrap();
            let f = d.path().join("a.txt");
            std::fs::write(&f, b"x").unwrap();
            let p = probe(Mode::Recycle, &[&f], Vec::new());
            let item = shell_item(&f);
            let busy = p.state.borrow_mut();
            assert!(unsafe { p.sink.PreDeleteItem(DESTROY, &item) }.is_err());
            assert_eq!(p.refusal.get(), Refusal::Item(0));
            // A recycle while busy goes through: its check needs no state.
            assert!(unsafe { p.sink.PreDeleteItem(RECYCLE, &item) }.is_ok());
            drop(busy);
        }

        #[test]
        fn a_refusal_names_the_item_it_belongs_to() {
            let _com = crate::shell::com::ComGuard::apartment_threaded().unwrap();
            let d = tempfile::tempdir().unwrap();
            let (a, b) = (d.path().join("a"), d.path().join("b"));
            std::fs::create_dir_all(b.join("sub")).unwrap();
            std::fs::create_dir_all(&a).unwrap();
            std::fs::write(b.join("sub").join("f.txt"), b"x").unwrap();
            let p = probe(Mode::Recycle, &[&a, &b], Vec::new());
            let child = shell_item(&b.join("sub").join("f.txt"));
            assert!(unsafe { p.sink.PreDeleteItem(DESTROY, &child) }.is_err());
            assert_eq!(p.refusal.get(), Refusal::Item(1));
        }

        #[test]
        fn a_refusal_of_an_item_outside_the_pass_is_unknown_not_item_zero() {
            let _com = crate::shell::com::ComGuard::apartment_threaded().unwrap();
            let d = tempfile::tempdir().unwrap();
            let (a, other) = (d.path().join("a.txt"), d.path().join("other.txt"));
            std::fs::write(&a, b"x").unwrap();
            std::fs::write(&other, b"x").unwrap();
            let p = probe(Mode::Recycle, &[&a], Vec::new());
            assert!(unsafe { p.sink.PreDeleteItem(DESTROY, &shell_item(&other)) }.is_err());
            assert_eq!(p.refusal.get(), Refusal::Unknown);
        }

        /// What the observer heard: the stages in order and how many reports came.
        fn listener() -> (DeleteObserver, Rc<RefCell<Vec<DeleteProgress>>>) {
            let heard = Rc::new(RefCell::new(Vec::new()));
            let log = heard.clone();
            let observer: DeleteObserver = Box::new(move |p| {
                log.borrow_mut().push(p.clone());
                true
            });
            (observer, heard)
        }

        #[test]
        fn counting_inside_a_folder_is_throttled_and_every_stage_is_reported() {
            let _com = crate::shell::com::ComGuard::apartment_threaded().unwrap();
            let d = tempfile::tempdir().unwrap();
            let folder = d.path().join("big");
            std::fs::create_dir(&folder).unwrap();
            let (observer, heard) = listener();
            let p = probe_with(Mode::Recycle, &[&folder], Vec::new(), observer);
            let started = std::time::Instant::now();
            // The shell reports each file it finds while counting.
            for _ in 0..5_000 {
                unsafe { p.sink.UpdateProgress(0, 0) }.unwrap();
            }
            let counting_reports = heard.borrow().len() as u128;
            let bound = 2 + started.elapsed().as_millis() / super::super::TELL_EVERY.as_millis();
            assert!(
                counting_reports <= bound,
                "{counting_reports} reports for 5000 files (at most {bound})"
            );
            assert_eq!(p.state.borrow().progress.found, 5_000);
            unsafe { p.sink.UpdateProgress(5_000, 0) }.unwrap();
            let item = shell_item(&folder);
            unsafe { p.sink.PreDeleteItem(RECYCLE, &item) }.unwrap();
            unsafe { p.sink.PostDeleteItem(RECYCLE, &item, HRESULT(0), &item) }.unwrap();
            let heard = heard.borrow();
            let mut stages: Vec<DeleteStage> = heard.iter().map(|p| p.stage).collect();
            stages.dedup();
            assert_eq!(
                stages,
                [
                    DeleteStage::Counting,
                    DeleteStage::Deleting,
                    DeleteStage::MovingWhole,
                    DeleteStage::Deleting
                ]
            );
            let last = heard.last().unwrap();
            assert_eq!((last.items_done, last.work_total), (1, 5_000));
        }

        #[test]
        fn files_inside_a_permanently_deleted_folder_are_reported_at_most_20_times_a_second() {
            let _com = crate::shell::com::ComGuard::apartment_threaded().unwrap();
            let d = tempfile::tempdir().unwrap();
            let folder = d.path().join("tree");
            std::fs::create_dir(&folder).unwrap();
            let file = folder.join("a.txt");
            std::fs::write(&file, b"x").unwrap();
            let (observer, heard) = listener();
            let p = probe_with(Mode::Permanent, &[&folder], Vec::new(), observer);
            let inner = shell_item(&file);
            let started = std::time::Instant::now();
            for done in 0..2_000 {
                unsafe { p.sink.PreDeleteItem(DESTROY, &inner) }.unwrap();
                unsafe { p.sink.PostDeleteItem(DESTROY, &inner, HRESULT(0), &inner) }.unwrap();
                unsafe { p.sink.UpdateProgress(2_000, done + 1) }.unwrap();
            }
            let reports = heard.borrow().len() as u128;
            // The first report is the stage change to Deleting.
            let bound = 2 + started.elapsed().as_millis() / super::super::TELL_EVERY.as_millis();
            assert!(reports <= bound, "{reports} reports for 2000 files (at most {bound})");
            let s = p.state.borrow();
            assert_eq!((s.progress.work_done, s.progress.work_total), (2_000, 2_000));
            assert_eq!(s.progress.current.as_deref(), Some(file.as_path()));
            // Inside an item, nothing counts as a finished top-level item.
            assert_eq!(s.progress.items_done, 0);
        }

        #[test]
        fn a_permanent_delete_is_never_refused() {
            let _com = crate::shell::com::ComGuard::apartment_threaded().unwrap();
            let d = tempfile::tempdir().unwrap();
            let f = d.path().join("a.txt");
            std::fs::write(&f, b"x").unwrap();
            let p = probe(Mode::Permanent, &[&f], Vec::new());
            assert!(unsafe { p.sink.PreDeleteItem(2, &shell_item(&f)) }.is_ok());
            assert_eq!(p.refusal.get(), Refusal::None);
        }

        #[test]
        fn a_cancel_at_the_moment_of_a_refusal_counts() {
            let _com = crate::shell::com::ComGuard::apartment_threaded().unwrap();
            let d = tempfile::tempdir().unwrap();
            let f = d.path().join("a.txt");
            std::fs::write(&f, b"x").unwrap();
            let p = probe(Mode::Recycle, &[&f], Vec::new());
            p.state.borrow_mut().observer = Box::new(|_| false);
            assert!(unsafe { p.sink.PreDeleteItem(DESTROY, &shell_item(&f)) }.is_err());
            assert!(p.state.borrow().stopped);
        }

        #[test]
        fn a_confirmed_item_swapped_before_the_shell_reaches_it_is_refused() {
            let _com = crate::shell::com::ComGuard::apartment_threaded().unwrap();
            let d = tempfile::tempdir().unwrap();
            let (kept, swapped) = (d.path().join("kept"), d.path().join("swapped.txt"));
            std::fs::create_dir_all(&kept).unwrap();
            std::fs::write(kept.join("inner.txt"), b"x").unwrap();
            std::fs::write(&swapped, b"same size").unwrap();
            let expected = vec![ItemIdentity::of(&kept), ItemIdentity::of(&swapped)];
            // Replaced by another file of the same size and name after the question.
            std::fs::remove_file(&swapped).unwrap();
            std::fs::write(&swapped, b"same size").unwrap();
            let p = probe(Mode::Permanent, &[&kept, &swapped], expected);
            // The unchanged folder and what is inside it go through.
            assert!(unsafe { p.sink.PreDeleteItem(2, &shell_item(&kept)) }.is_ok());
            let inner = shell_item(&kept.join("inner.txt"));
            assert!(unsafe { p.sink.PreDeleteItem(2, &inner) }.is_ok());
            assert!(unsafe { p.sink.PreDeleteItem(2, &shell_item(&swapped)) }.is_err());
            assert_eq!(p.refusal.get(), Refusal::Changed(1));
        }

        #[test]
        fn a_confirmed_delete_refuses_what_it_cannot_tie_to_an_item() {
            let _com = crate::shell::com::ComGuard::apartment_threaded().unwrap();
            let d = tempfile::tempdir().unwrap();
            let (a, other) = (d.path().join("a.txt"), d.path().join("other.txt"));
            std::fs::write(&a, b"x").unwrap();
            std::fs::write(&other, b"x").unwrap();
            let p = probe(Mode::Permanent, &[&a], vec![ItemIdentity::of(&a)]);
            assert!(unsafe { p.sink.PreDeleteItem(2, &shell_item(&other)) }.is_err());
            assert_eq!(p.refusal.get(), Refusal::Unknown);
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn quiet() -> DeleteObserver {
        Box::new(|_| true)
    }

    /// A sparse file far bigger than any Recycle Bin. It takes no disk space.
    fn huge_sparse_file(path: &Path) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::IO::DeviceIoControl;
        use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;
        let f = std::fs::File::create(path).unwrap();
        let mut ret = 0u32;
        unsafe {
            DeviceIoControl(
                HANDLE(f.as_raw_handle()),
                FSCTL_SET_SPARSE,
                None,
                0,
                None,
                0,
                Some(&mut ret),
                None,
            )
            .unwrap();
        }
        f.set_len(8 << 40).unwrap();
    }

    #[test]
    fn folders_too_big_for_the_recycle_bin_are_kept_and_reported() {
        let d = tempfile::tempdir().unwrap();
        let make = |name: &str| {
            let big = d.path().join(name);
            std::fs::create_dir(&big).unwrap();
            std::fs::write(big.join("a.txt"), b"x").unwrap();
            huge_sparse_file(&big.join("huge.bin"));
            big
        };
        let (one, two) = (make("one"), make("two"));
        // Every item is refused, so the real Recycle Bin is never touched. The shell stops
        // at the first refusal; the second item gets a pass of its own.
        let report = run_shell(&[one.clone(), two.clone()], &[], Mode::Recycle, quiet());
        assert_eq!(
            report.failed,
            vec![
                (one.clone(), DeleteFailure::NotRecyclable),
                (two.clone(), DeleteFailure::NotRecyclable)
            ]
        );
        assert!(report.done.is_empty() && !report.cancelled);
        for big in [one, two] {
            assert!(big.join("a.txt").is_file() && big.join("huge.bin").is_file());
        }
    }

    #[test]
    fn a_missing_item_is_reported_without_touching_the_others() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("nope.txt");
        let report = run_shell(std::slice::from_ref(&missing), &[], Mode::Recycle, quiet());
        assert_eq!(report.failed, vec![(missing, DeleteFailure::Missing)]);
    }

    #[test]
    fn permanent_shell_delete_reports_progress_and_removes_the_tree() {
        let d = tempfile::tempdir().unwrap();
        let tree = d.path().join("tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        for n in ["a", "b", "sub/c"] {
            std::fs::write(tree.join(n), b"x").unwrap();
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let report = delete_items_permanently(
            std::slice::from_ref(&tree),
            Box::new(move |p| {
                log.lock().unwrap().push(p.clone());
                true
            }),
        );
        assert_eq!(report.done, vec![tree.clone()]);
        assert!(report.failed.is_empty());
        assert!(!tree.exists());
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().any(|p| p.work_total > 0),
            "no work units reported"
        );
        assert!(
            seen.iter().any(|p| p.current.is_some()),
            "no current item reported"
        );
        assert_eq!(seen.last().unwrap().items_done, 1, "{seen:#?}");
    }

    #[test]
    fn a_cancelled_permanent_delete_leaves_the_items() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        let b = d.path().join("b.txt");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        let report = delete_items_permanently(&[a.clone(), b.clone()], Box::new(|_| false));
        assert!(report.cancelled);
        assert!(a.exists() && b.exists());
        assert_eq!(report.not_started.len(), 2);
    }

    #[test]
    fn an_open_file_is_reported_in_use() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("locked.txt");
        std::fs::write(&f, b"x").unwrap();
        // Share read only: nobody may delete or rename it while it is open.
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&f)
            .unwrap();
        let report = delete_items_permanently(std::slice::from_ref(&f), quiet());
        assert_eq!(report.failed, vec![(f.clone(), DeleteFailure::InUse)]);
        assert!(f.exists());
    }

    #[test]
    fn the_sandbox_refuses_marked_items_and_recycles_the_rest() {
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("bin");
        let keep = d
            .path()
            .join(format!("{SANDBOX_NOT_RECYCLABLE_PREFIX} big"));
        let go = d.path().join("go.txt");
        std::fs::write(&keep, b"x").unwrap();
        std::fs::write(&go, b"x").unwrap();
        let report = recycle_into_sandbox(&bin, &[keep.clone(), go.clone()], quiet());
        assert_eq!(
            report.failed,
            vec![(keep.clone(), DeleteFailure::NotRecyclable)]
        );
        assert_eq!(report.done, vec![go.clone()]);
        assert!(keep.exists() && !go.exists());
    }

    #[test]
    fn shell_errors_are_classified() {
        let other = || "x".to_string();
        assert_eq!(
            codes::classify(codes::E_REQUIRES_ELEVATION, other),
            DeleteFailure::NeedsAdmin
        );
        assert_eq!(
            codes::classify(codes::E_ACCESS_DENIED_SRC, other),
            DeleteFailure::NeedsAdmin
        );
        assert_eq!(
            codes::classify(codes::E_SHARING_VIOLATION_DEST, other),
            DeleteFailure::InUse
        );
        assert_eq!(
            codes::classify(codes::E_RECYCLE_SIZE_TOO_BIG, other),
            DeleteFailure::NotRecyclable
        );
        assert_eq!(
            codes::classify(0x8000_4005, other),
            DeleteFailure::Other("x".into())
        );
    }
    #[test]
    fn only_a_recycle_refuses_and_only_without_the_recycle_flag() {
        assert!(refuses_permanent_delete(Mode::Recycle, 0x202));
        assert!(refuses_permanent_delete(Mode::Recycle, 0x2));
        assert!(!refuses_permanent_delete(Mode::Recycle, 0x282));
        assert!(!refuses_permanent_delete(Mode::Permanent, 0x2));
    }

    #[test]
    fn the_throttle_lets_one_report_through_per_interval_and_every_forced_one() {
        let mut t = Throttle::default();
        let t0 = Instant::now();
        assert!(t.due(t0, false), "the first report always goes through");
        assert!(!t.due(t0 + Duration::from_millis(10), false));
        assert!(!t.due(t0 + TELL_EVERY - Duration::from_millis(1), false));
        assert!(t.due(t0 + Duration::from_millis(5), true), "forced");
        // A forced report restarts the interval.
        assert!(!t.due(t0 + TELL_EVERY, false));
        assert!(t.due(t0 + Duration::from_millis(5) + TELL_EVERY, false));
        // A clock that goes backwards never lets reports through faster.
        assert!(!t.due(t0, false));
    }

    #[test]
    fn a_permanent_delete_reports_files_inside_the_folder_with_a_total() {
        let d = tempfile::tempdir().unwrap();
        let tree = d.path().join("tree");
        for i in 0..400 {
            let dir = tree.join(format!("d{}", i / 100));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("f{i}.txt")), b"x").unwrap();
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let report = delete_items_permanently(
            std::slice::from_ref(&tree),
            Box::new(move |p| {
                log.lock().unwrap().push(p.clone());
                true
            }),
        );
        assert_eq!(report.done, vec![tree.clone()]);
        let seen = seen.lock().unwrap();
        // The total counts the files and folders inside, not the one top-level item.
        let total = seen.iter().map(|p| p.work_total).max().unwrap();
        assert!(total >= 400, "work total {total}");
        assert!(
            seen.iter()
                .filter_map(|p| p.current.as_deref())
                .any(|c| c.starts_with(&tree) && c != tree),
            "no file inside the folder was reported"
        );
        assert!(seen.len() < 400, "{} reports for 400 files", seen.len());
        let last = seen.last().unwrap();
        assert_eq!((last.items_done, last.items_total), (1, 1));
    }

    #[test]
    fn shell_paths_map_to_the_item_they_are_in() {
        let keys = vec![r"c:\a\big".to_string(), r"c:\a\big two".to_string(), r"c:\a\big\in".to_string()];
        assert_eq!(item_of(&keys, r"C:\A\Big"), Some(0));
        assert_eq!(item_of(&keys, r"C:\a\big\x\y.bin"), Some(0));
        assert_eq!(item_of(&keys, r"C:\a\big two\y.bin"), Some(1));
        // The deepest item wins when items nest.
        assert_eq!(item_of(&keys, r"C:\a\big\in\z"), Some(2));
        assert_eq!(item_of(&keys, r"C:\a\bigger"), None);
        assert_eq!(item_of(&keys, r"D:\elsewhere"), None);
    }

    fn settle_with(
        items: &[PathBuf],
        gone: &[bool],
        refusal: Refusal,
        cancelled: bool,
        aborted: bool,
        errors: &[Option<u32>],
    ) -> (DeleteReport, Vec<PathBuf>) {
        settle_full(items, gone, refusal, cancelled, aborted, errors, &vec![false; items.len()], None)
    }

    #[allow(clippy::too_many_arguments)]
    fn settle_full(
        items: &[PathBuf],
        gone: &[bool],
        refusal: Refusal,
        cancelled: bool,
        aborted: bool,
        errors: &[Option<u32>],
        started: &[bool],
        run_error: Option<(u32, String)>,
    ) -> (DeleteReport, Vec<PathBuf>) {
        let mut report = DeleteReport::default();
        let retry = settle_pass(
            Pass {
                items,
                gone,
                refusal,
                errors,
                started,
                cancelled,
                aborted,
                run_error,
            },
            |_| String::new(),
            &mut report,
        );
        let retry = retry.into_iter().map(|i| items[i].clone()).collect();
        (report, retry)
    }

    fn settle(
        items: &[PathBuf],
        gone: &[bool],
        refusal: Refusal,
        cancelled: bool,
    ) -> (DeleteReport, Vec<PathBuf>) {
        settle_with(items, gone, refusal, cancelled, false, &vec![None; items.len()])
    }

    #[test]
    fn a_known_refusal_offers_that_item_and_retries_the_rest() {
        let items: Vec<PathBuf> = ["a", "b", "c"].iter().map(PathBuf::from).collect();
        let (report, retry) = settle(&items, &[true, false, false], Refusal::Item(1), false);
        assert_eq!(report.done, vec![items[0].clone()]);
        assert_eq!(report.failed, vec![(items[1].clone(), DeleteFailure::NotRecyclable)]);
        assert_eq!(retry, vec![items[2].clone()]);
    }

    #[test]
    fn an_unknown_refusal_offers_nothing_and_retries_nothing() {
        let items: Vec<PathBuf> = ["a", "b"].iter().map(PathBuf::from).collect();
        let (report, retry) = settle(&items, &[false, false], Refusal::Unknown, false);
        assert!(retry.is_empty());
        assert_eq!(report.failed.len(), 2);
        assert!(
            report
                .failed
                .iter()
                .all(|(_, why)| matches!(why, DeleteFailure::Other(m) if m == UNKNOWN_REFUSAL)),
            "{:?}",
            report.failed
        );
    }

    #[test]
    fn a_cancel_at_a_refusal_stops_without_retrying() {
        let items: Vec<PathBuf> = ["a", "b"].iter().map(PathBuf::from).collect();
        let (report, retry) = settle(&items, &[false, false], Refusal::Item(0), true);
        assert!(report.cancelled && retry.is_empty());
        assert_eq!(report.failed, vec![(items[0].clone(), DeleteFailure::NotRecyclable)]);
        assert_eq!(report.not_started, vec![items[1].clone()]);
    }

    #[test]
    fn a_cancel_after_the_last_sandbox_item_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let big = d.path().join(format!("{SANDBOX_NOT_RECYCLABLE_PREFIX}-big"));
        std::fs::write(&big, b"x").unwrap();
        let calls = std::cell::Cell::new(0);
        let calls = std::rc::Rc::new(calls);
        let seen = calls.clone();
        let report = recycle_into_sandbox(
            &d.path().join("bin"),
            std::slice::from_ref(&big),
            Box::new(move |_| {
                seen.set(seen.get() + 1);
                seen.get() < 2
            }),
        );
        assert!(report.cancelled, "the cancel after the refusal was lost");
        assert_eq!(report.failed, vec![(big, DeleteFailure::NotRecyclable)]);
    }

    #[test]
    fn a_changed_item_is_reported_and_the_rest_retried() {
        let items: Vec<PathBuf> = ["a", "b", "c"].iter().map(PathBuf::from).collect();
        let (report, retry) = settle(&items, &[true, false, false], Refusal::Changed(1), false);
        assert_eq!(report.failed, vec![(items[1].clone(), DeleteFailure::Changed)]);
        assert_eq!(retry, vec![items[2].clone()]);
    }

    #[test]
    fn an_aborted_pass_with_unexplained_items_is_a_cancel() {
        let items: Vec<PathBuf> = ["a", "b"].iter().map(PathBuf::from).collect();
        // PerformOperations returned S_OK, but the shell says it aborted: b is left with
        // nothing to explain it, so the user or the system cancelled.
        let (report, retry) =
            settle_with(&items, &[true, false], Refusal::None, false, true, &[None, None]);
        assert!(report.cancelled && retry.is_empty());
        assert_eq!(report.not_started, vec![items[1].clone()]);
        assert!(report.failed.is_empty());
    }

    #[test]
    fn an_aborted_pass_explained_by_errors_is_not_a_cancel() {
        let items: Vec<PathBuf> = ["a"].iter().map(PathBuf::from).collect();
        let (report, _) = settle_with(
            &items,
            &[false],
            Refusal::None,
            false,
            true,
            &[Some(codes::E_ACCESS_DENIED_SRC)],
        );
        assert!(!report.cancelled);
        assert_eq!(report.failed, vec![(items[0].clone(), DeleteFailure::NeedsAdmin)]);
    }

    #[test]
    fn an_abort_caused_by_our_own_refusal_is_not_a_cancel() {
        let items: Vec<PathBuf> = ["a", "b"].iter().map(PathBuf::from).collect();
        let (report, retry) =
            settle_with(&items, &[false, false], Refusal::Item(0), false, true, &[None, None]);
        assert!(!report.cancelled);
        assert_eq!(retry, vec![items[1].clone()]);
    }

    fn identity(id: Option<(u32, u64)>, created: u64, file: Option<(u64, u64)>) -> ItemIdentity {
        let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s);
        ItemIdentity {
            dir: file.is_none(),
            id,
            created: t(created),
            file: file.map(|(len, modified)| (len, t(modified))),
        }
    }

    #[test]
    fn identities_without_a_file_id_must_match_on_every_part() {
        let then = identity(None, 100, Some((9, 200)));
        assert_eq!(then, identity(None, 100, Some((9, 200))));
        // Same size and write time (FAT's 2 s resolution), another creation time.
        assert_ne!(then, identity(None, 101, Some((9, 200))));
        assert_ne!(then, identity(None, 100, Some((9, 202))));
        assert_ne!(then, identity(None, 100, Some((8, 200))));
        // A file ID on one side only never matches.
        assert_ne!(then, identity(Some((1, 5)), 100, Some((9, 200))));
        assert_ne!(identity(Some((1, 5)), 100, None), identity(Some((1, 6)), 100, None));
    }

    #[test]
    fn a_file_replaced_by_one_of_the_same_size_has_another_identity() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"12345").unwrap();
        let then = ItemIdentity::of(&f).unwrap();
        assert_eq!(ItemIdentity::of(&f), Some(then.clone()));
        std::fs::remove_file(&f).unwrap();
        std::fs::write(&f, b"12345").unwrap();
        assert_ne!(ItemIdentity::of(&f), Some(then));
        assert_eq!(ItemIdentity::of(&d.path().join("missing")), None);
    }

    #[test]
    fn a_confirmed_delete_skips_a_swapped_item_and_deletes_the_rest() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (d.path().join("a.txt"), d.path().join("b.txt"), d.path().join("c.txt"));
        for f in [&a, &b, &c] {
            std::fs::write(f, b"same").unwrap();
        }
        let items: Vec<(PathBuf, ItemIdentity)> = [&a, &b, &c]
            .iter()
            .map(|p| (p.to_path_buf(), ItemIdentity::of(p).unwrap()))
            .collect();
        std::fs::remove_file(&b).unwrap();
        std::fs::write(&b, b"same").unwrap();
        let report = delete_confirmed_items(&items, quiet());
        assert_eq!(report.failed, vec![(b.clone(), DeleteFailure::Changed)]);
        assert_eq!(report.done, vec![a.clone(), c.clone()]);
        assert!(!a.exists() && b.exists() && !c.exists());
        assert!(!report.cancelled);
    }

    #[test]
    fn an_abort_with_an_item_error_elsewhere_in_the_pass_is_not_a_cancel() {
        let items: Vec<PathBuf> = ["a", "b"].iter().map(PathBuf::from).collect();
        // a failed with an access error; b was never reached. The abort is the error's.
        let (report, _) = settle_with(
            &items,
            &[false, false],
            Refusal::None,
            false,
            true,
            &[Some(codes::E_ACCESS_DENIED_SRC), None],
        );
        assert!(!report.cancelled);
        assert_eq!(report.failed.len(), 2);
        assert!(report.not_started.is_empty());
    }

    #[test]
    fn a_failed_abort_query_is_an_error_not_a_cancel() {
        let items: Vec<PathBuf> = ["a"].iter().map(PathBuf::from).collect();
        let query = (0x8000_4005, "GetAnyOperationsAborted: failed".to_string());
        let (report, _) = settle_full(
            &items,
            &[false],
            Refusal::None,
            false,
            false,
            &[None],
            &[false],
            Some(query),
        );
        assert!(!report.cancelled);
        assert_eq!(
            report.failed,
            vec![(items[0].clone(), DeleteFailure::Other("GetAnyOperationsAborted: failed".into()))]
        );
    }

    #[test]
    fn a_cancel_reports_items_the_shell_had_started_on_as_partly_deleted() {
        let items: Vec<PathBuf> = ["a", "b", "c"].iter().map(PathBuf::from).collect();
        let (report, _) = settle_full(
            &items,
            &[true, false, false],
            Refusal::None,
            true,
            false,
            &[None, None, None],
            &[true, true, false],
            None,
        );
        assert!(report.cancelled);
        assert_eq!(report.done, vec![items[0].clone()]);
        assert_eq!(report.partly_deleted, vec![items[1].clone()]);
        assert_eq!(report.not_started, vec![items[2].clone()]);
    }

    #[test]
    fn a_cancelled_folder_delete_with_files_gone_is_partly_deleted() {
        let d = tempfile::tempdir().unwrap();
        let tree = d.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        for i in 0..20 {
            std::fs::write(tree.join(format!("f{i:02}.txt")), b"x").unwrap();
        }
        // Stop once the shell has deleted a few files inside the folder. The observer is
        // slower than the throttle's interval, so it hears about every file.
        let report = delete_items_permanently(
            std::slice::from_ref(&tree),
            Box::new(|p| {
                std::thread::sleep(TELL_EVERY + Duration::from_millis(5));
                p.work_done < 4
            }),
        );
        assert!(report.cancelled);
        assert!(tree.exists());
        assert_eq!(report.partly_deleted, vec![tree.clone()], "{report:?}");
    }
}
