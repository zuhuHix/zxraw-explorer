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
    /// The caller or the user (in a shell dialog) cancelled.
    pub cancelled: bool,
}

/// Progress passed to the observer of [`recycle_items`] and [`delete_items_permanently`].
#[derive(Debug, Clone, Default)]
pub struct DeleteProgress {
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

/// Called with progress; returning `false` cancels the operation.
pub type DeleteObserver = Box<dyn FnMut(&DeleteProgress) -> bool>;

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
        None => run_shell(paths, Mode::Recycle, observer),
    }
}

/// Deletes `paths` permanently through the shell, which can ask for administrator
/// permission for protected folders (unlike an in-process delete).
pub fn delete_items_permanently(paths: &[PathBuf], observer: DeleteObserver) -> DeleteReport {
    run_shell(paths, Mode::Permanent, observer)
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
        observer(&progress);
    }
    report
}

/// Whether `path` is certainly gone. Any error other than "not found" counts as present.
fn is_gone(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(windows)]
fn run_shell(paths: &[PathBuf], mode: Mode, observer: DeleteObserver) -> DeleteReport {
    imp::run(paths, mode, observer)
}

#[cfg(not(windows))]
fn run_shell(paths: &[PathBuf], _mode: Mode, _observer: DeleteObserver) -> DeleteReport {
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

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        FILEOPERATION_FLAGS, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
        FOFX_RECYCLEONDELETE, FOFX_SHOWELEVATIONPROMPT, FileOperation, IFileOperation,
        IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
        SHCreateItemFromParsingName, SIGDN_FILESYSPATH, TSF_DELETE_RECYCLE_IF_POSSIBLE,
    };
    use windows::core::{HRESULT, PCWSTR, Ref, implement};

    use super::{
        DeleteFailure, DeleteObserver, DeleteProgress, DeleteReport, Mode, codes, is_gone,
    };

    struct State {
        mode: Mode,
        /// The items of the current pass, and a lowercase key of each for matching the
        /// shell's paths.
        items: Vec<PathBuf>,
        keys: Vec<String>,
        /// The item the shell is working on (children are reported under it).
        current: Option<usize>,
        /// The first error the shell reported for each item (a cancel only when nothing
        /// more specific came).
        errors: Vec<Option<u32>>,
        /// The item whose permanent delete the sink refused.
        refused: Option<usize>,
        /// The observer asked to stop.
        stopped: bool,
        /// The user cancelled in a shell dialog.
        user_cancelled: bool,
        progress: DeleteProgress,
        /// Items finished in earlier passes.
        done_before: usize,
        observer: DeleteObserver,
    }

    impl State {
        fn locate(&mut self, path: &str) -> Option<usize> {
            let key = path.to_lowercase();
            if let Some(i) = self.keys.iter().position(|k| *k == key) {
                self.current = Some(i);
            }
            self.current
        }

        fn record(&mut self, item: Option<usize>, hr: HRESULT) {
            let Some(i) = item else { return };
            let hr = hr.0 as u32;
            if hr & 0x8000_0000 == 0 {
                return;
            }
            if codes::is_cancel(hr) && self.refused != Some(i) && !self.stopped {
                self.user_cancelled = true;
            }
            let slot = &mut self.errors[i];
            match slot {
                None => *slot = Some(hr),
                Some(old) if codes::is_cancel(*old) && !codes::is_cancel(hr) => *slot = Some(hr),
                _ => {}
            }
        }

        /// Tells the observer; false when it asked to stop.
        fn tell(&mut self) -> bool {
            if self.stopped {
                return false;
            }
            if !(self.observer)(&self.progress) {
                self.stopped = true;
            }
            !self.stopped
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

    #[implement(IFileOperationProgressSink)]
    struct Sink(Rc<RefCell<State>>);

    impl Sink {
        /// Runs `f` on the state unless it is already borrowed (a reentrant call during a
        /// shell dialog), in which case the call is let through.
        fn with(
            &self,
            f: impl FnOnce(&mut State) -> windows::core::Result<()>,
        ) -> windows::core::Result<()> {
            match self.0.try_borrow_mut() {
                Ok(mut s) => f(&mut s),
                Err(_) => Ok(()),
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
            self.with(|s| {
                let i = path.as_deref().and_then(|p| s.locate(p));
                // The shell drops the recycle flag for an item it is about to destroy (too
                // big for the Recycle Bin, or no Recycle Bin on its drive). Refuse that; the
                // whole pass stops and the items after it are tried again in a new one.
                if s.mode == Mode::Recycle && flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
                    s.refused = i.or(Some(0));
                    return Err(cancel_hr());
                }
                s.progress.current = path.map(PathBuf::from);
                if s.tell() { Ok(()) } else { Err(cancel_hr()) }
            })
        }
        fn PostDeleteItem(
            &self,
            _: u32,
            item: Ref<IShellItem>,
            hr: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            let path = path_of(item);
            self.with(|s| {
                let key = path.as_deref().map(str::to_lowercase);
                let top = key.and_then(|k| s.keys.iter().position(|x| *x == k));
                let i = top.or(s.current);
                s.record(i, hr);
                if top.is_some() && hr.is_ok() && hr.0 as u32 != codes::S_NOT_HANDLED {
                    s.progress.items_done += 1;
                }
                if s.tell() { Ok(()) } else { Err(cancel_hr()) }
            })
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
            self.with(|s| {
                s.progress.work_total = total.into();
                s.progress.work_done = done.into();
                if s.tell() { Ok(()) } else { Err(cancel_hr()) }
            })
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

    pub(super) fn run(paths: &[PathBuf], mode: Mode, observer: DeleteObserver) -> DeleteReport {
        let mut report = DeleteReport::default();
        let mut pending = Vec::new();
        for p in paths {
            if is_gone(p) {
                report.failed.push((p.clone(), DeleteFailure::Missing));
            } else {
                pending.push(p.clone());
            }
        }
        let state = Rc::new(RefCell::new(State {
            mode,
            items: Vec::new(),
            keys: Vec::new(),
            current: None,
            errors: Vec::new(),
            refused: None,
            stopped: false,
            user_cancelled: false,
            progress: DeleteProgress {
                items_total: paths.len(),
                items_done: report.failed.len(),
                ..Default::default()
            },
            done_before: report.failed.len(),
            observer,
        }));
        let _com = match crate::shell::com::ComGuard::apartment_threaded() {
            Ok(g) => g,
            Err(e) => {
                let why = DeleteFailure::Other(e.to_string());
                report
                    .failed
                    .extend(pending.into_iter().map(|p| (p, why.clone())));
                return report;
            }
        };
        while !pending.is_empty() {
            {
                let mut s = state.borrow_mut();
                s.keys = pending
                    .iter()
                    .map(|p| p.to_string_lossy().to_lowercase())
                    .collect();
                s.errors = vec![None; pending.len()];
                s.items = pending.clone();
                s.current = None;
                s.refused = None;
                s.progress.items_done = s.done_before;
                s.progress.work_done = 0;
                s.progress.work_total = 0;
            }
            let run = unsafe { perform(&pending, mode, &state) };
            let mut s = state.borrow_mut();
            let mut left = Vec::new();
            for (i, p) in pending.iter().enumerate() {
                if is_gone(p) {
                    report.done.push(p.clone());
                    s.done_before += 1;
                } else if s.refused == Some(i) {
                    report
                        .failed
                        .push((p.clone(), DeleteFailure::NotRecyclable));
                    s.done_before += 1;
                } else if let Some(hr) = s.errors[i].filter(|hr| !codes::is_cancel(*hr)) {
                    let why = codes::classify(hr, || {
                        windows::core::Error::from(HRESULT(hr as i32)).message()
                    });
                    report.failed.push((p.clone(), why));
                    s.done_before += 1;
                } else {
                    left.push(p.clone());
                }
            }
            // The shell's per-item reports are not reliable for counting; the disk is.
            s.progress.items_done = s.done_before;
            s.progress.current = None;
            if !s.stopped {
                let progress = s.progress.clone();
                (s.observer)(&progress);
            }
            if s.stopped || s.user_cancelled {
                report.cancelled = true;
                report.not_started = left;
                break;
            }
            if s.refused.is_some() {
                // The pass stopped at the refused item; the rest get a pass of their own.
                pending = left;
                continue;
            }
            let why = match run {
                Err(e) => DeleteFailure::Other(e),
                Ok(()) => DeleteFailure::Other("Windows did not delete it".into()),
            };
            report
                .failed
                .extend(left.into_iter().map(|p| (p, why.clone())));
            break;
        }
        report
    }

    /// One `IFileOperation` pass over `items`. Errors setting it up are returned as text.
    unsafe fn perform(
        items: &[PathBuf],
        mode: Mode,
        state: &Rc<RefCell<State>>,
    ) -> Result<(), String> {
        let err = |op: &str, e: windows::core::Error| format!("{op}: {}", e.message());
        unsafe {
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)
                .map_err(|e| err("IFileOperation", e))?;
            op.SetOperationFlags(flags(mode))
                .map_err(|e| err("SetOperationFlags", e))?;
            let owner = super::DIALOG_OWNER.load(std::sync::atomic::Ordering::Relaxed);
            if owner != 0 {
                // A stale handle only costs the owner; the dialog still shows.
                let _ = op.SetOwnerWindow(HWND(owner as *mut _));
            }
            let sink: IFileOperationProgressSink = Sink(state.clone()).into();
            let cookie = op.Advise(&sink).map_err(|e| err("Advise", e))?;
            for p in items {
                let wide = crate::to_wide(p);
                let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)
                    .map_err(|e| err("open item", e))?;
                op.DeleteItem(&item, None)
                    .map_err(|e| err("DeleteItem", e))?;
            }
            let result = op.PerformOperations();
            let _ = op.Unadvise(cookie);
            result.map_err(|e| e.message())
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
        let report = run_shell(&[one.clone(), two.clone()], Mode::Recycle, quiet());
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
        let report = run_shell(std::slice::from_ref(&missing), Mode::Recycle, quiet());
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
}
