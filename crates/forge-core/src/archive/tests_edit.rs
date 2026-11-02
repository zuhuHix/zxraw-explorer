//! Smart extraction, updates and deletes (atomicity), cancel cleanup.

use super::*;
use forge_jobs::{CancelToken, Progress};
use std::fs::File;
use std::io::Write;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

fn ctx_parts() -> (CancelToken, Progress) {
    (CancelToken::new(), Progress::default())
}

fn cancelled_parts() -> (CancelToken, Progress) {
    let cancel = CancelToken::new();
    cancel.cancel();
    (cancel, Progress::default())
}

/// Write a ZIP. Names ending with `/` are folder entries (their text is ignored).
fn make_zip(path: &Path, entries: &[(&str, &str)]) {
    let mut zw = ZipWriter::new(File::create(path).unwrap());
    for (name, text) in entries {
        if name.ends_with('/') {
            zw.add_directory(*name, SimpleFileOptions::default())
                .unwrap();
        } else {
            zw.start_file(*name, SimpleFileOptions::default()).unwrap();
            zw.write_all(text.as_bytes()).unwrap();
        }
    }
    zw.finish().unwrap();
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

/// Every path under `dir` (relative, `/`-separated, folders end with `/`), sorted.
fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            let rel = p
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if p.is_dir() {
                out.push(format!("{rel}/"));
                walk(base, &p, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn no_partial_files(dir: &Path) -> bool {
    tree(dir).iter().all(|n| !n.contains(".forge-partial-"))
}

fn entry_names(archive: &Path) -> Vec<String> {
    let mut names: Vec<String> = list(archive)
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    names
}

fn edit_add(src: PathBuf, dest: &str) -> (PathBuf, String) {
    (src, dest.to_owned())
}

// ---------------------------------------------------------------------------
// top_level
// ---------------------------------------------------------------------------

#[test]
fn top_level_classifies_the_archive_root() {
    let cases: Vec<(Vec<(&str, &str)>, TopLevel)> = vec![
        (
            vec![("proj/", ""), ("proj/a.txt", "a")],
            TopLevel::Folder("proj".into()),
        ),
        (
            vec![("proj/a.txt", "a"), ("proj/sub/b.txt", "b")],
            TopLevel::Folder("proj".into()),
        ),
        (
            vec![("Proj/", ""), ("proj/a.txt", "a")],
            TopLevel::Folder("Proj".into()),
        ),
        (vec![("empty/", "")], TopLevel::Folder("empty".into())),
        (vec![("note.txt", "n")], TopLevel::File("note.txt".into())),
        (vec![("a.txt", "a"), ("b.txt", "b")], TopLevel::Many),
        (vec![("proj/", ""), ("other.txt", "o")], TopLevel::Many),
        (vec![], TopLevel::Empty),
    ];
    let d = tempfile::tempdir().unwrap();
    for (i, (entries, expected)) in cases.into_iter().enumerate() {
        let z = d.path().join(format!("case{i}.zip"));
        make_zip(&z, &entries);
        let listing = list(&z).unwrap();
        assert_eq!(top_level(&listing), expected, "case {i}: {entries:?}");
    }
}

// ---------------------------------------------------------------------------
// extract_to: Smart and NamedFolder
// ---------------------------------------------------------------------------

#[test]
fn smart_extract_avoids_extra_nesting_and_picks_a_fresh_folder_on_collision() {
    let d = tempfile::tempdir().unwrap();
    let parent = d.path().join("out");
    fs::create_dir(&parent).unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let opts = ExtractOptions::default();

    // One top folder: contents go straight into parent/proj.
    let proj = d.path().join("proj.zip");
    make_zip(
        &proj,
        &[("proj/", ""), ("proj/a.txt", "A"), ("proj/sub/b.txt", "B")],
    );
    let r = extract_to(&proj, &parent, ExtractTarget::Smart, &opts, &ctx).unwrap();
    assert_eq!(r.dest, parent);
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(fs::read(parent.join("proj/a.txt")).unwrap(), b"A");
    assert_eq!(fs::read(parent.join("proj/sub/b.txt")).unwrap(), b"B");
    assert!(!parent.join("proj/proj").exists());

    // One file: extracted as parent/file.txt.
    let note = d.path().join("note.zip");
    make_zip(&note, &[("file.txt", "F")]);
    let r = extract_to(&note, &parent, ExtractTarget::Smart, &opts, &ctx).unwrap();
    assert_eq!(r.dest, parent);
    assert_eq!(fs::read(parent.join("file.txt")).unwrap(), b"F");
    assert!(!parent.join("note").exists());

    // Several top-level items: nested in parent/<stem>.
    let photos = d.path().join("photos.zip");
    make_zip(&photos, &[("a.txt", "1"), ("b/c.txt", "2")]);
    let r = extract_to(&photos, &parent, ExtractTarget::Smart, &opts, &ctx).unwrap();
    assert_eq!(r.dest, parent.join("photos"));
    assert_eq!(fs::read(parent.join("photos/a.txt")).unwrap(), b"1");
    assert_eq!(fs::read(parent.join("photos/b/c.txt")).unwrap(), b"2");

    // parent/album already exists: a new folder is used and the old one is untouched.
    write(&parent.join("album/keep.txt"), "mine");
    let album = d.path().join("album.zip");
    make_zip(&album, &[("x.txt", "x"), ("y.txt", "y")]);
    let expected = parent.join("album (2)");
    assert_eq!(paths::unique_name(&parent, "album"), expected);
    let r = extract_to(&album, &parent, ExtractTarget::Smart, &opts, &ctx).unwrap();
    assert_eq!(r.dest, expected);
    assert_eq!(fs::read(expected.join("x.txt")).unwrap(), b"x");
    assert_eq!(fs::read(expected.join("y.txt")).unwrap(), b"y");
    assert_eq!(tree(&parent.join("album")), vec!["keep.txt"]);
}

#[test]
fn named_folder_always_nests_and_merges_into_an_existing_folder() {
    let d = tempfile::tempdir().unwrap();
    let parent = d.path().join("out");
    write(&parent.join("proj/old.txt"), "old");
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let opts = ExtractOptions::default();

    let proj = d.path().join("proj.zip");
    make_zip(&proj, &[("proj/", ""), ("proj/a.txt", "A")]);
    let r = extract_to(&proj, &parent, ExtractTarget::NamedFolder, &opts, &ctx).unwrap();
    assert_eq!(r.dest, parent.join("proj"));
    assert_eq!(fs::read(parent.join("proj/proj/a.txt")).unwrap(), b"A");
    assert_eq!(fs::read(parent.join("proj/old.txt")).unwrap(), b"old");

    let note = d.path().join("note.zip");
    make_zip(&note, &[("file.txt", "F")]);
    let r = extract_to(&note, &parent, ExtractTarget::NamedFolder, &opts, &ctx).unwrap();
    assert_eq!(r.dest, parent.join("note"));
    assert_eq!(fs::read(parent.join("note/file.txt")).unwrap(), b"F");
}

#[test]
fn cancelled_smart_extract_leaves_no_folder_or_partial_files() {
    let d = tempfile::tempdir().unwrap();
    let parent = d.path().join("out");
    fs::create_dir(&parent).unwrap();
    let photos = d.path().join("photos.zip");
    make_zip(&photos, &[("a.txt", "1"), ("b.txt", "2"), ("c.txt", "3")]);

    let (c, p) = cancelled_parts();
    let ctx = JobCtx::new(&c, &p);
    match extract_to(
        &photos,
        &parent,
        ExtractTarget::Smart,
        &ExtractOptions::default(),
        &ctx,
    ) {
        Ok(r) => assert!(r.cancelled, "{r:?}"),
        Err(CoreError::Cancelled) => {}
        Err(e) => panic!("unexpected error: {e}"),
    }
    assert!(!parent.join("photos").exists());
    assert!(tree(&parent).is_empty(), "{:?}", tree(&parent));
    assert!(no_partial_files(d.path()));
}

// ---------------------------------------------------------------------------
// extract with a selection
// ---------------------------------------------------------------------------

#[test]
fn selection_extracts_relative_to_base_and_reports_missing_names() {
    let d = tempfile::tempdir().unwrap();
    let archive = d.path().join("docs.zip");
    make_zip(
        &archive,
        &[
            ("docs/a/1.txt", "1"),
            ("docs/a/2.txt", "2"),
            ("docs/x.txt", "x"),
            ("docs/y.txt", "y"),
            ("other.txt", "o"),
        ],
    );
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let opts = ExtractOptions::default();

    let sel = Selection::new("docs/", vec!["docs/a/".into(), "docs/x.txt".into()]);
    let dest = d.path().join("dest");
    fs::create_dir(&dest).unwrap();
    let r = extract(&archive, &dest, &opts, Some(&sel), &ctx).unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(r.extracted.len(), 3, "{r:?}");
    assert_eq!(tree(&dest), vec!["a/", "a/1.txt", "a/2.txt", "x.txt"]);
    assert_eq!(fs::read(dest.join("a/2.txt")).unwrap(), b"2");
    assert_eq!(fs::read(dest.join("x.txt")).unwrap(), b"x");

    let sel = Selection::new(
        "docs/",
        vec!["docs/x.txt".into(), "docs/missing.txt".into()],
    );
    let dest2 = d.path().join("dest2");
    fs::create_dir(&dest2).unwrap();
    let r = extract(&archive, &dest2, &opts, Some(&sel), &ctx).unwrap();
    assert_eq!(r.failed.len(), 1, "{r:?}");
    assert_eq!(r.failed[0].path, PathBuf::from("docs/missing.txt"));
    assert_eq!(fs::read(dest2.join("x.txt")).unwrap(), b"x");
    assert_eq!(tree(&dest2), vec!["x.txt"]);
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

#[test]
fn update_add_raw_copies_old_entries_and_writes_new_ones_with_new_options() {
    let d = tempfile::tempdir().unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);

    let a = d.path().join("a.txt");
    write(&a, "old a");
    let sub = d.path().join("sub");
    write(&sub.join("b.txt"), "old b");
    let archive = d.path().join("base.zip");
    let zstd = CreateOptions {
        method: Some((Method::Zstd, Some(3))),
        auto_store: false,
        ..Default::default()
    };
    create(&[a.clone(), sub.clone()], &archive, &zstd, &ctx).unwrap();

    let new_file = d.path().join("new.txt");
    write(&new_file, "new data");
    let extra = d.path().join("extra");
    write(&extra.join("x.txt"), "x data");
    let edit = ArchiveEdit {
        add: vec![edit_add(new_file, ""), edit_add(extra, "sub/")],
        delete: vec![],
    };
    let rep = update(&archive, &edit, &CreateOptions::default(), &ctx).unwrap();
    // Kept: a.txt, sub/, sub/b.txt. Written: new.txt, sub/extra/x.txt and the sub/extra/ folder.
    assert_eq!(rep.kept, 3, "{rep:?}");
    assert_eq!(rep.files, 2, "{rep:?}");
    assert_eq!(rep.dirs, 1, "{rep:?}");
    assert_eq!(rep.replaced, 0);
    assert_eq!(rep.deleted, 0);

    let listing = list(&archive).unwrap();
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    for expected in ["a.txt", "sub/b.txt", "new.txt", "sub/extra/x.txt"] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    let method = |n: &str| {
        listing
            .entries
            .iter()
            .find(|e| e.name == n)
            .unwrap()
            .method
            .clone()
    };
    assert_eq!(method("a.txt"), "Zstandard");
    assert_eq!(method("sub/b.txt"), "Zstandard");
    assert_eq!(method("new.txt"), "Deflate");
    assert_eq!(method("sub/extra/x.txt"), "Deflate");

    let dest = d.path().join("dest");
    fs::create_dir(&dest).unwrap();
    let r = extract(&archive, &dest, &ExtractOptions::default(), None, &ctx).unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"old a");
    assert_eq!(fs::read(dest.join("sub/b.txt")).unwrap(), b"old b");
    assert_eq!(fs::read(dest.join("new.txt")).unwrap(), b"new data");
    assert_eq!(fs::read(dest.join("sub/extra/x.txt")).unwrap(), b"x data");
}

#[test]
fn update_replaces_an_entry_whose_name_matches_ignoring_case() {
    let d = tempfile::tempdir().unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let archive = d.path().join("replace.zip");
    make_zip(&archive, &[("a.txt", "old a"), ("dir/b.txt", "b")]);

    let upper = d.path().join("A.TXT");
    write(&upper, "new a");
    let edit = ArchiveEdit {
        add: vec![edit_add(upper, "")],
        delete: vec![],
    };
    let rep = update(&archive, &edit, &CreateOptions::default(), &ctx).unwrap();
    assert_eq!(rep.replaced, 1, "{rep:?}");
    assert_eq!(rep.kept, 1, "{rep:?}");

    let listing = list(&archive).unwrap();
    let hits: Vec<&ArchiveEntry> = listing
        .entries
        .iter()
        .filter(|e| e.name.eq_ignore_ascii_case("a.txt"))
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "{:?}",
        listing.entries.iter().map(|e| &e.name).collect::<Vec<_>>()
    );

    let dest = d.path().join("dest");
    fs::create_dir(&dest).unwrap();
    extract(&archive, &dest, &ExtractOptions::default(), None, &ctx).unwrap();
    assert_eq!(fs::read(dest.join(&hits[0].name)).unwrap(), b"new a");
    assert_eq!(fs::read(dest.join("dir/b.txt")).unwrap(), b"b");
}

#[test]
fn update_delete_removes_named_files_and_folder_contents_only() {
    let d = tempfile::tempdir().unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let archive = d.path().join("delete.zip");
    make_zip(
        &archive,
        &[
            ("a.txt", "a"),
            ("keep.txt", "k"),
            ("dir/", ""),
            ("dir/x.txt", "x"),
            ("dir/sub/y.txt", "y"),
            ("dir2/z.txt", "z"),
        ],
    );
    let edit = ArchiveEdit {
        add: vec![],
        delete: vec!["a.txt".into(), "dir/".into()],
    };
    let rep = update(&archive, &edit, &CreateOptions::default(), &ctx).unwrap();
    // a.txt, dir/, dir/x.txt, dir/sub/y.txt
    assert_eq!(rep.deleted, 4, "{rep:?}");
    assert_eq!(rep.kept, 2, "{rep:?}");
    assert_eq!(entry_names(&archive), vec!["dir2/z.txt", "keep.txt"]);

    let dest = d.path().join("dest");
    fs::create_dir(&dest).unwrap();
    extract(&archive, &dest, &ExtractOptions::default(), None, &ctx).unwrap();
    assert_eq!(fs::read(dest.join("keep.txt")).unwrap(), b"k");
    assert_eq!(fs::read(dest.join("dir2/z.txt")).unwrap(), b"z");
}

#[test]
fn cancelled_or_failed_update_leaves_the_original_byte_for_byte() {
    let d = tempfile::tempdir().unwrap();
    let archive = d.path().join("orig.zip");
    make_zip(&archive, &[("a.txt", "a"), ("dir/b.txt", "b")]);
    let before = fs::read(&archive).unwrap();
    let new_file = d.path().join("new.txt");
    write(&new_file, "new");
    let edit = ArchiveEdit {
        add: vec![edit_add(new_file, "")],
        delete: vec!["a.txt".into()],
    };

    let (c, p) = cancelled_parts();
    let ctx = JobCtx::new(&c, &p);
    let err = update(&archive, &edit, &CreateOptions::default(), &ctx).unwrap_err();
    assert!(matches!(err, CoreError::Cancelled), "{err}");
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert!(no_partial_files(d.path()), "{:?}", tree(d.path()));

    // A source that does not exist fails the update before anything is written.
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let missing = ArchiveEdit {
        add: vec![edit_add(d.path().join("does-not-exist.txt"), "")],
        delete: vec![],
    };
    assert!(update(&archive, &missing, &CreateOptions::default(), &ctx).is_err());
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert!(no_partial_files(d.path()), "{:?}", tree(d.path()));
}

#[test]
fn encrypted_update_keeps_old_entries_encrypted_and_they_still_extract() {
    let d = tempfile::tempdir().unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);

    let secret = d.path().join("secret.txt");
    write(&secret, "top secret");
    let sub = d.path().join("sub");
    write(&sub.join("inner.txt"), "inner");
    let archive = d.path().join("enc.zip");
    let enc = || CreateOptions {
        encryption: Encryption::Aes256(Password::new("pw")),
        ..Default::default()
    };
    create(&[secret.clone(), sub.clone()], &archive, &enc(), &ctx).unwrap();

    let fresh = d.path().join("fresh.txt");
    write(&fresh, "fresh data");
    let edit = ArchiveEdit {
        add: vec![edit_add(fresh, "")],
        delete: vec![],
    };
    let rep = update(&archive, &edit, &enc(), &ctx).unwrap();
    assert_eq!(rep.kept, 3, "{rep:?}");
    assert_eq!(rep.files, 1, "{rep:?}");

    let listing = list(&archive).unwrap();
    for e in listing.entries.iter().filter(|e| !e.is_dir) {
        assert!(e.encrypted, "{} is not encrypted", e.name);
    }

    let dest = d.path().join("dest");
    fs::create_dir(&dest).unwrap();
    let opts = ExtractOptions {
        password: Some(Password::new("pw")),
        ..Default::default()
    };
    let r = extract(&archive, &dest, &opts, None, &ctx).unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    assert_eq!(fs::read(dest.join("secret.txt")).unwrap(), b"top secret");
    assert_eq!(fs::read(dest.join("sub/inner.txt")).unwrap(), b"inner");
    assert_eq!(fs::read(dest.join("fresh.txt")).unwrap(), b"fresh data");
}

#[test]
fn index_cached_is_refreshed_after_update() {
    let d = tempfile::tempdir().unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let archive = d.path().join("idx.zip");
    make_zip(&archive, &[("old.txt", "o")]);

    let before = index_cached(&archive).unwrap();
    assert!(before.node("old.txt").is_some());
    assert!(before.node("new.txt").is_none());

    let new_file = d.path().join("new.txt");
    write(&new_file, "n");
    let edit = ArchiveEdit {
        add: vec![edit_add(new_file, "")],
        delete: vec![],
    };
    update(&archive, &edit, &CreateOptions::default(), &ctx).unwrap();

    let after = index_cached(&archive).unwrap();
    assert!(after.node("new.txt").is_some());
    assert!(after.node("old.txt").is_some());
}
