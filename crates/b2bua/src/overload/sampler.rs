//! Current-load read seam: the [`LoadSampler`] trait, the production tokio
//! busy-ratio sampler, and the injectable [`simulated`] pair for tests.
//! The front proxy's self-gate sampler does NOT live here — see
//! `sip_proxy::self_gate`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Clamp a reading to `0..=1`, mapping non-finite to `0`.
fn clamp01(v: f64) -> f64 {
    if !v.is_finite() {
        return 0.0;
    }
    v.clamp(0.0, 1.0)
}

/// Current-load reader: two snapshot reads consumed by the per-worker overload
/// signal pipeline. Both return a `0..=1` ratio of wall time since the previous
/// call. Smoothing (EWMA) is the consumer's
/// ([`OverloadSignal`](super::OverloadSignal)) responsibility, not the
/// sampler's — a test fixture injects a raw value with no convergence wait.
pub trait LoadSampler: Send + Sync {
    /// Event-Loop Utilization since the previous `elu()` call (`0..=1`) — "the
    /// loop is busy".
    fn elu(&self) -> f64;
    /// Fraction of wall time spent in GC pauses since the previous `gc_fraction()`
    /// call (`0..=1`).
    fn gc_fraction(&self) -> f64;
}

/// Production [`LoadSampler`] — a faithful tokio runtime busy-ratio.
///
/// The multi-thread tokio runtime exposes per-worker cumulative busy time via
/// [`RuntimeMetrics::worker_total_busy_duration`]
/// (`tokio::runtime::Handle::current().metrics()`). The published ELU is the
/// **busy fraction since the previous `elu()` read**:
///
/// ```text
///   elu = (Σ_w busy_total(w)_now − Σ_w busy_total(w)_prev)
///         ─────────────────────────────────────────────────
///                 num_workers × wall_elapsed_since_prev
/// ```
///
/// i.e. the share of available worker-thread time the runtime actually spent
/// processing work between two reads, clamped to `0..=1` — the signal the
/// proxy band classifier and the Tier-3 panic-ELU backstop both key on. The GC
/// fraction is structurally `0`: Rust has no stop-the-world GC pauses to
/// attribute.
///
/// **Requires `--cfg tokio_unstable`** (set workspace-wide in
/// `/.cargo/config.toml`).
///
/// **Runtime handle.** Captured once at construction via
/// [`Handle::try_current`](tokio::runtime::Handle::try_current). The production
/// sampler is built inside the worker runtime
/// ([`OverloadSignal::live`](super::OverloadSignal::live) →
/// `b2bua_core::spawn_with_overload`, an async context), so the handle is
/// present and points at the worker runtime. Built **outside** any runtime
/// (e.g. a bare `#[test]` that only reads the zero-state header),
/// `try_current` yields `None` and `elu()` reads a constant `0.0` — the
/// correct "no signal" classification.
pub(super) struct LiveLoadSampler {
    /// The runtime handle captured at construction (`None` when built outside a
    /// runtime). Reads `RuntimeMetrics` off it on every `elu()`.
    handle: Option<tokio::runtime::Handle>,
    /// Last `(instant, Σ worker busy total)` snapshot; the busy ratio is the
    /// delta of the busy sum over the delta of `num_workers × wall_elapsed`.
    prev: Mutex<BusySnapshot>,
}

/// A `(wall instant, summed worker busy time)` snapshot for the busy-ratio diff.
struct BusySnapshot {
    at: std::time::Instant,
    busy_total: std::time::Duration,
}

impl LiveLoadSampler {
    /// Build a live sampler over the current tokio runtime (if any). The busy
    /// ratio normalises by the *actual* wall time between reads, so no nominal
    /// sample window is configured here.
    pub(super) fn new() -> Self {
        let handle = tokio::runtime::Handle::try_current().ok();
        let busy_total = handle.as_ref().map(Self::sum_busy).unwrap_or_default();
        Self {
            handle,
            prev: Mutex::new(BusySnapshot { at: std::time::Instant::now(), busy_total }),
        }
    }

    /// Sum `worker_total_busy_duration` across all runtime workers (the cumulative
    /// busy clock; monotonic, never reset). `cfg(tokio_unstable)` gates the broader
    /// metrics surface — see the struct docs.
    fn sum_busy(handle: &tokio::runtime::Handle) -> std::time::Duration {
        let m = handle.metrics();
        let n = m.num_workers();
        (0..n).map(|w| m.worker_total_busy_duration(w)).sum()
    }
}

impl LoadSampler for LiveLoadSampler {
    fn elu(&self) -> f64 {
        // No runtime handle (built outside a runtime) → no signal → 0.0.
        let Some(handle) = self.handle.as_ref() else {
            return 0.0;
        };
        let now = std::time::Instant::now();
        let busy_now = Self::sum_busy(handle);
        let num_workers = handle.metrics().num_workers().max(1);

        let mut prev = self.prev.lock().unwrap();
        let wall = now.saturating_duration_since(prev.at).as_secs_f64();
        // Busy clock is monotonic, but guard against a zero/negative wall window
        // (two reads in the same instant) which would divide by ~0.
        if wall <= 0.0 {
            return 0.0;
        }
        let busy = busy_now.saturating_sub(prev.busy_total).as_secs_f64();
        prev.at = now;
        prev.busy_total = busy_now;
        // Busy fraction of the available worker-thread time over the interval.
        clamp01(busy / (num_workers as f64 * wall))
    }

    fn gc_fraction(&self) -> f64 {
        // Rust has no managed stop-the-world GC; there are no GC pauses to
        // attribute, so the fraction is structurally 0 (not a stub).
        0.0
    }
}

/// Test/simulated [`LoadSampler`] with a paired control surface.
///
/// A single shared cell backs both the read seam and the control surface, so a
/// test that holds the [`SimulatedLoadControl`] and calls `set_elu(0.85)` sees
/// `0.85` from `LoadSampler::elu()`. Build with [`simulated`].
#[derive(Clone)]
pub struct SimulatedLoadSampler {
    inner: Arc<SimulatedInner>,
}

/// The control half of [`SimulatedLoadSampler`] — set the next reading.
/// Clamped to `0..=1`.
#[derive(Clone)]
pub struct SimulatedLoadControl {
    inner: Arc<SimulatedInner>,
}

struct SimulatedInner {
    // Stored as the bit pattern of an f64 so the read seam is lock-free and the
    // control writes are atomic — a test on another task observes the latest set.
    elu_bits: AtomicU64,
    gc_bits: AtomicU64,
}

/// Build a simulated sampler + its control, sharing one backing cell — a value
/// set through the control is read back through the sampler.
pub fn simulated() -> (SimulatedLoadSampler, SimulatedLoadControl) {
    let inner = Arc::new(SimulatedInner {
        elu_bits: AtomicU64::new(0.0f64.to_bits()),
        gc_bits: AtomicU64::new(0.0f64.to_bits()),
    });
    (SimulatedLoadSampler { inner: inner.clone() }, SimulatedLoadControl { inner })
}

impl LoadSampler for SimulatedLoadSampler {
    fn elu(&self) -> f64 {
        f64::from_bits(self.inner.elu_bits.load(Ordering::Relaxed))
    }
    fn gc_fraction(&self) -> f64 {
        f64::from_bits(self.inner.gc_bits.load(Ordering::Relaxed))
    }
}

impl SimulatedLoadControl {
    /// Set the next `elu()` reading (clamped to `0..=1`).
    pub fn set_elu(&self, v: f64) {
        self.inner.elu_bits.store(clamp01(v).to_bits(), Ordering::Relaxed);
    }
    /// Set the next `gc_fraction()` reading (clamped to `0..=1`).
    pub fn set_gc_fraction(&self, v: f64) {
        self.inner.gc_bits.store(clamp01(v).to_bits(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod sampler_tests {
    use super::*;

    /// The simulated control and sampler share one cell: a value set through
    /// the control is read back through the sampler, and readings are clamped
    /// to `0..=1`.
    #[test]
    fn simulated_control_and_sampler_share_one_cell_and_clamp() {
        let (sampler, ctl) = simulated();
        ctl.set_elu(0.85);
        ctl.set_gc_fraction(0.1);
        assert!((sampler.elu() - 0.85).abs() < 1e-9);
        assert!((sampler.gc_fraction() - 0.1).abs() < 1e-9);
        // Clamp: above 1 → 1, below 0 → 0, NaN → 0.
        ctl.set_elu(5.0);
        assert_eq!(sampler.elu(), 1.0);
        ctl.set_elu(-1.0);
        assert_eq!(sampler.elu(), 0.0);
        ctl.set_elu(f64::NAN);
        assert_eq!(sampler.elu(), 0.0);
    }

    /// The live sampler reports a `0..=1` ELU and a structurally-`0` GC fraction.
    ///
    /// The busy-ratio sampler measures REAL runtime busy time, not
    /// `tokio::time` — so under a paused, idle runtime the busy fraction is ~0,
    /// and forcing a non-zero value here is neither possible nor meaningful
    /// (the real end-to-end signal is validated on the cluster, per the module
    /// docs). This pins only the invariants that hold everywhere: every
    /// `elu()` is a clamped `0..=1` value and `gc_fraction()` is structurally
    /// `0`. Uses a multi-thread runtime so
    /// `RuntimeMetrics::worker_total_busy_duration` has real workers to sum
    /// (the default current-thread test runtime has one).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_sampler_reports_clamped_elu_and_zero_gc() {
        let s = LiveLoadSampler::new();
        // An immediate read is in range; GC fraction is structurally 0.
        let e0 = s.elu();
        assert!((0.0..=1.0).contains(&e0), "elu {e0} out of [0,1]");
        assert_eq!(s.gc_fraction(), 0.0);
        // A later read (after real time passes + the runtime does some work) is
        // still a clamped, in-range value — that is all the unit test asserts.
        tokio::task::yield_now().await;
        let e1 = s.elu();
        assert!((0.0..=1.0).contains(&e1), "elu {e1} out of [0,1]");
        assert_eq!(s.gc_fraction(), 0.0);
    }
}
