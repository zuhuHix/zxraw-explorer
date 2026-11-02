//! Archive items out to other apps: a drag of items of an archive pane onto Explorer, the
//! desktop or a mail client, and Ctrl+C / Ctrl+X there, offer them as virtual files
//! (`forge_win::virtual_source`), the way Explorer's zip folders do. Their data is
//! extracted on demand by an `archive::stage::Stage`, so the checks of an extraction apply:
//! checksums, limits, safe names, held-back unverifiable files.
//!
//! A move (Ctrl+X pasted elsewhere, a Shift-drag, or a move inside the app) removes the
//! items from the archive only once the target confirmed it and read every file, the
//! archive is unchanged since it was listed, and the user said yes to the same kind of
//! confirmation Delete asks for (see [`MoveOut`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use forge_core::archive::stage::{Stage, StageEvent};
use forge_core::archive::{self, ExtractLimits, ExtractReport};
use forge_win::virtual_source::{VirtualDataObject, VirtualItem, VirtualOffer, VirtualProvider};

use crate::app::ZxrawApp;
use crate::archive_edit::{UpdateRequest, items};
use crate::archive_ui::ArchiveView;
use crate::modal::{Confirm, ConfirmKind, Modal};

/// Items to remove from an archive once a move out of it is confirmed.
#[derive(Debug, Clone)]
pub struct MoveOut {
    pub archive: PathBuf,
    /// [`crate::archive_ui::archive_version`] when the items were listed: nothing is removed
    /// from an archive that changed since.
    pub version: u64,
    /// Selection names (a folder ends with `/` and covers what is inside).
    pub names: Vec<String>,
    /// The data object it came from, if any (taken off the clipboard after the move).
    offer: Option<u64>,
}

impl MoveOut {
    /// The move of `names` out of the archive `view` shows, done inside this app.
    pub fn in_app(view: &ArchiveView, names: Vec<String>) -> Self {
        Self {
            archive: view.archive.clone(),
            version: view.version,
            names,
            offer: None,
        }
    }
}

enum OutMsg {
    /// A target (or an extraction here) confirmed a move: check the archive, then ask.
    Moved(MoveOut),
    Checked { moved: MoveOut, unchanged: bool },
    Stage { archive: PathBuf, event: StageEvent },
}

/// Archive items on the clipboard and the messages of their stages (in `app.ops.archive`).
pub struct OutState {
    tx: Sender<OutMsg>,
    rx: Receiver<OutMsg>,
    /// The data object last put on the clipboard, and its id.
    clipboard: Option<(u64, VirtualDataObject)>,
    checked_clipboard: Option<Instant>,
    /// Automated runs: drags of archive items that got as far as their data object.
    pub drag_offers: usize,
}

impl Default for OutState {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self {
            tx,
            rx,
            clipboard: None,
            checked_clipboard: None,
            drag_offers: 0,
        }
    }
}

impl OutState {
    /// Whether the clipboard still holds the archive items this app put there.
    pub fn on_clipboard(&self) -> bool {
        self.clipboard
            .as_ref()
            .is_some_and(|(_, data)| data.is_on_clipboard())
    }

    /// For extraction jobs: report an in-app move that extracted everything.
    pub fn sender(&self) -> OutSender {
        OutSender(self.tx.clone())
    }
}

/// Sends a finished in-app move out of an archive (see [`OutSender::extracted`]).
#[derive(Clone)]
pub struct OutSender(Sender<OutMsg>);

impl OutSender {
    /// An extraction that was the first half of a move finished: the items are removed from
    /// the archive only when every one of them was extracted.
    pub fn extracted(&self, moved: &MoveOut, report: &ExtractReport, ctx: &egui::Context) {
        if move_extracted_everything(report) {
            let _ = self.0.send(OutMsg::Moved(moved.clone()));
            ctx.request_repaint();
        }
    }
}

/// Whether an extraction left nothing behind: no failure, skip, held-back file or cancel.
pub fn move_extracted_everything(report: &ExtractReport) -> bool {
    !report.cancelled
        && report.failed.is_empty()
        && report.skipped.is_empty()
        && report.unchecked.is_empty()
}

/// Reads the staged files of one offer.
struct Provider {
    stage: Stage,
    /// Per offered item: its file in the stage (`None` for folders).
    files: Vec<Option<usize>>,
    on_moved: Box<dyn Fn() + Send + Sync>,
}

impl VirtualProvider for Provider {
    fn open(&self, index: usize) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        let k = self.files.get(index).copied().flatten().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "not a file")
        })?;
        Ok(Box::new(self.stage.open(k)?))
    }

    fn moved(&self) {
        (self.on_moved)();
    }
}

static NEXT_OFFER: AtomicU64 = AtomicU64::new(1);

/// The archive pane view that lists all of `paths`.
fn view_of(app: &ZxrawApp, paths: &[PathBuf]) -> Option<ArchiveView> {
    let view = crate::archive_edit::view_listing(app, paths.first()?)?;
    paths
        .iter()
        .all(|p| p != &view.archive && p.starts_with(&view.archive))
        .then_some(view)
}

/// Builds the data object for `paths` of an archive pane. `None` after telling the user
/// why not (a password is asked for first when one is needed and not known).
/// `Err(true)`: the password is being asked for.
fn offer(
    app: &mut ZxrawApp,
    paths: &[PathBuf],
    cut: bool,
    retry_paths: bool,
) -> Result<(VirtualDataObject, MoveOut), bool> {
    let Some(view) = view_of(app, paths) else {
        app.toast("Copy items from one archive at a time");
        return Err(false);
    };
    let selection = crate::archive_edit::selection_of(&view, paths).ok_or(false)?;
    let limits = ExtractLimits::default();
    let plan = match archive::stage::plan(&view.index.listing, &selection, &limits) {
        Ok(plan) => plan,
        Err(e) => {
            app.toast_err(format!("Can't offer the items to other apps: {e}"));
            return Err(false);
        }
    };
    if plan.items.is_empty() {
        app.toast("Nothing to copy: the items hold no files the archive can give out");
        return Err(false);
    }
    let password = app.ops.archive.passwords.get(&view.archive).cloned();
    if plan.any_encrypted() && password.is_none() {
        // Asked now, never from inside a drag or a paste.
        let then = retry_paths.then(|| (paths.to_vec(), cut));
        crate::archive_ui::ask_password(
            app,
            crate::archive_ui::Retry::Out {
                archive: view.archive.clone(),
                then,
            },
            false,
        );
        return Err(true);
    }
    if !plan.skipped.is_empty() {
        app.toast(format!(
            "Left out {} (links, or names that clash on Windows)",
            items(plan.skipped.len())
        ));
    }
    let id = NEXT_OFFER.fetch_add(1, Ordering::Relaxed);
    let moved = MoveOut {
        archive: view.archive.clone(),
        version: view.version,
        names: selection.names.clone(),
        offer: Some(id),
    };
    let items: Vec<VirtualItem> = plan
        .items
        .iter()
        .map(|item| VirtualItem {
            path: item.rel.to_string_lossy().into_owned(),
            is_dir: item.is_dir,
            size: item.size,
            modified: item.modified,
        })
        .collect();
    let files = plan.items.iter().map(|item| item.file).collect();
    let ctx = app.ops.ctx.clone();
    let tx = app.ops.archive.out.tx.clone();
    let (archive_path, version) = (view.archive.clone(), view.version);
    let dir = archive::temp_root().join(format!("out-{}-{id}", std::process::id()));
    let stage = {
        let (tx, ctx, archive) = (tx.clone(), ctx.clone(), archive_path.clone());
        let still = archive_path.clone();
        Stage::new(
            archive_path.clone(),
            plan.files,
            password,
            limits,
            dir,
            move || crate::archive_ui::archive_version(&still) == version,
            move |event| {
                let _ = tx.send(OutMsg::Stage {
                    archive: archive.clone(),
                    event,
                });
                ctx.request_repaint();
            },
        )
    };
    let told = moved.clone();
    let provider = Provider {
        stage,
        files,
        on_moved: Box::new(move || {
            let _ = tx.send(OutMsg::Moved(told.clone()));
            ctx.request_repaint();
        }),
    };
    let offer = VirtualOffer {
        items,
        preferred_move: cut,
        own_paths: paths.to_vec(),
    };
    match VirtualDataObject::new(offer, Arc::new(provider)) {
        Ok(data) => Ok((data, moved)),
        Err(e) => {
            app.toast_err(format!("Can't offer the items to other apps: {e}"));
            Err(false)
        }
    }
}

/// A drag of archive items left the window: run the OS drag of their virtual files.
/// Explorer copies them (moves them with Shift: removed here once it confirmed the move).
pub fn drag(
    app: &mut ZxrawApp,
    paths: &[PathBuf],
    first_is_dir: bool,
    thumbnail: Option<forge_win::dragout::DragThumbnail<'_>>,
) {
    let Ok((data, _)) = offer(app, paths, false, false) else {
        return;
    };
    // The OLE drag is modal; a scripted run has no one to drop it and would hang.
    if app.testhook.active() {
        app.ops.archive.out.drag_offers += 1;
        tracing::info!("automated run: not starting the OS drag of archive items");
        return;
    }
    match forge_win::dragout::active_window() {
        Ok(hwnd) => {
            if let Err(e) =
                forge_win::dragout::drag_out_virtual(hwnd, &data, true, first_is_dir, thumbnail)
            {
                tracing::warn!("archive drag-out failed: {e}");
            }
        }
        Err(e) => tracing::warn!("could not find the app window for drag-out: {e}"),
    }
}

/// What [`publish`] did.
pub enum Published {
    /// On the Windows clipboard; what a confirmed move of them removes.
    Yes(MoveOut),
    /// Not (a toast said why): they paste only in this app.
    No,
    /// The archive's password is being asked for; the copy runs again once it is typed.
    Asking,
}

/// Ctrl+C / Ctrl+X in an archive pane: put the items on the Windows clipboard as virtual
/// files.
pub fn publish(app: &mut ZxrawApp, paths: &[PathBuf], cut: bool) -> Published {
    let (data, moved) = match offer(app, paths, cut, true) {
        Ok(offer) => offer,
        Err(true) => return Published::Asking,
        Err(false) => return Published::No,
    };
    if let Err(e) = data.set_clipboard() {
        app.toast_err(format!(
            "Could not put the items on the Windows clipboard ({e}): they paste only here"
        ));
        return Published::No;
    }
    let id = moved.offer.unwrap_or_default();
    app.ops.archive.out.clipboard = Some((id, data));
    app.ops.archive.out.checked_clipboard = Some(Instant::now());
    Published::Yes(moved)
}

/// The password for archive items was typed: copy them again, or tell the user to drag
/// again (a drag can't be resumed).
pub fn password_ready(app: &mut ZxrawApp, then: Option<(Vec<PathBuf>, bool)>) {
    match then {
        Some((paths, cut)) => crate::archive_edit::copy_paths(app, &paths, cut),
        None => app.toast("Password saved: drag the items again"),
    }
}

/// The app is closing: virtual files can't be pasted once their streams are gone, so they
/// are taken off the clipboard rather than flushed.
pub fn shutdown(app: &mut ZxrawApp) {
    if let Some((_, data)) = app.ops.archive.out.clipboard.take() {
        data.clear_clipboard_if_current();
    }
}

/// Messages of stages and confirmed moves (every frame). True when something changed.
pub fn poll(app: &mut ZxrawApp) -> bool {
    let mut changed = false;
    // Something else was copied: drop the old offer, which removes its staged files.
    let out = &mut app.ops.archive.out;
    if out.clipboard.is_some()
        && out
            .checked_clipboard
            .is_none_or(|t| t.elapsed() > Duration::from_secs(2))
    {
        out.checked_clipboard = Some(Instant::now());
        if !out.on_clipboard() {
            out.clipboard = None;
        }
    }
    while let Ok(msg) = app.ops.archive.out.rx.try_recv() {
        changed = true;
        match msg {
            OutMsg::Moved(moved) => {
                // The archive's metadata is read off the UI thread.
                let tx = app.ops.archive.out.tx.clone();
                let ctx = app.ops.ctx.clone();
                std::thread::spawn(move || {
                    let unchanged =
                        crate::archive_ui::archive_version(&moved.archive) == moved.version;
                    let _ = tx.send(OutMsg::Checked { moved, unchanged });
                    ctx.request_repaint();
                });
            }
            OutMsg::Checked { moved, unchanged } => finish_move(app, moved, unchanged),
            OutMsg::Stage { archive, event } => stage_event(app, &archive, event),
        }
    }
    changed
}

fn finish_move(app: &mut ZxrawApp, moved: MoveOut, unchanged: bool) {
    let name = forge_core::paths::name_of(&moved.archive);
    // The moved items are no longer to be pasted from the clipboard.
    if let Some(id) = moved.offer
        && app.ops.archive.out.clipboard.as_ref().is_some_and(|(c, _)| *c == id)
    {
        if let Some((_, data)) = app.ops.archive.out.clipboard.take() {
            data.clear_clipboard_if_current();
        }
        app.ops.archive.copied = None;
    }
    if !unchanged {
        app.toast_err(format!(
            "The items were copied, but {name} changed meanwhile: nothing was removed from it"
        ));
        return;
    }
    let n = moved.names.len();
    app.ops.modal = Some(Modal::Confirm(Confirm {
        body: format!(
            "{} moved out of {name}. Remove {} from the archive now? This can't be undone.",
            if n == 1 { "1 item was" } else { "The items were" },
            if n == 1 { "it" } else { "them" }
        ),
        kind: ConfirmKind::ArchiveUpdate(UpdateRequest {
            archive: moved.archive.clone(),
            title: format!("Removing {} from {name}", items(n)),
            edit: archive::ArchiveEdit {
                add: Vec::new(),
                delete: moved.names,
            },
            // Kept entries are copied as stored: deleting needs no password.
            encrypted: false,
            confirm_password: false,
        }),
        title: "Finish the move".into(),
        confirm_label: "Remove".into(),
        danger: true,
    }));
}

fn stage_event(app: &mut ZxrawApp, archive: &Path, event: StageEvent) {
    let name = forge_core::paths::name_of(archive);
    match event {
        StageEvent::Password { wrong } => {
            app.toast_err(format!(
                "{name}: {} password, so nothing was given to the other app",
                if wrong { "wrong" } else { "no" }
            ));
            crate::archive_ui::ask_password(
                app,
                crate::archive_ui::Retry::Out {
                    archive: archive.to_path_buf(),
                    then: None,
                },
                wrong,
            );
        }
        StageEvent::Finished { failed, unchecked } => {
            if failed.is_empty() {
                return;
            }
            let mut lines = Vec::new();
            if !unchecked.is_empty() {
                lines.push(format!(
                    "{} encrypted file(s) were refused: {}. Extract them here to choose \
                     \"Extract them anyway\".",
                    unchecked.len(),
                    archive::UNCHECKED_REASON
                ));
            }
            for f in failed
                .iter()
                .filter(|f| f.reason != archive::UNCHECKED_REASON)
            {
                lines.push(format!("FAILED  {}: {}", f.path.display(), f.reason));
            }
            let _ = app.ops.tx.send(crate::actions::Event::Report {
                title: format!("Copying out of {name} — some files were not given out"),
                lines,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::ops::OpIssue;

    #[test]
    fn a_move_out_removes_only_after_a_clean_extraction() {
        let clean = ExtractReport::default();
        assert!(move_extracted_everything(&clean));
        for report in [
            ExtractReport {
                cancelled: true,
                ..Default::default()
            },
            ExtractReport {
                failed: vec![OpIssue::new("a", "crc")],
                ..Default::default()
            },
            ExtractReport {
                skipped: vec![OpIssue::new("a", "already exists")],
                ..Default::default()
            },
            ExtractReport {
                unchecked: vec!["a".into()],
                ..Default::default()
            },
        ] {
            assert!(!move_extracted_everything(&report), "{report:?}");
        }
    }
}
