//! Scale check: 100k synthetic rows in a file-backed (WAL) database.
//! Prints timings (run with `--nocapture` to see them); bounds are generous so the test is
//! stable on slow CI machines and in unoptimized builds.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use forge_core::model::EntryKind;
use forge_core::query::Query;
use forge_db::{Db, NewFile, SearchOptions};

const N: usize = 100_000;
const BATCH: usize = 2_000;

fn timed<T>(label: &str, f: impl FnOnce() -> T) -> (T, Duration) {
    let t = Instant::now();
    let out = f();
    let d = t.elapsed();
    eprintln!("[perf] {label}: {:.1} ms", d.as_secs_f64() * 1000.0);
    (out, d)
}

#[test]
fn hundred_thousand_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("perf.db")).unwrap();
    let exts = ["cr3", "jpg", "nef", "mp4", "mov", "arw", "png", "xmp"];
    let rows: Vec<NewFile> = (0..N)
        .map(|i| NewFile {
            path: PathBuf::from(format!(
                r"D:\Shoots\{:04}\Card_{:02}\IMG_{i:06}.{}",
                2015 + i % 10,
                (i / 1000) % 50,
                exts[i % exts.len()]
            )),
            kind: EntryKind::File,
            size: (i as u64 * 7919) % (50 << 20),
            mtime: Some(1_600_000_000_000 + i as i64 * 60_000),
            ctime: None,
        })
        .collect();

    let (_, insert) = timed("insert 100k rows (50 txns x 2000)", || {
        for chunk in rows.chunks(BATCH) {
            db.upsert_files(chunk).unwrap();
        }
    });
    let (_, reupsert) = timed("re-upsert 100k unchanged rows", || {
        for chunk in rows.chunks(BATCH) {
            db.upsert_files(chunk).unwrap();
        }
    });
    assert_eq!(db.stats().unwrap().files, N as u64);

    let opts = SearchOptions {
        limit: Some(500),
        ..Default::default()
    };
    // Warm the page cache once, as a running app would have.
    db.count(&Query::all(), &opts).unwrap();

    let q = Query::parse("name:img_0421").unwrap();
    let (hits, name_q) = timed("name substring query (sorted, limit 500)", || {
        db.search(&q, &opts).unwrap()
    });
    assert_eq!(hits.len(), 100); // IMG_042100..IMG_042199
    assert!(hits.windows(2).all(|w| w[0].name <= w[1].name));

    let q = Query::parse("card_07 ext:cr3,nef size:>10mb").unwrap();
    let (hits, combo) = timed("free word + ext list + size", || {
        db.search(&q, &opts).unwrap()
    });
    assert!(!hits.is_empty());

    let q = Query::parse(r"\2019\").unwrap();
    let (n, cnt) = timed("count free-word matches", || db.count(&q, &opts).unwrap());
    assert_eq!(n, (N / 10) as u64);

    let ids: Vec<i64> = db
        .search(
            &Query::parse("ext:jpg").unwrap(),
            &SearchOptions {
                limit: Some(5000),
                ..Default::default()
            },
        )
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    let (_, rate) = timed("rate+tag 5000 files in one txn", || {
        db.annotate(
            &ids,
            &forge_db::AnnotationChange {
                rating: Some(4),
                add_tags: vec!["batch".into()],
                ..Default::default()
            },
        )
        .unwrap()
    });
    let q = Query::parse("rating:>=4 tag:batch").unwrap();
    let (n, ann_q) = timed("count rating+tag query", || db.count(&q, &opts).unwrap());
    assert_eq!(n, 5000);

    // Generous ceilings (debug build on a slow machine); real numbers are printed above.
    assert!(insert < Duration::from_secs(60), "insert took {insert:?}");
    assert!(reupsert < Duration::from_secs(60));
    assert!(
        name_q < Duration::from_secs(2),
        "name query took {name_q:?}"
    );
    assert!(combo < Duration::from_secs(2));
    assert!(cnt < Duration::from_secs(2));
    assert!(rate < Duration::from_secs(10));
    assert!(ann_q < Duration::from_secs(2));
}
