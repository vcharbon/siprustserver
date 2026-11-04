//! The allocation cap (ADR-0051 §4: no allocation sized from an input value
//! without a bound). libFuzzer's `-malloc_limit_mb` needs a sanitizer's
//! malloc hooks, which an uninstrumented-memory build lacks, so the targets
//! carry the cap themselves: one request over [`LIMIT`] aborts, and libFuzzer
//! keeps the input as a crash.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write as _;

/// The largest single allocation a case may make. No input of a target's
/// size reaches it by its length alone.
pub const LIMIT: usize = 256 << 20;

/// The system allocator, refusing any one request over [`LIMIT`].
pub struct CappedAlloc;

impl CappedAlloc {
    fn check(size: usize) {
        if size > LIMIT {
            // A fixed message: formatting here would allocate.
            let _ = std::io::stderr().write_all(b"==alloc-cap== one allocation over the cap\n");
            std::process::abort();
        }
    }
}

// SAFETY: every call forwards to `System` unchanged once the size is checked.
unsafe impl GlobalAlloc for CappedAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::check(layout.size());
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::check(layout.size());
        System.alloc_zeroed(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::check(new_size);
        System.realloc(ptr, layout, new_size)
    }
}
