//! The persistent library: SQLite database (opened off the UI thread), per-folder
//! annotations (ratings/labels/picks), indexing + metadata extraction jobs, and search.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use forge_core::query::{ColorLabel, PickState};
use forge_db::{AnnotationChange, Db, FileRecord, SearchOptions};
use forge_jobs::{JobHandle, JobRegistry};
use forge_media::{SidecarOutcome, SidecarUpdate, update_sidecar};

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
    /// Edits made before the database finished opening; flushed once it is ready.
    queued: Vec<AnnotationWrite>,
    /// Single writer thread so edits reach the database in the order they were made.
    writer: Option<Sender<AnnotationWrite>>,
    /// Mirror annotation changes into `.xmp` sidecars (set from the settings every frame).
    xmp_sync: bool,
    pub search: SearchState,
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
                    .filter(|r| r.rating > 0 || r.label.is_some() || r.pick != PickState::None)
                    .map(|r| (r.path.clone(), to_view(r.rating, r.label, r.pick)))
                    .collect();
                let _ = tx.send(LibMsg::DirAnnotations(dir, map));
                ctx.request_repaint();
            }
        });
    }

    /// Apply an annotation change to files (optimistically updates the local cache, then
    /// persists in the background). Files are added to the database if not indexed yet.
    pub fn annotate(&mut self, paths: Vec<PathBuf>, change: AnnotationChange) {
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
        }
        let write = AnnotationWrite {
            paths,
            change,
            versions,
            xmp_sync: self.xmp_sync,
        };
        if self.db.is_none() {
            if self.open_error.is_some() {
                let _ = self.tx.send(LibMsg::Error(
                    "Library database is not available; change not saved".into(),
                ));
            } else {
                self.queued.push(write);
            }
            return;
        }
        self.persist(write);
    }

    fn persist(&self, write: AnnotationWrite) {
        if let Some(w) = &self.writer {
            let _ = w.send(write);
        }
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
            .volume_resolver(|p| {
                let mut v = forge_db::VolumeInfo::from_path_root(p);
                v.serial = forge_win::volume_serial(Path::new(&v.root))
                    .ok()
                    .map(|s| format!("{s:08X}"));
                Some(v)
            });
        let report = indexer.index_root(root_id).map_err(|e| e.to_string())?;
        job.log(format!("{report:?}"));
        if job.cancel.is_cancelled() {
            return Ok(());
        }
        extract_metadata(&db, job);
        notify(format!("Indexed {}", root.display()));
        Ok(())
    })
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
            Ok((
                path.clone(),
                to_view(annotation.rating, annotation.label, annotation.pick),
            ))
        })
        .collect()
}

/// The XMP-visible part of an annotation change. Picks and notes stay in the app library.
fn sidecar_update(change: &AnnotationChange) -> SidecarUpdate {
    SidecarUpdate {
        rating: change.rating.map(i32::from),
        label: change.label.map(|l| l.map(|c| xmp_label(c).to_string())),
        add_keywords: change.add_tags.clone(),
        remove_keywords: change.remove_tags.clone(),
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
    mut write: impl FnMut(&Path, &SidecarUpdate) -> forge_media::Result<SidecarOutcome>,
) -> Option<String> {
    if !enabled || update.is_empty() {
        return None;
    }
    let failures: Vec<String> = paths
        .iter()
        .filter_map(|p| write(p.as_path(), update).err().map(|e| e.to_string()))
        .collect();
    let first = failures.first()?;
    Some(format!(
        "XMP sidecar sync failed for {} of {} file(s); the photos were not modified. First error: {first}",
        failures.len(),
        paths.len()
    ))
}

fn spawn_writer(db: Arc<Db>, tx: Sender<LibMsg>) -> Sender<AnnotationWrite> {
    let (wtx, wrx) = unbounded::<AnnotationWrite>();
    std::thread::Builder::new()
        .name("library-writer".into())
        .spawn(move || {
            for AnnotationWrite {
                paths,
                change,
                versions,
                xmp_sync,
            } in wrx
            {
                let r = (|| -> Result<(), String> {
                    let entries: Vec<_> = paths
                        .iter()
                        .filter_map(|p| forge_core::listing::entry_for_path(p).ok())
                        .collect();
                    db.upsert_entries(&entries).map_err(|e| e.to_string())?;
                    let ids: Vec<_> = db
                        .file_ids(&paths)
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .map(|(_, id)| id)
                        .collect();
                    db.annotate(&ids, &change)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                })();
                if let Err(e) = r {
                    let _ = tx.send(LibMsg::Error(format!("Saving rating failed: {e}")));
                    match authoritative_annotations(&db, &paths) {
                        Ok(annotations) => {
                            let _ = tx.send(LibMsg::AuthoritativeAnnotations {
                                versions,
                                annotations,
                            });
                        }
                        Err(reload_error) => {
                            let _ = tx.send(LibMsg::Error(format!(
                                "Reloading saved ratings failed: {reload_error}"
                            )));
                        }
                    }
                } else if let Some(msg) =
                    sync_sidecars(xmp_sync, &paths, &sidecar_update(&change), update_sidecar)
                {
                    let _ = tx.send(LibMsg::Error(msg));
                }
            }
        })
        .expect("spawn library writer");
    wtx
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_media::MediaError;

    fn annotation(rating: u8) -> Annotation {
        Annotation {
            rating,
            ..Default::default()
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
        let off = sync_sidecars(false, &paths, &update, |p, _| {
            calls += 1;
            Ok(SidecarOutcome::Unchanged(p.to_path_buf()))
        });
        assert_eq!(calls, 0);
        assert!(off.is_none());

        let on = sync_sidecars(true, &paths, &update, |p, _| {
            calls += 1;
            Ok(SidecarOutcome::Unchanged(p.to_path_buf()))
        });
        assert_eq!(calls, 2);
        assert!(on.is_none());
    }

    #[test]
    fn sidecar_failures_are_reported_as_one_summary() {
        let paths = vec![PathBuf::from("a.jpg"), PathBuf::from("b.jpg")];
        let update = SidecarUpdate {
            add_keywords: vec!["k".into()],
            ..Default::default()
        };
        let msg = sync_sidecars(true, &paths, &update, |p, _| {
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
}
