//! An archive replaced by another one of the same size and modified time is not mistaken
//! for the old one: the listing, the index and the temp copies follow the new file.

use super::*;
use forge_jobs::{CancelToken, Progress};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

/// A stored (uncompressed) zip with one entry, so equal name and data lengths give equal
/// file sizes.
fn stored_zip(path: &Path, name: &str, data: &[u8]) {
    let mut zw = ZipWriter::new(File::create(path).unwrap());
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zw.start_file(name, opts).unwrap();
    zw.write_all(data).unwrap();
    zw.finish().unwrap();
}

fn set_mtime(path: &Path, when: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

/// Extract `entry` from `archive` into a temp copy and return the copy's bytes.
fn temp_copy(archive: &Path, entry: &str) -> Result<Vec<u8>> {
    let (cancel, progress) = (CancelToken::new(), Progress::default());
    let ctx = JobCtx::new(&cancel, &progress);
    let path = extract_entry_to_temp(archive, entry, None, u64::MAX, &ctx)?;
    let bytes = std::fs::read(&path).unwrap();
    // Remove this copy's folders (the archive and entry folders under the temp root).
    if let Some(entry_dir) = path.parent() {
        let _ = std::fs::remove_dir_all(entry_dir.parent().unwrap_or(entry_dir));
    }
    Ok(bytes)
}

#[test]
fn same_size_and_time_replacement_is_a_new_archive() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("swap.zip");
    let a_data = vec![b'a'; 2048];
    let b_data = vec![b'b'; 2048];
    stored_zip(&archive, "one.bin", &a_data);
    let before_len = std::fs::metadata(&archive).unwrap().len();
    let mtime = std::fs::metadata(&archive).unwrap().modified().unwrap();
    let stamp_a = archive_stamp(&archive).unwrap();
    let index_a = index_cached(&archive).unwrap();
    assert!(index_a.node("one.bin").is_some());
    assert_eq!(temp_copy(&archive, "one.bin").unwrap(), a_data);

    // Another archive of the same size, written over the old one with the old time kept.
    stored_zip(&archive, "two.bin", &b_data);
    set_mtime(&archive, mtime);
    assert_eq!(std::fs::metadata(&archive).unwrap().len(), before_len);
    assert_eq!(
        std::fs::metadata(&archive).unwrap().modified().unwrap(),
        mtime
    );
    assert_ne!(archive_stamp(&archive).unwrap(), stamp_a);

    let index_b = index_cached(&archive).unwrap();
    assert!(
        !Arc::ptr_eq(&index_a, &index_b),
        "the old index is not reused"
    );
    assert!(index_b.node("two.bin").is_some());
    assert!(index_b.node("one.bin").is_none());
    assert_eq!(temp_copy(&archive, "two.bin").unwrap(), b_data);
    assert!(temp_copy(&archive, "one.bin").is_err(), "A's entry is gone");
}

#[test]
fn unchanged_archive_keeps_its_stamp_and_its_cached_index() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("still.zip");
    stored_zip(&archive, "x.bin", b"same");
    assert_eq!(
        archive_stamp(&archive).unwrap(),
        archive_stamp(&archive).unwrap()
    );
    let first = index_cached(&archive).unwrap();
    let second = index_cached(&archive).unwrap();
    assert!(Arc::ptr_eq(&first, &second), "the cache still hits");
    assert_eq!(temp_copy(&archive, "x.bin").unwrap(), b"same");
}
