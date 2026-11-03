//! Fuzzing entry points (not a public API). One [`Target`] per untrusted-input path of the
//! archive engine; [`run`] feeds it one input under tight [`limits`]. Shared by the
//! libFuzzer targets in `fuzz/` (nightly, `cargo fuzz`) and the stable replay and mutation
//! smoke test in `crates/forge-core/tests/archive_fuzz.rs`.
//!
//! A target must never panic, hang or allocate without bound, whatever the input: errors
//! are fine and ignored.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use forge_jobs::{CancelToken, Progress};

use crate::ops::JobCtx;

use super::{
    ArchiveBackend, ArchiveEntry, Codec, ExtractLimits, Password, codec, crypto_peek, sevenz_guard,
    tar_backend, tar_read, zip_backend,
};

/// The password the encrypted fixtures were made with.
pub const PASSWORD: &str = "secret";

/// Entries a fuzzed archive may list.
pub const MAX_ENTRIES: usize = 1_000;

/// The limits every target runs under: small enough that a hostile header can't make the
/// engine allocate or decode much, large enough for the seed archives.
pub fn limits() -> ExtractLimits {
    ExtractLimits {
        max_entries: MAX_ENTRIES,
        max_total_bytes: 64 << 20,
        max_file_bytes: 16 << 20,
        max_ratio: 1_000,
        max_decoder_memory: 64 << 20,
    }
}

/// Bytes of one entry a sink reads before it stops (past the limits, so overruns show).
const SINK_CAP: u64 = (16 << 20) + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Content detection (`backend_for_file`), then whatever it picked, on a file with no
    /// archive extension.
    Detect,
    /// Full pipeline (list, preflight, read, check password, Test) as `.zip`.
    Zip,
    /// Full pipeline as `.7z`.
    SevenZ,
    /// Full pipeline as a plain `.tar`.
    Tar,
    /// Full pipeline as a compressed tar or a single compressed file: the first byte picks
    /// the codec and whether it is a tar.
    Stream,
    /// Full pipeline as `.rar` (UnRAR).
    Rar,
    /// The ZIP encryption peek (WinZip AES and ZipCrypto heads).
    ZipPeek,
    /// The 7z header guard alone (signature header, header walk, encoded headers).
    SevenZGuard,
    /// The tar header walk over a byte slice.
    TarWalk,
    /// A codec's header walk (`stream_info`): the first byte picks the codec.
    CodecWalk,
}

impl Target {
    pub const ALL: [Target; 10] = [
        Target::Detect,
        Target::Zip,
        Target::SevenZ,
        Target::Tar,
        Target::Stream,
        Target::Rar,
        Target::ZipPeek,
        Target::SevenZGuard,
        Target::TarWalk,
        Target::CodecWalk,
    ];

    /// Its name: the fuzz target's and its corpus folder's.
    pub fn name(self) -> &'static str {
        match self {
            Target::Detect => "detect",
            Target::Zip => "zip",
            Target::SevenZ => "sevenz",
            Target::Tar => "tar",
            Target::Stream => "stream",
            Target::Rar => "rar",
            Target::ZipPeek => "zip_peek",
            Target::SevenZGuard => "sevenz_guard",
            Target::TarWalk => "tar_walk",
            Target::CodecWalk => "codec_walk",
        }
    }

    pub fn from_name(name: &str) -> Option<Target> {
        Target::ALL.into_iter().find(|t| t.name() == name)
    }
}

/// Codecs a selector byte picks from (Stream, CodecWalk).
const CODECS: [Codec; 6] = [
    Codec::Gzip,
    Codec::Bzip2,
    Codec::Xz,
    Codec::Zstd,
    Codec::Lz4,
    Codec::Brotli,
];

/// The selector byte for `codec` (to build Stream and CodecWalk inputs from real files).
pub fn codec_selector(codec_ext: &str, tar: bool) -> Option<u8> {
    let i = CODECS.iter().position(|c| c.extension() == codec_ext)? as u8;
    Some(i | if tar { 0x80 } else { 0 })
}

fn pick_codec(sel: u8) -> (Codec, bool) {
    (
        CODECS[(sel & 0x7F) as usize % CODECS.len()],
        sel & 0x80 != 0,
    )
}

/// A folder of its own for this thread's input files, removed when the thread ends.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new() -> Self {
        let base = std::env::var_os("ZXRAW_FUZZ_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("zxraw-fuzz"));
        // One per thread: a test runs each input on a thread of its own, and one that hung
        // is left running.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = base.join(format!("{}-{n}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

thread_local! {
    static DIR: WorkDir = WorkDir::new();
}

/// Feed `data` to `target`. Never panics on its own account: a panic is a bug found.
pub fn run(target: Target, data: &[u8]) {
    match target {
        Target::ZipPeek => zip_peek(data),
        Target::TarWalk => tar_walk(data),
        Target::Detect => with_file(data, "input.bin", |p| {
            if let Ok(b) = super::backend_for_file(p) {
                pipeline(b, p);
            }
        }),
        Target::Zip => with_file(data, "input.zip", |p| pipeline(&zip_backend::ZIP, p)),
        Target::SevenZ => with_file(data, "input.7z", |p| {
            pipeline(&super::sevenz_backend::SEVEN_ZIP, p)
        }),
        Target::Tar => with_file(data, "input.tar", |p| {
            pipeline(tar_backend::tar_backend(Codec::None), p)
        }),
        Target::Rar => with_file(data, "input.rar", |p| pipeline(&super::rar_backend::RAR, p)),
        Target::Stream => {
            let Some((&sel, rest)) = data.split_first() else {
                return;
            };
            let (codec, tar) = pick_codec(sel);
            let name = if tar {
                format!("input.tar.{}", codec.extension())
            } else {
                format!("input.{}", codec.extension())
            };
            with_file(rest, &name, |p| {
                let b: &dyn ArchiveBackend = if tar {
                    tar_backend::tar_backend(codec)
                } else {
                    tar_backend::single_backend(codec)
                };
                pipeline(b, p)
            })
        }
        Target::CodecWalk => {
            let Some((&sel, rest)) = data.split_first() else {
                return;
            };
            let (codec, _) = pick_codec(sel);
            with_file(rest, "input.walk", |p| {
                let _ = codec::stream_info(codec, p, None);
                let _ = codec::stream_info_with(codec, p, codec::WalkCaps::peek(), None);
            })
        }
        Target::SevenZGuard => with_file(data, "input.7z", |p| {
            let pw = Password::new(PASSWORD);
            for password in [None, Some(&pw)] {
                if let Ok(mut f) = std::fs::File::open(p) {
                    let _ = sevenz_guard::check(p, &mut f, password, MAX_ENTRIES);
                }
            }
        }),
    }
}

/// Write `data` to this process's input file `name`, run `f` on it, then forget anything
/// cached about it (the next input reuses the name).
fn with_file(data: &[u8], name: &str, f: impl FnOnce(&Path)) {
    let path = DIR.with(|d| d.0.join(name));
    super::invalidate_cached(&path);
    if std::fs::write(&path, data).is_err() {
        return;
    }
    f(&path);
    super::invalidate_cached(&path);
}

/// Everything a hostile archive can reach before anything is written: list, preflight,
/// stream every entry (unchecked, then checked through Test), with no password and with
/// the fixtures' one.
fn pipeline(backend: &dyn ArchiveBackend, path: &Path) {
    let limits = limits();
    let cancel = CancelToken::new();
    let progress = Progress::default();
    let ctx = JobCtx::new(&cancel, &progress);
    let pw = Password::new(PASSWORD);
    let t0 = std::time::Instant::now();
    let trace = std::env::var_os("ZXRAW_FUZZ_TRACE").is_some();
    let step = |what: &str, ok: bool| {
        if trace {
            eprintln!(
                "[{:>8.3}s] {what}: {}",
                t0.elapsed().as_secs_f64(),
                if ok { "ok" } else { "err" }
            );
        }
    };
    for password in [None, Some(&pw)] {
        let listed = backend.list_progress(path, &limits, password, Some(&ctx));
        step("list", listed.is_ok());
        let Ok(listing) = listed else {
            continue;
        };
        assert!(
            listing.entries.len() <= limits.max_entries,
            "listing over max_entries"
        );
        if trace {
            for e in &listing.entries {
                eprintln!(
                    "  {:?} size {} packed {} cost {} solid {} block {:?}",
                    e.name,
                    e.size,
                    e.compressed_size,
                    e.decode_cost(),
                    e.solid,
                    e.block
                );
            }
        }
        let files: Vec<&ArchiveEntry> = listing.entries.iter().filter(|e| !e.is_dir).collect();
        let indices: Vec<usize> = files.iter().map(|e| e.index).collect();
        let pre = backend.preflight(path, &indices, password, &limits);
        step("preflight", pre.is_ok());
        if pre.is_err() {
            continue;
        }
        let checked = backend.check_password(path, &files, password);
        step("check_password", checked.is_ok());
        let mut visit = |_: &ArchiveEntry, r: crate::Result<&mut dyn Read>| -> crate::Result<()> {
            if let Ok(r) = r {
                sink(r);
            }
            Ok(())
        };
        let wanted = |e: &ArchiveEntry| !e.is_dir && e.skip.is_none();
        let read = backend.read_entries(path, &wanted, password, Some(&ctx), &mut visit);
        step("read_entries", read.is_ok());
        let tested = super::test_archive(path, password, &limits, &ctx);
        step("test", tested.is_ok());
    }
}

/// Read an entry's data to its end (or [`SINK_CAP`]), discarding it.
fn sink(r: &mut dyn Read) {
    let mut buf = [0u8; 64 << 10];
    let mut total = 0u64;
    while total < SINK_CAP {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n as u64,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

/// `[mode, check, raw...]`: the AES head (mode 0..=2) and the ZipCrypto head.
fn zip_peek(data: &[u8]) {
    let [mode, check, raw @ ..] = data else {
        return;
    };
    let mode = match mode % 3 {
        0 => zip::AesMode::Aes128,
        1 => zip::AesMode::Aes192,
        _ => zip::AesMode::Aes256,
    };
    // AES derives its key with 1,000 PBKDF2 rounds: once per input.
    if let Some(head) = crypto_peek::aes_head(raw, mode, PASSWORD.as_bytes(), 64) {
        assert!(head.len() <= 64);
    }
    for n in [0, 5, 13, 64] {
        if let Some(head) = crypto_peek::zipcrypto_head(raw, PASSWORD.as_bytes(), *check, n) {
            assert!(head.len() <= n);
        }
    }
}

/// Walk every member of a tar held in memory, reading its data.
fn tar_walk(data: &[u8]) {
    let mut p = tar_read::TarParser::new(data, MAX_ENTRIES);
    let mut members = 0usize;
    while let Ok(Some(m)) = p.next_member() {
        members += 1;
        assert!(members <= MAX_ENTRIES, "tar walk over max_members");
        assert!(
            m.start <= m.data && m.data <= m.end,
            "member offsets out of order"
        );
        if m.is_file() {
            sink(&mut p.data());
        }
    }
}
