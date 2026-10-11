//! File hashing and checksum manifests.
//!
//! * **BLAKE3** — full-content hash used for copy verification, duplicate confirmation and
//!   manifests. Fast (multi-GB/s, SIMD) and cryptographically strong.
//! * **xxh3 partial hash** — size + first/last 64 KiB; a cheap prefilter for duplicate
//!   staging. Never used as proof of equality on its own.
//!
//! SHA-256/MD5 are deliberately not provided: BLAKE3 covers integrity needs and the manifest
//! format is `b3sum`-compatible, so users can verify with an off-the-shelf tool.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use forge_jobs::{CancelToken, Progress};
use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::Xxh3;

use crate::error::{CoreError, Result};
use crate::ops::{WalkItem, walk_tree};
use crate::paths;

/// Read chunk size for hashing (1 MiB: large enough for sequential throughput, small enough
/// for responsive cancellation).
pub const HASH_CHUNK: usize = 1024 * 1024;
/// Bytes read from each end of the file for [`partial_hash`].
pub const PARTIAL_BYTES: u64 = 64 * 1024;

/// A 32-byte BLAKE3 digest.
pub type Digest = [u8; 32];

/// A path that could not be processed, with the error message.
pub type PathError = (PathBuf, String);

/// Stream a reader through BLAKE3, checking `cancel` (pause/cancel) per chunk and adding
/// bytes to `progress` if given. `path` is only used for error messages.
pub fn hash_reader<R: Read>(
    mut r: R,
    path: &Path,
    cancel: &CancelToken,
    progress: Option<&Progress>,
) -> Result<Digest> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        cancel.check()?;
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(CoreError::io(path, e)),
        };
        hasher.update(&buf[..n]);
        if let Some(p) = progress {
            p.add_bytes(n as u64);
        }
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Full BLAKE3 hash of a file (streaming, cancellable).
pub fn hash_file(path: &Path, cancel: &CancelToken, progress: Option<&Progress>) -> Result<Digest> {
    let f = File::open(path).map_err(|e| CoreError::io(path, e))?;
    hash_reader(f, path, cancel, progress)
}

/// Cheap staging hash: xxh3-64 over (size, first 64 KiB, last 64 KiB). Files up to 128 KiB
/// are hashed completely.
pub fn partial_hash(path: &Path) -> Result<u64> {
    let mut f = File::open(path).map_err(|e| CoreError::io(path, e))?;
    let size = f.metadata().map_err(|e| CoreError::io(path, e))?.len();
    let mut h = Xxh3::new();
    h.update(&size.to_le_bytes());
    let io = |e| CoreError::io(path, e);
    if size <= PARTIAL_BYTES * 2 {
        let mut all = Vec::with_capacity(size as usize);
        f.read_to_end(&mut all).map_err(io)?;
        h.update(&all);
    } else {
        // Only large files need the 64 KiB window; small ones (the common case in bulk
        // duplicate scans) skip allocating and zeroing it.
        let mut buf = vec![0u8; PARTIAL_BYTES as usize];
        f.read_exact(&mut buf).map_err(io)?;
        h.update(&buf);
        f.seek(SeekFrom::End(-(PARTIAL_BYTES as i64))).map_err(io)?;
        f.read_exact(&mut buf).map_err(io)?;
        h.update(&buf);
    }
    Ok(h.digest())
}

/// Lowercase hex encoding.
pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

/// Parse a 64-char hex BLAKE3 digest (case-insensitive).
pub fn parse_hex_digest(s: &str) -> Option<Digest> {
    let s = s.as_bytes();
    if s.len() != 64 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (nib(s[2 * i])? << 4) | nib(s[2 * i + 1])?;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Manifests (b3sum format)
// ---------------------------------------------------------------------------

/// One manifest line: digest + path relative to the manifest root, `/`-separated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub hash: Digest,
    pub rel_path: String,
}

/// Format entries as `b3sum` output: `<hex>  <path>\n`. Paths containing `\` or newlines
/// are escaped the way `b3sum` does (line prefixed with `\`).
pub fn format_manifest(entries: &[ManifestEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        let needs_escape = e.rel_path.contains(['\\', '\n']);
        if needs_escape {
            out.push('\\');
        }
        out.push_str(&to_hex(&e.hash));
        out.push_str("  ");
        if needs_escape {
            out.push_str(&e.rel_path.replace('\\', "\\\\").replace('\n', "\\n"));
        } else {
            out.push_str(&e.rel_path);
        }
        out.push('\n');
    }
    out
}

/// Parse `b3sum`-format text. Blank lines are ignored; malformed lines are errors.
pub fn parse_manifest(text: &str) -> Result<Vec<ManifestEntry>> {
    let mut out = Vec::new();
    for (no, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let bad = || CoreError::Invalid(format!("manifest line {}: malformed", no + 1));
        let (escaped, line) = match line.strip_prefix('\\') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let (hex, path) = line.split_once("  ").ok_or_else(bad)?;
        let hash = parse_hex_digest(hex).ok_or_else(bad)?;
        let rel_path = if escaped {
            unescape(path).ok_or_else(bad)?
        } else {
            path.to_string()
        };
        if rel_path.is_empty() {
            return Err(bad());
        }
        out.push(ManifestEntry { hash, rel_path });
    }
    Ok(out)
}

fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next()? {
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Hash every regular file under `root` (links are skipped, never followed) and return
/// manifest entries sorted by path. `exclude` paths (e.g. the manifest file itself) are
/// skipped.
pub fn build_manifest(
    root: &Path,
    exclude: &[PathBuf],
    cancel: &CancelToken,
    progress: Option<&Progress>,
) -> Result<(Vec<ManifestEntry>, Vec<PathError>)> {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    walk_tree(root, cancel, |item| match item {
        WalkItem::Entry(e) if e.is_file() => {
            if !exclude.iter().any(|x| paths::paths_equal(x, &e.path)) {
                files.push((e.path.clone(), e.rel.clone()));
            }
        }
        WalkItem::Entry(_) => {}
        WalkItem::Error { path, error } => errors.push((path.clone(), error.to_string())),
    })?;
    let mut out = Vec::with_capacity(files.len());
    for (path, rel) in files {
        cancel.check()?;
        if let Some(p) = progress {
            p.set_current(path.display().to_string());
        }
        match hash_file(&path, cancel, progress) {
            Ok(hash) => out.push(ManifestEntry {
                hash,
                rel_path: paths::to_slash(&rel),
            }),
            Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
            Err(e) => errors.push((path, e.to_string())),
        }
        if let Some(p) = progress {
            p.add_items(1);
        }
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok((out, errors))
}

/// Write a manifest atomically (temp file + rename).
pub fn write_manifest(path: &Path, entries: &[ManifestEntry]) -> Result<()> {
    write_atomic(path, format_manifest(entries).as_bytes())
}

/// Write bytes to `path` via a temp file in the same directory, then rename into place.
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = dir.join(crate::ops::partial_name());
    let res = (|| -> io::Result<()> {
        let mut f = File::create_new(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(CoreError::io(path, e));
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifyReport {
    pub ok: Vec<String>,
    /// Content differs from the recorded digest (corruption or modification).
    pub mismatched: Vec<String>,
    pub missing: Vec<String>,
    /// Exists but could not be read, or the manifest path itself was unsafe.
    pub unreadable: Vec<(String, String)>,
}

impl VerifyReport {
    pub fn all_ok(&self) -> bool {
        self.mismatched.is_empty() && self.missing.is_empty() && self.unreadable.is_empty()
    }
}

/// Verify entries against files under `root`. Manifest paths are untrusted and resolved
/// with [`paths::safe_join`], so a manifest cannot make us read outside `root`.
pub fn verify_entries(
    root: &Path,
    entries: &[ManifestEntry],
    cancel: &CancelToken,
    progress: Option<&Progress>,
) -> Result<VerifyReport> {
    let mut r = VerifyReport::default();
    for e in entries {
        cancel.check()?;
        let path = match paths::safe_join(root, &e.rel_path) {
            Ok(p) => p,
            Err(err) => {
                r.unreadable.push((e.rel_path.clone(), err.to_string()));
                continue;
            }
        };
        match hash_file(&path, cancel, progress) {
            Ok(h) if h == e.hash => r.ok.push(e.rel_path.clone()),
            Ok(_) => r.mismatched.push(e.rel_path.clone()),
            Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
            Err(CoreError::NotFound(_)) => r.missing.push(e.rel_path.clone()),
            Err(err) => r.unreadable.push((e.rel_path.clone(), err.to_string())),
        }
        if let Some(p) = progress {
            p.add_items(1);
        }
    }
    Ok(r)
}

/// Read a manifest file and verify it against files relative to the manifest's directory.
pub fn verify_manifest(
    manifest: &Path,
    cancel: &CancelToken,
    progress: Option<&Progress>,
) -> Result<VerifyReport> {
    let text = fs::read_to_string(manifest).map_err(|e| CoreError::io(manifest, e))?;
    let entries = parse_manifest(&text)?;
    let root = manifest.parent().unwrap_or(Path::new("."));
    verify_entries(root, &entries, cancel, progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake3_matches_reference() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f.bin");
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&p, &data).unwrap();
        let prog = Progress::default();
        let h = hash_file(&p, &CancelToken::new(), Some(&prog)).unwrap();
        assert_eq!(h, *blake3::hash(&data).as_bytes());
        assert_eq!(prog.snapshot().bytes_done, data.len() as u64);
        assert_eq!(to_hex(&h), blake3::hash(&data).to_hex().as_str());
        assert_eq!(parse_hex_digest(&to_hex(&h)), Some(h));
    }

    #[test]
    fn hash_cancel() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f");
        fs::write(&p, b"abc").unwrap();
        let c = CancelToken::new();
        c.cancel();
        assert!(matches!(hash_file(&p, &c, None), Err(CoreError::Cancelled)));
    }

    #[test]
    fn partial_hash_distinguishes_size_and_ends() {
        let d = tempfile::tempdir().unwrap();
        let big = vec![7u8; 300_000];
        let mut big2 = big.clone();
        big2[150_000] = 8; // middle differs: partial hash cannot see it (by design)
        let mut big3 = big.clone();
        big3[299_999] = 8; // tail differs
        let w = |n: &str, b: &[u8]| {
            let p = d.path().join(n);
            fs::write(&p, b).unwrap();
            p
        };
        let (a, b, c) = (w("a", &big), w("b", &big2), w("c", &big3));
        let e = w("e", &big[..299_999]);
        assert_eq!(partial_hash(&a).unwrap(), partial_hash(&b).unwrap());
        assert_ne!(partial_hash(&a).unwrap(), partial_hash(&c).unwrap());
        assert_ne!(partial_hash(&a).unwrap(), partial_hash(&e).unwrap());
    }

    #[test]
    fn manifest_roundtrip_and_corruption() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("sub/日本")).unwrap();
        fs::write(d.path().join("a.txt"), b"hello").unwrap();
        fs::write(d.path().join("sub/日本/b.raw"), b"world").unwrap();
        fs::write(d.path().join("sub/c d.txt"), b"x").unwrap();
        let mpath = d.path().join("checksums.b3");
        let (entries, errs) = build_manifest(
            d.path(),
            std::slice::from_ref(&mpath),
            &CancelToken::new(),
            None,
        )
        .unwrap();
        assert!(errs.is_empty());
        assert_eq!(entries.len(), 3);
        write_manifest(&mpath, &entries).unwrap();
        let text = fs::read_to_string(&mpath).unwrap();
        assert!(text.contains(&format!("{}  a.txt\n", blake3::hash(b"hello").to_hex())));
        assert!(text.contains("  sub/日本/b.raw\n"));
        assert_eq!(parse_manifest(&text).unwrap(), entries);

        let ok = verify_manifest(&mpath, &CancelToken::new(), None).unwrap();
        assert!(ok.all_ok());
        assert_eq!(ok.ok.len(), 3);

        fs::write(d.path().join("sub/日本/b.raw"), b"w0rld").unwrap();
        fs::remove_file(d.path().join("a.txt")).unwrap();
        let bad = verify_manifest(&mpath, &CancelToken::new(), None).unwrap();
        assert!(!bad.all_ok());
        assert_eq!(bad.mismatched, vec!["sub/日本/b.raw".to_string()]);
        assert_eq!(bad.missing, vec!["a.txt".to_string()]);
    }

    #[test]
    fn manifest_escaping_and_hostile_paths() {
        let e = vec![ManifestEntry {
            hash: [1; 32],
            rel_path: "we\\ird\nname".into(),
        }];
        let text = format_manifest(&e);
        assert!(text.starts_with('\\'));
        assert_eq!(parse_manifest(&text).unwrap(), e);
        assert!(parse_manifest("zz  x\n").is_err());

        let d = tempfile::tempdir().unwrap();
        let hostile = vec![ManifestEntry {
            hash: [0; 32],
            rel_path: "../outside.txt".into(),
        }];
        let r = verify_entries(d.path(), &hostile, &CancelToken::new(), None).unwrap();
        assert_eq!(r.unreadable.len(), 1);
    }
}
