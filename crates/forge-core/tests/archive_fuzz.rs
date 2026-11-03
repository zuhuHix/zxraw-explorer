//! Stable companion of the libFuzzer targets in `fuzz/` (see docs/TESTING.md):
//!
//! * `replay_regressions` feeds every input committed under `fuzz/regressions/<target>/`
//!   (minimised crashes, hangs and blow-ups the fuzzers found) to its target;
//! * `mutation_smoke` runs a short fixed-seed mutation loop over seed archives (the committed
//!   fixtures plus archives made here), a few seconds per target;
//! * `write_fuzz_seeds` (ignored) writes those seeds to `fuzz/seeds/<target>/` for
//!   `cargo fuzz`.
//!
//! Every input must finish without a panic, within a time budget, and without allocating
//! more than [`MAX_PEAK`] at once (a tracking allocator; one allocation over
//! [`MAX_ONE`] aborts the test binary on the spot).

use std::alloc::{GlobalAlloc, Layout, System};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use forge_core::archive::fuzz::{self, Target};
use forge_core::archive::{self, CreateOptions, Encryption, Password, Preset};
use forge_core::ops::JobCtx;
use forge_jobs::{CancelToken, Progress};

// ---------------------------------------------------------------------------
// Tracking allocator
// ---------------------------------------------------------------------------

/// Largest single allocation: past it the process aborts (it can't be caught).
const MAX_ONE: usize = 512 << 20;
/// Most bytes live at once while one input runs, above what was live before it.
const MAX_PEAK: usize = 512 << 20;
/// Wall time one input may take (debug build, the machine possibly busy).
const MAX_TIME: Duration = Duration::from_secs(20);

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Tracking;

fn grow(by: usize) {
    let live = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

fn too_big(size: usize) -> ! {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr(),
        "archive_fuzz: one allocation of {size} bytes"
    );
    std::process::abort();
}

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.size() > MAX_ONE {
            too_big(l.size());
        }
        grow(l.size());
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if l.size() > MAX_ONE {
            too_big(l.size());
        }
        grow(l.size());
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        if new > MAX_ONE {
            too_big(new);
        }
        if new >= l.size() {
            grow(new - l.size());
        } else {
            LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
        }
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

/// One test at a time: the peak is process-wide.
static SERIAL: Mutex<()> = Mutex::new(());

/// Run one input; `Err` describes what went wrong (panic, time, memory).
fn check_one(target: Target, data: &[u8]) -> Result<(), String> {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let start = Instant::now();
    let outcome = catch_unwind(AssertUnwindSafe(|| fuzz::run(target, data)));
    let took = start.elapsed();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    if let Err(p) = outcome {
        let msg = p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        return Err(format!("panicked: {msg}"));
    }
    if took > MAX_TIME {
        return Err(format!("took {took:?}"));
    }
    if peak > MAX_PEAK {
        return Err(format!("peak allocation {} MiB", peak >> 20));
    }
    Ok(())
}

/// Save a failing input where it can be replayed, and say where.
fn keep_failure(target: Target, data: &[u8], what: &str) -> String {
    let dir = std::env::temp_dir()
        .join("zxraw-fuzz-failures")
        .join(target.name());
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("{:016x}", xxhash(data)));
    let _ = std::fs::write(&file, data);
    format!(
        "{}: {what} (input saved to {})",
        target.name(),
        file.display()
    )
}

fn xxhash(data: &[u8]) -> u64 {
    // FNV-1a: enough to name a file.
    data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

// ---------------------------------------------------------------------------
// Seeds
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/archive/fixtures")
}

/// Small source files to archive: text, binary, empty, a folder tree.
fn make_sources(root: &Path) -> Vec<PathBuf> {
    let top = root.join("src");
    std::fs::create_dir_all(top.join("docs/deep")).unwrap();
    std::fs::create_dir_all(top.join("empty")).unwrap();
    let text: String = (0..300)
        .map(|i| format!("line {i} of some text\n"))
        .collect();
    std::fs::write(top.join("readme.txt"), &text).unwrap();
    std::fs::write(top.join("docs/notes.md"), text.repeat(3)).unwrap();
    let bin: Vec<u8> = (0..4000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    std::fs::write(top.join("docs/deep/data.bin"), &bin).unwrap();
    std::fs::write(top.join("zero.txt"), b"").unwrap();
    vec![top]
}

/// Archives this engine writes, by output name.
fn generated(root: &Path) -> Vec<(String, Vec<u8>)> {
    let sources = make_sources(root);
    let single = vec![sources[0].join("readme.txt")];
    let cancel = CancelToken::new();
    let progress = Progress::default();
    let ctx = JobCtx::new(&cancel, &progress);
    let pw = || Password::new(fuzz::PASSWORD);
    let mut plans: Vec<(String, CreateOptions, &Vec<PathBuf>)> = Vec::new();
    for preset in [Preset::Fast, Preset::Balanced, Preset::Zstd] {
        plans.push((
            format!("{preset:?}.zip"),
            CreateOptions {
                preset,
                ..Default::default()
            },
            &sources,
        ));
    }
    plans.push((
        "aes.zip".into(),
        CreateOptions {
            encryption: Encryption::Aes256(pw()),
            ..Default::default()
        },
        &sources,
    ));
    plans.push((
        "zipcrypto.zip".into(),
        CreateOptions {
            encryption: Encryption::ZipCryptoLegacy(pw()),
            ..Default::default()
        },
        &sources,
    ));
    plans.push(("solid.7z".into(), CreateOptions::default(), &sources));
    plans.push((
        "nonsolid.7z".into(),
        CreateOptions {
            solid: false,
            ..Default::default()
        },
        &sources,
    ));
    plans.push((
        "aes.7z".into(),
        CreateOptions {
            encryption: Encryption::Aes256(pw()),
            encrypt_names: false,
            ..Default::default()
        },
        &sources,
    ));
    plans.push((
        "aes-names.7z".into(),
        CreateOptions {
            encryption: Encryption::Aes256(pw()),
            ..Default::default()
        },
        &sources,
    ));
    for ext in [
        "tar", "tar.gz", "tar.bz2", "tar.xz", "tar.zst", "tar.lz4", "tar.br",
    ] {
        plans.push((format!("gen.{ext}"), CreateOptions::default(), &sources));
    }
    for ext in ["gz", "bz2", "xz", "zst", "lz4", "br"] {
        plans.push((
            format!("readme.txt.{ext}"),
            CreateOptions::default(),
            &single,
        ));
    }
    let out_dir = root.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut out = Vec::new();
    for (name, opts, srcs) in plans {
        let path = out_dir.join(&name);
        match archive::create(srcs, &path, &opts, &ctx) {
            Ok(_) => out.push((name, std::fs::read(&path).unwrap())),
            Err(e) => panic!("seed {name}: {e}"),
        }
    }
    out
}

/// `(codec extension, is a tar)` of a stream file name, if it is one.
fn stream_kind(name: &str) -> Option<(&'static str, bool)> {
    for ext in ["gz", "bz2", "xz", "zst", "lz4", "br"] {
        if name.ends_with(&format!(".tar.{ext}")) {
            return Some((ext_static(ext), true));
        }
        if name.ends_with(&format!(".{ext}")) {
            return Some((ext_static(ext), false));
        }
    }
    None
}

fn ext_static(ext: &str) -> &'static str {
    ["gz", "bz2", "xz", "zst", "lz4", "br"]
        .into_iter()
        .find(|e| *e == ext)
        .unwrap()
}

/// Seeds for every target, from the fixtures and freshly made archives.
fn seeds() -> Vec<(Target, Vec<u8>)> {
    let tmp = tempfile::tempdir().unwrap();
    let mut files = generated(tmp.path());
    for e in std::fs::read_dir(fixtures_dir()).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if name.ends_with(".sh") || name.ends_with(".py") {
            continue;
        }
        files.push((name, std::fs::read(&p).unwrap()));
    }
    let mut out = Vec::new();
    for (name, data) in files {
        out.push((Target::Detect, data.clone()));
        if name.ends_with(".zip") {
            out.push((Target::Zip, data.clone()));
            // An encryption peek input: [mode, check, the bytes after the first local header].
            let raw = data.get(30..).unwrap_or_default();
            let mut peek = vec![2u8, raw.get(11).copied().unwrap_or(0)];
            peek.extend_from_slice(&raw[..raw.len().min(256)]);
            out.push((Target::ZipPeek, peek));
        } else if name.ends_with(".7z") {
            out.push((Target::SevenZ, data.clone()));
            out.push((Target::SevenZGuard, data.clone()));
        } else if name.ends_with(".rar") || name.contains(".r0") {
            out.push((Target::Rar, data.clone()));
        } else if name.ends_with(".tar") {
            out.push((Target::Tar, data.clone()));
            out.push((Target::TarWalk, data.clone()));
        } else if let Some((ext, tar)) = stream_kind(&name) {
            let sel = fuzz::codec_selector(ext, tar).unwrap();
            let mut with_sel = vec![sel];
            with_sel.extend_from_slice(&data);
            out.push((Target::Stream, with_sel.clone()));
            out.push((Target::CodecWalk, with_sel));
        }
    }
    out
}

fn regressions() -> Vec<(Target, PathBuf)> {
    let root = repo_root().join("fuzz/regressions");
    let mut out = Vec::new();
    for t in Target::ALL {
        let Ok(dir) = std::fs::read_dir(root.join(t.name())) else {
            continue;
        };
        for e in dir {
            out.push((t, e.unwrap().path()));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn replay_regressions() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut failures = Vec::new();
    let inputs = regressions();
    for (t, path) in &inputs {
        let data = std::fs::read(path).unwrap();
        if let Err(what) = check_one(*t, &data) {
            failures.push(format!("{}: {what}", path.display()));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} regressions failed:\n{}",
        failures.len(),
        inputs.len(),
        failures.join("\n")
    );
}

/// xorshift64*: the smoke run must be the same every time.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

const INTERESTING: [u64; 10] = [
    0,
    1,
    0x7F,
    0x80,
    0xFF,
    0xFFFF,
    0x7FFF_FFFF,
    0xFFFF_FFFF,
    1 << 40,
    u64::MAX,
];

/// A few random edits of `seed`, sometimes spliced with `other`. The first byte of a
/// Stream/CodecWalk input (the codec selector) is kept.
fn mutate(rng: &mut Rng, seed: &[u8], other: &[u8], keep: usize) -> Vec<u8> {
    let mut d = seed.to_vec();
    for _ in 0..1 + rng.below(4) {
        let len = d.len();
        if len <= keep {
            d.extend_from_slice(&other[..other.len().min(64)]);
            continue;
        }
        let at = keep + rng.below(len - keep);
        match rng.below(8) {
            0 => d[at] ^= 1 << rng.below(8),
            1 => d[at] = INTERESTING[rng.below(INTERESTING.len())] as u8,
            2 => {
                let v = INTERESTING[rng.below(INTERESTING.len())].to_le_bytes();
                let w = [2, 4, 8][rng.below(3)];
                let end = (at + w).min(len);
                d[at..end].copy_from_slice(&v[..end - at]);
            }
            3 => {
                let end = (at + 1 + rng.below(64)).min(len);
                d.drain(at..end);
            }
            4 => {
                let end = (at + 1 + rng.below(256)).min(len);
                let chunk = d[at..end].to_vec();
                let to = keep + rng.below(d.len() - keep + 1);
                d.splice(to..to, chunk);
            }
            5 => d.truncate(at),
            6 => {
                // Splice: our head, the other input's tail.
                let from = rng.below(other.len().max(1));
                d.truncate(at);
                d.extend_from_slice(other.get(from..).unwrap_or_default());
            }
            _ => d[at] = rng.next() as u8,
        }
    }
    d
}

/// Mutated inputs per target: the cheap in-memory ones get more.
fn iterations(t: Target) -> usize {
    match t {
        Target::TarWalk => 3000,
        Target::ZipPeek | Target::CodecWalk | Target::SevenZGuard => 300,
        _ => 120,
    }
}

#[test]
fn mutation_smoke() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let seeds = seeds();
    let mut failures = Vec::new();
    for t in Target::ALL {
        let mine: Vec<&[u8]> = seeds
            .iter()
            .filter(|(s, _)| *s == t)
            .map(|(_, d)| d.as_slice())
            .collect();
        assert!(!mine.is_empty(), "no seeds for {}", t.name());
        let keep = usize::from(matches!(t, Target::Stream | Target::CodecWalk));
        let mut rng = Rng(0x5EED_0000 + t as u64);
        let start = Instant::now();
        for seed in &mine {
            if let Err(what) = check_one(t, seed) {
                failures.push(keep_failure(t, seed, &format!("seed: {what}")));
            }
        }
        for _ in 0..iterations(t) {
            let seed = mine[rng.below(mine.len())];
            let other = mine[rng.below(mine.len())];
            let input = mutate(&mut rng, seed, other, keep);
            if let Err(what) = check_one(t, &input) {
                failures.push(keep_failure(t, &input, &what));
            }
        }
        eprintln!(
            "{}: {} inputs in {:?}",
            t.name(),
            mine.len() + iterations(t),
            start.elapsed()
        );
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// `cargo test -p forge-core --test archive_fuzz -- --ignored write_fuzz_seeds`
#[test]
#[ignore]
fn write_fuzz_seeds() {
    let root = repo_root().join("fuzz/seeds");
    for (i, (t, data)) in seeds().into_iter().enumerate() {
        let dir = root.join(t.name());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("seed-{i:03}")), data).unwrap();
    }
}
