//! The extract and test commands. On archive files in a folder: "Extract here (smart)",
//! "Extract to `<name>\`", "Extract to…" and "Test archive", one job per selected archive.
//! In a pane showing an archive's contents, the same commands extract the selected entries
//! (the whole archive when nothing is selected) next to the archive or into a chosen folder.
//!
//! Files that already exist are asked about once, up front, for the whole archive (Keep
//! both, Skip, Replace), as the engine cannot ask per file. Encrypted archives ask for the
//! password with `archive_ui`'s dialog and run again; the password is kept for the session.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use egui::{Key, RichText};
use forge_core::archive::{
    self, ArchiveEntry, ArchiveListing, ExtractLimits, ExtractOptions, ExtractReport,
    ExtractTarget, Password, TestReport, TopLevel,
};
use forge_core::model::format_size;
use forge_core::ops::{ConflictPolicy, JobCtx};
use forge_core::{CoreError, EntryKind};
use forge_jobs::JobHandle;

use crate::actions::Event;
use crate::app::ZxrawApp;
use crate::archive_ui::{ArchiveView, Msg, Retry};
use crate::commands::Cmd;

/// Job kind of extractions (shown in the progress popup, like copies).
pub const EXTRACT_KIND: &str = "extract";
/// Job kind of archive tests.
pub const TEST_KIND: &str = "archive-test";

/// Where an extraction puts the entries, relative to [`ExtractRequest::parent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// Into the folder when there is one item (the archive holds one folder or file, or one
    /// entry is selected), else into a new folder named after the archive.
    Smart,
    /// Into `<parent>\<archive name>\` (created, or added to when it exists).
    Named,
    /// Into the folder itself.
    Into,
}

#[derive(Debug, Clone)]
pub struct ExtractRequest {
    pub archive: PathBuf,
    /// The folder the entries go into or next to.
    pub parent: PathBuf,
    pub place: Place,
    /// The entries selected in an archive pane; `None` for the whole archive.
    pub selection: Option<archive::Selection>,
    /// How existing files are handled; `None` until asked (only when there are any).
    pub policy: Option<ConflictPolicy>,
}

#[derive(Debug, Clone)]
pub struct TestRequest {
    pub archive: PathBuf,
}

/// Files an extraction would write already exist: the job stopped to ask.
pub struct ConflictAsk {
    request: ExtractRequest,
    dest: PathBuf,
    count: usize,
    example: String,
}

/// The result of a "Test archive" job.
pub struct Tested {
    archive: PathBuf,
    result: Result<TestReport, String>,
}

/// Open questions and test results (in `app.ops.archive.extract`).
#[derive(Default)]
pub struct ExtractUi {
    questions: VecDeque<ConflictAsk>,
    tests: Vec<Tested>,
}

impl ExtractUi {
    pub fn question_open(&self) -> bool {
        !self.questions.is_empty()
    }
    pub(crate) fn ask(&mut self, ask: ConflictAsk) {
        self.questions.push_back(ask);
    }
    pub(crate) fn tested(&mut self, tested: Tested) {
        // A test run again replaces its old result.
        self.tests
            .retain(|t| !forge_core::paths::paths_equal(&t.archive, &tested.archive));
        self.tests.push(tested);
    }
}

/// The extract commands' own text for the menu: "Extract to `photos\`" names the folder.
pub fn named_folder_label(archive: &Path) -> String {
    format!("Extract to {}\\", archive::archive_stem(archive))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Archive files among the pane's targets.
fn archive_targets(app: &ZxrawApp) -> Vec<PathBuf> {
    let pane = app.pane();
    let not_file = |p: &PathBuf| {
        pane.entries
            .iter()
            .any(|e| &e.path == p && e.kind != EntryKind::File)
    };
    pane.action_targets()
        .into_iter()
        .filter(|p| archive::is_archive_name(p) && !not_file(p))
        .collect()
}

/// Run an extract or test command for the focused pane.
pub fn run(app: &mut ZxrawApp, cmd: Cmd) {
    if let Some(view) = app.pane().archive.clone() {
        return run_inside(app, cmd, view);
    }
    if app.pane().in_archive() {
        // Still listing the archive: nothing to act on yet.
        return;
    }
    let archives = archive_targets(app);
    if archives.is_empty() {
        app.toast("Select an archive (.zip) first");
        return;
    }
    if cmd == Cmd::TestArchive {
        for archive in archives {
            start_test(app, TestRequest { archive });
        }
        return;
    }
    let picked = match cmd {
        Cmd::ExtractTo => match pick_folder(&app.pane().path) {
            Some(p) => Some(p),
            None => return,
        },
        _ => None,
    };
    let place = place_of(cmd);
    for archive in archives {
        let parent = match &picked {
            Some(p) => p.clone(),
            None => parent_of(&archive),
        };
        start_extract(
            app,
            ExtractRequest {
                archive,
                parent,
                place,
                selection: None,
                policy: None,
            },
        );
    }
}

/// The commands in a pane showing an archive: on the selected entries, or all of it.
fn run_inside(app: &mut ZxrawApp, cmd: Cmd, view: ArchiveView) {
    if cmd == Cmd::TestArchive {
        return start_test(
            app,
            TestRequest {
                archive: view.archive,
            },
        );
    }
    let pane = app.pane();
    let targets = pane.action_targets();
    if targets.is_empty() && !pane.selected.is_empty() {
        app.toast("Nothing to extract: the selected items are hidden");
        return;
    }
    let selection = if pane.selected.is_empty() {
        None
    } else {
        let nodes = targets
            .iter()
            .filter_map(|p| view.node_of(p))
            .map(|n| (n.path.clone(), n.is_dir));
        Some(archive::Selection::new(
            view.inner.clone(),
            selection_names(nodes),
        ))
    };
    let home = parent_of(&view.archive);
    let parent = if cmd == Cmd::ExtractTo {
        match pick_folder(&home) {
            Some(p) => p,
            None => return,
        }
    } else {
        home
    };
    start_extract(
        app,
        ExtractRequest {
            archive: view.archive,
            parent,
            place: place_of(cmd),
            selection,
            policy: None,
        },
    );
}

fn place_of(cmd: Cmd) -> Place {
    match cmd {
        Cmd::ExtractHere => Place::Smart,
        Cmd::ExtractToFolder => Place::Named,
        _ => Place::Into,
    }
}

fn parent_of(archive: &Path) -> PathBuf {
    archive.parent().unwrap_or(Path::new(".")).to_path_buf()
}

fn pick_folder(start: &Path) -> Option<PathBuf> {
    crate::dialogs::pick_folder(
        rfd::FileDialog::new()
            .set_title("Extract to")
            .set_directory(start),
    )
}

/// Selection names of archive nodes (inner path, folder or not): folders end with `/`, so
/// they take everything inside them.
pub fn selection_names(nodes: impl IntoIterator<Item = (String, bool)>) -> Vec<String> {
    nodes
        .into_iter()
        .map(|(path, is_dir)| if is_dir { format!("{path}/") } else { path })
        .collect()
}

// ---------------------------------------------------------------------------
// Extraction job
// ---------------------------------------------------------------------------

/// Where the selected entries of an archive pane go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionDest {
    /// Into the chosen folder.
    Parent,
    /// Into a new folder named after the archive (the next free name).
    Fresh,
    /// Into the folder named after the archive, created when missing.
    Named,
}

fn selection_dest(place: Place, count: usize) -> SelectionDest {
    match place {
        Place::Into => SelectionDest::Parent,
        Place::Smart if count <= 1 => SelectionDest::Parent,
        Place::Smart => SelectionDest::Fresh,
        Place::Named => SelectionDest::Named,
    }
}

enum Outcome {
    Done(ExtractReport),
    Asked(ConflictAsk),
}

pub fn start_extract(app: &mut ZxrawApp, request: ExtractRequest) {
    let password = app.ops.archive.passwords.get(&request.archive).cloned();
    let name = forge_core::paths::name_of(&request.archive);
    let title = format!("Extracting {name}");
    let msgs = app.ops.archive.tx.clone();
    let events = app.ops.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops.jobs.spawn(EXTRACT_KIND, title, move |job| {
        let job_ctx = JobCtx::from_job(job);
        let result = match run_extract(&request, password, &job_ctx) {
            Ok(Outcome::Asked(ask)) => {
                let _ = msgs.send(Msg::Conflicts(ask));
                Ok(())
            }
            Ok(Outcome::Done(report)) => {
                finish_extract(job, &request, &report, &events);
                Ok(())
            }
            Err(e @ (CoreError::PasswordRequired(_) | CoreError::WrongPassword(_))) => {
                let wrong = matches!(e, CoreError::WrongPassword(_));
                let retry = Retry::Extract(request);
                let _ = msgs.send(Msg::NeedPassword { retry, wrong });
                Ok(())
            }
            Err(CoreError::Cancelled) => Ok(()),
            Err(e) => {
                let _ = events.send(Event::Error(format!("Extracting {name} failed: {e}")));
                Err(e.to_string())
            }
        };
        ctx.request_repaint();
        result
    });
}

fn run_extract(
    r: &ExtractRequest,
    password: Option<Password>,
    ctx: &JobCtx<'_>,
) -> forge_core::Result<Outcome> {
    if r.policy.is_none() {
        ctx.progress.set_note("Checking for existing files…");
        let listing = archive::list(&r.archive)?;
        ctx.progress.set_note("");
        if let Some(dest) = conflict_dest(r, &listing) {
            let existing = existing_files(&listing.entries, r.selection.as_ref(), &dest);
            if let Some(first) = existing.first() {
                return Ok(Outcome::Asked(ConflictAsk {
                    request: r.clone(),
                    dest,
                    count: existing.len(),
                    example: first.clone(),
                }));
            }
        }
    }
    let opts = ExtractOptions {
        // Nothing existed when checked: a file that turns up meanwhile is kept.
        policy: r.policy.unwrap_or(ConflictPolicy::Skip),
        password,
        ..ExtractOptions::default()
    };
    let (archive, parent) = (&r.archive, &r.parent);
    let report = match &r.selection {
        None => match r.place {
            Place::Smart => archive::extract_to(archive, parent, ExtractTarget::Smart, &opts, ctx),
            Place::Named => {
                archive::extract_to(archive, parent, ExtractTarget::NamedFolder, &opts, ctx)
            }
            Place::Into => archive::extract(archive, parent, &opts, None, ctx),
        },
        Some(sel) => match selection_dest(r.place, sel.names.len()) {
            SelectionDest::Parent => archive::extract(archive, parent, &opts, Some(sel), ctx),
            SelectionDest::Fresh => {
                let stem = archive::archive_stem(archive);
                let dest = forge_core::paths::unique_name_with(parent, &stem, false, |p| {
                    fs::symlink_metadata(p).is_ok()
                });
                fs::create_dir(&dest).map_err(|e| CoreError::io(&dest, e))?;
                let result = archive::extract(archive, &dest, &opts, Some(sel), ctx);
                let empty = match &result {
                    Err(_) => true,
                    Ok(r) => r.extracted.is_empty(),
                };
                if empty {
                    // Only if nothing is in it.
                    let _ = fs::remove_dir(&dest);
                }
                result
            }
            SelectionDest::Named => {
                let dest = parent.join(archive::archive_stem(archive));
                match fs::create_dir(&dest) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dest.is_dir() => {}
                    Err(e) => return Err(CoreError::io(&dest, e)),
                }
                archive::extract(archive, &dest, &opts, Some(sel), ctx)
            }
        },
    }?;
    Ok(Outcome::Done(report))
}

/// The folder an extraction writes its entries into, when files there could already
/// exist; `None` when it makes a new folder.
fn conflict_dest(r: &ExtractRequest, listing: &ArchiveListing) -> Option<PathBuf> {
    let named = || r.parent.join(archive::archive_stem(&r.archive));
    match &r.selection {
        None => match r.place {
            Place::Smart => {
                (!matches!(archive::top_level(listing), TopLevel::Many)).then(|| r.parent.clone())
            }
            Place::Named => Some(named()),
            Place::Into => Some(r.parent.clone()),
        },
        Some(sel) => match selection_dest(r.place, sel.names.len()) {
            SelectionDest::Parent => Some(r.parent.clone()),
            SelectionDest::Fresh => None,
            SelectionDest::Named => Some(named()),
        },
    }
}

/// Relative `/`-separated paths the files of `entries` extract to: all of them, or those
/// `selection` covers, relative to its base folder. Unsafe names are never extracted.
pub fn planned_files(
    entries: &[ArchiveEntry],
    selection: Option<&archive::Selection>,
) -> Vec<String> {
    let lower = |s: &str| s.to_lowercase();
    entries
        .iter()
        .filter(|e| !e.is_dir && e.safe_name)
        .filter_map(|e| {
            let Some(sel) = selection else {
                return Some(e.name.clone());
            };
            let name = lower(&e.name);
            let covered = sel.names.iter().any(|n| {
                let n = lower(n);
                n == name || (n.ends_with('/') && name.starts_with(&n))
            });
            if !covered {
                return None;
            }
            let base = lower(&sel.base);
            let rel = if !base.is_empty() && name.starts_with(&base) {
                e.name.get(sel.base.len()..).unwrap_or(&e.name)
            } else {
                &e.name
            };
            Some(rel.to_owned())
        })
        .collect()
}

/// The planned files that already exist in `dest`.
fn existing_files(
    entries: &[ArchiveEntry],
    selection: Option<&archive::Selection>,
    dest: &Path,
) -> Vec<String> {
    planned_files(entries, selection)
        .into_iter()
        .filter(|rel| {
            let path = rel.split('/').fold(dest.to_path_buf(), |p, c| p.join(c));
            fs::symlink_metadata(path).is_ok()
        })
        .collect()
}

/// "Extracted 12 file(s) (3.4 MB) to D:\x\photos, 2 skipped, 1 failed".
pub fn extract_summary(report: &ExtractReport) -> String {
    let mut s = format!(
        "Extracted {} file(s) ({}) to {}",
        report.extracted.len(),
        format_size(report.bytes),
        report.dest.display()
    );
    if !report.skipped.is_empty() {
        s.push_str(&format!(", {} skipped", report.skipped.len()));
    }
    if !report.failed.is_empty() {
        s.push_str(&format!(", {} failed", report.failed.len()));
    }
    if report.cancelled {
        s.push_str(", cancelled");
    }
    s
}

fn finish_extract(
    job: &JobHandle,
    request: &ExtractRequest,
    report: &ExtractReport,
    tx: &crossbeam_channel::Sender<Event>,
) {
    let mut lines = vec![extract_summary(report)];
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
    let _ = tx.send(Event::Changed(request.parent.clone()));
    if report.dest != request.parent {
        let _ = tx.send(Event::Changed(report.dest.clone()));
        // A new folder next to the archive: select it where the pane shows the folder.
        if report.dest.parent() == Some(request.parent.as_path())
            && let Some(name) = report.dest.file_name()
        {
            let _ = tx.send(Event::SelectNew {
                dir: request.parent.clone(),
                name: name.to_string_lossy().into_owned(),
            });
        }
    }
    let event = if !report.failed.is_empty() || !report.skipped.is_empty() {
        let heading = if report.failed.is_empty() {
            "some files skipped"
        } else {
            "completed with errors"
        };
        Event::Report {
            title: format!("{} — {heading}", job.title),
            lines,
        }
    } else if report.cancelled {
        Event::Toast(format!("{} — cancelled", job.title))
    } else {
        Event::Toast(lines.swap_remove(0))
    };
    let _ = tx.send(event);
}

// ---------------------------------------------------------------------------
// Test job
// ---------------------------------------------------------------------------

pub fn start_test(app: &mut ZxrawApp, request: TestRequest) {
    let password = app.ops.archive.passwords.get(&request.archive).cloned();
    let title = format!("Testing {}", forge_core::paths::name_of(&request.archive));
    let msgs = app.ops.archive.tx.clone();
    let ctx = app.ops.ctx.clone();
    app.ops.jobs.spawn(TEST_KIND, title, move |job| {
        let job_ctx = JobCtx::from_job(job);
        let limits = ExtractLimits::default();
        let r = archive::test_archive(&request.archive, password.as_ref(), &limits, &job_ctx);
        let archive = request.archive.clone();
        let result = match r {
            Ok(report) => {
                let failed = report.failures().count();
                for _ in 0..failed {
                    job.progress.add_error();
                }
                let _ = msgs.send(Msg::Tested(Tested {
                    archive,
                    result: Ok(report),
                }));
                Ok(())
            }
            Err(e @ (CoreError::PasswordRequired(_) | CoreError::WrongPassword(_))) => {
                let wrong = matches!(e, CoreError::WrongPassword(_));
                let retry = Retry::Test(request);
                let _ = msgs.send(Msg::NeedPassword { retry, wrong });
                Ok(())
            }
            Err(CoreError::Cancelled) => Ok(()),
            Err(e) => {
                let message = e.to_string();
                let _ = msgs.send(Msg::Tested(Tested {
                    archive,
                    result: Err(message.clone()),
                }));
                Err(message)
            }
        };
        ctx.request_repaint();
        result
    });
}

/// One line about a test: "OK: 120 files (3.4 MB) verified" or "2 of 120 files failed".
pub fn test_summary(report: &TestReport) -> String {
    let total = report.entries.len();
    let failed = report.failures().count();
    if report.cancelled {
        format!("Cancelled after {total} file(s)")
    } else if failed == 0 {
        format!(
            "OK: {total} file(s) ({}) verified",
            format_size(report.bytes)
        )
    } else {
        format!("{failed} of {total} file(s) failed")
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// The conflict question and the test results (call every frame).
pub fn windows(app: &mut ZxrawApp, ctx: &egui::Context) {
    question_modal(app, ctx);
    test_window(app, ctx);
}

/// "“a.txt” and 4 more files already exist in D:\x."
pub fn conflict_text(count: usize, example: &str, dest: &Path) -> String {
    let name = example.rsplit('/').next().unwrap_or(example);
    match count {
        0 | 1 => format!("“{name}” already exists in {}.", dest.display()),
        2 => format!(
            "“{name}” and 1 more file already exist in {}.",
            dest.display()
        ),
        n => format!(
            "“{name}” and {} more files already exist in {}.",
            n - 1,
            dest.display()
        ),
    }
}

fn question_modal(app: &mut ZxrawApp, ctx: &egui::Context) {
    let Some(q) = app.ops.archive.extract.questions.front() else {
        return;
    };
    let esc = ctx.input(|i| i.key_pressed(Key::Escape));
    let mut answer: Option<Option<ConflictPolicy>> = None;
    egui::Modal::new(egui::Id::new("archive-extract-conflicts")).show(ctx, |ui| {
        ui.set_width(480.0);
        ui.heading("Files already exist");
        ui.label(
            RichText::new(format!(
                "Extracting {}",
                forge_core::paths::name_of(&q.request.archive)
            ))
            .weak(),
        );
        ui.add_space(6.0);
        ui.label(conflict_text(q.count, &q.example, &q.dest));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .button("Keep both")
                .on_hover_text("Extracted files get a new name like “name (2)”")
                .clicked()
            {
                answer = Some(Some(ConflictPolicy::KeepBoth));
            }
            if ui
                .button("Skip")
                .on_hover_text("Existing files are left as they are")
                .clicked()
            {
                answer = Some(Some(ConflictPolicy::Skip));
            }
            let replace = egui::Button::new(RichText::new("Replace").color(egui::Color32::WHITE))
                .fill(egui::Color32::from_rgb(170, 60, 50));
            if ui
                .add(replace)
                .on_hover_text("Existing files are overwritten. This cannot be undone.")
                .clicked()
            {
                answer = Some(Some(ConflictPolicy::Overwrite));
            }
            if ui.button("Cancel").clicked() || esc {
                answer = Some(None);
            }
        });
    });
    if let Some(policy) = answer {
        let q = app
            .ops
            .archive
            .extract
            .questions
            .pop_front()
            .expect("front exists");
        if let Some(policy) = policy {
            let mut request = q.request;
            request.policy = Some(policy);
            start_extract(app, request);
        }
    }
}

fn test_window(app: &mut ZxrawApp, ctx: &egui::Context) {
    let tests = &mut app.ops.archive.extract.tests;
    if tests.is_empty() {
        return;
    }
    let mut open = true;
    let mut close = false;
    egui::Window::new("Archive test")
        .id(egui::Id::new("archive-test-results"))
        .open(&mut open)
        .default_size([460.0, 280.0])
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for t in tests.iter() {
                        ui.label(RichText::new(forge_core::paths::name_of(&t.archive)).strong());
                        match &t.result {
                            Ok(report) if report.is_ok() => {
                                ui.label(
                                    RichText::new(test_summary(report))
                                        .color(egui::Color32::from_rgb(80, 170, 90)),
                                );
                            }
                            Ok(report) => {
                                let color = if report.cancelled {
                                    ui.visuals().warn_fg_color
                                } else {
                                    ui.visuals().error_fg_color
                                };
                                ui.label(RichText::new(test_summary(report)).color(color));
                                for f in report.failures().take(500) {
                                    let why = f.error.as_deref().unwrap_or_default();
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(format!("{}: {why}", f.name)).small(),
                                        )
                                        .wrap(),
                                    );
                                }
                            }
                            Err(e) => {
                                ui.label(
                                    RichText::new(format!("Could not test: {e}"))
                                        .color(ui.visuals().error_fg_color),
                                );
                            }
                        }
                        ui.add_space(6.0);
                    }
                });
            if ui.button("Close").clicked() {
                close = true;
            }
        });
    if !open || close {
        tests.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::archive::EntryTest;
    use forge_core::ops::OpIssue;

    fn entry(name: &str, is_dir: bool) -> ArchiveEntry {
        ArchiveEntry {
            index: 0,
            name: name.into(),
            is_dir,
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

    #[test]
    fn the_named_label_shows_the_folder() {
        assert_eq!(
            named_folder_label(Path::new(r"D:\x\Trip 2024.zip")),
            "Extract to Trip 2024\\"
        );
    }

    #[test]
    fn selected_folders_take_everything_inside_them() {
        let names = selection_names([("docs/a".to_string(), true), ("docs/b.txt".into(), false)]);
        assert_eq!(names, ["docs/a/", "docs/b.txt"]);
        let sel = archive::Selection::new("docs", names);
        assert_eq!(sel.base, "docs/");
        let entries = [
            entry("docs/", true),
            entry("docs/a/", true),
            entry("docs/a/x.txt", false),
            entry("docs/a/y/z.txt", false),
            entry("docs/b.txt", false),
            entry("docs/c.txt", false),
            entry("other.txt", false),
        ];
        assert_eq!(
            planned_files(&entries, Some(&sel)),
            ["a/x.txt", "a/y/z.txt", "b.txt"]
        );
        // The whole archive: every file at its full name, unsafe ones left out.
        let mut all = entries.to_vec();
        all.push(ArchiveEntry {
            safe_name: false,
            ..entry("../evil.txt", false)
        });
        assert_eq!(planned_files(&all, None).len(), 5);
    }

    #[test]
    fn existing_files_are_found_under_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a").join("x.txt"), b"x").unwrap();
        let entries = [entry("a/x.txt", false), entry("a/new.txt", false)];
        assert_eq!(existing_files(&entries, None, dir.path()), ["a/x.txt"]);
    }

    #[test]
    fn smart_extraction_of_a_selection_nests_only_several_items() {
        assert_eq!(selection_dest(Place::Smart, 1), SelectionDest::Parent);
        assert_eq!(selection_dest(Place::Smart, 3), SelectionDest::Fresh);
        assert_eq!(selection_dest(Place::Into, 3), SelectionDest::Parent);
        assert_eq!(selection_dest(Place::Named, 1), SelectionDest::Named);
        assert_eq!(place_of(Cmd::ExtractHere), Place::Smart);
        assert_eq!(place_of(Cmd::ExtractToFolder), Place::Named);
        assert_eq!(place_of(Cmd::ExtractTo), Place::Into);
    }

    #[test]
    fn a_new_folder_is_never_asked_about() {
        let request = |place, selection| ExtractRequest {
            archive: PathBuf::from(r"D:\x\photos.zip"),
            parent: PathBuf::from(r"D:\x"),
            place,
            selection,
            policy: None,
        };
        let many = ArchiveListing {
            format: archive::ArchiveFormat::Zip,
            entries: vec![entry("a.txt", false), entry("b.txt", false)],
            comment: String::new(),
        };
        let one = ArchiveListing {
            entries: vec![entry("photos/", true), entry("photos/a.txt", false)],
            ..many.clone()
        };
        assert_eq!(conflict_dest(&request(Place::Smart, None), &many), None);
        assert_eq!(
            conflict_dest(&request(Place::Smart, None), &one),
            Some(PathBuf::from(r"D:\x"))
        );
        assert_eq!(
            conflict_dest(&request(Place::Named, None), &many),
            Some(PathBuf::from(r"D:\x\photos"))
        );
        let two = archive::Selection::new("", vec!["a.txt".into(), "b.txt".into()]);
        assert_eq!(
            conflict_dest(&request(Place::Smart, Some(two)), &many),
            None
        );
    }

    #[test]
    fn summaries_count_what_happened() {
        let mut report = ExtractReport {
            dest: PathBuf::from(r"D:\x\photos"),
            extracted: vec![PathBuf::from(r"D:\x\photos\a.jpg")],
            bytes: 2048,
            ..Default::default()
        };
        assert_eq!(
            extract_summary(&report),
            r"Extracted 1 file(s) (2.0 KB) to D:\x\photos".replace("2.0 KB", &format_size(2048))
        );
        report
            .skipped
            .push(OpIssue::new(r"D:\x\photos\b.jpg", "already exists"));
        report.cancelled = true;
        assert!(extract_summary(&report).ends_with(", 1 skipped, cancelled"));

        let mut test = TestReport {
            entries: vec![EntryTest {
                name: "a.txt".into(),
                size: 1,
                error: None,
                unverified: false,
            }],
            bytes: 1,
            cancelled: false,
        };
        assert!(test_summary(&test).starts_with("OK: 1 file(s)"));
        test.entries.push(EntryTest {
            name: "b.txt".into(),
            size: 1,
            error: Some("CRC mismatch".into()),
            unverified: false,
        });
        assert_eq!(test_summary(&test), "1 of 2 file(s) failed");
    }

    #[test]
    fn the_conflict_question_names_one_file_and_counts_the_rest() {
        let dest = Path::new(r"D:\x");
        assert_eq!(
            conflict_text(1, "sub/a.txt", dest),
            r"“a.txt” already exists in D:\x."
        );
        assert_eq!(
            conflict_text(5, "a.txt", dest),
            r"“a.txt” and 4 more files already exist in D:\x."
        );
    }
}
