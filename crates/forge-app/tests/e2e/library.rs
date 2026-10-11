//! Library scenarios: collections (including moved folders, relink and JSON backup), search
//! after indexing, tags and notes, XMP sidecars, settings and resumable indexing.
//!
//! Scenarios that need an app restart run two or more windows on the same profile, so the
//! assertions see what was persisted, not what memory held. Assertions on persisted state
//! read the library database after the app has exited.

use std::fs;
use std::path::{Path, PathBuf};

use forge_db::{Db, Indexer, SearchOptions};
use forge_jobs::CancelToken;

use crate::*;

/// Screenshots of the library scenarios: `%TEMP%\zxfr-shots\library\`.
fn shot(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("zxfr-shots").join("library");
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// The library database the app keeps in its profile.
fn library(profile: &Path) -> Db {
    Db::open(profile.join("data").join("forge.db")).expect("open the library database")
}

fn photos(dir: &Path, names: &[&str]) {
    for (i, name) in names.iter().enumerate() {
        write_file(&dir.join(name), &noise(4096, i as u32 + 1));
    }
}

fn file_names(paths: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = paths
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn collection_members(db: &Db, name: &str) -> Vec<PathBuf> {
    let id = db
        .collection_by_name(name)
        .unwrap()
        .unwrap_or_else(|| panic!("collection `{name}` is missing"))
        .id;
    db.collection_files(
        id,
        &SearchOptions {
            include_offline: true,
            limit: None,
            ..Default::default()
        },
    )
    .unwrap()
    .into_iter()
    .map(|r| r.path)
    .collect()
}

/// Writes a JSON backup by hand, so the tests control every field of the file.
fn backup_json(collections: &[(&str, &[&str])]) -> String {
    let collections: Vec<serde_json::Value> = collections
        .iter()
        .map(|(name, items)| serde_json::json!({ "name": name, "kind": "manual", "items": items }))
        .collect();
    serde_json::json!({
        "format": "forge-collections",
        "version": 1,
        "exported_at": 0,
        "collections": collections,
    })
    .to_string()
}

/// Index-root state for `root`, read from the library.
fn index_state_of(db: &Db, root: &Path) -> Option<forge_db::IndexState> {
    let root_id = db
        .index_roots()
        .unwrap()
        .into_iter()
        .find(|r| {
            r.path
                .to_string_lossy()
                .eq_ignore_ascii_case(&root.to_string_lossy())
        })?
        .id;
    db.index_state(root_id).unwrap()
}

#[test]
fn collection_holds_the_selected_files_and_lists_them_when_opened() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("shoot");
    photos(&dir, &["a.jpg", "b.jpg", "c.jpg"]);
    let profile = sandbox.path().join("profile");
    let script = format!(
        "navigate {dir}\n\
         select a.jpg, b.jpg\n\
         ui library collection-new Keepers\n\
         ui library collection-open Keepers\n\
         ui library collection-shown Keepers | a.jpg, b.jpg\n\
         ui library collection-assert Keepers | a.jpg, b.jpg\n\
         screenshot {shot}\n\
         exit\n",
        dir = shown(&dir),
        shot = shown(&shot("collection-open.png")),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    assert!(
        shot("collection-open.png").is_file(),
        "no screenshot was saved"
    );
    let members = collection_members(&library(&profile), "Keepers");
    assert_eq!(file_names(&members), ["a.jpg", "b.jpg"]);
    assert!(members.iter().all(|p| p.starts_with(&dir)));
}

#[test]
fn a_collection_assertion_that_does_not_hold_fails_the_script() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("shoot");
    photos(&dir, &["a.jpg", "b.jpg"]);
    let script = format!(
        "navigate {dir}\n\
         select a.jpg\n\
         ui library collection-new Keepers\n\
         ui library collection-assert Keepers | b.jpg\n\
         exit\n",
        dir = shown(&dir),
    );

    let outcome = run_app_with(
        sandbox.path(),
        &sandbox.path().join("profile"),
        &script,
        &RunOpts {
            limit: Some(std::time::Duration::from_secs(90)),
            ..Default::default()
        },
    );

    expect_script_error(&outcome, "timed out");
}

#[test]
fn moved_folder_takes_collection_items_offline_and_relink_brings_them_back() {
    let sandbox = tempfile::tempdir().unwrap();
    let shoot = sandbox.path().join("shoot");
    let moved = sandbox.path().join("moved-shoot");
    photos(&shoot, &["a.jpg", "b.jpg", "c.jpg"]);
    let profile = sandbox.path().join("profile");
    let long = RunOpts {
        limit: Some(std::time::Duration::from_secs(120)),
        ..Default::default()
    };

    // The library learns the files and puts them in a collection.
    let first = format!(
        "navigate {shoot}\n\
         select-all\n\
         ui library collection-new Picks\n\
         ui library index {shoot}\n\
         wait-jobs\n\
         ui library collection-assert Picks | a.jpg, b.jpg, c.jpg\n\
         exit\n",
        shoot = shown(&shoot),
    );
    expect_ok(&run_app_with(sandbox.path(), &profile, &first, &long));

    // The folder moves, as when a library moves to another drive letter.
    fs::rename(&shoot, &moved).unwrap();

    // Re-indexing the old location finds it gone: its files go offline.
    let offline = format!(
        "ui library index {shoot}\n\
         wait-jobs\n\
         ui library collection-open Picks\n\
         ui library collection-shown Picks | a.jpg, b.jpg, c.jpg\n\
         ui library collection-assert-offline Picks | a.jpg, b.jpg, c.jpg\n\
         wait-frames 30\n\
         screenshot {offline}\n\
         exit\n",
        shoot = shown(&shoot),
        offline = shown(&shot("collection-offline.png")),
    );
    expect_ok(&run_app_with(sandbox.path(), &profile, &offline, &long));
    assert!(shot("collection-offline.png").is_file());

    // Relink old -> new through the window's dialogs: the files come back online and point
    // at the new location.
    let relink = format!(
        "dialog {shoot}\n\
         ui library choose relink-old\n\
         dialog {moved}\n\
         ui library choose relink-new\n\
         ui library relink-preview 3\n\
         ui library relink-confirm\n\
         ui library collection-assert-offline Picks | none\n\
         ui library collection-open Picks\n\
         ui library collection-shown Picks | a.jpg, b.jpg, c.jpg\n\
         wait-frames 30\n\
         screenshot {relinked}\n\
         exit\n",
        shoot = shown(&shoot),
        moved = shown(&moved),
        relinked = shown(&shot("collection-relinked.png")),
    );
    expect_ok(&run_app_with(sandbox.path(), &profile, &relink, &long));

    let db = library(&profile);
    let members = collection_members(&db, "Picks");
    assert_eq!(file_names(&members), ["a.jpg", "b.jpg", "c.jpg"]);
    assert!(members.iter().all(|p| p.starts_with(&moved)), "{members:?}");
    assert!(shot("collection-relinked.png").is_file());
    assert_no_crash(&profile);
}

#[test]
fn relink_leaves_files_missing_from_the_new_folder_offline() {
    let sandbox = tempfile::tempdir().unwrap();
    let shoot = sandbox.path().join("shoot");
    let moved = sandbox.path().join("moved-shoot");
    photos(&shoot, &["a.jpg", "b.jpg", "c.jpg"]);
    let profile = sandbox.path().join("profile");
    let first = format!(
        "navigate {shoot}\n\
         select-all\n\
         ui library collection-new Picks\n\
         ui library index {shoot}\n\
         wait-jobs\n\
         exit\n",
        shoot = shown(&shoot),
    );
    expect_ok(&run_app(sandbox.path(), &profile, &first));

    // The folder moves, and one of its photos is lost on the way.
    fs::rename(&shoot, &moved).unwrap();
    fs::remove_file(moved.join("c.jpg")).unwrap();

    let relink = format!(
        "dialog {shoot}\n\
         ui library choose relink-old\n\
         dialog {moved}\n\
         ui library choose relink-new\n\
         ui library relink-preview 3\n\
         ui library relink-confirm\n\
         ui library collection-assert Picks | a.jpg, b.jpg, c.jpg\n\
         ui library collection-assert-offline Picks | c.jpg\n\
         exit\n",
        shoot = shown(&shoot),
        moved = shown(&moved),
    );
    expect_ok(&run_app(sandbox.path(), &profile, &relink));

    let db = library(&profile);
    let rows = db
        .collection_files(
            db.collection_by_name("Picks").unwrap().unwrap().id,
            &SearchOptions {
                include_offline: true,
                limit: None,
                ..Default::default()
            },
        )
        .unwrap();
    let present: Vec<String> = rows
        .iter()
        .filter(|r| r.present)
        .map(|r| r.name.clone())
        .collect();
    let mut present = present;
    present.sort();
    assert_eq!(present, ["a.jpg", "b.jpg"]);
    let c = rows.iter().find(|r| r.name == "c.jpg").unwrap();
    assert!(!c.present);
    assert!(
        c.path.starts_with(&moved),
        "the missing file still moves with the folder"
    );
}

#[test]
fn exported_collection_imports_into_a_fresh_library_as_offline_placeholders() {
    let sandbox = tempfile::tempdir().unwrap();
    let shoot = sandbox.path().join("shoot");
    photos(&shoot, &["a.jpg", "b.jpg"]);
    let export = sandbox.path().join("keepers.json");
    let first_profile = sandbox.path().join("profile-a");
    let first = format!(
        "navigate {shoot}\n\
         select-all\n\
         ui library collection-new Keepers\n\
         dialog {export}\n\
         ui library choose export Keepers\n\
         wait-exists {export}\n\
         exit\n",
        shoot = shown(&shoot),
        export = shown(&export),
    );
    expect_ok(&run_app(sandbox.path(), &first_profile, &first));
    let json = fs::read_to_string(&export).unwrap();
    assert!(json.contains("forge-collections") && json.contains("a.jpg"));

    // A second profile that has never seen the photos: they come back as offline items.
    let second_profile = sandbox.path().join("profile-b");
    let second = format!(
        "dialog {export}\n\
         ui library choose import\n\
         ui library collection-assert Keepers | a.jpg, b.jpg\n\
         ui library collection-assert-offline Keepers | a.jpg, b.jpg\n\
         wait-frames 30\n\
         assert-toast Imported\n\
         exit\n",
        export = shown(&export),
    );
    expect_ok(&run_app(sandbox.path(), &second_profile, &second));
    let db = library(&second_profile);
    assert_eq!(collection_members(&db, "Keepers").len(), 2);
}

#[test]
fn hostile_or_malformed_import_is_rejected_without_partial_writes() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let malformed = sandbox.path().join("malformed.json");
    let traversal = sandbox.path().join("traversal.json");
    fs::write(
        &malformed,
        "{\"format\": \"forge-collections\", \"collections\": [",
    )
    .unwrap();
    fs::write(
        &traversal,
        backup_json(&[
            ("Good", &[r"C:\photos\safe.jpg"]),
            ("Bad", &[r"C:\photos\..\..\Windows\escape.jpg"]),
        ]),
    )
    .unwrap();
    let script = format!(
        "dialog {malformed}\n\
         ui library choose import\n\
         dialog {traversal}\n\
         ui library choose import\n\
         wait-frames 30\n\
         assert-toast unsafe or invalid collection item path\n\
         exit\n",
        malformed = shown(&malformed),
        traversal = shown(&traversal),
    );

    let outcome = run_app(sandbox.path(), &profile, &script);

    expect_ok(&outcome);
    assert_no_crash(&profile);
    let db = library(&profile);
    assert!(
        db.collections().unwrap().is_empty(),
        "a rejected import must not create anything, not even its valid collection"
    );
    assert_eq!(db.stats().unwrap().files, 0);
}

#[test]
fn search_finds_files_by_name_extension_rating_and_tag_after_indexing() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("shots-library");
    let photos_dir = root.join("photos");
    photos(
        &photos_dir,
        &["sunset.jpg", "sunrise.png", "portrait.jpg", "notes.txt"],
    );
    photos(&photos_dir.join("sub"), &["beach.jpg"]);
    let script = format!(
        "ui library index {root}\n\
         wait-jobs\n\
         navigate {photos}\n\
         select sunset.jpg\n\
         ui library tag add sunny\n\
         ui library rate 5\n\
         ui library assert-rating {sunset} | 5\n\
         ui library assert-tags {sunset} | sunny\n\
         ui library search sun\n\
         ui library assert-results sunrise.png, sunset.jpg\n\
         ui library search ext:jpg\n\
         ui library assert-results sunset.jpg, portrait.jpg, beach.jpg\n\
         screenshot {shot}\n\
         ui library search ext:jpg rating:>=4\n\
         ui library assert-results sunset.jpg\n\
         ui library search tag:sunny\n\
         ui library assert-results sunset.jpg\n\
         ui library search ext:txt\n\
         ui library assert-results notes.txt\n\
         ui library search no-such-file-anywhere\n\
         ui library assert-result-count 0\n\
         exit\n",
        root = shown(&root),
        photos = shown(&photos_dir),
        sunset = shown(&photos_dir.join("sunset.jpg")),
        shot = shown(&shot("search-ext.png")),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert!(shot("search-ext.png").is_file());
}

#[test]
fn search_with_special_characters_matches_literally_and_never_errors() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("shots-library");
    photos(
        &root,
        &[
            "100%_done.jpg",
            "Фото [1].jpg",
            "it's here.jpg",
            "plain.jpg",
        ],
    );
    let script = format!(
        "ui library index {root}\n\
         wait-jobs\n\
         ui library search %\n\
         ui library assert-results 100%_done.jpg\n\
         ui library search _\n\
         ui library assert-results 100%_done.jpg\n\
         ui library search %%\n\
         ui library assert-results none\n\
         ui library search Фото\n\
         ui library assert-results Фото [1].jpg\n\
         ui library search [1]\n\
         ui library assert-results Фото [1].jpg\n\
         ui library search '\n\
         ui library assert-results it's here.jpg\n\
         exit\n",
        root = shown(&root),
    );

    let outcome = run_app(sandbox.path(), &sandbox.path().join("profile"), &script);

    expect_ok(&outcome);
    assert_no_crash(&sandbox.path().join("profile"));
}

#[test]
fn reindex_shows_an_added_file_and_drops_a_deleted_one() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("shots-library");
    photos(&root, &["a.jpg", "b.jpg", "c.jpg"]);
    let profile = sandbox.path().join("profile");
    let first = format!(
        "ui library index {root}\n\
         wait-jobs\n\
         ui library search ext:jpg\n\
         ui library assert-results a.jpg, b.jpg, c.jpg\n\
         exit\n",
        root = shown(&root),
    );
    expect_ok(&run_app(sandbox.path(), &profile, &first));

    // Changed outside the app: one file added, one deleted.
    write_file(&root.join("d.jpg"), &noise(2048, 9));
    fs::remove_file(root.join("a.jpg")).unwrap();

    let second = format!(
        "ui library index {root}\n\
         wait-jobs\n\
         ui library search ext:jpg\n\
         ui library assert-results b.jpg, c.jpg, d.jpg\n\
         ui library search a.jpg\n\
         ui library assert-results none\n\
         exit\n",
        root = shown(&root),
    );
    expect_ok(&run_app(sandbox.path(), &profile, &second));
    assert_no_crash(&profile);
}

#[test]
fn tags_and_notes_persist_across_a_restart_and_stay_searchable() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("shoot");
    photos(&dir, &["a.jpg", "b.jpg", "c.jpg"]);
    let profile = sandbox.path().join("profile");
    let first = format!(
        "navigate {dir}\n\
         select a.jpg, b.jpg\n\
         ui library tag add Wedding, Family\n\
         ui library tag add wedding\n\
         ui library tag add ,  ,\n\
         ui library note Client proof\n\
         ui library tag remove Family\n\
         ui library assert-tags {a} | Wedding\n\
         ui library assert-tags {b} | Wedding\n\
         ui library assert-note {a} | Client proof\n\
         exit\n",
        dir = shown(&dir),
        a = shown(&dir.join("a.jpg")),
        b = shown(&dir.join("b.jpg")),
    );
    expect_ok(&run_app(sandbox.path(), &profile, &first));

    let db = library(&profile);
    let a_id = db.file_id(&dir.join("a.jpg")).unwrap().unwrap();
    let persisted = db.annotation(a_id).unwrap();
    assert_eq!(
        persisted.tags,
        ["Wedding"],
        "duplicates are merged case-insensitively"
    );
    assert_eq!(persisted.notes.as_deref(), Some("Client proof"));

    // Restart with the same profile.
    let second = format!(
        "ui library assert-tags {a} | Wedding\n\
         ui library assert-tags {b} | Wedding\n\
         ui library assert-note {a} | Client proof\n\
         ui library search tag:Wedding\n\
         ui library assert-results a.jpg, b.jpg\n\
         ui library search notes:proof\n\
         ui library assert-results a.jpg, b.jpg\n\
         exit\n",
        a = shown(&dir.join("a.jpg")),
        b = shown(&dir.join("b.jpg")),
    );
    expect_ok(&run_app(sandbox.path(), &profile, &second));
    assert_no_crash(&profile);
}

#[test]
fn tag_length_limit_is_enforced_and_reported() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("shoot");
    photos(&dir, &["a.jpg"]);
    let profile = sandbox.path().join("profile");
    let longest = "t".repeat(256);
    let too_long = "t".repeat(257);
    let script = format!(
        "navigate {dir}\n\
         select a.jpg\n\
         ui library tag add {longest}\n\
         ui library assert-tags {a} | {longest}\n\
         ui library tag add {too_long}\n\
         sleep 500\n\
         assert-toast exceeds 256 characters\n\
         exit\n",
        dir = shown(&dir),
        a = shown(&dir.join("a.jpg")),
    );
    expect_ok(&run_app(sandbox.path(), &profile, &script));

    let db = library(&profile);
    let a_id = db.file_id(&dir.join("a.jpg")).unwrap().unwrap();
    assert_eq!(db.annotation(a_id).unwrap().tags, [longest]);
}

#[test]
fn xmp_sync_off_writes_no_sidecar_and_on_writes_one_without_touching_the_photo() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("shoot");
    photos(&dir, &["off.jpg", "on.jpg"]);
    let profile = sandbox.path().join("profile");
    let media_before = fs::read(dir.join("on.jpg")).unwrap();

    // Default: sync is off, so tagging the file must not create a sidecar.
    let off = format!(
        "navigate {dir}\n\
         select off.jpg\n\
         ui library tag add Wedding\n\
         ui library assert-tags {off} | Wedding\n\
         exit\n",
        dir = shown(&dir),
        off = shown(&dir.join("off.jpg")),
    );
    expect_ok(&run_app(sandbox.path(), &profile, &off));
    assert!(!dir.join("off.xmp").exists(), "sidecars are opt-in");

    // Turn sync on through the settings, then tag and rate a file.
    let on = format!(
        "ui library setting xmp_sidecar_sync true\n\
         navigate {dir}\n\
         select on.jpg\n\
         ui library tag add Wedding\n\
         ui library rate 4\n\
         ui library assert-rating {photo} | 4\n\
         wait-frames 60\n\
         assert-no-error-toast\n\
         wait-exists {sidecar}\n\
         exit\n",
        dir = shown(&dir),
        photo = shown(&dir.join("on.jpg")),
        sidecar = shown(&dir.join("on.xmp")),
    );
    expect_ok(&run_app(sandbox.path(), &profile, &on));

    let sidecar = fs::read_to_string(dir.join("on.xmp")).unwrap();
    assert!(
        sidecar.contains("Wedding"),
        "keyword missing from the sidecar"
    );
    assert_eq!(
        fs::read(dir.join("on.jpg")).unwrap(),
        media_before,
        "the photo's bytes must not change"
    );
    assert!(!dir.join("off.xmp").exists());
}

#[test]
fn settings_changed_in_one_session_are_restored_on_the_next_start() {
    let sandbox = tempfile::tempdir().unwrap();
    let profile = sandbox.path().join("profile");
    let first = "ui library setting theme light\n\
                 ui library setting show_hidden true\n\
                 ui library setting show_system true\n\
                 ui library setting default_view grid\n\
                 ui library setting thumb_size 220\n\
                 ui library setting xmp_sidecar_sync true\n\
                 exit\n";
    expect_ok(&run_app(sandbox.path(), &profile, first));

    let second = "ui library assert-setting theme light\n\
                  ui library assert-setting show_hidden true\n\
                  ui library assert-setting show_system true\n\
                  ui library assert-setting default_view grid\n\
                  ui library assert-setting thumb_size 220\n\
                  ui library assert-setting xmp_sidecar_sync true\n\
                  exit\n";
    expect_ok(&run_app(sandbox.path(), &profile, second));
    assert_no_crash(&profile);
}

#[test]
fn invalid_setting_values_fail_the_script_instead_of_being_saved() {
    let sandbox = tempfile::tempdir().unwrap();
    let outcome = run_app(
        sandbox.path(),
        &sandbox.path().join("profile"),
        "ui library setting thumb_size huge\nexit\n",
    );
    expect_script_error(&outcome, "takes a number");
}

/// Writes `count` tiny files into `folders` subfolders of `root`.
fn many_files(root: &Path, folders: usize, per_folder: usize) {
    for d in 0..folders {
        let sub = root.join(format!("f{d:03}"));
        fs::create_dir_all(&sub).unwrap();
        for f in 0..per_folder {
            let n = d * per_folder + f;
            fs::write(sub.join(format!("file_{n:06}.txt")), b"x").unwrap();
        }
    }
}

/// Starts an index pass in the library and stops it after `batches` committed batches, as
/// an interrupted run would leave it.
fn interrupt_index(profile: &Path, root: &Path, batches: usize) {
    let db = library(profile);
    let root_id = db.add_index_root(root).unwrap();
    let token = CancelToken::new();
    let stop = token.clone();
    let mut seen = 0;
    let mut indexer = Indexer::new(&db)
        .cancel_token(token)
        .batch_size(500)
        .on_batch(move |_| {
            seen += 1;
            if seen == batches {
                stop.cancel();
            }
        });
    let report = indexer.index_root(root_id).unwrap();
    assert_eq!(report.outcome, forge_db::IndexOutcome::Cancelled);
}

#[test]
fn interrupted_index_resumes_on_the_next_start_and_finishes_the_whole_tree() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("big-library");
    many_files(&root, 300, 100);
    let profile = sandbox.path().join("profile");
    let long = RunOpts {
        limit: Some(std::time::Duration::from_secs(170)),
        ..Default::default()
    };

    // Start the index and exit while part of the tree is still unread.
    let first = format!(
        "ui library index {root}\n\
         ui library wait-files 1000 30000\n\
         exit\n",
        root = shown(&root),
    );
    expect_ok(&run_app_with(sandbox.path(), &profile, &first, &long));
    {
        let db = library(&profile);
        let state = index_state_of(&db, &root).expect("the pass left a checkpoint");
        assert!(state.is_resumable(), "status {:?}", state.status);
        assert!(db.stats().unwrap().files < 30_000);
    }

    // Next start: nothing asks for indexing, yet the pass continues from its checkpoint.
    let second = "wait-jobs 150\n\
                  wait-frames 30\n\
                  assert-toast Resumed and finished indexing\n\
                  ui library search file_029999\n\
                  ui library assert-results file_029999.txt\n\
                  screenshot {shot}\n\
                  exit\n"
        .replace("{shot}", &shown(&shot("resume-finished.png")));
    expect_ok(&run_app_with(sandbox.path(), &profile, &second, &long));

    let db = library(&profile);
    let state = index_state_of(&db, &root).expect("the pass is recorded");
    assert_eq!(state.status, forge_db::ScanStatus::Complete);
    let stats = db.stats().unwrap();
    assert_eq!(stats.files, 30_000, "every file of the tree is indexed");
    assert!(shot("resume-finished.png").is_file());
}

#[test]
fn interrupted_index_whose_folder_is_gone_is_dropped_on_start() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("gone-library");
    many_files(&root, 30, 100);
    let profile = sandbox.path().join("profile");
    interrupt_index(&profile, &root, 2);
    {
        let db = library(&profile);
        assert!(index_state_of(&db, &root).unwrap().is_resumable());
    }

    // The folder is deleted while the app is closed.
    fs::remove_dir_all(&root).unwrap();

    let script = "wait-jobs 30\n\
                  wait-frames 30\n\
                  assert-toast was dropped\n\
                  exit\n";
    expect_ok(&run_app(sandbox.path(), &profile, script));
    assert_no_crash(&profile);
    let db = library(&profile);
    assert!(
        index_state_of(&db, &root).is_none(),
        "the pending pass must be cleared, not kept for every later start"
    );
}

/// An unterminated quote is a query error with a position (by design in `forge-core`'s
/// parser): the search window must report it instead of failing or showing stale results.
#[test]
fn unterminated_quote_in_a_search_is_reported_not_crashed() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join("shots-library");
    photos(&root, &["plain.jpg"]);
    let script = format!(
        "ui library index {root}\n\
         wait-jobs\n\
         ui library search ext:jpg\n\
         ui library assert-results plain.jpg\n\
         ui library search \"\n\
         ui library assert-search-error unterminated quote\n\
         ui library search ext:jpg \"unclosed\n\
         ui library assert-search-error unterminated quote\n\
         exit\n",
        root = shown(&root),
    );

    expect_ok(&run_app(
        sandbox.path(),
        &sandbox.path().join("profile"),
        &script,
    ));
}
