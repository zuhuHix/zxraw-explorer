//! Linux deletes: the freedesktop.org Trash (spec 1.0) for a recycle, and an in-process
//! delete that never follows links for a permanent delete.
//!
//! A recycle only ever renames. An item on a file system with no usable trash (the home
//! trash is on another device, and its mount has no `.Trash/$uid` or `.Trash-$uid` that
//! can be used or made) is left in place as [`DeleteFailure::NotRecyclable`]: nothing is
//! copied across devices and nothing is destroyed, so the app can ask "delete permanently
//! instead?" in its own dialog, as on Windows.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::{
    DeleteFailure, DeleteObserver, DeleteProgress, DeleteReport, DeleteStage, ItemIdentity, Mode,
    Throttle, is_gone, refuses_permanent_delete,
};

/// Where the trashes are. Read from the process at call time by [`Env::from_process`];
/// tests build their own so they never reach the user's trash.
pub(super) struct Env<'a> {
    /// The home trash (`$XDG_DATA_HOME/Trash`), or `None` when there is no home to put it in.
    pub home_trash: Option<PathBuf>,
    pub uid: u32,
    /// The top directory (mount point) of the file system holding a directory.
    pub topdir_of: &'a dyn Fn(&Path) -> Option<PathBuf>,
}

impl Env<'static> {
    pub fn from_process() -> Self {
        Env {
            home_trash: home_trash_dir(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME")),
            // SAFETY: getuid cannot fail.
            uid: unsafe { libc::getuid() },
            topdir_of: &mount_point_of,
        }
    }
}

/// `$XDG_DATA_HOME/Trash` when that is absolute, else `$HOME/.local/share/Trash`.
fn home_trash_dir(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(data) = xdg_data_home.map(PathBuf::from).filter(|p| p.is_absolute()) {
        return Some(data.join("Trash"));
    }
    home.map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|h| h.join(".local/share/Trash"))
}

pub(super) fn run(
    paths: &[PathBuf],
    expected: &[Option<ItemIdentity>],
    mode: Mode,
    observer: DeleteObserver,
) -> DeleteReport {
    run_with(paths, expected, mode, observer, &Env::from_process())
}

/// Progress and the observer.
struct Teller {
    observer: DeleteObserver,
    throttle: Throttle,
    progress: DeleteProgress,
    /// The observer asked to stop.
    stopped: bool,
}

impl Teller {
    /// Tells the observer when `force` is set or the throttle lets it through; false when
    /// it asked to stop (now or before).
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

pub(super) fn run_with(
    paths: &[PathBuf],
    expected: &[Option<ItemIdentity>],
    mode: Mode,
    observer: DeleteObserver,
    env: &Env<'_>,
) -> DeleteReport {
    let mut report = DeleteReport::default();
    let mut pending: Vec<usize> = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        if is_gone(p) {
            report.failed.push((p.clone(), DeleteFailure::Missing));
        } else {
            pending.push(i);
        }
    }
    let mut t = Teller {
        observer,
        throttle: Throttle::default(),
        progress: DeleteProgress {
            items_total: paths.len(),
            items_done: report.failed.len(),
            ..Default::default()
        },
        stopped: false,
    };
    match mode {
        Mode::Recycle => recycle_all(paths, &pending, env, &mut t, &mut report),
        Mode::Permanent => delete_all(paths, &pending, expected, mode, &mut t, &mut report),
    }
    // The last word on cancelling: a cancel at the end counts too.
    t.progress.current = None;
    if !report.cancelled && !t.tell_if(true) {
        report.cancelled = true;
    }
    report
}

fn recycle_all(
    paths: &[PathBuf],
    pending: &[usize],
    env: &Env<'_>,
    t: &mut Teller,
    report: &mut DeleteReport,
) {
    for (k, &i) in pending.iter().enumerate() {
        let p = &paths[i];
        t.progress.current = Some(p.clone());
        // Each item goes in one rename, a folder with everything in it.
        let changed = t.enter(DeleteStage::MovingWhole);
        if !t.tell_if(changed || k == 0) {
            report.cancelled = true;
            report
                .not_started
                .extend(pending[k..].iter().map(|&j| paths[j].clone()));
            return;
        }
        match recycle_one(env, p) {
            Ok(()) => report.done.push(p.clone()),
            Err(why) => report.failed.push((p.clone(), why)),
        }
        t.progress.items_done += 1;
        if !t.tell_if(false) {
            report.cancelled = true;
            report
                .not_started
                .extend(pending[k + 1..].iter().map(|&j| paths[j].clone()));
            return;
        }
    }
}

fn delete_all(
    paths: &[PathBuf],
    pending: &[usize],
    expected: &[Option<ItemIdentity>],
    mode: Mode,
    t: &mut Teller,
    report: &mut DeleteReport,
) {
    // Count first, as the shell does, so the delete has a total.
    t.enter(DeleteStage::Counting);
    let mut total = 0u64;
    for &i in pending {
        let counted = count_tree(&paths[i], &mut |found_path| {
            t.progress.found += 1;
            t.progress.current = Some(found_path.to_path_buf());
            let first = t.progress.found == 1;
            t.tell_if(first)
        });
        total += counted;
        if t.stopped {
            report.cancelled = true;
            report
                .not_started
                .extend(pending.iter().map(|&j| paths[j].clone()));
            return;
        }
    }
    t.enter(DeleteStage::Deleting);
    t.progress.work_total = total;
    t.progress.work_done = 0;
    for (k, &i) in pending.iter().enumerate() {
        let p = &paths[i];
        t.progress.current = Some(p.clone());
        if !t.tell_if(k == 0) {
            report.cancelled = true;
            report
                .not_started
                .extend(pending[k..].iter().map(|&j| paths[j].clone()));
            return;
        }
        // A confirmed delete must remove the very object that was confirmed: `destroy`
        // checks the identity on the item it has pinned, not on the path.
        let check = if expected.is_empty() {
            Check::None
        } else {
            match expected.get(i).cloned().flatten() {
                Some(then) => Check::Same(then),
                None => Check::Unknown,
            }
        };
        let outcome = destroy_checked(mode, p, &check, &mut |inner| {
            t.progress.work_done += 1;
            t.progress.current = Some(inner.to_path_buf());
            t.tell_if(false)
        });
        t.progress.items_done += 1;
        if outcome.changed {
            report.failed.push((p.clone(), DeleteFailure::Changed));
        } else if outcome.gone {
            report.done.push(p.clone());
        } else if t.stopped {
            report.cancelled = true;
            if outcome.removed_inside {
                report.partly_deleted.push(p.clone());
            } else {
                report.not_started.push(p.clone());
            }
            report
                .not_started
                .extend(pending[k + 1..].iter().map(|&j| paths[j].clone()));
            return;
        } else {
            let why = match outcome.error {
                Some(why) => why,
                None => DeleteFailure::Other("it could not be deleted".into()),
            };
            report.failed.push((p.clone(), why));
        }
    }
}

/// Why an io error left an item in place.
fn classify(e: &io::Error) -> DeleteFailure {
    match e.raw_os_error() {
        Some(libc::ENOENT) => DeleteFailure::Missing,
        Some(libc::EACCES | libc::EPERM) => DeleteFailure::NeedsAdmin,
        Some(libc::EBUSY | libc::ETXTBSY) => DeleteFailure::InUse,
        Some(libc::EROFS) => DeleteFailure::Other("it is read-only".into()),
        _ => DeleteFailure::Other(e.to_string()),
    }
}

// ---------------------------------------------------------------------------------------
// Permanent delete
// ---------------------------------------------------------------------------------------

struct Destroyed {
    /// The item itself was removed.
    gone: bool,
    /// The item at the path was not the confirmed one; nothing was removed.
    changed: bool,
    /// Something inside the item was removed.
    removed_inside: bool,
    /// The first reason something was not removed.
    error: Option<DeleteFailure>,
}

fn cstring(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

/// What a delete must check about the item before removing anything.
enum Check {
    /// Nothing: an unconfirmed delete.
    None,
    /// It must still be this object.
    Same(ItemIdentity),
    /// It was confirmed but had no identity then, so it can't be shown to be the same.
    Unknown,
}

/// Deletes `path` and, for a folder, everything in it, without following any link (a link
/// is removed, never what it points to). Works through directory handles, so a folder
/// swapped for a link midway is not followed either. `tick` hears about each entry before
/// it goes and stops the delete by returning false.
#[cfg(test)]
fn destroy(mode: Mode, path: &Path, tick: &mut dyn FnMut(&Path) -> bool) -> Destroyed {
    destroy_checked(mode, path, &Check::None, tick)
}

#[cfg(test)]
thread_local! {
    /// Test seam: runs at the points named by [`Seam`], with the item's path.
    static SEAM: std::cell::RefCell<Option<Box<dyn FnMut(Seam, &Path)>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seam {
    /// Before the item is moved to its staging name (the item could still be swapped).
    BeforeStage,
    /// After it was staged and checked, before anything is removed.
    AfterCheck,
}

fn seam(_at: &'static str, _path: &Path) {
    #[cfg(test)]
    SEAM.with(|s| {
        if let Some(f) = s.borrow_mut().as_mut() {
            let at = if _at == "stage" {
                Seam::BeforeStage
            } else {
                Seam::AfterCheck
            };
            f(at, _path);
        }
    });
}

/// Unique private names for items being deleted, in the item's own folder.
fn staging_name(parent: RawFd) -> io::Result<CString> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    cstring(format!(".zxraw-deleting-{}-{n}-{parent}", std::process::id()).as_bytes())
}

/// `fstatat(AT_SYMLINK_NOFOLLOW)` of `name` in the open folder `dir`.
fn stat_at(dir: RawFd, name: &CStr) -> Option<libc::stat> {
    // SAFETY: valid fd, NUL-terminated name, and a zeroed plain-data out value.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        (libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) == 0).then_some(st)
    }
}

/// `renameat2(RENAME_NOREPLACE)` within one folder handle.
fn rename_at_noreplace(dir: RawFd, from: &CStr, to: &CStr) -> io::Result<()> {
    // SAFETY: valid fd and NUL-terminated names.
    let r =
        unsafe { libc::renameat2(dir, from.as_ptr(), dir, to.as_ptr(), libc::RENAME_NOREPLACE) };
    if r == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        // The file system cannot refuse to replace: check first, then rename.
        Some(libc::EINVAL | libc::ENOSYS) => {
            if stat_at(dir, to).is_some() {
                return Err(io::Error::from_raw_os_error(libc::EEXIST));
            }
            // SAFETY: as above.
            if unsafe { libc::renameat(dir, from.as_ptr(), dir, to.as_ptr()) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        _ => Err(e),
    }
}

/// The identity of `name` inside the open folder `dir` (not following links). Read through
/// `/proc/self/fd`, so the folder is the one we hold open, whatever its path names now.
fn identity_at(dir: RawFd, name: &CStr) -> Option<ItemIdentity> {
    let mut p = PathBuf::from(format!("/proc/self/fd/{dir}"));
    p.push(OsStr::from_bytes(name.to_bytes()));
    ItemIdentity::of(&p)
}

/// [`destroy`] with a check of what is being deleted. The item is first renamed to a
/// private staging name in its own folder (through a handle on the folder), and only that
/// staged object is checked and deleted: whatever is put at the old path meanwhile is never
/// touched. If the staged object is not the expected one it is moved back and nothing is
/// removed (`changed`); a delete that stops early moves what is left back as well.
fn destroy_checked(
    mode: Mode,
    path: &Path,
    check: &Check,
    tick: &mut dyn FnMut(&Path) -> bool,
) -> Destroyed {
    // Never destroy anything during a recycle (see `refuses_permanent_delete`).
    if refuses_permanent_delete(mode, 0) {
        return Destroyed {
            gone: false,
            changed: false,
            removed_inside: false,
            error: Some(DeleteFailure::NotRecyclable),
        };
    }
    let fail = |why| Destroyed {
        gone: false,
        changed: false,
        removed_inside: false,
        error: Some(why),
    };
    if matches!(check, Check::Unknown) {
        return Destroyed {
            changed: true,
            ..fail(DeleteFailure::Changed)
        };
    }
    let Some(name) = path.file_name() else {
        return fail(DeleteFailure::Other("this path cannot be deleted".into()));
    };
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let (parent_c, name_c) = match (
        cstring(parent.as_os_str().as_bytes()),
        cstring(name.as_bytes()),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return fail(classify(&e)),
    };
    let parent_fd = match open_dir_at(libc::AT_FDCWD, &parent_c, false) {
        Ok(fd) => fd,
        Err(e) => return fail(classify(&e)),
    };
    let dir = parent_fd.as_raw_fd();

    // Pin the object: move it to a private name, then check that name.
    seam("stage", path);
    let staged = match staging_name(dir) {
        Ok(n) => n,
        Err(e) => return fail(classify(&e)),
    };
    if let Err(e) = rename_at_noreplace(dir, &name_c, &staged) {
        return fail(classify(&e));
    }
    let put_back = || {
        if let Err(e) = rename_at_noreplace(dir, &staged, &name_c) {
            tracing::warn!(
                path = %path.display(),
                staged = %String::from_utf8_lossy(staged.to_bytes()),
                error = %e,
                "could not move a partly deleted item back to its name"
            );
        }
    };
    if let Check::Same(then) = check
        && identity_at(dir, &staged).as_ref() != Some(then)
    {
        put_back();
        return Destroyed {
            changed: true,
            ..fail(DeleteFailure::Changed)
        };
    }
    seam("check", path);

    let mut d = Destroyer {
        tick,
        removed: 0,
        error: None,
        stopped: false,
    };
    let top_dir =
        stat_at(dir, &staged).is_some_and(|st| st.st_mode & libc::S_IFMT == libc::S_IFDIR);
    d.remove_at(dir, &staged, path, top_dir);
    let gone = stat_at(dir, &staged).is_none();
    if !gone {
        put_back();
    }
    // The item itself counts once if it went; anything more was inside it.
    Destroyed {
        gone,
        changed: false,
        removed_inside: d.removed > usize::from(gone),
        error: d.error.as_ref().map(classify),
    }
}

struct Destroyer<'a> {
    tick: &'a mut dyn FnMut(&Path) -> bool,
    removed: usize,
    error: Option<io::Error>,
    stopped: bool,
}

impl Destroyer<'_> {
    fn note(&mut self, e: io::Error) {
        if e.raw_os_error() != Some(libc::ENOENT) && self.error.is_none() {
            self.error = Some(e);
        }
    }

    fn unlink(&mut self, parent: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<()> {
        // SAFETY: valid fd and NUL-terminated name.
        if unsafe { libc::unlinkat(parent, name.as_ptr(), flags) } == 0 {
            self.removed += 1;
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn remove_at(&mut self, parent: RawFd, name: &CStr, path: &Path, maybe_dir: bool) {
        if self.stopped {
            return;
        }
        if !(self.tick)(path) {
            self.stopped = true;
            return;
        }
        if !maybe_dir {
            match self.unlink(parent, name, 0) {
                Ok(()) => return,
                // It became a folder after it was listed.
                Err(e) if e.raw_os_error() == Some(libc::EISDIR) => {}
                Err(e) => return self.note(e),
            }
        }
        let fd = match open_dir_at(parent, name, true) {
            Ok(fd) => fd,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) => {
                // Not a folder (or a link to one): remove the entry itself.
                if let Err(e) = self.unlink(parent, name, 0) {
                    self.note(e);
                }
                return;
            }
            Err(e) => return self.note(e),
        };
        let entries = match list_dir(&fd) {
            Ok(entries) => entries,
            Err(e) => return self.note(e),
        };
        for (child, dtype) in entries {
            let child_path = path.join(OsStr::from_bytes(child.to_bytes()));
            let maybe_dir = matches!(dtype, libc::DT_DIR | libc::DT_UNKNOWN);
            self.remove_at(fd.as_raw_fd(), &child, &child_path, maybe_dir);
            if self.stopped {
                return;
            }
        }
        drop(fd);
        if let Err(e) = self.unlink(parent, name, libc::AT_REMOVEDIR) {
            self.note(e);
        }
    }
}

/// Opens the directory `name` relative to `parent`. With `no_follow`, a link is not
/// followed (the open fails with `ELOOP` or `ENOTDIR`).
fn open_dir_at(parent: RawFd, name: &CStr, no_follow: bool) -> io::Result<OwnedFd> {
    let mut flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    if no_follow {
        flags |= libc::O_NOFOLLOW;
    }
    // SAFETY: valid fd and NUL-terminated name.
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a fresh fd we own.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// The entries of an open directory (without `.` and `..`), with their `d_type`.
fn list_dir(fd: &OwnedFd) -> io::Result<Vec<(CString, u8)>> {
    // fdopendir takes over the fd it is given, so give it a copy.
    // SAFETY: valid fd.
    let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `dup` is ours; on failure we close it.
    let dir = unsafe { libc::fdopendir(dup) };
    if dir.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so `dup` is still ours.
        unsafe { libc::close(dup) };
        return Err(e);
    }
    let mut out = Vec::new();
    let result = loop {
        // SAFETY: errno is thread-local; dir is a valid DIR*.
        unsafe { *libc::__errno_location() = 0 };
        let ent = unsafe { libc::readdir(dir) };
        if ent.is_null() {
            let e = io::Error::last_os_error();
            break if e.raw_os_error().unwrap_or(0) == 0 {
                Ok(())
            } else {
                Err(e)
            };
        }
        // SAFETY: readdir returned a valid entry with a NUL-terminated name.
        let (name, dtype) = unsafe { (CStr::from_ptr((*ent).d_name.as_ptr()), (*ent).d_type) };
        let bytes = name.to_bytes();
        if bytes != b"." && bytes != b".." {
            out.push((name.to_owned(), dtype));
        }
    };
    // SAFETY: closes `dir` and with it `dup`.
    unsafe { libc::closedir(dir) };
    result.map(|()| out)
}

/// Counts `path` and everything in it (links not followed). `tick` hears about each entry
/// and stops the count by returning false.
fn count_tree(path: &Path, tick: &mut dyn FnMut(&Path) -> bool) -> u64 {
    fn walk(path: &Path, tick: &mut dyn FnMut(&Path) -> bool, n: &mut u64) -> bool {
        *n += 1;
        if !tick(path) {
            return false;
        }
        if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
            && let Ok(rd) = fs::read_dir(path)
        {
            for entry in rd.flatten() {
                if !walk(&entry.path(), tick, n) {
                    return false;
                }
            }
        }
        true
    }
    let mut n = 0;
    walk(path, tick, &mut n);
    n
}

/// The disk usage of `path` and everything under it, in bytes, as `du -B1` reports it
/// (`st_blocks * 512` of each entry, folders included; links not followed, counted as
/// themselves). Unlike `du`, a file with several hard links inside is counted each time.
fn tree_size(path: &Path) -> u64 {
    let Ok(md) = fs::symlink_metadata(path) else {
        return 0;
    };
    let own = md.blocks().saturating_mul(512);
    if !md.is_dir() {
        return own;
    }
    own + fs::read_dir(path)
        .map(|rd| rd.flatten().map(|e| tree_size(&e.path())).sum::<u64>())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------
// Trash
// ---------------------------------------------------------------------------------------

/// Where trashing stopped.
#[derive(Debug)]
enum TrashError {
    /// The trash could not be used (nothing of the item was touched).
    Trash(io::Error),
    /// Moving the item into a usable trash failed.
    Move(io::Error),
}

/// `path` made absolute with its parent resolved, the item itself not followed.
fn absolute_item(path: &Path) -> io::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "this path cannot be recycled")
    })?;
    let parent = path.parent().unwrap_or(Path::new("/")).canonicalize()?;
    Ok(parent.join(name))
}

fn recycle_one(env: &Env<'_>, path: &Path) -> Result<(), DeleteFailure> {
    let item = absolute_item(path).map_err(|e| classify(&e))?;
    let md = fs::symlink_metadata(&item).map_err(|e| classify(&e))?;
    let dev = md.dev();
    let is_dir = md.is_dir();
    let mut tried: Vec<String> = Vec::new();
    let guard = |trash: &Path| -> Result<(), DeleteFailure> {
        let trash = trash.canonicalize().unwrap_or_else(|_| trash.to_path_buf());
        if trash.starts_with(&item) {
            Err(DeleteFailure::Other("it holds the trash folder".into()))
        } else {
            Ok(())
        }
    };

    if let Some(home) = &env.home_trash {
        match prepare_trash(home, None) {
            Ok(trash_dev) if trash_dev == dev => {
                guard(home)?;
                let field = encode_path(item.as_os_str().as_bytes());
                match trash_into(home, &item, &field, is_dir) {
                    Ok(()) => return Ok(()),
                    // Same device but another mount (a bind mount): try the item's own.
                    Err(TrashError::Move(e)) if e.raw_os_error() == Some(libc::EXDEV) => {
                        tried.push(format!("home trash: {e}"));
                    }
                    Err(TrashError::Move(e)) => return Err(classify(&e)),
                    // The item belongs in the home trash, which cannot take it: never put it
                    // in a top directory trash of the home file system instead.
                    Err(TrashError::Trash(e)) => {
                        tracing::info!(
                            path = %item.display(),
                            error = %e,
                            "home trash unusable; not recycled"
                        );
                        return Err(DeleteFailure::NotRecyclable);
                    }
                }
            }
            Ok(_) => tried.push("home trash is on another device".into()),
            Err(e) => tried.push(format!("home trash: {e}")),
        }
    }

    let parent = item.parent().unwrap_or(Path::new("/"));
    if let Some(top) = (env.topdir_of)(parent) {
        match topdir_trash(&top, env.uid) {
            Ok((trash, trash_dev)) if trash_dev == dev => {
                guard(&trash)?;
                let rel = item.strip_prefix(&top).unwrap_or(&item);
                let field = encode_path(rel.as_os_str().as_bytes());
                match trash_into(&trash, &item, &field, is_dir) {
                    Ok(()) => return Ok(()),
                    Err(TrashError::Move(e)) if e.raw_os_error() != Some(libc::EXDEV) => {
                        return Err(classify(&e));
                    }
                    Err(TrashError::Move(e) | TrashError::Trash(e)) => {
                        tried.push(format!("{}: {e}", trash.display()));
                    }
                }
            }
            Ok((trash, _)) => tried.push(format!("{} is on another device", trash.display())),
            Err(e) => tried.push(format!("trash in {}: {e}", top.display())),
        }
    } else {
        tried.push("mount point not found".into());
    }
    tracing::info!(
        path = %item.display(),
        tried = %tried.join("; "),
        "no usable trash; not recycled"
    );
    Err(DeleteFailure::NotRecyclable)
}

/// Creates `dir` (mode 0700) if missing. With `owner`, it must then be a real folder
/// (not a link) owned by that user.
fn private_dir(dir: &Path, owner: Option<u32>, recursive: bool) -> io::Result<()> {
    match fs::DirBuilder::new()
        .recursive(recursive)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let md = match owner {
        Some(_) => fs::symlink_metadata(dir)?,
        None => fs::metadata(dir)?,
    };
    if !md.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a folder", dir.display()),
        ));
    }
    if let Some(uid) = owner
        && md.uid() != uid
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} belongs to another user", dir.display()),
        ));
    }
    // A trash others can write to could have entries planted or swapped in it. Ours gets
    // its private mode back; someone else's is refused (the caller falls through).
    if md.mode() & 0o022 != 0 {
        // SAFETY: getuid cannot fail.
        let me = owner.unwrap_or_else(|| unsafe { libc::getuid() });
        if md.uid() != me {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is writable by others and not ours", dir.display()),
            ));
        }
        tracing::warn!(dir = %dir.display(), mode = format!("{:o}", md.mode() & 0o7777), "trash folder was writable by others; made private");
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Makes `trash` usable (it and its `files` and `info`), and returns the device of
/// `files`. The home trash may be a link (`owner` None); a top directory trash must be
/// the user's own folder.
fn prepare_trash(trash: &Path, owner: Option<u32>) -> io::Result<u64> {
    private_dir(trash, owner, owner.is_none())?;
    private_dir(&trash.join("files"), owner, false)?;
    private_dir(&trash.join("info"), owner, false)?;
    Ok(fs::metadata(trash.join("files"))?.dev())
}

/// The trash for items on the file system whose top directory is `top`: `$top/.Trash/$uid`
/// when `$top/.Trash` is a real folder with the sticky bit, else `$top/.Trash-$uid`. Made
/// if missing. Returns it and the device of its `files`.
fn topdir_trash(top: &Path, uid: u32) -> io::Result<(PathBuf, u64)> {
    let shared = top.join(".Trash");
    if let Ok(md) = fs::symlink_metadata(&shared) {
        let usable = md.is_dir() && md.mode() & libc::S_ISVTX != 0;
        if usable {
            let mine = shared.join(uid.to_string());
            match prepare_trash(&mine, Some(uid)) {
                Ok(dev) => return Ok((mine, dev)),
                Err(e) => tracing::warn!(dir = %mine.display(), error = %e, "unusable trash"),
            }
        } else {
            tracing::warn!(
                dir = %shared.display(),
                "not a folder with the sticky bit; not used as a trash"
            );
        }
    }
    let own = top.join(format!(".Trash-{uid}"));
    let dev = prepare_trash(&own, Some(uid))?;
    Ok((own, dev))
}

/// The most bytes of the item's name kept in its trash name, leaving room for a number
/// and `.trashinfo` within the usual 255-byte name limit.
const MAX_TRASH_NAME: usize = 255 - ".trashinfo".len() - ".99999".len();

/// The name an item is first tried under in the trash: its own, shortened if too long.
fn trash_base_name(name: &OsStr) -> OsString {
    let bytes = name.as_bytes();
    if bytes.len() <= MAX_TRASH_NAME {
        return name.to_os_string();
    }
    let mut end = MAX_TRASH_NAME;
    if let Ok(s) = std::str::from_utf8(bytes) {
        while !s.is_char_boundary(end) {
            end -= 1;
        }
    }
    OsString::from_vec(bytes[..end].to_vec())
}

/// Renames `from` to `to` unless `to` exists.
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let a = cstring(from.as_os_str().as_bytes())?;
    let b = cstring(to.as_os_str().as_bytes())?;
    // SAFETY: valid NUL-terminated paths.
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        // The file system cannot refuse to replace: check first, then rename.
        Some(libc::EINVAL | libc::ENOSYS) => {
            if fs::symlink_metadata(to).is_ok() {
                Err(io::Error::from_raw_os_error(libc::EEXIST))
            } else {
                fs::rename(from, to)
            }
        }
        _ => Err(e),
    }
}

/// Moves `item` into `trash` (prepared), writing its `.trashinfo` first with `Path=field`.
fn trash_into(trash: &Path, item: &Path, field: &str, is_dir: bool) -> Result<(), TrashError> {
    let files = trash.join("files");
    let info = trash.join("info");
    let base = trash_base_name(item.file_name().unwrap_or(OsStr::new("item")));
    let content = format!(
        "[Trash Info]\nPath={field}\nDeletionDate={}\n",
        deletion_date(std::time::SystemTime::now())
    );
    for n in 1..100_000u32 {
        let mut name = base.clone();
        if n > 1 {
            name.push(format!(".{n}"));
        }
        let mut info_name = name.clone();
        info_name.push(".trashinfo");
        let info_path = info.join(&info_name);
        // Creating the info file reserves the name.
        let mut f = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&info_path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(TrashError::Trash(e)),
        };
        let target = files.join(&name);
        // An item left in `files` without its info still owns the name.
        if fs::symlink_metadata(&target).is_ok() {
            drop(f);
            let _ = fs::remove_file(&info_path);
            continue;
        }
        if let Err(e) = f.write_all(content.as_bytes()).and_then(|()| f.sync_all()) {
            drop(f);
            let _ = fs::remove_file(&info_path);
            return Err(TrashError::Trash(e));
        }
        drop(f);
        match rename_noreplace(item, &target) {
            Ok(()) => {
                if is_dir && let Err(e) = record_dir_size(trash, &name, &info_path) {
                    tracing::warn!(trash = %trash.display(), error = %e, "directorysizes not updated");
                }
                return Ok(());
            }
            Err(e) => {
                let _ = fs::remove_file(&info_path);
                if e.raw_os_error() == Some(libc::EEXIST) {
                    continue;
                }
                return Err(TrashError::Move(e));
            }
        }
    }
    Err(TrashError::Trash(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name in the trash",
    )))
}

/// Adds the folder trashed as `name` to `trash/directorysizes` (spec 1.0: `size mtime
/// name` lines), dropping entries whose folder is no longer in the trash. Rewritten through
/// a temporary file and a rename.
fn record_dir_size(trash: &Path, name: &OsStr, info_path: &Path) -> io::Result<()> {
    let files = trash.join("files");
    let size = tree_size(&files.join(name));
    let mtime = fs::metadata(info_path)?.mtime();
    let encoded = encode_name(name.as_bytes());
    let file = trash.join("directorysizes");
    let mut out = String::new();
    if let Ok(old) = fs::read_to_string(&file) {
        for line in old.lines() {
            let mut parts = line.splitn(3, ' ');
            let (Some(_), Some(_), Some(entry)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            if entry == encoded {
                continue;
            }
            let decoded = OsString::from_vec(decode(entry));
            if fs::symlink_metadata(files.join(&decoded)).is_ok() {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out.push_str(&format!("{size} {mtime} {encoded}\n"));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = trash.join(format!(
        ".directorysizes.{}.{nanos}.tmp",
        std::process::id()
    ));
    let write = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(out.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &file)
    })();
    if write.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write
}

/// Percent-encodes a path for `Path=`: everything but unreserved characters and `/`.
fn encode_path(bytes: &[u8]) -> String {
    encode(bytes, true)
}

/// Percent-encodes a trash name for `directorysizes` (a `/` is encoded too).
fn encode_name(bytes: &[u8]) -> String {
    encode(bytes, false)
}

fn encode(bytes: &[u8], keep_slash: bool) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && b == b'/')
        {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// `YYYY-MM-DDThh:mm:ss` in local time.
fn deletion_date(at: std::time::SystemTime) -> String {
    let secs = at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as libc::time_t;
    // SAFETY: plain C struct, filled by localtime_r.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers.
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        // SAFETY: valid pointers.
        unsafe { libc::gmtime_r(&secs, &mut tm) };
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// The mount point of the file system holding `dir` (an absolute, resolved path), from
/// `/proc/self/mountinfo`, or by walking up while the device stays the same.
fn mount_point_of(dir: &Path) -> Option<PathBuf> {
    if let Ok(bytes) = fs::read("/proc/self/mountinfo")
        && let Ok(text) = String::from_utf8(bytes)
        && let Some(top) = mount_point_in(&text, dir)
    {
        return Some(top);
    }
    let dev = fs::metadata(dir).ok()?.dev();
    let mut top = dir.to_path_buf();
    while let Some(up) = top.parent() {
        if fs::metadata(up).ok()?.dev() != dev {
            break;
        }
        top = up.to_path_buf();
    }
    Some(top)
}

/// The longest mount point in a `mountinfo` text that holds `dir`.
fn mount_point_in(mountinfo: &str, dir: &Path) -> Option<PathBuf> {
    mountinfo
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|field| PathBuf::from(OsString::from_vec(unescape_mount(field))))
        .filter(|m| dir.starts_with(m))
        // The last of equal length wins: it is mounted over the earlier ones.
        .fold(None::<PathBuf>, |best, m| match best {
            Some(b) if b.as_os_str().len() > m.as_os_str().len() => Some(b),
            _ => Some(m),
        })
}

/// Undoes the `\ooo` octal escapes of `mountinfo` fields.
fn unescape_mount(field: &str) -> Vec<u8> {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 4]).unwrap_or("x"), 8)
        {
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recycle::TELL_EVERY;
    use std::cell::RefCell;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::rc::Rc;

    fn no_topdir(_: &Path) -> Option<PathBuf> {
        None
    }

    fn uid() -> u32 {
        // SAFETY: getuid cannot fail.
        unsafe { libc::getuid() }
    }

    fn is_root() -> bool {
        // SAFETY: geteuid cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    /// A temp folder with a home trash inside it, never the user's.
    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }
        fn root(&self) -> &Path {
            self.dir.path()
        }
        fn trash(&self) -> PathBuf {
            self.root().join("xdg/Trash")
        }
        fn env(&self) -> Env<'static> {
            Env {
                home_trash: Some(self.trash()),
                uid: uid(),
                topdir_of: &no_topdir,
            }
        }
        fn file(&self, rel: &str, body: &[u8]) -> PathBuf {
            let p = self.root().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, body).unwrap();
            p
        }
    }

    fn quiet() -> DeleteObserver {
        Box::new(|_| true)
    }

    fn recycle(env: &Env<'_>, paths: &[PathBuf]) -> DeleteReport {
        run_with(paths, &[], Mode::Recycle, quiet(), env)
    }

    fn info(trash: &Path, name: &str) -> String {
        fs::read_to_string(trash.join("info").join(format!("{name}.trashinfo"))).unwrap()
    }

    fn date_ok(s: &str) -> bool {
        let b = s.as_bytes();
        b.len() == 19
            && b.iter().enumerate().all(|(i, c)| match i {
                4 | 7 => *c == b'-',
                10 => *c == b'T',
                13 | 16 => *c == b':',
                _ => c.is_ascii_digit(),
            })
    }

    fn listener() -> (DeleteObserver, Rc<RefCell<Vec<DeleteProgress>>>) {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let s = seen.clone();
        (
            Box::new(move |p: &DeleteProgress| {
                s.borrow_mut().push(p.clone());
                true
            }),
            seen,
        )
    }

    #[test]
    fn a_file_goes_to_the_trash_with_its_info() {
        let fx = Fixture::new();
        let f = fx.file("src/a b%ç.txt", b"hello");
        let report = recycle(&fx.env(), std::slice::from_ref(&f));
        assert_eq!(report.done, vec![f.clone()], "{report:?}");
        assert!(report.failed.is_empty() && !report.cancelled);
        assert!(!f.exists());
        let trash = fx.trash();
        assert_eq!(fs::read(trash.join("files/a b%ç.txt")).unwrap(), b"hello");
        let text = info(&trash, "a b%ç.txt");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "[Trash Info]");
        let src = fs::canonicalize(fx.root()).unwrap().join("src");
        let want = format!(
            "Path={}/a%20b%25%C3%A7.txt",
            encode_path(src.as_os_str().as_bytes())
        );
        assert_eq!(lines[1], want);
        let date = lines[2].strip_prefix("DeletionDate=").unwrap();
        assert!(date_ok(date), "{date}");
        // A file adds nothing to directorysizes.
        assert!(!trash.join("directorysizes").exists());
    }

    #[test]
    fn a_folder_is_trashed_whole_and_recorded_in_directorysizes() {
        let fx = Fixture::new();
        fx.file("tree/sub/one.bin", &[0u8; 100]);
        fx.file("tree/two.bin", &[0u8; 23]);
        let tree = fx.root().join("tree");
        let report = recycle(&fx.env(), std::slice::from_ref(&tree));
        assert_eq!(report.done, vec![tree.clone()], "{report:?}");
        let trash = fx.trash();
        assert!(trash.join("files/tree/sub/one.bin").is_file());
        let sizes = fs::read_to_string(trash.join("directorysizes")).unwrap();
        let mtime = fs::metadata(trash.join("info/tree.trashinfo"))
            .unwrap()
            .mtime();
        let usage = tree_size(&trash.join("files/tree"));
        assert!(usage >= 123, "disk usage, at least the bytes: {usage}");
        assert_eq!(sizes, format!("{usage} {mtime} tree\n"));
        // A second folder keeps the first entry.
        fx.file("other dir/x", b"xyz");
        let other = fx.root().join("other dir");
        assert!(
            recycle(&fx.env(), std::slice::from_ref(&other))
                .failed
                .is_empty()
        );
        let sizes = fs::read_to_string(trash.join("directorysizes")).unwrap();
        let lines: Vec<&str> = sizes.lines().collect();
        assert_eq!(lines.len(), 2, "{sizes}");
        assert!(
            lines[1].starts_with(&format!("{} ", tree_size(&trash.join("files/other dir"))))
                && lines[1].ends_with(" other%20dir"),
            "{sizes}"
        );
        // An entry whose folder left the trash is dropped at the next write.
        fs::remove_dir_all(trash.join("files/tree")).unwrap();
        fx.file("third/y", b"y");
        assert!(
            recycle(&fx.env(), &[fx.root().join("third")])
                .failed
                .is_empty()
        );
        let sizes = fs::read_to_string(trash.join("directorysizes")).unwrap();
        assert!(!sizes.contains(" tree\n"), "{sizes}");
        assert_eq!(sizes.lines().count(), 2, "{sizes}");
    }

    #[test]
    fn names_already_in_the_trash_get_a_number() {
        let fx = Fixture::new();
        let a = fx.file("one/same.txt", b"1");
        let b = fx.file("two/same.txt", b"2");
        let c = fx.file("three/same.txt", b"3");
        // An orphan in files/ without its info still owns its name.
        fs::create_dir_all(fx.trash().join("files")).unwrap();
        fs::write(fx.trash().join("files/same.txt.3"), b"orphan").unwrap();
        let report = recycle(&fx.env(), &[a, b, c]);
        assert_eq!(report.done.len(), 3, "{report:?}");
        let trash = fx.trash();
        assert_eq!(fs::read(trash.join("files/same.txt")).unwrap(), b"1");
        assert_eq!(fs::read(trash.join("files/same.txt.2")).unwrap(), b"2");
        assert_eq!(fs::read(trash.join("files/same.txt.3")).unwrap(), b"orphan");
        assert_eq!(fs::read(trash.join("files/same.txt.4")).unwrap(), b"3");
        assert!(info(&trash, "same.txt.2").contains("/two/same.txt\n"));
        assert!(!trash.join("info/same.txt.3.trashinfo").exists());
    }

    #[test]
    fn a_failed_move_removes_its_info_file_and_leaves_the_item() {
        if is_root() {
            return; // root ignores folder permissions
        }
        let fx = Fixture::new();
        let f = fx.file("locked/f.txt", b"x");
        let locked = fx.root().join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let report = recycle(&fx.env(), std::slice::from_ref(&f));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(report.failed, vec![(f.clone(), DeleteFailure::NeedsAdmin)]);
        assert!(f.exists());
        assert_eq!(fs::read_dir(fx.trash().join("info")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(fx.trash().join("files")).unwrap().count(), 0);
    }

    #[test]
    fn a_link_is_trashed_as_a_link() {
        let fx = Fixture::new();
        let target = fx.file("target/keep.txt", b"keep");
        let link = fx.root().join("link");
        symlink(fx.root().join("target"), &link).unwrap();
        let report = recycle(&fx.env(), std::slice::from_ref(&link));
        assert_eq!(report.done, vec![link.clone()], "{report:?}");
        let moved = fx.trash().join("files/link");
        assert!(
            fs::symlink_metadata(&moved)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        // Not a folder of its own: no directorysizes entry.
        assert!(!fx.trash().join("directorysizes").exists());
    }

    #[test]
    fn a_missing_item_fails_and_the_others_go() {
        let fx = Fixture::new();
        let missing = fx.root().join("missing");
        for mode in [Mode::Recycle, Mode::Permanent] {
            let b = fx.file("b", b"b");
            let report = run_with(&[missing.clone(), b.clone()], &[], mode, quiet(), &fx.env());
            assert_eq!(
                report.failed,
                vec![(missing.clone(), DeleteFailure::Missing)]
            );
            assert_eq!(report.done, vec![b]);
        }
    }

    #[test]
    fn without_a_usable_trash_nothing_is_deleted() {
        let fx = Fixture::new();
        let f = fx.file("f", b"x");
        let env = Env {
            home_trash: None,
            uid: uid(),
            topdir_of: &no_topdir,
        };
        let report = recycle(&env, std::slice::from_ref(&f));
        assert_eq!(
            report.failed,
            vec![(f.clone(), DeleteFailure::NotRecyclable)]
        );
        assert!(f.exists());
    }

    #[test]
    fn a_folder_holding_the_trash_is_not_moved_into_it() {
        let fx = Fixture::new();
        let xdg = fx.root().join("xdg");
        fs::create_dir_all(xdg.join("Trash")).unwrap();
        let report = recycle(&fx.env(), std::slice::from_ref(&xdg));
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(xdg.join("Trash/files").is_dir());
    }

    #[test]
    fn the_observer_sees_each_item_and_can_cancel() {
        let fx = Fixture::new();
        let items: Vec<PathBuf> = (0..3).map(|i| fx.file(&format!("f{i}"), b"x")).collect();
        let second = items[1].clone();
        let calls = Rc::new(RefCell::new(Vec::<DeleteProgress>::new()));
        let c = calls.clone();
        let observer: DeleteObserver = Box::new(move |p| {
            c.borrow_mut().push(p.clone());
            // Wait out the throttle so every report is heard.
            std::thread::sleep(TELL_EVERY);
            // Stop as the second item is about to go.
            p.current.as_deref() != Some(&*second)
        });
        let report = run_with(&items, &[], Mode::Recycle, observer, &fx.env());
        assert_eq!(report.done, vec![items[0].clone()], "{report:?}");
        assert!(report.cancelled);
        assert_eq!(report.not_started, items[1..].to_vec());
        assert!(items[1].exists() && items[2].exists());
        let seen = calls.borrow();
        assert_eq!(seen[0].stage, DeleteStage::MovingWhole);
        assert_eq!(seen[0].items_total, 3);
        assert_eq!(seen[0].current.as_deref(), Some(&*items[0]));
        assert_eq!(seen[1].items_done, 1);
    }

    #[test]
    fn reports_between_items_are_throttled() {
        let fx = Fixture::new();
        let items: Vec<PathBuf> = (0..50).map(|i| fx.file(&format!("f{i}"), b"x")).collect();
        let (observer, seen) = listener();
        let report = run_with(&items, &[], Mode::Recycle, observer, &fx.env());
        assert_eq!(report.done.len(), 50);
        let seen = seen.borrow();
        assert!(seen.len() < 20, "{} reports", seen.len());
        let last = seen.last().unwrap();
        assert_eq!(last.items_done, 50);
        assert!(last.current.is_none());
    }

    #[test]
    fn a_cancel_at_the_end_is_reported() {
        let fx = Fixture::new();
        let f = fx.file("f", b"x");
        let observer: DeleteObserver = Box::new(|p| p.items_done == 0);
        let report = run_with(
            std::slice::from_ref(&f),
            &[],
            Mode::Recycle,
            observer,
            &fx.env(),
        );
        assert_eq!(report.done, vec![f]);
        assert!(report.cancelled);
        assert!(report.not_started.is_empty());
    }

    #[test]
    fn a_permanent_delete_counts_then_removes_the_tree_with_progress() {
        let fx = Fixture::new();
        for i in 0..5 {
            fx.file(&format!("tree/d{i}/f"), b"x");
        }
        let tree = fx.root().join("tree");
        let file = fx.file("loose", b"x");
        let (observer, seen) = listener();
        let report = run_with(
            &[tree.clone(), file.clone()],
            &[],
            Mode::Permanent,
            observer,
            &fx.env(),
        );
        assert_eq!(report.done, vec![tree.clone(), file.clone()], "{report:?}");
        assert!(!tree.exists() && !file.exists());
        let seen = seen.borrow();
        assert_eq!(seen[0].stage, DeleteStage::Counting);
        let deleting = seen
            .iter()
            .find(|p| p.stage == DeleteStage::Deleting)
            .expect("a deleting report");
        // tree + 5 folders + 5 files + the loose file.
        assert_eq!(deleting.work_total, 12);
        let last = seen.last().unwrap();
        assert_eq!(last.items_done, 2);
        assert_eq!(last.work_done, 12);
        assert!(last.current.is_none());
        // Nothing went to the trash.
        assert!(!fx.trash().exists());
    }

    #[test]
    fn a_cancelled_permanent_delete_reports_a_partly_deleted_folder() {
        let fx = Fixture::new();
        for i in 0..20 {
            fx.file(&format!("tree/f{i}"), b"x");
        }
        let tree = fx.root().join("tree");
        let after = fx.file("after", b"x");
        let observer: DeleteObserver = Box::new(|p| {
            std::thread::sleep(TELL_EVERY);
            p.stage != DeleteStage::Deleting || p.work_done < 5
        });
        let report = run_with(
            &[tree.clone(), after.clone()],
            &[],
            Mode::Permanent,
            observer,
            &fx.env(),
        );
        assert!(report.cancelled, "{report:?}");
        assert_eq!(report.partly_deleted, vec![tree.clone()]);
        assert_eq!(report.not_started, vec![after.clone()]);
        assert!(tree.exists() && after.exists());
    }

    #[test]
    fn a_cancel_while_counting_deletes_nothing() {
        let fx = Fixture::new();
        let f = fx.file("tree/f", b"x");
        let tree = fx.root().join("tree");
        let observer: DeleteObserver = Box::new(|p| p.stage != DeleteStage::Counting);
        let report = run_with(
            std::slice::from_ref(&tree),
            &[],
            Mode::Permanent,
            observer,
            &fx.env(),
        );
        assert!(report.cancelled);
        assert_eq!(report.not_started, vec![tree]);
        assert!(f.exists());
    }

    #[test]
    fn a_permanent_delete_removes_links_not_their_targets() {
        let fx = Fixture::new();
        let target = fx.file("target/keep.txt", b"keep");
        fx.file("tree/f", b"x");
        let tree = fx.root().join("tree");
        symlink(fx.root().join("target"), tree.join("inner-link")).unwrap();
        let top_link = fx.root().join("top-link");
        symlink(fx.root().join("target"), &top_link).unwrap();
        let report = run_with(
            &[tree.clone(), top_link.clone()],
            &[],
            Mode::Permanent,
            quiet(),
            &fx.env(),
        );
        assert_eq!(report.done.len(), 2, "{report:?}");
        assert!(!tree.exists() && fs::symlink_metadata(&top_link).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn a_permanent_delete_reports_what_it_could_not_remove() {
        if is_root() {
            return;
        }
        let fx = Fixture::new();
        let f = fx.file("locked/f", b"x");
        let locked = fx.root().join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let report = run_with(
            std::slice::from_ref(&f),
            &[],
            Mode::Permanent,
            quiet(),
            &fx.env(),
        );
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(report.failed, vec![(f.clone(), DeleteFailure::NeedsAdmin)]);
        assert!(f.exists());
    }

    #[test]
    fn a_confirmed_item_that_changed_is_left() {
        let fx = Fixture::new();
        let a = fx.file("a", b"one");
        let b = fx.file("b", b"two");
        let (Some(id_a), Some(id_b)) = (ItemIdentity::of(&a), ItemIdentity::of(&b)) else {
            return; // no creation time on this file system
        };
        // Replace `a` with another file of the same size.
        fs::remove_file(&a).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&a, b"uno").unwrap();
        let report = run_with(
            &[a.clone(), b.clone()],
            &[Some(id_a), Some(id_b)],
            Mode::Permanent,
            quiet(),
            &fx.env(),
        );
        assert_eq!(report.failed, vec![(a.clone(), DeleteFailure::Changed)]);
        assert_eq!(report.done, vec![b]);
        assert!(a.exists());
    }

    #[test]
    fn the_destroyer_never_runs_for_a_recycle() {
        let fx = Fixture::new();
        let f = fx.file("f", b"x");
        let out = destroy(Mode::Recycle, &f, &mut |_| true);
        assert_eq!(out.error, Some(DeleteFailure::NotRecyclable));
        assert!(f.exists());
    }

    #[test]
    fn a_top_directory_trash_is_the_sticky_shared_one_or_the_users_own() {
        let u = uid();
        // No .Trash: .Trash-$uid is made, mode 0700.
        let top = tempfile::tempdir().unwrap();
        let (trash, _) = topdir_trash(top.path(), u).unwrap();
        assert_eq!(trash, top.path().join(format!(".Trash-{u}")));
        assert_eq!(fs::metadata(&trash).unwrap().mode() & 0o777, 0o700);
        assert!(trash.join("files").is_dir() && trash.join("info").is_dir());

        // A sticky .Trash: .Trash/$uid.
        let top = tempfile::tempdir().unwrap();
        let shared = top.path().join(".Trash");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();
        let (trash, _) = topdir_trash(top.path(), u).unwrap();
        assert_eq!(trash, shared.join(u.to_string()));
        assert!(trash.join("files").is_dir());

        // Without the sticky bit it is not used.
        let top = tempfile::tempdir().unwrap();
        fs::create_dir(top.path().join(".Trash")).unwrap();
        fs::set_permissions(top.path().join(".Trash"), fs::Permissions::from_mode(0o777)).unwrap();
        let (trash, _) = topdir_trash(top.path(), u).unwrap();
        assert_eq!(trash, top.path().join(format!(".Trash-{u}")));
        assert!(!top.path().join(".Trash").join(u.to_string()).exists());

        // A link to a sticky folder is not used either.
        let top = tempfile::tempdir().unwrap();
        let elsewhere = top.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o1777)).unwrap();
        symlink(&elsewhere, top.path().join(".Trash")).unwrap();
        let (trash, _) = topdir_trash(top.path(), u).unwrap();
        assert_eq!(trash, top.path().join(format!(".Trash-{u}")));
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);

        // A .Trash-$uid that is a link is refused.
        let top = tempfile::tempdir().unwrap();
        symlink(&elsewhere, top.path().join(format!(".Trash-{u}"))).unwrap();
        assert!(topdir_trash(top.path(), u).is_err());
    }

    #[test]
    fn an_item_off_the_home_device_goes_to_its_top_directory_trash() {
        let top = tempfile::tempdir().unwrap();
        let top_path = fs::canonicalize(top.path()).unwrap();
        let resolver_top = top_path.clone();
        let resolver = move |_: &Path| Some(resolver_top.clone());
        let f = top_path.join("sub dir/f.txt");
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(&f, b"x").unwrap();
        let env = Env {
            home_trash: None,
            uid: uid(),
            topdir_of: &resolver,
        };
        let report = recycle(&env, std::slice::from_ref(&f));
        assert_eq!(report.done, vec![f.clone()], "{report:?}");
        let trash = top_path.join(format!(".Trash-{}", uid()));
        assert!(trash.join("files/f.txt").is_file());
        let text = info(&trash, "f.txt");
        assert!(text.contains("\nPath=sub%20dir/f.txt\n"), "{text}");
    }

    #[test]
    fn the_home_trash_follows_xdg_data_home_then_home() {
        let d = home_trash_dir(Some("/x/data".into()), Some("/home/u".into()));
        assert_eq!(d, Some(PathBuf::from("/x/data/Trash")));
        let d = home_trash_dir(Some("relative".into()), Some("/home/u".into()));
        assert_eq!(d, Some(PathBuf::from("/home/u/.local/share/Trash")));
        assert_eq!(home_trash_dir(None, None), None);
    }

    #[test]
    fn encoding_and_names() {
        assert_eq!(encode_path(b"/a b/%/\xff~.-_"), "/a%20b/%25/%FF~.-_");
        assert_eq!(encode_name(b"a/b"), "a%2Fb");
        assert_eq!(decode("a%20b%2"), b"a b%2");
        assert_eq!(decode(&encode_name("ç x".as_bytes())), "ç x".as_bytes());
        let long = "é".repeat(200);
        let short = trash_base_name(OsStr::new(&long));
        assert!(short.len() <= MAX_TRASH_NAME);
        assert!(short.to_str().is_some(), "cut on a character boundary");
        assert!(date_ok(&deletion_date(std::time::SystemTime::now())));
    }

    #[test]
    fn mount_points_come_from_mountinfo() {
        let text = "22 1 8:1 / / rw - ext4 /dev/sda1 rw\n\
                    30 22 8:2 / /media/u/My\\040Disk rw - vfat /dev/sdb1 rw\n\
                    31 22 8:3 / /media/u/My rw - vfat /dev/sdc1 rw\n";
        assert_eq!(
            mount_point_in(text, Path::new("/media/u/My Disk/photos")),
            Some(PathBuf::from("/media/u/My Disk"))
        );
        assert_eq!(
            mount_point_in(text, Path::new("/media/u/Mystery")),
            Some(PathBuf::from("/"))
        );
        assert_eq!(
            mount_point_in(text, Path::new("/media/u/My/x")),
            Some(PathBuf::from("/media/u/My"))
        );
        // The real table resolves something for the temp folder.
        let d = tempfile::tempdir().unwrap();
        assert!(mount_point_of(&fs::canonicalize(d.path()).unwrap()).is_some());
    }

    #[test]
    fn io_errors_are_classified() {
        let e = io::Error::from_raw_os_error;
        assert_eq!(classify(&e(libc::ENOENT)), DeleteFailure::Missing);
        assert_eq!(classify(&e(libc::EACCES)), DeleteFailure::NeedsAdmin);
        assert_eq!(classify(&e(libc::EPERM)), DeleteFailure::NeedsAdmin);
        assert_eq!(classify(&e(libc::EBUSY)), DeleteFailure::InUse);
        assert_eq!(
            classify(&e(libc::EROFS)),
            DeleteFailure::Other("it is read-only".into())
        );
        assert!(matches!(classify(&e(libc::EIO)), DeleteFailure::Other(_)));
    }
    /// Runs `body` with the delete seam set to `hook`, cleared afterwards.
    fn with_seam(hook: impl FnMut(Seam, &Path) + 'static, body: impl FnOnce()) {
        SEAM.with(|s| *s.borrow_mut() = Some(Box::new(hook)));
        body();
        SEAM.with(|s| *s.borrow_mut() = None);
    }

    fn no_staging_left(dir: &Path) {
        let left: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".zxraw-deleting-")
            })
            .collect();
        assert!(left.is_empty(), "staging names left: {left:?}");
    }

    #[test]
    fn a_file_swapped_in_after_the_check_is_never_deleted() {
        let fx = Fixture::new();
        let a = fx.file("a", b"one");
        let Some(id_a) = ItemIdentity::of(&a) else {
            return; // no creation time on this file system
        };
        let kept = fx.root().join("a.original");
        let (a2, kept2) = (a.clone(), kept.clone());
        let mut report = None;
        with_seam(
            move |at, _| {
                if at == Seam::BeforeStage {
                    // The confirmed file moves away and a different one takes its name.
                    fs::rename(&a2, &kept2).unwrap();
                    fs::write(&a2, b"replacement").unwrap();
                }
            },
            || {
                report = Some(run_with(
                    &[a.clone()],
                    &[Some(id_a)],
                    Mode::Permanent,
                    quiet(),
                    &fx.env(),
                ));
            },
        );
        let report = report.unwrap();
        assert_eq!(report.failed, vec![(a.clone(), DeleteFailure::Changed)]);
        assert!(report.done.is_empty());
        assert_eq!(fs::read(&a).unwrap(), b"replacement");
        assert_eq!(fs::read(&kept).unwrap(), b"one");
        no_staging_left(fx.root());
    }

    #[test]
    fn a_folder_swapped_in_after_the_check_is_never_deleted() {
        let fx = Fixture::new();
        let d = fx.root().join("d");
        fx.file("d/inner.txt", b"x");
        let Some(id_d) = ItemIdentity::of(&d) else {
            return;
        };
        let kept = fx.root().join("d.original");
        let (d2, kept2) = (d.clone(), kept.clone());
        let mut report = None;
        with_seam(
            move |at, _| {
                if at == Seam::BeforeStage {
                    fs::rename(&d2, &kept2).unwrap();
                    fs::create_dir(&d2).unwrap();
                    fs::write(d2.join("precious.txt"), b"keep me").unwrap();
                }
            },
            || {
                report = Some(run_with(
                    &[d.clone()],
                    &[Some(id_d)],
                    Mode::Permanent,
                    quiet(),
                    &fx.env(),
                ));
            },
        );
        let report = report.unwrap();
        assert_eq!(report.failed, vec![(d.clone(), DeleteFailure::Changed)]);
        assert_eq!(fs::read(d.join("precious.txt")).unwrap(), b"keep me");
        assert!(kept.join("inner.txt").exists());
        no_staging_left(fx.root());
    }

    #[test]
    fn a_new_item_at_the_path_after_staging_survives_and_the_confirmed_one_goes() {
        let fx = Fixture::new();
        let d = fx.root().join("d");
        fx.file("d/inner.txt", b"x");
        let Some(id_d) = ItemIdentity::of(&d) else {
            return;
        };
        let d2 = d.clone();
        let mut report = None;
        with_seam(
            move |at, _| {
                if at == Seam::AfterCheck {
                    // The confirmed folder is staged; someone makes a new one at its path.
                    fs::create_dir(&d2).unwrap();
                    fs::write(d2.join("new.txt"), b"new").unwrap();
                }
            },
            || {
                report = Some(run_with(
                    &[d.clone()],
                    &[Some(id_d)],
                    Mode::Permanent,
                    quiet(),
                    &fx.env(),
                ));
            },
        );
        let report = report.unwrap();
        assert_eq!(report.done, vec![d.clone()], "{report:?}");
        assert!(report.failed.is_empty());
        assert_eq!(fs::read(d.join("new.txt")).unwrap(), b"new");
        assert!(!d.join("inner.txt").exists());
        no_staging_left(fx.root());
    }

    #[test]
    fn a_stopped_delete_moves_what_is_left_back_to_its_name() {
        let fx = Fixture::new();
        let d = fx.root().join("d");
        for i in 0..5 {
            fx.file(&format!("d/f{i}"), b"x");
        }
        let mut seen = 0;
        let out = destroy(Mode::Permanent, &d, &mut |_| {
            seen += 1;
            seen < 3
        });
        assert!(!out.gone);
        assert!(out.removed_inside);
        assert!(d.is_dir());
        no_staging_left(fx.root());
    }

    #[test]
    fn a_trash_others_can_write_to_is_made_private() {
        let fx = Fixture::new();
        let trash = fx.trash();
        for d in [trash.clone(), trash.join("files"), trash.join("info")] {
            fs::create_dir_all(&d).unwrap();
            fs::set_permissions(&d, fs::Permissions::from_mode(0o777)).unwrap();
        }
        let f = fx.file("f", b"x");
        let report = recycle(&fx.env(), &[f.clone()]);
        assert_eq!(report.done, vec![f]);
        for d in [trash.clone(), trash.join("files"), trash.join("info")] {
            let mode = fs::metadata(&d).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", d.display());
        }
    }

    #[test]
    fn a_top_directory_trash_open_to_others_is_repaired_and_a_foreign_one_refused() {
        // Only a mode check can be shown without another user: a trash that is ours and
        // writable by others is repaired; ownership refusal is covered by `private_dir`.
        let top = tempfile::tempdir().unwrap();
        let own = top.path().join(format!(".Trash-{}", uid()));
        fs::create_dir(&own).unwrap();
        fs::set_permissions(&own, fs::Permissions::from_mode(0o757)).unwrap();
        let (trash, _) = topdir_trash(top.path(), uid()).unwrap();
        assert_eq!(trash, own);
        assert_eq!(fs::metadata(&own).unwrap().mode() & 0o777, 0o700);
        // Owned by another uid: refused.
        assert!(private_dir(&own, Some(uid().wrapping_add(1)), false).is_err());
    }

    #[test]
    fn a_shared_trash_must_be_sticky_and_not_a_link() {
        let top = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        fs::set_permissions(elsewhere.path(), fs::Permissions::from_mode(0o1777)).unwrap();
        symlink(elsewhere.path(), top.path().join(".Trash")).unwrap();
        let (trash, _) = topdir_trash(top.path(), uid()).unwrap();
        assert_eq!(
            trash,
            top.path().join(format!(".Trash-{}", uid())),
            "a linked .Trash is not used"
        );
        assert!(!elsewhere.path().join(uid().to_string()).exists());
    }

    #[test]
    fn directory_sizes_are_disk_usage() {
        let fx = Fixture::new();
        fx.file("d/a", &[1u8; 10]);
        fx.file("d/sub/b", &[2u8; 5000]);
        let d = fx.root().join("d");
        let expected: u64 = ["d", "d/a", "d/sub", "d/sub/b"]
            .iter()
            .map(|r| fs::symlink_metadata(fx.root().join(r)).unwrap().blocks() * 512)
            .sum();
        assert_eq!(tree_size(&d), expected);
        // Matches `du -B1 -s` where coreutils is around.
        if let Ok(out) = std::process::Command::new("du")
            .args(["-B1", "-s"])
            .arg(&d)
            .output()
            && out.status.success()
        {
            let du: u64 = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(tree_size(&d), du);
        }
    }
}
