//! Copying into and out of archive panes, and deleting inside them.
//!
//! * Out: items of an archive pane copied (F5, a drag, Ctrl+C then Ctrl+V) into a real
//!   folder are extracted with `archive_extract`'s job, so its conflict question and
//!   password retry apply. A move out of an archive copies (a toast says so).
//! * In: files copied into an archive pane are added with `archive::update` as a job into
//!   the folder the pane shows. Names that already exist are asked about once (Replace or
//!   Cancel). A move into an archive copies too: the originals stay where they are.
//! * Delete: Delete and Shift+Delete in an archive pane remove the entries after a
//!   confirmation, with the same kind of job.
//!
//! Ctrl+C and Ctrl+X in an archive pane put the entries on the Windows clipboard as virtual
//! files (see `archive_out`), for Explorer and other apps. The app also keeps an
//! [`ArchiveCopy`] record that Ctrl+V here uses while nothing newer was copied (see
//! [`archive_copy_is_newest`]): it extracts with the job below rather than reading the
//! virtual files back. A move out (Ctrl+X, F6, a Shift-drag) extracts, then removes the
//! items only once all of them were extracted, after a confirmation.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use forge_core::CoreError;
use forge_core::archive::{
    self, ArchiveEdit, ArchiveIndex, CreateOptions, CreateReport, Encryption,
};
use forge_core::ops::JobCtx;

use crate::actions::Event;
use crate::app::ZxrawApp;
use crate::archive_extract::{ExtractRequest, Place, selection_names};
use crate::archive_ui::{ArchiveView, Msg};
use crate::modal::{Confirm, ConfirmKind, Modal};

/// Job kind of archive edits (shown in the progress popup, like copies).
pub const UPDATE_KIND: &str = "archive-update";

/// Entries copied with Ctrl+C in an archive pane, for Ctrl+V in a folder.
#[derive(Debug, Clone)]
pub struct ArchiveCopy {
    pub archive: PathBuf,
    pub selection: archive::Selection,
    /// The Windows clipboard's sequence number when copied: a later copy anywhere changes it.
    pub baseline_sequence: u32,
    /// Ctrl+X: what a paste removes from the archive once everything was extracted.
    pub move_out: Option<crate::archive_out::MoveOut>,
}

/// An edit of an archive to run as a job (after a confirmation when it replaces or deletes).
#[derive(Debug, Clone)]
pub struct UpdateRequest {
    pub archive: PathBuf,
    pub edit: ArchiveEdit,
    /// The archive has encrypted entries: the edit needs its password (added files are
    /// encrypted with it). The engine refuses to add files without it in any case.
    pub encrypted: bool,
    /// Only folders are encrypted: there is no file to check a password against, so the new
    /// files' password is typed twice, as for a new encrypted archive.
    pub confirm_password: bool,
    pub title: String,
    /// The end of a move out of the archive: the archive file the items were read from. The
    /// job edits only a verified copy of it, and nothing if it changed (see
    /// [`removal_update`]).
    pub expect: Option<crate::archive_identity::ArchiveIdentity>,
}

/// The archive edit of `request`; for the end of a move (`request.expect`), only ever on a
/// verified copy of the very archive file the items were read from (see `archive_identity`):
///
/// 1. Under the edit lock, the original is opened sharing only reading (no other program can
///    write, rename or delete it; one already writing is waited for, then the removal is
///    refused), and its identity (file id, size, write time, directory hash) must be the one
///    recorded when the items were read.
/// 2. It is copied through that handle to a private temp file next to it, then closed.
/// 3. The copy is listed: an archive holding names that differ only in case is refused (its
///    rewrite would keep only the first of each), and a name more than one entry answers to
///    (ignoring case, a file or a folder) is not deleted.
/// 4. The edit runs on the copy.
/// 5. The original is opened again, sharing reading and deleting, and checked again.
/// 6. The edited copy is renamed over it through the copy's own handle.
///
/// A failure or a cancel anywhere leaves the original as it was, and the copy is removed.
fn removal_update(
    request: &UpdateRequest,
    password: Option<&archive::Password>,
    opts: &CreateOptions,
    ctx: &JobCtx<'_>,
) -> forge_core::Result<CreateReport> {
    use crate::archive_identity::{Replaced, WRITER_WAIT, hold_unchanged};

    let Some(expected) = &request.expect else {
        return archive::update(&request.archive, &request.edit, opts, ctx);
    };
    let refuse = |why: String| CoreError::Invalid(format!("{why}; nothing was removed"));
    let copy = hold_unchanged(&request.archive, expected, WRITER_WAIT)
        .and_then(|held| held.copy_to_private())
        .map_err(|why| refuse(format!("the archive {why}")))?;
    let listing = archive::list_with_password(copy.path(), password)?;
    if archive::stage::has_shared_names(&listing) {
        return Err(refuse(
            "the archive holds names that differ only in case, which an edit would merge".into(),
        ));
    }
    let (delete, ambiguous) = archive::stage::unambiguous_names(&listing, &request.edit.delete);
    if delete.is_empty() {
        return Err(refuse(
            "every name moved is shared by another entry of the archive".into(),
        ));
    }
    let edit = ArchiveEdit {
        add: request.edit.add.clone(),
        delete,
    };
    let mut report = archive::update(copy.path(), &edit, opts, ctx)?;
    if ambiguous > 0 {
        report.warnings.push(format!(
            "{} kept in the archive: another entry has the same name",
            items(ambiguous)
        ));
    }
    match copy
        .replace(&request.archive, expected)
        .map_err(|why| refuse(format!("the archive {why}")))?
    {
        Replaced::Original => {}
        Replaced::MovedAway => report.warnings.push(
            "the archive was moved away at the last moment: its edited copy was put at its \
             path, and the moved file is unchanged"
                .into(),
        ),
    }
    Ok(report)
}

/// Whether adding to an archive needs a password, and whether it must be typed twice: only
/// folders are encrypted, so there is no file to check it against.
pub fn password_needs(listing: &forge_core::archive::ArchiveListing) -> (bool, bool) {
    let encrypted = listing.any_encrypted();
    let files = listing.entries.iter().any(|e| e.encrypted && !e.is_dir);
    (encrypted, encrypted && !files)
}

/// "1 item" / "3 items".
pub fn items(n: usize) -> String {
    if n == 1 {
        "1 item".into()
    } else {
        format!("{n} items")
    }
}

// ---------------------------------------------------------------------------
// Where paths are
// ---------------------------------------------------------------------------

/// The archive view of a pane that lists `path` (a path inside an archive, not the archive).
pub(crate) fn view_listing(app: &ZxrawApp, path: &Path) -> Option<ArchiveView> {
    panes(app)
        .filter_map(|p| p.archive.as_ref())
        .find(|v| path != v.archive && path.starts_with(&v.archive))
        .cloned()
}

/// The archive and folder inside it that `target` names when something is copied there:
/// the folder an archive pane shows (its root is the archive's own path) or a folder in it.
/// `Err` when `target` is inside an archive but not a folder of it.
fn archive_folder(app: &ZxrawApp, target: &Path) -> Option<Result<(ArchiveView, String), ()>> {
    let view = panes(app)
        .filter_map(|p| p.archive.as_ref())
        .find(|v| target.starts_with(&v.archive))?;
    let inner = view.inner_of(target).unwrap_or_default();
    Some(if view.index.is_dir(&inner) {
        // The node's own spelling, as the archive stores it.
        let folder = view.index.node(&inner).map_or(inner, |n| n.path.clone());
        Ok((view.clone(), folder))
    } else {
        Err(())
    })
}

fn panes(app: &ZxrawApp) -> impl Iterator<Item = &crate::pane::Pane> {
    app.tabs
        .iter()
        .flat_map(|t| std::iter::once(&t.left).chain(t.right.as_ref()))
}

/// The folder inside the archive that holds all of `inner` paths (`""`: the root), so a
/// selection extracts relative to it, as Explorer copies items without their parents.
pub fn common_folder<'a>(inner: impl IntoIterator<Item = &'a str>) -> String {
    let mut common: Option<Vec<&str>> = None;
    for path in inner {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let parent = &parts[..parts.len().saturating_sub(1)];
        common = Some(match common {
            None => parent.to_vec(),
            Some(c) => c
                .iter()
                .zip(parent)
                .take_while(|(a, b)| a.to_lowercase() == b.to_lowercase())
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    common.unwrap_or_default().join("/")
}

/// The extraction selection of listed paths in `view`; `None` if none is in the archive.
pub fn selection_of(view: &ArchiveView, paths: &[PathBuf]) -> Option<archive::Selection> {
    let nodes: Vec<(String, bool)> = paths
        .iter()
        .filter_map(|p| view.node_of(p))
        .map(|n| (n.path.clone(), n.is_dir))
        .collect();
    if nodes.is_empty() {
        return None;
    }
    let base = common_folder(nodes.iter().map(|(p, _)| p.as_str()));
    Some(archive::Selection::new(base, selection_names(nodes)))
}

// ---------------------------------------------------------------------------
// Copy and move
// ---------------------------------------------------------------------------

/// A copy or move of `paths` into `target` that involves an archive pane: extracted when
/// the items come from one, added when the target is one. Returns false when neither is the
/// case (the caller transfers as usual). `what` names the command in toasts.
pub fn transfer(
    app: &mut ZxrawApp,
    paths: &[PathBuf],
    target: &Path,
    copy: bool,
    what: &str,
) -> bool {
    let from_archive = paths.iter().any(|p| view_listing(app, p).is_some());
    let into = archive_folder(app, target);
    let source = paths.iter().find_map(|p| view_listing(app, p));
    match (from_archive, into) {
        (false, None) => false,
        // Into the archive the items come from (a drag back onto its pane): nothing to do.
        (true, Some(Ok((view, _))))
            if source
                .is_some_and(|s| forge_core::paths::paths_equal(&s.archive, &view.archive)) =>
        {
            true
        }
        (true, Some(_)) => {
            app.toast(format!(
                "{what}: from one archive into another is not available yet"
            ));
            true
        }
        (_, Some(Err(()))) => {
            app.toast(format!("{what}: not a folder inside the archive"));
            true
        }
        (true, None) => {
            copy_out(app, paths, target, copy);
            true
        }
        (false, Some(Ok((view, _)))) if view.read_only().is_some() => {
            app.toast(format!("{what}: {}", view.read_only().unwrap_or_default()));
            true
        }
        (false, Some(Ok((view, folder)))) => {
            add_into(app, &view, &folder, paths.to_vec(), copy);
            true
        }
    }
}

/// Extract archive items listed by a pane into the real folder `target`.
fn copy_out(app: &mut ZxrawApp, paths: &[PathBuf], target: &Path, copy: bool) {
    let Some(view) = paths.iter().find_map(|p| view_listing(app, p)) else {
        return;
    };
    if paths
        .iter()
        .any(|p| !(p != &view.archive && p.starts_with(&view.archive)))
    {
        app.toast("Copy items from one archive at a time");
        return;
    }
    let Some(selection) = selection_of(&view, paths) else {
        app.toast("Nothing to copy: the items are no longer in the archive");
        return;
    };
    let then_remove = if copy {
        None
    } else {
        crate::archive_out::MoveOut::in_app(&view, &selection)
    };
    extract_selection(app, view.archive, selection, target.to_path_buf(), then_remove);
}

fn extract_selection(
    app: &mut ZxrawApp,
    archive: PathBuf,
    selection: archive::Selection,
    dest: PathBuf,
    then_remove: Option<crate::archive_out::MoveOut>,
) {
    crate::archive_extract::start_extract(
        app,
        ExtractRequest {
            archive,
            parent: dest,
            place: Place::Into,
            selection: Some(selection),
            policy: None,
            allow_unchecked: false,
            then_remove,
        },
    );
}

/// Ctrl+C in an archive pane: offer the selected entries on the Windows clipboard, and
/// remember them for Ctrl+V in a folder here.
pub fn copy(app: &mut ZxrawApp) {
    let targets = app.pane().action_targets();
    copy_paths(app, &targets, false);
}

/// Ctrl+X in an archive pane: as [`copy`], and a paste moves the entries out.
pub fn cut(app: &mut ZxrawApp) {
    let targets = app.pane().action_targets();
    copy_paths(app, &targets, true);
}

/// Copy or cut listed archive `paths` (see [`copy`]).
pub fn copy_paths(app: &mut ZxrawApp, paths: &[PathBuf], cut: bool) {
    let Some(view) = paths.first().and_then(|p| view_listing(app, p)) else {
        return;
    };
    if cut && let Some(why) = view.read_only() {
        app.toast(format!("Cut: {why}. Copy the items instead."));
        return;
    }
    let Some(selection) = selection_of(&view, paths) else {
        return;
    };
    let n = selection.names.len();
    let move_out = match crate::archive_out::publish(app, paths, cut) {
        crate::archive_out::Published::Asking => return,
        crate::archive_out::Published::Yes(moved) => cut.then_some(moved),
        crate::archive_out::Published::No => {
            if cut {
                crate::archive_out::MoveOut::in_app(&view, &selection)
            } else {
                None
            }
        }
    };
    // Newer than any file list copied before: that one no longer pastes.
    app.ops.supersede_file_clipboard();
    app.ops.archive.copied = Some(ArchiveCopy {
        archive: view.archive,
        selection,
        baseline_sequence: forge_win::clipboard_sequence(),
        move_out,
    });
    app.toast(if cut {
        format!(
            "Cut {} from the archive: paste in a folder (here or in Explorer) to move them out",
            items(n)
        )
    } else {
        format!("Copied {} from the archive", items(n))
    });
}

/// Whether Ctrl+V should paste the archive copy rather than a file list: nothing else was
/// copied since (the clipboard sequence is unchanged, or what changed it is this app's own
/// older list), or the clipboard holds no files (text copied since does not count, as the
/// in-app clipboard works).
pub fn archive_copy_is_newest(
    baseline_sequence: u32,
    system_sequence: u32,
    owner_is_this_app: bool,
    system_has_files: Option<bool>,
) -> bool {
    system_sequence == baseline_sequence || owner_is_this_app || system_has_files == Some(false)
}

/// Ctrl+V of an archive copy into `dest`.
pub fn paste_archive_copy(app: &mut ZxrawApp, copied: ArchiveCopy, dest: PathBuf) {
    if archive_folder(app, &dest).is_some() {
        app.toast("Paste: from one archive into another is not available yet");
        return;
    }
    extract_selection(app, copied.archive, copied.selection, dest, copied.move_out);
}

/// Whether `dest` is inside an archive some pane shows (a paste there adds to it).
pub fn is_archive_target(app: &ZxrawApp, dest: &Path) -> bool {
    archive_folder(app, dest).is_some()
}

/// Paste files into the archive folder `dest` (see [`add_into`]).
pub fn paste_into(app: &mut ZxrawApp, paths: Vec<PathBuf>, dest: &Path, copy: bool) {
    transfer(app, &paths, dest, copy, "Paste");
}

/// Names of `sources` (files and folders) that already exist in `folder` of the archive.
pub fn existing_names(index: &ArchiveIndex, folder: &str, sources: &[PathBuf]) -> Vec<String> {
    sources
        .iter()
        .filter_map(|s| s.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|name| {
            let inner = if folder.is_empty() {
                name.clone()
            } else {
                format!("{folder}/{name}")
            };
            index.node(&inner).is_some()
        })
        .collect()
}

/// Add real files to `folder` of the archive `view` shows, asking first when names exist.
fn add_into(
    app: &mut ZxrawApp,
    view: &ArchiveView,
    folder: &str,
    sources: Vec<PathBuf>,
    copy: bool,
) {
    if sources.is_empty() {
        return;
    }
    // The archive itself (or a folder holding it) can't go into it.
    if sources.iter().any(|s| view.archive.starts_with(s)) {
        app.toast("An archive can't be added to itself");
        return;
    }
    if !copy {
        app.toast("Items are copied into the archive: the originals stay where they are");
    }
    let name = forge_core::paths::name_of(&view.archive);
    let dest = if folder.is_empty() {
        String::new()
    } else {
        format!("{folder}/")
    };
    let existing = existing_names(&view.index, folder, &sources);
    let lock = password_needs(&view.index.listing);
    let request = UpdateRequest {
        archive: view.archive.clone(),
        title: format!("Adding {} to {name}", items(sources.len())),
        edit: ArchiveEdit {
            add: sources.into_iter().map(|s| (s, dest.clone())).collect(),
            delete: Vec::new(),
        },
        encrypted: lock.0,
        confirm_password: lock.1,
        expect: None,
    };
    if existing.is_empty() {
        return start_update(app, request);
    }
    let shown = if folder.is_empty() {
        name.clone()
    } else {
        format!("{name}\\{}", folder.replace('/', "\\"))
    };
    app.ops.modal = Some(Modal::Confirm(Confirm {
        body: format!(
            "{}\n\nReplace them? This can't be undone.",
            crate::archive_extract::conflict_text(existing.len(), &existing[0], Path::new(&shown))
        ),
        kind: ConfirmKind::ArchiveUpdate(request),
        title: "Items already exist".into(),
        confirm_label: "Replace".into(),
        danger: true,
    }));
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// Delete or Shift+Delete in an archive pane: confirm, then remove the entries.
pub fn ask_delete(app: &mut ZxrawApp) {
    let Some(view) = app.pane().archive.clone() else {
        return;
    };
    if let Some(why) = view.read_only() {
        app.toast(format!("Delete: {why}"));
        return;
    }
    let targets = app.pane().action_targets();
    let nodes: Vec<(String, bool)> = targets
        .iter()
        .filter_map(|p| view.node_of(p))
        .map(|n| (n.path.clone(), n.is_dir))
        .collect();
    if nodes.is_empty() {
        if !app.pane().selected.is_empty() {
            app.toast("Nothing to delete: the selected items are hidden");
        }
        return;
    }
    let name = forge_core::paths::name_of(&view.archive);
    let n = nodes.len();
    app.ops.modal = Some(Modal::Confirm(Confirm {
        body: format!(
            "Delete {} from {name}? This can't be undone.\n\n{}",
            items(n),
            crate::modal::describe_items(&targets)
        ),
        kind: ConfirmKind::ArchiveUpdate(UpdateRequest {
            archive: view.archive.clone(),
            title: format!("Deleting {} from {name}", items(n)),
            edit: ArchiveEdit {
                add: Vec::new(),
                delete: selection_names(nodes),
            },
            // Kept entries are copied as stored: deleting needs no password.
            encrypted: false,
            confirm_password: false,
            expect: None,
        }),
        title: "Delete from archive".into(),
        confirm_label: "Delete".into(),
        danger: true,
    }));
}

// ---------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------

/// One archive edit at a time: two rebuilding the same archive would lose one's changes.
static UPDATING: Mutex<()> = Mutex::new(());

/// "Added 3 file(s) to a.zip (1 replaced)" / "Deleted 2 items from a.zip".
pub fn update_summary(report: &CreateReport, edit: &ArchiveEdit, name: &str) -> String {
    let mut s = if edit.add.is_empty() {
        format!("Deleted {} from {name}", items(edit.delete.len()))
    } else {
        format!("Added {} file(s) to {name}", report.files)
    };
    if !edit.add.is_empty() && report.replaced > 0 {
        s.push_str(&format!(" ({} replaced)", report.replaced));
    }
    if !report.skipped.is_empty() {
        s.push_str(&format!(", {} skipped", report.skipped.len()));
    }
    s
}

/// Run an archive edit as a job; the panes showing the archive are listed again after.
pub fn start_update(app: &mut ZxrawApp, request: UpdateRequest) {
    // Always the archive's own password: new entries never go in unencrypted, or encrypted
    // with a different one. Without it, ask first; the engine checks it before rebuilding.
    let password = app.ops.archive.passwords.get(&request.archive).cloned();
    if request.encrypted && password.is_none() {
        crate::archive_ui::ask_password(app, crate::archive_ui::Retry::Update(request), false);
        return;
    }
    let msgs = app.ops.archive.tx.clone();
    let events = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    let title = request.title.clone();
    app.ops.jobs.spawn(UPDATE_KIND, title, move |job| {
        let job_ctx = JobCtx::from_job(job);
        // Encrypted 7z names need it to list the archive again.
        let list_password = password.clone();
        let opts = CreateOptions {
            encryption: password.map_or(Encryption::None, Encryption::Aes256),
            ..CreateOptions::default()
        };
        let name = forge_core::paths::name_of(&request.archive);
        let result = {
            job.progress.set_note("Waiting for another archive edit…");
            let _one = UPDATING.lock().unwrap_or_else(|p| p.into_inner());
            job.progress.set_note("");
            removal_update(&request, list_password.as_ref(), &opts, &job_ctx)
        };
        let outcome = match result {
            Ok(report) => {
                let summary = update_summary(&report, &request.edit, &name);
                job.log(summary.clone());
                let event = if report.skipped.is_empty() && report.warnings.is_empty() {
                    Event::Toast(summary)
                } else {
                    let mut lines = vec![summary];
                    lines.extend(report.warnings.iter().cloned());
                    for s in &report.skipped {
                        lines.push(format!("skipped {}: {}", s.path.display(), s.reason));
                    }
                    let title = if report.skipped.is_empty() {
                        job.title.to_string()
                    } else {
                        format!("{} — some files skipped", job.title)
                    };
                    Event::Report { title, lines }
                };
                let _ = events.send(event);
                Ok(())
            }
            Err(CoreError::Cancelled) => {
                let _ = events.send(Event::Toast(format!(
                    "{} — cancelled; {name} is unchanged",
                    job.title
                )));
                Ok(())
            }
            // Nothing was written: ask (again) and run the edit once the password is typed.
            Err(e @ (CoreError::PasswordRequired(_) | CoreError::WrongPassword(_))) => {
                let wrong = matches!(e, CoreError::WrongPassword(_));
                let retry = crate::archive_ui::Retry::Update(UpdateRequest {
                    encrypted: true,
                    ..request.clone()
                });
                let _ = msgs.send(crate::archive_ui::Msg::NeedPassword { retry, wrong });
                Ok(())
            }
            // Said as it is: a split zip, for one, is left untouched.
            Err(CoreError::Unsupported(msg)) => {
                let _ = events.send(Event::Error(format!("{name}: {msg}")));
                Err(msg)
            }
            Err(e) => {
                let _ = events.send(Event::Error(format!("{} failed: {e}", job.title)));
                Err(e.to_string())
            }
        };
        let _ = msgs.send(Msg::Updated(request.archive.clone()));
        // Panes in the archive are listed again when its folder changes (its index was
        // dropped from the cache by the update).
        if let Some(dir) = request.archive.parent() {
            let _ = events.send(Event::Changed(dir.to_path_buf()));
        }
        ctx.request_repaint();
        outcome
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dnd::win;
    use forge_core::archive::{ArchiveEntry, ArchiveFormat, ArchiveListing};
    use std::sync::Arc;

    fn entry(name: &str) -> ArchiveEntry {
        ArchiveEntry {
            index: 0,
            name: name.into(),
            is_dir: name.ends_with('/'),
            is_symlink: false,
            encrypted: false,
            encryption: None,
            size: 1,
            compressed_size: 1,
            modified: None,
            crc32: 0,
            method: String::new(),
            safe_name: true,
            ..Default::default()
        }
    }

    fn view(names: &[&str]) -> ArchiveView {
        ArchiveView {
            archive: win(r"D:\x\a.zip"),
            inner: String::new(),
            version: 0,
            index: Arc::new(ArchiveIndex::build(ArchiveListing {
                format: ArchiveFormat::Zip,
                entries: names.iter().map(|n| entry(n)).collect(),
                comment: String::new(),
            })),
        }
    }

    /// A zip of `entries` (a name ending with `/` is a folder).
    fn zip_of(path: &Path, entries: &[&str]) {
        use std::io::Write;
        let mut zw = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        for name in entries {
            if name.ends_with('/') {
                zw.add_directory(*name, options).unwrap();
            } else {
                zw.start_file(*name, options).unwrap();
                zw.write_all(name.as_bytes()).unwrap();
            }
        }
        zw.finish().unwrap();
    }

    fn removal(zip: &Path, delete: &[&str]) -> forge_core::Result<CreateReport> {
        let request = UpdateRequest {
            archive: zip.to_path_buf(),
            edit: ArchiveEdit {
                add: Vec::new(),
                delete: delete.iter().map(|s| s.to_string()).collect(),
            },
            encrypted: false,
            confirm_password: false,
            title: "Removing".into(),
            expect: Some(crate::archive_identity::read(zip).unwrap()),
        };
        let (cancel, progress) = (forge_jobs::CancelToken::new(), forge_jobs::Progress::default());
        removal_update(
            &request,
            None,
            &CreateOptions::default(),
            &JobCtx::new(&cancel, &progress),
        )
    }

    fn names_in(zip: &Path) -> Vec<String> {
        let mut names: Vec<String> = archive::list(zip)
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.name)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_removal_keeps_names_another_entry_shares() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, &["A.txt", "a.txt", "top.txt"]);
        assert_eq!(names_in(&zip), ["A.txt", "a.txt", "top.txt"], "as written");
        // "A.txt" would delete "a.txt" too, and a rewrite would keep only one of them:
        // nothing is removed at all.
        for delete in [&["A.txt", "top.txt"][..], &["top.txt"][..]] {
            let error = removal(&zip, delete).unwrap_err().to_string();
            assert!(error.contains("nothing was removed"), "{error}");
            assert_eq!(names_in(&zip), ["A.txt", "a.txt", "top.txt"]);
        }
    }

    #[test]
    fn a_folder_moved_out_keeps_a_file_of_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, &["docs", "docs/", "docs/in.txt"]);
        // The folder entry's name is also the file's: the file must stay, so nothing goes.
        let error = removal(&zip, &["docs", "docs/in.txt"]).unwrap_err().to_string();
        assert!(error.contains("nothing was removed"), "{error}");
        assert_eq!(names_in(&zip), ["docs", "docs/", "docs/in.txt"]);
    }

    #[test]
    fn a_removal_refuses_an_archive_rewritten_with_its_size_and_time() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_of(&zip, &["one.txt", "top.txt"]);
        let request_identity = crate::archive_identity::read(&zip).unwrap();
        let modified = std::fs::metadata(&zip).unwrap().modified().unwrap();
        let other = dir.path().join("b.zip");
        zip_of(&other, &["two.txt", "top.txt"]);
        std::fs::write(&zip, std::fs::read(&other).unwrap()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&zip)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let request = UpdateRequest {
            archive: zip.clone(),
            edit: ArchiveEdit {
                add: Vec::new(),
                delete: vec!["top.txt".into()],
            },
            encrypted: false,
            confirm_password: false,
            title: "Removing".into(),
            expect: Some(request_identity),
        };
        let (cancel, progress) = (forge_jobs::CancelToken::new(), forge_jobs::Progress::default());
        let error = removal_update(
            &request,
            None,
            &CreateOptions::default(),
            &JobCtx::new(&cancel, &progress),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("nothing was removed"), "{error}");
        assert_eq!(names_in(&zip), ["top.txt", "two.txt"]);
    }

    #[test]
    fn items_are_copied_relative_to_their_common_folder() {
        assert_eq!(common_folder(["a.txt"]), "");
        assert_eq!(common_folder(["sub/a.txt", "sub/b"]), "sub");
        assert_eq!(common_folder(["sub/x/a.txt", "Sub/y.txt"]), "sub");
        assert_eq!(common_folder(["sub/x/a.txt", "other.txt"]), "");
        assert_eq!(common_folder(std::iter::empty()), "");

        let v = view(&["sub/", "sub/a.txt", "sub/deep/b.txt", "top.txt"]);
        let sel = selection_of(
            &v,
            &[
                win(r"D:\x\a.zip\sub\a.txt"),
                win(r"D:\x\a.zip\sub\deep"),
            ],
        )
        .unwrap();
        assert_eq!(sel.base, "sub/");
        assert_eq!(sel.names, ["sub/a.txt", "sub/deep/"]);
        assert!(selection_of(&v, &[win(r"D:\x\a.zip\missing")]).is_none());
    }

    #[test]
    fn encrypted_folders_alone_ask_for_the_password_twice() {
        let listing = |locked: &[(&str, bool)]| ArchiveListing {
            format: ArchiveFormat::Zip,
            entries: locked
                .iter()
                .map(|&(n, enc)| ArchiveEntry {
                    encrypted: enc,
                    ..entry(n)
                })
                .collect(),
            comment: String::new(),
        };
        assert_eq!(
            password_needs(&listing(&[("a.txt", false)])),
            (false, false)
        );
        assert_eq!(
            password_needs(&listing(&[("a/", true), ("b.txt", false)])),
            (true, true)
        );
        assert_eq!(
            password_needs(&listing(&[("a/", true), ("a/b.txt", true)])),
            (true, false)
        );
    }

    #[test]
    fn existing_names_are_found_in_the_target_folder_ignoring_case() {
        let v = view(&["sub/A.txt", "b.txt"]);
        let sources = [win(r"C:\in\a.txt"), win(r"C:\in\b.txt")];
        assert_eq!(existing_names(&v.index, "sub", &sources), ["a.txt"]);
        assert_eq!(existing_names(&v.index, "", &sources), ["b.txt"]);
    }

    #[test]
    fn the_archive_copy_wins_until_something_else_is_copied() {
        // Nothing copied since.
        assert!(archive_copy_is_newest(7, 7, false, Some(true)));
        // Files copied in Explorer since.
        assert!(!archive_copy_is_newest(7, 8, false, Some(true)));
        // This app's own older list landed on the clipboard after the archive copy.
        assert!(archive_copy_is_newest(7, 8, true, Some(true)));
        // Only text was copied since.
        assert!(archive_copy_is_newest(7, 8, false, Some(false)));
        // The clipboard changed and could not be read: not pasted from an old record.
        assert!(!archive_copy_is_newest(7, 8, false, None));
    }

    #[test]
    fn summaries_and_titles_count_items() {
        assert_eq!(items(1), "1 item");
        assert_eq!(items(3), "3 items");
        let add = ArchiveEdit {
            add: vec![(PathBuf::from(r"C:\a.txt"), String::new())],
            delete: Vec::new(),
        };
        let report = CreateReport {
            files: 2,
            replaced: 1,
            ..Default::default()
        };
        assert_eq!(
            update_summary(&report, &add, "a.zip"),
            "Added 2 file(s) to a.zip (1 replaced)"
        );
        let del = ArchiveEdit {
            add: Vec::new(),
            delete: vec!["x/".into()],
        };
        let report = CreateReport {
            deleted: 4,
            ..Default::default()
        };
        assert_eq!(
            update_summary(&report, &del, "a.zip"),
            "Deleted 1 item from a.zip"
        );
    }
}
