//! Archives whose names collide on Windows (`A.txt` and `a.txt`, a file `docs` and a folder
//! `docs/`, a tar name given twice) are never edited: every edit keeps or drops entries by
//! name, so it could lose one it wasn't asked to delete. Delete and add are refused before
//! anything is written, for zip, 7z and tar alike, and the archive stays byte for byte.

use super::*;
use forge_jobs::{CancelToken, Progress};
use std::io::Write;

/// Files of the test archives: two names differing only in capitals and a third file.
const CASE: &[(&str, &[u8])] = &[("A.txt", b"upper"), ("a.txt", b"lower"), ("x.txt", b"x")];
/// A file `docs` beside a folder `docs/` (named by the file inside it).
const FOLDER: &[(&str, &[u8])] = &[
    ("docs", b"a file"),
    ("docs/inner.txt", b"inside"),
    ("x.txt", b"x"),
];

fn zip_of(path: &Path, files: &[(&str, &[u8])]) {
    let mut w = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, data) in files {
        w.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap();
}

fn sevenz_of(path: &Path, files: &[(&str, &[u8])]) {
    let mut w = sevenz_rust2::ArchiveWriter::create(path).unwrap();
    for (name, data) in files {
        w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file(name), Some(*data))
            .unwrap();
    }
    w.finish().unwrap();
}

fn tar_of(path: &Path, files: &[(&str, &[u8])]) {
    let mut out = Vec::new();
    for (name, data) in files {
        out.extend(tar_write::member_headers(
            name,
            false,
            data.len() as u64,
            None,
        ));
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    fs::write(path, out).unwrap();
}

fn edit_of(dir: &Path, delete: bool) -> ArchiveEdit {
    if delete {
        ArchiveEdit {
            add: vec![],
            delete: vec!["x.txt".into()],
        }
    } else {
        let extra = dir.join("extra.txt");
        fs::write(&extra, b"added").unwrap();
        ArchiveEdit {
            add: vec![(extra, String::new())],
            delete: vec![],
        }
    }
}

/// Every format, both collisions, Delete and add: refused with the message, and the archive
/// byte-identical with no temp file left beside it.
#[test]
fn colliding_names_are_never_edited() {
    type Make = fn(&Path, &[(&str, &[u8])]);
    let formats: [(&str, Make); 3] = [("zip", zip_of), ("7z", sevenz_of), ("tar", tar_of)];
    for (ext, make) in formats {
        for (files, message) in [(CASE, CASE_COLLISION), (FOLDER, NAME_COLLISION)] {
            for delete in [true, false] {
                let d = tempfile::tempdir().unwrap();
                let archive = d.path().join(format!("a.{ext}"));
                make(&archive, files);
                let listing = list(&archive).unwrap();
                assert!(has_colliding_names(&listing.entries), "{ext} {files:?}");
                let before = fs::read(&archive).unwrap();
                let (c, p) = (CancelToken::new(), Progress::default());
                let err = update(
                    &archive,
                    &edit_of(d.path(), delete),
                    &CreateOptions::default(),
                    &JobCtx::new(&c, &p),
                )
                .unwrap_err();
                let what = format!("{ext} {} delete={delete}", files[0].0);
                assert!(
                    matches!(&err, CoreError::Unsupported(m) if m == message),
                    "{what}: {err:?}"
                );
                assert_eq!(fs::read(&archive).unwrap(), before, "{what}: untouched");
                let others: Vec<String> = fs::read_dir(d.path())
                    .unwrap()
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .filter(|n| n != &format!("a.{ext}") && n != "extra.txt")
                    .collect();
                assert!(others.is_empty(), "{what}: left {others:?}");
            }
        }
    }
}

#[test]
fn a_tar_name_given_twice_is_never_edited() {
    let d = tempfile::tempdir().unwrap();
    let archive = d.path().join("dup.tar");
    tar_of(
        &archive,
        &[
            ("dup.txt", b"first"),
            ("dup.txt", b"second"),
            ("x.txt", b"x"),
        ],
    );
    // The listing shows the last copy only, so the names don't collide there...
    assert!(!has_colliding_names(&list(&archive).unwrap().entries));
    // ...but an edit would drop the first copy: refused.
    let before = fs::read(&archive).unwrap();
    let (c, p) = (CancelToken::new(), Progress::default());
    let err = update(
        &archive,
        &edit_of(d.path(), true),
        &CreateOptions::default(),
        &JobCtx::new(&c, &p),
    )
    .unwrap_err();
    assert!(
        matches!(&err, CoreError::Unsupported(m) if m == NAME_COLLISION),
        "{err:?}"
    );
    assert_eq!(fs::read(&archive).unwrap(), before);
}

#[test]
fn collisions_are_told_apart() {
    let e = |name: &str, is_dir: bool| ArchiveEntry {
        name: name.into(),
        is_dir,
        ..Default::default()
    };
    assert_eq!(
        name_collision(&[e("A.txt", false), e("a.txt", false)]),
        Some(NameCollision::Case)
    );
    assert_eq!(
        name_collision(&[e("docs", false), e("docs/", true)]),
        Some(NameCollision::FileAndFolder)
    );
    // A folder named only by a path inside it, either order.
    assert_eq!(
        name_collision(&[e("docs/x", false), e("Docs", false)]),
        Some(NameCollision::FileAndFolder)
    );
    assert_eq!(
        name_collision(&[e("a/b", false), e("a/b", false)]),
        Some(NameCollision::Duplicate)
    );
    // Ordinary archives: folders and their files, a folder entry after its files.
    assert_eq!(
        name_collision(&[
            e("docs/x.txt", false),
            e("docs/", true),
            e("Docs/y.txt", false),
            e("other.txt", false)
        ]),
        None
    );
}

/// An archive without colliding names is still edited, in every format.
#[test]
fn ordinary_archives_are_still_edited() {
    type Make = fn(&Path, &[(&str, &[u8])]);
    let formats: [(&str, Make); 3] = [("zip", zip_of), ("7z", sevenz_of), ("tar", tar_of)];
    for (ext, make) in formats {
        let d = tempfile::tempdir().unwrap();
        let archive = d.path().join(format!("ok.{ext}"));
        make(&archive, &[("A.txt", b"upper"), ("x.txt", b"x")]);
        let (c, p) = (CancelToken::new(), Progress::default());
        update(
            &archive,
            &edit_of(d.path(), true),
            &CreateOptions::default(),
            &JobCtx::new(&c, &p),
        )
        .unwrap();
        let names: Vec<String> = list(&archive)
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["A.txt"], "{ext}");
    }
}
