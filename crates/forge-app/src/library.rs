//! The persistent library: SQLite database (opened off the UI thread), per-folder
//! annotations (ratings/labels/picks), indexing + metadata extraction jobs, and search.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use forge_core::query::{ColorLabel, PickState};
use forge_db::{AnnotationChange, Db, FileRecord, IndexRoot, IndexState, SearchOptions};
use forge_jobs::{JobHandle, JobRegistry};
use forge_media::{SidecarOutcome, SidecarUpdate, update_sidecar};

use crate::app::ZxrawApp;
use crate::views::Annotation;

pub enum LibMsg {
    Opened(Result<Arc<Db>, String>, Option<String>),
    DirAnnotations(PathBuf, HashMap<PathBuf, Annotation>),
    AuthoritativeAnnotations {
        versions: HashMap<PathBuf, u64>,
        annotations: HashMap<PathBuf, Annotation>,
    },
    SearchResults {
        query: String,
        result: Result<Vec<FileRecord>, String>,
    },
    Error(String),
}

struct AnnotationWrite {
    paths: Vec<PathBuf>,
    change: AnnotationChange,
    versions: HashMap<PathBuf, u64>,
    /// Whether to mirror the change into XMP sidecars once the database write succeeds.
    xmp_sync: bool,
    /// Each file's entry as read when the change was requested. The writer thread may run
    /// after the file was renamed, so it must not re-read the path itself.
    entries: HashMap<PathBuf, forge_core::FileEntry>,
    /// Told whether the database write succeeded (sidecar sync is reported separately).
    ack: Option<Sender<Result<(), String>>>,
}

enum LibraryWrite {
    Annotation(AnnotationWrite),
    RenameTag {
        old: String,
        new: String,
        reply: Sender<Result<(), String>>,
    },
    DeleteTag {
        name: String,
        reply: Sender<Result<bool, String>>,
    },
    /// Files or folders ZXraw moved or renamed: their rows (notes, ratings, tags) follow.
    Relink {
        pairs: Vec<(PathBuf, PathBuf)>,
    },
    /// Answered once every write queued before it has been applied.
    Flush(Sender<()>),
}

pub struct Library {
    pub db: Option<Arc<Db>>,
    pub open_error: Option<String>,
    tx: Sender<LibMsg>,
    rx: Receiver<LibMsg>,
    ctx: egui::Context,
    /// Annotations of the folders currently shown, keyed by full path.
    pub annotations: HashMap<PathBuf, Annotation>,
    loaded_dirs: HashSet<PathBuf>,
    /// Paths edited locally are protected from any async directory snapshot. They remain
    /// protected for this process lifetime; failed writes are reconciled from the DB below.
    locally_edited: HashSet<PathBuf>,
    annotation_versions: HashMap<PathBuf, u64>,
    next_annotation_version: u64,
    /// Writes made before the database finished opening; flushed once it is ready, in order.
    queued: Vec<LibraryWrite>,
    /// Single writer thread so edits reach the database in the order they were made.
    writer: Option<Sender<LibraryWrite>>,
    /// Mirror annotation changes into `.xmp` sidecars (set from the settings every frame).
    xmp_sync: bool,
    pub search: SearchState,
    /// Set once interrupted index passes have been looked at for this session.
    pub resume_checked: bool,
}

#[derive(Default)]
pub struct SearchState {
    pub open: bool,
    pub query: String,
    pub running: bool,
    pub last_query: String,
    pub results: Vec<FileRecord>,
    pub error: Option<String>,
    pub focus: bool,
}

/// Up to this many files, [`Library::annotate`] reads their entries on the calling thread.
const UI_THREAD_ENTRY_LIMIT: usize = 32;

pub fn to_view(rating: u8, label: Option<ColorLabel>, pick: PickState) -> Annotation {
    Annotation {
        rating,
        label: label
            .and_then(|l| ColorLabel::ALL.iter().position(|x| *x == l))
            .map(|i| i as u8),
        pick: match pick {
            PickState::Pick => 1,
            PickState::Reject => -1,
            PickState::None => 0,
        },
        // Callers that know the notes set this after the call.
        has_note: false,
    }
}

impl Library {
    pub fn open(path: PathBuf, ctx: egui::Context) -> Self {
        let (tx, rx) = unbounded();
        let t = tx.clone();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let r = Db::open(&path);
            let msg = match r {
                Ok(db) => {
                    let recovered = db
                        .recovered_corrupt_file()
                        .map(|p: &Path| format!("The library database was damaged and has been reset. The old file was kept as {}", p.display()));
                    LibMsg::Opened(Ok(Arc::new(db)), recovered)
                }
                Err(e) => LibMsg::Opened(Err(e.to_string()), None),
            };
            let _ = t.send(msg);
            c.request_repaint();
        });
        Self {
            db: None,
            open_error: None,
            tx,
            rx,
            ctx,
            annotations: HashMap::new(),
            loaded_dirs: HashSet::new(),
            locally_edited: HashSet::new(),
            annotation_versions: HashMap::new(),
            next_annotation_version: 0,
            queued: Vec::new(),
            writer: None,
            xmp_sync: false,
            search: SearchState::default(),
            resume_checked: false,
        }
    }

    /// Drain messages; returns user-facing notices (toasts).
    pub fn poll(&mut self) -> Vec<Result<String, String>> {
        let mut notices = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            match m {
                LibMsg::Opened(Ok(db), recovered) => {
                    self.writer = Some(spawn_writer(db.clone(), self.tx.clone()));
                    self.db = Some(db);
                    for write in std::mem::take(&mut self.queued) {
                        self.persist(write);
                    }
                    if let Some(r) = recovered {
                        notices.push(Err(r));
                    }
                    // Reload annotations for folders requested before the DB was ready.
                    let dirs: Vec<PathBuf> = self.loaded_dirs.drain().collect();
                    for d in dirs {
                        self.ensure_dir(&d);
                    }
                }
                LibMsg::Opened(Err(e), _) => {
                    if !self.queued.is_empty() {
                        notices.push(Err(format!(
                            "{} rating/label change(s) could not be saved",
                            self.queued.len()
                        )));
                        self.queued.clear();
                    }
                    notices.push(Err(format!("Library database unavailable: {e}")));
                    self.open_error = Some(e);
                }
                LibMsg::DirAnnotations(_dir, map) => {
                    merge_loaded_annotations(&mut self.annotations, &self.locally_edited, map);
                }
                LibMsg::AuthoritativeAnnotations {
                    versions,
                    annotations,
                } => {
                    merge_authoritative_annotations(
                        &mut self.annotations,
                        &self.annotation_versions,
                        versions,
                        annotations,
                    );
                }
                LibMsg::SearchResults { query, result } => {
                    if query == self.search.last_query {
                        self.search.running = false;
                        match result {
                            Ok(r) => {
                                self.search.results = r;
                                self.search.error = None;
                            }
                            Err(e) => {
                                self.search.results.clear();
                                self.search.error = Some(e);
                            }
                        }
                    }
                }
                LibMsg::Error(e) => notices.push(Err(e)),
            }
        }
        notices
    }

    /// Enables or disables XMP sidecar sync for annotation changes made from now on.
    pub fn set_xmp_sync(&mut self, enabled: bool) {
        self.xmp_sync = enabled;
    }

    /// Load annotations for `dir` once (async).
    pub fn ensure_dir(&mut self, dir: &Path) {
        if !self.loaded_dirs.insert(dir.to_path_buf()) {
            return;
        }
        let Some(db) = self.db.clone() else { return };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let dir = dir.to_path_buf();
        std::thread::spawn(move || {
            if let Ok(rows) = db.children(&dir, false) {
                let map = rows
                    .into_iter()
                    .filter(|r| {
                        r.rating > 0 || r.label.is_some() || r.pick != PickState::None || r.has_notes
                    })
                    .map(|r| {
                        let mut view = to_view(r.rating, r.label, r.pick);
                        view.has_note = r.has_notes;
                        (r.path.clone(), view)
                    })
                    .collect();
                let _ = tx.send(LibMsg::DirAnnotations(dir, map));
                ctx.request_repaint();
            }
        });
    }

    /// Apply an annotation change to files (optimistically updates the local cache, then
    /// persists in the background). Files are added to the database if not indexed yet.
    pub fn annotate(&mut self, paths: Vec<PathBuf>, change: AnnotationChange) {
        self.annotate_with(paths, change, None);
    }

    /// [`Library::annotate`], and the returned channel answers once the database write has
    /// succeeded or failed. It disconnects without an answer when the write is never made
    /// (the library database is unavailable).
    pub fn annotate_confirmed(
        &mut self,
        paths: Vec<PathBuf>,
        change: AnnotationChange,
    ) -> Receiver<Result<(), String>> {
        let (ack, result) = bounded(1);
        self.annotate_with(paths, change, Some(ack));
        result
    }

    /// Waits up to `timeout` until every write queued so far has reached the database (used
    /// at exit). Returns false on timeout or when there is no writer.
    pub fn flush(&self, timeout: std::time::Duration) -> bool {
        let Some(writer) = &self.writer else {
            return false;
        };
        let (done, wait) = bounded(1);
        writer.send(LibraryWrite::Flush(done)).is_ok() && wait.recv_timeout(timeout).is_ok()
    }

    fn annotate_with(
        &mut self,
        paths: Vec<PathBuf>,
        change: AnnotationChange,
        ack: Option<Sender<Result<(), String>>>,
    ) {
        self.next_annotation_version = self.next_annotation_version.wrapping_add(1).max(1);
        let version = self.next_annotation_version;
        let mut versions = HashMap::new();
        for p in &paths {
            self.locally_edited.insert(p.clone());
            self.annotation_versions.insert(p.clone(), version);
            versions.insert(p.clone(), version);
            let a = self.annotations.entry(p.clone()).or_default();
            if let Some(r) = change.rating {
                a.rating = r;
            }
            if let Some(l) = change.label {
                *a = Annotation {
                    label: l
                        .and_then(|l| ColorLabel::ALL.iter().position(|x| *x == l))
                        .map(|i| i as u8),
                    ..*a
                };
            }
            if let Some(pk) = change.pick {
                a.pick = to_view(0, None, pk).pick;
            }
            if let Some(notes) = &change.notes {
                a.has_note = has_note(notes.as_deref());
            }
        }
        // Read the entries now, on this thread: a rename that follows this change is queued
        // behind it, so the writer would otherwise find the old paths already gone. Only small
        // edits (a note, a few files) are read here; large batches would stall the frame.
        let entries = paths
            .iter()
            .take(if paths.len() <= UI_THREAD_ENTRY_LIMIT { paths.len() } else { 0 })
            .filter_map(|p| {
                forge_core::listing::entry_for_path(p)
                    .ok()
                    .map(|e| (p.clone(), e))
            })
            .collect();
        let write = AnnotationWrite {
            paths,
            change,
            versions,
            xmp_sync: self.xmp_sync,
            entries,
            ack,
        };
        if self.db.is_none() {
            if self.open_error.is_some() {
                let _ = self.tx.send(LibMsg::Error(
                    "Library database is not available; change not saved".into(),
                ));
            } else {
                self.queued.push(LibraryWrite::Annotation(write));
            }
            return;
        }
        self.persist(LibraryWrite::Annotation(write));
    }

    /// Tell the library that ZXraw moved or renamed files (`old` -> `new`, a file or a folder).
    /// The in-memory annotations are re-keyed at once, and the database rows follow through
    /// the same ordered writer as annotation edits, so a note or rating made before the move
    /// stays on the file. Pairs where old == new are ignored.
    pub fn follow_moves(&mut self, pairs: Vec<(PathBuf, PathBuf)>) {
        let pairs: Vec<(PathBuf, PathBuf)> =
            pairs.into_iter().filter(|(old, new)| old != new).collect();
        if pairs.is_empty() {
            return;
        }
        rekey_map(&mut self.annotations, &pairs);
        rekey_set(&mut self.locally_edited, &pairs);
        rekey_map(&mut self.annotation_versions, &pairs);
        let write = LibraryWrite::Relink { pairs };
        if self.db.is_some() {
            self.persist(write);
        } else if self.open_error.is_none() {
            self.queued.push(write);
        }
    }

    fn persist(&self, write: LibraryWrite) {
        if let Some(w) = &self.writer {
            let _ = w.send(write);
        }
    }

    /// Rename a tag in the same ordered queue as annotation edits.
    pub fn rename_tag(&self, old: String, new: String) -> Receiver<Result<(), String>> {
        let (reply, result) = bounded(1);
        let error = "Library database is not available; tag change was not saved".to_string();
        let Some(writer) = &self.writer else {
            let _ = reply.send(Err(error));
            return result;
        };
        if writer
            .send(LibraryWrite::RenameTag {
                old,
                new,
                reply: reply.clone(),
            })
            .is_err()
        {
            let _ = reply.send(Err(error));
        }
        result
    }

    /// Delete a tag in the same ordered queue as annotation edits.
    pub fn delete_tag(&self, name: String) -> Receiver<Result<bool, String>> {
        let (reply, result) = bounded(1);
        let error = "Library database is not available; tag change was not saved".to_string();
        let Some(writer) = &self.writer else {
            let _ = reply.send(Err(error));
            return result;
        };
        if writer
            .send(LibraryWrite::DeleteTag {
                name,
                reply: reply.clone(),
            })
            .is_err()
        {
            let _ = reply.send(Err(error));
        }
        result
    }

    pub fn run_search(&mut self) {
        let Some(db) = self.db.clone() else {
            self.search.error = Some("Library database is not available".into());
            return;
        };
        let q = self.search.query.trim().to_string();
        if q.is_empty() {
            return;
        }
        self.search.last_query = q.clone();
        self.search.running = true;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let opts = SearchOptions {
                limit: Some(5000),
                ..Default::default()
            };
            let result = db.search_str(&q, &opts).map_err(|e| e.to_string());
            let _ = tx.send(LibMsg::SearchResults { query: q, result });
            ctx.request_repaint();
        });
    }
}

/// Index `root` (adding it as an index root) and then extract media metadata, as one
/// cancellable job. Safe to re-run: the indexer resumes from its checkpoint.
pub fn start_index_job(
    jobs: &JobRegistry,
    db: Arc<Db>,
    root: PathBuf,
    notify: impl Fn(String) + Send + 'static,
) -> JobHandle {
    let title = format!("Indexing {}", root.display());
    jobs.spawn("index", title, move |job| {
        let root_id = db.add_index_root(&root).map_err(|e| e.to_string())?;
        job.progress.set_current("Scanning folders…");
        let mut indexer = forge_db::Indexer::new(&db)
            .cancel_token(job.cancel.clone())
            .progress(job.progress.clone())
            .volume_resolver(volume_info);
        let report = indexer.index_root(root_id).map_err(|e| e.to_string())?;
        job.log(format!("{report:?}"));
        if job.cancel.is_cancelled() {
            return Ok(());
        }
        extract_metadata(&db, job);
        if report.resumed {
            notify(format!("Resumed and finished indexing {}", root.display()));
        } else {
            notify(format!("Indexed {}", root.display()));
        }
        Ok(())
    })
}

/// Volume identity for indexing: the root plus the volume serial, so a drive keeps one row
/// even when its letter changes. Shared by index jobs and relink reconciles.
pub fn volume_info(path: &Path) -> Option<forge_db::VolumeInfo> {
    let mut v = forge_db::VolumeInfo::from_path_root(path);
    v.serial = forge_win::volume_serial(Path::new(&v.root))
        .ok()
        .map(|s| format!("{s:08X}"));
    Some(v)
}

/// What startup does with an index pass that an earlier run left unfinished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumePlan {
    /// Continue the pass from its checkpoint.
    Resume(PathBuf),
    /// The folder is gone, so the pass cannot continue: forget its checkpoint.
    Drop(PathBuf),
}

/// Decides how to pick up one index root at startup. `None` means nothing is pending
/// (disabled root, no pass, or a pass that already completed).
pub fn plan_resume(
    root: &IndexRoot,
    state: Option<&IndexState>,
    folder_reachable: bool,
) -> Option<ResumePlan> {
    if !root.enabled || !state.is_some_and(IndexState::is_resumable) {
        return None;
    }
    Some(if folder_reachable {
        ResumePlan::Resume(root.path.clone())
    } else {
        ResumePlan::Drop(root.path.clone())
    })
}

/// Restarts every interrupted index pass in the background, as its own job, so startup
/// never waits for it. Runs once per session as soon as the library is open. The DB reads
/// and the reachability check (an offline network root can block for seconds) run on a
/// worker thread, never the UI thread.
pub fn resume_on_start(app: &mut ZxrawApp, ctx: &egui::Context) {
    let library = &mut app.ops.library;
    if library.resume_checked {
        return;
    }
    let Some(db) = library.db.clone() else {
        return;
    };
    library.resume_checked = true;
    let jobs = app.ops.jobs.clone();
    let tx = app.ops.tx.clone();
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("index-resume".into())
        .spawn(move || resume_pending(&db, &jobs, &tx, &ctx));
    if let Err(e) = spawned {
        app.toast_err(format!("Could not check for interrupted indexing: {e}"));
    }
}

fn resume_pending(
    db: &Arc<Db>,
    jobs: &JobRegistry,
    tx: &crossbeam_channel::Sender<crate::actions::Event>,
    ctx: &egui::Context,
) {
    use crate::actions::Event;
    let send = |event: Event| {
        let _ = tx.send(event);
        ctx.request_repaint();
    };
    let roots = match db.index_roots() {
        Ok(roots) => roots,
        Err(e) => {
            send(Event::Error(format!("Could not read indexed folders: {e}")));
            return;
        }
    };
    for root in roots {
        let state = db.index_state(root.id).ok().flatten();
        if !root.enabled || !state.as_ref().is_some_and(IndexState::is_resumable) {
            continue;
        }
        let reachable = std::fs::symlink_metadata(&root.path).is_ok_and(|m| m.is_dir());
        match plan_resume(&root, state.as_ref(), reachable) {
            Some(ResumePlan::Resume(path)) => {
                send(Event::Toast(format!(
                    "Resuming interrupted indexing of {}",
                    path.display()
                )));
                let tx = tx.clone();
                let c = ctx.clone();
                start_index_job(jobs, db.clone(), path, move |m| {
                    let _ = tx.send(Event::Toast(m));
                    c.request_repaint();
                });
            }
            Some(ResumePlan::Drop(path)) => {
                let _ = db.reset_index_state(root.id);
                send(Event::Error(format!(
                    "Interrupted indexing of {} was dropped: the folder is no longer available",
                    path.display()
                )));
            }
            None => {}
        }
    }
}

/// Fill image/video metadata for indexed files that lack it. Failures are recorded as
/// empty metadata so they are not retried forever; videos are skipped (not marked) when
/// ffprobe is unavailable, so they get picked up once it is installed.
pub fn extract_metadata(db: &Db, job: &JobHandle) {
    let ff = forge_media::FfTools::global();
    let include_video = ff.ffprobe.is_some();
    let mut after_id = 0;
    job.progress
        .set_current("Reading photo and video metadata…");
    loop {
        if job.cancel.check().is_err() {
            return;
        }
        let Ok(batch) = db.files_needing_meta_after(after_id, 500, include_video) else {
            return;
        };
        if batch.is_empty() {
            return;
        }
        after_id = batch.last().expect("non-empty metadata batch").id;
        job.progress.add_total_items(batch.len() as u64);
        for r in batch {
            if job.cancel.check().is_err() {
                return;
            }
            // Left without metadata: it is read once the file is downloaded (or never, if not).
            if !forge_core::cloud::background_read_allowed(&r.path) {
                job.progress.add_items(1);
                continue;
            }
            match r.media_kind {
                forge_core::MediaKind::Video => {
                    let m = ff
                        .probe(&r.path)
                        .map(|v| video_to_db(&v))
                        .unwrap_or_default();
                    let _ = db.upsert_video_meta(r.id, &m);
                }
                _ => {
                    let m = forge_media::read_image_metadata(&r.path)
                        .map(|i| image_to_db(&i))
                        .unwrap_or_default();
                    let _ = db.upsert_image_meta(r.id, &m);
                }
            }
            job.progress.add_items(1);
        }
    }
}

pub fn image_to_db(m: &forge_media::ImageMetadata) -> forge_db::ImageMeta {
    let (w, h) = m.display_dimensions().unzip();
    forge_db::ImageMeta {
        width: w,
        height: h,
        camera_make: m.make.clone(),
        camera_model: m.model.clone(),
        lens: m.lens.clone(),
        focal_length: m.focal_length_mm,
        aperture: m.aperture,
        shutter: m.exposure_time_s,
        iso: m.iso,
        exposure_comp: m.exposure_compensation,
        taken_at: m
            .capture_timestamp
            .map(|s| s * 1000 + m.capture_millis.unwrap_or(0) as i64),
        gps_lat: m.gps_lat,
        gps_lon: m.gps_lon,
        orientation: m.orientation,
        color_space: m.color_space.clone(),
        bit_depth: m.bit_depth.map(|b| b as u16),
    }
}

pub fn video_to_db(v: &forge_media::VideoMetadata) -> forge_db::VideoMeta {
    forge_db::VideoMeta {
        duration: v.duration_s,
        width: v.width,
        height: v.height,
        fps: v.fps,
        vfr: v.vfr,
        codec: v.codec.clone(),
        bit_depth: v.bit_depth.map(|b| b as u16),
        chroma: v.chroma_subsampling.clone(),
        bitrate: v.bitrate,
        audio_streams: Some(v.audio.len() as u32),
        hdr: v.hdr.map(|h| format!("{h:?}").to_lowercase()),
        rotation: Some(v.rotation as i32),
        timecode: v.timecode.clone(),
        camera: v.make.clone().map(|m| {
            format!("{m} {}", v.model.clone().unwrap_or_default())
                .trim()
                .to_string()
        }),
        recorded_at: None,
    }
}

fn merge_loaded_annotations(
    target: &mut HashMap<PathBuf, Annotation>,
    locally_edited: &HashSet<PathBuf>,
    loaded: HashMap<PathBuf, Annotation>,
) {
    for (path, annotation) in loaded {
        if !locally_edited.contains(&path) {
            target.insert(path, annotation);
        }
    }
}

fn merge_authoritative_annotations(
    target: &mut HashMap<PathBuf, Annotation>,
    current_versions: &HashMap<PathBuf, u64>,
    failed_versions: HashMap<PathBuf, u64>,
    authoritative: HashMap<PathBuf, Annotation>,
) {
    for (path, annotation) in authoritative {
        if current_versions.get(&path) == failed_versions.get(&path) {
            target.insert(path, annotation);
        }
    }
}

fn authoritative_annotations(
    db: &Db,
    paths: &[PathBuf],
) -> Result<HashMap<PathBuf, Annotation>, String> {
    let ids: HashMap<PathBuf, i64> = db
        .file_ids(paths)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    paths
        .iter()
        .map(|path| {
            let annotation = match ids.get(path) {
                Some(id) => db.annotation(*id).map_err(|e| e.to_string())?,
                None => forge_db::Annotation::default(),
            };
            let mut view = to_view(annotation.rating, annotation.label, annotation.pick);
            view.has_note = has_note(annotation.notes.as_deref());
            Ok((path.clone(), view))
        })
        .collect()
}

/// Whether a note exists for the badge: blank notes do not count.
fn has_note(notes: Option<&str>) -> bool {
    notes.is_some_and(|n| !n.trim().is_empty())
}

/// The new location of `path` when `old` (a file or folder) was moved to `new`. Components
/// are compared case-insensitively, so `C:\Shoot` matches `c:\shoot\a.jpg`, while
/// `C:\Shoot2` does not match `C:\Shoot`.
fn moved_path(path: &Path, old: &Path, new: &Path) -> Option<PathBuf> {
    let mut parts = path.components();
    for old_part in old.components() {
        let part = parts.next()?;
        if !part
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&old_part.as_os_str().to_string_lossy())
        {
            return None;
        }
    }
    Some(parts.fold(new.to_path_buf(), |acc, part| acc.join(part)))
}

/// Where `path` goes under a set of moves made together, if any of them covers it.
fn moved_by(path: &Path, pairs: &[(PathBuf, PathBuf)]) -> Option<PathBuf> {
    pairs.iter().find_map(|(old, new)| moved_path(path, old, new))
}

/// Re-keys every entry of `map` that sits under one of the moved paths. All moves are applied
/// as one set (every entry is taken out before any is put back), so a swap or a cycle of
/// renames keeps each value with its file. Moved entries replace whatever was keyed at their
/// destination, as the database merge does.
fn rekey_map<V>(map: &mut HashMap<PathBuf, V>, pairs: &[(PathBuf, PathBuf)]) {
    let moves: Vec<(PathBuf, PathBuf)> = map
        .keys()
        .filter_map(|key| moved_by(key, pairs).map(|to| (key.clone(), to)))
        .collect();
    let taken: Vec<(PathBuf, V)> = moves
        .into_iter()
        .filter_map(|(from, to)| map.remove(&from).map(|value| (to, value)))
        .collect();
    map.extend(taken);
}

/// The set counterpart of [`rekey_map`].
fn rekey_set(set: &mut HashSet<PathBuf>, pairs: &[(PathBuf, PathBuf)]) {
    let moves: Vec<(PathBuf, PathBuf)> = set
        .iter()
        .filter_map(|key| moved_by(key, pairs).map(|to| (key.clone(), to)))
        .collect();
    for (from, _) in &moves {
        set.remove(from);
    }
    set.extend(moves.into_iter().map(|(_, to)| to));
}

/// The XMP-visible part of an annotation change. Picks stay in the app library. Notes are
/// not part of this update: they go to `xmpDM:logComment`, and only for photos (see
/// `persist_annotation`).
fn sidecar_update(change: &AnnotationChange) -> SidecarUpdate {
    SidecarUpdate {
        rating: change.rating.map(i32::from),
        label: change.label.map(|l| l.map(|c| xmp_label(c).to_string())),
        add_keywords: change.add_tags.clone(),
        remove_keywords: change.remove_tags.clone(),
        ..Default::default()
    }
}

/// Label names as Lightroom, Bridge and Capture One write them in `xmp:Label`.
fn xmp_label(label: ColorLabel) -> &'static str {
    match label {
        ColorLabel::Red => "Red",
        ColorLabel::Yellow => "Yellow",
        ColorLabel::Green => "Green",
        ColorLabel::Blue => "Blue",
        ColorLabel::Purple => "Purple",
    }
}

/// Writes `update` to the sidecar of every path when enabled. Returns one summary message
/// if any file failed; the originals are never modified, so only sidecars are at risk.
fn sync_sidecars(
    enabled: bool,
    paths: &[PathBuf],
    update: &SidecarUpdate,
    skip: impl Fn(&Path) -> bool,
    mut write: impl FnMut(&Path, &SidecarUpdate) -> forge_media::Result<SidecarOutcome>,
) -> Option<String> {
    if !enabled || update.is_empty() {
        return None;
    }
    // Cloud placeholders are skipped, not failed: reading them would download the file.
    let (skipped, targets): (Vec<&PathBuf>, Vec<&PathBuf>) = paths.iter().partition(|p| skip(p));
    if !skipped.is_empty() {
        tracing::info!(
            count = skipped.len(),
            "XMP sidecar sync skipped online-only or unreadable files"
        );
    }
    let failures: Vec<String> = targets
        .iter()
        .filter_map(|p| write(p.as_path(), update).err().map(|e| e.to_string()))
        .collect();
    let first = failures.first()?;
    Some(format!(
        "XMP sidecar sync failed for {} of {} file(s); the photos were not modified. First error: {first}",
        failures.len(),
        targets.len()
    ))
}

/// Whether a sidecar write must be skipped for `image`: the photo itself, or its existing
/// sidecar, is an online-only cloud placeholder. Fails closed: when the attributes of either
/// cannot be read, the write is skipped. A sidecar that does not exist yet is not checked.
fn cloud_placeholder(image: &Path) -> bool {
    cloud_placeholder_with(image, &forge_core::cloud::FsAttributes)
}

fn cloud_placeholder_with(image: &Path, source: &dyn forge_core::cloud::AttributeSource) -> bool {
    use forge_core::cloud::background_read_allowed_with;
    if !background_read_allowed_with(image, source) {
        return true;
    }
    let sidecar = forge_media::xmp_write::sidecar_path_for(image);
    match std::fs::symlink_metadata(&sidecar) {
        Ok(_) => !background_read_allowed_with(&sidecar, source),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Whether a file gets the note in its sidecar (`xmpDM:logComment`): photos only.
fn note_goes_to_sidecar(path: &Path) -> bool {
    matches!(
        forge_core::model::media_kind_for_ext(&forge_core::model::ext_of(path)),
        forge_core::MediaKind::Image | forge_core::MediaKind::Raw
    )
}

fn spawn_writer(db: Arc<Db>, tx: Sender<LibMsg>) -> Sender<LibraryWrite> {
    let (wtx, wrx) = unbounded::<LibraryWrite>();
    std::thread::Builder::new()
        .name("library-writer".into())
        .spawn(move || {
            for write in wrx {
                match write {
                    LibraryWrite::Annotation(write) => {
                        persist_annotation(&db, &tx, write);
                    }
                    LibraryWrite::RenameTag { old, new, reply } => {
                        let result = db.rename_tag(&old, &new).map_err(|e| e.to_string());
                        let _ = reply.send(result);
                    }
                    LibraryWrite::DeleteTag { name, reply } => {
                        let result = db.delete_tag(&name).map_err(|e| e.to_string());
                        let _ = reply.send(result);
                    }
                    LibraryWrite::Flush(done) => {
                        let _ = done.send(());
                    }
                    LibraryWrite::Relink { pairs } => {
                        // One set in one transaction: swaps and cycles stay correct.
                        if let Err(error) = db.relink_many(&pairs) {
                            let _ = tx.send(LibMsg::Error(format!(
                                "Could not update the library after {} item(s) were moved or renamed: {error}",
                                pairs.len()
                            )));
                        }
                    }
                }
            }
        })
        .expect("spawn library writer");
    wtx
}

fn persist_annotation(db: &Db, tx: &Sender<LibMsg>, write: AnnotationWrite) {
    let AnnotationWrite {
        paths,
        change,
        versions,
        xmp_sync,
        entries: captured,
        mut ack,
    } = write;
    if paths.is_empty() {
        if let Some(ack) = ack {
            let _ = ack.send(Ok(()));
        }
        return;
    }

    let mut entries = Vec::with_capacity(paths.len());
    let mut failures = Vec::new();
    for path in &paths {
        let looked_up = match captured.get(path) {
            Some(entry) => Ok(entry.clone()),
            None => forge_core::listing::entry_for_path(path),
        };
        match looked_up {
            Ok(entry) => entries.push((path.clone(), entry)),
            Err(error) => failures.push((path.clone(), error.to_string())),
        }
    }

    let mut saved_paths = Vec::new();
    let mut db_failure = None;
    if !entries.is_empty() {
        let entry_rows: Vec<_> = entries.iter().map(|(_, entry)| entry.clone()).collect();
        if let Err(error) = db.upsert_entries(&entry_rows) {
            db_failure = Some(format!("could not index requested files: {error}"));
        } else {
            let index_paths: Vec<_> = entries.iter().map(|(path, _)| path.clone()).collect();
            match db.file_ids(&index_paths) {
                Err(error) => {
                    db_failure = Some(format!("could not resolve indexed files: {error}"))
                }
                Ok(resolved) => {
                    let id_by_path: HashMap<PathBuf, i64> = resolved.into_iter().collect();
                    let ids: Vec<_> = entries
                        .iter()
                        .filter_map(|(path, _)| id_by_path.get(path).copied())
                        .collect();
                    for (path, _) in &entries {
                        if !id_by_path.contains_key(path) {
                            failures.push((
                                path.clone(),
                                "the indexed file row could not be resolved".into(),
                            ));
                        }
                    }
                    if ids.is_empty() {
                        db_failure = Some(
                            "no requested path resolved to a database id; no annotation was saved"
                                .into(),
                        );
                    } else if let Err(error) = db.annotate(&ids, &change) {
                        db_failure = Some(format!("database annotation write failed: {error}"));
                    } else {
                        saved_paths = entries
                            .iter()
                            .filter(|(path, entry)| {
                                id_by_path.contains_key(path)
                                    && entry.kind == forge_core::model::EntryKind::File
                            })
                            .map(|(path, _)| path.clone())
                            .collect();
                    }
                }
            }
        }
    } else {
        db_failure =
            Some("no requested path exists or can be indexed; no annotation was saved".into());
    }

    if db_failure.is_some() {
        saved_paths.clear();
    }

    if !failures.is_empty() || db_failure.is_some() {
        let mut failed_paths: Vec<_> = failures.iter().map(|(path, _)| path.clone()).collect();
        if db_failure.is_some() {
            failed_paths.extend(paths.iter().cloned());
        }
        failed_paths.sort();
        failed_paths.dedup();
        let failed_versions = versions
            .into_iter()
            .filter(|(path, _)| failed_paths.contains(path))
            .collect();
        let mut details = failures
            .iter()
            .map(|(path, error)| format!("{}: {error}", path.display()))
            .collect::<Vec<_>>();
        if let Some(error) = db_failure {
            details.push(error);
        }
        if let Some(ack) = ack.take() {
            let _ = ack.send(Err(details.join("; ")));
        }
        let _ = tx.send(LibMsg::Error(format!(
            "Some annotations were not saved: {}",
            details.join("; ")
        )));
        match authoritative_annotations(db, &failed_paths) {
            Ok(annotations) => {
                let _ = tx.send(LibMsg::AuthoritativeAnnotations {
                    versions: failed_versions,
                    annotations,
                });
            }
            Err(reload_error) => {
                let _ = tx.send(LibMsg::Error(format!(
                    "Reloading saved annotations failed: {reload_error}"
                )));
            }
        }
    }

    if let Some(ack) = ack {
        let _ = ack.send(Ok(()));
    }

    // Photos get the note as well; other files get only the XMP-visible fields. Each group
    // is its own update, so a note-only change never touches a non-photo's sidecar.
    let (photos, others): (Vec<PathBuf>, Vec<PathBuf>) = saved_paths
        .into_iter()
        .partition(|path| note_goes_to_sidecar(path));
    let base = sidecar_update(&change);
    let photo_update = SidecarUpdate {
        note: change.notes.clone(),
        ..base.clone()
    };
    for msg in [
        sync_sidecars(xmp_sync, &photos, &photo_update, cloud_placeholder, update_sidecar),
        sync_sidecars(xmp_sync, &others, &base, cloud_placeholder, update_sidecar),
    ]
    .into_iter()
    .flatten()
    {
        let _ = tx.send(LibMsg::Error(msg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_media::MediaError;

    fn pass(status: forge_db::ScanStatus, pending: &[&str]) -> IndexState {
        IndexState {
            root_id: 1,
            scan_id: 1,
            status,
            started_at: 0,
            updated_at: 0,
            finished_at: None,
            pending: pending.iter().map(PathBuf::from).collect(),
            items_seen: 0,
            errors: 0,
            last_error: None,
        }
    }

    #[test]
    fn startup_resumes_only_enabled_unfinished_passes_whose_folder_exists() {
        use forge_db::ScanStatus::{Complete, Running};
        let root = IndexRoot {
            id: 1,
            path: PathBuf::from(r"C:\shoot"),
            enabled: true,
            added_at: 0,
            last_complete_at: None,
        };
        let unfinished = pass(Running, &[r"C:\shoot\day1"]);
        assert_eq!(
            plan_resume(&root, Some(&unfinished), true),
            Some(ResumePlan::Resume(root.path.clone()))
        );
        assert_eq!(
            plan_resume(&root, Some(&unfinished), false),
            Some(ResumePlan::Drop(root.path.clone()))
        );
        assert_eq!(plan_resume(&root, Some(&pass(Complete, &[])), true), None);
        assert_eq!(plan_resume(&root, None, true), None);
        let disabled = IndexRoot {
            enabled: false,
            ..root.clone()
        };
        assert_eq!(plan_resume(&disabled, Some(&unfinished), true), None);
    }

    fn annotation(rating: u8) -> Annotation {
        Annotation {
            rating,
            ..Default::default()
        }
    }

    fn library_with_test_writer(db: Arc<Db>) -> Library {
        let (tx, rx) = unbounded();
        let writer = Some(spawn_writer(db.clone(), tx.clone()));
        Library {
            db: Some(db),
            open_error: None,
            tx,
            rx,
            ctx: egui::Context::default(),
            annotations: HashMap::new(),
            loaded_dirs: HashSet::new(),
            locally_edited: HashSet::new(),
            annotation_versions: HashMap::new(),
            next_annotation_version: 0,
            queued: Vec::new(),
            writer,
            xmp_sync: false,
            search: SearchState::default(),
            resume_checked: false,
        }
    }

    #[test]
    fn directory_load_does_not_overwrite_optimistic_edits() {
        let edited = PathBuf::from(r"C:\photos\edited.jpg");
        let untouched = PathBuf::from(r"C:\photos\untouched.jpg");
        let mut cache = HashMap::from([(edited.clone(), annotation(5))]);
        let locally_edited = HashSet::from([edited.clone()]);
        let loaded = HashMap::from([
            (edited.clone(), annotation(1)),
            (untouched.clone(), annotation(2)),
        ]);

        merge_loaded_annotations(&mut cache, &locally_edited, loaded);

        assert_eq!(cache.get(&edited).unwrap().rating, 5);
        assert_eq!(cache.get(&untouched).unwrap().rating, 2);
    }

    #[test]
    fn failed_write_reload_only_replaces_the_version_that_failed() {
        let edited = PathBuf::from(r"C:\photos\edited.jpg");
        let mut cache = HashMap::from([(edited.clone(), annotation(5))]);
        let current_versions = HashMap::from([(edited.clone(), 2)]);
        let failed_versions = HashMap::from([(edited.clone(), 1)]);
        let authoritative = HashMap::from([(edited.clone(), annotation(3))]);

        merge_authoritative_annotations(
            &mut cache,
            &current_versions,
            failed_versions,
            authoritative,
        );

        assert_eq!(cache.get(&edited).unwrap().rating, 5);
    }

    #[test]
    fn failed_write_reload_restores_authoritative_value_when_version_is_current() {
        let edited = PathBuf::from(r"C:\photos\edited.jpg");
        let mut cache = HashMap::from([(edited.clone(), annotation(5))]);
        let current_versions = HashMap::from([(edited.clone(), 2)]);
        let failed_versions = HashMap::from([(edited.clone(), 2)]);
        let authoritative = HashMap::from([(edited.clone(), annotation(3))]);

        merge_authoritative_annotations(
            &mut cache,
            &current_versions,
            failed_versions,
            authoritative,
        );

        assert_eq!(cache.get(&edited).unwrap().rating, 3);
    }

    #[test]
    fn sidecars_are_written_only_when_sync_is_enabled() {
        let paths = vec![PathBuf::from("a.jpg"), PathBuf::from("b.jpg")];
        let update = SidecarUpdate {
            rating: Some(3),
            ..Default::default()
        };
        let mut calls = 0;
        let off = sync_sidecars(false, &paths, &update, |_| false, |p, _| {
            calls += 1;
            Ok(SidecarOutcome::Unchanged(p.to_path_buf()))
        });
        assert_eq!(calls, 0);
        assert!(off.is_none());

        let on = sync_sidecars(true, &paths, &update, |_| false, |p, _| {
            calls += 1;
            Ok(SidecarOutcome::Unchanged(p.to_path_buf()))
        });
        assert_eq!(calls, 2);
        assert!(on.is_none());
    }

    #[test]
    fn skipped_files_are_never_written_or_counted_as_failures() {
        let paths = vec![PathBuf::from("cloud.jpg"), PathBuf::from("local.jpg")];
        let update = SidecarUpdate {
            rating: Some(2),
            ..Default::default()
        };
        let mut written = Vec::new();
        let msg = sync_sidecars(true, &paths, &update, |p| p.ends_with("cloud.jpg"), |p, _| {
            written.push(p.to_path_buf());
            Ok(SidecarOutcome::Unchanged(p.to_path_buf()))
        });
        assert!(msg.is_none());
        assert_eq!(written, vec![PathBuf::from("local.jpg")]);
    }

    #[test]
    fn sidecar_failures_are_reported_as_one_summary() {
        let paths = vec![PathBuf::from("a.jpg"), PathBuf::from("b.jpg")];
        let update = SidecarUpdate {
            add_keywords: vec!["k".into()],
            ..Default::default()
        };
        let msg = sync_sidecars(true, &paths, &update, |_| false, |p, _| {
            if p.ends_with("b.jpg") {
                Err(MediaError::Unsupported("read-only".into()))
            } else {
                Ok(SidecarOutcome::Unchanged(p.to_path_buf()))
            }
        })
        .expect("one failure");
        assert!(msg.contains("1 of 2"), "{msg}");
        assert!(msg.contains("read-only"), "{msg}");
    }

    #[test]
    fn pick_only_change_never_touches_sidecars() {
        let change = AnnotationChange {
            pick: Some(PickState::Pick),
            ..Default::default()
        };
        assert!(sidecar_update(&change).is_empty());
    }

    #[test]
    fn annotation_change_maps_to_sidecar_fields() {
        let change = AnnotationChange {
            rating: Some(4),
            label: Some(Some(ColorLabel::Purple)),
            add_tags: vec!["travel".into()],
            remove_tags: vec!["old".into()],
            ..Default::default()
        };
        let u = sidecar_update(&change);
        assert_eq!(u.rating, Some(4));
        assert_eq!(u.label, Some(Some("Purple".to_string())));
        assert_eq!(u.add_keywords, vec!["travel".to_string()]);
        assert_eq!(u.remove_keywords, vec!["old".to_string()]);
        let clear = AnnotationChange {
            label: Some(None),
            ..Default::default()
        };
        assert_eq!(sidecar_update(&clear).label, Some(None));
    }

    #[test]
    fn unresolved_annotation_path_reports_error_and_reverts_optimistic_cache() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let mut library = library_with_test_writer(db);
        let path = std::env::temp_dir().join(format!(
            "zxraw-does-not-exist-{}-{}.jpg",
            std::process::id(),
            forge_db::latest_version()
        ));
        let _ = std::fs::remove_file(&path);
        let sidecar = path.with_extension("xmp");
        let _ = std::fs::remove_file(&sidecar);
        library.xmp_sync = true;

        library.annotate(
            vec![path.clone()],
            AnnotationChange {
                rating: Some(5),
                ..Default::default()
            },
        );
        assert!(
            !library
                .delete_tag("test-writer-barrier-tag".into())
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .unwrap()
        );
        let notices = library.poll();

        assert!(notices.iter().any(|notice| match notice {
            Err(error) => error.contains(&path.display().to_string()),
            Ok(_) => false,
        }));
        assert_eq!(library.annotations[&path].rating, 0);
        assert!(!sidecar.exists());
    }

    #[test]
    fn metadata_extraction_skips_online_only_photos() {
        let photo: &[u8] = include_bytes!("../../../docs/images/privacy.jpg");
        let root = std::env::temp_dir().join(format!(
            "zxraw-meta-online-only-{}-{}",
            std::process::id(),
            forge_db::latest_version()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let local = root.join("local.jpg");
        let cloud = root.join("cloud.jpg");
        std::fs::write(&local, photo).unwrap();
        std::fs::write(&cloud, photo).unwrap();
        let db = Db::open_in_memory().unwrap();
        let root_id = db.add_index_root(&root).unwrap();
        forge_db::Indexer::new(&db).index_root(root_id).unwrap();
        forge_core::cloud::simulate_online_only(&cloud);

        let job = forge_jobs::JobRegistry::new().spawn("test", "test", |_| Ok(()));
        extract_metadata(&db, &job);
        forge_core::cloud::clear_simulated_online_only(&cloud);

        let pending = db.files_needing_meta_after(0, 100, false).unwrap();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending[0].path, cloud);
        let local_id = db.file_id(&local).unwrap().unwrap();
        assert!(db.image_meta(local_id).unwrap().is_some());
        assert!(db.image_meta(pending[0].id).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tag_rename_then_annotation_add_share_writer_order() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let root = std::env::temp_dir().join(format!(
            "zxraw-writer-order-{}-{}",
            std::process::id(),
            forge_db::latest_version()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("photo.jpg");
        std::fs::write(&path, b"photo").unwrap();
        let entry = forge_core::listing::entry_for_path(&path).unwrap();
        let id = db.upsert_entries(&[entry]).unwrap()[0];
        db.add_tags(&[id], &["old"]).unwrap();
        let mut library = library_with_test_writer(db.clone());

        let rename = library.rename_tag("old".into(), "renamed".into());
        library.annotate(
            vec![path.clone()],
            AnnotationChange {
                add_tags: vec!["old".into()],
                ..Default::default()
            },
        );
        let barrier = library.delete_tag("unused-writer-barrier-tag".into());

        rename
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert!(
            !barrier
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .unwrap()
        );
        assert_eq!(db.annotation(id).unwrap().tags, ["old", "renamed"]);
        drop(library);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zxraw-{name}-{}-{}",
            std::process::id(),
            forge_db::latest_version()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn index(db: &Db, paths: &[&Path]) {
        let entries: Vec<_> = paths
            .iter()
            .map(|p| forge_core::listing::entry_for_path(p).unwrap())
            .collect();
        db.upsert_entries(&entries).unwrap();
    }

    /// Returns once every write queued before this call has reached the database.
    fn wait_for_writer(library: &Library) {
        let barrier = library.delete_tag("follow-writer-barrier-tag".into());
        assert!(
            !barrier
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap()
        );
    }

    #[test]
    fn blank_notes_do_not_count_as_a_note() {
        assert!(has_note(Some("client proof")));
        assert!(!has_note(Some("  \t ")));
        assert!(!has_note(None));
    }

    #[test]
    fn confirmed_annotation_answers_after_the_database_write() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let root = temp_root("confirmed-note");
        let photo = root.join("a.jpg");
        std::fs::write(&photo, b"photo").unwrap();
        let mut library = library_with_test_writer(db.clone());
        let note = |text: &str| AnnotationChange {
            notes: Some(Some(text.into())),
            ..Default::default()
        };
        let answer = library.annotate_confirmed(vec![photo.clone()], note("client proof"));
        assert_eq!(
            answer.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
            Ok(())
        );
        let id = db.file_id(&photo).unwrap().unwrap();
        assert_eq!(db.annotation(id).unwrap().notes.as_deref(), Some("client proof"));

        let missing = root.join("gone.jpg");
        let answer = library.annotate_confirmed(vec![missing], note("lost"));
        assert!(
            answer
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .is_err()
        );
        assert!(library.flush(std::time::Duration::from_secs(5)));
        drop(library);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn swapped_names_keep_their_notes_in_memory_and_in_the_database() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let root = temp_root("swap-notes");
        let (a, b) = (root.join("a.jpg"), root.join("b.jpg"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        index(&db, &[&a, &b]);
        let mut library = library_with_test_writer(db.clone());
        for (path, text, rating) in [(&a, "note of a", 5), (&b, "note of b", 1)] {
            library.annotate(
                vec![path.clone()],
                AnnotationChange {
                    rating: Some(rating),
                    notes: Some(Some(text.into())),
                    ..Default::default()
                },
            );
        }
        assert!(library.flush(std::time::Duration::from_secs(5)));

        // Swap the files on disk, as a batch rename would, then follow both renames at once.
        let tmp = root.join("swap.tmp");
        std::fs::rename(&a, &tmp).unwrap();
        std::fs::rename(&b, &a).unwrap();
        std::fs::rename(&tmp, &b).unwrap();
        library.follow_moves(vec![(a.clone(), b.clone()), (b.clone(), a.clone())]);
        assert!(library.flush(std::time::Duration::from_secs(5)));

        assert_eq!(library.annotations[&b].rating, 5);
        assert_eq!(library.annotations[&a].rating, 1);
        let note_at = |p: &Path| {
            let id = db.file_id(p).unwrap().unwrap();
            db.annotation(id).unwrap().notes
        };
        assert_eq!(note_at(&b).as_deref(), Some("note of a"));
        assert_eq!(note_at(&a).as_deref(), Some("note of b"));
        drop(library);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn moved_path_matches_whole_components_case_insensitively() {
        let old = Path::new(r"C:\Shoot");
        let new = Path::new(r"D:\Archive\Shoot");
        assert_eq!(
            moved_path(Path::new(r"c:\shoot\day1\a.jpg"), old, new),
            Some(PathBuf::from(r"D:\Archive\Shoot\day1\a.jpg"))
        );
        assert_eq!(moved_path(Path::new(r"C:\Shoot"), old, new), Some(new.to_path_buf()));
        assert_eq!(moved_path(Path::new(r"C:\Shoot2\a.jpg"), old, new), None);
        assert_eq!(moved_path(Path::new(r"C:\Other\a.jpg"), old, new), None);
    }

    #[test]
    fn note_and_rating_follow_a_renamed_file() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let root = temp_root("follow-file");
        let photo = root.join("a.jpg");
        std::fs::write(&photo, b"photo").unwrap();
        index(&db, &[&photo]);
        let mut library = library_with_test_writer(db.clone());
        library.annotate(
            vec![photo.clone()],
            AnnotationChange {
                rating: Some(4),
                notes: Some(Some("client proof".into())),
                ..Default::default()
            },
        );
        wait_for_writer(&library);

        let renamed = root.join("final.jpg");
        std::fs::rename(&photo, &renamed).unwrap();
        library.follow_moves(vec![(photo.clone(), renamed.clone())]);
        wait_for_writer(&library);

        let id = db
            .file_id(&renamed)
            .unwrap()
            .expect("the row moved to the new path");
        let saved = db.annotation(id).unwrap();
        assert_eq!(saved.rating, 4);
        assert_eq!(saved.notes.as_deref(), Some("client proof"));
        assert!(db.file_id(&photo).unwrap().is_none());
        assert!(!library.annotations.contains_key(&photo));
        assert_eq!(library.annotations[&renamed].rating, 4);
        assert!(library.annotations[&renamed].has_note);
        drop(library);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn note_follows_a_moved_folder_and_its_files() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let root = temp_root("follow-folder");
        let trip = root.join("trip");
        std::fs::create_dir_all(&trip).unwrap();
        let photo = trip.join("a.jpg");
        std::fs::write(&photo, b"photo").unwrap();
        index(&db, &[&photo]);
        let mut library = library_with_test_writer(db.clone());
        library.annotate(
            vec![photo.clone()],
            AnnotationChange {
                rating: Some(2),
                notes: Some(Some("day one".into())),
                ..Default::default()
            },
        );
        wait_for_writer(&library);

        let moved = root.join("trip-final");
        std::fs::rename(&trip, &moved).unwrap();
        library.follow_moves(vec![(trip.clone(), moved.clone())]);
        wait_for_writer(&library);

        let new_photo = moved.join("a.jpg");
        let id = db
            .file_id(&new_photo)
            .unwrap()
            .expect("the file row moved with its folder");
        let saved = db.annotation(id).unwrap();
        assert_eq!(saved.rating, 2);
        assert_eq!(saved.notes.as_deref(), Some("day one"));
        assert!(!library.annotations.contains_key(&photo));
        assert!(library.annotations[&new_photo].has_note);
        drop(library);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn xmp_note_goes_to_photos_only_and_keeps_the_caption() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let root = temp_root("xmp-note");
        let photo = root.join("p.jpg");
        let text = root.join("n.txt");
        std::fs::write(&photo, b"photo").unwrap();
        std::fs::write(&text, b"text").unwrap();
        index(&db, &[&photo, &text]);
        // A caption that was already in the photo's sidecar before the note.
        forge_media::update_sidecar(
            &photo,
            &forge_media::SidecarUpdate {
                description: Some(Some("Harbour at dusk".into())),
                ..Default::default()
            },
        )
        .unwrap();
        let mut library = library_with_test_writer(db);
        library.xmp_sync = true;

        library.annotate(
            vec![photo.clone(), text.clone()],
            AnnotationChange {
                notes: Some(Some("client proof".into())),
                ..Default::default()
            },
        );
        wait_for_writer(&library);

        let sidecar = forge_media::xmp_write::sidecar_path_for(&photo);
        let xmp = std::fs::read_to_string(&sidecar).unwrap();
        assert!(xmp.contains("logComment"), "note missing: {xmp}");
        assert!(xmp.contains("client proof"), "note missing: {xmp}");
        assert!(xmp.contains("Harbour at dusk"), "caption was lost: {xmp}");
        assert!(
            !forge_media::xmp_write::sidecar_path_for(&text).exists(),
            "a note must not create a sidecar for a non-photo"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sidecar_writes_fail_closed_for_cloud_placeholders() {
        struct Attrs(Option<u32>, Option<u32>);
        impl forge_core::cloud::AttributeSource for Attrs {
            fn attributes(&self, path: &Path) -> Option<u32> {
                if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("xmp")) {
                    self.1
                } else {
                    self.0
                }
            }
        }
        let root = temp_root("cloud-sidecar");
        let photo = root.join("a.jpg");
        std::fs::write(&photo, b"photo").unwrap();
        // No sidecar yet: only the photo is checked.
        assert!(!cloud_placeholder_with(&photo, &Attrs(Some(0x20), None)));
        assert!(cloud_placeholder_with(&photo, &Attrs(Some(0x400000), None)));
        // Unreadable attributes fail closed.
        assert!(cloud_placeholder_with(&photo, &Attrs(None, None)));
        std::fs::write(forge_media::xmp_write::sidecar_path_for(&photo), b"<x/>").unwrap();
        assert!(!cloud_placeholder_with(&photo, &Attrs(Some(0x20), Some(0x20))));
        assert!(cloud_placeholder_with(&photo, &Attrs(Some(0x20), Some(0x1000))));
        assert!(cloud_placeholder_with(&photo, &Attrs(Some(0x20), None)));
        std::fs::remove_dir_all(root).unwrap();
    }
}
