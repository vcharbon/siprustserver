//! The worker's production [`LoadSampler`]: the tokio runtime busy ratio,
//! bounded by the process's CPU budget.

use std::sync::Mutex;

use load_shed::{clamp01, LoadSampler};

use super::cpu_budget::CpuBudget;

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
///           capacity(num_workers) × wall_elapsed_since_prev
/// ```
///
/// i.e. the share of the CPU the runtime may use that it actually spent
/// processing work between two reads, clamped to `0..=1` — the signal the
/// proxy band classifier and the panic-ELU rung both key on. The
/// capacity is [`CpuBudget::capacity`]: the cgroup CPU quota when it is below
/// the worker count and the affinity set gives every worker a CPU, else the
/// worker count. The budget is re-read on every `elu()`, so a resized limit
/// takes effect at the next sample; a read that fails keeps the last one. The GC
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
    /// Reads the process's CPU budget; [`CpuBudget::read`] in production.
    budget: fn() -> Option<CpuBudget>,
    /// Last `(instant, Σ worker busy total)` snapshot; the busy ratio is the
    /// delta of the busy sum over the delta of `capacity × wall_elapsed`.
    prev: Mutex<BusySnapshot>,
}

/// A `(wall instant, summed worker busy time)` snapshot for the busy-ratio
/// diff, with the last CPU budget read in full.
struct BusySnapshot {
    at: std::time::Instant,
    busy_total: std::time::Duration,
    budget: CpuBudget,
}

impl LiveLoadSampler {
    /// Build a live sampler over the current tokio runtime (if any), bounded
    /// by this process's CPU budget. The busy ratio normalises by the *actual*
    /// wall time between reads, so no nominal sample window is configured here.
    pub(super) fn new() -> Self {
        Self::with_budget(CpuBudget::read)
    }

    /// A live sampler whose CPU budget comes from `budget`.
    pub(super) fn with_budget(budget: fn() -> Option<CpuBudget>) -> Self {
        let handle = tokio::runtime::Handle::try_current().ok();
        let busy_total = handle.as_ref().map(Self::sum_busy).unwrap_or_default();
        Self {
            handle,
            budget,
            prev: Mutex::new(BusySnapshot {
                at: std::time::Instant::now(),
                busy_total,
                budget: budget().unwrap_or(CpuBudget { quota: None, affinity: None }),
            }),
        }
    }

    /// The budget read now, or `last` when the read was voided; stored as `last`.
    fn refresh_budget(&self, last: &mut CpuBudget) -> CpuBudget {
        if let Some(budget) = (self.budget)() {
            *last = budget;
        }
        *last
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
        let workers = handle.metrics().num_workers();

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
        // Busy fraction of the CPU the runtime may use over the interval.
        let capacity = self.refresh_budget(&mut prev.budget).capacity(workers);
        clamp01(busy / (capacity * wall))
    }

    fn gc_fraction(&self) -> f64 {
        // Rust has no managed stop-the-world GC; there are no GC pauses to
        // attribute, so the fraction is structurally 0 (not a stub).
        0.0
    }
}

#[cfg(test)]
mod sampler_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A voided budget read keeps the last full one; the next full read replaces it.
    #[test]
    fn a_voided_budget_read_keeps_the_last_full_read() {
        static VOID: AtomicBool = AtomicBool::new(false);
        fn source() -> Option<CpuBudget> {
            (!VOID.load(Ordering::Relaxed))
                .then_some(CpuBudget { quota: Some(0.5), affinity: Some(8) })
        }
        let s = LiveLoadSampler::with_budget(source);
        let mut last = CpuBudget { quota: None, affinity: None };
        assert_eq!(s.refresh_budget(&mut last).capacity(1), 0.5);
        VOID.store(true, Ordering::Relaxed);
        assert_eq!(s.refresh_budget(&mut last).capacity(1), 0.5);
        assert_eq!(last.quota, Some(0.5));
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
