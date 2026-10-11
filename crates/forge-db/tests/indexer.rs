//! Indexer behaviour on real temp folders: Unicode paths, exclusions, interruption and
//! resume, missing-file sweep, annotations surviving offline + relink, reconcile, links.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use forge_core::query::{ColorLabel, PickState, Query};
use forge_db::{
    AnnotationChange, Db, ExclusionKind, IndexOutcome, Indexer, ScanStatus, SearchOptions,
};
use forge_jobs::CancelToken;

fn touch(p: &Path, bytes: usize) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, vec![b'x'; bytes]).unwrap();
}

fn count(db: &Db, q: &str) -> u64 {
    db.count(&Query::parse(q).unwrap(), &SearchOptions::default())
        .unwrap()
}

fn offline_opts() -> SearchOptions {
    SearchOptions {
        include_offline: true,
        ..Default::default()
    }
}

/// 12 folders x 25 files (+ nested), Unicode names.
fn build_tree(root: &Path) -> usize {
    let mut n = 0;
    for d in 0..12 {
        let dir = root.join(format!("Папка_{d:02}")).join("日本語");
        for f in 0..25 {
            touch(&dir.join(format!("IMG_{d:02}{f:03}.CR3")), 10);
            n += 1;
        }
    }
    n
}

#[test]
fn indexes_unicode_tree_and_searches_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Shoot ü 写真");
    let files = build_tree(&root);
    touch(&root.join("Ärger.JPG"), 2048);
    let db = Db::open(tmp.path().join("forge.db")).unwrap();
    let rid = db.add_index_root(&root).unwrap();
    let rep = Indexer::new(&db).index_root(rid).unwrap();
    assert_eq!(rep.outcome, IndexOutcome::Completed);
    assert!(!rep.resumed);
    // root + 12 + 12 dirs + files + Ärger
    assert_eq!(rep.items as usize, 1 + 24 + files + 1);
    assert_eq!(count(&db, "kind:raw"), files as u64);
    assert_eq!(count(&db, "папка_03 kind:file"), 25);
    assert_eq!(count(&db, "日本語 kind:dir"), 12);
    assert_eq!(count(&db, "name:ärger ext:jpg size:2kb"), 1);
    let st = db.index_state(rid).unwrap().unwrap();
    assert_eq!(st.status, ScanStatus::Complete);
    assert!(st.pending.is_empty());
    assert!(db.index_roots().unwrap()[0].last_complete_at.is_some());

    // A second pass is a no-op for presence.
    let rep2 = Indexer::new(&db).index_root(rid).unwrap();
    assert_eq!(rep2.marked_missing, 0);
    assert_eq!(db.stats().unwrap().offline, 0);
}

#[test]
fn exclusions_are_honoured() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    touch(&root.join("keep/a.jpg"), 1);
    touch(&root.join("keep/b.tmp"), 1);
    touch(&root.join("cache/c.jpg"), 1);
    touch(&root.join("deep/node_modules/x.js"), 1);
    touch(&root.join("deep/y.jpg"), 1);
    let db = Db::open_in_memory().unwrap();
    let rid = db.add_index_root(&root).unwrap();
    db.add_exclusion(ExclusionKind::Folder, &root.join("CACHE").to_string_lossy())
        .unwrap();
    db.add_exclusion(ExclusionKind::Ext, ".TMP").unwrap();
    db.add_exclusion(ExclusionKind::Name, "node_modules")
        .unwrap();
    Indexer::new(&db).index_root(rid).unwrap();
    let names: Vec<String> = db
        .search(&Query::all(), &SearchOptions::default())
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert!(names.contains(&"a.jpg".to_string()));
    assert!(names.contains(&"y.jpg".to_string()));
    for gone in ["b.tmp", "cache", "c.jpg", "node_modules", "x.js"] {
        assert!(
            !names.contains(&gone.to_string()),
            "{gone} should be excluded: {names:?}"
        );
    }

    // Adding an exclusion later hides already-indexed rows on the next complete pass.
    db.add_exclusion(ExclusionKind::Name, "deep").unwrap();
    let rep = Indexer::new(&db).index_root(rid).unwrap();
    assert_eq!(rep.marked_missing, 2); // deep + deep\y.jpg
    assert_eq!(count(&db, "name:y.jpg"), 0);

    // reconcile_dir honours exclusions too.
    touch(&root.join("keep/z.tmp"), 1);
    Indexer::new(&db).reconcile_dir(&root.join("keep")).unwrap();
    assert_eq!(count(&db, "ext:tmp"), 0);
}

#[test]
fn interrupted_pass_resumes_from_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("big");
    let files = build_tree(&root);
    let total = 1 + 24 + files;
    let db = Db::open(tmp.path().join("forge.db")).unwrap();
    let rid = db.add_index_root(&root).unwrap();

    // Run 1: cancel after the 2nd committed batch.
    let token = CancelToken::new();
    let batches = AtomicUsize::new(0);
    let t2 = token.clone();
    let rep1 = Indexer::new(&db)
        .batch_size(40)
        .cancel_token(token.clone())
        .on_batch(|_| {
            if batches.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                t2.cancel();
            }
        })
        .index_root(rid)
        .unwrap();
    assert_eq!(rep1.outcome, IndexOutcome::Cancelled);
    let st = db.index_state(rid).unwrap().unwrap();
    assert_eq!(st.status, ScanStatus::Interrupted);
    assert!(!st.pending.is_empty());
    let after1 = db.stats().unwrap();
    let indexed1 = (after1.files + after1.dirs) as usize;
    assert!(
        indexed1 > 0 && indexed1 < total,
        "partial: {indexed1} of {total}"
    );

    // Run 2: simulate a crash (panic) right after the first committed batch.
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Indexer::new(&db)
            .batch_size(40)
            .on_batch(|_| panic!("simulated crash"))
            .index_root(rid)
            .unwrap();
    }));
    assert!(crashed.is_err());
    let st = db.index_state(rid).unwrap().unwrap();
    assert_eq!(st.status, ScanStatus::Running);
    let indexed2 = {
        let s = db.stats().unwrap();
        (s.files + s.dirs) as usize
    };
    assert!(indexed2 >= indexed1 && indexed2 < total);

    // Run 3: resumes, finishes, does not mark anything seen earlier as missing.
    let rep3 = Indexer::new(&db).batch_size(40).index_root(rid).unwrap();
    assert_eq!(rep3.outcome, IndexOutcome::Completed);
    assert!(rep3.resumed);
    assert!(
        rep3.dirs < 25,
        "resume must not re-list everything (listed {})",
        rep3.dirs
    );
    assert_eq!(rep3.marked_missing, 0);
    let s = db.stats().unwrap();
    assert_eq!((s.files + s.dirs) as usize, total);
    assert_eq!(s.offline, 0);
    assert_eq!(
        db.index_state(rid).unwrap().unwrap().status,
        ScanStatus::Complete
    );

    // Next run is a fresh pass.
    let rep4 = Indexer::new(&db).index_root(rid).unwrap();
    assert!(!rep4.resumed);
    assert_eq!(rep4.items as usize, total);
}

#[test]
fn annotations_survive_missing_and_relink() {
    let tmp = tempfile::tempdir().unwrap();
    let old = tmp.path().join("Card A");
    let new = tmp.path().join("Archive").join("Card A (2024)");
    touch(&old.join("DCIM/IMG_0001.CR3"), 5);
    touch(&old.join("DCIM/IMG_0002.CR3"), 5);
    touch(&old.join("DCIM/clip.mp4"), 5);
    let db = Db::open(tmp.path().join("forge.db")).unwrap();
    let rid = db.add_index_root(&old).unwrap();
    Indexer::new(&db).index_root(rid).unwrap();

    let id1 = db.file_id(&old.join("DCIM/IMG_0001.CR3")).unwrap().unwrap();
    let id2 = db.file_id(&old.join("DCIM/IMG_0002.CR3")).unwrap().unwrap();
    let idv = db.file_id(&old.join("DCIM/clip.mp4")).unwrap().unwrap();
    db.annotate(
        &[id1, id2],
        &AnnotationChange {
            rating: Some(5),
            label: Some(Some(ColorLabel::Purple)),
            pick: Some(PickState::Pick),
            add_tags: vec!["wedding".into()],
            ..Default::default()
        },
    )
    .unwrap();
    let coll = db.create_manual_collection("Keepers").unwrap();
    db.add_to_collection(coll, &[id2, id1]).unwrap();

    // The folder moves away outside ZXraw Explorer; a pass marks everything missing.
    fs::create_dir_all(new.parent().unwrap()).unwrap();
    fs::rename(&old, &new).unwrap();
    let rep = Indexer::new(&db).index_root(rid).unwrap();
    assert!(matches!(rep.outcome, IndexOutcome::RootUnavailable(_)));
    assert_eq!(count(&db, "rating:5"), 0, "offline files hidden by default");
    assert_eq!(
        db.count(
            &Query::parse("rating:5 tag:wedding").unwrap(),
            &offline_opts()
        )
        .unwrap(),
        2
    );
    assert_eq!(db.annotation(id1).unwrap().rating, 5);

    // Meanwhile the new location was indexed (one row already exists there) …
    Indexer::new(&db).reconcile_dir(&new.join("DCIM")).unwrap();
    let new_id_v = db.file_id(&new.join("DCIM/clip.mp4")).unwrap().unwrap();
    assert_ne!(new_id_v, idv);
    db.set_label(&[new_id_v], Some(ColorLabel::Green)).unwrap();
    db.set_rating(&[idv], 2).unwrap();

    // … relink rewrites the old rows; existing rows at the target absorb the old ones.
    let r = db.relink(&old, &new, None).unwrap();
    assert!(r.moved >= 1, "{r:?}");
    assert!(r.merged >= 3, "{r:?}"); // DCIM dir + 3 files already reconciled
    let a1 = db
        .file_by_path(&new.join("DCIM/IMG_0001.CR3"))
        .unwrap()
        .unwrap();
    assert!(a1.present);
    assert_eq!(a1.rating, 5);
    assert_eq!(a1.label, Some(ColorLabel::Purple));
    assert_eq!(a1.pick, PickState::Pick);
    assert_eq!(
        db.annotation(a1.id).unwrap().tags,
        vec!["wedding".to_string()]
    );
    // Existing annotation at the target wins; nothing duplicated.
    let v = db
        .file_by_path(&new.join("DCIM/clip.mp4"))
        .unwrap()
        .unwrap();
    assert_eq!(v.label, Some(ColorLabel::Green));
    assert_eq!(db.stats().unwrap().files, 3);
    // Collection order and membership preserved through the merge.
    let members: Vec<String> = db
        .collection_files(coll, &SearchOptions::default())
        .unwrap()
        .into_iter()
        .map(|f| f.name)
        .collect();
    assert_eq!(members, ["IMG_0002.CR3", "IMG_0001.CR3"]);
    assert!(
        db.file_by_path(&old.join("DCIM/IMG_0001.CR3"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn relink_simple_prefix_rewrite_then_present_again() {
    let tmp = tempfile::tempdir().unwrap();
    let old = tmp.path().join("E_drive");
    let new = tmp.path().join("F_drive");
    touch(&old.join("a/b/photo.jpg"), 3);
    let db = Db::open_in_memory().unwrap();
    let rid = db.add_index_root(&old).unwrap();
    Indexer::new(&db).index_root(rid).unwrap();
    let id = db.file_id(&old.join("a/b/photo.jpg")).unwrap().unwrap();
    db.set_rating(&[id], 4).unwrap();
    fs::rename(&old, &new).unwrap();
    db.mark_missing(std::slice::from_ref(&old)).unwrap();
    assert!(!db.file(id).unwrap().unwrap().present);

    let r = db.relink(&old, &new, None).unwrap();
    assert_eq!(r.merged, 0);
    assert_eq!(r.moved, 4); // root, a, b, photo
    let rec = db.file(id).unwrap().unwrap();
    assert_eq!(rec.path, new.join("a").join("b").join("photo.jpg"));
    assert_eq!(rec.parent_path, new.join("a").join("b"));
    // The destination exists on disk after the rename, so relink marks the row present
    // straight away; nothing is reported offline.
    assert!(rec.present);
    assert!(r.offline_paths.is_empty());
    let rid2 = db.add_index_root(&new).unwrap();
    Indexer::new(&db).index_root(rid2).unwrap();
    let rec = db.file(id).unwrap().unwrap();
    assert!(rec.present);
    assert_eq!(rec.rating, 4);
    assert!(db.relink(&new, &new.join("sub"), None).is_err());
}

#[test]
fn deleted_files_become_offline_and_return() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    touch(&root.join("x/1.jpg"), 1);
    touch(&root.join("x/2.jpg"), 1);
    touch(&root.join("y/3.jpg"), 1);
    let db = Db::open_in_memory().unwrap();
    let rid = db.add_index_root(&root).unwrap();
    Indexer::new(&db).index_root(rid).unwrap();
    let id2 = db.file_id(&root.join("x/2.jpg")).unwrap().unwrap();
    db.add_tags(&[id2], &["keep"]).unwrap();

    fs::remove_file(root.join("x/2.jpg")).unwrap();
    fs::remove_dir_all(root.join("y")).unwrap();
    let rep = Indexer::new(&db).index_root(rid).unwrap();
    assert_eq!(rep.marked_missing, 3); // 2.jpg, y, 3.jpg
    assert_eq!(count(&db, "is:offline"), 3);

    touch(&root.join("x/2.jpg"), 1);
    Indexer::new(&db).index_root(rid).unwrap();
    let r = db.file(id2).unwrap().unwrap();
    assert!(r.present);
    assert_eq!(db.annotation(id2).unwrap().tags, vec!["keep".to_string()]);
}

#[test]
fn reconcile_single_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("d");
    touch(&dir.join("a.jpg"), 1);
    touch(&dir.join("sub/deep/b.jpg"), 1);
    let db = Db::open_in_memory().unwrap();
    let rid = db.add_index_root(&dir).unwrap();
    Indexer::new(&db).index_root(rid).unwrap();

    touch(&dir.join("new.png"), 1);
    fs::remove_file(dir.join("a.jpg")).unwrap();
    fs::remove_dir_all(dir.join("sub")).unwrap();
    let rep = Indexer::new(&db).reconcile_dir(&dir).unwrap();
    assert_eq!(rep.upserted, 2); // the folder itself + new.png
    assert_eq!(rep.marked_missing, 4); // a.jpg, sub, sub\deep, b.jpg
    assert_eq!(count(&db, "kind:file"), 1);
    assert_eq!(count(&db, "new.png"), 1);

    fs::remove_dir_all(&dir).unwrap();
    let rep = Indexer::new(&db).reconcile_dir(&dir).unwrap();
    assert_eq!(rep.marked_missing, 2);
    assert_eq!(count(&db, ""), 0);
}

#[test]
fn rows_refreshed_during_a_pass_are_not_swept() {
    // A file added via reconcile while an interrupted pass is pending must survive the
    // end-of-pass sweep even though the pass itself never saw it.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    build_tree(&root);
    let db = Db::open_in_memory().unwrap();
    let rid = db.add_index_root(&root).unwrap();
    let token = CancelToken::new();
    let t2 = token.clone();
    Indexer::new(&db)
        .batch_size(30)
        .cancel_token(token)
        .on_batch(move |_| t2.cancel())
        .index_root(rid)
        .unwrap();
    // Every folder (some already passed by the interrupted run, some not) gets a new
    // file that is reconciled outside the indexer.
    for d in 0..12 {
        let dir = root.join(format!("Папка_{d:02}")).join("日本語");
        touch(&dir.join("late.jpg"), 1);
        Indexer::new(&db).reconcile_dir(&dir).unwrap();
    }
    let rep = Indexer::new(&db).index_root(rid).unwrap();
    assert!(rep.resumed);
    assert_eq!(rep.marked_missing, 0);
    assert_eq!(count(&db, "name:late.jpg"), 12);
}

#[test]
fn cancel_progress_and_unavailable_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    build_tree(&root);
    let db = Db::open_in_memory().unwrap();
    let rid = db.add_index_root(&root).unwrap();
    let token = CancelToken::new();
    token.cancel();
    let ix = Indexer::new(&db).cancel_token(token);
    let progress = ix.progress_handle();
    let mut ix = ix;
    let rep = ix.run().unwrap();
    assert!(rep.cancelled());
    assert_eq!(progress.snapshot().items_done, 0);

    let mut ix = Indexer::new(&db);
    let progress = ix.progress_handle();
    let rep = ix.run().unwrap();
    assert_eq!(rep.roots[0].outcome, IndexOutcome::Completed);
    assert_eq!(progress.snapshot().items_done, 24 + 300);

    fs::remove_dir_all(&root).unwrap();
    let rep = Indexer::new(&db).index_root(rid).unwrap();
    assert!(matches!(rep.outcome, IndexOutcome::RootUnavailable(_)));
    assert_eq!(rep.marked_missing, 1 + 24 + 300);
    assert_eq!(
        db.index_state(rid).unwrap().unwrap().status,
        ScanStatus::Unavailable
    );
}

#[cfg(windows)]
#[test]
fn junctions_are_recorded_but_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    let outside = tmp.path().join("outside");
    touch(&root.join("real.jpg"), 1);
    touch(&outside.join("secret.jpg"), 1);
    let link: PathBuf = root.join("jump");
    let ok = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&outside)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("skipping: could not create a junction");
        return;
    }
    let db = Db::open_in_memory().unwrap();
    let rid = db.add_index_root(&root).unwrap();
    Indexer::new(&db).index_root(rid).unwrap();
    assert_eq!(count(&db, "kind:link name:jump"), 1);
    assert_eq!(count(&db, "secret"), 0);
}
