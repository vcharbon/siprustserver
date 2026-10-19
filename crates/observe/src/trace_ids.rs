//! Correlation-id minting for per-call traces (ADR-0026).
//!
//! Ids are W3C-shaped — a 32-hex trace id, a 16-hex span id — and are minted
//! HERE rather than by the exporter, so a domain crate populates
//! `Call.trace_id` / `Call.root_span_id` and links a takeover span without
//! linking the OpenTelemetry dependency tree. They ride on every span as
//! attributes, which is also what the HA link reads.
//!
//! The stream is a per-process xorshift64* mixed with a monotonic counter: ids
//! are unique within a process and unguessable enough for correlation. It is
//! not a cryptographic source. A thread holding a [`ThreadSeed`] mints from a
//! stream of its own instead, a function of the seed and the mint order alone,
//! so a run driven on one thread mints the same ids each time it is replayed.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

/// Hex length of a trace id.
pub const TRACE_ID_HEX: usize = 32;

/// Hex length of a span id.
pub const SPAN_ID_HEX: usize = 16;

static COUNTER: AtomicU64 = AtomicU64::new(0);
static SEED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// This thread's own stream while a [`ThreadSeed`] holds it: the seed and
    /// the count of ids minted from it.
    static THREAD_STREAM: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
}

/// While held, every id minted on the thread that took it comes from a stream
/// seeded `seed`, its n-th id the same on every replay; dropping it restores
/// the stream the thread minted from before. Ids minted on other threads keep
/// the process stream. The guard is neither `Send` nor `Sync`: it seeds the
/// thread that took it, so it cannot move to another one.
///
/// ```compile_fail
/// fn send<T: Send>(_: T) {}
/// send(observe::trace_ids::seed_thread(1));
/// ```
#[must_use = "the thread mints from the seeded stream only while the guard is held"]
pub struct ThreadSeed {
    previous: Option<(u64, u64)>,
    _this_thread: PhantomData<*const ()>,
}

/// Mint this thread's ids from `seed` until the returned guard drops.
pub fn seed_thread(seed: u64) -> ThreadSeed {
    let previous = THREAD_STREAM.with(|s| s.replace(Some((spread(seed), 0))));
    ThreadSeed { previous, _this_thread: PhantomData }
}

/// SplitMix64's finaliser: a bijection, so two seeds never share a stream, and
/// a small seed still mints ids spread over every digit.
fn spread(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Drop for ThreadSeed {
    fn drop(&mut self) {
        THREAD_STREAM.with(|s| s.set(self.previous));
    }
}

/// A fresh trace id: 32 lowercase hex digits, never all-zero.
pub fn new_trace_id() -> String {
    format!("{:016x}{:016x}", next_nonzero(), next_nonzero())
}

/// A fresh span id: 16 lowercase hex digits, never all-zero.
pub fn new_span_id() -> String {
    format!("{:016x}", next_nonzero())
}

/// Whether `id` is a syntactically usable correlation id of `width` hex digits.
/// A replicated id that fails this is not linked against — a malformed link
/// target is worse than none.
pub fn is_valid_id(id: &str, width: usize) -> bool {
    id.len() == width && id.bytes().all(|b| b.is_ascii_hexdigit()) && id.bytes().any(|b| b != b'0')
}

/// The seed and the count of the next id: this thread's stream where a
/// [`ThreadSeed`] holds one, the process stream otherwise.
fn next_draw() -> (u64, u64) {
    let seeded = THREAD_STREAM.with(|s| {
        let (seed, n) = s.get()?;
        s.set(Some((seed, n + 1)));
        Some((seed, n))
    });
    seeded.unwrap_or_else(|| (seed(), COUNTER.fetch_add(1, Ordering::Relaxed)))
}

fn next_nonzero() -> u64 {
    let (seed, n) = next_draw();
    let mut x = seed ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    if x == 0 {
        1
    } else {
        x
    }
}

/// The process seed, drawn once from wall-clock + pid entropy. Read at first
/// use, never on a paused-clock behaviour path (id minting happens only for an
/// admitted call, and admission is inert without an exporter).
fn seed() -> u64 {
    let current = SEED.load(Ordering::Relaxed);
    if current != 0 {
        return current;
    }
    let fresh = crate::rate_draw::clock_pid_entropy() | 1;
    SEED.store(fresh, Ordering::Relaxed);
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_have_the_w3c_widths_and_are_valid() {
        let trace = new_trace_id();
        let span = new_span_id();
        assert_eq!(trace.len(), TRACE_ID_HEX);
        assert_eq!(span.len(), SPAN_ID_HEX);
        assert!(is_valid_id(&trace, TRACE_ID_HEX));
        assert!(is_valid_id(&span, SPAN_ID_HEX));
    }

    #[test]
    fn ids_do_not_repeat_within_a_process() {
        let ids: HashSet<String> = (0..10_000).map(|_| new_span_id()).collect();
        assert_eq!(ids.len(), 10_000, "span ids collided");
    }

    /// The ids `seed` mints on a thread of their own: a trace id, then a span id.
    fn seeded_pair(seed: u64) -> (String, String) {
        std::thread::spawn(move || {
            let _seed = seed_thread(seed);
            (new_trace_id(), new_span_id())
        })
        .join()
        .unwrap()
    }

    #[test]
    fn a_seeded_thread_mints_the_same_ids_on_every_replay() {
        let first = seeded_pair(42);
        assert_eq!(seeded_pair(42), first, "one seed, one stream");
        assert_ne!(seeded_pair(43), first, "another seed, another stream");
        assert!(is_valid_id(&first.0, TRACE_ID_HEX) && is_valid_id(&first.1, SPAN_ID_HEX));
    }

    /// The trace ids `seed` mints on a thread of their own, a span id minted
    /// between each two.
    fn seeded_traces(seed: u64, count: usize) -> Vec<String> {
        std::thread::spawn(move || {
            let _seed = seed_thread(seed);
            (0..count).map(|_| (new_trace_id(), new_span_id()).0).collect()
        })
        .join()
        .unwrap()
    }

    #[test]
    fn the_seed_is_the_holding_threads_alone() {
        let seeded = seeded_pair(7);
        std::thread::spawn(move || {
            let _guard = seed_thread(7);
            let other = std::thread::spawn(|| (new_trace_id(), new_span_id())).join().unwrap();
            assert_ne!(other, seeded, "another thread keeps the process stream");
            assert_eq!((new_trace_id(), new_span_id()), seeded, "the holder mints from the seed");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_dropped_guard_ends_the_seeded_stream() {
        let stream = seeded_traces(7, 2);
        let after = std::thread::spawn(|| {
            let guard = seed_thread(7);
            let _ = (new_trace_id(), new_span_id());
            drop(guard);
            new_trace_id()
        })
        .join()
        .unwrap();
        assert_ne!(after, stream[1], "past its guard the thread no longer draws the seed's ids");
    }

    #[test]
    fn a_dropped_inner_guard_restores_the_outer_seeds_stream() {
        let outer = seeded_traces(7, 2);
        let resumed = std::thread::spawn(|| {
            let _outer = seed_thread(7);
            let _ = (new_trace_id(), new_span_id());
            let inner = seed_thread(9);
            let _ = (new_trace_id(), new_span_id());
            drop(inner);
            new_trace_id()
        })
        .join()
        .unwrap();
        assert_eq!(resumed, outer[1], "the outer seed's stream goes on where it stopped");
    }

    #[test]
    fn a_malformed_or_zero_id_is_refused() {
        assert!(!is_valid_id("", TRACE_ID_HEX));
        assert!(!is_valid_id(&"0".repeat(TRACE_ID_HEX), TRACE_ID_HEX));
        assert!(!is_valid_id(&"z".repeat(TRACE_ID_HEX), TRACE_ID_HEX));
        assert!(!is_valid_id(&"a".repeat(TRACE_ID_HEX - 1), TRACE_ID_HEX));
    }
}
