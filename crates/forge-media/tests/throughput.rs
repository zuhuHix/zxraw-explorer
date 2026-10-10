//! Thumbnail throughput on generated 24 MP JPEGs.
//!
//! Run with:
//! `cargo test --release -p forge-media --test throughput -- --ignored --nocapture`
mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::*;
use forge_media::thumbs::DecodeLimits;
use forge_media::*;

const N: usize = 12;
const W: u32 = 6000;
const H: u32 = 4000;

#[test]
#[ignore = "benchmark; run in release with --ignored --nocapture"]
fn thumbnail_throughput_24mp() {
    let dir = tempfile::tempdir().unwrap();
    let t0 = Instant::now();
    let thumb = jpeg(160, 107, 0);
    let files: Vec<PathBuf> = (0..N)
        .map(|i| {
            let j = encode_jpeg(&pattern(W, H, i as u32), 90);
            let j = with_exif(&j, &exif_tiff(1, Some(&thumb)));
            let p = dir.path().join(format!("IMG_{i:04}.jpg"));
            std::fs::write(&p, j).unwrap();
            p
        })
        .collect();
    let total_mb: f64 = files
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().len() as f64)
        .sum::<f64>()
        / 1e6;
    println!(
        "generated {N} x {W}x{H} JPEGs ({total_mb:.1} MB) in {:.1}s",
        t0.elapsed().as_secs_f64()
    );
    let limits = DecodeLimits::default();

    // 1. Single-threaded full decode + downscale.
    let t = Instant::now();
    for f in &files {
        let th = generate_thumbnail(f, 256, &limits, None, 1, None).unwrap();
        assert_eq!(th.width, 256);
    }
    let single = t.elapsed().as_secs_f64() / N as f64;
    println!(
        "single-thread 256px (full decode): {:.1} ms/image",
        single * 1e3
    );

    // 2. EXIF-thumbnail fast path (request ≤ embedded thumb).
    let t = Instant::now();
    for f in &files {
        generate_thumbnail(f, 128, &limits, None, 1, None).unwrap();
    }
    println!(
        "single-thread 128px (EXIF thumb path): {:.2} ms/image",
        t.elapsed().as_secs_f64() * 1e3 / N as f64
    );

    // 3. Metadata read.
    let t = Instant::now();
    for f in &files {
        let m = read_image_metadata(f).unwrap();
        assert_eq!(m.width, Some(W));
    }
    println!(
        "read_image_metadata: {:.2} ms/image",
        t.elapsed().as_secs_f64() * 1e3 / N as f64
    );

    // 4. Service, default pool size, cold disk cache.
    let cache = dir.path().join("cache");
    let cfg = ThumbConfig {
        cache_dir: Some(cache.clone()),
        ff_tools: Some(FfTools::default()),
        ..Default::default()
    };
    let threads = cfg.image_threads;
    let s = ThumbnailService::new(cfg.clone());
    let t = Instant::now();
    for f in &files {
        s.request(ThumbRequest::new(f, 256));
    }
    for _ in 0..N {
        s.results()
            .recv_timeout(Duration::from_secs(120))
            .unwrap()
            .image
            .unwrap();
    }
    let el = t.elapsed().as_secs_f64();
    println!(
        "service cold ({threads} threads): {:.1} images/s ({:.1} ms wall/image, {:.0} MP/s)",
        N as f64 / el,
        el * 1e3 / N as f64,
        N as f64 * (W * H) as f64 / 1e6 / el
    );
    drop(s);

    // 5. Disk-cache hits (fresh service = empty memory cache).
    let s = ThumbnailService::new(cfg);
    let t = Instant::now();
    for f in &files {
        s.request(ThumbRequest::new(f, 256));
    }
    for _ in 0..N {
        s.results()
            .recv_timeout(Duration::from_secs(30))
            .unwrap()
            .image
            .unwrap();
    }
    println!(
        "service disk-cache hits: {:.2} ms wall/image",
        t.elapsed().as_secs_f64() * 1e3 / N as f64
    );
    assert_eq!(s.stats().disk_hits, N as u64);

    // 6. Memory hits (synchronous).
    let stats: Vec<_> = files
        .iter()
        .map(|f| {
            let md = std::fs::metadata(f).unwrap();
            (md.modified().ok(), md.len())
        })
        .collect();
    let t = Instant::now();
    let mut hits = 0;
    for _ in 0..100 {
        for (f, (m, l)) in files.iter().zip(&stats) {
            if s.get_cached(f, 256, *m, *l).is_some() {
                hits += 1;
            }
        }
    }
    println!(
        "memory-cache lookups: {:.2} µs/lookup ({hits} hits)",
        t.elapsed().as_secs_f64() * 1e6 / (100 * N) as f64
    );
}
