//! File notes: storage rules, `has_notes`, survival through relink (rename, folder move,
//! merge into an existing row) and search (`note:`, `notes:`, plain text, `has:`/`missing:`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use forge_core::model::EntryKind;
use forge_db::{Db, NewFile, SearchOptions};

fn file(p: &str) -> NewFile {
    NewFile {
        path: PathBuf::from(p),
        kind: EntryKind::File,
        size: 1024,
        mtime: Some(1_700_000_000_000),
        ctime: Some(1_700_000_000_000),
    }
}

/// Index one file at `p` and return its row id.
fn add(db: &Db, p: &str) -> i64 {
    db.upsert_files(&[file(p)]).unwrap()[0]
}

fn names(db: &Db, q: &str) -> BTreeSet<String> {
    db.search_str(q, &SearchOptions::default())
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .into_iter()
        .map(|r| r.name)
        .collect()
}

fn set(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn notes_round_trip_and_blank_means_none() {
    let db = Db::open_in_memory().unwrap();
    let id = add(&db, r"D:\Shoot\a.jpg");
    assert_eq!(db.annotation(id).unwrap().notes, None);
    assert!(!db.file(id).unwrap().unwrap().has_notes);

    db.set_notes(&[id], Some("Client proof")).unwrap();
    assert_eq!(db.annotation(id).unwrap().notes.as_deref(), Some("Client proof"));
    let rec = db.file(id).unwrap().unwrap();
    assert!(rec.has_notes);
    let dir = db.children(Path::new(r"D:\Shoot"), false).unwrap();
    assert_eq!(dir.len(), 1);
    assert!(dir[0].has_notes);

    // Blank and whitespace-only notes are stored as no note.
    db.set_notes(&[id], Some("   ")).unwrap();
    assert_eq!(db.annotation(id).unwrap().notes, None);
    assert!(!db.file(id).unwrap().unwrap().has_notes);

    db.set_notes(&[id], Some("again")).unwrap();
    db.set_notes(&[id], None).unwrap();
    assert_eq!(db.annotation(id).unwrap().notes, None);
    assert!(!db.children(Path::new(r"D:\Shoot"), false).unwrap()[0].has_notes);
}

#[test]
fn note_survives_single_file_rename_with_rating() {
    let db = Db::open_in_memory().unwrap();
    let old = add(&db, r"D:\Old\a.jpg");
    db.set_rating(&[old], 4).unwrap();
    db.set_notes(&[old], Some("keep me")).unwrap();

    let report = db
        .relink(Path::new(r"D:\Old\a.jpg"), Path::new(r"D:\New\b.jpg"), None)
        .unwrap();
    assert_eq!(report.moved, 1);
    assert_eq!(report.merged, 0);
    assert!(report.conflicts.is_empty());

    let new = db.file_id(Path::new(r"D:\New\b.jpg")).unwrap().unwrap();
    assert_eq!(new, old);
    let ann = db.annotation(new).unwrap();
    assert_eq!(ann.notes.as_deref(), Some("keep me"));
    assert_eq!(ann.rating, 4);
    assert!(db.file_id(Path::new(r"D:\Old\a.jpg")).unwrap().is_none());
}

#[test]
fn note_survives_folder_move() {
    let db = Db::open_in_memory().unwrap();
    let a = add(&db, r"D:\Old\Trip\a.jpg");
    let b = add(&db, r"D:\Old\Trip\Day 2\b.jpg");
    db.set_notes(&[a], Some("top shot")).unwrap();
    db.set_notes(&[b], Some("second take")).unwrap();

    let report = db
        .relink(Path::new(r"D:\Old\Trip"), Path::new(r"D:\Moved\Trip"), None)
        .unwrap();
    assert_eq!(report.moved, 2);
    assert_eq!(report.merged, 0);

    let a2 = db.file_id(Path::new(r"D:\Moved\Trip\a.jpg")).unwrap().unwrap();
    let b2 = db
        .file_id(Path::new(r"D:\Moved\Trip\Day 2\b.jpg"))
        .unwrap()
        .unwrap();
    assert_eq!(db.annotation(a2).unwrap().notes.as_deref(), Some("top shot"));
    assert_eq!(db.annotation(b2).unwrap().notes.as_deref(), Some("second take"));
}

#[test]
fn relink_into_existing_row_merges_notes() {
    let db = Db::open_in_memory().unwrap();
    let src = add(&db, r"D:\Old\a.jpg");
    let dst = add(&db, r"D:\Dst\a.jpg");
    db.set_rating(&[src], 4).unwrap();
    db.set_notes(&[src], Some("source note")).unwrap();
    db.set_notes(&[dst], Some("destination note")).unwrap();

    let report = db
        .relink(Path::new(r"D:\Old\a.jpg"), Path::new(r"D:\Dst\a.jpg"), None)
        .unwrap();
    assert_eq!(report.merged, 1);
    assert_eq!(report.moved, 0);
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].field, "notes");

    // Both notes are kept: the source first, then the destination, separated by a marker.
    let ann = db.annotation(dst).unwrap();
    assert_eq!(
        ann.notes.as_deref(),
        Some("source note\n\n--- merged note ---\n\ndestination note")
    );
    assert_eq!(ann.rating, 4);
    assert!(db.file(src).unwrap().is_none());
}

#[test]
fn relink_into_existing_row_keeps_the_only_note() {
    let db = Db::open_in_memory().unwrap();
    let src = add(&db, r"D:\Old\a.jpg");
    let dst = add(&db, r"D:\Dst\a.jpg");
    db.set_notes(&[src], Some("only in source")).unwrap();

    let report = db
        .relink(Path::new(r"D:\Old\a.jpg"), Path::new(r"D:\Dst\a.jpg"), None)
        .unwrap();
    assert_eq!(report.merged, 1);
    assert!(report.conflicts.is_empty());
    assert_eq!(
        db.annotation(dst).unwrap().notes.as_deref(),
        Some("only in source")
    );
}

#[test]
fn search_matches_notes_and_has_missing_notes() {
    let db = Db::open_in_memory().unwrap();
    add(&db, r"D:\Shoot\IMG_001.jpg");
    add(&db, r"D:\Shoot\IMG_002.jpg");
    add(&db, r"D:\Shoot\IMG_003.jpg");
    add(&db, r"D:\Shoot\proof_scan.jpg");
    let ids = db
        .file_ids(&[PathBuf::from(r"D:\Shoot\IMG_001.jpg"), PathBuf::from(r"D:\Shoot\IMG_002.jpg")])
        .unwrap();
    for (path, id) in &ids {
        let note = if path.ends_with("IMG_001.jpg") {
            "Client proof"
        } else {
            "second take"
        };
        db.set_notes(&[*id], Some(note)).unwrap();
    }

    // Field-scoped note search.
    assert_eq!(names(&db, "note:proof"), set(&["IMG_001.jpg"]));
    assert_eq!(names(&db, "notes:proof"), set(&["IMG_001.jpg"]));
    assert_eq!(names(&db, "notes:\"second take\""), set(&["IMG_002.jpg"]));

    // Plain text matches notes and paths.
    assert_eq!(names(&db, "proof"), set(&["IMG_001.jpg", "proof_scan.jpg"]));
    assert_eq!(names(&db, "second"), set(&["IMG_002.jpg"]));
    assert_eq!(names(&db, "IMG_003"), set(&["IMG_003.jpg"]));

    // Path filter stays path-only.
    assert_eq!(names(&db, "path:proof"), set(&["proof_scan.jpg"]));

    // Presence of notes.
    assert_eq!(
        names(&db, "has:notes"),
        set(&["IMG_001.jpg", "IMG_002.jpg"])
    );
    assert_eq!(
        names(&db, "missing:notes"),
        set(&["IMG_003.jpg", "proof_scan.jpg"])
    );
}
