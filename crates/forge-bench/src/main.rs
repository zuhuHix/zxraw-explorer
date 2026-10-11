use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use clap::{Args, Parser, Subcommand};
use forge_core::duplicates::{DupOptions, find_duplicates};
use forge_core::hashing::hash_file;
use forge_core::listing::{SortSpec, list_dir_streaming, sort_entries};
use forge_core::model::{EntryKind, ext_of};
use forge_core::ops::JobCtx;
use forge_core::query::Query;
use forge_db::{Db, Indexer, NewFile, SearchOptions};
use forge_jobs::{CancelToken, Progress};
use forge_media::{ThumbConfig, ThumbRequest, ThumbnailService};

const FLAT_DIR: &str = "flat50k";
const TREE_DIR: &str = "tree200k";
const JPEG_DIR: &str = "jpeg24mp";
const HASH_FILE: &str = "hash-2gb.bin";
const INDEX_DB: &str = "bench-index.db";
const FLAT_BATCH: usize = 2048;
const JPEG_LONG_EDGE: u32 = 256;
const JPEG_EXTS: [&str; 8] = ["jpg", "cr3", "mp4", "txt", "nef", "arw", "xmp", "png"];

#[derive(Parser)]
#[command(
    name = "forge-bench",
    about = "ZXraw Explorer data generator and benchmark harness"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create reproducible test data below %TEMP%\\zxraw-bench (or --root).
    Generate(GenerateArgs),
    /// Run the project-engine benchmarks on a generated data set.
    Run(RunArgs),
}

#[derive(Args)]
struct GenerateArgs {
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long, default_value_t = 50_000)]
    flat_files: usize,
    #[arg(long, default_value_t = 200_000)]
    tree_files: usize,
    #[arg(long, default_value_t = 2_000)]
    tree_leaf_dirs: usize,
    #[arg(long, default_value_t = 200)]
    jpeg_count: usize,
    #[arg(long, default_value_t = 6_000)]
    jpeg_width: u32,
    #[arg(long, default_value_t = 4_000)]
    jpeg_height: u32,
    #[arg(long, default_value_t = 2_000_000_000)]
    hash_bytes: u64,
}

#[derive(Args)]
struct RunArgs {
    #[arg(long)]
    root: Option<PathBuf>,
    /// Repetitions for enumeration, query, hashing, and search samples.
    #[arg(long, default_value_t = 3)]
    samples: usize,
    /// Repetitions for thumbnail cold and disk-cache passes.
    #[arg(long, default_value_t = 3)]
    thumb_samples: usize,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Generate(args) => generate(args),
        Command::Run(args) => run(args),
    }
}

fn default_root() -> PathBuf {
    std::env::temp_dir().join("zxraw-bench")
}

fn checked_temp_root(root: PathBuf, create: bool) -> anyhow::Result<PathBuf> {
    let temp_input = std::env::temp_dir();
    let temp_input = if temp_input.is_absolute() {
        temp_input
    } else {
        std::env::current_dir()?.join(temp_input)
    };
    let temp = fs::canonicalize(&temp_input).context("resolve TEMP directory")?;
    let candidate = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()?.join(root)
    };
    ensure!(
        candidate != temp_input && candidate.starts_with(&temp_input),
        "benchmark data root must be a child of TEMP (TEMP is {})",
        temp_input.display()
    );
    // Resolve every existing component before creating anything. This also rejects a
    // pre-existing junction/symlink below TEMP that redirects the data elsewhere.
    let relative = candidate.strip_prefix(&temp_input)?;
    let mut probe = temp.clone();
    for component in relative.components() {
        probe.push(component.as_os_str());
        if !probe.exists() {
            break;
        }
        let resolved =
            fs::canonicalize(&probe).with_context(|| format!("resolve {}", probe.display()))?;
        ensure!(
            resolved.starts_with(&temp),
            "benchmark path component leaves TEMP: {}",
            probe.display()
        );
        probe = resolved;
    }
    if create {
        fs::create_dir_all(&candidate)
            .with_context(|| format!("create {}", candidate.display()))?;
    }
    let root =
        fs::canonicalize(&candidate).with_context(|| format!("resolve {}", candidate.display()))?;
    ensure!(
        root != temp && root.starts_with(&temp),
        "benchmark data root must resolve below TEMP"
    );
    Ok(root)
}

fn generate(args: GenerateArgs) -> anyhow::Result<()> {
    ensure!(
        args.flat_files > 0,
        "--flat-files must be greater than zero"
    );
    ensure!(
        args.tree_files > 0,
        "--tree-files must be greater than zero"
    );
    ensure!(
        args.tree_leaf_dirs > 0,
        "--tree-leaf-dirs must be greater than zero"
    );
    ensure!(
        args.jpeg_count > 0,
        "--jpeg-count must be greater than zero"
    );
    ensure!(
        args.jpeg_width > 0 && args.jpeg_height > 0,
        "JPEG dimensions must be nonzero"
    );
    ensure!(
        args.hash_bytes > 0,
        "--hash-bytes must be greater than zero"
    );

    let root = checked_temp_root(args.root.unwrap_or_else(default_root), true)?;
    let flat = root.join(FLAT_DIR);
    let tree = root.join(TREE_DIR);
    let jpeg = root.join(JPEG_DIR);
    for path in [&flat, &tree, &jpeg] {
        fs::create_dir(path)
            .with_context(|| format!("create {} (remove old data first)", path.display()))?;
    }
    let hash_path = root.join(HASH_FILE);
    ensure!(
        !hash_path.exists(),
        "{} already exists; remove old data first",
        hash_path.display()
    );

    println!("root: {}", root.display());
    println!("generating flat files: {}", args.flat_files);
    generate_flat(&flat, args.flat_files)?;
    println!(
        "generating nested tree: {} files across {} leaf folders",
        args.tree_files, args.tree_leaf_dirs
    );
    generate_tree(&tree, args.tree_files, args.tree_leaf_dirs)?;
    println!(
        "generating JPEGs: {} x {}x{}",
        args.jpeg_count, args.jpeg_width, args.jpeg_height
    );
    generate_jpegs(&jpeg, args.jpeg_count, args.jpeg_width, args.jpeg_height)?;
    println!(
        "writing {} non-sparse bytes: {}",
        args.hash_bytes,
        hash_path.display()
    );
    generate_hash_file(&hash_path, args.hash_bytes)?;
    println!(
        "generation complete; benchmark database and thumbnail cache will be created by `run`"
    );
    Ok(())
}

fn generate_flat(root: &Path, n: usize) -> anyhow::Result<()> {
    for i in 0..n {
        let ext = JPEG_EXTS[i % JPEG_EXTS.len()];
        let name = format!("Päivä_{i:06}_東京.{ext}");
        let len = 32 + (i * 73 % 481);
        let mut bytes = vec![0; len];
        fill_pattern(&mut bytes, i as u64 + 1);
        fs::write(root.join(name), bytes)?;
    }
    Ok(())
}

fn generate_tree(root: &Path, files: usize, leaf_dirs: usize) -> anyhow::Result<()> {
    let top_count = leaf_dirs.min(40).max(1);
    let mut dirs = Vec::with_capacity(leaf_dirs);
    for dir_idx in 0..leaf_dirs {
        let path = root
            .join(format!("Group_{:02}_μ", dir_idx % top_count))
            .join(format!("Set_{dir_idx:04}_東京"));
        fs::create_dir_all(&path)?;
        dirs.push(path);
    }
    for i in 0..files {
        let dir_idx = i % leaf_dirs;
        let ordinal = i / leaf_dirs;
        let ext = JPEG_EXTS[(i * 5 + dir_idx) % JPEG_EXTS.len()];
        let name = format!("IMG_{i:06}_фото.{ext}");
        let pair = i / 2;
        let len = 64 + (pair % 29) * 16;
        let within = i % 10_000;
        let seed = if within < 2 {
            (i / 10_000) as u64 * 10_000 + 7
        } else {
            i as u64 + 1_000_000
        };
        let mut bytes = vec![0; len];
        fill_pattern(&mut bytes, seed);
        fs::write(dirs[dir_idx].join(name), bytes)
            .with_context(|| format!("write tree file {i} (leaf ordinal {ordinal})"))?;
    }
    Ok(())
}

fn fill_pattern(bytes: &mut [u8], seed: u64) {
    let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    for byte in bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = (state >> 24) as u8;
    }
}

fn generate_jpegs(root: &Path, n: usize, width: u32, height: u32) -> anyhow::Result<()> {
    for i in 0..n {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            let seed = i as u32;
            image::Rgb([
                (x.wrapping_mul(17).wrapping_add(seed * 29) % 256) as u8,
                (y.wrapping_mul(31).wrapping_add(seed * 47) % 256) as u8,
                (x.wrapping_add(y.wrapping_mul(3)).wrapping_add(seed * 61) % 256) as u8,
            ])
        });
        let path = root.join(format!("IMG_{i:04}.jpg"));
        let mut writer = BufWriter::new(File::create(&path)?);
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, 85)
            .encode(
                image.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgb8,
            )
            .with_context(|| format!("encode {}", path.display()))?;
        writer.flush()?;
    }
    Ok(())
}

fn generate_hash_file(path: &Path, total: u64) -> anyhow::Result<()> {
    let mut writer = BufWriter::with_capacity(1024 * 1024, File::create_new(path)?);
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut written = 0u64;
    let mut block = 0u64;
    while written < total {
        let count = (total - written).min(buffer.len() as u64) as usize;
        let mut h = blake3::Hasher::new_derive_key("ZXraw Explorer forge-bench file generator v1");
        h.update(&block.to_le_bytes());
        h.finalize_xof().fill(&mut buffer[..count]);
        writer.write_all(&buffer[..count])?;
        written += count as u64;
        block += 1;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn run(args: RunArgs) -> anyhow::Result<()> {
    ensure!(args.samples > 0, "--samples must be greater than zero");
    ensure!(
        args.thumb_samples > 0,
        "--thumb-samples must be greater than zero"
    );
    let root = checked_temp_root(args.root.unwrap_or_else(default_root), false)?;
    let flat = root.join(FLAT_DIR);
    let tree = root.join(TREE_DIR);
    let jpeg_dir = root.join(JPEG_DIR);
    let hash_path = root.join(HASH_FILE);
    for path in [&flat, &tree, &jpeg_dir, &hash_path] {
        ensure!(
            path.exists(),
            "missing {}; run `forge-bench generate` first",
            path.display()
        );
    }
    let jpeg_files = jpg_files(&jpeg_dir)?;
    ensure!(
        !jpeg_files.is_empty(),
        "no JPEGs found in {}",
        jpeg_dir.display()
    );

    println!("ZXraw Explorer engine benchmark");
    println!("root: {}", root.display());
    println!(
        "samples: {}; thumbnail samples: {}",
        args.samples, args.thumb_samples
    );
    println!("hash input: {} bytes", fs::metadata(&hash_path)?.len());

    benchmark_listing_and_query(&flat, args.samples)?;
    let db = benchmark_index_and_search(&root, &tree, args.samples)?;
    benchmark_duplicates(&tree)?;
    benchmark_hashing(&hash_path, args.samples)?;
    benchmark_thumbnails(&root, &jpeg_files, args.thumb_samples)?;
    println!(
        "database retained at {}",
        db.path().unwrap_or(Path::new("")).display()
    );
    println!(
        "complete; filesystem cache state is controlled by Windows and is not flushed by this harness"
    );
    Ok(())
}

fn benchmark_listing_and_query(flat: &Path, samples: usize) -> anyhow::Result<()> {
    println!("\n[flat directory: streaming, natural sort, query]");
    let mut first_ms = Vec::with_capacity(samples);
    let mut enumeration_ms = Vec::with_capacity(samples);
    let mut sorting_ms = Vec::with_capacity(samples);
    let mut complete_ms = Vec::with_capacity(samples);
    let mut entries = Vec::new();
    for sample in 1..=samples {
        let start = Instant::now();
        let mut first = None;
        let mut all = Vec::new();
        let errors = list_dir_streaming(flat, FLAT_BATCH, &CancelToken::new(), |batch| {
            if first.is_none() {
                first = Some(start.elapsed());
            }
            all.extend(batch);
        })?;
        let enumeration = start.elapsed();
        let first = first.context("listing returned no batches")?;
        ensure!(errors == 0, "directory listing reported {errors} errors");
        ensure!(!all.is_empty(), "flat directory contains no entries");
        let sort_start = Instant::now();
        sort_entries(&mut all, SortSpec::default());
        let sorting = sort_start.elapsed();
        let complete = start.elapsed();
        println!(
            "sample {sample}: entries={} first_batch={:.3} ms enumeration={:.3} ms natural_sort={:.3} ms enumerate_plus_sort={:.3} ms",
            all.len(),
            ms(first),
            ms(enumeration),
            ms(sorting),
            ms(complete)
        );
        first_ms.push(ms(first));
        enumeration_ms.push(ms(enumeration));
        sorting_ms.push(ms(sorting));
        complete_ms.push(ms(complete));
        entries = all;
    }
    println!(
        "median ({samples}): first_batch={:.3} ms enumeration_complete={:.3} ms sort_per_resort={:.3} ms enumerate_plus_sort={:.3} ms",
        median(&first_ms),
        median(&enumeration_ms),
        median(&sorting_ms),
        median(&complete_ms)
    );

    let query = Query::parse("ext:jpg OR ext:cr3")?;
    let mut filter_ms = Vec::with_capacity(samples);
    let mut hits = 0usize;
    for sample in 1..=samples {
        let start = Instant::now();
        hits = entries
            .iter()
            .filter(|entry| query.matches_entry(entry))
            .count();
        let elapsed = start.elapsed();
        println!(
            "query sample {sample}: ext:jpg OR ext:cr3 matched={hits} elapsed={:.3} ms",
            ms(elapsed)
        );
        filter_ms.push(ms(elapsed));
    }
    println!(
        "median ({samples}): Query::matches_entry over {} entries={:.3} ms; matched={hits}",
        entries.len(),
        median(&filter_ms)
    );
    Ok(())
}

fn benchmark_index_and_search(root: &Path, tree: &Path, samples: usize) -> anyhow::Result<Db> {
    println!("\n[forge_db index and search: nested tree]");
    let db_path = root.join(INDEX_DB);
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", db_path.display()));
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("remove prior benchmark db {}", path.display()))?;
        }
    }
    let cold_start = Instant::now();
    let db = Db::open(&db_path)?;
    let root_id = db.add_index_root(tree)?;
    let cold_report = Indexer::new(&db).index_root(root_id)?;
    let cold = cold_start.elapsed();
    ensure!(
        cold_report.errors == 0,
        "cold index reported {} errors",
        cold_report.errors
    );
    println!(
        "cold index: {:.3} s; items={} dirs={} outcome={:?}",
        cold.as_secs_f64(),
        cold_report.items,
        cold_report.dirs,
        cold_report.outcome
    );

    let warm_start = Instant::now();
    let unchanged_report = Indexer::new(&db).index_root(root_id)?;
    let unchanged = warm_start.elapsed();
    ensure!(
        unchanged_report.errors == 0,
        "unchanged re-index reported {} errors",
        unchanged_report.errors
    );
    println!(
        "re-index unchanged: {:.3} s; items={} dirs={} marked_missing={}",
        unchanged.as_secs_f64(),
        unchanged_report.items,
        unchanged_report.dirs,
        unchanged_report.marked_missing
    );
    println!("indexed rows: {:?}", db.stats()?);

    let options = SearchOptions {
        limit: Some(100),
        ..Default::default()
    };
    for query in ["name:IMG_123456", "ext:jpg", "size:>256b ext:cr3"] {
        let mut elapsed_ms = Vec::with_capacity(samples);
        let mut hits = 0usize;
        for sample in 1..=samples {
            let start = Instant::now();
            let found = db.search_str(query, &options)?;
            let elapsed = start.elapsed();
            hits = found.len();
            println!(
                "search `{query}` sample {sample}: hits={hits} elapsed={:.3} ms",
                ms(elapsed)
            );
            elapsed_ms.push(ms(elapsed));
        }
        println!(
            "search `{query}` median ({samples}): {:.3} ms; returned={hits}",
            median(&elapsed_ms)
        );
    }
    benchmark_index_walk_reference(tree, samples)?;
    Ok(db)
}

/// Time the indexer's filesystem traversal and row preparation without SQLite.
/// This is a reference profile, not an exact subtraction from the end-to-end index time.
fn benchmark_index_walk_reference(root: &Path, samples: usize) -> anyhow::Result<()> {
    println!("\n[index filesystem walk reference: no SQLite]");
    let mut elapsed_ms = Vec::with_capacity(samples);
    let mut item_count = 0_u64;
    let mut dir_count = 0_u64;
    for sample in 1..=samples {
        let start = Instant::now();
        let root_md = fs::symlink_metadata(root)?;
        let mut stack = vec![root.to_path_buf()];
        let mut batch = Vec::with_capacity(2_000);
        let mut items = 1_u64;
        let mut dirs = 1_u64;
        batch.push(index_row(root, EntryKind::Dir, &root_md));

        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                let name_lc = entry.file_name().to_string_lossy().to_lowercase();
                let metadata = entry.metadata()?;
                let kind = if metadata.file_type().is_symlink() {
                    EntryKind::Link
                } else if metadata.is_dir() {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                };
                if kind == EntryKind::Dir {
                    stack.push(path.clone());
                    dirs += 1;
                } else if kind == EntryKind::File || kind == EntryKind::Link {
                    std::hint::black_box(ext_of(&path));
                }
                std::hint::black_box(name_lc);
                batch.push(index_row(&path, kind, &metadata));
                items += 1;
                if batch.len() >= 2_000 {
                    std::hint::black_box(&batch);
                    batch.clear();
                }
            }
        }
        std::hint::black_box(&batch);
        let elapsed = start.elapsed();
        item_count = items;
        dir_count = dirs;
        println!(
            "walk sample {sample}: items={items} dirs={dirs} elapsed={:.3} ms",
            ms(elapsed)
        );
        elapsed_ms.push(ms(elapsed));
    }
    println!(
        "walk median ({samples}): items={item_count} dirs={dir_count} elapsed={:.3} ms; excludes SQLite, checkpoint, and index sweep",
        median(&elapsed_ms)
    );
    Ok(())
}

fn index_row(path: &Path, kind: EntryKind, metadata: &fs::Metadata) -> NewFile {
    NewFile {
        path: path.to_path_buf(),
        kind,
        size: if kind == EntryKind::File {
            metadata.len()
        } else {
            0
        },
        mtime: metadata.modified().ok().map(system_time_ms),
        ctime: metadata.created().ok().map(system_time_ms),
    }
}

fn system_time_ms(time: std::time::SystemTime) -> i64 {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(error) => -(error.duration().as_millis() as i64),
    }
}

fn benchmark_duplicates(tree: &Path) -> anyhow::Result<()> {
    println!("\n[forge_core duplicate scan: nested tree]");
    let cancel = CancelToken::new();
    let progress = Progress::new();
    let ctx = JobCtx::new(&cancel, progress.as_ref());
    let start = Instant::now();
    let report = find_duplicates(&[tree.to_path_buf()], &DupOptions::default(), &ctx)?;
    let elapsed = start.elapsed();
    println!(
        "duplicate scan: {:.3} s; files_scanned={} candidates={:?} groups={} errors={}",
        elapsed.as_secs_f64(),
        report.files_scanned,
        report.stage_candidates,
        report.groups.len(),
        report.errors.len()
    );
    Ok(())
}

fn benchmark_hashing(path: &Path, samples: usize) -> anyhow::Result<()> {
    println!("\n[forge_core BLAKE3 full-file hashing]");
    let bytes = fs::metadata(path)?.len();
    let mut elapsed_ms = Vec::with_capacity(samples);
    for sample in 1..=samples {
        let start = Instant::now();
        let digest = hash_file(path, &CancelToken::new(), None)?;
        std::hint::black_box(digest);
        let elapsed = start.elapsed();
        let mb_s = bytes as f64 / 1_000_000.0 / elapsed.as_secs_f64();
        println!(
            "sample {sample}: bytes={bytes} elapsed={:.3} s throughput={mb_s:.1} MB/s",
            elapsed.as_secs_f64()
        );
        elapsed_ms.push(ms(elapsed));
    }
    let median_ms = median(&elapsed_ms);
    println!(
        "median ({samples}): {median_ms:.3} ms; throughput={:.1} MB/s (decimal MB/s)",
        bytes as f64 / 1_000_000.0 / (median_ms / 1000.0)
    );
    Ok(())
}

fn benchmark_thumbnails(root: &Path, files: &[PathBuf], samples: usize) -> anyhow::Result<()> {
    println!(
        "\n[forge_media ThumbnailService: {JPEG_LONG_EDGE}px thumbnails from {} JPEGs]",
        files.len()
    );
    let cache = root.join("thumb-cache");
    let mut cold_ms = Vec::with_capacity(samples);
    let mut cold_rates = Vec::with_capacity(samples);
    for sample in 1..=samples {
        if cache.exists() {
            fs::remove_dir_all(&cache).with_context(|| format!("clear {}", cache.display()))?;
        }
        let (elapsed, stats) = thumbnail_pass(files, &cache)?;
        ensure!(
            stats.generated == files.len() as u64,
            "cold pass generated {} of {} thumbnails",
            stats.generated,
            files.len()
        );
        ensure!(
            stats.errors == 0,
            "cold thumbnail pass returned {} errors",
            stats.errors
        );
        let rate = files.len() as f64 / elapsed.as_secs_f64();
        println!(
            "cold sample {sample}: elapsed={:.3} s throughput={rate:.2} images/s generated={} disk_hits={}",
            elapsed.as_secs_f64(),
            stats.generated,
            stats.disk_hits
        );
        cold_ms.push(ms(elapsed));
        cold_rates.push(rate);
    }
    println!(
        "cold median ({samples}): {:.3} ms wall/image; {:.2} images/s",
        median(&cold_ms) / files.len() as f64,
        files.len() as f64 / (median(&cold_ms) / 1000.0)
    );

    let mut disk_ms = Vec::with_capacity(samples);
    for sample in 1..=samples {
        let (elapsed, stats) = thumbnail_pass(files, &cache)?;
        ensure!(
            stats.disk_hits == files.len() as u64,
            "disk-cache pass had {} hits of {}",
            stats.disk_hits,
            files.len()
        );
        ensure!(
            stats.errors == 0,
            "disk-cache pass returned {} errors",
            stats.errors
        );
        println!(
            "disk-cache sample {sample}: elapsed={:.3} s wall/image={:.3} ms hits={}",
            elapsed.as_secs_f64(),
            ms(elapsed) / files.len() as f64,
            stats.disk_hits
        );
        disk_ms.push(ms(elapsed));
    }
    println!(
        "disk-cache median ({samples}): {:.3} ms wall/image; {:.2} images/s",
        median(&disk_ms) / files.len() as f64,
        files.len() as f64 / (median(&disk_ms) / 1000.0)
    );
    let _ = cold_rates;
    Ok(())
}

fn thumbnail_pass(
    files: &[PathBuf],
    cache: &Path,
) -> anyhow::Result<(Duration, forge_media::ThumbStats)> {
    let cfg = ThumbConfig {
        cache_dir: Some(cache.to_path_buf()),
        ..Default::default()
    };
    let service = ThumbnailService::new(cfg);
    let start = Instant::now();
    for path in files {
        let status = service.request(ThumbRequest::new(path, JPEG_LONG_EDGE));
        ensure!(
            matches!(
                status,
                forge_media::RequestStatus::Queued | forge_media::RequestStatus::AlreadyInFlight
            ),
            "thumbnail request for {} was not queued: {status:?}",
            path.display()
        );
    }
    for _ in files {
        let result = service
            .results()
            .recv_timeout(Duration::from_secs(300))
            .context("waiting for thumbnail result")?;
        result
            .image
            .with_context(|| format!("decode thumbnail {}", result.path.display()))?;
    }
    let elapsed = start.elapsed();
    let stats = service.stats();
    drop(service);
    Ok((elapsed, stats))
}

fn jpg_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg"))
        })
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn median(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}
