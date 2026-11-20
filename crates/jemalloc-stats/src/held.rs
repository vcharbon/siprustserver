//! What the allocator counts held, read around a region of work: the
//! process's `(allocated, active)` bytes, and the bytes one thread holds.

#[cfg(not(target_env = "msvc"))]
mod imp {
    use tikv_jemalloc_ctl::{epoch, stats, thread};

    /// `(allocated, active)` bytes from a fresh stats epoch; `None` when
    /// jemalloc is not answering.
    pub fn footprint_bytes() -> Option<(u64, u64)> {
        epoch::advance().ok()?;
        Some((stats::allocated::read().ok()? as u64, stats::active::read().ok()? as u64))
    }

    /// Bytes the calling thread holds: every byte it allocated less every
    /// byte it freed (`thread.allocated` less `thread.deallocated`). No other
    /// thread moves it, so two readings around synchronous work on one thread
    /// give what that work kept. Negative once the thread freed more than it
    /// allocated; `None` when jemalloc is not answering.
    pub fn thread_held_bytes() -> Option<i64> {
        let allocated = thread::allocatedp::read().ok()?.get();
        let freed = thread::deallocatedp::read().ok()?.get();
        Some(i64::try_from(allocated).ok()? - i64::try_from(freed).ok()?)
    }
}

#[cfg(target_env = "msvc")]
mod imp {
    /// No jemalloc on msvc: nothing to read.
    pub fn footprint_bytes() -> Option<(u64, u64)> {
        None
    }
    pub fn thread_held_bytes() -> Option<i64> {
        None
    }
}

pub use imp::{footprint_bytes, thread_held_bytes};
