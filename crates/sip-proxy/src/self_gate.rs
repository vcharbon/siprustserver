//! [`ProxySelfGate`] — proxy self-overload admission gate (port of
//! `src/sip-front-proxy/ProxySelfGate.ts`).
//!
//! The front proxy is a single task with its own event-loop budget. Under flood
//! the proxy itself saturates before any worker sees the traffic. This gate
//! sheds **external, new-dialog, non-emergency** INVITEs on two cheap binary
//! checks:
//!
//!   - `proxy_elu > elu_critical` — hard rejection by event-loop pressure
//!     (`proxy_overload_elu`).
//!   - a per-class CPS token bucket — hard cap on the external new-dialog
//!     non-emergency rate (`proxy_overload_cps`).
//!
//! Caller classification (emergency / in-dialog / worker-originated) happens in
//! [`crate::core::request`] *before* this gate; internal traffic from workers
//! always bypasses (rejecting a worker's B-leg INVITE causes re-routing churn
//! rather than load shedding). The request path's branch + the [`note_bypass`]
//! counters are wired exactly as they were against the prior always-admit stub
//! — only the decision is now real.
//!
//! Unlike the per-worker AIMD on the LB-side [`crate::load_observer`], this gate
//! is binary: the proxy's ELU is its own, with no second party to converge with.
//!
//! ## Clock — rides `tokio::time`, NOT the TS raw `setInterval`
//!
//! The TS sampler is a raw `setInterval` deliberately off `TestClock` (which is
//! *why* the TS code path needed real time for an injected ELU to converge). The
//! Rust [`TokenBucket`] refills on `tokio::time::Instant` and the EWMA is fed by
//! an explicit [`EluCpsGate::sample`] tick driven by a `tokio::time::interval`
//! task in the runner — so a `start_paused` test advances both with
//! `tokio::time::advance` like every other behaviour timer (CLAUDE.md: behaviour
//! rides `tokio::time` directly — there is no separate fake clock to keep in
//! sync). This is the same shape the b2bua-side `overload.rs` uses.
//!
//! ## Why the bits below are inlined (not shared with the b2bua)
//!
//! The TS [`TokenBucket`]/`LoadSampler`/EWMA are **inlined here** rather than
//! imported from the worker, exactly as the TS source inlines its own
//! `TokenBucket` ("the dep graph between b2bua and front-proxy stays one-way").
//! `sip-proxy` does not depend on `b2bua`; duplicating these few small,
//! independently-tested primitives keeps that edge absent.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Decision / bypass types (port of `AdmitDecision` + the caller-classified
// bypass kinds)
// ---------------------------------------------------------------------------

/// The outcome of an admission check (port of TS `AdmitDecision`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmitDecision {
    pub admit: bool,
    /// Set on rejection — the `Reason` phrase (`proxy_overload_elu` /
    /// `proxy_overload_cps`). `None` on an admit.
    pub reason: Option<String>,
    /// `Retry-After` seconds on rejection. `0` on an admit.
    pub retry_after_sec: u32,
}

impl AdmitDecision {
    pub fn admit() -> Self {
        Self { admit: true, reason: None, retry_after_sec: 0 }
    }
    /// A reject carrying the `Reason` phrase + a `Retry-After` hint.
    fn reject(reason: &str, retry_after_sec: u32) -> Self {
        Self { admit: false, reason: Some(reason.to_string()), retry_after_sec }
    }
}

/// Why a request bypassed the gate (for the metrics path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassKind {
    Internal,
    Emergency,
}

/// The admission seam. A trait so the gate can be swapped (e.g. a test double)
/// without touching the request path.
pub trait ProxySelfGate: Send + Sync {
    /// Decide whether to admit an external, non-emergency, new-dialog request.
    fn try_admit_external(&self) -> AdmitDecision;
    /// Note that a request bypassed the gate (internal / emergency).
    fn note_bypass(&self, _kind: BypassKind) {}
}

/// The always-admit gate — the trivial [`ProxySelfGate`] for deployments that
/// run no self-overload protection (and the test default in `ProxyCoreBuilder`).
#[derive(Debug, Default, Clone)]
pub struct AlwaysAdmitGate;

impl ProxySelfGate for AlwaysAdmitGate {
    fn try_admit_external(&self) -> AdmitDecision {
        AdmitDecision::admit()
    }
}

// ---------------------------------------------------------------------------
// Config (port of `ProxySelfGateConfigData` + `defaultProxySelfGateConfig`)
// ---------------------------------------------------------------------------

/// Tunables for [`EluCpsGate`] (port of TS `ProxySelfGateConfigData`).
#[derive(Debug, Clone, Copy)]
pub struct ProxySelfGateConfig {
    /// Above this ELU the gate rejects every external new-dialog INVITE.
    pub elu_critical: f64,
    /// Token-bucket capacity (= max burst size).
    pub cps_bucket_size: u32,
    /// Bucket refill rate (tokens/sec).
    pub cps_bucket_rate: u32,
    /// EWMA α for the proxy's own ELU.
    pub elu_smoothing_alpha: f64,
    /// Sampler cadence — the period of the runner task that drives
    /// [`EluCpsGate::sample`]. The TS default is 100 ms (`setInterval`).
    pub sampler_interval: std::time::Duration,
}

impl Default for ProxySelfGateConfig {
    /// Aggressive defaults, copied verbatim from `defaultProxySelfGateConfig`
    /// (calibrated against the 2026-05-16 perf test). `cps_bucket_size = 50`,
    /// `cps_bucket_rate = 100`: at 200 CAPS the bucket drains in ~500 ms then
    /// admits at the refill rate; a sustained 50 CAPS non-emergency baseline
    /// passes cleanly. `elu_critical = 0.80` fires the gate the moment the
    /// proxy's own loop exceeds 80 % utilisation.
    fn default() -> Self {
        Self {
            elu_critical: 0.8,
            cps_bucket_size: 50,
            cps_bucket_rate: 100,
            elu_smoothing_alpha: 0.2,
            sampler_interval: std::time::Duration::from_millis(100),
        }
    }
}

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

/// Current-load reader: two snapshot reads consumed by the proxy-self gate. Both
/// return a `0..=1` ratio of wall time since the previous call. Smoothing (EWMA)
/// is the consumer's responsibility, not the sampler's — keeps the test fixture
/// simple (inject a raw value, no convergence wait), exactly as in the TS source.
///
/// `gc_fraction()` is day-1 informational only (the gate keys on `elu` alone),
/// mirroring the TS `gcFraction` gauge.
pub trait LoadSampler: Send + Sync {
    /// Event-Loop Utilization since the previous `elu()` call (`0..=1`).
    fn elu(&self) -> f64;
    /// Fraction of wall time spent in GC pauses since the previous read (`0..=1`).
    fn gc_fraction(&self) -> f64;
}

/// Production [`LoadSampler`].
///
/// **Platform caveat (TODO):** Rust has no Node `perf_hooks`
/// `eventLoopUtilization()` and no managed GC, so there is no direct analogue of
/// the TS live sampler. This impl reports an ELU derived from how late the
/// periodic sampler task lands (the fraction of wall time beyond the nominal
/// window is a coarse proxy for loop saturation — when the runtime is busy the
/// 100 ms task lands late) and a GC fraction of `0` (Rust has no stop-the-world
/// GC pauses to attribute). The gate keys on `elu` only, so a `gc` of `0` is
/// correct, not a stub. This is the **same** coarse busy-proxy the b2bua-side
/// `LiveLoadSampler` uses; the fidelity debt and the `RuntimeMetrics` follow-up
/// are tracked there (MIGRATION_STATUS debt (1)).
//
// TODO(migration/14): replace the elapsed-since-last-read busy proxy with
// `tokio::runtime::Handle::current().metrics()` busy-duration accounting once an
// ELU definition that matches `elu_critical` is settled (shared with migration/08).
pub struct LiveLoadSampler {
    prev_elu_at: Mutex<tokio::time::Instant>,
    sample_window: std::time::Duration,
}

impl LiveLoadSampler {
    /// Build a live sampler normalising busy-time against `sample_window` (the
    /// sampler cadence; pass [`ProxySelfGateConfig::sampler_interval`]).
    pub fn new(sample_window: std::time::Duration) -> Self {
        Self { prev_elu_at: Mutex::new(tokio::time::Instant::now()), sample_window }
    }
}

impl LoadSampler for LiveLoadSampler {
    fn elu(&self) -> f64 {
        let now = tokio::time::Instant::now();
        let mut prev = self.prev_elu_at.lock().unwrap();
        let elapsed = now.saturating_duration_since(*prev);
        *prev = now;
        let window = self.sample_window.as_secs_f64();
        if window <= 0.0 {
            return 0.0;
        }
        // Lag beyond the nominal window is the "busy" signal; an on-time sample
        // reads ~0, a window-late sample reads ~1.
        let lag = (elapsed.as_secs_f64() - window).max(0.0);
        clamp01(lag / window)
    }

    fn gc_fraction(&self) -> f64 {
        // Rust has no managed stop-the-world GC; structurally 0 (not a stub).
        0.0
    }
}

/// Test/simulated [`LoadSampler`] with a paired control surface.
///
/// A single shared cell backs both the read seam and the control surface, so a
/// test that holds the [`SimulatedLoadControl`] and calls `set_elu(0.85)` sees
/// `0.85` from `LoadSampler::elu()` — the single-closure guarantee the TS
/// `simulatedLayer()` provides. Build with [`simulated`].
#[derive(Clone)]
pub struct SimulatedLoadSampler {
    inner: Arc<SimulatedInner>,
}

/// The control half of [`SimulatedLoadSampler`] — set the next reading (clamped
/// to `0..=1`; TS `LoadSamplerSimulatedControl`).
#[derive(Clone)]
pub struct SimulatedLoadControl {
    inner: Arc<SimulatedInner>,
}

struct SimulatedInner {
    // Bit patterns of f64s so the read seam is lock-free and the control writes
    // are atomic — a test on another task observes the latest set.
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

// ---------------------------------------------------------------------------
// Ewma — the EWMA the TS sampler applies to `proxy_elu`
// ---------------------------------------------------------------------------

/// Simple EWMA. Stays exactly `0` until the first `observe` (then the first
/// observe seats it at the sample), matching the TS `initialized` flag — so the
/// gate reads `elu_ewma = 0` before the sampler has ever fired and never sheds
/// on a cold start.
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
// TokenBucket — the hard per-class CPS gate (port of the inlined TS `TokenBucket`)
// ---------------------------------------------------------------------------

/// Lazy-refill token bucket. Tokens accrue continuously at `rate_per_sec` up to
/// `capacity`; [`try_consume`](TokenBucket::try_consume) succeeds iff ≥ 1 token
/// is available. Port of the `TokenBucket` inlined in `ProxySelfGate.ts`.
///
/// **Clock — rides `tokio::time::Instant`, not wall time.** The TS source refills
/// off `Date.now()`; here the elapsed-since-last-refill is measured on
/// `tokio::time::Instant`, which `tokio::time::advance` moves under a paused
/// runtime (CLAUDE.md: behaviour rides `tokio::time` directly). A `start_paused`
/// test that advances 1 s sees exactly `rate_per_sec` tokens refill,
/// deterministically, with no real sleeping.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    rate_per_sec: f64,
    last_refill: tokio::time::Instant,
}

impl TokenBucket {
    /// Build a full bucket (`tokens == capacity`) refilling at `rate_per_sec`.
    fn new(capacity: u32, rate_per_sec: u32) -> Self {
        Self {
            tokens: capacity as f64,
            capacity: capacity as f64,
            rate_per_sec: rate_per_sec as f64,
            last_refill: tokio::time::Instant::now(),
        }
    }

    /// Accrue tokens for the elapsed time since the last refill (capped at
    /// `capacity`). A no-op when no time has passed (paused clock between
    /// advances) — the TS `elapsedSec <= 0` guard.
    fn refill(&mut self) {
        let now = tokio::time::Instant::now();
        let elapsed_sec = now.saturating_duration_since(self.last_refill).as_secs_f64();
        if elapsed_sec <= 0.0 {
            return;
        }
        self.tokens = self.capacity.min(self.tokens + elapsed_sec * self.rate_per_sec);
        self.last_refill = now;
    }

    /// Try to consume one token. Returns `true` (and decrements) iff ≥ 1 is
    /// available after a refill; `false` leaves the bucket untouched.
    fn try_consume(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Current level, floored at `0`. Port of TS `level`.
    fn level(&mut self) -> f64 {
        self.refill();
        self.tokens.max(0.0)
    }

    /// Seconds until ≥ 1 token will be available (`0` if available now). With a
    /// zero refill rate and an empty bucket, returns `60` (the TS fallback so a
    /// misconfigured `rate == 0` still hands the caller a finite Retry-After).
    /// Port of TS `retryAfterSec`.
    fn retry_after_sec(&mut self) -> u32 {
        self.refill();
        if self.tokens >= 1.0 {
            return 0;
        }
        if self.rate_per_sec <= 0.0 {
            return 60;
        }
        ((1.0 - self.tokens) / self.rate_per_sec).ceil() as u32
    }
}

// ---------------------------------------------------------------------------
// EluCpsGate — the real proxy-self admission gate (port of `ProxySelfGate`)
// ---------------------------------------------------------------------------

/// Rejection reason phrases — the exact `Reason`/metric strings the TS gate
/// emits (`ProxySelfRejection`), reused on the wire by [`crate::core::request`].
const REASON_ELU: &str = "proxy_overload_elu";
const REASON_CPS: &str = "proxy_overload_cps";

/// Snapshot of the gate's published state, for `/status` + Prometheus (port of
/// the subset of `ProxySelfGateMetrics` this gate owns).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProxySelfGateMetrics {
    /// EWMA-smoothed proxy ELU (`0..=1`) — crosses `elu_critical` → 503.
    pub elu_ewma: f64,
    /// Most recent raw GC fraction (day-1 informational only).
    pub gc_fraction: f64,
    /// Token-bucket fill level (tokens remaining, floored at 0).
    pub cps_bucket_level: f64,
    /// Configured bucket capacity (constant per config; a gauge for ratios).
    pub cps_bucket_max: f64,
    /// External new-dialog non-emergency INVITEs admitted by the gate.
    pub external_admitted_total: u64,
    /// Rejections by ELU pressure (`proxy_overload_elu`).
    pub rejected_elu_total: u64,
    /// Rejections by the CPS bucket (`proxy_overload_cps`).
    pub rejected_cps_total: u64,
    /// Worker-originated INVITEs that bypassed the gate (`is_worker_outbound`).
    pub internal_bypassed_total: u64,
    /// Emergency INVITEs that bypassed the gate.
    pub emergency_bypassed_total: u64,
}

/// Mutable, lock-guarded gate state (the EWMA + the token bucket + the latest GC
/// fraction). Behind one `Mutex`; the admission path is one cheap lock.
struct GateInner {
    sampler: Arc<dyn LoadSampler>,
    elu_ewma: Ewma,
    gc_fraction: f64,
    bucket: TokenBucket,
}

/// The real ELU/CPS proxy-self gate (port of TS `ProxySelfGate`). Clone-cheap
/// (shares one `Arc`); construct one with [`EluCpsGate::new`]/[`EluCpsGate::live`],
/// hand it to `ProxyCoreBuilder::self_gate`, and drive [`sample`](EluCpsGate::sample)
/// from a periodic task at [`ProxySelfGateConfig::sampler_interval`].
#[derive(Clone)]
pub struct EluCpsGate {
    inner: Arc<Mutex<GateInner>>,
    elu_critical: f64,
    cps_bucket_max: f64,
    /// Sampler cadence, re-exported for the runner task that drives `sample()`.
    sampler_interval: std::time::Duration,
    // Lock-free counters — bumped off the EWMA lock on the admission/bypass paths.
    external_admitted: Arc<AtomicU64>,
    rejected_elu: Arc<AtomicU64>,
    rejected_cps: Arc<AtomicU64>,
    internal_bypassed: Arc<AtomicU64>,
    emergency_bypassed: Arc<AtomicU64>,
}

impl EluCpsGate {
    /// Build over a [`LoadSampler`] with the given config. The EWMA starts at `0`
    /// (uninitialised), so the gate admits until the first [`sample`](EluCpsGate::sample)
    /// fires — the TS cold-start contract — and the bucket starts full at
    /// `cps_bucket_size`.
    pub fn new(sampler: Arc<dyn LoadSampler>, config: ProxySelfGateConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(GateInner {
                sampler,
                elu_ewma: Ewma::new(config.elu_smoothing_alpha),
                gc_fraction: 0.0,
                bucket: TokenBucket::new(config.cps_bucket_size, config.cps_bucket_rate),
            })),
            elu_critical: config.elu_critical,
            cps_bucket_max: config.cps_bucket_size as f64,
            sampler_interval: config.sampler_interval,
            external_admitted: Arc::new(AtomicU64::new(0)),
            rejected_elu: Arc::new(AtomicU64::new(0)),
            rejected_cps: Arc::new(AtomicU64::new(0)),
            internal_bypassed: Arc::new(AtomicU64::new(0)),
            emergency_bypassed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Production gate: a [`LiveLoadSampler`] at the configured cadence, default
    /// config. The EWMA only moves once a sampler task drives
    /// [`sample`](EluCpsGate::sample); without one the ELU reads `0` (admit) —
    /// the correct "no signal yet" classification.
    pub fn live() -> Self {
        Self::live_with(ProxySelfGateConfig::default())
    }

    /// Production gate at an explicit config (its [`LiveLoadSampler`] normalises
    /// busy-time against `config.sampler_interval`).
    pub fn live_with(config: ProxySelfGateConfig) -> Self {
        Self::new(Arc::new(LiveLoadSampler::new(config.sampler_interval)), config)
    }

    /// The sampler cadence — the period a runner task should `tick` before each
    /// [`sample`](EluCpsGate::sample) (the TS `setInterval(…, samplerIntervalMs)`).
    pub fn sampler_interval(&self) -> std::time::Duration {
        self.sampler_interval
    }

    /// One sampler tick: read the sampler and feed the ELU EWMA (and stash the raw
    /// GC fraction). Called by the periodic task every
    /// [`sampler_interval`](EluCpsGate::sampler_interval). Port of the TS sampler
    /// `setInterval` body.
    pub fn sample(&self) {
        let mut inner = self.inner.lock().unwrap();
        let elu = inner.sampler.elu();
        let gc = inner.sampler.gc_fraction();
        inner.elu_ewma.observe(elu);
        inner.gc_fraction = gc;
    }

    /// Current EWMA-smoothed proxy ELU (`0..=1`) — exposed for tests/diagnostics.
    pub fn elu_ewma(&self) -> f64 {
        self.inner.lock().unwrap().elu_ewma.get()
    }

    /// Snapshot of the gate's published state. Reading `cps_bucket_level` refills
    /// the bucket as a side effect (lazy refill), which is harmless — it is the
    /// same refill the next admission would do.
    pub fn metrics(&self) -> ProxySelfGateMetrics {
        let (elu_ewma, gc_fraction, cps_bucket_level) = {
            let mut inner = self.inner.lock().unwrap();
            let level = inner.bucket.level();
            (inner.elu_ewma.get(), inner.gc_fraction, level)
        };
        ProxySelfGateMetrics {
            elu_ewma,
            gc_fraction,
            cps_bucket_level,
            cps_bucket_max: self.cps_bucket_max,
            external_admitted_total: self.external_admitted.load(Ordering::Relaxed),
            rejected_elu_total: self.rejected_elu.load(Ordering::Relaxed),
            rejected_cps_total: self.rejected_cps.load(Ordering::Relaxed),
            internal_bypassed_total: self.internal_bypassed.load(Ordering::Relaxed),
            emergency_bypassed_total: self.emergency_bypassed.load(Ordering::Relaxed),
        }
    }
}

impl ProxySelfGate for EluCpsGate {
    /// Try to admit one external new-dialog non-emergency INVITE. Order is
    /// faithful to TS `tryAdmitExternal`:
    /// 1. **ELU** — `elu_ewma > elu_critical` → reject `proxy_overload_elu`,
    ///    `Retry-After: 1` (the bucket is NOT touched).
    /// 2. **CPS bucket** — `try_consume`; on empty → reject `proxy_overload_cps`,
    ///    `Retry-After = bucket.retry_after_sec()`.
    /// 3. Otherwise **admit** (a token was consumed in step 2).
    fn try_admit_external(&self) -> AdmitDecision {
        let mut inner = self.inner.lock().unwrap();

        // 1. Event-loop pressure — hard reject before spending a token.
        if inner.elu_ewma.get() > self.elu_critical {
            drop(inner);
            self.rejected_elu.fetch_add(1, Ordering::Relaxed);
            return AdmitDecision::reject(REASON_ELU, 1);
        }

        // 2. Hard CPS gate.
        if !inner.bucket.try_consume() {
            let retry = inner.bucket.retry_after_sec();
            drop(inner);
            self.rejected_cps.fetch_add(1, Ordering::Relaxed);
            return AdmitDecision::reject(REASON_CPS, retry);
        }

        // 3. Admit (a token has been consumed above).
        drop(inner);
        self.external_admitted.fetch_add(1, Ordering::Relaxed);
        AdmitDecision::admit()
    }

    fn note_bypass(&self, kind: BypassKind) {
        match kind {
            BypassKind::Internal => self.internal_bypassed.fetch_add(1, Ordering::Relaxed),
            BypassKind::Emergency => self.emergency_bypassed.fetch_add(1, Ordering::Relaxed),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // The TS source has NO dedicated `ProxySelfGate.test.ts`; these pin the
    // ported behaviour directly. Where the bucket's time-based refill is
    // exercised the test is `start_paused` and drives `tokio::time::advance`,
    // since the bucket rides `tokio::time::Instant` (CLAUDE.md: behaviour rides
    // `tokio::time` — no separate fake clock to keep in sync).

    #[test]
    fn stub_always_admits() {
        let g = AlwaysAdmitGate;
        assert!(g.try_admit_external().admit);
        g.note_bypass(BypassKind::Emergency); // no-op, must not panic
    }

    /// Build a gate over a simulated sampler whose ELU the returned control sets.
    fn gate(size: u32, rate: u32, elu_critical: f64) -> (EluCpsGate, SimulatedLoadControl) {
        let (sampler, ctl) = simulated();
        let g = EluCpsGate::new(
            Arc::new(sampler),
            ProxySelfGateConfig { cps_bucket_size: size, cps_bucket_rate: rate, elu_critical, ..Default::default() },
        );
        (g, ctl)
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
        ctl.set_elu(5.0);
        assert_eq!(sampler.elu(), 1.0);
        ctl.set_elu(-1.0);
        assert_eq!(sampler.elu(), 0.0);
        ctl.set_elu(f64::NAN);
        assert_eq!(sampler.elu(), 0.0);
    }

    /// The default config matches `defaultProxySelfGateConfig` exactly.
    #[test]
    fn defaults_match_the_ts_source() {
        let c = ProxySelfGateConfig::default();
        assert_eq!(c.elu_critical, 0.8);
        assert_eq!(c.cps_bucket_size, 50);
        assert_eq!(c.cps_bucket_rate, 100);
        assert_eq!(c.elu_smoothing_alpha, 0.2);
        assert_eq!(c.sampler_interval, Duration::from_millis(100));
    }

    /// Before any sample fires the EWMA is `0`, so the gate admits external
    /// traffic up to the bucket capacity even with a high injected ELU (cold
    /// start: the TS `initialized` flag keeps `eluEwma = 0`).
    #[tokio::test(start_paused = true)]
    async fn cold_start_admits_until_the_first_sample() {
        let (g, ctl) = gate(2, 0, 0.8);
        ctl.set_elu(1.0); // would shed, but no sample has fed the EWMA yet
        assert!(g.try_admit_external().admit);
        assert_eq!(g.elu_ewma(), 0.0, "EWMA stays 0 until the first sample()");
    }

    /// ELU above `elu_critical` (once sampled) sheds with `proxy_overload_elu` +
    /// `Retry-After: 1`, and crucially does NOT spend a CPS token (step 1 is
    /// before the bucket). Port of the `eluEwma > eluCritical` branch.
    #[tokio::test(start_paused = true)]
    async fn elu_above_critical_503s_without_spending_a_token() {
        let (g, ctl) = gate(1, 0, 0.8);
        ctl.set_elu(0.9);
        g.sample(); // seat the EWMA at 0.9 (> 0.8)
        let d = g.try_admit_external();
        assert!(!d.admit);
        assert_eq!(d.reason.as_deref(), Some("proxy_overload_elu"));
        assert_eq!(d.retry_after_sec, 1);
        assert_eq!(g.metrics().rejected_elu_total, 1);
        // The lone token was NOT consumed: once ELU recovers, the call admits.
        ctl.set_elu(0.0);
        g.sample();
        g.sample(); // pull the EWMA back below 0.8
        assert!(g.try_admit_external().admit, "the ELU reject must not have spent the token");
    }

    /// At exactly `elu_critical` the gate still admits (`>` is strict, matching TS).
    #[tokio::test(start_paused = true)]
    async fn elu_exactly_at_critical_still_admits() {
        let (g, ctl) = gate(10, 0, 0.8);
        ctl.set_elu(0.8);
        g.sample(); // EWMA seated exactly at the threshold
        assert!(g.try_admit_external().admit, "elu == critical is admit (strict >)");
    }

    /// With ELU calm, the gate admits until the CPS bucket drains, then sheds
    /// with `proxy_overload_cps`. Port of the `!bucket.tryConsume()` branch.
    #[tokio::test(start_paused = true)]
    async fn admits_until_the_cps_bucket_drains_then_503s_cps() {
        // Capacity 2, refill 0/s so the bucket can't top up between consumes.
        let (g, _ctl) = gate(2, 0, 0.8);
        assert!(g.try_admit_external().admit);
        assert!(g.try_admit_external().admit);
        let d = g.try_admit_external();
        assert!(!d.admit);
        assert_eq!(d.reason.as_deref(), Some("proxy_overload_cps"));
        // rate 0 + empty → the TS 60 s Retry-After fallback.
        assert_eq!(d.retry_after_sec, 60);
        assert_eq!(g.metrics().rejected_cps_total, 1);
        assert_eq!(g.metrics().external_admitted_total, 2);
    }

    /// The bucket refills over wall time at `rate_per_sec` (lazy refill on the
    /// next consume), driven by `tokio::time::advance` — no real sleep.
    #[tokio::test(start_paused = true)]
    async fn the_cps_bucket_refills_over_time() {
        let (g, _ctl) = gate(1, 1, 0.8);
        assert!(g.try_admit_external().admit); // drains the lone token
        assert!(!g.try_admit_external().admit); // empty immediately after
        tokio::time::advance(Duration::from_secs(1)).await; // +1 token at 1/s
        assert!(g.try_admit_external().admit, "a refilled token should admit");
    }

    /// `note_bypass` advances the per-kind counters (emergency / internal); the
    /// gate state (bucket, EWMA) is untouched by a bypass.
    #[test]
    fn note_bypass_counts_per_kind() {
        let (g, _ctl) = gate(50, 100, 0.8);
        g.note_bypass(BypassKind::Emergency);
        g.note_bypass(BypassKind::Emergency);
        g.note_bypass(BypassKind::Internal);
        let m = g.metrics();
        assert_eq!(m.emergency_bypassed_total, 2);
        assert_eq!(m.internal_bypassed_total, 1);
        // A bypass spends no token and admits nothing.
        assert_eq!(m.external_admitted_total, 0);
        assert_eq!(m.cps_bucket_level, 50.0);
    }

    /// The metrics snapshot mirrors config + live counters: capacity is the
    /// configured constant, the level floors at 0, and admits/rejects tally.
    #[tokio::test(start_paused = true)]
    async fn metrics_snapshot_reflects_config_and_counters() {
        let (g, _ctl) = gate(3, 0, 0.8);
        assert_eq!(g.metrics().cps_bucket_max, 3.0);
        assert_eq!(g.metrics().cps_bucket_level, 3.0);
        for _ in 0..3 {
            assert!(g.try_admit_external().admit);
        }
        assert!(!g.try_admit_external().admit); // drains then sheds
        let m = g.metrics();
        assert_eq!(m.cps_bucket_level, 0.0, "a drained bucket reads as empty");
        assert_eq!(m.external_admitted_total, 3);
        assert_eq!(m.rejected_cps_total, 1);
    }

    /// The EWMA smooths toward a sustained reading across repeated ticks
    /// (alpha 0.2): the first tick seats it, subsequent ticks pull it toward the
    /// new value but never past it. Port of `eluEwma = α·raw + (1−α)·eluEwma`.
    #[test]
    fn ewma_smooths_across_repeated_samples() {
        let (g, ctl) = gate(50, 100, 0.8);
        ctl.set_elu(1.0);
        g.sample(); // seats at 1.0
        assert!((g.elu_ewma() - 1.0).abs() < 1e-9);
        ctl.set_elu(0.0);
        g.sample(); // 0.2*0 + 0.8*1.0 = 0.8
        assert!((g.elu_ewma() - 0.8).abs() < 1e-9);
        g.sample(); // 0.2*0 + 0.8*0.8 = 0.64
        assert!((g.elu_ewma() - 0.64).abs() < 1e-9);
    }

    /// The live sampler reports a `0..=1` ELU and a structurally-`0` GC fraction;
    /// a late sample (runtime starved past the window) reads busy (> 0).
    #[tokio::test(start_paused = true)]
    async fn live_sampler_reports_clamped_elu_and_zero_gc() {
        let s = LiveLoadSampler::new(Duration::from_millis(100));
        let e0 = s.elu();
        assert!((0.0..=1.0).contains(&e0), "elu {e0} out of [0,1]");
        assert_eq!(s.gc_fraction(), 0.0);
        tokio::time::advance(Duration::from_millis(500)).await;
        let e1 = s.elu();
        assert!((0.0..=1.0).contains(&e1), "elu {e1} out of [0,1]");
        assert!(e1 > 0.0, "a late sample should read busy (> 0), got {e1}");
    }

    /// The end-to-end injected-ELU → running-sampler-task → shed loop. The live
    /// sampler alone reads ~0 under a paused runtime, so (like the b2bua
    /// `overload.rs`) this injects the `simulated()` sampler and spawns the real
    /// 100 ms `sample()` task, then advances the paused clock to drive the
    /// injected ELU through the task into the EWMA and observe the gate flip to
    /// shedding. Pins the seam the runner wires.
    #[tokio::test(start_paused = true)]
    async fn running_sampler_task_drives_the_gate_to_shed_on_injected_elu() {
        let (sampler, ctl) = simulated();
        let g = EluCpsGate::new(
            Arc::new(sampler),
            ProxySelfGateConfig { cps_bucket_size: 50, cps_bucket_rate: 100, elu_critical: 0.8, ..Default::default() },
        );
        // The runner's sampler task, verbatim shape.
        let task = {
            let g = g.clone();
            let period = g.sampler_interval();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(period);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                tick.tick().await; // skip the immediate first tick
                loop {
                    tick.tick().await;
                    g.sample();
                }
            })
        };

        // Calm to start: a fresh external INVITE is admitted.
        assert!(g.try_admit_external().admit);

        // Inject a pegged ELU and let several sampler ticks pull the EWMA above
        // 0.8 (alpha 0.2 needs a few ticks from 0 to cross the threshold).
        ctl.set_elu(1.0);
        for _ in 0..20 {
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await; // let the spawned tick run its sample()
        }
        assert!(g.elu_ewma() > 0.8, "the running task must have driven the EWMA over critical, got {}", g.elu_ewma());

        let d = g.try_admit_external();
        assert!(!d.admit, "a pegged ELU must shed the next external INVITE");
        assert_eq!(d.reason.as_deref(), Some("proxy_overload_elu"));

        task.abort();
    }
}
