//! Power-user tool windows: folder size, duplicates, checksums, folder compare and the
//! backup check (compare a backup with its original by content).
//!
//! The engine work stays in `forge-core`; every long operation is registered with the
//! Operations panel and reports back to its own window over a channel.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender};
use egui::{Align2, Color32, FontId, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};
use forge_core::backup_verify::{
    BackupReport, VerifyMode, VerifyOptions, VerifyStatus, verify_backup,
};
use forge_core::compare::{self, CompareReport, EntryState, SyncAction, SyncMode, SyncPlan};
use forge_core::disk_usage::{self, UsageReport};
use forge_core::duplicates::{self, DupOptions, DupReport, DuplicateGroup};
use forge_core::error::CoreError;
use forge_core::hashing::{self, ManifestEntry, OnlineOnlyPolicy, OnlineOnlyTotals, VerifyReport};
use forge_core::model::format_size;
use forge_core::ops::{JobCtx, OpReport};
use forge_jobs::{CancelToken, JobHandle, JobRegistry};

use crate::actions::Event;
use crate::app::ZxrawApp;
use crate::theme;

type ManifestJobResult = Result<hashing::ManifestBuild, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    FolderSize,
    Duplicates,
    Checksums,
    CompareFolders,
    VerifyBackup,
}

#[derive(Default)]
pub struct ToolsState {
    windows: Vec<ToolWindow>,
    next_id: u64,
}

impl ToolsState {
    /// Open `tool`. `dir` = focused pane folder, `selection` = selected paths (or empty),
    /// `other_dir` = the other pane's folder in dual-pane mode.
    pub fn open(
        &mut self,
        tool: Tool,
        dir: PathBuf,
        selection: Vec<PathBuf>,
        other_dir: Option<PathBuf>,
    ) {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let (tx, rx) = crossbeam_channel::unbounded();
        let state = match tool {
            Tool::FolderSize => ToolState::Folder(FolderState {
                root: dir,
                report: None,
                focused: 0,
                sort: ChildSort::Largest,
                selected_child: None,
                pending_focus: None,
            }),
            Tool::Duplicates => ToolState::Duplicates(DuplicatesState {
                folder: dir,
                selection,
                min_size: 1,
                byte_compare: false,
                include_empty: false,
                report: None,
                modified: HashMap::new(),
                selected: BTreeSet::new(),
                confirming: false,
            }),
            Tool::Checksums => ToolState::Checksums(ChecksumsState {
                root: dir,
                selection,
                entries: None,
                errors: Vec::new(),
                skipped: Vec::new(),
                prompt: None,
                verify: None,
            }),
            Tool::CompareFolders => ToolState::Compare(CompareState {
                left: dir,
                right: other_dir,
                use_hash: false,
                report: None,
                filter: CompareFilter::All,
                sync_mode: SyncMode::Update,
                plan: None,
                last_sync: None,
                confirming_mirror: false,
            }),
            Tool::VerifyBackup => ToolState::Verify(VerifyState {
                original: dir,
                backup: other_dir,
                full: false,
                detect_moves: false,
                report: None,
                filter: VerifyFilter::Problems,
                last_export: None,
                export_error: None,
            }),
        };
        // Compare and the backup check need their second folder before they can start.
        let start_initial = match &state {
            ToolState::Compare(c) => c.right.is_some(),
            ToolState::Verify(v) => v.backup.is_some(),
            _ => true,
        };
        self.windows.push(ToolWindow {
            id: self.next_id,
            open: true,
            generation: 0,
            initial_pending: start_initial,
            follow_up: None,
            job: None,
            tx,
            rx,
            notice: None,
            state,
        });
    }
}

struct ToolWindow {
    id: u64,
    open: bool,
    generation: u64,
    initial_pending: bool,
    /// Work started by a finished job without user input (no online-only files found).
    follow_up: Option<DeferredAction>,
    job: Option<JobHandle>,
    tx: Sender<ToolEnvelope>,
    rx: Receiver<ToolEnvelope>,
    notice: Option<String>,
    state: ToolState,
}

enum ToolState {
    Folder(FolderState),
    Duplicates(DuplicatesState),
    Checksums(ChecksumsState),
    Compare(CompareState),
    Verify(VerifyState),
}

struct FolderState {
    root: PathBuf,
    report: Option<UsageReport>,
    focused: usize,
    sort: ChildSort,
    selected_child: Option<usize>,
    pending_focus: Option<(usize, Instant)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildSort {
    Largest,
    Smallest,
    Name,
}

struct DuplicatesState {
    folder: PathBuf,
    selection: Vec<PathBuf>,
    min_size: u64,
    byte_compare: bool,
    include_empty: bool,
    report: Option<DupReport>,
    modified: HashMap<PathBuf, SystemTime>,
    /// `(group index, copy index)` pairs. Empty by default, so nothing is queued for removal.
    selected: BTreeSet<(usize, usize)>,
    confirming: bool,
}

struct ChecksumsState {
    root: PathBuf,
    selection: Vec<PathBuf>,
    entries: Option<Vec<ManifestEntry>>,
    errors: Vec<(PathBuf, String)>,
    skipped: Vec<String>,
    /// Waiting for the user to choose what to do with online-only files.
    prompt: Option<OnlineOnlyPrompt>,
    verify: Option<VerifyReport>,
}

/// A checksum job waiting for its online-only count and the user's choice.
#[derive(Debug)]
enum ChecksumWork {
    Build {
        root: PathBuf,
        selection: Vec<PathBuf>,
    },
    /// `manifest_allowed`: the user agreed to download an online-only manifest.
    Verify {
        manifest: PathBuf,
        manifest_allowed: bool,
    },
}

struct OnlineOnlyPrompt {
    work: ChecksumWork,
    totals: OnlineOnlyTotals,
}

struct CompareState {
    left: PathBuf,
    right: Option<PathBuf>,
    use_hash: bool,
    report: Option<CompareReport>,
    filter: CompareFilter,
    sync_mode: SyncMode,
    plan: Option<SyncPlan>,
    last_sync: Option<OpReport>,
    confirming_mirror: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompareFilter {
    All,
    LeftOnly,
    RightOnly,
    Different,
    Same,
    Errors,
}

struct VerifyState {
    original: PathBuf,
    backup: Option<PathBuf>,
    /// "Full verify": hash every pair with equal sizes, whatever the timestamps say.
    full: bool,
    detect_moves: bool,
    report: Option<BackupReport>,
    filter: VerifyFilter,
    last_export: Option<PathBuf>,
    export_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifyFilter {
    /// Everything except matches. The default.
    Problems,
    All,
    Missing,
    Differs,
    Extra,
    Moved,
    NotChecked,
    Errors,
    Matches,
}

const VERIFY_FILTERS: [(&str, VerifyFilter); 9] = [
    ("Problems", VerifyFilter::Problems),
    ("All", VerifyFilter::All),
    ("Missing", VerifyFilter::Missing),
    ("Differs", VerifyFilter::Differs),
    ("Only in backup", VerifyFilter::Extra),
    ("Moved", VerifyFilter::Moved),
    ("Not checked", VerifyFilter::NotChecked),
    ("Errors", VerifyFilter::Errors),
    ("Matches", VerifyFilter::Matches),
];

struct ToolEnvelope {
    generation: u64,
    payload: ToolPayload,
}

enum ToolPayload {
    Folder(Result<UsageReport, String>),
    Duplicates(Result<(DupReport, HashMap<PathBuf, SystemTime>), String>),
    OnlineOnlyCount(ChecksumWork, Result<OnlineOnlyTotals, String>),
    Manifest(ManifestJobResult),
    Verify(Result<VerifyReport, String>),
    Saved(Result<PathBuf, String>, usize),
    Compare(Result<CompareReport, String>),
    SyncPlan(Result<SyncPlan, String>),
    Recycled(Result<(), String>),
    Synced(OpReport),
    BackupVerified(Result<BackupReport, String>),
    /// Outcome of a background report export: the saved path, or a message for the window.
    Exported(Result<PathBuf, String>),
}

enum UiAction {
    Navigate(PathBuf),
    Start(DeferredAction),
    /// Open the backup check for the folders of a folder compare.
    OpenVerify {
        original: PathBuf,
        backup: PathBuf,
    },    /// Show a message in the window and start nothing.
    Notice(String),
}

enum DeferredAction {
    FolderScan(PathBuf),
    DuplicatesScan {
        folder: PathBuf,
        selection: Vec<PathBuf>,
        options: DupOptions,
    },
    /// Phase 1 of a checksum job: count the online-only files it would read.
    ChecksumCount(ChecksumWork),
    /// Phase 2: run the checksum job with the chosen policy.
    Checksum {
        work: ChecksumWork,
        policy: OnlineOnlyPolicy,
    },
    SaveManifest {
        path: PathBuf,
        entries: Vec<ManifestEntry>,
        /// Online-only files skipped when hashing, so not in the manifest.
        skipped: usize,
    },
    CompareScan {
        left: PathBuf,
        right: PathBuf,
        use_hash: bool,
    },
    VerifyBackup {
        original: PathBuf,
        backup: PathBuf,
        options: VerifyOptions,
    },
    BuildSyncPlan {
        report: CompareReport,
        left: PathBuf,
        right: PathBuf,
        mode: SyncMode,
    },
    RecycleDuplicates {
        targets: Vec<PathBuf>,
        bytes: u64,
        /// Per-group facts from the scan, re-verified on disk right before recycling.
        checks: Vec<RecycleCheck>,
    },
    ExecuteSync(SyncPlan),
    /// Write an already-rendered backup check report to a path the user chose.
    ExportVerify {
        path: PathBuf,
        text: String,
    },
}

impl ToolWindow {
    fn title(&self) -> &'static str {
        match &self.state {
            ToolState::Folder(_) => "Folder size",
            ToolState::Duplicates(_) => "Find duplicates",
            ToolState::Checksums(_) => "Checksums",
            ToolState::Compare(_) => "Compare folders",
            ToolState::Verify(_) => "Verify backup",
        }
    }

    fn poll_messages(&mut self) {
        while let Ok(envelope) = self.rx.try_recv() {
            if envelope.generation != self.generation {
                continue;
            }
            self.job = None;
            match envelope.payload {
                ToolPayload::Folder(result) => match result {
                    Ok(report) => {
                        if let ToolState::Folder(state) = &mut self.state {
                            state.focused = 0;
                            state.report = Some(report);
                        }
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(error),
                },
                ToolPayload::Duplicates(result) => match result {
                    Ok((report, modified)) => {
                        if let ToolState::Duplicates(state) = &mut self.state {
                            state.report = Some(report);
                            state.modified = modified;
                            state.selected.clear();
                            state.confirming = false;
                        }
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(error),
                },
                ToolPayload::OnlineOnlyCount(work, result) => {
                    if let ToolState::Checksums(state) = &mut self.state {
                        state.prompt = None;
                    }
                    match result {
                        // Skip, not Download: a file that goes online-only after the count
                        // is reported as skipped instead of being downloaded unasked.
                        Ok(totals) if totals.count == 0 => {
                            self.follow_up = Some(DeferredAction::Checksum {
                                work,
                                policy: OnlineOnlyPolicy::Skip,
                            });
                            self.notice = None;
                        }
                        Ok(totals) => {
                            if let ToolState::Checksums(state) = &mut self.state {
                                state.prompt = Some(OnlineOnlyPrompt { work, totals });
                            }
                            self.notice = None;
                        }
                        Err(error) => self.notice = Some(error),
                    }
                }
                ToolPayload::Manifest(result) => match result {
                    Ok(build) => {
                        if let ToolState::Checksums(state) = &mut self.state {
                            state.entries = Some(build.entries);
                            state.errors = build.errors;
                            state.skipped = build.skipped_online_only;
                            state.verify = None;
                        }
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(error),
                },
                ToolPayload::Verify(result) => match result {
                    Ok(report) => {
                        self.notice = Some(verify_summary(&report));
                        if let ToolState::Checksums(state) = &mut self.state {
                            state.verify = Some(report);
                        }
                    }
                    Err(error) => self.notice = Some(error),
                },
                ToolPayload::Saved(result, skipped) => {
                    self.notice = Some(match result {
                        Ok(path) => saved_notice(&path, skipped),
                        Err(error) => error,
                    });
                }
                ToolPayload::Compare(result) => match result {
                    Ok(report) => {
                        if let ToolState::Compare(state) = &mut self.state {
                            state.report = Some(report);
                            state.plan = None;
                            state.confirming_mirror = false;
                        }
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(error),
                },
                ToolPayload::SyncPlan(result) => match result {
                    Ok(plan) => {
                        if let ToolState::Compare(state) = &mut self.state {
                            state.plan = Some(plan);
                            state.confirming_mirror = false;
                        }
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(error),
                },
                ToolPayload::Recycled(result) => match result {
                    Ok(()) => {
                        if let ToolState::Duplicates(state) = &mut self.state {
                            state.report = None;
                            state.selected.clear();
                        }
                        self.notice =
                            Some("Copies moved to the Recycle Bin; refreshing results…".into());
                        self.initial_pending = true;
                    }
                    Err(error) => self.notice = Some(error),
                },
                ToolPayload::BackupVerified(result) => match result {
                    Ok(report) => {
                        if let ToolState::Verify(state) = &mut self.state {
                            state.report = Some(report);
                        }
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(error),
                },
                ToolPayload::Exported(result) => {
                    if let ToolState::Verify(state) = &mut self.state {
                        apply_export_result(state, result);
                    }
                }
                ToolPayload::Synced(report) => {
                    self.notice = Some(sync_summary(&report));
                    if let ToolState::Compare(state) = &mut self.state {
                        state.last_sync = Some(report);
                        state.plan = None;
                        state.report = None;
                        state.confirming_mirror = false;
                    }
                    self.initial_pending =
                        matches!(&self.state, ToolState::Compare(c) if c.right.is_some());
                }
            }
        }
    }

    fn initial_action(&mut self) -> Option<DeferredAction> {
        if !self.initial_pending {
            return None;
        }
        self.initial_pending = false;
        match &self.state {
            ToolState::Folder(state) => Some(DeferredAction::FolderScan(state.root.clone())),
            ToolState::Duplicates(state) => Some(DeferredAction::DuplicatesScan {
                folder: state.folder.clone(),
                selection: state.selection.clone(),
                options: DupOptions {
                    min_size: state.min_size,
                    include_empty: state.include_empty,
                    byte_compare: state.byte_compare,
                },
            }),
            ToolState::Checksums(state) => Some(DeferredAction::ChecksumCount(ChecksumWork::Build {
                root: state.root.clone(),
                selection: state.selection.clone(),
            })),
            ToolState::Compare(state) => {
                state
                    .right
                    .as_ref()
                    .map(|right| DeferredAction::CompareScan {
                        left: state.left.clone(),
                        right: right.clone(),
                        use_hash: state.use_hash,
                    })
            }
            ToolState::Verify(state) => {
                state
                    .backup
                    .as_ref()
                    .map(|backup| DeferredAction::VerifyBackup {
                        original: state.original.clone(),
                        backup: backup.clone(),
                        options: verify_options(state),
                    })
            }
        }
    }
}

/// Settings for a backup check from the window's controls.
fn verify_options(state: &VerifyState) -> VerifyOptions {
    VerifyOptions {
        mode: if state.full {
            VerifyMode::Full
        } else {
            VerifyMode::Quick
        },
        detect_moves: state.detect_moves,
        ..Default::default()
    }
}

pub fn windows(app: &mut ZxrawApp, ctx: &egui::Context) {
    let mut index = 0;
    while index < app.ops.tools.windows.len() {
        let follow_up = {
            let window = &mut app.ops.tools.windows[index];
            window.poll_messages();
            window.follow_up.take().filter(|_| window.open)
        };
        // Started before this frame's UI, so a click in the same frame supersedes it like any
        // running job instead of dropping it.
        if let Some(action) = follow_up {
            let jobs = &app.ops.jobs;
            let event_tx = &app.ops.tx;
            let app_ctx = &app.ops.ctx;
            let window = &mut app.ops.tools.windows[index];
            launch_action(jobs, event_tx, app_ctx, window, action);
        }
        let ui_action = render_window(&mut app.ops.tools.windows[index], ctx);

        let mut navigate = None;
        let mut deferred = None;
        let mut verify = None;
        match ui_action {
            Some(UiAction::Navigate(path)) => navigate = Some(path),
            Some(UiAction::Start(action)) => deferred = Some(action),
            Some(UiAction::OpenVerify { original, backup }) => verify = Some((original, backup)),
            Some(UiAction::Notice(notice)) => app.ops.tools.windows[index].notice = Some(notice),
            None => {}
        }
        if deferred.is_none() {
            deferred = app.ops.tools.windows[index].initial_action();
        } else {
            app.ops.tools.windows[index].initial_pending = false;
        }
        if let Some(path) = navigate {
            app.navigate(path);
        }
        if let Some((original, backup)) = verify {
            app.ops
                .tools
                .open(Tool::VerifyBackup, original, Vec::new(), Some(backup));
        }
        if let Some(action) = deferred {
            let jobs = &app.ops.jobs;
            let event_tx = &app.ops.tx;
            let app_ctx = &app.ops.ctx;
            let window = &mut app.ops.tools.windows[index];
            launch_action(jobs, event_tx, app_ctx, window, action);
        }

        if !app.ops.tools.windows[index].open {
            if let Some(job) = &app.ops.tools.windows[index].job {
                job.cancel.cancel();
            }
            app.ops.tools.windows.remove(index);
        } else {
            index += 1;
        }
    }
}

pub(crate) fn automation_open_checksums(app: &mut ZxrawApp, folder: PathBuf) {
    app.ops
        .tools
        .open(Tool::Checksums, folder, Vec::new(), None);
}

/// What the newest checksum window shows: the online-only prompt, a finished manifest, or
/// neither yet (`None`, or an error notice once its job ended).
pub(crate) enum ChecksumsView {
    Prompt(OnlineOnlyTotals),
    Manifest { entries: usize, skipped: usize },
}

pub(crate) fn automation_checksums(
    app: &mut ZxrawApp,
) -> Option<std::result::Result<ChecksumsView, String>> {
    let window = app
        .ops
        .tools
        .windows
        .iter_mut()
        .rev()
        .find(|window| matches!(window.state, ToolState::Checksums(_)))?;
    window.poll_messages();
    let job_done = window
        .job
        .as_ref()
        .is_none_or(|job| job.state().is_finished());
    let ToolState::Checksums(state) = &window.state else {
        unreachable!();
    };
    if let Some(prompt) = &state.prompt {
        Some(Ok(ChecksumsView::Prompt(prompt.totals)))
    } else if !job_done || window.follow_up.is_some() {
        None
    } else if let Some(entries) = &state.entries {
        Some(Ok(ChecksumsView::Manifest {
            entries: entries.len(),
            skipped: state.skipped.len(),
        }))
    } else {
        window.notice.clone().map(Err)
    }
}

pub(crate) fn automation_open_duplicates(app: &mut ZxrawApp, folder: PathBuf) {
    app.ops
        .tools
        .open(Tool::Duplicates, folder, Vec::new(), None);
}

pub(crate) fn automation_duplicate_groups(
    app: &mut ZxrawApp,
) -> Option<std::result::Result<Vec<Vec<PathBuf>>, String>> {
    let window = app
        .ops
        .tools
        .windows
        .iter_mut()
        .rev()
        .find(|window| matches!(window.state, ToolState::Duplicates(_)))?;
    window.poll_messages();
    let ToolState::Duplicates(state) = &window.state else {
        unreachable!();
    };
    if let Some(report) = &state.report {
        Some(Ok(report
            .groups
            .iter()
            .map(|group| group.files.clone())
            .collect()))
    } else if window
        .job
        .as_ref()
        .is_none_or(|job| job.state().is_finished())
        && let Some(notice) = &window.notice
    {
        Some(Err(notice.clone()))
    } else {
        None
    }
}

pub(crate) fn automation_duplicates_keep_first(app: &mut ZxrawApp) -> Result<usize, String> {
    let Some(window) = app
        .ops
        .tools
        .windows
        .iter_mut()
        .rev()
        .find(|window| matches!(window.state, ToolState::Duplicates(_)))
    else {
        return Err("the duplicate finder is not open".into());
    };
    window.poll_messages();
    let ToolState::Duplicates(state) = &mut window.state else {
        unreachable!();
    };
    let report = state
        .report
        .as_ref()
        .ok_or("duplicate scan has not completed")?;
    state.selected.clear();
    for (group_index, group) in report.groups.iter().enumerate() {
        for file_index in 1..group.files.len() {
            state.selected.insert((group_index, file_index));
        }
    }
    if !selection_is_safe(&report.groups, &state.selected) {
        return Err("the selected copies would remove every file in a group".into());
    }
    Ok(state.selected.len())
}

pub(crate) fn automation_duplicates_recycle(app: &mut ZxrawApp) -> Result<(), String> {
    let jobs = app.ops.jobs.clone();
    let event_tx = app.ops.tx.clone();
    let app_ctx = app.ops.ctx.clone();
    let Some(window) = app
        .ops
        .tools
        .windows
        .iter_mut()
        .rev()
        .find(|window| matches!(window.state, ToolState::Duplicates(_)))
    else {
        return Err("the duplicate finder is not open".into());
    };
    window.poll_messages();
    let ToolState::Duplicates(state) = &mut window.state else {
        unreachable!();
    };
    let report = state
        .report
        .as_ref()
        .ok_or("duplicate scan has not completed")?;
    let (targets, bytes) = selected_duplicate_targets(&report.groups, &state.selected)
        .ok_or("unsafe duplicate selection")?;
    if targets.is_empty() {
        return Err("no duplicate copies are selected".into());
    }
    let checks = recycle_checks(&report.groups, &state.selected);
    launch_action(
        &jobs,
        &event_tx,
        &app_ctx,
        window,
        DeferredAction::RecycleDuplicates {
            targets,
            bytes,
            checks,
        },
    );
    Ok(())
}

/// `Cmd::SyncFolders`: opens folder compare with the active pane on the left and the other
/// pane (when dual-pane) on the right.
pub fn open_sync(app: &mut ZxrawApp) {
    let dir = app.pane().path.clone();
    let other = app.tab().other().map(|p| p.path.clone());
    app.ops
        .tools
        .open(Tool::CompareFolders, dir, Vec::new(), other);
}

/// `Cmd::VerifyBackup`: the focused pane is the original, the other pane (when dual-pane)
/// is the backup.
pub fn open_verify_backup(app: &mut ZxrawApp) {
    let dir = app.pane().path.clone();
    let other = app.tab().other().map(|p| p.path.clone());
    app.ops
        .tools
        .open(Tool::VerifyBackup, dir, Vec::new(), other);
}

pub(crate) fn automation_open_compare(app: &mut ZxrawApp, left: PathBuf) {
    app.ops
        .tools
        .open(Tool::CompareFolders, left, Vec::new(), None);
}

pub(crate) fn automation_compare_set_right(
    app: &mut ZxrawApp,
    right: PathBuf,
) -> Result<(), String> {
    let Some(window) = app
        .ops
        .tools
        .windows
        .iter_mut()
        .rev()
        .find(|window| matches!(window.state, ToolState::Compare(_)))
    else {
        return Err("folder compare is not open".into());
    };
    let ToolState::Compare(state) = &mut window.state else {
        unreachable!();
    };
    state.right = Some(right);
    state.report = None;
    state.plan = None;
    state.last_sync = None;
    state.confirming_mirror = false;
    Ok(())
}

pub(crate) fn automation_compare_start(app: &mut ZxrawApp) -> Result<(), String> {
    let jobs = app.ops.jobs.clone();
    let event_tx = app.ops.tx.clone();
    let app_ctx = app.ops.ctx.clone();
    let Some(window) = app
        .ops
        .tools
        .windows
        .iter_mut()
        .rev()
        .find(|window| matches!(window.state, ToolState::Compare(_)))
    else {
        return Err("folder compare is not open".into());
    };
    let ToolState::Compare(state) = &mut window.state else {
        unreachable!();
    };
    let right = state
        .right
        .clone()
        .ok_or("choose the right folder before comparing")?;
    let left = state.left.clone();
    let use_hash = state.use_hash;
    state.report = None;
    state.plan = None;
    state.last_sync = None;
    launch_action(
        &jobs,
        &event_tx,
        &app_ctx,
        window,
        DeferredAction::CompareScan {
            left,
            right,
            use_hash,
        },
    );
    Ok(())
}

pub(crate) fn automation_compare_report(
    app: &mut ZxrawApp,
) -> Option<std::result::Result<CompareReport, String>> {
    let window = app
        .ops
        .tools
        .windows
        .iter_mut()
        .rev()
        .find(|window| matches!(window.state, ToolState::Compare(_)))?;
    window.poll_messages();
    let ToolState::Compare(state) = &window.state else {
        unreachable!();
    };
    if let Some(report) = &state.report {
        Some(Ok(report.clone()))
    } else if window
        .job
        .as_ref()
        .is_none_or(|job| job.state().is_finished())
        && let Some(notice) = &window.notice
    {
        Some(Err(notice.clone()))
    } else {
        None
    }
}

pub(crate) fn automation_compare_build_update(app: &mut ZxrawApp) -> Result<SyncPlan, String> {
    automation_compare_build_plan(app, SyncMode::Update)
}

pub(crate) fn automation_compare_build_two_way(app: &mut ZxrawApp) -> Result<SyncPlan, String> {
    automation_compare_build_plan(app, SyncMode::TwoWay)
}

fn automation_compare_build_plan(app: &mut ZxrawApp, mode: SyncMode) -> Result<SyncPlan, String> {
    let Some(window) = app
        .ops
        .tools
        .windows
        .iter_mut()
        .rev()
        .find(|window| matches!(window.state, ToolState::Compare(_)))
    else {
        return Err("folder compare is not open".into());
    };
    let ToolState::Compare(state) = &mut window.state else {
        unreachable!();
    };
    let right = state
        .right
        .as_ref()
        .ok_or("choose the right folder before building a plan")?;
    let report = state
        .report
        .as_ref()
        .ok_or("comparison has not completed")?;
    state.sync_mode = mode;
    let plan = compare::plan_sync(report, &state.left, right, mode);
    state.plan = Some(plan.clone());
    Ok(plan)
}

pub(crate) fn automation_compare_execute_update(app: &mut ZxrawApp) -> Result<(), String> {
    automation_compare_execute_plan(app, SyncMode::Update)
}

pub(crate) fn automation_compare_execute_two_way(app: &mut ZxrawApp) -> Result<(), String> {
    automation_compare_execute_plan(app, SyncMode::TwoWay)
}

fn automation_compare_execute_plan(app: &mut ZxrawApp, mode: SyncMode) -> Result<(), String> {
    let jobs = app.ops.jobs.clone();
    let event_tx = app.ops.tx.clone();
    let app_ctx = app.ops.ctx.clone();
    let Some(window) = app
        .ops
        .tools
        .windows
        .iter_mut()
        .rev()
        .find(|window| matches!(window.state, ToolState::Compare(_)))
    else {
        return Err("folder compare is not open".into());
    };
    let ToolState::Compare(state) = &window.state else {
        unreachable!();
    };
    if state.sync_mode != mode {
        return Err(format!("sync mode is not {mode:?}"));
    }
    let plan = state.plan.clone().ok_or("build a sync preview first")?;
    launch_action(
        &jobs,
        &event_tx,
        &app_ctx,
        window,
        DeferredAction::ExecuteSync(plan),
    );
    Ok(())
}

/// One-line result of a folder sync, warnings counted after the items.
fn sync_summary(report: &OpReport) -> String {
    let warnings = crate::ops_ui::warnings_suffix(report.warnings.len());
    if report.failed.is_empty() {
        format!(
            "Sync complete: {} item(s), {}{warnings}",
            report.done.len(),
            format_size(report.bytes)
        )
    } else {
        format!("Sync finished with {} error(s){warnings}", report.failed.len())
    }
}

pub(crate) fn automation_compare_sync_result(app: &mut ZxrawApp) -> Option<OpReport> {
    let window = app
        .ops
        .tools
        .windows
        .iter_mut()
        .rev()
        .find(|window| matches!(window.state, ToolState::Compare(_)))?;
    window.poll_messages();
    let ToolState::Compare(state) = &window.state else {
        unreachable!();
    };
    state.last_sync.clone()
}

#[allow(dead_code)] // used by the automation functions below
fn find_verify_window(app: &mut ZxrawApp) -> Option<&mut ToolWindow> {
    app.ops
        .tools
        .windows
        .iter_mut()
        .rev()
        .find(|window| matches!(window.state, ToolState::Verify(_)))
}

pub(crate) fn automation_open_verify(app: &mut ZxrawApp, original: PathBuf) {
    app.ops
        .tools
        .open(Tool::VerifyBackup, original, Vec::new(), None);
}

pub(crate) fn automation_verify_set_backup(
    app: &mut ZxrawApp,
    backup: PathBuf,
) -> Result<(), String> {
    let Some(window) = find_verify_window(app) else {
        return Err("verify backup is not open".into());
    };
    let ToolState::Verify(state) = &mut window.state else {
        unreachable!();
    };
    state.backup = Some(backup);
    state.report = None;
    Ok(())
}

pub(crate) fn automation_verify_set_options(
    app: &mut ZxrawApp,
    full: bool,
    detect_moves: bool,
) -> Result<(), String> {
    let Some(window) = find_verify_window(app) else {
        return Err("verify backup is not open".into());
    };
    let ToolState::Verify(state) = &mut window.state else {
        unreachable!();
    };
    state.full = full;
    state.detect_moves = detect_moves;
    Ok(())
}

pub(crate) fn automation_verify_start(app: &mut ZxrawApp) -> Result<(), String> {
    let jobs = app.ops.jobs.clone();
    let event_tx = app.ops.tx.clone();
    let app_ctx = app.ops.ctx.clone();
    let Some(window) = find_verify_window(app) else {
        return Err("verify backup is not open".into());
    };
    let ToolState::Verify(state) = &mut window.state else {
        unreachable!();
    };
    let backup = state
        .backup
        .clone()
        .ok_or("choose the backup folder before verifying")?;
    let action = DeferredAction::VerifyBackup {
        original: state.original.clone(),
        backup,
        options: verify_options(state),
    };
    state.report = None;
    launch_action(&jobs, &event_tx, &app_ctx, window, action);
    Ok(())
}

/// Cancels the running backup check. The job then reports "Backup check cancelled".
pub(crate) fn automation_verify_cancel(app: &mut ZxrawApp) -> Result<(), String> {
    let Some(window) = find_verify_window(app) else {
        return Err("verify backup is not open".into());
    };
    let Some(job) = window.job.as_ref().filter(|job| !job.state().is_finished()) else {
        return Err("no backup check is running".into());
    };
    job.cancel.cancel();
    Ok(())
}

pub(crate) fn automation_verify_report(
    app: &mut ZxrawApp,
) -> Option<std::result::Result<BackupReport, String>> {
    let window = find_verify_window(app)?;
    window.poll_messages();
    let ToolState::Verify(state) = &window.state else {
        unreachable!();
    };
    if let Some(report) = &state.report {
        Some(Ok(report.clone()))
    } else if window
        .job
        .as_ref()
        .is_none_or(|job| job.state().is_finished())
        && let Some(notice) = &window.notice
    {
        Some(Err(notice.clone()))
    } else {
        None
    }
}

pub(crate) fn automation_verify_export(
    app: &mut ZxrawApp,
    path: PathBuf,
    csv: bool,
) -> Result<(), String> {
    let Some(window) = find_verify_window(app) else {
        return Err("verify backup is not open".into());
    };
    let ToolState::Verify(state) = &mut window.state else {
        unreachable!();
    };
    export_verify_report(state, &path, csv)
}

fn render_window(window: &mut ToolWindow, ctx: &egui::Context) -> Option<UiAction> {
    let mut action = None;
    let title = window.title();
    let mut open = window.open;
    let job = window.job.clone();
    let notice = window.notice.clone();
    let size = match &window.state {
        ToolState::Folder(_) => [940.0, 760.0],
        ToolState::Duplicates(_) => [900.0, 700.0],
        ToolState::Checksums(_) => [900.0, 700.0],
        ToolState::Compare(_) => [960.0, 720.0],
        ToolState::Verify(_) => [960.0, 720.0],
    };
    egui::Window::new(title)
        .id(egui::Id::new(("tool-window", window.id)))
        .open(&mut open)
        .default_size(size)
        .min_size([620.0, 420.0])
        .resizable(true)
        .show(ctx, |ui| {
            if let Some(job) = &job {
                draw_job_progress(ui, job);
            }
            if let Some(notice) = &notice {
                ui.label(RichText::new(notice).color(theme::muted(ui)));
            }
            if let Some(message) = render_body(window, ui) {
                action = Some(message);
            }
        });
    window.open = open;
    action
}

fn draw_job_progress(ui: &mut egui::Ui, job: &JobHandle) {
    let progress = job.progress.snapshot();
    let status = format!("{} · {:?}", job.title, job.state());
    ui.horizontal(|ui| {
        ui.label(RichText::new(status).strong());
        if !job.state().is_finished() {
            ui.small_button("Cancel")
                .on_hover_text("Cancel this scan")
                .clicked()
                .then(|| job.cancel.cancel());
        }
    });
    let detail = if progress.bytes_total > 0 {
        format!(
            "{} / {}",
            format_size(progress.bytes_done),
            format_size(progress.bytes_total)
        )
    } else if progress.items_total > 0 {
        format!("{} / {} items", progress.items_done, progress.items_total)
    } else {
        format!(
            "{} item(s) · {}",
            progress.items_done,
            format_size(progress.bytes_done)
        )
    };
    if let Some(fraction) = progress.fraction() {
        ui.add(egui::ProgressBar::new(fraction).text(detail));
    } else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(detail);
        });
    }
    if !progress.current.is_empty() {
        ui.label(RichText::new(progress.current).small().weak());
    }
    ui.separator();
}

fn render_body(window: &mut ToolWindow, ui: &mut egui::Ui) -> Option<UiAction> {
    match &mut window.state {
        ToolState::Folder(state) => render_folder(window.id, state, ui),
        ToolState::Duplicates(state) => render_duplicates(window.id, state, ui),
        ToolState::Checksums(state) => render_checksums(window.id, state, ui),
        ToolState::Compare(state) => render_compare(window.id, state, ui),
        ToolState::Verify(state) => render_verify(window.id, state, ui),
    }
}

fn launch_action(
    jobs: &JobRegistry,
    event_tx: &Sender<Event>,
    app_ctx: &egui::Context,
    window: &mut ToolWindow,
    action: DeferredAction,
) {
    if let Some(old) = window.job.take() {
        old.cancel.cancel();
    }
    window.generation = window.generation.wrapping_add(1).max(1);
    let generation = window.generation;
    let result_tx = window.tx.clone();
    let ctx = app_ctx.clone();
    let event_tx = event_tx.clone();
    let (kind, title, job) = match action {
        DeferredAction::FolderScan(root) => {
            let title = format!("Scanning disk usage: {}", root.display());
            let result_tx = result_tx.clone();
            let ctx = ctx.clone();
            let handle = jobs.spawn("folder-size", title.clone(), move |job| {
                let result = disk_usage::scan(&root, &Default::default(), &JobCtx::from_job(job))
                    .map_err(|error| error.to_string());
                if let Ok(report) = &result {
                    for _ in 0..report.errors {
                        job.progress.add_error();
                    }
                }
                let err = result.as_ref().err().cloned();
                let _ = result_tx.send(ToolEnvelope {
                    generation,
                    payload: ToolPayload::Folder(result),
                });
                ctx.request_repaint();
                err.map_or(Ok(()), Err)
            });
            ("folder-size", title, handle)
        }
        DeferredAction::DuplicatesScan {
            folder,
            selection,
            options,
        } => {
            let title = format!("Finding duplicates in {}", folder.display());
            let result_tx = result_tx.clone();
            let ctx = ctx.clone();
            let handle = jobs.spawn("duplicates", title.clone(), move |job| {
                let roots = selected_directory_roots(&selection, &folder);
                let result = duplicates::find_duplicates(&roots, &options, &JobCtx::from_job(job))
                    .map_err(|error| error.to_string())
                    .map(|report| {
                        let mut modified = HashMap::new();
                        for group in &report.groups {
                            for path in &group.files {
                                if let Ok(metadata) = std::fs::metadata(path)
                                    && let Ok(time) = metadata.modified()
                                {
                                    modified.insert(path.clone(), time);
                                }
                            }
                        }
                        (report, modified)
                    });
                if let Ok((report, _)) = &result {
                    for _ in &report.errors {
                        job.progress.add_error();
                    }
                }
                let err = result.as_ref().err().cloned();
                let _ = result_tx.send(ToolEnvelope {
                    generation,
                    payload: ToolPayload::Duplicates(result),
                });
                ctx.request_repaint();
                err.map_or(Ok(()), Err)
            });
            ("duplicates", title, handle)
        }
        DeferredAction::ChecksumCount(work) => {
            let title = format!("Checking for online-only files: {}", work.describe());
            let result_tx = result_tx.clone();
            let ctx = ctx.clone();
            let handle = jobs.spawn("checksums-count", title.clone(), move |job| {
                job.progress.set_current("Checking for online-only files");
                let result = count_online_only(&work, &job.cancel)
                    .map_err(|error| error.to_string());
                let err = result.as_ref().err().cloned();
                let _ = result_tx.send(ToolEnvelope {
                    generation,
                    payload: ToolPayload::OnlineOnlyCount(work, result),
                });
                ctx.request_repaint();
                err.map_or(Ok(()), Err)
            });
            ("checksums-count", title, handle)
        }
        DeferredAction::Checksum { work, policy } => match work {
            ChecksumWork::Build { root, selection } => {
                let title = if selection.is_empty() {
                    format!("Building BLAKE3 manifest: {}", root.display())
                } else {
                    format!("Hashing {} selected item(s)", selection.len())
                };
                let result_tx = result_tx.clone();
                let ctx = ctx.clone();
                let handle = jobs.spawn("checksums", title.clone(), move |job| {
                    let result = manifest_for_selection(&root, &selection, policy, job)
                        .map_err(|error| error.to_string());
                    if let Ok(build) = &result {
                        for _ in &build.errors {
                            job.progress.add_error();
                        }
                    }
                    let err = result.as_ref().err().cloned();
                    let _ = result_tx.send(ToolEnvelope {
                        generation,
                        payload: ToolPayload::Manifest(result),
                    });
                    ctx.request_repaint();
                    err.map_or(Ok(()), Err)
                });
                ("checksums", title, handle)
            }
            ChecksumWork::Verify { manifest: path, .. } => {
                let title = format!("Verifying manifest: {}", path.display());
                let result_tx = result_tx.clone();
                let ctx = ctx.clone();
                let handle = jobs.spawn("verify-manifest", title.clone(), move |job| {
                    job.progress.set_current("Reading BLAKE3 manifest");
                    if let Ok(text) = std::fs::read_to_string(&path)
                        && let Ok(entries) = hashing::parse_manifest(&text)
                    {
                        job.progress.add_total_items(entries.len() as u64);
                    }
                    let result = hashing::verify_manifest_with(
                        &path,
                        &job.cancel,
                        Some(&job.progress),
                        policy,
                    )
                    .map_err(|error| error.to_string());
                    if let Ok(report) = &result {
                        for _ in &report.unreadable {
                            job.progress.add_error();
                        }
                    }
                    let err = result.as_ref().err().cloned();
                    let _ = result_tx.send(ToolEnvelope {
                        generation,
                        payload: ToolPayload::Verify(result),
                    });
                    ctx.request_repaint();
                    err.map_or(Ok(()), Err)
                });
                ("verify-manifest", title, handle)
            }
        },
        DeferredAction::SaveManifest {
            path,
            entries,
            skipped,
        } => {
            let title = format!("Saving BLAKE3 manifest: {}", path.display());
            let result_tx = result_tx.clone();
            let ctx = ctx.clone();
            let handle = jobs.spawn("save-manifest", title.clone(), move |job| {
                job.progress.add_total_items(entries.len() as u64);
                let result = hashing::write_manifest(&path, &entries)
                    .map(|()| path.clone())
                    .map_err(|error| error.to_string());
                job.progress.add_items(entries.len() as u64);
                let err = result.as_ref().err().cloned();
                let _ = result_tx.send(ToolEnvelope {
                    generation,
                    payload: ToolPayload::Saved(result, skipped),
                });
                ctx.request_repaint();
                err.map_or(Ok(()), Err)
            });
            ("save-manifest", title, handle)
        }
        DeferredAction::CompareScan {
            left,
            right,
            use_hash,
        } => {
            let title = format!("Comparing {} ↔ {}", left.display(), right.display());
            let result_tx = result_tx.clone();
            let ctx = ctx.clone();
            let handle = jobs.spawn("compare-folders", title.clone(), move |job| {
                let result = compare::compare_dirs(
                    &left,
                    &right,
                    &compare::CompareOptions {
                        use_hash,
                        ..Default::default()
                    },
                    &JobCtx::from_job(job),
                )
                .map_err(|error| error.to_string());
                if let Ok(report) = &result {
                    for _ in &report.errors {
                        job.progress.add_error();
                    }
                }
                let err = result.as_ref().err().cloned();
                let _ = result_tx.send(ToolEnvelope {
                    generation,
                    payload: ToolPayload::Compare(result),
                });
                ctx.request_repaint();
                err.map_or(Ok(()), Err)
            });
            ("compare-folders", title, handle)
        }
        DeferredAction::BuildSyncPlan {
            report,
            left,
            right,
            mode,
        } => {
            let title = format!("Building sync preview for {}", right.display());
            let result_tx = result_tx.clone();
            let ctx = ctx.clone();
            let handle = jobs.spawn("sync-preview", title.clone(), move |job| {
                job.progress.set_current("Fingerprinting preview paths");
                job.progress.add_total_items(report.entries.len() as u64);
                let job_ctx = JobCtx::from_job(job);
                let plan = compare::plan_sync_with_ctx(&report, &left, &right, mode, &job_ctx);
                job.progress.add_items(report.entries.len() as u64);
                let result = match plan {
                    Ok(plan) if !job.cancel.is_cancelled() => Ok(plan),
                    _ => Err("sync preview cancelled".to_owned()),
                };
                let err = result.as_ref().err().cloned();
                let _ = result_tx.send(ToolEnvelope {
                    generation,
                    payload: ToolPayload::SyncPlan(result),
                });
                ctx.request_repaint();
                err.map_or(Ok(()), Err)
            });
            ("sync-preview", title, handle)
        }
        DeferredAction::VerifyBackup {
            original,
            backup,
            options,
        } => {
            let title = format!(
                "Verifying backup {} against {}",
                backup.display(),
                original.display()
            );
            let result_tx = result_tx.clone();
            let ctx = ctx.clone();
            let handle = jobs.spawn("verify-backup", title.clone(), move |job| {
                let result =
                    match verify_backup(&original, &backup, &options, &JobCtx::from_job(job)) {
                        Ok(report) => {
                            for entry in &report.entries {
                                if matches!(entry.status, VerifyStatus::Error(_)) {
                                    job.progress.add_error();
                                }
                            }
                            Ok(report)
                        }
                        Err(CoreError::Cancelled) => Err("Backup check cancelled".to_owned()),
                        Err(error) => Err(error.to_string()),
                    };
                let err = result.as_ref().err().cloned();
                let _ = result_tx.send(ToolEnvelope {
                    generation,
                    payload: ToolPayload::BackupVerified(result),
                });
                ctx.request_repaint();
                err.map_or(Ok(()), Err)
            });
            ("verify-backup", title, handle)
        }
        DeferredAction::RecycleDuplicates {
            targets,
            bytes,
            checks,
        } => {
            let title = format!(
                "Moving {} duplicate copy/copies to the Recycle Bin",
                targets.len()
            );
            let result_tx = result_tx.clone();
            let ctx = ctx.clone();
            let tx = event_tx.clone();
            let handle = jobs.spawn("recycle-duplicates", title.clone(), move |job| {
                job.progress.add_total_items(targets.len() as u64);
                job.progress.add_total_bytes(bytes);
                job.progress
                    .set_current("Re-verifying duplicates before removal");
                let (verified, skipped) = verify_recycle(&checks, &job.cancel);
                for line in &skipped {
                    job.log(line.clone());
                }
                if !skipped.is_empty() {
                    let _ = tx.send(Event::Report {
                        title: "Some duplicates were not removed".into(),
                        lines: skipped.clone(),
                    });
                }
                debug_assert!(verified.iter().all(|v| targets.contains(v)));
                let targets = verified;
                if job.cancel.is_cancelled() || targets.is_empty() {
                    return Ok(());
                }
                job.progress
                    .set_current("Moving selected copies to the Recycle Bin");
                let result = forge_win::recycle(&targets).map_err(|error| error.to_string());
                job.progress.add_items(targets.len() as u64);
                job.progress.add_bytes(bytes);
                if let Err(error) = &result {
                    job.progress.add_error();
                    let _ = tx.send(Event::Error(format!("Recycle Bin move failed: {error}")));
                } else {
                    let mut parents: Vec<PathBuf> = targets
                        .iter()
                        .filter_map(|p| p.parent().map(Path::to_path_buf))
                        .collect();
                    parents.sort();
                    parents.dedup();
                    for dir in parents {
                        let _ = tx.send(Event::Changed(dir));
                    }
                    let _ = tx.send(Event::Toast(format!(
                        "Moved {} duplicate copy/copies to the Recycle Bin",
                        targets.len()
                    )));
                }
                let err = result.as_ref().err().cloned();
                let _ = result_tx.send(ToolEnvelope {
                    generation,
                    payload: ToolPayload::Recycled(result),
                });
                ctx.request_repaint();
                err.map_or(Ok(()), Err)
            });
            ("recycle-duplicates", title, handle)
        }
        DeferredAction::ExecuteSync(plan) => {
            let title = match plan.mode {
                SyncMode::Update => format!(
                    "Updating {} from {}",
                    plan.right.display(),
                    plan.left.display()
                ),
                SyncMode::Mirror => format!(
                    "Mirroring {} to {}",
                    plan.left.display(),
                    plan.right.display()
                ),
                SyncMode::TwoWay => format!(
                    "Syncing {} and {} both ways",
                    plan.left.display(),
                    plan.right.display()
                ),
            };
            let result_tx = result_tx.clone();
            let ctx = ctx.clone();
            let tx = event_tx.clone();
            let handle = jobs.spawn("sync-folders", title.clone(), move |job| {
                let mut recycle_one = |path: &Path| {
                    forge_win::recycle(std::slice::from_ref(&path.to_path_buf()))
                        .map_err(|error| error.to_string())
                };
                let report = compare::execute_sync(
                    &plan,
                    true,
                    Some(&mut recycle_one),
                    &JobCtx::from_job(job),
                );
                let mut changed = HashSet::new();
                changed.insert(plan.right.clone());
                if plan.mode == compare::SyncMode::TwoWay {
                    changed.insert(plan.left.clone());
                }
                for path in &report.done {
                    if let Some(parent) = path.parent() {
                        changed.insert(parent.to_path_buf());
                    }
                }
                for dir in changed {
                    let _ = tx.send(Event::Changed(dir));
                }
                let lines: Vec<String> = report
                    .failed
                    .iter()
                    .map(|issue| format!("FAILED  {}: {}", issue.path.display(), issue.reason))
                    .collect();
                let warnings = crate::modal::warning_lines(&report.warnings);
                for line in lines.iter().chain(&warnings) {
                    job.log(line.clone());
                }
                if lines.is_empty() && warnings.is_empty() {
                    let _ = tx.send(Event::Toast(sync_summary(&report)));
                } else {
                    let heading = if lines.is_empty() { "warnings" } else { "errors" };
                    let _ = tx.send(Event::ReportWithWarnings {
                        title: format!("Folder sync completed with {heading}"),
                        lines,
                        warnings: report.warnings.clone(),
                    });
                }
                for _ in &report.failed {
                    job.progress.add_error();
                }
                let _ = result_tx.send(ToolEnvelope {
                    generation,
                    payload: ToolPayload::Synced(report),
                });
                ctx.request_repaint();
                Ok(())
            });
            ("sync-folders", title, handle)
        }
        DeferredAction::ExportVerify { path, text } => {
            let title = format!("Saving report: {}", path.display());
            let result_tx = result_tx.clone();
            let ctx = ctx.clone();
            let handle = jobs.spawn("export-verify", title.clone(), move |_job| {
                let result = write_report_for_export(&path, &text);
                let err = result.as_ref().err().cloned();
                let _ = result_tx.send(ToolEnvelope {
                    generation,
                    payload: ToolPayload::Exported(result),
                });
                ctx.request_repaint();
                err.map_or(Ok(()), Err)
            });
            ("export-verify", title, handle)
        }
    };
    let _ = (kind, title);
    window.job = Some(job);
    window.notice = None;
}

fn selected_directory_roots(selection: &[PathBuf], fallback: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = selection
        .iter()
        .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|md| md.is_dir()))
        .cloned()
        .collect();
    dirs.sort();
    dirs.dedup();
    let sorted = dirs.clone();
    dirs.retain(|path| {
        !sorted
            .iter()
            .any(|other| other != path && forge_core::paths::is_within(other, path))
    });
    if dirs.is_empty() {
        vec![fallback.to_path_buf()]
    } else {
        dirs
    }
}

// -----------------------------------------------------------------------------
// Folder size
// -----------------------------------------------------------------------------

fn render_folder(id: u64, state: &mut FolderState, ui: &mut egui::Ui) -> Option<UiAction> {
    ui.label(RichText::new(state.root.display().to_string()).weak());
    let Some(report) = state.report.as_ref() else {
        if ui.button("Scan again").clicked() {
            state.report = None;
            return Some(UiAction::Start(DeferredAction::FolderScan(
                state.root.clone(),
            )));
        }
        return None;
    };
    if let Some((target, started)) = state.pending_focus {
        if started.elapsed() >= Duration::from_millis(360) {
            state.focused = target;
            state.selected_child = None;
            state.pending_focus = None;
        } else {
            ui.ctx().request_repaint_after(
                Duration::from_millis(370).saturating_sub(started.elapsed()),
            );
        }
    }
    state.focused = state.focused.min(report.nodes.len().saturating_sub(1));
    let root_size = report.root().size;
    let mut rescan = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Total").weak());
        ui.label(RichText::new(format_size(root_size)).strong());
        ui.separator();
        ui.label(format!("{} files", report.root().files));
        ui.label(format!("{} folders", report.root().dirs));
        ui.label(format!("{} inaccessible entries", report.errors));
        ui.label(format!("{} links skipped", report.links_skipped));
        if ui.button("Rescan").clicked() {
            rescan = true;
        }
    });
    if rescan {
        return Some(UiAction::Start(DeferredAction::FolderScan(
            state.root.clone(),
        )));
    }
    if report.errors > 0 || report.links_skipped > 0 {
        ui.collapsing("Scan details", |ui| {
            ui.label(format!("{} inaccessible entry/entries were recorded; links are counted but never followed.", report.errors));
            for issue in report.error_samples.iter().take(40) {
                ui.label(RichText::new(format!("{} — {}", issue.path.display(), issue.reason)).small());
            }
            if report.error_samples.len() > 40 {
                ui.label(format!("… and {} more error(s)", report.error_samples.len() - 40));
            }
        });
    }
    ui.separator();

    let mut breadcrumb = Vec::new();
    let mut cursor = Some(state.focused);
    while let Some(index) = cursor {
        breadcrumb.push(index);
        cursor = report.nodes.get(index).and_then(|node| node.parent);
    }
    breadcrumb.reverse();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Treemap").strong());
        for index in breadcrumb {
            ui.label("›");
            if let Some(node) = report.nodes.get(index)
                && ui.button(&node.name).clicked()
            {
                state.focused = index;
                state.selected_child = None;
                state.pending_focus = None;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(
                    "Click a block to drill in · double-click to open in the focused pane",
                )
                .small()
                .weak(),
            );
        });
    });

    let node = &report.nodes[state.focused];
    let mut specs: Vec<(String, PathBuf, u64, Option<usize>)> = node
        .children
        .iter()
        .map(|&child_index| {
            let child = &report.nodes[child_index];
            (
                child.name.clone(),
                child.path.clone(),
                child.size,
                Some(child_index),
            )
        })
        .collect();
    if node.own_size > 0 {
        specs.push((
            "Files in this folder".into(),
            node.path.clone(),
            node.own_size,
            None,
        ));
    }
    let values: Vec<u64> = specs.iter().map(|s| s.2).collect();
    let (treemap_rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), 250.0), Sense::hover());
    ui.painter()
        .rect_filled(treemap_rect, 4.0, ui.visuals().extreme_bg_color);
    let rects = squarified_treemap(treemap_rect.shrink(3.0), &values);
    let palette = treemap_palette();
    let mut navigation = None;
    for (i, ((name, path, size, child), rect)) in specs.iter().zip(rects.iter()).enumerate() {
        if *size == 0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
            continue;
        }
        let color = palette[i % palette.len()];
        let fill = color.linear_multiply(if ui.visuals().dark_mode { 0.70 } else { 0.48 });
        ui.painter().rect_filled(*rect, 3.0, fill);
        ui.painter().rect_stroke(
            *rect,
            3.0,
            Stroke::new(1.0, color),
            egui::StrokeKind::Inside,
        );
        if rect.width() >= 48.0 && rect.height() >= 30.0 {
            let shown_name = if rect.width() < 100.0 && name.chars().count() > 14 {
                format!("{}…", name.chars().take(13).collect::<String>())
            } else {
                name.clone()
            };
            let text_color = if ui.visuals().dark_mode {
                Color32::WHITE
            } else {
                Color32::from_rgb(24, 28, 34)
            };
            ui.painter().text(
                rect.left_top() + vec2(7.0, 7.0),
                Align2::LEFT_TOP,
                shown_name,
                FontId::proportional(12.0),
                text_color,
            );
            if rect.height() >= 44.0 {
                ui.painter().text(
                    rect.left_bottom() + vec2(7.0, -6.0),
                    Align2::LEFT_BOTTOM,
                    format_size(*size),
                    FontId::proportional(11.0),
                    text_color,
                );
            }
        }
        let response = ui.interact(
            *rect,
            egui::Id::new(("treemap-block", id, state.focused, i)),
            Sense::click(),
        );
        let response = response.on_hover_ui(|ui| {
            let percent = if node.size == 0 {
                0.0
            } else {
                *size as f64 * 100.0 / node.size as f64
            };
            ui.label(RichText::new(name).strong());
            ui.label(path.display().to_string());
            ui.label(format!(
                "{} · {percent:.1}% of {}",
                format_size(*size),
                node.name
            ));
        });
        if response.double_clicked() {
            if child.is_some() {
                navigation = Some(path.clone());
            }
            state.pending_focus = None;
        } else if response.clicked()
            && let Some(child_index) = child
        {
            state.pending_focus = Some((*child_index, Instant::now()));
        }
    }

    ui.add_space(5.0);
    let focused = state.focused;
    let sort_state = &mut state.sort;
    let selected_child = &mut state.selected_child;
    ui.columns(2, |columns| {
        columns[0].horizontal(|ui| {
            ui.label(RichText::new("Largest children").strong());
            for (label, sort_choice) in [
                ("Size ↓", ChildSort::Largest),
                ("Size ↑", ChildSort::Smallest),
                ("Name", ChildSort::Name),
            ] {
                if ui
                    .selectable_label(*sort_state == sort_choice, label)
                    .clicked()
                {
                    *sort_state = sort_choice;
                }
            }
        });
        render_child_list(
            id,
            focused,
            sort_state,
            selected_child,
            report,
            &mut columns[0],
            &mut navigation,
        );
        columns[1].label(RichText::new("Size by extension").strong());
        render_extension_bars(report, &mut columns[1]);
    });
    ui.separator();
    ui.label(
        RichText::new(format!(
            "Largest files · {} recorded",
            report.top_files.len()
        ))
        .strong(),
    );
    let mut top_navigation = None;
    egui::ScrollArea::vertical()
        .id_salt(("top-files", id))
        .max_height(150.0)
        .show_rows(ui, 22.0, report.top_files.len(), |ui, range| {
            for index in range {
                let (path, size) = &report.top_files[index];
                let response = ui.selectable_label(
                    false,
                    format!("{}   {}", path.display(), format_size(*size)),
                );
                if response.double_clicked() {
                    top_navigation = Some(path.clone());
                }
                response.on_hover_text(path.display().to_string());
            }
        });
    navigation = top_navigation.or(navigation);
    navigation.map(UiAction::Navigate)
}

fn render_child_list(
    id: u64,
    focused: usize,
    sort: &mut ChildSort,
    selected_child: &mut Option<usize>,
    report: &UsageReport,
    ui: &mut egui::Ui,
    navigate: &mut Option<PathBuf>,
) {
    let node = &report.nodes[focused];
    let mut children = node.children.clone();
    match *sort {
        ChildSort::Largest => {
            children.sort_by(|a, b| report.nodes[*b].size.cmp(&report.nodes[*a].size))
        }
        ChildSort::Smallest => {
            children.sort_by(|a, b| report.nodes[*a].size.cmp(&report.nodes[*b].size))
        }
        ChildSort::Name => children.sort_by(|a, b| {
            report.nodes[*a]
                .name
                .to_lowercase()
                .cmp(&report.nodes[*b].name.to_lowercase())
        }),
    }
    if children.is_empty() {
        ui.label(RichText::new("No child folders at this level.").weak());
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt(("children", id))
        .max_height(180.0)
        .show_rows(ui, 23.0, children.len(), |ui, range| {
            for row in range {
                let child_index = children[row];
                let child = &report.nodes[child_index];
                ui.horizontal(|ui| {
                    let response =
                        ui.selectable_label(*selected_child == Some(child_index), &child.name);
                    if response.clicked() {
                        *selected_child = Some(child_index);
                    }
                    if response.double_clicked() {
                        *navigate = Some(child.path.clone());
                    }
                    response.on_hover_text(child.path.display().to_string());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format_size(child.size));
                    });
                });
            }
        });
}

fn render_extension_bars(report: &UsageReport, ui: &mut egui::Ui) {
    if report.by_ext.is_empty() {
        ui.label(RichText::new("No files to summarize.").weak());
        return;
    }
    let max_bytes = report.by_ext.first().map_or(1, |e| e.bytes).max(1);
    let total = report.root().size.max(1);
    egui::ScrollArea::vertical()
        .max_height(180.0)
        .show(ui, |ui| {
            for item in report.by_ext.iter().take(12) {
                ui.horizontal(|ui| {
                    let name = if item.ext.is_empty() {
                        "(no extension)"
                    } else {
                        &item.ext
                    };
                    ui.label(name);
                    let width = (ui.available_width() - 110.0).max(40.0);
                    let (rect, _) = ui.allocate_exact_size(vec2(width, 13.0), Sense::hover());
                    ui.painter()
                        .rect_filled(rect, 3.0, ui.visuals().faint_bg_color);
                    let fill_width = width * item.bytes as f32 / max_bytes as f32;
                    ui.painter().rect_filled(
                        Rect::from_min_size(rect.min, vec2(fill_width.max(1.0), rect.height())),
                        3.0,
                        theme::accent(ui.ctx()),
                    );
                    let percent = item.bytes as f64 * 100.0 / total as f64;
                    ui.label(format!("{} · {:.1}%", format_size(item.bytes), percent));
                });
            }
        });
}

fn treemap_palette() -> [Color32; 6] {
    [
        theme::ACCENT,
        Color32::from_rgb(82, 170, 128),
        Color32::from_rgb(220, 151, 71),
        Color32::from_rgb(153, 123, 210),
        Color32::from_rgb(63, 170, 178),
        Color32::from_rgb(213, 103, 117),
    ]
}

/// Squarified treemap layout. The returned rectangles preserve the input order; zero-sized
/// items receive a zero-area rectangle. Every positive area is proportional to its value.
fn squarified_treemap(area: Rect, values: &[u64]) -> Vec<Rect> {
    let mut result = vec![Rect::from_min_size(area.min, Vec2::ZERO); values.len()];
    let total_value: f64 = values.iter().map(|&v| v as f64).sum();
    let total_area = area.width().max(0.0) as f64 * area.height().max(0.0) as f64;
    if total_value <= 0.0 || total_area <= 0.0 || !total_area.is_finite() {
        return result;
    }
    let mut items: Vec<(usize, f64)> = values
        .iter()
        .enumerate()
        .filter(|(_, value)| **value > 0)
        .map(|(index, value)| (index, *value as f64 / total_value * total_area))
        .collect();
    items.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut remaining = area;
    let mut row: Vec<(usize, f64)> = Vec::new();
    let mut next = 0;
    while next < items.len() {
        let short_side = remaining.width().min(remaining.height()).max(0.0) as f64;
        let mut candidate = row.clone();
        candidate.push(items[next]);
        if row.is_empty() || worst_aspect(&candidate, short_side) <= worst_aspect(&row, short_side)
        {
            row.push(items[next]);
            next += 1;
        } else {
            place_treemap_row(&mut remaining, &row, &mut result);
            row.clear();
        }
    }
    if !row.is_empty() {
        place_treemap_row(&mut remaining, &row, &mut result);
    }
    result
}

fn worst_aspect(row: &[(usize, f64)], short_side: f64) -> f64 {
    if row.is_empty() || short_side <= 0.0 {
        return f64::INFINITY;
    }
    let sum: f64 = row.iter().map(|item| item.1).sum();
    let min = row.iter().map(|item| item.1).fold(f64::INFINITY, f64::min);
    let max = row.iter().map(|item| item.1).fold(0.0, f64::max);
    let s2 = short_side * short_side;
    (s2 * max / (sum * sum)).max((sum * sum) / (s2 * min))
}

fn place_treemap_row(remaining: &mut Rect, row: &[(usize, f64)], output: &mut [Rect]) {
    let row_area: f64 = row.iter().map(|item| item.1).sum();
    if row_area <= 0.0 || remaining.width() <= 0.0 || remaining.height() <= 0.0 {
        return;
    }
    if remaining.width() >= remaining.height() {
        let strip_width = (row_area / remaining.height() as f64) as f32;
        let strip_width = strip_width.clamp(0.0, remaining.width());
        let mut y = remaining.top();
        for (position, (index, item_area)) in row.iter().enumerate() {
            let height = if position + 1 == row.len() {
                remaining.bottom() - y
            } else {
                (*item_area / strip_width.max(f32::MIN_POSITIVE) as f64) as f32
            };
            output[*index] = Rect::from_min_max(
                pos2(remaining.left(), y),
                pos2(remaining.left() + strip_width, y + height.max(0.0)),
            );
            y += height;
        }
        remaining.min.x += strip_width;
    } else {
        let strip_height = (row_area / remaining.width() as f64) as f32;
        let strip_height = strip_height.clamp(0.0, remaining.height());
        let mut x = remaining.left();
        for (position, (index, item_area)) in row.iter().enumerate() {
            let width = if position + 1 == row.len() {
                remaining.right() - x
            } else {
                (*item_area / strip_height.max(f32::MIN_POSITIVE) as f64) as f32
            };
            output[*index] = Rect::from_min_max(
                pos2(x, remaining.top()),
                pos2(x + width.max(0.0), remaining.top() + strip_height),
            );
            x += width;
        }
        remaining.min.y += strip_height;
    }
}

// -----------------------------------------------------------------------------
// Duplicates
// -----------------------------------------------------------------------------

/// Remember a button's egui id so the test hook can click it through real pointer input
/// (`ui tools click-button <label>`). Only debug and automation builds keep the id.
pub(crate) fn register_click_target(ctx: &egui::Context, label: &str, response: &egui::Response) {
    #[cfg(any(debug_assertions, feature = "automation"))]
    ctx.data_mut(|data| {
        data.insert_temp(egui::Id::new(("testhook-tools-button", label)), response.id);
    });
    #[cfg(not(any(debug_assertions, feature = "automation")))]
    let _ = (ctx, label, response);
}

#[derive(Clone, Copy)]
enum KeepRule {
    Newest,
    Oldest,
    ShortestPath,
}

/// Return the copies to tick for removal, always leaving one copy in the group.
fn copies_to_tick(
    group: &DuplicateGroup,
    rule: KeepRule,
    modified: &HashMap<PathBuf, SystemTime>,
) -> BTreeSet<usize> {
    if group.files.len() < 2 {
        return BTreeSet::new();
    }
    let keep = match rule {
        KeepRule::Newest => group
            .files
            .iter()
            .enumerate()
            .max_by(|(ia, a), (ib, b)| {
                modified
                    .get(*a)
                    .cmp(&modified.get(*b))
                    .then_with(|| ib.cmp(ia))
            })
            .map(|(index, _)| index)
            .unwrap_or(0),
        KeepRule::Oldest => group
            .files
            .iter()
            .enumerate()
            .min_by(|(ia, a), (ib, b)| {
                modified
                    .get(*a)
                    .cmp(&modified.get(*b))
                    .then_with(|| ia.cmp(ib))
            })
            .map(|(index, _)| index)
            .unwrap_or(0),
        KeepRule::ShortestPath => group
            .files
            .iter()
            .enumerate()
            .min_by(|(ia, a), (ib, b)| {
                a.as_os_str()
                    .len()
                    .cmp(&b.as_os_str().len())
                    .then_with(|| a.cmp(b))
                    .then_with(|| ia.cmp(ib))
            })
            .map(|(index, _)| index)
            .unwrap_or(0),
    };
    (0..group.files.len())
        .filter(|index| *index != keep)
        .collect()
}

fn selection_is_safe(groups: &[DuplicateGroup], selected: &BTreeSet<(usize, usize)>) -> bool {
    groups.iter().enumerate().all(|(group_index, group)| {
        !group.files.is_empty()
            && selected
                .iter()
                .filter(|(g, file)| *g == group_index && *file < group.files.len())
                .count()
                < group.files.len()
    })
}

fn selected_duplicate_targets(
    groups: &[DuplicateGroup],
    selected: &BTreeSet<(usize, usize)>,
) -> Option<(Vec<PathBuf>, u64)> {
    if !selection_is_safe(groups, selected) {
        return None;
    }
    let mut targets = Vec::new();
    let mut bytes = 0u64;
    for &(group_index, file_index) in selected {
        let group = groups.get(group_index)?;
        let path = group.files.get(file_index)?;
        targets.push(path.clone());
        bytes = bytes.saturating_add(group.size);
    }
    targets.sort();
    targets.dedup();
    Some((targets, bytes))
}

/// One duplicate group as the user decided it: `keep` stays, `remove` goes to the Recycle
/// Bin. Re-checked against the disk immediately before acting, because files can change
/// between the scan and the click.
#[derive(Debug, Clone)]
struct RecycleCheck {
    size: u64,
    hash: Option<forge_core::hashing::Digest>,
    keep: Vec<PathBuf>,
    remove: Vec<PathBuf>,
}

fn recycle_checks(
    groups: &[DuplicateGroup],
    selected: &BTreeSet<(usize, usize)>,
) -> Vec<RecycleCheck> {
    groups
        .iter()
        .enumerate()
        .filter_map(|(gi, g)| {
            let (remove, keep): (Vec<_>, Vec<_>) = g
                .files
                .iter()
                .enumerate()
                .partition(|(fi, _)| selected.contains(&(gi, *fi)));
            (!remove.is_empty()).then(|| RecycleCheck {
                size: g.size,
                hash: g.hash,
                keep: keep.into_iter().map(|(_, p)| p.clone()).collect(),
                remove: remove.into_iter().map(|(_, p)| p.clone()).collect(),
            })
        })
        .collect()
}

/// Does `path` still hold exactly the group's content?
fn still_matches(
    path: &Path,
    size: u64,
    hash: Option<forge_core::hashing::Digest>,
    cancel: &forge_jobs::CancelToken,
) -> bool {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !md.is_file() || md.len() != size {
        return false;
    }
    match hash {
        None => size == 0,
        // An online-only copy cannot be verified without downloading it: not a match.
        Some(_) if !forge_core::cloud::background_read_allowed(path) => false,
        Some(h) => forge_core::hashing::hash_file(path, cancel, None).is_ok_and(|d| d == h),
    }
}

/// Returns the paths that are safe to recycle and a description of anything skipped. A
/// group is skipped entirely unless at least one kept copy still matches; individual
/// removal candidates that no longer match are skipped (they are no longer duplicates).
fn verify_recycle(
    checks: &[RecycleCheck],
    cancel: &forge_jobs::CancelToken,
) -> (Vec<PathBuf>, Vec<String>) {
    let mut ok = Vec::new();
    let mut skipped = Vec::new();
    for c in checks {
        if cancel.is_cancelled() {
            break;
        }
        if !c
            .keep
            .iter()
            .any(|k| still_matches(k, c.size, c.hash, cancel))
        {
            skipped.push(format!(
                "Skipped group of {} file(s): no kept copy still has the original content",
                c.remove.len() + c.keep.len()
            ));
            continue;
        }
        for r in &c.remove {
            if still_matches(r, c.size, c.hash, cancel) {
                ok.push(r.clone());
            } else {
                skipped.push(format!(
                    "Skipped {}: changed or missing since the scan",
                    r.display()
                ));
            }
        }
    }
    (ok, skipped)
}

fn render_duplicates(id: u64, state: &mut DuplicatesState, ui: &mut egui::Ui) -> Option<UiAction> {
    let roots_label = if state.selection.is_empty() {
        state.folder.display().to_string()
    } else {
        format!(
            "{} selected item(s); selected folders are scanned",
            state.selection.len()
        )
    };
    ui.label(RichText::new(roots_label).weak());
    let mut scan_clicked = false;
    ui.horizontal_wrapped(|ui| {
        ui.label("Minimum file size");
        ui.add(
            egui::DragValue::new(&mut state.min_size)
                .range(0..=u64::MAX)
                .suffix(" bytes"),
        );
        ui.checkbox(&mut state.include_empty, "Include empty files");
        ui.checkbox(
            &mut state.byte_compare,
            "Confirm byte-for-byte after BLAKE3",
        );
        let scan_label = if state.report.is_some() {
            "Scan again"
        } else {
            "Scan"
        };
        let scan = ui.button(scan_label);
        register_click_target(ui.ctx(), "duplicates:Scan", &scan);
        if scan.clicked() {
            state.report = None;
            state.selected.clear();
            state.confirming = false;
            scan_clicked = true;
        }
    });
    if scan_clicked {
        return Some(UiAction::Start(DeferredAction::DuplicatesScan {
            folder: state.folder.clone(),
            selection: state.selection.clone(),
            options: DupOptions {
                min_size: state.min_size,
                include_empty: state.include_empty,
                byte_compare: state.byte_compare,
            },
        }));
    }

    let Some(report) = state.report.as_ref() else {
        ui.label(RichText::new("Files are grouped by size, partial hash and full BLAKE3. No copies are selected by default.").weak());
        return None;
    };
    let total_wasted: u64 = report.groups.iter().map(DuplicateGroup::wasted_bytes).sum();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{} duplicate group(s)", report.groups.len())).strong());
        ui.label(format!("{} files scanned", report.files_scanned));
        ui.label(format!("{} reclaimable", format_size(total_wasted)));
        ui.label(format!("{} links skipped", report.links_skipped));
    });
    if !report.errors.is_empty() {
        ui.collapsing(format!("{} scan issue(s)", report.errors.len()), |ui| {
            for issue in report.errors.iter().take(60) {
                ui.label(
                    RichText::new(format!("{} — {}", issue.path.display(), issue.reason)).small(),
                );
            }
        });
    }
    if report.groups.is_empty() {
        ui.add_space(8.0);
        ui.label("No duplicate files found with these options.");
    } else {
        ui.label(RichText::new("Tick only the copies you want to recycle. Every group must keep at least one copy.").weak());
        let mut chosen_rule = None;
        egui::ScrollArea::vertical()
            .id_salt(("duplicate-groups", id))
            .max_height(340.0)
            .show(ui, |ui| {
                for (group_index, group) in report.groups.iter().enumerate() {
                    ui.group(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(format!("Group {}", group_index + 1)).strong());
                            ui.label(format!(
                                "{} copies · {} each · {} wasted",
                                group.files.len(),
                                format_size(group.size),
                                format_size(group.wasted_bytes())
                            ));
                            let newest = ui.small_button("Keep newest");
                            register_click_target(ui.ctx(), "duplicates:Keep newest", &newest);
                            if newest.clicked() {
                                chosen_rule = Some((group_index, KeepRule::Newest));
                            }
                            let oldest = ui.small_button("Keep oldest");
                            register_click_target(ui.ctx(), "duplicates:Keep oldest", &oldest);
                            if oldest.clicked() {
                                chosen_rule = Some((group_index, KeepRule::Oldest));
                            }
                            let shortest = ui.small_button("Keep shortest path");
                            register_click_target(
                                ui.ctx(),
                                "duplicates:Keep shortest path",
                                &shortest,
                            );
                            if shortest.clicked() {
                                chosen_rule = Some((group_index, KeepRule::ShortestPath));
                            }
                        });
                        for (file_index, path) in group.files.iter().enumerate() {
                            let mut checked = state.selected.contains(&(group_index, file_index));
                            let label = path.display().to_string();
                            if ui.checkbox(&mut checked, label).changed() {
                                if checked {
                                    state.selected.insert((group_index, file_index));
                                } else {
                                    state.selected.remove(&(group_index, file_index));
                                }
                            }
                        }
                    });
                }
            });
        if let Some((group_index, rule)) = chosen_rule {
            let to_tick = copies_to_tick(&report.groups[group_index], rule, &state.modified);
            state.selected.retain(|(group, _)| *group != group_index);
            state
                .selected
                .extend(to_tick.into_iter().map(|file| (group_index, file)));
        }
    }
    let targets = selected_duplicate_targets(&report.groups, &state.selected);
    let (target_count, target_bytes) = targets
        .as_ref()
        .map_or((0, 0), |(paths, bytes)| (paths.len(), *bytes));
    ui.horizontal(|ui| {
        ui.label(format!(
            "{} selected · {}",
            target_count,
            format_size(target_bytes)
        ));
        let safe = targets.is_some() && target_count > 0;
        let move_ticked = ui.add_enabled(safe, egui::Button::new("Move ticked to Recycle Bin"));
        register_click_target(
            ui.ctx(),
            "duplicates:Move ticked to Recycle Bin",
            &move_ticked,
        );
        if move_ticked.clicked() {
            state.confirming = true;
        }
        if state.confirming && ui.button("Cancel confirmation").clicked() {
            state.confirming = false;
        }
    });
    if state.confirming {
        if let Some((paths, bytes)) = selected_duplicate_targets(&report.groups, &state.selected) {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(RichText::new("Confirm Recycle Bin move").strong());
                ui.label(format!("Move {} selected file(s), totaling {}, to the Recycle Bin? Every duplicate group will retain at least one copy.", paths.len(), format_size(bytes)));
                egui::ScrollArea::vertical().max_height(100.0).show(ui, |ui| {
                    for path in paths.iter().take(40) { ui.label(path.display().to_string()); }
                    if paths.len() > 40 { ui.label(format!("… and {} more", paths.len() - 40)); }
                });
                ui.horizontal(|ui| {
                    let move_confirm = ui.button(
                        RichText::new("Move to Recycle Bin")
                            .color(Color32::WHITE)
                            .strong(),
                    );
                    register_click_target(
                        ui.ctx(),
                        "duplicates:Confirm Move to Recycle Bin",
                        &move_confirm,
                    );
                    if move_confirm.clicked() {
                        ui.ctx().data_mut(|data| data.insert_temp(egui::Id::new(("dup-confirm", id)), true));
                    }
                    let keep_copies = ui.button("Keep copies");
                    register_click_target(ui.ctx(), "duplicates:Keep copies", &keep_copies);
                    if keep_copies.clicked() { state.confirming = false; }
                });
            });
        } else {
            ui.colored_label(
                ui.visuals().error_fg_color,
                "Action refused: each group must keep at least one copy.",
            );
        }
    }
    let confirmed = ui
        .ctx()
        .data(|data| data.get_temp::<bool>(egui::Id::new(("dup-confirm", id))))
        .unwrap_or(false);
    if confirmed {
        ui.ctx()
            .data_mut(|data| data.remove::<bool>(egui::Id::new(("dup-confirm", id))));
        state.confirming = false;
        if let Some((targets, bytes)) = selected_duplicate_targets(&report.groups, &state.selected)
        {
            let checks = recycle_checks(&report.groups, &state.selected);
            return Some(UiAction::Start(DeferredAction::RecycleDuplicates {
                targets,
                bytes,
                checks,
            }));
        }
    }
    None
}

// -----------------------------------------------------------------------------
// Checksums and manifest verification
// -----------------------------------------------------------------------------

fn render_checksums(id: u64, state: &mut ChecksumsState, ui: &mut egui::Ui) -> Option<UiAction> {
    ui.label(
        RichText::new(if state.selection.is_empty() {
            format!("Folder: {}", state.root.display())
        } else {
            format!(
                "{} selected item(s) in {}",
                state.selection.len(),
                state.root.display()
            )
        })
        .weak(),
    );
    let mut rescan = false;
    let mut save_path = None;
    let mut verify_path = None;
    // While the online-only prompt waits, its answer is the only way on: a new count would
    // leave the prompt showing stale totals.
    let idle = state.prompt.is_none();
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                idle,
                egui::Button::new(if state.entries.is_some() {
                    "Rehash"
                } else {
                    "Build BLAKE3 manifest"
                }),
            )
            .clicked()
        {
            rescan = true;
        }
        if ui
            .add_enabled(
                idle && state.entries.is_some(),
                egui::Button::new("Save manifest…"),
            )
            .clicked()
            && let Some(path) =
                crate::dialogs::save_file(rfd::FileDialog::new().set_file_name("checksums.b3"))
        {
            save_path = Some(path);
        }
        if ui
            .add_enabled(idle, egui::Button::new("Verify manifest…"))
            .clicked()
            && let Some(path) = crate::dialogs::pick_file(
                rfd::FileDialog::new().add_filter("BLAKE3 manifest", &["b3", "txt"]),
            )
        {
            verify_path = Some(path);
        }
    });
    if let Some((totals, text)) = state
        .prompt
        .as_ref()
        .map(|prompt| (prompt.totals, online_only_prompt_text(prompt)))
    {
        let mut policy = None;
        let mut cancel = false;
        ui.separator();
        ui.label(RichText::new(text).strong());
        ui.label(
            RichText::new(if totals.manifest_online_only {
                "Skip stops here: the manifest is needed to know which files to check."
            } else {
                "Hashing reads each file whole, so it downloads it from the cloud first."
            })
            .weak(),
        );
        ui.horizontal(|ui| {
            let download = ui.button("Download and hash");
            register_click_target(ui.ctx(), "checksums:Download and hash", &download);
            if download.clicked() {
                policy = Some(OnlineOnlyPolicy::Download);
            }
            let skip = ui.button("Skip online-only");
            register_click_target(ui.ctx(), "checksums:Skip online-only", &skip);
            if skip.clicked() {
                policy = Some(OnlineOnlyPolicy::Skip);
            }
            let cancel_button = ui.button("Cancel");
            register_click_target(ui.ctx(), "checksums:Cancel", &cancel_button);
            if cancel_button.clicked() {
                cancel = true;
            }
        });
        if let Some(policy) = policy
            && let Some(prompt) = state.prompt.take()
        {
            return Some(answer_online_only_prompt(prompt, policy));
        }
        if cancel {
            state.prompt = None;
        }
    }
    if rescan {
        return Some(UiAction::Start(DeferredAction::ChecksumCount(
            ChecksumWork::Build {
                root: state.root.clone(),
                selection: state.selection.clone(),
            },
        )));
    }
    if let Some(path) = save_path
        && let Some(entries) = &state.entries
    {
        return Some(UiAction::Start(DeferredAction::SaveManifest {
            path,
            entries: entries.clone(),
            skipped: state.skipped.len(),
        }));
    }
    if let Some(path) = verify_path {
        return Some(UiAction::Start(DeferredAction::ChecksumCount(
            ChecksumWork::Verify {
                manifest: path,
                manifest_allowed: false,
            },
        )));
    }
    if let Some(entries) = &state.entries {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} BLAKE3 hash(es)", entries.len())).strong());
            if state.errors.is_empty() {
                ui.label("No read errors");
            } else {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("{} read error(s)", state.errors.len()),
                );
            }
        });
        egui::ScrollArea::vertical()
            .id_salt(("manifest-entries", id))
            .max_height(270.0)
            .show_rows(ui, 24.0, entries.len(), |ui, range| {
                for index in range {
                    let entry = &entries[index];
                    let hex = hashing::to_hex(&entry.hash);
                    ui.horizontal(|ui| {
                        ui.label(entry.rel_path.clone());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Copy hash").clicked() {
                                ui.ctx().copy_text(hex.clone());
                            }
                            ui.monospace(&hex);
                        });
                    });
                }
            });
        if !state.errors.is_empty() {
            ui.collapsing("Files that could not be hashed", |ui| {
                for (path, error) in state.errors.iter().take(80) {
                    ui.label(format!("{} — {error}", path.display()));
                }
            });
        }
        if !state.skipped.is_empty() {
            ui.collapsing(
                format!("{} skipped (online-only)", state.skipped.len()),
                |ui| {
                    for path in &state.skipped {
                        ui.label(path);
                    }
                },
            );
        }
    } else {
        ui.label(RichText::new("Hashes stream into the Operations progress row. The saved file uses b3sum-compatible output.").weak());
    }
    if let Some(report) = &state.verify {
        let status = if report.all_ok() {
            "OK"
        } else {
            "MISMATCH / MISSING"
        };
        ui.separator();
        ui.label(
            RichText::new(format!("Verification: {status}"))
                .strong()
                .color(if report.all_ok() {
                    Color32::from_rgb(76, 185, 112)
                } else {
                    ui.visuals().error_fg_color
                }),
        );
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{} OK", report.ok.len()));
            ui.label(format!("{} MISMATCH", report.mismatched.len()));
            ui.label(format!("{} MISSING", report.missing.len()));
            ui.label(format!("{} unreadable", report.unreadable.len()));
            ui.label(format!(
                "{} skipped (online-only)",
                report.skipped_online_only.len()
            ));
        });
        egui::ScrollArea::vertical()
            .id_salt(("verify-results", id))
            .max_height(160.0)
            .show(ui, |ui| {
                for path in &report.ok {
                    ui.colored_label(Color32::from_rgb(76, 185, 112), format!("OK       {path}"));
                }
                for path in &report.mismatched {
                    ui.colored_label(ui.visuals().error_fg_color, format!("MISMATCH {path}"));
                }
                for path in &report.missing {
                    ui.colored_label(ui.visuals().error_fg_color, format!("MISSING  {path}"));
                }
                for (path, error) in &report.unreadable {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("UNREADABLE {path}: {error}"),
                    );
                }
                for path in &report.skipped_online_only {
                    ui.label(format!("SKIPPED  {path} (online-only)"));
                }
            });
    }
    None
}

/// The paths a selection hashes: a selected item inside another selected folder is covered
/// by that folder.
fn selection_roots(selection: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = selection.to_vec();
    // By depth, then by path, so duplicates are adjacent for `dedup`.
    paths.sort_by(|a, b| (a.components().count(), a).cmp(&(b.components().count(), b)));
    paths.dedup();
    let mut roots: Vec<PathBuf> = Vec::new();
    for path in paths {
        if roots
            .iter()
            .any(|parent| forge_core::paths::is_within(parent, &path))
        {
            continue;
        }
        roots.push(path);
    }
    roots
}

fn manifest_for_selection(
    root: &Path,
    selection: &[PathBuf],
    policy: OnlineOnlyPolicy,
    job: &JobHandle,
) -> forge_core::Result<hashing::ManifestBuild> {
    let progress = Some(job.progress.as_ref());
    if selection.is_empty() {
        return hashing::build_manifest_with(root, &[], &job.cancel, progress, policy);
    }
    let mut build = hashing::ManifestBuild::default();
    for path in selection_roots(selection) {
        job.cancel
            .check()
            .map_err(|_| forge_core::error::CoreError::Cancelled)?;
        job.progress.set_current(path.display().to_string());
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                build.errors.push((path.clone(), error.to_string()));
                continue;
            }
        };
        if metadata.is_dir() {
            let mut found =
                hashing::build_manifest_with(&path, &[], &job.cancel, progress, policy)?;
            let prefix = forge_core::paths::to_slash(
                path.strip_prefix(root).unwrap_or_else(|_| Path::new("")),
            );
            let with_prefix = |rel: String| {
                if prefix.is_empty() {
                    rel
                } else {
                    format!("{prefix}/{rel}")
                }
            };
            for entry in &mut found.entries {
                entry.rel_path = with_prefix(std::mem::take(&mut entry.rel_path));
            }
            build.entries.extend(found.entries);
            build.errors.append(&mut found.errors);
            build.skipped_online_only.extend(
                found
                    .skipped_online_only
                    .into_iter()
                    .map(with_prefix),
            );
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .ok()
                .filter(|rel| !rel.as_os_str().is_empty())
                .map(forge_core::paths::to_slash)
                .or_else(|| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| path.display().to_string());
            match hashing::hash_file_with(&path, &job.cancel, Some(&job.progress), policy) {
                Ok(Some(hash)) => {
                    build.entries.push(ManifestEntry {
                        hash,
                        rel_path: relative,
                    });
                    job.progress.add_items(1);
                }
                Ok(None) => {
                    build.skipped_online_only.push(relative);
                    job.progress.add_items(1);
                }
                Err(forge_core::error::CoreError::Cancelled) => {
                    return Err(forge_core::error::CoreError::Cancelled);
                }
                Err(error) => build.errors.push((path.clone(), error.to_string())),
            }
        } else {
            build
                .errors
                .push((path, "links and non-regular items are skipped".into()));
        }
    }
    build.entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    build.skipped_online_only.sort();
    Ok(build)
}

impl ChecksumWork {
    fn describe(&self) -> String {
        match self {
            Self::Build { root, selection } if selection.is_empty() => root.display().to_string(),
            Self::Build { selection, .. } => format!("{} selected item(s)", selection.len()),
            Self::Verify { manifest, .. } => manifest.display().to_string(),
        }
    }
}

/// How many files the job would read that are online-only. Phase 1 of a checksum job.
fn count_online_only(
    work: &ChecksumWork,
    cancel: &CancelToken,
) -> forge_core::Result<OnlineOnlyTotals> {
    match work {
        ChecksumWork::Build { root, selection } if selection.is_empty() => {
            hashing::count_online_only_tree(root, cancel)
        }
        ChecksumWork::Build { selection, .. } => {
            let mut totals = OnlineOnlyTotals::default();
            for path in selection_roots(selection) {
                cancel.check()?;
                match std::fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.is_dir() => {
                        let tree = hashing::count_online_only_tree(&path, cancel)?;
                        totals.count += tree.count;
                        totals.bytes += tree.bytes;
                    }
                    Ok(metadata) if metadata.is_file() => totals.add_file(&path),
                    _ => {}
                }
            }
            Ok(totals)
        }
        ChecksumWork::Verify {
            manifest,
            manifest_allowed,
        } => hashing::count_online_only_manifest(manifest, cancel, *manifest_allowed),
    }
}

/// What the user's answer to the online-only prompt starts. For an online-only manifest,
/// Download reads it (then counts its files and may ask again) and Skip stops: without the
/// manifest there is nothing to verify.
fn answer_online_only_prompt(prompt: OnlineOnlyPrompt, policy: OnlineOnlyPolicy) -> UiAction {
    match prompt.work {
        ChecksumWork::Verify { manifest, .. } if prompt.totals.manifest_online_only => {
            match policy {
                OnlineOnlyPolicy::Download => {
                    UiAction::Start(DeferredAction::ChecksumCount(ChecksumWork::Verify {
                        manifest,
                        manifest_allowed: true,
                    }))
                }
                OnlineOnlyPolicy::Skip => UiAction::Notice(format!(
                    "Verification stopped: {} is online-only and was not read",
                    manifest.display()
                )),
            }
        }
        work => UiAction::Start(DeferredAction::Checksum { work, policy }),
    }
}

fn online_only_prompt_text(prompt: &OnlineOnlyPrompt) -> String {
    let totals = prompt.totals;
    let size = format_size(totals.bytes);
    if totals.manifest_online_only {
        let name = match &prompt.work {
            ChecksumWork::Verify { manifest, .. } => manifest
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| manifest.display().to_string()),
            ChecksumWork::Build { .. } => "file".into(),
        };
        return format!(
            "The manifest {name} is online-only and would be downloaded ({size}) before its \
             files can be checked."
        );
    }
    if totals.count == 1 {
        format!("1 file is online-only and would be downloaded ({size}).")
    } else {
        format!(
            "{} files are online-only and would be downloaded ({size}).",
            totals.count
        )
    }
}

fn saved_notice(path: &Path, skipped: usize) -> String {
    let saved = format!("Manifest saved to {}", path.display());
    match skipped {
        0 => saved,
        1 => format!("{saved}. 1 online-only file was skipped and is not in it"),
        n => format!("{saved}. {n} online-only files were skipped and are not in it"),
    }
}

fn verify_summary(report: &VerifyReport) -> String {
    let counts = if report.all_ok() {
        format!("Verification complete: {} file(s) OK", report.ok.len())
    } else {
        format!(
            "Verification complete: {} mismatch, {} missing, {} unreadable",
            report.mismatched.len(),
            report.missing.len(),
            report.unreadable.len()
        )
    };
    match report.skipped_online_only.len() {
        0 => counts,
        skipped => format!("{counts}, {skipped} skipped (online-only)"),
    }
}

// -----------------------------------------------------------------------------
// Folder comparison and one-way sync
// -----------------------------------------------------------------------------

fn render_compare(id: u64, state: &mut CompareState, ui: &mut egui::Ui) -> Option<UiAction> {
    let mut picked_right = None;
    let mut compare_now = false;
    let mut verify_now = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Left").strong());
        ui.label(state.left.display().to_string());
        ui.separator();
        ui.label(RichText::new("Right").strong());
        if let Some(right) = &state.right {
            ui.label(right.display().to_string());
        } else {
            ui.label(RichText::new("Choose a folder").weak());
        }
        if ui.button("Choose right folder…").clicked() {
            picked_right = crate::dialogs::pick_folder(rfd::FileDialog::new());
        }
        ui.checkbox(&mut state.use_hash, "Compare file contents with BLAKE3");
        if state.right.is_some() && ui.button("Compare / refresh").clicked() {
            compare_now = true;
        }
        if state.right.is_some()
            && ui
                .small_button("Verify backup by content…")
                .on_hover_text("Check that the right folder is a complete backup of the left")
                .clicked()
        {
            verify_now = true;
        }
    });
    if verify_now && let Some(backup) = &state.right {
        return Some(UiAction::OpenVerify {
            original: state.left.clone(),
            backup: backup.clone(),
        });
    }
    if let Some(right) = picked_right {
        state.right = Some(right.clone());
        state.report = None;
        state.plan = None;
        state.confirming_mirror = false;
        return Some(UiAction::Start(DeferredAction::CompareScan {
            left: state.left.clone(),
            right,
            use_hash: state.use_hash,
        }));
    }
    if compare_now && let Some(right) = &state.right {
        state.report = None;
        state.plan = None;
        state.confirming_mirror = false;
        return Some(UiAction::Start(DeferredAction::CompareScan {
            left: state.left.clone(),
            right: right.clone(),
            use_hash: state.use_hash,
        }));
    }
    if state.right.is_none() {
        ui.add_space(10.0);
        ui.label(
            "Compare the focused pane folder with the other pane, or choose a right-side folder.",
        );
        return None;
    }
    let Some(report) = state.report.as_ref() else {
        ui.label(
            RichText::new(
                "Comparison results will appear here. The scan runs as a cancellable job.",
            )
            .weak(),
        );
        return None;
    };

    let left_only = report.count(|s| matches!(s, EntryState::LeftOnly));
    let right_only = report.count(|s| matches!(s, EntryState::RightOnly));
    let different =
        report.count(|s| matches!(s, EntryState::Different(_) | EntryState::TypeMismatch));
    let same = report.count(|s| matches!(s, EntryState::Same));
    let not_compared = report.count(|s| matches!(s, EntryState::NotCompared(_)));
    let errors = report.count(|s| matches!(s, EntryState::Error(_))) + report.errors.len();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{} entries", report.entries.len())).strong());
        ui.label(format!("{left_only} left-only"));
        ui.label(format!("{right_only} right-only"));
        ui.label(format!("{different} different"));
        ui.label(format!("{same} same"));
        if not_compared > 0 {
            ui.label(format!("{not_compared} online-only, not compared"));
        }
        if errors > 0 {
            ui.colored_label(ui.visuals().error_fg_color, format!("{errors} errors"));
        }
    });
    if !report.errors.is_empty() {
        ui.collapsing(
            format!("{} traversal/hash issue(s)", report.errors.len()),
            |ui| {
                for issue in report.errors.iter().take(60) {
                    ui.label(format!("{} — {}", issue.path.display(), issue.reason));
                }
            },
        );
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Filter").weak());
        for (label, filter) in [
            ("All", CompareFilter::All),
            ("Left-only", CompareFilter::LeftOnly),
            ("Right-only", CompareFilter::RightOnly),
            ("Different", CompareFilter::Different),
            ("Same", CompareFilter::Same),
            ("Errors", CompareFilter::Errors),
        ] {
            if ui.selectable_label(state.filter == filter, label).clicked() {
                state.filter = filter;
            }
        }
    });
    let indices: Vec<usize> = report
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| compare_filter_matches(state.filter, &entry.state))
        .map(|(index, _)| index)
        .collect();
    egui::ScrollArea::vertical()
        .id_salt(("compare-entries", id))
        .max_height(280.0)
        .show_rows(ui, 24.0, indices.len(), |ui, range| {
            for row in range {
                let entry = &report.entries[indices[row]];
                ui.horizontal(|ui| {
                    let (label, color) = entry_state_label(&entry.state, ui);
                    ui.colored_label(color, label);
                    ui.label(entry.rel.display().to_string());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format!(
                            "{}  |  {}",
                            entry
                                .right
                                .map_or("—".to_string(), |side| format_size(side.size)),
                            entry
                                .left
                                .map_or("—".to_string(), |side| format_size(side.size))
                        ));
                    });
                });
            }
        });

    ui.separator();
    let mut build_preview = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Sync preview").strong());
        let old_mode = state.sync_mode;
        ui.radio_value(
            &mut state.sync_mode,
            SyncMode::Update,
            "Update left → right",
        );
        ui.radio_value(
            &mut state.sync_mode,
            SyncMode::Mirror,
            "Mirror left → right",
        );
        ui.radio_value(&mut state.sync_mode, SyncMode::TwoWay, "Two-way");
        if old_mode != state.sync_mode {
            state.plan = None;
            state.confirming_mirror = false;
        }
        build_preview = ui.button("Build preview").clicked();
    });
    for help in [
        "Update: copies new and left-newer files; never deletes or overwrites a newer right file.",
        "Mirror: makes right identical to left; right-only items go to the Recycle Bin after confirmation.",
        "Two-way: copies missing items both ways and the newer file over the older; same-time conflicts are skipped; never deletes.",
    ] {
        ui.label(RichText::new(help).weak());
    }
    // The preview is fingerprinted off the UI thread; `ToolPayload::SyncPlan` stores it.
    if build_preview && let Some(right) = &state.right {
        return Some(UiAction::Start(DeferredAction::BuildSyncPlan {
            report: report.clone(),
            left: state.left.clone(),
            right: right.clone(),
            mode: state.sync_mode,
        }));
    }
    if let Some(plan) = &state.plan {
        let copies_to_left = plan
            .actions
            .iter()
            .filter(|action| matches!(action, SyncAction::Copy { to_left: true, .. }))
            .count();
        let copies = plan
            .actions
            .iter()
            .filter(|action| matches!(action, SyncAction::Copy { .. }))
            .count()
            - copies_to_left;
        let dirs_to_left = plan
            .actions
            .iter()
            .filter(|action| matches!(action, SyncAction::CreateDir { to_left: true, .. }))
            .count();
        let dirs = plan
            .actions
            .iter()
            .filter(|action| matches!(action, SyncAction::CreateDir { .. }))
            .count()
            - dirs_to_left;
        let deletes = plan.deletes();
        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "→ right: {copies} file copy/update(s), {dirs} folder(s)"
            ));
            if plan.mode == SyncMode::TwoWay {
                ui.label(format!(
                    "← left: {copies_to_left} file copy/update(s), {dirs_to_left} folder(s)"
                ));
            }
            ui.label(format!("{} to copy", format_size(plan.bytes_to_copy)));
            if !plan.skipped.is_empty() {
                ui.label(format!("{} skipped", plan.skipped.len()));
            }
            if deletes > 0 {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("{deletes} destination item(s) to recycle"),
                );
            }
        });
        egui::ScrollArea::vertical()
            .id_salt(("sync-plan", id))
            .max_height(120.0)
            .show(ui, |ui| {
                for action in &plan.actions {
                    match action {
                        SyncAction::CreateDir { rel, to_left } => {
                            let (arrow, root) = sync_destination(plan, *to_left);
                            ui.label(format!(
                                "{arrow}  CREATE FOLDER  {}",
                                root.join(rel).display()
                            ))
                        }
                        SyncAction::Copy {
                            rel,
                            overwrite,
                            bytes,
                            to_left,
                        } => {
                            let (arrow, root) = sync_destination(plan, *to_left);
                            ui.label(format!(
                                "{arrow}  {}  {}  ({})",
                                if *overwrite { "UPDATE" } else { "COPY" },
                                root.join(rel).display(),
                                format_size(*bytes)
                            ))
                        }
                        SyncAction::Delete { rel, is_dir, .. } => ui.colored_label(
                            ui.visuals().error_fg_color,
                            format!(
                                "RECYCLE {}  {}",
                                if *is_dir { "FOLDER" } else { "FILE" },
                                plan.right.join(rel).display()
                            ),
                        ),
                    };
                }
                for skipped in &plan.skipped {
                    ui.label(
                        RichText::new(format!(
                            "SKIP  {} — {}",
                            skipped.path.display(),
                            skipped.reason
                        ))
                        .weak(),
                    );
                }
            });
        if deletes > 0 {
            if state.confirming_mirror {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.colored_label(ui.visuals().error_fg_color, RichText::new("Confirm Mirror deletions").strong());
                    ui.label(format!("{} destination item(s) will be moved to the Recycle Bin. Review the exact list below before continuing.", deletes));
                    egui::ScrollArea::vertical().max_height(130.0).show(ui, |ui| {
                        for action in &plan.actions {
                            if let SyncAction::Delete { rel, is_dir, .. } = action {
                                ui.label(format!("{}  {}", if *is_dir { "FOLDER" } else { "FILE" }, plan.right.join(rel).display()));
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Confirm Mirror and recycle listed items").color(Color32::WHITE).strong()).clicked() {
                            ui.ctx().data_mut(|data| data.insert_temp(egui::Id::new(("sync-confirm", id)), true));
                        }
                        if ui.button("Cancel").clicked() { state.confirming_mirror = false; }
                    });
                });
            } else if ui.button("Review Mirror deletions…").clicked() {
                state.confirming_mirror = true;
            }
        } else {
            let enabled = !plan.actions.is_empty();
            if ui
                .add_enabled(enabled, egui::Button::new("Execute sync plan"))
                .clicked()
            {
                return Some(UiAction::Start(DeferredAction::ExecuteSync(plan.clone())));
            }
        }
        let confirmed = ui
            .ctx()
            .data(|data| data.get_temp::<bool>(egui::Id::new(("sync-confirm", id))))
            .unwrap_or(false);
        if confirmed {
            ui.ctx()
                .data_mut(|data| data.remove::<bool>(egui::Id::new(("sync-confirm", id))));
            state.confirming_mirror = false;
            return Some(UiAction::Start(DeferredAction::ExecuteSync(plan.clone())));
        }
    }
    None
}

/// Arrow and destination root for an action that writes to the right or the left side.
fn sync_destination(plan: &SyncPlan, to_left: bool) -> (&'static str, &Path) {
    if to_left {
        ("← left", plan.left.as_path())
    } else {
        ("→ right", plan.right.as_path())
    }
}

fn compare_filter_matches(filter: CompareFilter, state: &EntryState) -> bool {
    match filter {
        CompareFilter::All => true,
        CompareFilter::LeftOnly => matches!(state, EntryState::LeftOnly),
        CompareFilter::RightOnly => matches!(state, EntryState::RightOnly),
        CompareFilter::Different => {
            matches!(state, EntryState::Different(_) | EntryState::TypeMismatch)
        }
        CompareFilter::Same => matches!(state, EntryState::Same),
        CompareFilter::Errors => {
            matches!(state, EntryState::Error(_) | EntryState::NotCompared(_))
        }
    }
}

fn entry_state_label(state: &EntryState, ui: &egui::Ui) -> (String, Color32) {
    match state {
        EntryState::LeftOnly => ("LEFT ONLY".into(), Color32::from_rgb(86, 165, 230)),
        EntryState::RightOnly => ("RIGHT ONLY".into(), Color32::from_rgb(223, 164, 75)),
        EntryState::Same => ("SAME".into(), Color32::from_rgb(87, 177, 112)),
        EntryState::Different(reason) => (
            format!("DIFFERENT · {reason:?}"),
            ui.visuals().error_fg_color,
        ),
        EntryState::TypeMismatch => ("TYPE MISMATCH".into(), ui.visuals().error_fg_color),
        EntryState::Error(message) => (format!("ERROR · {message}"), ui.visuals().error_fg_color),
        EntryState::NotCompared(message) => (
            format!("NOT COMPARED · {message}"),
            ui.visuals().weak_text_color(),
        ),
    }
}

// -----------------------------------------------------------------------------
// Backup check
// -----------------------------------------------------------------------------

fn render_verify(id: u64, state: &mut VerifyState, ui: &mut egui::Ui) -> Option<UiAction> {
    let mut picked_original = None;
    let mut picked_backup = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Original").strong());
        ui.label(state.original.display().to_string());
        if ui.button("Choose original folder…").clicked() {
            picked_original = crate::dialogs::pick_folder(rfd::FileDialog::new());
        }
        ui.separator();
        ui.label(RichText::new("Backup").strong());
        if let Some(backup) = &state.backup {
            ui.label(backup.display().to_string());
        } else {
            ui.label(RichText::new("Choose a folder").weak());
        }
        if ui.button("Choose backup folder…").clicked() {
            picked_backup = crate::dialogs::pick_folder(rfd::FileDialog::new());
        }
    });
    if let Some(path) = picked_original {
        state.original = path;
        state.report = None;
    }
    if let Some(path) = picked_backup {
        state.backup = Some(path);
        state.report = None;
    }

    ui.checkbox(&mut state.full, "Full verify (hash every file)");
    ui.label(
        RichText::new(
            "Quick (unchecked): sizes and dates are compared first, and a file is hashed only when its date differs.",
        )
        .weak(),
    );
    ui.checkbox(
        &mut state.detect_moves,
        "Find moved or renamed files by content",
    );
    let start = ui
        .horizontal(|ui| {
            let verify = ui.add_enabled(state.backup.is_some(), egui::Button::new("Verify"));
            register_click_target(ui.ctx(), "verify:Verify", &verify);
            verify.clicked()
        })
        .inner;
    if start && let Some(backup) = state.backup.clone() {
        state.report = None;
        state.last_export = None;
        state.export_error = None;
        return Some(UiAction::Start(DeferredAction::VerifyBackup {
            original: state.original.clone(),
            backup,
            options: verify_options(state),
        }));
    }
    if let Some(path) = &state.last_export {
        ui.label(RichText::new(format!("Report saved to {}", path.display())).weak());
    }
    if let Some(error) = &state.export_error {
        ui.colored_label(ui.visuals().error_fg_color, error.as_str());
    }

    let Some(report) = state.report.as_ref() else {
        ui.add_space(8.0);
        ui.label(RichText::new("Results appear here. The check runs as a cancellable job.").weak());
        return None;
    };
    ui.separator();
    let error_color = ui.visuals().error_fg_color;
    let counts = report.counts();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{} matched", counts.matched)).strong());
        for (count, text, problem) in [
            (counts.missing, "missing from the backup", true),
            (counts.differs, "differ", true),
            (counts.extra, "only in the backup", false),
            (counts.moved, "moved or renamed", false),
            (counts.not_checked, "not checked", false),
            (counts.errors, "errors", true),
        ] {
            let text = format!("{count} {text}");
            if problem && count > 0 {
                ui.colored_label(error_color, text);
            } else {
                ui.label(text);
            }
        }
    });
    if report.is_ok() {
        ui.label(
            RichText::new("Backup verified: no problems found.")
                .strong()
                .color(Color32::from_rgb(76, 185, 112)),
        );
    } else {
        ui.label(RichText::new("Problems found").strong().color(error_color));
    }

    let mut export: Option<(PathBuf, bool)> = None;
    ui.horizontal(|ui| {
        if ui.button("Export CSV…").clicked()
            && let Some(path) = crate::dialogs::save_file(
                rfd::FileDialog::new()
                    .set_file_name("backup-check.csv")
                    .add_filter("CSV", &["csv"]),
            )
        {
            export = Some((path, true));
        }
        if ui.button("Export text…").clicked()
            && let Some(path) = crate::dialogs::save_file(
                rfd::FileDialog::new()
                    .set_file_name("backup-check.txt")
                    .add_filter("Text", &["txt"]),
            )
        {
            export = Some((path, false));
        }
    });

    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Show").weak());
        for (label, filter) in VERIFY_FILTERS {
            if ui.selectable_label(state.filter == filter, label).clicked() {
                state.filter = filter;
            }
        }
    });
    let filter = state.filter;
    let indices: Vec<usize> = report
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| verify_filter_matches(filter, &entry.status))
        .map(|(index, _)| index)
        .collect();
    egui::ScrollArea::vertical()
        .id_salt(("verify-entries", id))
        .max_height(340.0)
        .show_rows(ui, 24.0, indices.len(), |ui, range| {
            for row in range {
                let entry = &report.entries[indices[row]];
                ui.horizontal(|ui| {
                    ui.colored_label(verify_status_color(&entry.status, ui), entry.status.label());
                    ui.label(forge_core::paths::to_slash(&entry.rel));
                    match &entry.status {
                        VerifyStatus::Moved { backup_rel } => {
                            ui.label(
                                RichText::new(format!(
                                    "→ {}",
                                    forge_core::paths::to_slash(backup_rel)
                                ))
                                .weak(),
                            );
                        }
                        VerifyStatus::NotChecked(reason) | VerifyStatus::Error(reason) => {
                            ui.label(RichText::new(reason).weak());
                        }
                        _ => {}
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format!(
                            "{}  |  {}",
                            size_label(entry.original_size),
                            size_label(entry.backup_size)
                        ));
                    });
                });
            }
        });

    // Rendering the text is pure computation; the write to the chosen path (which may be on a
    // network share) runs on a job and reports back through `ToolPayload::Exported`.
    export.map(|(path, csv)| {
        let text = if csv {
            report.to_csv()
        } else {
            report.to_text()
        };
        UiAction::Start(DeferredAction::ExportVerify { path, text })
    })
}

/// Does `filter` show an entry with `status`?
fn verify_filter_matches(filter: VerifyFilter, status: &VerifyStatus) -> bool {
    match filter {
        VerifyFilter::Problems => !matches!(status, VerifyStatus::Match),
        VerifyFilter::All => true,
        VerifyFilter::Missing => matches!(status, VerifyStatus::Missing),
        VerifyFilter::Differs => matches!(status, VerifyStatus::Differs),
        VerifyFilter::Extra => matches!(status, VerifyStatus::Extra),
        VerifyFilter::Moved => matches!(status, VerifyStatus::Moved { .. }),
        VerifyFilter::NotChecked => matches!(status, VerifyStatus::NotChecked(_)),
        VerifyFilter::Errors => matches!(status, VerifyStatus::Error(_)),
        VerifyFilter::Matches => matches!(status, VerifyStatus::Match),
    }
}

fn verify_status_color(status: &VerifyStatus, ui: &egui::Ui) -> Color32 {
    match status {
        VerifyStatus::Match => Color32::from_rgb(87, 177, 112),
        VerifyStatus::Missing | VerifyStatus::Differs | VerifyStatus::Error(_) => {
            ui.visuals().error_fg_color
        }
        VerifyStatus::Extra => Color32::from_rgb(223, 164, 75),
        VerifyStatus::Moved { .. } => Color32::from_rgb(86, 165, 230),
        VerifyStatus::NotChecked(_) => theme::muted(ui),
    }
}

fn size_label(size: Option<u64>) -> String {
    size.map_or_else(|| "—".to_string(), format_size)
}

/// Save the report as CSV or text. Failures are kept in the window for display. Synchronous,
/// so automation only: the window's buttons write the file on a job instead.
fn export_verify_report(state: &mut VerifyState, path: &Path, csv: bool) -> Result<(), String> {
    let report = state
        .report
        .as_ref()
        .ok_or("the backup check has not completed")?;
    let text = if csv {
        report.to_csv()
    } else {
        report.to_text()
    };
    let result = write_report_for_export(path, &text);
    apply_export_result(state, result.clone());
    result.map(|_| ())
}

/// Record the outcome of a report export in the window state.
fn apply_export_result(state: &mut VerifyState, result: Result<PathBuf, String>) {
    match result {
        Ok(path) => {
            state.last_export = Some(path);
            state.export_error = None;
        }
        Err(message) => {
            state.last_export = None;
            state.export_error = Some(message);
        }
    }
}

/// Write a rendered report and turn any failure into the message the window shows.
fn write_report_for_export(path: &Path, text: &str) -> Result<PathBuf, String> {
    write_report_file(path, text)
        .map(|()| path.to_path_buf())
        .map_err(|error| format!("Could not save the report to {}: {error}", path.display()))
}

/// Write `text` to `path` through a temporary file in the same folder, so a failed export
/// never leaves a partial report under the chosen name.
fn write_report_file(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;

    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "report path has no file name",
        )
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(
        ".{}.zxraw-tmp-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_summary_counts_warnings() {
        let mut report = OpReport {
            done: vec![PathBuf::from("a.jpg")],
            warnings: vec![forge_core::ops::OpIssue::new("a.jpg", "kept as a.old.jpg")],
            ..Default::default()
        };
        assert_eq!(
            sync_summary(&report),
            format!("Sync complete: 1 item(s), {}, 1 warning", format_size(0))
        );
        report.failed.push(forge_core::ops::OpIssue::new("b.jpg", "denied"));
        report.warnings.push(forge_core::ops::OpIssue::new("c.jpg", "kept"));
        assert_eq!(sync_summary(&report), "Sync finished with 1 error(s), 2 warnings");
    }

    #[test]
    fn verify_filter_chips_show_their_statuses() {
        let all = [
            VerifyStatus::Match,
            VerifyStatus::Missing,
            VerifyStatus::Differs,
            VerifyStatus::Extra,
            VerifyStatus::Moved {
                backup_rel: PathBuf::from("b.bin"),
            },
            VerifyStatus::NotChecked("link".into()),
            VerifyStatus::Error("denied".into()),
        ];
        let shown = |filter| {
            all.iter()
                .filter(|status| verify_filter_matches(filter, status))
                .count()
        };
        assert_eq!(shown(VerifyFilter::Problems), 6);
        assert_eq!(shown(VerifyFilter::All), 7);
        assert_eq!(shown(VerifyFilter::Matches), 1);
        for (filter, index) in [
            (VerifyFilter::Missing, 1),
            (VerifyFilter::Differs, 2),
            (VerifyFilter::Extra, 3),
            (VerifyFilter::Moved, 4),
            (VerifyFilter::NotChecked, 5),
            (VerifyFilter::Errors, 6),
        ] {
            let hits: Vec<usize> = all
                .iter()
                .enumerate()
                .filter(|(_, status)| verify_filter_matches(filter, status))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(hits, vec![index], "{filter:?}");
        }
        assert_eq!(VERIFY_FILTERS[0].1, VerifyFilter::Problems);
    }

    #[test]
    fn report_file_is_written_whole_and_never_partial() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("backup-check.csv");
        write_report_file(&target, "status,path\r\nmatch,a\r\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "status,path\r\nmatch,a\r\n"
        );
        write_report_file(&target, "replaced").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "replaced");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        let missing_folder = dir.path().join("no-such-folder").join("report.txt");
        assert!(write_report_file(&missing_folder, "text").is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn export_result_updates_window_status_on_success_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = VerifyState {
            original: dir.path().to_path_buf(),
            backup: None,
            full: false,
            detect_moves: false,
            report: None,
            filter: VerifyFilter::Problems,
            last_export: None,
            export_error: Some("old error".into()),
        };
        let target = dir.path().join("backup-check.txt");
        let saved = write_report_for_export(&target, "ok").unwrap();
        assert_eq!(saved, target);
        apply_export_result(&mut state, Ok(saved.clone()));
        assert_eq!(state.last_export.as_deref(), Some(target.as_path()));
        assert!(state.export_error.is_none());

        let missing = dir.path().join("no-such-folder").join("report.csv");
        let message = write_report_for_export(&missing, "x").unwrap_err();
        assert!(message.starts_with("Could not save the report to "), "{message}");
        apply_export_result(&mut state, Err(message.clone()));
        assert!(state.last_export.is_none());
        assert_eq!(state.export_error.as_deref(), Some(message.as_str()));
    }

    #[test]
    fn squarified_treemap_tiles_area_without_overlap_and_preserves_proportions() {
        let area = Rect::from_min_size(pos2(13.0, 27.0), vec2(100.0, 60.0));
        let values = [6, 3, 1, 0];
        let rects = squarified_treemap(area, &values);
        assert_eq!(rects.len(), values.len());
        let occupied: f32 = rects
            .iter()
            .filter(|r| r.width() > 0.0 && r.height() > 0.0)
            .map(Rect::area)
            .sum();
        assert!(
            (occupied - area.area()).abs() < 0.01,
            "occupied={occupied}, expected={}",
            area.area()
        );
        for (index, rect) in rects.iter().enumerate() {
            assert!(rect.min.x >= area.min.x - 0.001 && rect.min.y >= area.min.y - 0.001);
            assert!(rect.max.x <= area.max.x + 0.001 && rect.max.y <= area.max.y + 0.001);
            if values[index] > 0 {
                let expected = area.area() * values[index] as f32 / 10.0;
                assert!(
                    (rect.area() - expected).abs() < 0.01,
                    "area {} expected {expected}",
                    rect.area()
                );
            }
        }
        for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                let overlap = rects[i].intersect(rects[j]);
                assert!(
                    overlap.width() <= 0.001 || overlap.height() <= 0.001,
                    "overlap: {overlap:?}"
                );
            }
        }
    }

    fn duplicate_group(paths: &[&str]) -> DuplicateGroup {
        DuplicateGroup {
            size: 10,
            hash: None,
            files: paths.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn duplicate_keep_helpers_never_tick_every_copy() {
        let group = duplicate_group(&[
            "C:/short.bin",
            "C:/a/much-longer/path/copy.bin",
            "C:/other.bin",
        ]);
        let mut modified = HashMap::new();
        modified.insert(
            group.files[0].clone(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        );
        modified.insert(
            group.files[1].clone(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(3),
        );
        modified.insert(
            group.files[2].clone(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(2),
        );
        for rule in [KeepRule::Newest, KeepRule::Oldest, KeepRule::ShortestPath] {
            let ticked = copies_to_tick(&group, rule, &modified);
            assert_eq!(ticked.len(), group.files.len() - 1);
            assert!(selection_is_safe(
                std::slice::from_ref(&group),
                &ticked.iter().map(|i| (0, *i)).collect()
            ));
        }
        assert!(
            !copies_to_tick(
                &duplicate_group(&["one", "two"]),
                KeepRule::Newest,
                &HashMap::new()
            )
            .is_empty()
        );
    }

    #[test]
    fn recycle_reverifies_against_disk() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.bin");
        let b = d.path().join("b.bin");
        let c = d.path().join("c.bin");
        for p in [&a, &b, &c] {
            std::fs::write(p, b"same content").unwrap();
        }
        let hash =
            forge_core::hashing::hash_file(&a, &forge_jobs::CancelToken::new(), None).unwrap();
        let check = RecycleCheck {
            size: 12,
            hash: Some(hash),
            keep: vec![a.clone()],
            remove: vec![b.clone(), c.clone()],
        };
        let cancel = forge_jobs::CancelToken::new();

        // Normal case: both removal candidates verified.
        let (ok, skipped) = verify_recycle(std::slice::from_ref(&check), &cancel);
        assert_eq!(ok, vec![b.clone(), c.clone()]);
        assert!(skipped.is_empty());

        // A removal candidate edited after the scan (same size!) is no longer a duplicate.
        std::fs::write(&c, b"SAME CONTENT").unwrap();
        let (ok, skipped) = verify_recycle(std::slice::from_ref(&check), &cancel);
        assert_eq!(ok, vec![b.clone()]);
        assert_eq!(skipped.len(), 1);

        // A kept copy that became online-only cannot be verified without downloading it.
        forge_core::cloud::simulate_online_only(&a);
        let (ok, skipped) = verify_recycle(std::slice::from_ref(&check), &cancel);
        forge_core::cloud::clear_simulated_online_only(&a);
        assert!(ok.is_empty());
        assert_eq!(skipped.len(), 1);

        // The kept copy deleted in Explorer: nothing in the group may be removed.
        std::fs::remove_file(&a).unwrap();
        let (ok, skipped) = verify_recycle(std::slice::from_ref(&check), &cancel);
        assert!(ok.is_empty());
        assert_eq!(skipped.len(), 1);
    }

    #[test]
    fn duplicate_selection_safety_refuses_to_remove_the_last_copy() {
        let groups = vec![
            duplicate_group(&["a", "b"]),
            duplicate_group(&["c", "d", "e"]),
        ];
        let safe = BTreeSet::from([(0, 0), (1, 0), (1, 2)]);
        assert!(selection_is_safe(&groups, &safe));
        let (targets, bytes) = selected_duplicate_targets(&groups, &safe).unwrap();
        assert_eq!(targets.len(), 3);
        assert_eq!(bytes, 30);

        let unsafe_all = BTreeSet::from([(0, 0), (0, 1), (1, 0), (1, 1), (1, 2)]);
        assert!(!selection_is_safe(&groups, &unsafe_all));
        assert!(selected_duplicate_targets(&groups, &unsafe_all).is_none());
    }

    fn files_prompt(totals: OnlineOnlyTotals) -> OnlineOnlyPrompt {
        OnlineOnlyPrompt {
            work: ChecksumWork::Build {
                root: PathBuf::from(r"C:\shoot"),
                selection: Vec::new(),
            },
            totals,
        }
    }

    #[test]
    fn online_only_prompt_counts_read_naturally() {
        let one = OnlineOnlyTotals {
            count: 1,
            bytes: 512,
            ..Default::default()
        };
        assert_eq!(
            online_only_prompt_text(&files_prompt(one)),
            format!(
                "1 file is online-only and would be downloaded ({}).",
                format_size(512)
            )
        );
        let many = OnlineOnlyTotals {
            count: 3,
            bytes: 3 << 30,
            ..Default::default()
        };
        assert_eq!(
            online_only_prompt_text(&files_prompt(many)),
            format!(
                "3 files are online-only and would be downloaded ({}).",
                format_size(3 << 30)
            )
        );
    }

    #[test]
    fn an_online_only_manifest_needs_consent_and_skip_stops() {
        let manifest = PathBuf::from(r"C:\shoot\checksums.b3");
        let totals = OnlineOnlyTotals {
            count: 1,
            bytes: 100,
            manifest_online_only: true,
        };
        let prompt = || OnlineOnlyPrompt {
            work: ChecksumWork::Verify {
                manifest: manifest.clone(),
                manifest_allowed: false,
            },
            totals,
        };
        assert_eq!(
            online_only_prompt_text(&prompt()),
            format!(
                "The manifest checksums.b3 is online-only and would be downloaded ({}) before \
                 its files can be checked.",
                format_size(100)
            )
        );
        // Download reads the manifest, then counts its files (which may ask again).
        assert!(matches!(
            answer_online_only_prompt(prompt(), OnlineOnlyPolicy::Download),
            UiAction::Start(DeferredAction::ChecksumCount(ChecksumWork::Verify {
                manifest_allowed: true,
                ..
            }))
        ));
        // Skip starts nothing: the manifest is never read.
        assert!(matches!(
            answer_online_only_prompt(prompt(), OnlineOnlyPolicy::Skip),
            UiAction::Notice(n) if n.starts_with("Verification stopped")
        ));
        // For files, the answer runs the verification with that policy.
        let files = OnlineOnlyPrompt {
            totals: OnlineOnlyTotals {
                count: 2,
                bytes: 10,
                manifest_online_only: false,
            },
            ..prompt()
        };
        assert!(matches!(
            answer_online_only_prompt(files, OnlineOnlyPolicy::Skip),
            UiAction::Start(DeferredAction::Checksum {
                policy: OnlineOnlyPolicy::Skip,
                ..
            })
        ));
    }

    #[test]
    fn selection_roots_are_covered_by_outer_folders() {
        let folder = PathBuf::from(r"C:\photos\2026");
        let inner = folder.join("raw");
        let file = PathBuf::from(r"C:\photos\cover.jpg");
        let roots = selection_roots(&[inner.clone(), file.clone(), folder.clone(), inner]);
        assert_eq!(roots, vec![folder, file]);
    }

    #[test]
    fn verify_summary_reports_skipped_files() {
        let mut report = VerifyReport {
            ok: vec!["a.txt".into()],
            ..Default::default()
        };
        assert_eq!(verify_summary(&report), "Verification complete: 1 file(s) OK");
        report.skipped_online_only = vec!["b.mov".into(), "c.mov".into()];
        assert_eq!(
            verify_summary(&report),
            "Verification complete: 1 file(s) OK, 2 skipped (online-only)"
        );
        report.missing = vec!["d.txt".into()];
        assert_eq!(
            verify_summary(&report),
            "Verification complete: 0 mismatch, 1 missing, 0 unreadable, 2 skipped (online-only)"
        );
    }
}
