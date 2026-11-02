//! 7z hostile names, links and items.
//!
//! Archives are written with `sevenz_rust2` directly, so entry names, attributes and anti-items
//! can be set freely.

use super::*;
use forge_jobs::{CancelToken, Progress};
use sevenz_rust2 as sz;

/// Write a 7z at `dir/name`, one entry per `(entry, data)` pair (each in its own block).
fn seven_z(dir: &Path, name: &str, entries: Vec<(sz::ArchiveEntry, Option<&[u8]>)>) -> PathBuf {
    let path = dir.join(name);
    let mut w = sz::ArchiveWriter::create(&path).unwrap();
    w.set_encrypt_header(false);
    for (entry, data) in entries {
        w.push_archive_entry(entry, data).unwrap();
    }
    w.finish().unwrap();
    path
}

fn ctx_run(archive: &Path, dest: &Path, opts: &ExtractOptions) -> Result<ExtractReport> {
    let (cancel, progress) = (CancelToken::new(), Progress::default());
    extract(archive, dest, opts, None, &JobCtx::new(&cancel, &progress))
}

fn fresh_dest(dir: &Path, name: &str) -> PathBuf {
    let dest = dir.join(name);
    fs::create_dir(&dest).unwrap();
    dest
}

fn is_empty_dir(path: &Path) -> bool {
    fs::read_dir(path).unwrap().next().is_none()
}

#[test]
fn malicious_7z_names_reject_the_whole_archive() {
    let d = tempfile::tempdir().unwrap();
    let names = [
        "../x",
        "..\\x",
        "/abs",
        "C:/x",
        "C:x",
        "\\\\server\\share\\x",
        "\\\\?\\C:\\x",
        "a.txt:ads",
        "CON",
        "nul.txt",
        "COM1",
        "LPT9.log",
        "trailing.",
        "trailing ",
        "a\0b.txt",
    ];
    for (i, name) in names.iter().enumerate() {
        let dest = fresh_dest(d.path(), &format!("dest{i}"));
        let z = seven_z(
            d.path(),
            "evil.7z",
            vec![
                (sz::ArchiveEntry::new_file("good.txt"), Some(&b"fine"[..])),
                (sz::ArchiveEntry::new_file(name), Some(&b"pwned"[..])),
            ],
        );

        // sevenz-rust2 writes a NUL in a name but its reader then refuses the whole directory
        // ("Error parsing file names"), so that one can't be listed; it must still be refused.
        match list(&z) {
            Ok(listing) => assert!(
                listing.entries.iter().any(|e| !e.safe_name),
                "{name:?} must be listed as unsafe"
            ),
            Err(e) => assert!(name.contains('\0'), "{name:?}: {e}"),
        }
        assert!(
            ctx_run(&z, &dest, &ExtractOptions::default()).is_err(),
            "{name:?}"
        );
        assert!(is_empty_dir(&dest), "nothing written for {name:?}");
        assert!(!d.path().join("x").exists(), "{name:?} escaped");
        fs::remove_file(&z).unwrap();
    }
}

#[test]
fn seven_zip_link_attributes_are_skipped_never_created() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let mut reparse = sz::ArchiveEntry::new_file("reparse.txt");
    reparse.has_windows_attributes = true;
    reparse.windows_attributes = 0x400;
    let mut unix_link = sz::ArchiveEntry::new_file("unix-link");
    unix_link.has_windows_attributes = true;
    unix_link.windows_attributes = 0x8000 | (0o120777 << 16);
    let z = seven_z(
        d.path(),
        "links.7z",
        vec![
            (reparse, Some(&b"/etc/passwd"[..])),
            (unix_link, Some(&b"/etc/passwd"[..])),
            (sz::ArchiveEntry::new_file("real.txt"), Some(&b"x"[..])),
        ],
    );

    let report = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    assert!(
        report
            .skipped
            .iter()
            .any(|s| s.path.ends_with("reparse.txt")),
        "{report:?}"
    );
    assert!(
        report.skipped.iter().any(|s| s.path.ends_with("unix-link")),
        "{report:?}"
    );
    assert_eq!(report.extracted, vec![dest.join("real.txt")], "{report:?}");
    assert!(!paths::exists_no_follow(&dest.join("reparse.txt")));
    assert!(!paths::exists_no_follow(&dest.join("unix-link")));
}

#[test]
fn anti_items_are_not_listed_or_extracted() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let mut anti = sz::ArchiveEntry::new_file("gone.txt");
    anti.has_stream = false;
    anti.is_anti_item = true;
    let z = seven_z(
        d.path(),
        "anti.7z",
        vec![
            (anti, None::<&[u8]>),
            (sz::ArchiveEntry::new_file("real.txt"), Some(&b"x"[..])),
        ],
    );

    let listing = list(&z).unwrap();
    assert!(
        listing.entries.iter().all(|e| e.name != "gone.txt"),
        "{:?}",
        listing.entries.iter().map(|e| &e.name).collect::<Vec<_>>()
    );

    let report = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    assert_eq!(report.extracted, vec![dest.join("real.txt")], "{report:?}");
    assert!(!paths::exists_no_follow(&dest.join("gone.txt")));
    assert!(report.failed.is_empty(), "{report:?}");
}

#[test]
fn nameless_entry_is_named_after_the_archive() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let z = seven_z(
        d.path(),
        "blob.7z",
        vec![(sz::ArchiveEntry::new_file(""), Some(&b"blob data"[..]))],
    );

    let names: Vec<String> = list(&z)
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["blob"]);

    let report = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(fs::read(dest.join("blob")).unwrap(), b"blob data");
}

#[test]
fn case_collisions_in_a_solid_block_keep_the_first() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let path = d.path().join("case.7z");
    let mut w = sz::ArchiveWriter::create(&path).unwrap();
    w.set_encrypt_header(false);
    let entries = vec![
        sz::ArchiveEntry::new_file("A.txt"),
        sz::ArchiveEntry::new_file("a.txt"),
    ];
    let readers = vec![
        sz::SourceReader::new(&b"first"[..]),
        sz::SourceReader::new(&b"second"[..]),
    ];
    w.push_archive_entries(entries, readers).unwrap();
    w.finish().unwrap();

    let report = ctx_run(&path, &dest, &ExtractOptions::default()).unwrap();
    assert_eq!(report.extracted.len(), 1, "{report:?}");
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert!(report.skipped[0].reason.contains("case"), "{report:?}");
    let out: Vec<_> = fs::read_dir(&dest).unwrap().flatten().collect();
    assert_eq!(out.len(), 1, "{report:?}");
    let kept = fs::read(&report.extracted[0]).unwrap();
    assert_eq!(kept, b"first");
}

#[test]
fn backslash_names_are_folders() {
    let d = tempfile::tempdir().unwrap();
    let dest = fresh_dest(d.path(), "dest");
    let z = seven_z(
        d.path(),
        "backslash.7z",
        vec![(
            sz::ArchiveEntry::new_file("dir\\sub\\f.txt"),
            Some(&b"nested"[..]),
        )],
    );

    let report = ctx_run(&z, &dest, &ExtractOptions::default()).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(
        report.extracted,
        vec![dest.join("dir").join("sub").join("f.txt")]
    );
    assert_eq!(
        fs::read(dest.join("dir").join("sub").join("f.txt")).unwrap(),
        b"nested"
    );
}
