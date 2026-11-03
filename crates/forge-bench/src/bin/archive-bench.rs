//! Archive throughput benchmark.
//!
//! Builds a reproducible synthetic corpus (fixed-seed PRNG, no `rand`), then times create and
//! extract for each corpus and each archive configuration, and prints a Markdown table.
//!
//! ```text
//! cargo run -p forge-bench --release --bin archive-bench -- \
//!     [--scale <f>] [--samples <n>] [--root <dir>] [--out <file.md>] \
//!     [--sevenzip <path> | --no-7zip]
//! ```
//!
//! Everything it writes or deletes is inside `<root>\zxraw-bench-corpus` (default root
//! `%TEMP%\zxraw-archive-bench`). That folder is created with an ownership marker, and an
//! existing one without it is refused, so `--root` can never make it delete anything else.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use forge_core::archive::{self, CreateOptions, ExtractOptions, Preset};
use forge_core::ops::JobCtx;
use forge_jobs::{CancelToken, Progress};

const MIB: f64 = 1024.0 * 1024.0;
const CORPORA: [&str; 4] = ["text", "binary", "precompressed", "small"];
const MARKER: &str = "corpus.marker";
/// The folder under `--root` that the benchmark owns: it never deletes anything outside it.
const BENCH_DIR: &str = "zxraw-bench-corpus";
/// Proof that a [`BENCH_DIR`] was made by this tool (written when it creates the folder).
const OWNER: &str = "zxraw-archive-bench.owner";
const OWNER_TEXT: &str =
    "Created by ZXraw archive-bench; everything in this folder may be deleted by it.
";
const SEVENZIP_DEFAULT: &str = r"C:\Program Files\7-Zip\7z.exe";
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

const WORDS: &[&str] = &[
    "the", "of", "and", "to", "in", "a", "is", "that", "for", "it", "as", "was", "with", "be",
    "by", "on", "not", "he", "this", "are", "or", "his", "from", "at", "which", "but", "have",
    "an", "had", "they", "you", "were", "their", "one", "all", "we", "can", "her", "has", "there",
    "been", "if", "more", "when", "will", "would", "who", "so", "no", "she", "other", "its", "may",
    "these", "time", "into", "only", "about", "new", "some", "could", "them", "than", "then",
];

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

struct Args {
    scale: f64,
    samples: usize,
    root: PathBuf,
    out: Option<PathBuf>,
    sevenzip: Option<PathBuf>,
}

fn next_value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    it.next().with_context(|| format!("{flag} needs a value"))
}

fn parse_args() -> Result<Args> {
    let mut scale = 1.0_f64;
    let mut samples = 3_usize;
    let mut root = std::env::temp_dir().join("zxraw-archive-bench");
    let mut out = None;
    let mut sevenzip_flag: Option<PathBuf> = None;
    let mut no_7zip = false;

    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--scale" => scale = next_value(&mut it, &flag)?.parse().context("--scale")?,
            "--samples" => samples = next_value(&mut it, &flag)?.parse().context("--samples")?,
            "--root" => root = PathBuf::from(next_value(&mut it, &flag)?),
            "--out" => out = Some(PathBuf::from(next_value(&mut it, &flag)?)),
            "--sevenzip" => sevenzip_flag = Some(PathBuf::from(next_value(&mut it, &flag)?)),
            "--no-7zip" => no_7zip = true,
            "--help" | "-h" => {
                println!(
                    "usage: archive-bench [--scale f] [--samples n] [--root dir] [--out file.md] \
                     [--sevenzip path | --no-7zip]"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}"),
        }
    }
    if !(scale.is_finite() && scale > 0.0) {
        bail!("--scale must be a positive number");
    }
    if samples == 0 {
        bail!("--samples must be at least 1");
    }

    let sevenzip = if no_7zip {
        None
    } else if let Some(p) = sevenzip_flag {
        if !p.is_file() {
            bail!("{} is not a file", p.display());
        }
        Some(p)
    } else {
        let d = PathBuf::from(SEVENZIP_DEFAULT);
        d.is_file().then_some(d)
    };

    Ok(Args {
        scale,
        samples,
        root,
        out,
        sevenzip,
    })
}

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

/// xorshift64: small, fixed-seed, reproducible.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn text_bytes(rng: &mut Rng, target: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(target + 16);
    let mut col = 0usize;
    while buf.len() < target {
        let w = WORDS[(rng.next_u64() % WORDS.len() as u64) as usize];
        buf.extend_from_slice(w.as_bytes());
        col += w.len() + 1;
        match rng.next_u64() % 17 {
            0 => buf.extend_from_slice(b". "),
            1 => buf.extend_from_slice(b", "),
            _ => buf.push(b' '),
        }
        if col > 76 {
            buf.push(b'\n');
            col = 0;
        }
    }
    buf.truncate(target);
    buf
}

/// Little-endian u32 table with small deltas, plus an occasional run of random bytes.
fn binary_bytes(rng: &mut Rng, target: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(target + 8);
    let mut v: u32 = 0;
    while buf.len() < target {
        if rng.next_u64() % 256 == 0 {
            for _ in 0..32 {
                buf.extend_from_slice(&rng.next_u64().to_le_bytes());
            }
        } else {
            v = v.wrapping_add((rng.next_u64() % 16) as u32);
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    buf.truncate(target);
    buf
}

fn random_bytes(rng: &mut Rng, target: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(target + 8);
    while buf.len() < target {
        buf.extend_from_slice(&rng.next_u64().to_le_bytes());
    }
    buf.truncate(target);
    buf
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut w = BufWriter::new(File::create(path).with_context(|| path.display().to_string())?);
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

fn dir_bytes(path: &Path) -> Result<u64> {
    let mut total = 0;
    for e in fs::read_dir(path).with_context(|| path.display().to_string())? {
        let e = e?;
        let m = e.metadata()?;
        total += if m.is_dir() {
            dir_bytes(&e.path())?
        } else {
            m.len()
        };
    }
    Ok(total)
}

fn reset_dir(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path).with_context(|| path.display().to_string())?;
    }
    fs::create_dir_all(path)?;
    Ok(())
}

/// The benchmark's own folder under `root`: created (with its ownership marker) when
/// missing; an existing one is used only when it carries that marker, since everything in
/// it may be deleted.
fn bench_dir(root: &Path) -> Result<PathBuf> {
    let dir = root.join(BENCH_DIR);
    let owner = dir.join(OWNER);
    if dir.exists() {
        if fs::read_to_string(&owner).ok().as_deref() != Some(OWNER_TEXT) {
            bail!(
                "{} exists but was not created by archive-bench (no {OWNER}); refusing to use it",
                dir.display()
            );
        }
    } else {
        fs::create_dir_all(&dir).with_context(|| dir.display().to_string())?;
        fs::write(&owner, OWNER_TEXT)?;
    }
    Ok(dir)
}

/// `root` is a folder from [`bench_dir`]: only folders inside it are deleted.
fn ensure_corpus(root: &Path, scale: f64) -> Result<()> {
    debug_assert!(root.join(OWNER).is_file());
    let want = format!("archive-bench corpus v1 scale={scale}");
    let marker = root.join(MARKER);
    let fresh = fs::read_to_string(&marker).ok().as_deref() == Some(want.as_str())
        && CORPORA.iter().all(|c| root.join(c).is_dir());
    if fresh {
        eprintln!("reusing corpus in {}", root.display());
        return Ok(());
    }

    eprintln!("generating corpus in {} (scale {scale})", root.display());
    for c in CORPORA {
        let _ = fs::remove_dir_all(root.join(c));
    }
    let _ = fs::remove_file(&marker);
    let mut rng = Rng(SEED);

    // Each of the big corpora: 8 files of 64 MiB * scale / 8.
    let per_file = ((64.0 * MIB * scale) / 8.0).max(1.0) as usize;

    let text = root.join("text");
    fs::create_dir_all(&text)?;
    for i in 0..8 {
        write_file(
            &text.join(format!("doc_{i:02}.txt")),
            &text_bytes(&mut rng, per_file),
        )?;
    }

    let binary = root.join("binary");
    fs::create_dir_all(&binary)?;
    for i in 0..8 {
        write_file(
            &binary.join(format!("table_{i:02}.bin")),
            &binary_bytes(&mut rng, per_file),
        )?;
    }

    let pre = root.join("precompressed");
    fs::create_dir_all(&pre)?;
    for i in 0..8 {
        let name = if i % 2 == 0 {
            format!("photo_{i:02}.jpg")
        } else {
            format!("clip_{i:02}.mp4")
        };
        write_file(&pre.join(name), &random_bytes(&mut rng, per_file))?;
    }

    let small = root.join("small");
    fs::create_dir_all(&small)?;
    let n = ((10_000.0 * scale).round() as usize).max(1);
    let dirs = n.min(100);
    for d in 0..dirs {
        fs::create_dir_all(small.join(format!("d{d:03}")))?;
    }
    for i in 0..n {
        let size = 200 + (rng.next_u64() % 3801) as usize;
        let bytes = text_bytes(&mut rng, size);
        write_file(
            &small
                .join(format!("d{:03}", i % dirs))
                .join(format!("f{i:05}.txt")),
            &bytes,
        )?;
    }

    fs::write(&marker, &want)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Measurements
// ---------------------------------------------------------------------------

struct Config {
    name: &'static str,
    ext: &'static str,
    preset: Preset,
}

fn configs() -> Vec<Config> {
    vec![
        Config {
            name: "zip Fast",
            ext: "zip",
            preset: Preset::Fast,
        },
        Config {
            name: "zip Balanced",
            ext: "zip",
            preset: Preset::Balanced,
        },
        Config {
            name: "zip Max",
            ext: "zip",
            preset: Preset::Max,
        },
        Config {
            name: "zip Zstd",
            ext: "zip",
            preset: Preset::Zstd,
        },
        Config {
            name: "7z",
            ext: "7z",
            preset: Preset::Balanced,
        },
        Config {
            name: "tar.zst",
            ext: "tar.zst",
            preset: Preset::Balanced,
        },
    ]
}

/// One timed sample: (archive bytes, create seconds, extract seconds).
type Sample = (u64, f64, f64);

fn forge_sample(src: &Path, work: &Path, cfg: &Config, idx: usize) -> Result<Sample> {
    let out = work.join(format!("bench-{idx}.{}", cfg.ext));
    let _ = fs::remove_file(&out);
    let dest = work.join("extract");

    let cancel = CancelToken::new();
    let progress = Progress::default();
    let ctx = JobCtx::new(&cancel, &progress);

    let opts = CreateOptions {
        preset: cfg.preset,
        ..Default::default()
    };
    let t = Instant::now();
    let created = archive::create(&[src.to_path_buf()], &out, &opts, &ctx)
        .with_context(|| format!("create {}", out.display()))?;
    let create_s = t.elapsed().as_secs_f64();
    if !created.skipped.is_empty() {
        eprintln!(
            "  warning: {} entries skipped while creating",
            created.skipped.len()
        );
    }
    let size = fs::metadata(&out)?.len();

    reset_dir(&dest)?;
    let mut eopts = ExtractOptions::default();
    eopts.mark_of_the_web = false;
    let t = Instant::now();
    let extracted = archive::extract(&out, &dest, &eopts, None, &ctx)
        .with_context(|| format!("extract {}", out.display()))?;
    let extract_s = t.elapsed().as_secs_f64();
    if !extracted.failed.is_empty() {
        bail!(
            "{} entries failed to extract from {}",
            extracted.failed.len(),
            out.display()
        );
    }

    let _ = fs::remove_file(&out);
    let _ = fs::remove_dir_all(&dest);
    Ok((size, create_s, extract_s))
}

fn sevenzip_sample(
    exe: &Path,
    root: &Path,
    corpus: &str,
    work: &Path,
    zip: bool,
    idx: usize,
) -> Result<Sample> {
    let ext = if zip { "zip" } else { "7z" };
    let out = work.join(format!("bench7z-{idx}.{ext}"));
    let _ = fs::remove_file(&out);
    let dest = work.join("extract7z");

    // Run from the corpus root with a relative name, so entries are stored as `corpus\...`
    // like the forge archives (which store each source folder by its own name).
    let t = Instant::now();
    let status = Command::new(exe)
        .current_dir(root)
        .arg("a")
        .args(if zip { &["-tzip"][..] } else { &[][..] })
        .arg("-mx=5")
        .arg(&out)
        .arg(corpus)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("starting 7-Zip")?;
    let create_s = t.elapsed().as_secs_f64();
    if !status.success() {
        bail!("7-Zip create failed ({status}) for {corpus}");
    }
    let size = fs::metadata(&out)?.len();

    reset_dir(&dest)?;
    let t = Instant::now();
    let status = Command::new(exe)
        .arg("x")
        .arg(format!("-o{}", dest.display()))
        .arg("-y")
        .arg(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("starting 7-Zip")?;
    let extract_s = t.elapsed().as_secs_f64();
    if !status.success() {
        bail!("7-Zip extract failed ({status}) for {corpus}");
    }

    let _ = fs::remove_file(&out);
    let _ = fs::remove_dir_all(&dest);
    Ok((size, create_s, extract_s))
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = s.len();
    match n {
        0 => 0.0,
        _ if n % 2 == 1 => s[n / 2],
        _ => (s[n / 2 - 1] + s[n / 2]) / 2.0,
    }
}

struct Row {
    corpus: &'static str,
    config: &'static str,
    input: u64,
    archive: u64,
    compress_s: Vec<f64>,
    extract_s: Vec<f64>,
}

fn render(rows: &[Row]) -> String {
    let mut out = String::from(
        "| corpus | config | input MiB | archive MiB | ratio % | compress MiB/s | extract MiB/s |\n\
         |---|---|---:|---:|---:|---:|---:|\n",
    );
    for r in rows {
        let input_mib = r.input as f64 / MIB;
        let archive_mib = r.archive as f64 / MIB;
        let ratio = if r.input == 0 {
            0.0
        } else {
            r.archive as f64 / r.input as f64 * 100.0
        };
        let c_rate = input_mib / median(&r.compress_s).max(1e-6);
        let x_rate = input_mib / median(&r.extract_s).max(1e-6);
        out.push_str(&format!(
            "| {} | {} | {:.2} | {:.2} | {:.1} | {:.1} | {:.1} |\n",
            r.corpus, r.config, input_mib, archive_mib, ratio, c_rate, x_rate
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args = parse_args()?;
    let base = bench_dir(&args.root)?;
    ensure_corpus(&base, args.scale)?;

    let work = base.join("work");
    reset_dir(&work)?;

    let mut rows = Vec::new();
    for corpus in CORPORA {
        let src = base.join(corpus);
        let input = dir_bytes(&src)?;

        for cfg in configs() {
            let mut row = Row {
                corpus,
                config: cfg.name,
                input,
                archive: 0,
                compress_s: Vec::new(),
                extract_s: Vec::new(),
            };
            for s in 0..args.samples {
                let (size, c, x) = forge_sample(&src, &work, &cfg, s)?;
                eprintln!(
                    "[{corpus}] {} sample {}/{}: archive {:.2} MiB, create {c:.2} s, extract {x:.2} s",
                    cfg.name,
                    s + 1,
                    args.samples,
                    size as f64 / MIB
                );
                row.archive = size;
                row.compress_s.push(c);
                row.extract_s.push(x);
            }
            rows.push(row);
        }

        if let Some(exe) = &args.sevenzip {
            for (name, zip) in [("7-Zip 7z mx5", false), ("7-Zip zip mx5", true)] {
                let mut row = Row {
                    corpus,
                    config: name,
                    input,
                    archive: 0,
                    compress_s: Vec::new(),
                    extract_s: Vec::new(),
                };
                for s in 0..args.samples {
                    let (size, c, x) = sevenzip_sample(exe, &base, corpus, &work, zip, s)?;
                    eprintln!(
                        "[{corpus}] {name} sample {}/{}: archive {:.2} MiB, create {c:.2} s, extract {x:.2} s",
                        s + 1,
                        args.samples,
                        size as f64 / MIB
                    );
                    row.archive = size;
                    row.compress_s.push(c);
                    row.extract_s.push(x);
                }
                rows.push(row);
            }
        }
    }

    let _ = fs::remove_dir_all(&work);
    let table = render(&rows);
    print!("{table}");
    if let Some(path) = &args.out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &table).with_context(|| path.display().to_string())?;
        eprintln!("wrote {}", path.display());
    }
    Ok(())
}
