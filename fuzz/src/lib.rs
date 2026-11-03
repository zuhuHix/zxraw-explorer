//! Shared by every fuzz target: a tracking allocator that aborts on an outsized allocation
//! (a hostile header asking for gigabytes) and runs the target through forge-core's harness.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

pub use forge_core::archive::fuzz::Target;

/// Largest single allocation allowed.
const MAX_ONE: usize = 256 << 20;
/// Most bytes live at once (Rust allocations only; C codecs are bounded by `-rss_limit_mb`).
const MAX_LIVE: usize = 1 << 30;

static LIVE: AtomicUsize = AtomicUsize::new(0);

pub struct Tracking;

fn over(what: &str, size: usize) -> ! {
    // No allocation here: we are inside the allocator.
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "==zxraw-fuzz== {what}: {size} bytes");
    std::process::abort();
}

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.size() > MAX_ONE {
            over("single allocation over the cap", l.size());
        }
        let live = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
        if live > MAX_LIVE {
            over("live allocations over the cap", live);
        }
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if l.size() > MAX_ONE {
            over("single allocation over the cap", l.size());
        }
        let live = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
        if live > MAX_LIVE {
            over("live allocations over the cap", live);
        }
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        if new > MAX_ONE {
            over("single allocation over the cap", new);
        }
        if new >= l.size() {
            let live = LIVE.fetch_add(new - l.size(), Ordering::Relaxed) + (new - l.size());
            if live > MAX_LIVE {
                over("live allocations over the cap", live);
            }
        } else {
            LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
        }
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

pub fn run(target: Target, data: &[u8]) {
    forge_core::archive::fuzz::run(target, data);
}
