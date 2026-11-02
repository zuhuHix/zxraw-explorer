//! The browse index's memory estimate (what its cache budget counts) against what it really
//! holds, measured by a counting allocator, for a deep and wide folder tree.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use forge_core::archive::{ArchiveEntry, ArchiveFormat, ArchiveIndex, ArchiveListing, Codec};

/// Counts the bytes live on the heap (this test binary runs one test).
struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(
            new_size as isize - layout.size() as isize,
            Ordering::Relaxed,
        );
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn entry(name: String, is_dir: bool) -> ArchiveEntry {
    ArchiveEntry {
        name,
        is_dir,
        size: if is_dir { 0 } else { 100 },
        compressed_size: 50,
        method: "gzip".into(),
        safe_name: true,
        ..Default::default()
    }
}

#[test]
fn the_index_estimate_bounds_what_it_holds() {
    let before = LIVE.load(Ordering::Relaxed);
    let index = {
        // 40 top folders, each 6 levels deep, with 25 files and a sub-folder per level; no
        // folder entries of their own for half of them (implied folders).
        let mut entries = Vec::new();
        for top in 0..40 {
            let mut dir = format!("top-{top:02}-{}", "w".repeat(20));
            for depth in 0..6 {
                if top % 2 == 0 {
                    entries.push(entry(format!("{dir}/"), true));
                }
                for f in 0..25 {
                    entries.push(entry(format!("{dir}/file-{f:03}.txt"), false));
                }
                dir = format!("{dir}/level-{depth}");
            }
        }
        let listing = ArchiveListing {
            format: ArchiveFormat::Tar(Codec::Gzip),
            entries,
            comment: String::new(),
        };
        Box::new(ArchiveIndex::build(listing))
    };
    let held = (LIVE.load(Ordering::Relaxed) - before) as u64;
    let estimate = index.memory_estimate();
    assert!(
        estimate >= held,
        "the estimate ({estimate}) must not be below the heap held ({held})"
    );
    assert!(
        estimate <= held * 2,
        "the estimate ({estimate}) should be within twice the heap held ({held})"
    );
    drop(index);
}
