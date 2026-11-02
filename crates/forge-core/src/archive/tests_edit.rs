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
        // A file and a folder with the same top-level name are two items.
        (vec![("foo", "f"), ("foo/bar.txt", "b")], TopLevel::Many),
        (vec![("Foo/bar.txt", "b"), ("foo", "f")], TopLevel::Many),
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

#[cfg(windows)]
#[test]
fn stale_partials_beside_an_archive_are_removed_unless_in_use() {
    let d = tempfile::tempdir().unwrap();
    let name = || d.path().join(crate::ops::partial_name());
    let (old, busy, fresh, linked) = (name(), name(), name(), name());
    // Look like ours but aren't exactly: a user's file.
    let notes = d
        .path()
        .join(format!("{}notes.txt", crate::ops::PARTIAL_PREFIX));
    let upper = d.path().join(format!(
        "{}{}.tmp",
        crate::ops::PARTIAL_PREFIX,
        "A".repeat(32)
    ));
    for p in [&old, &busy, &fresh, &linked, &notes, &upper] {
        fs::write(p, b"partial").unwrap();
    }
    // A matching name hard-linked to a user's file: deleting it would be harmless to the data,
    // but it is not ours alone, so it stays.
    let user_file = d.path().join("precious.dat");
    fs::write(&user_file, b"precious").unwrap();
    fs::remove_file(&linked).unwrap();
    fs::hard_link(&user_file, &linked).unwrap();
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    for p in [&old, &busy, &linked, &notes, &upper] {
        File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(hour_ago)
            .unwrap();
    }
    let held = File::open(&busy).unwrap();
    zip_backend::remove_stale_partials(d.path(), std::time::Duration::from_secs(3600));
    assert!(!old.exists(), "an old unused partial is removed");
    assert!(busy.exists(), "a partial something has open stays");
    assert!(fresh.exists(), "a recent partial stays");
    assert!(linked.exists(), "a hard-linked match stays");
    assert!(notes.exists(), "a user's .forge-partial-notes.txt stays");
    assert!(upper.exists(), "names not exactly ours stay");
    assert_eq!(fs::read(&user_file).unwrap(), b"precious");
    drop(held);
}

#[test]
fn adding_to_an_encrypted_archive_needs_its_password() {
    let d = tempfile::tempdir().unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    for legacy in [false, true] {
        let secret = d.path().join("secret.txt");
        write(&secret, "top secret");
        let archive = d.path().join(format!("enc-{legacy}.zip"));
        let with = |pw: &str| CreateOptions {
            encryption: if legacy {
                Encryption::ZipCryptoLegacy(Password::new(pw))
            } else {
                Encryption::Aes256(Password::new(pw))
            },
            ..Default::default()
        };
        create(std::slice::from_ref(&secret), &archive, &with("pw"), &ctx).unwrap();
        let original = fs::read(&archive).unwrap();
        let fresh = d.path().join("fresh.txt");
        write(&fresh, "fresh data");
        let edit = ArchiveEdit {
            add: vec![edit_add(fresh, "")],
            delete: vec![],
        };

        // No password: never added unencrypted, nothing changes.
        let err = update(&archive, &edit, &CreateOptions::default(), &ctx).unwrap_err();
        assert!(matches!(err, CoreError::PasswordRequired(_)), "{err:?}");
        assert_eq!(fs::read(&archive).unwrap(), original);
        // A wrong password (ZipCrypto's one-byte check is backed by the checksum): nothing
        // changes either.
        for wrong in ["nope", "pw2", "PW", "x", "secret", "1234"] {
            let err = update(&archive, &edit, &with(wrong), &ctx).unwrap_err();
            assert!(
                matches!(err, CoreError::WrongPassword(_)),
                "{wrong}: {err:?}"
            );
            assert_eq!(fs::read(&archive).unwrap(), original);
        }
        assert!(
            fs::read_dir(d.path())
                .unwrap()
                .flatten()
                .all(|e| !ops::is_partial_name(&e.file_name().to_string_lossy()))
        );

        // The right password: the new entry is encrypted with it.
        update(&archive, &edit, &with("pw"), &ctx).unwrap();
        let listing = list(&archive).unwrap();
        let added = listing
            .entries
            .iter()
            .find(|e| e.name == "fresh.txt")
            .unwrap();
        assert!(added.encrypted && added.encryption.is_some(), "{added:?}");
        let dest = d.path().join(format!("dest-{legacy}"));
        fs::create_dir(&dest).unwrap();
        let none = extract(&archive, &dest, &ExtractOptions::default(), None, &ctx).unwrap_err();
        assert!(matches!(none, CoreError::PasswordRequired(_)), "{none:?}");
        let opts = ExtractOptions {
            password: Some(Password::new("pw")),
            ..Default::default()
        };
        let r = extract(&archive, &dest, &opts, None, &ctx).unwrap();
        assert!(r.failed.is_empty(), "{r:?}");
        assert_eq!(fs::read(dest.join("fresh.txt")).unwrap(), b"fresh data");

        // Deleting copies the other entries as they are: no password needed.
        let del = ArchiveEdit {
            add: vec![],
            delete: vec!["fresh.txt".into()],
        };
        update(&archive, &del, &CreateOptions::default(), &ctx).unwrap();
        let listing = list(&archive).unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert!(listing.entries[0].encrypted);
        let again = d.path().join(format!("again-{legacy}"));
        fs::create_dir(&again).unwrap();
        let r = extract(&archive, &again, &opts, None, &ctx).unwrap();
        assert!(r.failed.is_empty(), "{r:?}");
        assert_eq!(fs::read(again.join("secret.txt")).unwrap(), b"top secret");
    }
}

/// Each entry's stored local record (header, data, descriptor) and central record with the
/// local-header offset blanked, by name.
fn records_by_name(path: &Path) -> std::collections::HashMap<String, (Vec<u8>, Vec<u8>)> {
    let bytes = fs::read(path).unwrap();
    let mut file = File::open(path).unwrap();
    let mut za = zip::ZipArchive::new(std::io::BufReader::new(File::open(path).unwrap())).unwrap();
    let records = super::zip_merge::raw_records(path, &mut file, &mut za).unwrap();
    records
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let name = za.by_index_raw(i).unwrap().name().unwrap().into_owned();
            let local = bytes[r.start as usize..r.end as usize].to_vec();
            let mut central = r.central.clone();
            central[42..46].fill(0);
            (name, (local, central))
        })
        .collect()
}

fn seven_zip() -> Option<PathBuf> {
    let p = PathBuf::from(r"C:\Program Files\7-Zip\7z.exe");
    p.exists().then_some(p)
}

/// Edit `archive` (delete `gone`, add a file with `password`) and check every other entry
/// kept its stored bytes, then that it all opens, in 7-Zip too when it is installed.
fn edit_keeps_records(archive: &Path, gone: &str, password: &str) {
    let d = archive.parent().unwrap();
    let before = records_by_name(archive);
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let added = d.join("added.txt");
    write(&added, "added later");
    let edit = ArchiveEdit {
        add: vec![edit_add(added, "")],
        delete: vec![gone.to_owned()],
    };
    let opts = CreateOptions {
        encryption: Encryption::Aes256(Password::new(password)),
        ..Default::default()
    };
    let rep = update(archive, &edit, &opts, &ctx).unwrap();
    assert!(rep.warnings.is_empty(), "{rep:?}");
    let after = records_by_name(archive);
    assert!(!after.contains_key(gone));
    for (name, record) in &before {
        if name != gone {
            assert_eq!(after.get(name), Some(record), "{name} changed");
        }
    }
    assert!(after["added.txt"].1.len() > 46);
    // Everything opens with the password.
    let rep = test_archive(
        archive,
        Some(&Password::new(password)),
        &ExtractLimits::default(),
        &ctx,
    )
    .unwrap();
    assert!(rep.is_ok(), "{:?}", rep.entries);
    if let Some(seven) = seven_zip() {
        let out = std::process::Command::new(seven)
            .arg("t")
            .arg(format!("-p{password}"))
            .arg(archive)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "7-Zip: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

#[test]
fn editing_a_zipcrypto_zip_keeps_other_entries_byte_for_byte() {
    let d = tempfile::tempdir().unwrap();
    // One made by 7-Zip (ZipCrypto, Store and Deflate, NTFS times, a folder entry)...
    let fixture = d.path().join("seven.zip");
    fs::write(&fixture, include_bytes!("fixtures/zipcrypto-7zip.zip")).unwrap();
    edit_keeps_records(&fixture, "notes.txt", "fixture");
    // ... and one of ours (ZipCrypto entries with data descriptors).
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let src = d.path().join("src");
    write(&src.join("one.txt"), &"one ".repeat(100));
    write(&src.join("two.txt"), "two");
    write(&src.join("sub/three.bin"), "three");
    let ours = d.path().join("ours.zip");
    let opts = CreateOptions {
        encryption: Encryption::ZipCryptoLegacy(Password::new("pw")),
        threads: 1,
        ..Default::default()
    };
    create(std::slice::from_ref(&src), &ours, &opts, &ctx).unwrap();
    edit_keeps_records(&ours, "src/two.txt", "pw");
}

/// Mark entry `name` encrypted (general-purpose bit 0) in its local and central headers.
fn mark_encrypted(bytes: &mut [u8], name: &[u8]) {
    let mut marked = 0;
    for (sig, name_at, flags_at) in [(b"PK\x03\x04", 30, 6), (b"PK\x01\x02", 46, 8)] {
        let mut i = 0;
        while let Some(p) = bytes[i..].windows(4).position(|w| w == sig) {
            let at = i + p;
            if bytes.get(at + name_at..at + name_at + name.len()) == Some(name) {
                bytes[at + flags_at] |= 1;
                marked += 1;
            }
            i = at + 4;
        }
    }
    assert_eq!(marked, 2);
}

#[test]
fn an_archive_with_only_encrypted_folders_takes_only_encrypted_additions() {
    let d = tempfile::tempdir().unwrap();
    let archive = d.path().join("dirs.zip");
    make_zip(&archive, &[("locked/", ""), ("plain.txt", "p")]);
    let mut bytes = fs::read(&archive).unwrap();
    mark_encrypted(&mut bytes, b"locked/");
    fs::write(&archive, &bytes).unwrap();
    assert!(list(&archive).unwrap().any_encrypted());
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let new = d.path().join("new.txt");
    write(&new, "new data");
    let edit = ArchiveEdit {
        add: vec![edit_add(new, "locked/")],
        delete: vec![],
    };
    // Fails closed without a password: never a plaintext addition.
    let err = update(&archive, &edit, &CreateOptions::default(), &ctx).unwrap_err();
    assert!(matches!(err, CoreError::PasswordRequired(_)), "{err:?}");
    assert_eq!(fs::read(&archive).unwrap(), bytes);
    // With one (asked twice by the app: nothing to check it against), new files are AES-256.
    let opts = CreateOptions {
        encryption: Encryption::Aes256(Password::new("fresh pw")),
        ..Default::default()
    };
    update(&archive, &edit, &opts, &ctx).unwrap();
    let listing = list(&archive).unwrap();
    let added = listing
        .entries
        .iter()
        .find(|e| e.name == "locked/new.txt")
        .unwrap();
    assert_eq!(added.encryption, Some(EntryEncryption::Aes));
}

#[test]
fn several_passwords_warn_and_a_wrong_one_changes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let archive = d.path().join("mixed.zip");
    {
        let mut zw = ZipWriter::new(File::create(&archive).unwrap());
        for (name, pw) in [("a.txt", "one"), ("b.txt", "two")] {
            let o = SimpleFileOptions::default().with_aes_encryption(zip::AesMode::Aes256, pw);
            zw.start_file(name, o).unwrap();
            zw.write_all(format!("data of {name}").as_bytes()).unwrap();
        }
        zw.finish().unwrap();
    }
    let original = fs::read(&archive).unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let new = d.path().join("c.txt");
    write(&new, "third");
    let edit = ArchiveEdit {
        add: vec![edit_add(new, "")],
        delete: vec![],
    };
    let with = |pw: &str| CreateOptions {
        encryption: Encryption::Aes256(Password::new(pw)),
        ..Default::default()
    };
    // A password that opens none of them: refused before anything is written.
    let err = update(&archive, &edit, &with("three"), &ctx).unwrap_err();
    assert!(matches!(err, CoreError::WrongPassword(_)), "{err:?}");
    assert_eq!(fs::read(&archive).unwrap(), original);
    // One that opens some: the update runs and says the archive has several passwords.
    let rep = update(&archive, &edit, &with("one"), &ctx).unwrap();
    assert_eq!(rep.warnings, vec![SEVERAL_PASSWORDS.to_owned()]);
    let dest = d.path().join("dest");
    fs::create_dir(&dest).unwrap();
    let opts = ExtractOptions {
        password: Some(Password::new("one")),
        ..Default::default()
    };
    let r = extract(&archive, &dest, &opts, None, &ctx).unwrap();
    assert_eq!(fs::read(dest.join("c.txt")).unwrap(), b"third");
    assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"data of a.txt");
    // b.txt has its own password: it fails, the rest extracts.
    assert_eq!(r.failed.len(), 1, "{r:?}");
}

#[test]
fn the_password_probe_keeps_the_decoder_memory_limit() {
    use zip::unstable::write::FileOptionsExt;
    let d = tempfile::tempdir().unwrap();
    let archive = d.path().join("bomb.zip");
    // The only encrypted file declares LZMA with a 2 GiB dictionary.
    let mut model = vec![5, 2, 5, 0, 0x5D, 0, 0, 0, 0x80];
    model.extend([0u8; 64]);
    let mut zw = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let o = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .with_deprecated_encryption(b"pw")
        .unwrap();
    zw.start_file("model.bin", o).unwrap();
    zw.write_all(&model).unwrap();
    let mut bytes = zw.finish().unwrap().into_inner();
    let central = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    for at in [8, central + 10] {
        bytes[at] = 14;
    }
    fs::write(&archive, &bytes).unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let new = d.path().join("n.txt");
    write(&new, "n");
    let edit = ArchiveEdit {
        add: vec![edit_add(new, "")],
        delete: vec![],
    };
    let opts = CreateOptions {
        encryption: Encryption::Aes256(Password::new("pw")),
        ..Default::default()
    };
    match update(&archive, &edit, &opts, &ctx) {
        Err(CoreError::UnsafePath(msg)) => assert!(msg.contains("decoder memory"), "{msg}"),
        other => panic!("expected the decoder memory limit, got {other:?}"),
    }
    assert_eq!(fs::read(&archive).unwrap(), bytes);
}

#[test]
fn split_archives_are_not_edited() {
    let d = tempfile::tempdir().unwrap();
    let (c, p) = ctx_parts();
    let ctx = JobCtx::new(&c, &p);
    let new = d.path().join("new.txt");
    write(&new, "new");
    let edits = [
        ArchiveEdit {
            add: vec![edit_add(new, "")],
            delete: vec![],
        },
        ArchiveEdit {
            add: vec![],
            delete: vec!["a.txt".into()],
        },
    ];
    let base = d.path().join("base.zip");
    make_zip(&base, &[("a.txt", "a"), ("b.txt", "b")]);
    let good = fs::read(&base).unwrap();
    let central = good.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    let eocd = good.windows(4).rposition(|w| w == b"PK\x05\x06").unwrap();
    // One central record starting on disk 1; the end record on disk 1; the directory on
    // disk 1.
    let patches: [(&str, usize); 3] = [
        ("record", central + 34),
        ("this disk", eocd + 4),
        ("directory disk", eocd + 6),
    ];
    for (what, at) in patches {
        let archive = d
            .path()
            .join(format!("split-{}.zip", what.replace(' ', "-")));
        let mut bytes = good.clone();
        bytes[at] = 1;
        fs::write(&archive, &bytes).unwrap();
        for edit in &edits {
            match update(&archive, edit, &CreateOptions::default(), &ctx) {
                Err(CoreError::Unsupported(msg)) => {
                    assert!(msg.contains("Split (multi-part)"), "{what}: {msg}")
                }
                other => panic!("{what}: expected a refusal, got {other:?}"),
            }
            assert_eq!(fs::read(&archive).unwrap(), bytes, "{what}");
        }
    }
    // A one-disk archive still edits.
    update(&base, &edits[1], &CreateOptions::default(), &ctx).unwrap();
}
