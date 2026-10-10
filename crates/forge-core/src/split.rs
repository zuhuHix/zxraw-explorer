//! Verified, cancellable file splitting and joining.
//!
//! A split consists of adjacent `name.ext.001` parts and a `name.ext.b3split`
//! manifest. The manifest is written last, after every part has been placed.
//! Joining streams into a temporary file and publishes it only after both the
//! per-part and whole-file BLAKE3 hashes have been checked.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::ops::JobCtx;

const IO_CHUNK: usize = 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PARTS: usize = 1_000_000;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A part entry in a `.b3split` manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitPart {
    pub name: String,
    pub size: u64,
    /// Lowercase hexadecimal BLAKE3 digest.
    pub blake3: String,
}

/// JSON manifest stored beside the split parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitManifest {
    pub original_name: String,
    pub original_size: u64,
    pub part_size: u64,
    pub parts: Vec<SplitPart>,
    /// Lowercase hexadecimal BLAKE3 digest of the original file.
    pub whole_blake3: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitReport {
    pub manifest: PathBuf,
    pub parts: Vec<PathBuf>,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinReport {
    pub output: PathBuf,
    pub bytes: u64,
    pub parts: usize,
}

/// Split a regular file into parts no larger than `part_size` bytes.
///
/// Existing outputs are never overwritten. Cancellation or any failure removes
/// all temporary files and any parts already published by this invocation.
pub fn split_file(source: &Path, part_size: u64, ctx: &JobCtx<'_>) -> Result<SplitReport> {
    if part_size == 0 {
        return Err(CoreError::Invalid(
            "part size must be greater than zero".into(),
        ));
    }
    let source_file = File::open(source).map_err(|e| CoreError::io(source, e))?;
    let metadata = source_file
        .metadata()
        .map_err(|e| CoreError::io(source, e))?;
    if !metadata.is_file() {
        return Err(CoreError::Invalid(format!(
            "{} is not a file",
            source.display()
        )));
    }
    let original_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| CoreError::Invalid("source filename is not valid Unicode".into()))?
        .to_owned();
    let parent = source.parent().unwrap_or(Path::new("."));
    let source_size = metadata.len();
    let part_count_u64 = if source_size == 0 {
        1
    } else {
        source_size.div_ceil(part_size)
    };
    let part_count = usize::try_from(part_count_u64)
        .ok()
        .filter(|n| *n <= MAX_PARTS)
        .ok_or_else(|| {
            CoreError::Invalid(format!("split would create more than {MAX_PARTS} parts"))
        })?;

    let mut final_parts = Vec::with_capacity(part_count);
    for index in 1..=part_count {
        final_parts.push(parent.join(format!("{original_name}.{index:03}")));
    }
    let manifest_path = parent.join(format!("{original_name}.b3split"));
    for path in final_parts.iter().chain(std::iter::once(&manifest_path)) {
        if exists(path) {
            return Err(CoreError::AlreadyExists(path.clone()));
        }
    }

    ctx.progress.add_total_items(part_count as u64 + 1);
    ctx.progress.add_total_bytes(source_size);
    let mut source_file = source_file;
    let mut committed = Vec::<PathBuf>::with_capacity(part_count + 1);
    let operation = (|| {
        let mut parts = Vec::with_capacity(part_count);
        let mut whole = blake3::Hasher::new();
        let mut remaining = source_size;
        let mut buf = vec![0u8; IO_CHUNK];

        for (index, dest) in final_parts.iter().enumerate() {
            ctx.cancel.check()?;
            ctx.progress
                .set_current(format!("Writing part {} of {}", index + 1, part_count));
            let expected = remaining.min(part_size);
            let (temp_path, mut output, mut guard) = create_temp(dest)?;
            let mut part_hash = blake3::Hasher::new();
            let mut part_written = 0u64;
            while part_written < expected {
                ctx.cancel.check()?;
                let want = ((expected - part_written) as usize).min(buf.len());
                let n = read_retry(&mut source_file, &mut buf[..want])
                    .map_err(|e| CoreError::io(source, e))?;
                if n == 0 {
                    return Err(CoreError::Invalid(format!(
                        "source changed while splitting: expected {source_size} bytes, read {}",
                        source_size - remaining + part_written
                    )));
                }
                part_hash.update(&buf[..n]);
                whole.update(&buf[..n]);
                output
                    .write_all(&buf[..n])
                    .map_err(|e| CoreError::io(&temp_path, e))?;
                part_written += n as u64;
                ctx.progress.add_bytes(n as u64);
            }
            output.flush().map_err(|e| CoreError::io(&temp_path, e))?;
            output
                .sync_all()
                .map_err(|e| CoreError::io(&temp_path, e))?;
            drop(output);
            publish_temp(&temp_path, dest)?;
            guard.disarm();
            committed.push(dest.clone());
            parts.push(SplitPart {
                name: dest.file_name().unwrap().to_string_lossy().into_owned(),
                size: part_written,
                blake3: part_hash.finalize().to_hex().to_string(),
            });
            remaining -= part_written;
            ctx.progress.add_items(1);
        }

        let mut extra = [0u8; 1];
        if read_retry(&mut source_file, &mut extra).map_err(|e| CoreError::io(source, e))? != 0 {
            return Err(CoreError::Invalid(
                "source changed while splitting: file grew".into(),
            ));
        }
        ctx.cancel.check()?;
        let manifest = SplitManifest {
            original_name,
            original_size: source_size,
            part_size,
            parts,
            whole_blake3: whole.finalize().to_hex().to_string(),
        };
        let bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| CoreError::Invalid(format!("could not encode split manifest: {e}")))?;
        let (temp_path, mut output, mut guard) = create_temp(&manifest_path)?;
        output
            .write_all(&bytes)
            .map_err(|e| CoreError::io(&temp_path, e))?;
        output.flush().map_err(|e| CoreError::io(&temp_path, e))?;
        output
            .sync_all()
            .map_err(|e| CoreError::io(&temp_path, e))?;
        drop(output);
        ctx.cancel.check()?;
        publish_temp(&temp_path, &manifest_path)?;
        guard.disarm();
        committed.push(manifest_path.clone());
        ctx.progress.add_items(1);
        ctx.progress.set_current("Split complete");
        Ok(SplitReport {
            manifest: manifest_path.clone(),
            parts: final_parts.clone(),
            bytes: source_size,
        })
    })();

    if operation.is_err() {
        for path in committed.iter().rev() {
            let _ = fs::remove_file(path);
        }
    }
    operation
}

/// Join a `.b3split` manifest or its first `.001` part.
///
/// The output is placed beside the manifest under `original_name`. It is only
/// published after all lengths and hashes match, and an existing output is an error.
pub fn join_files(input: &Path, ctx: &JobCtx<'_>) -> Result<JoinReport> {
    let manifest_path = manifest_path_for(input)?;
    let metadata = fs::metadata(&manifest_path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            CoreError::Invalid(format!(
                "split manifest is missing: {}",
                manifest_path.display()
            ))
        } else {
            CoreError::io(&manifest_path, e)
        }
    })?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(CoreError::Invalid(format!(
            "split manifest is larger than {MAX_MANIFEST_BYTES} bytes"
        )));
    }
    let bytes = fs::read(&manifest_path).map_err(|e| CoreError::io(&manifest_path, e))?;
    let manifest: SplitManifest = serde_json::from_slice(&bytes)
        .map_err(|e| CoreError::Invalid(format!("invalid split manifest: {e}")))?;
    validate_manifest(&manifest)?;
    let parent = manifest_path.parent().unwrap_or(Path::new("."));
    let output_path = parent.join(&manifest.original_name);
    if exists(&output_path) {
        return Err(CoreError::AlreadyExists(output_path));
    }

    ctx.progress.add_total_items(manifest.parts.len() as u64);
    ctx.progress.add_total_bytes(manifest.original_size);
    let (temp_path, mut output, mut guard) = create_temp(&output_path)?;
    (|| {
        let mut whole = blake3::Hasher::new();
        let mut buf = vec![0u8; IO_CHUNK];
        let mut joined = 0u64;
        for (index, part) in manifest.parts.iter().enumerate() {
            ctx.cancel.check()?;
            ctx.progress.set_current(format!(
                "Verifying part {} of {}",
                index + 1,
                manifest.parts.len()
            ));
            let part_path = parent.join(&part.name);
            let mut input = match File::open(&part_path) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(CoreError::Invalid(format!("missing part {}", part.name)));
                }
                Err(e) => return Err(CoreError::io(&part_path, e)),
            };
            let actual_size = input
                .metadata()
                .map_err(|e| CoreError::io(&part_path, e))?
                .len();
            if actual_size < part.size {
                return Err(CoreError::Invalid(format!(
                    "truncated part {}: expected {} bytes, found {actual_size}",
                    part.name, part.size
                )));
            }
            if actual_size > part.size {
                return Err(CoreError::Invalid(format!(
                    "corrupt part {}: expected {} bytes, found {actual_size}",
                    part.name, part.size
                )));
            }

            let mut part_hash = blake3::Hasher::new();
            let mut part_read = 0u64;
            while part_read < part.size {
                ctx.cancel.check()?;
                let want = ((part.size - part_read) as usize).min(buf.len());
                let n = read_retry(&mut input, &mut buf[..want])
                    .map_err(|e| CoreError::io(&part_path, e))?;
                if n == 0 {
                    return Err(CoreError::Invalid(format!(
                        "truncated part {}: expected {} bytes, found {part_read}",
                        part.name, part.size
                    )));
                }
                part_hash.update(&buf[..n]);
                whole.update(&buf[..n]);
                output
                    .write_all(&buf[..n])
                    .map_err(|e| CoreError::io(&temp_path, e))?;
                part_read += n as u64;
                joined += n as u64;
                ctx.progress.add_bytes(n as u64);
            }
            if part_hash.finalize().to_hex().as_str() != part.blake3 {
                return Err(CoreError::Invalid(format!(
                    "corrupt part {}: BLAKE3 mismatch",
                    part.name
                )));
            }
            let mut extra = [0u8; 1];
            if read_retry(&mut input, &mut extra).map_err(|e| CoreError::io(&part_path, e))? != 0 {
                return Err(CoreError::Invalid(format!(
                    "corrupt part {}: unexpected trailing bytes",
                    part.name
                )));
            }
            ctx.progress.add_items(1);
        }
        if joined != manifest.original_size {
            return Err(CoreError::Invalid(format!(
                "manifest size mismatch: expected {} bytes, joined {joined}",
                manifest.original_size
            )));
        }
        let actual = whole.finalize().to_hex().to_string();
        if actual != manifest.whole_blake3 {
            return Err(CoreError::Invalid(format!(
                "joined file BLAKE3 mismatch: expected {}, found {actual}",
                manifest.whole_blake3
            )));
        }
        output.flush().map_err(|e| CoreError::io(&temp_path, e))?;
        output
            .sync_all()
            .map_err(|e| CoreError::io(&temp_path, e))?;
        drop(output);
        ctx.cancel.check()?;
        publish_temp(&temp_path, &output_path)?;
        guard.disarm();
        ctx.progress.set_current("Join complete");
        Ok(JoinReport {
            output: output_path.clone(),
            bytes: joined,
            parts: manifest.parts.len(),
        })
    })()
}

/// Return the manifest path for a manifest input or the `.001` first part.
pub fn manifest_path_for(input: &Path) -> Result<PathBuf> {
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    if ext.eq_ignore_ascii_case("b3split") {
        return Ok(input.to_path_buf());
    }
    if ext != "001" {
        return Err(CoreError::Invalid(format!(
            "select a .b3split manifest or the first .001 part: {}",
            input.display()
        )));
    }
    Ok(input.with_extension("b3split"))
}

fn validate_manifest(manifest: &SplitManifest) -> Result<()> {
    if !is_single_component(&manifest.original_name) || manifest.original_name.ends_with(".b3split")
    {
        return Err(CoreError::Invalid(
            "manifest has an unsafe original filename".into(),
        ));
    }
    if manifest.part_size == 0 || manifest.parts.is_empty() || manifest.parts.len() > MAX_PARTS {
        return Err(CoreError::Invalid(
            "manifest has an invalid part count or size".into(),
        ));
    }
    if !valid_digest(&manifest.whole_blake3) {
        return Err(CoreError::Invalid(
            "manifest has an invalid whole-file BLAKE3 digest".into(),
        ));
    }
    let mut names = HashSet::with_capacity(manifest.parts.len());
    let mut sum = 0u64;
    for (index, part) in manifest.parts.iter().enumerate() {
        if !is_single_component(&part.name) || !names.insert(part.name.to_lowercase()) {
            return Err(CoreError::Invalid(format!(
                "manifest has an unsafe or duplicate part name: {}",
                part.name
            )));
        }
        if !valid_digest(&part.blake3) {
            return Err(CoreError::Invalid(format!(
                "manifest has an invalid digest for part {}",
                part.name
            )));
        }
        let expected_name = format!("{}.{:03}", manifest.original_name, index + 1);
        if part.name != expected_name {
            return Err(CoreError::Invalid(format!(
                "manifest part {} has an unexpected name (expected {expected_name})",
                part.name
            )));
        }
        if part.size > manifest.part_size
            || (index + 1 < manifest.parts.len() && part.size != manifest.part_size)
        {
            return Err(CoreError::Invalid(format!(
                "manifest has an invalid size for part {}",
                part.name
            )));
        }
        sum = sum
            .checked_add(part.size)
            .ok_or_else(|| CoreError::Invalid("manifest total size overflows".into()))?;
    }
    if sum != manifest.original_size || (manifest.original_size == 0 && manifest.parts.len() != 1) {
        return Err(CoreError::Invalid(format!(
            "manifest part sizes total {sum}, expected {}",
            manifest.original_size
        )));
    }
    Ok(())
}

fn is_single_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\', ':'])
        && Path::new(value).file_name().is_some_and(|n| n == value)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn create_temp(final_path: &Path) -> Result<(PathBuf, File, TempGuard)> {
    let parent = final_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    for _ in 0..32 {
        let n = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(".forge-partial-{}-{n:016x}", std::process::id());
        let path = parent.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path.clone(), file, TempGuard(Some(path)))),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(CoreError::io(final_path, e)),
        }
    }
    Err(CoreError::Invalid(format!(
        "could not create a temporary file beside {}",
        final_path.display()
    )))
}

fn publish_temp(temp: &Path, final_path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        // Windows rename fails when the destination already exists, giving us an atomic
        // no-clobber publish while also working on FAT32 volumes (which lack hard links).
        fs::rename(temp, final_path).map_err(|e| CoreError::io(final_path, e))
    }
    #[cfg(not(windows))]
    {
        // POSIX rename replaces an existing path. A same-volume hard link is atomic and
        // fails if the destination exists; removing the temp name completes the publish.
        fs::hard_link(temp, final_path).map_err(|e| CoreError::io(final_path, e))?;
        if let Err(e) = fs::remove_file(temp) {
            let _ = fs::remove_file(final_path);
            return Err(CoreError::io(temp, e));
        }
        Ok(())
    }
}

fn read_retry(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

struct TempGuard(Option<PathBuf>);

impl TempGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_jobs::{CancelToken, Progress};
    use tempfile::TempDir;

    fn ctx<'a>(cancel: &'a CancelToken, progress: &'a Progress) -> JobCtx<'a> {
        JobCtx::new(cancel, progress)
    }

    fn run_split(path: &Path, part_size: u64) -> Result<SplitReport> {
        let cancel = CancelToken::new();
        let progress = Progress::default();
        split_file(path, part_size, &ctx(&cancel, &progress))
    }

    fn run_join(path: &Path) -> Result<JoinReport> {
        let cancel = CancelToken::new();
        let progress = Progress::default();
        join_files(path, &ctx(&cancel, &progress))
    }

    #[test]
    fn roundtrip_unicode_and_non_multiple_size() {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("café 雪.bin");
        let bytes: Vec<u8> = (0..31).map(|n| (n * 7) as u8).collect();
        fs::write(&input, &bytes).unwrap();
        let report = run_split(&input, 8).unwrap();
        assert_eq!(report.parts.len(), 4);
        assert_eq!(fs::metadata(&report.parts[3]).unwrap().len(), 7);
        fs::remove_file(&input).unwrap();
        let joined = run_join(&report.manifest).unwrap();
        assert_eq!(joined.output, input);
        assert_eq!(fs::read(input).unwrap(), bytes);
    }

    #[test]
    fn zero_byte_file_has_one_empty_part() {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("empty.dat");
        fs::write(&input, []).unwrap();
        let report = run_split(&input, 16).unwrap();
        assert_eq!(report.parts.len(), 1);
        assert_eq!(fs::metadata(&report.parts[0]).unwrap().len(), 0);
        fs::remove_file(&input).unwrap();
        run_join(&report.parts[0]).unwrap();
        assert_eq!(fs::metadata(input).unwrap().len(), 0);
    }

    #[test]
    fn missing_part_names_the_missing_part() {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("missing.bin");
        fs::write(&input, b"some content with several parts").unwrap();
        let report = run_split(&input, 7).unwrap();
        fs::remove_file(&input).unwrap();
        fs::remove_file(&report.parts[1]).unwrap();
        let error = run_join(&report.manifest).unwrap_err().to_string();
        assert!(error.contains("missing part missing.bin.002"), "{error}");
        assert!(!input.exists());
        assert!(!temp.path().read_dir().unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".forge-partial-")
        }));
    }

    #[test]
    fn changed_part_hash_is_reported_and_output_is_removed() {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("changed.bin");
        fs::write(&input, b"a little content").unwrap();
        let report = run_split(&input, 6).unwrap();
        fs::remove_file(&input).unwrap();
        let mut changed = fs::read(&report.parts[0]).unwrap();
        changed[0] ^= 0xff;
        fs::write(&report.parts[0], changed).unwrap();
        let error = run_join(&report.manifest).unwrap_err().to_string();
        assert!(
            error.contains("corrupt part changed.bin.001: BLAKE3 mismatch"),
            "{error}"
        );
        assert!(!input.exists());
    }

    #[test]
    fn truncated_part_is_identified_exactly() {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("truncated.bin");
        fs::write(&input, b"0123456789abcdef").unwrap();
        let report = run_split(&input, 8).unwrap();
        fs::remove_file(&input).unwrap();
        fs::write(&report.parts[1], b"short").unwrap();
        let error = run_join(&report.manifest).unwrap_err().to_string();
        assert!(
            error.contains("truncated part truncated.bin.002"),
            "{error}"
        );
        assert!(!input.exists());
    }

    #[test]
    fn whole_file_hash_mismatch_does_not_publish_output() {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("whole-hash.bin");
        fs::write(&input, b"valid part hashes but wrong manifest whole hash").unwrap();
        let report = run_split(&input, 10).unwrap();
        fs::remove_file(&input).unwrap();
        let mut manifest: SplitManifest =
            serde_json::from_slice(&fs::read(&report.manifest).unwrap()).unwrap();
        manifest.whole_blake3 = "0".repeat(64);
        fs::write(&report.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let error = run_join(&report.manifest).unwrap_err().to_string();
        assert!(error.contains("joined file BLAKE3 mismatch"), "{error}");
        assert!(!input.exists());
        assert!(!temp.path().read_dir().unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".forge-partial-")
        }));
    }

    #[test]
    fn split_and_join_refuse_to_overwrite() {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("keep.bin");
        fs::write(&input, b"original").unwrap();
        fs::write(temp.path().join("keep.bin.001"), b"existing part").unwrap();
        assert!(matches!(
            run_split(&input, 3),
            Err(CoreError::AlreadyExists(_))
        ));

        let manifest_path = temp.path().join("join.b3split");
        let output = temp.path().join("joined.bin");
        let part = temp.path().join("joined.bin.001");
        let data = b"join data";
        fs::write(&part, data).unwrap();
        let manifest = SplitManifest {
            original_name: "joined.bin".into(),
            original_size: data.len() as u64,
            part_size: 32,
            parts: vec![SplitPart {
                name: "joined.bin.001".into(),
                size: data.len() as u64,
                blake3: blake3::hash(data).to_hex().to_string(),
            }],
            whole_blake3: blake3::hash(data).to_hex().to_string(),
        };
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        fs::write(&output, b"do not replace").unwrap();
        assert!(matches!(
            run_join(&manifest_path),
            Err(CoreError::AlreadyExists(_))
        ));
        assert_eq!(fs::read(output).unwrap(), b"do not replace");
    }

    #[test]
    fn cancellation_leaves_no_partial_output() {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("cancel.bin");
        let mut source = File::create(&input).unwrap();
        let block = vec![0x5a; IO_CHUNK];
        for _ in 0..64 {
            source.write_all(&block).unwrap();
        }
        drop(source);
        let cancel = CancelToken::new();
        let progress = std::sync::Arc::new(Progress::default());
        let cancel_worker = cancel.clone();
        let progress_worker = progress.clone();
        let cancel_observer = cancel.clone();
        let progress_observer = progress.clone();
        let observer = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while progress_observer.items_done.load(Ordering::Relaxed) == 0
                && std::time::Instant::now() < deadline
            {
                std::thread::yield_now();
            }
            cancel_observer.cancel();
        });
        let worker = std::thread::spawn(move || {
            let ctx = JobCtx::new(&cancel_worker, &progress_worker);
            split_file(&input, 8 * IO_CHUNK as u64, &ctx)
        });
        let result = worker.join().unwrap();
        observer.join().unwrap();
        assert!(matches!(result, Err(CoreError::Cancelled)));
        let names: Vec<_> = temp
            .path()
            .read_dir()
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("cancel.bin")]);
    }

    #[test]
    fn join_cancellation_removes_partial_output() {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("join-cancel.bin");
        let mut source = File::create(&input).unwrap();
        let block = vec![0x35; IO_CHUNK];
        for _ in 0..64 {
            source.write_all(&block).unwrap();
        }
        drop(source);
        let split = run_split(&input, 8 * IO_CHUNK as u64).unwrap();
        fs::remove_file(&input).unwrap();

        let cancel = CancelToken::new();
        let progress = std::sync::Arc::new(Progress::default());
        let cancel_worker = cancel.clone();
        let progress_worker = progress.clone();
        let cancel_observer = cancel.clone();
        let progress_observer = progress.clone();
        let observer = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while progress_observer.bytes_done.load(Ordering::Relaxed) == 0
                && std::time::Instant::now() < deadline
            {
                std::thread::yield_now();
            }
            cancel_observer.cancel();
        });
        let manifest = split.manifest.clone();
        let worker = std::thread::spawn(move || {
            let ctx = JobCtx::new(&cancel_worker, &progress_worker);
            join_files(&manifest, &ctx)
        });
        let result = worker.join().unwrap();
        observer.join().unwrap();
        assert!(matches!(result, Err(CoreError::Cancelled)));
        assert!(!input.exists());
        assert!(!temp.path().read_dir().unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".forge-partial-")
        }));
    }

    #[test]
    fn rejects_non_first_part_input() {
        let error = manifest_path_for(Path::new("name.bin.002")).unwrap_err();
        assert!(error.to_string().contains("first .001 part"));
    }
}
