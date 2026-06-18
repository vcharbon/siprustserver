//! Worker-side overload signal — the X-Overload publish surface the front
//! proxy's ELU-band AIMD consumes (slice 2 of the overload rework; port of the
//! `X-Overload` half of `src/b2bua/OverloadController.ts` +
//! `src/observability/LoadSampler.ts`).
//!
//! ## What this is (and is *not*)
//!
//! The TS `OverloadController` is Tier 3 of the overload model — a token bucket,
//! a (retired) probabilistic shedder, a GC `PerformanceObserver`, routing-API
//! latency EWMAs, and the `shouldAdmit` gate. This module ports **only the
//! worker→proxy load-signal surface** the migration item targets:
//!
//!   - [`LoadSampler`] — the `elu()` / `gc_fraction()` read seam (TS `LoadSampler`).
//!   - [`OverloadSignal`] — the EWMA-smoothing + `adm` counter + the
//!     `X-Overload: v=1; elu=…; gc=…; adm=…` header builder
//!     (`OverloadController.xOverloadHeaderValue` + `incrementNonEmergencyAdmitted`).
//!
//! The token bucket / `shouldAdmit` admission gate is a separate migration item;
//! it is intentionally absent here. The **consumer** of this signal is already
//! ported — `sip_proxy::load_observer::parse_x_overload_header` parses the exact
//! `v=1` schema this module emits, so wiring [`OverloadSignal`] onto the OPTIONS
//! 200 reply closes the producer/consumer loop (the proxy's `AboveCritical`
//! exclusion finally has a producer).
//!
//! ## Sampler: rides `tokio::time`, unlike the TS raw `setInterval`
//!
//! The TS sampler is a raw `setInterval` deliberately off `TestClock`, which is
//! *why* the TS `it.live` test had to run on real time to see injected values
//! converge. The Rust sampler instead rides `tokio::time::interval`, so a
//! paused-clock test drives it with `tokio::time::advance` like every other
//! behaviour timer (CLAUDE.md: behaviour rides `tokio::time` directly). That
//! removes the need for a real-clock test — the `it.live` case ports to a
//! `start_paused` equivalent.
//!
//! State is per-worker, in-memory, behind a single `Mutex` (the read path is the
//! OPTIONS-200 hot path but it is one cheap lock + a `String` format).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// LoadSampler — current-load read seam (port of LoadSampler.ts)
// ---------------------------------------------------------------------------

/// Clamp a reading to `0..=1`, mapping non-finite to `0` (TS `clamp01`).
fn clamp01(v: f64) -> f64 {
    if !v.is_finite() {
        return 0.0;
    }
    v.clamp(0.0, 1.0)
}

/// Current-load reader: two snapshot reads consumed by the per-worker overload
/// signal pipeline. Both return a `0..=1` ratio of wall time since the previous
/// call. Smoothing (EWMA) is the consumer's ([`OverloadSignal`]) responsibility,
/// not the sampler's — keeps the test fixture simple (inject a raw value, no
/// convergence wait), exactly as in the TS source.
pub trait LoadSampler: Send + Sync {
    /// Event-Loop Utilization since the previous `elu()` call (`0..=1`). Includes
    /// busy spans (in Node, major GC pauses) — "the loop is busy".
    fn elu(&self) -> f64;
    /// Fraction of wall time spent in GC pauses since the previous `gc_fraction()`
    /// call (`0..=1`).
    fn gc_fraction(&self) -> f64;
}

/// Production [`LoadSampler`].
///
/// **Platform caveat (TODO):** Rust has no Node `perf_hooks`
/// `eventLoopUtilization()` and no managed GC, so there is no direct analogue of
/// the TS live sampler. This impl reports an ELU derived from how busy the tokio
/// worker is between samples (the fraction of wall time the sampling task was
/// *not* sleeping is a coarse proxy for loop utilization) and a GC fraction of
/// `0` (Rust has no stop-the-world GC pauses to attribute). The proxy-side band
/// classifier keys on `elu` only, so a `gc` of `0` is correct, not a stub.
///
/// A faithful tokio-runtime ELU (per-worker busy/idle accounting via
/// `tokio::runtime::RuntimeMetrics`) is a follow-up; the seam is here so swapping
/// the impl needs no caller change.
//
// TODO(migration/08): replace the elapsed-since-last-read busy proxy with
// `tokio::runtime::Handle::current().metrics()` busy-duration accounting once we
// settle on an ELU definition that matches the proxy band thresholds.
pub struct LiveLoadSampler {
    /// Monotonic instant of the previous `elu()` read; the busy proxy is the
    /// elapsed wall time since it, capped at 1.0 over a nominal sample window.
    prev_elu_at: Mutex<tokio::time::Instant>,
    /// Nominal sampler period (the 100 ms cadence) used to normalise the elapsed
    /// proxy into a `0..=1` utilization.
    sample_window: std::time::Duration,
}

impl LiveLoadSampler {
    /// Build a live sampler normalising busy-time against `sample_window` (the
    /// sampler cadence; pass [`OverloadSignal::SAMPLE_PERIOD`]).
    pub fn new(sample_window: std::time::Duration) -> Self {
        Self {
            prev_elu_at: Mutex::new(tokio::time::Instant::now()),
            sample_window,
        }
    }
}

impl LoadSampler for LiveLoadSampler {
    fn elu(&self) -> f64 {
        // Coarse busy proxy: elapsed since the previous read, normalised by the
        // sample window. Under steady sampling this hovers near 1.0 when the loop
        // is saturated (samples land late) and near the period when idle. This is
        // a placeholder until a true RuntimeMetrics busy-ratio lands (see TODO).
        let now = tokio::time::Instant::now();
        let mut prev = self.prev_elu_at.lock().unwrap();
        let elapsed = now.saturating_duration_since(*prev);
        *prev = now;
        let window = self.sample_window.as_secs_f64();
        if window <= 0.0 {
            return 0.0;
        }
        // Lag beyond the nominal window is the "busy" signal; normalise so an
        // on-time sample reads ~0 and a window-late sample reads ~1.
        let lag = (elapsed.as_secs_f64() - window).max(0.0);
        clamp01(lag / window)
    }

    fn gc_fraction(&self) -> f64 {
        // Rust has no managed stop-the-world GC; there are no GC pauses to
        // attribute, so the fraction is structurally 0 (not a stub).
        0.0
    }
}

/// Test/simulated [`LoadSampler`] with a paired control surface.
///
/// A single shared closure backs both the read seam and the control surface, so
/// a test that holds the [`SimulatedLoadControl`] and calls `set_elu(0.85)` sees
/// `0.85` from `LoadSampler::elu()` — the same single-closure guarantee the TS
/// `simulatedLayer()` provides. Build with [`simulated`].
#[derive(Clone)]
pub struct SimulatedLoadSampler {
    inner: Arc<SimulatedInner>,
}

/// The control half of [`SimulatedLoadSampler`] — set the next reading. Clamped
/// to `0..=1` (TS `LoadSamplerSimulatedControl`).
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

/// Build a simulated sampler + its control, sharing one backing cell (so a value
/// set through the control is read back through the sampler). Mirrors the TS
/// `simulatedLayer()` returning both `LoadSampler` and `LoadSamplerSimulatedControl`.
pub fn simulated() -> (SimulatedLoadSampler, SimulatedLoadControl) {
    let inner = Arc::new(SimulatedInner {
        elu_bits: AtomicU64::new(0.0f64.to_bits()),
        gc_bits: AtomicU64::new(0.0f64.to_bits()),
    });
    (
        SimulatedLoadSampler { inner: inner.clone() },
        SimulatedLoadControl { inner },
    )
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

// ---------------------------------------------------------------------------
// Ewma — moderate-smoothing exponential moving average (port of TS Ewma)
// ---------------------------------------------------------------------------

/// Simple EWMA. `alpha = 0.2` gives a ~5-sample smoothing window. Stays exactly
/// `0` until the first `observe`, so the published header reads `elu=0.000`
/// before the sampler has fired (the TS contract the schema test pins).
#[derive(Debug, Clone, Copy)]
struct Ewma {
    value: f64,
    alpha: f64,
    initialized: bool,
}

impl Ewma {
    fn new(alpha: f64) -> Self {
        Self { value: 0.0, alpha, initialized: false }
    }
    fn observe(&mut self, sample: f64) {
        if !self.initialized {
            self.value = sample;
            self.initialized = true;
        } else {
            self.value = self.alpha * sample + (1.0 - self.alpha) * self.value;
        }
    }
    fn get(&self) -> f64 {
        self.value
    }
}

// ---------------------------------------------------------------------------
// OverloadSignal — the X-Overload publish surface
// ---------------------------------------------------------------------------

/// Snapshot of the published EWMAs + the `adm` counter, for `/status` and
/// Prometheus (the subset of TS `OverloadControllerMetrics` this slice owns).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverloadMetrics {
    /// EWMA-smoothed Event Loop Utilization — the `elu` published on X-Overload.
    pub elu_ewma: f64,
    /// EWMA-smoothed GC pause fraction — the `gc` published on X-Overload.
    pub gc_fraction_ewma: f64,
    /// Monotonic count of non-emergency new-dialog INVITEs admitted by this
    /// worker — the `adm` published on X-Overload.
    pub non_emergency_admitted_total: u64,
}

struct OverloadInner {
    sampler: Arc<dyn LoadSampler>,
    elu_ewma: Ewma,
    gc_fraction_ewma: Ewma,
}

/// Worker-side overload signal. Clone-cheap (shares one `Arc`); wire one into
/// [`RouterCtx`](crate::router::RouterCtx) and read it on the OPTIONS-200 path.
///
/// The EWMAs advance only when [`sample`](OverloadSignal::sample) is called — by
/// the periodic sampler task (see [`OverloadSignal::SAMPLE_PERIOD`]). The `adm`
/// counter advances on [`increment_non_emergency_admitted`](OverloadSignal::increment_non_emergency_admitted).
#[derive(Clone)]
pub struct OverloadSignal {
    inner: Arc<Mutex<OverloadInner>>,
    /// Lock-free `adm` counter — read on the header hot path without taking the
    /// EWMA lock. Monotonic; `uint53`-safe like the TS counter.
    non_emergency_admitted: Arc<AtomicU64>,
}

impl OverloadSignal {
    /// The sampler cadence — the TS `setInterval(…, 100)`. The periodic task in
    /// `b2bua_core` calls [`sample`](OverloadSignal::sample) once per period.
    pub const SAMPLE_PERIOD: std::time::Duration = std::time::Duration::from_millis(100);

    /// Build over a [`LoadSampler`]. The EWMAs start at `0` (uninitialised) and
    /// the `adm` counter at `0`, so the first header reads
    /// `v=1; elu=0.000; gc=0.000; adm=0` until the sampler fires (TS contract).
    pub fn new(sampler: Arc<dyn LoadSampler>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(OverloadInner {
                sampler,
                elu_ewma: Ewma::new(0.2),
                gc_fraction_ewma: Ewma::new(0.2),
            })),
            non_emergency_admitted: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Default/legacy signal: a [`LiveLoadSampler`] at the standard cadence. The
    /// EWMAs only move once a sampler task drives [`sample`](OverloadSignal::sample);
    /// without one (the bare legacy path) the header is constant `elu=0.000;
    /// gc=0.000` — harmless to the proxy band (BelowSoft), which is the correct
    /// "no signal yet" classification.
    pub fn live() -> Self {
        Self::new(Arc::new(LiveLoadSampler::new(Self::SAMPLE_PERIOD)))
    }

    /// One sampler tick: read the sampler and feed both EWMAs. Called by the
    /// periodic task every [`SAMPLE_PERIOD`](OverloadSignal::SAMPLE_PERIOD).
    /// `loopLag` smoothing from the TS sampler is omitted — it fed the retired
    /// shedder, not the published signal.
    pub fn sample(&self) {
        let mut inner = self.inner.lock().unwrap();
        let elu = inner.sampler.elu();
        let gc = inner.sampler.gc_fraction();
        inner.elu_ewma.observe(elu);
        inner.gc_fraction_ewma.observe(gc);
    }

    /// Increment the monotonic counter of non-emergency new-dialog INVITEs
    /// admitted by this worker.
    ///
    /// The caller MUST guarantee the request was both (a) a new dialog (no
    /// To-tag) and (b) non-emergency. The counter is published as `adm` on every
    /// X-Overload header so LBs can derive the worker's treated rate by diffing
    /// successive samples. Port of TS `incrementNonEmergencyAdmitted`.
    pub fn increment_non_emergency_admitted(&self) {
        self.non_emergency_admitted.fetch_add(1, Ordering::Relaxed);
    }

    /// Build the value of the `X-Overload` header for this worker's current
    /// state, e.g. `v=1; elu=0.732; gc=0.012; adm=12345`. EWMAs are read directly
    /// (clamped to `0..=1` by the sampler); the `adm` counter is read lock-free.
    /// Cheap (one lock for the two EWMAs + a `String` format); safe on the
    /// OPTIONS-reply hot path. Port of TS `xOverloadHeaderValue`.
    pub fn x_overload_header_value(&self) -> String {
        let (elu, gc) = {
            let inner = self.inner.lock().unwrap();
            (inner.elu_ewma.get(), inner.gc_fraction_ewma.get())
        };
        let adm = self.non_emergency_admitted.load(Ordering::Relaxed);
        // `{:.3}` matches the TS `toFixed(3)` — three fractional digits, which the
        // proxy parser (`parse_x_overload_header`) and the schema test both expect.
        format!("v=1; elu={elu:.3}; gc={gc:.3}; adm={adm}")
    }

    /// Snapshot of the published EWMAs + the `adm` counter (for `/status`).
    pub fn metrics(&self) -> OverloadMetrics {
        let (elu_ewma, gc_fraction_ewma) = {
            let inner = self.inner.lock().unwrap();
            (inner.elu_ewma.get(), inner.gc_fraction_ewma.get())
        };
        OverloadMetrics {
            elu_ewma,
            gc_fraction_ewma,
            non_emergency_admitted_total: self.non_emergency_admitted.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Port of `OverloadController — X-Overload publishing >` "header value
    /// follows v=1 schema with elu, gc, adm fields". Format-only assertion (the
    /// EWMAs may be 0 before the sampler fires).
    #[test]
    fn header_value_follows_v1_schema() {
        let (sampler, _ctl) = simulated();
        let sig = OverloadSignal::new(Arc::new(sampler));
        let header = sig.x_overload_header_value();
        assert!(
            is_v1_schema(&header),
            "header {header:?} must match v=1; elu=<d>.<ddd>; gc=<d>.<ddd>; adm=<d>"
        );
        // And it must be exactly the zero-state header before any sample fires.
        assert_eq!(header, "v=1; elu=0.000; gc=0.000; adm=0");
    }

    /// Port of "incrementNonEmergencyAdmitted advances adm counter in the header".
    #[test]
    fn increment_non_emergency_admitted_advances_adm_in_header() {
        let (sampler, _ctl) = simulated();
        let sig = OverloadSignal::new(Arc::new(sampler));
        let before = parse_adm(&sig.x_overload_header_value());
        sig.increment_non_emergency_admitted();
        sig.increment_non_emergency_admitted();
        sig.increment_non_emergency_admitted();
        let after = parse_adm(&sig.x_overload_header_value());
        assert_eq!(after, before + 3);
        // The metrics surface mirrors the counter.
        assert_eq!(sig.metrics().non_emergency_admitted_total, before + 3);
    }

    /// Port of "metrics.eluEwma and gcFractionEwma start at 0 before the sampler
    /// fires". No `sample()` is called, so both EWMAs are still uninitialised.
    #[test]
    fn ewmas_start_at_zero_before_the_sampler_fires() {
        let (sampler, ctl) = simulated();
        // Even with a non-zero injected reading, no tick has fed the EWMA yet.
        ctl.set_elu(0.8);
        ctl.set_gc_fraction(0.2);
        let sig = OverloadSignal::new(Arc::new(sampler));
        assert_eq!(sig.metrics().elu_ewma, 0.0);
        assert_eq!(sig.metrics().gc_fraction_ewma, 0.0);
    }

    /// Port of the `it.live` "LoadSampler injection drives eluEwma once the
    /// sampler fires". The TS version needed real time because its sampler was a
    /// raw `setInterval` off `TestClock`; the Rust `sample()` is an explicit tick,
    /// so we drive it directly — no real clock, no paused-clock wait. (The
    /// periodic *task* that calls `sample` is exercised in the `b2bua_core` /
    /// router OPTIONS tests under `start_paused`.)
    #[test]
    fn load_sampler_injection_drives_elu_ewma_once_sampled() {
        let (sampler, ctl) = simulated();
        ctl.set_elu(0.8);
        ctl.set_gc_fraction(0.2);
        let sig = OverloadSignal::new(Arc::new(sampler));
        // One tick is enough for a fresh EWMA (first observe == the sample).
        sig.sample();
        assert!(sig.metrics().elu_ewma > 0.0);
        assert!(sig.metrics().gc_fraction_ewma > 0.0);
        // First observe seats the EWMA exactly at the sample.
        assert!((sig.metrics().elu_ewma - 0.8).abs() < 1e-9);
        assert!((sig.metrics().gc_fraction_ewma - 0.2).abs() < 1e-9);
        let header = sig.x_overload_header_value();
        assert!(is_v1_schema(&header), "header {header:?}");
        assert_eq!(header, "v=1; elu=0.800; gc=0.200; adm=0");
    }

    /// The EWMA smooths toward a sustained reading across repeated ticks (alpha
    /// 0.2): after the first tick seats it, subsequent ticks pull it toward the
    /// new value but never past it.
    #[test]
    fn ewma_smooths_across_repeated_samples() {
        let (sampler, ctl) = simulated();
        let sig = OverloadSignal::new(Arc::new(sampler));
        ctl.set_elu(1.0);
        sig.sample(); // seats at 1.0
        assert!((sig.metrics().elu_ewma - 1.0).abs() < 1e-9);
        ctl.set_elu(0.0);
        sig.sample(); // 0.2*0 + 0.8*1.0 = 0.8
        assert!((sig.metrics().elu_ewma - 0.8).abs() < 1e-9);
        sig.sample(); // 0.2*0 + 0.8*0.8 = 0.64
        assert!((sig.metrics().elu_ewma - 0.64).abs() < 1e-9);
    }

    /// The simulated control and sampler share one cell (TS single-closure
    /// guarantee): a value set through the control is read back through the
    /// sampler, and readings are clamped to `0..=1`.
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
    #[tokio::test(start_paused = true)]
    async fn live_sampler_reports_clamped_elu_and_zero_gc() {
        let s = LiveLoadSampler::new(Duration::from_millis(100));
        // An immediate read (no elapsed beyond the window) is ~0 and in range.
        let e0 = s.elu();
        assert!((0.0..=1.0).contains(&e0), "elu {e0} out of [0,1]");
        assert_eq!(s.gc_fraction(), 0.0);
        // Advancing well past the window saturates the busy proxy toward 1.0.
        tokio::time::advance(Duration::from_millis(500)).await;
        let e1 = s.elu();
        assert!((0.0..=1.0).contains(&e1), "elu {e1} out of [0,1]");
        assert!(e1 > 0.0, "a late sample should read busy (> 0), got {e1}");
    }

    // --- test-local header helpers (mirror the TS regex + parseAdm) -----------

    /// `^v=1; elu=<d+>.<ddd>; gc=<d+>.<ddd>; adm=<d+>$` without a regex dep.
    fn is_v1_schema(h: &str) -> bool {
        let rest = match h.strip_prefix("v=1; elu=") {
            Some(r) => r,
            None => return false,
        };
        let (elu, rest) = match rest.split_once("; gc=") {
            Some(p) => p,
            None => return false,
        };
        let (gc, adm) = match rest.split_once("; adm=") {
            Some(p) => p,
            None => return false,
        };
        is_fixed3(elu) && is_fixed3(gc) && !adm.is_empty() && adm.bytes().all(|b| b.is_ascii_digit())
    }

    /// `\d+\.\d{3}` — one or more integer digits, a dot, exactly three fractionals.
    fn is_fixed3(s: &str) -> bool {
        let (int, frac) = match s.split_once('.') {
            Some(p) => p,
            None => return false,
        };
        !int.is_empty()
            && int.bytes().all(|b| b.is_ascii_digit())
            && frac.len() == 3
            && frac.bytes().all(|b| b.is_ascii_digit())
    }

    fn parse_adm(h: &str) -> u64 {
        let idx = h.find("adm=").expect("no adm in header");
        h[idx + 4..].trim().parse().expect("adm not a number")
    }
}
