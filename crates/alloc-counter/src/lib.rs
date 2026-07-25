//! Counting `#[global_allocator]` wrapper — the measurement device behind
//! allocation-budget tests.
//!
//! A test binary installs [`CountingAlloc`] as its global allocator and wraps a
//! region of work in [`measure`]; the result is the exact number of allocation
//! events and requested bytes that region performed. Unlike a CPU profile this
//! is **deterministic** — the same input yields the same count on every machine
//! and every run — so it can be asserted as a budget in the default test lane.
//!
//! ## Accounting
//!
//! - An **allocation event** is one allocator round-trip that hands back
//!   storage: `alloc`, `alloc_zeroed`, and `realloc` each count as one. A
//!   `realloc` counts because it is a real allocator call (usually a copy of the
//!   whole buffer) — it is the `finish_grow` / `do_reserve_and_handle` cost of a
//!   `Vec`/`String` that was not sized up front.
//! - **Bytes** are *requested* bytes, not resident bytes: `layout.size()` for a
//!   fresh allocation, and the growth delta (`new_size - old_size`) for a
//!   `realloc`. So a buffer that grows 8→16→32 charges 8 + 8 + 16 = 32 bytes,
//!   the total storage the operation asked the allocator for.
//! - Frees are not counted. A budget is about how much work the allocator is
//!   asked to do, and every transient a parse mints is freed anyway.
//!
//! ## Scope
//!
//! The counters are process-wide, so a measured region must be the only thing
//! running: measure on a single thread, in a test that does not run concurrent
//! work of its own. Counting is two relaxed atomic adds per allocation — enough
//! to skew a wall-clock benchmark, which is why this is a *counting* device and
//! criterion remains the timing one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Allocation events (`alloc` + `alloc_zeroed` + `realloc`) since process start.
static ALLOC_EVENTS: AtomicUsize = AtomicUsize::new(0);
/// Requested bytes since process start (fresh size, or growth delta on realloc).
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

/// A pass-through allocator over the system allocator that tallies every
/// allocation event and the bytes it requested. Install it in a test binary:
///
/// ```ignore
/// #[global_allocator]
/// static ALLOC: alloc_counter::CountingAlloc = alloc_counter::CountingAlloc;
/// ```
pub struct CountingAlloc;

// SAFETY: every method forwards its arguments unchanged to the system
// allocator and returns its pointer unchanged, so the `GlobalAlloc` contract
// (valid layouts in, allocator-owned pointers out, deallocation with the
// matching layout) is exactly the system allocator's, which upholds it. The
// added work is two relaxed atomic counter adds, which touch no allocation.
#[allow(unsafe_code)] // irreducible: GlobalAlloc's methods are unsafe by definition
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_EVENTS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_EVENTS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        System.alloc_zeroed(layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_EVENTS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size.saturating_sub(layout.size()), Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}

/// What one measured region cost the allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocCost {
    /// Allocation events (`alloc` + `alloc_zeroed` + `realloc`).
    pub allocs: usize,
    /// Requested bytes (fresh size, or growth delta on `realloc`).
    pub bytes: usize,
}

impl AllocCost {
    /// This cost amortized over `n` operations, rounded to the nearest whole
    /// unit — the per-message figure a budget is stated in.
    pub fn per(self, n: usize) -> AllocCost {
        let n = n.max(1);
        AllocCost { allocs: self.allocs.div_ceil(n), bytes: self.bytes.div_ceil(n) }
    }
}

/// Run `f` and report what it cost the allocator, alongside its result.
///
/// The result is returned rather than dropped inside the region, so storage the
/// operation *retains* is charged to it (a parse that owns its header strings
/// pays for them here) while the cost of freeing is charged to nobody.
pub fn measure<T>(f: impl FnOnce() -> T) -> (T, AllocCost) {
    let allocs0 = ALLOC_EVENTS.load(Ordering::Relaxed);
    let bytes0 = ALLOC_BYTES.load(Ordering::Relaxed);
    let out = f();
    let cost = AllocCost {
        allocs: ALLOC_EVENTS.load(Ordering::Relaxed) - allocs0,
        bytes: ALLOC_BYTES.load(Ordering::Relaxed) - bytes0,
    };
    (out, cost)
}
