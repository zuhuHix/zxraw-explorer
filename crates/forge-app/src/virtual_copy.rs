//! Pasting or dropping "virtual" files: items another application offers as a list of
//! descriptors plus their contents (`FileGroupDescriptorW` + `FileContents`) instead of
//! paths. Explorer offers the items of an opened zip folder this way, and so do mail clients
//! for attachments.
//!
//! Our own reader rather than `IFileOperation::CopyItems` on the shell items: virtual files
//! from mail clients and browsers have no shell items at all, and this way the copy runs as
//! one of our jobs, with our progress, cancel, conflict questions, report and undo.
//!
//! The copy is done in two steps. The items are first written into a fresh hidden folder
//! inside the destination (streamed, never held whole in memory), and then moved out of it
//! like any other move, which renames them within the volume. That second step asks about
//! name collisions exactly as a paste of ordinary files does. Whatever is left in the
//! staging folder at the end (an item skipped on a conflict, a cancel) is deleted with it.
//! It is always a copy: nothing is ever deleted at the source.
//!
//! The descriptors come from another program and are untrusted: each name must be a clean
//! relative path ([`forge_core::paths::validate_relative`]: no `..`, no absolute or drive
//! paths, no `:` streams, no reserved device names), and an item that fails is reported and
//! skipped, so nothing is written outside the destination.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use forge_core::ops::{
    self, ConflictPolicy, JobCtx, OpIssue, OpKind, OpOptions, OpReport, StreamOptions, UndoRecord,
};
use forge_jobs::JobHandle;
use forge_win::file_descriptor::FileDescriptor;
use forge_win::virtual_files::{VirtualContent, VirtualFiles, VirtualSource};

use crate::actions::Event;
use crate::app::ZxrawApp;

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
/// Attributes kept from a descriptor: hidden, system and archive.
const KEPT_ATTRIBUTES: u32 = 0x2 | 0x4 | 0x20;

/// Copies the virtual files of `source` into the folder `dest` as a background job.
/// `count` is the number of items, when known (a drop knows it, a paste does not yet).
pub fn start(app: &mut ZxrawApp, source: VirtualSource, dest: PathBuf, count: Option<usize>) {
    if crate::ops_ui::refused_while_a_dialog_is_open(app, OpKind::Copy) {
        return;
    }
    if !dest.is_dir() {
        app.toast_err(format!(
            "Cannot copy the items here: {} is not a folder",
            dest.display()
        ));
        return;
    }
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dest.display().to_string());
    let title = match count {
        Some(n) => format!("Copying {n} item(s) to {name}"),
        None => format!("Copying files to {name}"),
    };
    let (tx, ctx) = (app.ops.tx.clone(), app.ops.ctx.clone());
    app.ops
        .jobs
        .transfers()
        .set_serial(app.settings.transfer_queue);
    let disks = crate::ops_ui::transfer_disks(&[], &dest);
    let unstarted = {
        let (tx, ctx, title) = (tx.clone(), ctx.clone(), title.clone());
        move || {
            let _ = tx.send(Event::Toast(format!(
                "{title} — cancelled before it started"
            )));
            ctx.request_repaint();
        }
    };
    let job_title = title.clone();
    let job_body = move |job: &JobHandle| {
        let result = run(job, source, &dest, &tx, &ctx, job_title);
        ctx.request_repaint();
        result
    };
    app.ops
        .jobs
        .spawn_queued("transfer", title, disks, job_body, unstarted);
}

fn run(
    job: &JobHandle,
    source: VirtualSource,
    dest: &Path,
    tx: &crossbeam_channel::Sender<Event>,
    ctx: &egui::Context,
    title: String,
) -> Result<(), String> {
    job.progress
        .set_current("Reading the items from the other application");
    let staging = Staging::create(dest).map_err(|e| {
        let message = format!("Could not copy into {}: {e}", dest.display());
        let _ = tx.send(Event::Error(message.clone()));
        message
    })?;
    let job_ctx = JobCtx::from_job(job);
    let extracted = match forge_win::virtual_files::with_virtual_files(source, |files| {
        let extracted = extract(files, &staging.path, dest, &job_ctx);
        scrub(&staging.path, &extracted.failed_targets);
        extracted
    }) {
        Ok(extracted) => extracted,
        Err(e) => {
            let why = e.to_string();
            let message = if why.contains(forge_win::virtual_files::CLIPBOARD_CHANGED) {
                "Nothing was pasted: the clipboard changed after Paste was pressed. Copy the \
                 items again."
                    .to_string()
            } else {
                format!("Could not read the items to paste: {why}")
            };
            let _ = tx.send(Event::Error(message.clone()));
            return Err(message);
        }
    };
    let mut report = if extracted.cancelled || job.cancel.is_cancelled() {
        OpReport {
            cancelled: true,
            ..OpReport::default()
        }
    } else {
        place(job, &staging.path, dest, tx, ctx, title)?
    };
    report.bytes = extracted.bytes;
    report.failed.extend(extracted.failed);
    report.skipped.extend(extracted.skipped);
    drop(staging);
    crate::ops_ui::finish_report(job, &report, tx, &[dest.to_path_buf()], Vec::new());
    Ok(())
}

/// What the first step wrote into the staging folder.
#[derive(Default)]
struct Extracted {
    bytes: u64,
    failed: Vec<OpIssue>,
    skipped: Vec<OpIssue>,
    /// Where the failed items would have been in the staging folder: whatever is there is
    /// removed before anything is placed (see [`scrub`]).
    failed_targets: Vec<PathBuf>,
    cancelled: bool,
}

/// The items to copy: the opened data object, or a stand-in in tests.
trait Items {
    fn items(&self) -> &[FileDescriptor];
    fn open(&self, index: usize) -> Result<Content<'_>, String>;
}

/// Writes a structured storage to the given path.
type SaveStorage<'a> = Box<dyn FnOnce(&Path) -> Result<(), String> + 'a>;

/// One item's contents.
enum Content<'a> {
    Stream(Box<dyn Read + 'a>),
    /// Writes a structured storage (an Outlook message) to the given path.
    Storage(SaveStorage<'a>),
}

impl Items for VirtualFiles {
    fn items(&self) -> &[FileDescriptor] {
        VirtualFiles::items(self)
    }

    fn open(&self, index: usize) -> Result<Content<'_>, String> {
        Ok(
            match VirtualFiles::open(self, index).map_err(|e| e.to_string())? {
                VirtualContent::Stream(reader) => Content::Stream(reader),
                VirtualContent::Storage(storage) => {
                    Content::Storage(Box::new(move |path: &Path| {
                        storage.save_as(path).map_err(|e| e.to_string())
                    }))
                }
            },
        )
    }
}

/// Writes every item of `files` into `staging`. Issues are reported under the path the item
/// would have had in `dest`.
fn extract(files: &dyn Items, staging: &Path, dest: &Path, ctx: &JobCtx<'_>) -> Extracted {
    let items = files.items();
    let mut out = Extracted::default();
    ctx.progress
        .add_total_items(items.iter().filter(|d| !d.is_dir).count() as u64);
    ctx.progress.add_total_bytes(
        items
            .iter()
            .filter(|d| !d.is_dir)
            .filter_map(|d| d.size)
            .sum(),
    );
    let mut folders: Vec<(PathBuf, SystemTime)> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if ctx.cancel.is_cancelled() {
            out.cancelled = true;
            break;
        }
        let rel = match item_path(&item.name) {
            Ok(rel) => rel,
            Err(why) => {
                out.failed.push(OpIssue::new(
                    PathBuf::from(&item.name),
                    format!("not copied: {why}"),
                ));
                continue;
            }
        };
        let shown = dest.join(&rel);
        let target = staging.join(&rel);
        // Two items with one name, as Windows compares names (the staging folder's file
        // system decides, case folding included): the first one is kept.
        let existing = std::fs::symlink_metadata(&target).ok();
        if item.is_dir {
            match existing {
                Some(md) if md.is_dir() => {}
                Some(_) => {
                    out.skipped
                        .push(OpIssue::new(shown, "offered twice, copied once"));
                    continue;
                }
                None => {
                    if let Err(e) = std::fs::create_dir_all(&target) {
                        out.failed.push(OpIssue::new(shown, e.to_string()));
                        continue;
                    }
                }
            }
            folders.extend(item.modified.map(|t| (target, t)));
            continue;
        }
        if existing.is_some() {
            out.skipped
                .push(OpIssue::new(shown, "offered twice, copied once"));
            continue;
        }
        if let Some(parent) = target.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            out.failed.push(OpIssue::new(shown, e.to_string()));
            continue;
        }
        ctx.progress.set_current(shown.display().to_string());
        match write_item(files, index, item, &shown, &target, ctx) {
            Ok(bytes) => {
                out.bytes += bytes;
                ctx.progress.add_items(1);
            }
            Err(_) if ctx.cancel.is_cancelled() => {
                out.failed_targets.push(target);
                out.cancelled = true;
                break;
            }
            Err(why) => {
                out.failed_targets.push(target);
                out.failed.push(OpIssue::new(shown, why));
                ctx.progress.add_error();
            }
        }
    }
    // Last, so writing the files inside does not change them again; innermost first.
    for (folder, modified) in folders.iter().rev() {
        let _ = filetime::set_file_mtime(folder, filetime::FileTime::from_system_time(*modified));
    }
    out
}

fn write_item(
    files: &dyn Items,
    index: usize,
    item: &FileDescriptor,
    shown: &Path,
    target: &Path,
    ctx: &JobCtx<'_>,
) -> Result<u64, String> {
    let attributes = item.attributes.unwrap_or(0);
    match files.open(index)? {
        Content::Stream(reader) => {
            let opts = StreamOptions {
                modified: item.modified,
                readonly: attributes & FILE_ATTRIBUTE_READONLY != 0,
                attributes: attributes & KEPT_ATTRIBUTES,
                ..StreamOptions::default()
            };
            ops::write_stream_atomic(reader, shown, target, opts, ctx)
                .map(|outcome| outcome.bytes)
                .map_err(|e| e.to_string())
        }
        Content::Storage(save) => {
            // Written under a temporary name and renamed when complete, like every copy: a
            // storage that fails half way never sits at the item's name.
            let temp = target.with_file_name(ops::partial_name());
            let saved = save(&temp).and_then(|()| {
                if let Some(modified) = item.modified {
                    let _ = filetime::set_file_mtime(
                        &temp,
                        filetime::FileTime::from_system_time(modified),
                    );
                }
                forge_core::paths::rename_no_replace(&temp, target).map_err(|e| e.to_string())
            });
            if let Err(why) = saved {
                let _ = std::fs::remove_file(&temp);
                return Err(why);
            }
            let bytes = std::fs::metadata(target).map(|m| m.len()).unwrap_or(0);
            ctx.progress.add_bytes(bytes);
            Ok(bytes)
        }
    }
}

/// Makes sure nothing that failed is placed: removes whatever sits at a failed item's path
/// and every temporary file left anywhere in the staging folder (it is new and ours alone,
/// so nothing in it is a link).
fn scrub(staging: &Path, failed: &[PathBuf]) {
    for path in failed {
        if let Ok(md) = std::fs::symlink_metadata(path) {
            let _ = if md.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
        }
    }
    let mut folders = vec![staging.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if ops::is_partial_name(&entry.file_name().to_string_lossy()) {
                let _ = if is_dir {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
            } else if is_dir {
                folders.push(path);
            }
        }
    }
}

/// The second step: moves what `staging` holds into `dest`, asking about name collisions as
/// a paste does. The report reads as a copy into `dest`.
fn place(
    job: &JobHandle,
    staging: &Path,
    dest: &Path,
    tx: &crossbeam_channel::Sender<Event>,
    ctx: &egui::Context,
    title: String,
) -> Result<OpReport, String> {
    let mut tops: Vec<PathBuf> = std::fs::read_dir(staging)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    tops.sort();
    if tops.is_empty() {
        return Ok(OpReport::default());
    }
    let opts = OpOptions {
        policy: ConflictPolicy::Ask,
        verify: false,
        force_copy_for_move: false,
    };
    let plan = ops::plan(OpKind::Move, &tops, dest, opts, &job.cancel).map_err(|e| {
        let _ = tx.send(Event::Error(e.to_string()));
        e.to_string()
    })?;
    let mut resolver = crate::ops_ui::conflict_resolver(tx, ctx, title, &job.cancel);
    // The bytes and items were counted while they were written; the renames add nothing.
    let quiet = forge_jobs::Progress::default();
    let report = ops::execute_with(&plan, &JobCtx::new(&job.cancel, &quiet), &mut resolver);
    Ok(as_copy(report, staging, dest))
}

/// Rewrites the move's report as the copy it is: undo deletes the copies (only while they
/// are unchanged), and issues name the destination, never the staging folder.
fn as_copy(mut report: OpReport, staging: &Path, dest: &Path) -> OpReport {
    let relabel = |path: &mut PathBuf| {
        if let Ok(rest) = path.strip_prefix(staging) {
            *path = dest.join(rest);
        }
    };
    for issue in report
        .failed
        .iter_mut()
        .chain(report.skipped.iter_mut())
        .chain(report.warnings.iter_mut())
    {
        relabel(&mut issue.path);
    }
    report.undo = std::mem::take(&mut report.undo)
        .into_iter()
        .filter_map(|record| match record {
            UndoRecord::Moved { to, .. } => {
                ops::fingerprint(&to)
                    .ok()
                    .map(|fingerprint| UndoRecord::Copied {
                        path: to,
                        fingerprint,
                    })
            }
            other => Some(other),
        })
        .collect();
    report
}

/// The relative path an item named `name` gets, or why it is refused.
pub(crate) fn item_path(name: &str) -> Result<PathBuf, String> {
    let trimmed = name.trim_end_matches(['\\', '/']);
    forge_core::paths::validate_relative(trimmed)
        .map_err(|e| e.to_string())
        .and_then(|rel| {
            let temporary = rel
                .components()
                .any(|c| ops::is_partial_name(&c.as_os_str().to_string_lossy()));
            if temporary {
                Err(format!(
                    "{name:?} looks like one of this app's temporary files"
                ))
            } else {
                Ok(rel)
            }
        })
}

/// The hidden folder the items are first written into; deleted with all it still holds when
/// dropped. It is new and ours alone, so nothing in it can be a planted link.
struct Staging {
    path: PathBuf,
}

impl Staging {
    fn create(dest: &Path) -> std::io::Result<Self> {
        let path = dest.join(ops::partial_name());
        std::fs::create_dir(&path)?;
        let _ = forge_win::set_attributes(&path, false, true);
        Ok(Self { path })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            tracing::warn!("could not remove {}: {e}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Items held in memory: a file's contents, or `None` for a storage that fails half way
    /// (it writes a little, then reports an error).
    struct Fake(
        Vec<(FileDescriptor, Option<&'static [u8]>)>,
        Vec<FileDescriptor>,
    );

    impl Fake {
        fn new(items: Vec<(&str, bool, Option<&'static [u8]>)>) -> Self {
            let list = items
                .iter()
                .map(|(name, is_dir, _)| FileDescriptor {
                    name: name.to_string(),
                    is_dir: *is_dir,
                    attributes: None,
                    size: None,
                    created: None,
                    modified: None,
                    flags: 0,
                })
                .collect::<Vec<_>>();
            let pairs = list
                .iter()
                .cloned()
                .zip(items.into_iter().map(|(_, _, data)| data))
                .collect();
            Self(pairs, list)
        }
    }

    impl Items for Fake {
        fn items(&self) -> &[FileDescriptor] {
            &self.1
        }

        fn open(&self, index: usize) -> Result<Content<'_>, String> {
            Ok(match self.0[index].1 {
                Some(bytes) => Content::Stream(Box::new(bytes)),
                None => Content::Storage(Box::new(|path: &Path| {
                    std::fs::write(path, b"half a message").unwrap();
                    Err("the storage could not be copied".into())
                })),
            })
        }
    }

    fn run_extract(fake: &Fake, staging: &Path, dest: &Path) -> Extracted {
        let (cancel, progress) = (
            forge_jobs::CancelToken::default(),
            forge_jobs::Progress::default(),
        );
        let extracted = extract(fake, staging, dest, &JobCtx::new(&cancel, &progress));
        scrub(staging, &extracted.failed_targets);
        extracted
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A storage item that fails half way is reported and leaves nothing behind that the
    /// placement step could move into the destination.
    #[test]
    fn a_failed_item_is_never_left_to_be_placed() {
        let dir = tempfile::tempdir().unwrap();
        let (staging, dest) = (dir.path().join("staging"), dir.path().join("dest"));
        std::fs::create_dir_all(&staging).unwrap();
        let fake = Fake::new(vec![
            ("good.txt", false, Some(b"fine")),
            ("mail.msg", false, None),
            ("sub", true, None),
            (r"sublso.msg", false, None),
        ]);
        let extracted = run_extract(&fake, &staging, &dest);
        assert_eq!(extracted.failed.len(), 2);
        assert_eq!(extracted.failed[0].path, dest.join("mail.msg"));
        assert_eq!(names_in(&staging), ["good.txt", "sub"]);
        assert!(names_in(&staging.join("sub")).is_empty());
    }

    /// Leftovers of a write that failed (temporary files) and an item at a failed path are
    /// removed before placement; everything else stays.
    #[test]
    fn scrub_removes_failed_items_and_temporary_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        std::fs::create_dir_all(staging.join("sub")).unwrap();
        std::fs::write(staging.join("ok.txt"), b"x").unwrap();
        std::fs::write(staging.join("bad.txt"), b"partial").unwrap();
        std::fs::write(staging.join("sub").join(".forge-partial-1234"), b"t").unwrap();
        std::fs::write(staging.join("sub").join("keep.txt"), b"x").unwrap();
        scrub(
            staging,
            &[staging.join("bad.txt"), staging.join("missing.txt")],
        );
        assert_eq!(names_in(staging), ["ok.txt", "sub"]);
        assert_eq!(names_in(&staging.join("sub")), ["keep.txt"]);
    }

    /// Names that Windows takes as the same (letter case, as the file system folds it) are
    /// copied once, the first one offered.
    #[test]
    fn names_windows_takes_as_equal_are_copied_once() {
        let dir = tempfile::tempdir().unwrap();
        let (staging, dest) = (dir.path().join("staging"), dir.path().join("dest"));
        std::fs::create_dir_all(&staging).unwrap();
        let fake = Fake::new(vec![
            ("a.txt", false, Some(b"first")),
            ("A.TXT", false, Some(b"second")),
            ("\u{3a3}.txt", false, Some(b"sigma")),
            ("\u{3c3}.txt", false, Some(b"small sigma")),
            ("Sub", true, None),
            (r"sub\b.txt", false, Some(b"b")),
            ("SUB", false, Some(b"a file named like the folder")),
        ]);
        let extracted = run_extract(&fake, &staging, &dest);
        assert!(extracted.failed.is_empty(), "{:?}", extracted.failed);
        assert_eq!(extracted.skipped.len(), 3, "{:?}", extracted.skipped);
        assert_eq!(std::fs::read(staging.join("a.txt")).unwrap(), b"first");
        assert_eq!(
            std::fs::read(staging.join("\u{3a3}.txt")).unwrap(),
            b"sigma"
        );
        assert_eq!(
            std::fs::read(staging.join("Sub").join("b.txt")).unwrap(),
            b"b"
        );
    }

    #[test]
    fn nested_names_keep_their_folders() {
        assert_eq!(item_path("a.txt").unwrap(), PathBuf::from("a.txt"));
        assert_eq!(
            item_path(r"sub\deeper\b.txt").unwrap(),
            PathBuf::from("sub").join("deeper").join("b.txt")
        );
        assert_eq!(
            item_path("sub/b.txt").unwrap(),
            PathBuf::from("sub").join("b.txt")
        );
        assert_eq!(item_path("sub\\").unwrap(), PathBuf::from("sub"));
    }

    #[test]
    fn names_that_would_leave_the_destination_are_refused() {
        for bad in [
            r"..\evil.txt",
            r"sub\..\..\evil.txt",
            "../evil.txt",
            "..",
            r"C:\Windows\evil.txt",
            r"C:evil.txt",
            r"\evil.txt",
            "/evil.txt",
            r"\\server\share\evil.txt",
            r"\\?\C:\evil.txt",
            r"\\.\PhysicalDrive0",
            "a.txt:hidden",
            "a.txt::$DATA",
            "CON",
            r"sub\nul.txt",
            "COM1.log",
            "",
            r"\",
            "a\u{1}b.txt",
            "trailing. ",
            r"sub\.forge-partial-1234\x",
            ".forge-partial-1234",
        ] {
            assert!(item_path(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_copy_report_names_the_destination_and_undoes_by_deleting_copies() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join(".forge-partial-x");
        let dest = dir.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("a.txt"), b"x").unwrap();
        let report = OpReport {
            undo: vec![UndoRecord::Moved {
                from: staging.join("a.txt"),
                to: dest.join("a.txt"),
            }],
            skipped: vec![OpIssue::new(staging.join("sub").join("b.txt"), "skipped")],
            ..OpReport::default()
        };
        let report = as_copy(report, &staging, &dest);
        assert_eq!(report.skipped[0].path, dest.join("sub").join("b.txt"));
        assert!(matches!(
            &report.undo[..],
            [UndoRecord::Copied { path, .. }] if path == &dest.join("a.txt")
        ));
    }
}
