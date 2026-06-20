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
/// the impl needs no caller change. **Until it lands, the proxy band classifier
/// (which keys on `elu` only) sees a ~constant placeholder, so its
/// `AboveCritical` exclusion is effectively inert in production** — do not mistake
/// this for a finished ELU implementation.
//
// TODO(migration/08): replace the elapsed-since-last-read busy proxy with
// `tokio::runtime::Handle::current().metrics()` busy-duration accounting once we
// settle on an ELU definition that matches the proxy band thresholds. Tracked as
// an explicit follow-up item in MIGRATION_STATUS.md ("Item 16" attribution note),
// shared with the sip-proxy `self_gate.rs` LiveLoadSampler (same debt); the
// proxy-side ELU-band AIMD it feeds must not be merged on top of this placeholder.
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
// TokenBucket — the Tier-3 hard CPS gate (port of TS `TokenBucket`)
// ---------------------------------------------------------------------------

/// Lazy-refill token bucket. Tokens accrue continuously at `rate_per_sec` up to
/// `capacity`; [`try_consume`](TokenBucket::try_consume) succeeds iff at least
/// one token is available. Port of the TS `TokenBucket` in `OverloadController.ts`.
///
/// **Clock — rides `tokio::time::Instant`, not wall time.** The TS source refills
/// off `Date.now()`; here the elapsed-since-last-refill is measured on
/// `tokio::time::Instant`, which `tokio::time::advance` moves under a paused
/// runtime (CLAUDE.md: behaviour rides `tokio::time` directly — there is no
/// separate fake clock). So a `start_paused` test that advances 1 s sees exactly
/// `rate_per_sec` tokens refill, deterministically, with no real sleeping.
#[derive(Debug)]
struct TokenBucket {
    /// Current token count. May go negative (the emergency `consume_forced` path).
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

    /// Accrue tokens for the time elapsed since the last refill (capped at
    /// `capacity`). A no-op when no time has passed (paused clock between
    /// advances) — exactly the TS `elapsedSec <= 0` guard.
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

    /// Consume one token unconditionally — the level may go negative. The
    /// emergency path uses this so the bucket still reflects true CPS load (a
    /// burst of emergency calls makes subsequent non-emergency callers wait
    /// longer for refill). Port of TS `consumeForced`.
    fn consume_forced(&mut self) {
        self.refill();
        self.tokens -= 1.0;
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

    /// Current level, floored at `0` (the negative emergency overdraft reads as
    /// empty to an observer). Port of TS `level`.
    fn level(&mut self) -> f64 {
        self.refill();
        self.tokens.max(0.0)
    }
}

// ---------------------------------------------------------------------------
// Admission gate — Tier 3 of the overload-protection model (port of `shouldAdmit`)
// ---------------------------------------------------------------------------

/// Why the Tier-3 gate rejected a new INVITE. Port of TS `AdmitReason` (the
/// `"shedder"` variant is omitted — the probabilistic shedder was retired in
/// slice 7, so the worker only ever rejects for `bucket_empty` or `panic_elu`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitReason {
    /// The hard CPS token bucket was empty.
    BucketEmpty,
    /// The worker's own EWMA-ELU exceeded the panic backstop threshold.
    PanicElu,
}

impl AdmitReason {
    /// Short tag for logs/metrics (matches the TS reason strings).
    pub fn as_str(self) -> &'static str {
        match self {
            AdmitReason::BucketEmpty => "bucket_empty",
            AdmitReason::PanicElu => "panic_elu",
        }
    }
}

/// The Tier-3 admission verdict for one new INVITE. Port of TS `AdmitDecision`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdmitDecision {
    /// `true` → proceed (build the call); `false` → send a stateless 503.
    pub admit: bool,
    /// The rejection reason when `!admit`; `None` on an admit.
    pub reason: Option<AdmitReason>,
    /// Suggested `Retry-After` value (seconds) when `!admit` (0 on an admit).
    pub retry_after_sec: u32,
}

impl AdmitDecision {
    /// An admit verdict (no reason, no Retry-After).
    fn admitted() -> Self {
        Self { admit: true, reason: None, retry_after_sec: 0 }
    }
    /// A reject verdict carrying the reason + a Retry-After hint.
    fn rejected(reason: AdmitReason, retry_after_sec: u32) -> Self {
        Self { admit: false, reason: Some(reason), retry_after_sec }
    }
}

/// The admission-gate tunables, copied from [`B2buaConfig`](crate::config::B2buaConfig)
/// when the signal is configured. The token bucket capacity/rate are fixed at
/// configure time (as in the TS constructor); the panic threshold + Retry-After
/// base are read live on each [`should_admit`](OverloadSignal::should_admit).
#[derive(Debug, Clone, Copy)]
struct AdmissionConfig {
    panic_elu_threshold: f64,
    retry_after_base_sec: u32,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        // Mirror `B2buaConfig::default()` so a signal that was never explicitly
        // configured still gates with the TS defaults rather than admitting blind.
        Self {
            panic_elu_threshold: DEFAULT_PANIC_ELU_THRESHOLD,
            retry_after_base_sec: DEFAULT_RETRY_AFTER_BASE_SEC,
        }
    }
}

/// Admission-gate seed defaults — kept in lock-step with the `B2buaConfig`
/// defaults (`b2bua-sdk`), so a publish-only `OverloadSignal::new`/`live` built
/// without config still gates with the TS defaults. `configure_admission`
/// overwrites these with the operator's settings at ctx build.
const DEFAULT_CPS_BUCKET_SIZE: u32 = 1000;
const DEFAULT_CPS_BUCKET_RATE: u32 = 500;
const DEFAULT_PANIC_ELU_THRESHOLD: f64 = 0.75;
const DEFAULT_RETRY_AFTER_BASE_SEC: u32 = 5;

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
    /// Tier-3 rejects because the hard CPS token bucket was empty (migration/09;
    /// the `rejectTotal.bucket_empty` subset of the TS metrics).
    pub reject_bucket_empty_total: u64,
    /// Tier-3 rejects because the worker's EWMA-ELU exceeded the panic backstop
    /// (`rejectTotal.panic_elu`).
    pub reject_panic_elu_total: u64,
    /// Current CPS token-bucket level, floored at 0 (port of the TS
    /// `tokenBucketLevel` gauge). A negative emergency overdraft reads as 0.
    pub token_bucket_level: f64,
    /// Monotonic count of EMERGENCY new-dialog INVITEs this worker admitted
    /// (Resource-Priority esnet/wps/q735 or an admitted `;emerg`/`;em` marker).
    /// These ALWAYS admit (bypassing the bucket-empty + panic-ELU checks, only
    /// `consume_forced`-ing a token) and are NOT counted on `adm`/the
    /// non-emergency total — so without this counter the emergency-admit branch
    /// was entirely uncounted. The sum `non_emergency_admitted_total +
    /// emergency_admitted_total` is the worker's total admit rate.
    pub emergency_admitted_total: u64,
}

struct OverloadInner {
    sampler: Arc<dyn LoadSampler>,
    elu_ewma: Ewma,
    gc_fraction_ewma: Ewma,
    /// Tier-3 hard CPS gate (migration/09). Seeded with the `B2buaConfig`
    /// defaults; reconfigured to the operator's values by
    /// [`configure_admission`](OverloadSignal::configure_admission) at ctx build.
    bucket: TokenBucket,
    /// Live-read admission knobs (panic-ELU threshold + Retry-After base).
    admission: AdmissionConfig,
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
    /// Tier-3 reject tallies (migration/09) — the subset of the TS
    /// `OverloadControllerMetrics.rejectTotal` this worker can produce (the
    /// `shedder` bucket is gone with the retired shedder). Lock-free so the
    /// admission gate bumps them without the EWMA lock.
    reject_bucket_empty: Arc<AtomicU64>,
    reject_panic_elu: Arc<AtomicU64>,
    /// Emergency new-dialog INVITEs admitted (the `is_emergency` true path of
    /// [`should_admit`](OverloadSignal::should_admit)). Bumped by the router on
    /// the emergency-admit branch, sibling to `increment_non_emergency_admitted`.
    /// Lock-free so the admit path never takes the EWMA lock for it.
    emergency_admitted: Arc<AtomicU64>,
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
                bucket: TokenBucket::new(DEFAULT_CPS_BUCKET_SIZE, DEFAULT_CPS_BUCKET_RATE),
                admission: AdmissionConfig::default(),
            })),
            non_emergency_admitted: Arc::new(AtomicU64::new(0)),
            reject_bucket_empty: Arc::new(AtomicU64::new(0)),
            reject_panic_elu: Arc::new(AtomicU64::new(0)),
            emergency_admitted: Arc::new(AtomicU64::new(0)),
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

    /// Increment the monotonic counter of EMERGENCY new-dialog INVITEs admitted
    /// by this worker. Bumped on the emergency-admit branch of the router (the
    /// `is_emergency` true path), sibling to
    /// [`increment_non_emergency_admitted`](OverloadSignal::increment_non_emergency_admitted).
    /// Emergency admits are NOT published on `adm` (the LB caps non-emergency
    /// traffic only) — this counter is the only visibility into emergency-admit
    /// volume, which would otherwise be uncounted.
    pub fn increment_emergency_admitted(&self) {
        self.emergency_admitted.fetch_add(1, Ordering::Relaxed);
    }

    /// Configure the Tier-3 admission gate from the worker's [`B2buaConfig`].
    /// Called once by `b2bua_core` at ctx build, after the config is final (the
    /// harness `tune` seam has run), so the bucket capacity/rate and the live
    /// panic-ELU / Retry-After knobs reflect the operator's settings — the TS
    /// equivalent of `new TokenBucket(config.cpsBucketSize, config.cpsBucketRate)`
    /// plus the live `config.overloadPanicEluThreshold` / `config.retryAfterBaseSec`
    /// reads in `shouldAdmit`.
    ///
    /// Replaces the bucket wholesale (resetting it to full at the configured
    /// capacity): it is a boot-time call before any admission decision, so there
    /// is no in-flight token state to preserve.
    pub fn configure_admission(&self, cfg: &crate::config::B2buaConfig) {
        let mut inner = self.inner.lock().unwrap();
        inner.bucket = TokenBucket::new(cfg.cps_bucket_size, cfg.cps_bucket_rate);
        inner.admission = AdmissionConfig {
            panic_elu_threshold: cfg.overload_panic_elu_threshold,
            retry_after_base_sec: cfg.retry_after_base_sec,
        };
    }

    /// Decide whether to admit a new INVITE — Tier 3 of the overload model (port
    /// of `OverloadController.shouldAdmit`). The caller MUST pass `is_emergency`
    /// for an emergency Resource-Priority request (`sip_message::is_emergency_request`).
    ///
    /// Order (faithful to TS):
    /// 1. **Emergency** → always admit, but `consume_forced` one token (the level
    ///    may go negative) so the bucket reflects true CPS load. Emergency callers
    ///    never see a reject and are NOT counted on `adm` (the caller must skip
    ///    [`increment_non_emergency_admitted`](OverloadSignal::increment_non_emergency_admitted)
    ///    for them — LBs cap non-emergency traffic only).
    /// 2. **Hard CPS gate** — `try_consume`; on empty → reject `bucket_empty` with
    ///    `Retry-After = bucket.retry_after_sec()`.
    /// 3. **Panic-ELU backstop** — only after a token was consumed: if the
    ///    EWMA-ELU exceeds the configured threshold → reject `panic_elu` with
    ///    `Retry-After = retry_after_base_sec`. The LB-side AIMD is the primary
    ///    loop; this catches an absent/misconfigured/overloaded LB.
    /// 4. Otherwise **admit**.
    ///
    /// Note the token is consumed for an admit AND for a `panic_elu` reject (the
    /// token is spent in step 2 before step 3 runs) — exactly as in the TS source.
    pub fn should_admit(&self, is_emergency: bool) -> AdmitDecision {
        let mut inner = self.inner.lock().unwrap();

        if is_emergency {
            // Always admit; still consume so the bucket tracks true CPS load.
            inner.bucket.consume_forced();
            return AdmitDecision::admitted();
        }

        // Hard CPS gate.
        if !inner.bucket.try_consume() {
            let retry = inner.bucket.retry_after_sec();
            drop(inner);
            self.reject_bucket_empty.fetch_add(1, Ordering::Relaxed);
            return AdmitDecision::rejected(AdmitReason::BucketEmpty, retry);
        }

        // Panic-ELU backstop (a token has already been consumed above).
        let elu = inner.elu_ewma.get();
        if elu > inner.admission.panic_elu_threshold {
            let retry = inner.admission.retry_after_base_sec;
            drop(inner);
            self.reject_panic_elu.fetch_add(1, Ordering::Relaxed);
            return AdmitDecision::rejected(AdmitReason::PanicElu, retry);
        }

        AdmitDecision::admitted()
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

    /// Snapshot of the published EWMAs + the `adm` counter + the Tier-3 gate
    /// tallies/level (for `/status`). Reading `token_bucket_level` refills the
    /// bucket as a side effect (lazy refill), which is harmless — it is the same
    /// refill the next `should_admit` would do.
    pub fn metrics(&self) -> OverloadMetrics {
        let (elu_ewma, gc_fraction_ewma, token_bucket_level) = {
            let mut inner = self.inner.lock().unwrap();
            let level = inner.bucket.level();
            (inner.elu_ewma.get(), inner.gc_fraction_ewma.get(), level)
        };
        OverloadMetrics {
            elu_ewma,
            gc_fraction_ewma,
            non_emergency_admitted_total: self.non_emergency_admitted.load(Ordering::Relaxed),
            reject_bucket_empty_total: self.reject_bucket_empty.load(Ordering::Relaxed),
            reject_panic_elu_total: self.reject_panic_elu.load(Ordering::Relaxed),
            token_bucket_level,
            emergency_admitted_total: self.emergency_admitted.load(Ordering::Relaxed),
        }
    }

    /// Render the worker-side overload decision INPUTS + DECISIONS as Prometheus
    /// text exposition (the migrated subset of the TS `OverloadController`
    /// metrics — Tier-3 admission gate + the X-Overload signal). Appended to the
    /// `/metrics` body by the runner (mirrors how the proxy-self gate's
    /// `self_gate_prometheus_text` is appended). Only the migrated
    /// inputs/decisions are emitted — the un-ported serving/draining OPTIONS
    /// matrix, the retired shedder, GC/loop-lag p95 and routing-API latency are
    /// intentionally absent.
    ///
    /// Series (all `b2bua_overload_*` / `b2bua_emergency_*`):
    ///   - `b2bua_overload_admit_total` (counter) — total new-dialog INVITEs the
    ///     gate admitted = non-emergency `adm` + emergency.
    ///   - `b2bua_overload_reject_total{reason}` (counter) — `bucket_empty` +
    ///     `panic_elu` split (the TS `rejectTotal` shape, minus the dead
    ///     `shedder` bucket).
    ///   - `b2bua_overload_non_emergency_admitted_total` (counter) — the `adm`
    ///     published on X-Overload (kept as its own series; dashboards key on it).
    ///   - `b2bua_emergency_admitted_total` (counter) — emergency-admit volume.
    ///   - `b2bua_overload_token_bucket_level` (gauge) — current CPS bucket level.
    ///   - `b2bua_overload_elu_ewma` / `b2bua_overload_gc_fraction` (gauges) —
    ///     the decision INPUTS (the EWMAs published on X-Overload).
    pub fn prometheus_text(&self) -> String {
        let m = self.metrics();
        let admit_total = m.non_emergency_admitted_total + m.emergency_admitted_total;
        let mut s = String::with_capacity(1024);
        let counter = |s: &mut String, name: &str, help: &str, v: u64| {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"));
        };
        let gauge = |s: &mut String, name: &str, help: &str, v: f64| {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}\n"));
        };
        counter(
            &mut s,
            "b2bua_overload_admit_total",
            "New-dialog INVITEs admitted by the Tier-3 overload gate (non-emergency adm + emergency).",
            admit_total,
        );
        // Per-reason reject split (the TS rejectTotal shape; the retired shedder
        // bucket is absent). Pairs with the aggregate b2bua_overload_rejected_total.
        s.push_str("# HELP b2bua_overload_reject_total New-dialog INVITEs shed by the Tier-3 overload gate, by reason.\n");
        s.push_str("# TYPE b2bua_overload_reject_total counter\n");
        s.push_str(&format!(
            "b2bua_overload_reject_total{{reason=\"bucket_empty\"}} {}\n",
            m.reject_bucket_empty_total
        ));
        s.push_str(&format!(
            "b2bua_overload_reject_total{{reason=\"panic_elu\"}} {}\n",
            m.reject_panic_elu_total
        ));
        counter(
            &mut s,
            "b2bua_overload_non_emergency_admitted_total",
            "Non-emergency new-dialog INVITEs admitted (the `adm` counter published on X-Overload).",
            m.non_emergency_admitted_total,
        );
        counter(
            &mut s,
            "b2bua_emergency_admitted_total",
            "Emergency new-dialog INVITEs admitted (always admitted, bypassing the bucket-empty + panic-ELU checks; NOT counted on `adm`).",
            m.emergency_admitted_total,
        );
        gauge(
            &mut s,
            "b2bua_overload_token_bucket_level",
            "Current CPS token-bucket level (tokens remaining; a negative emergency overdraft reads as 0).",
            m.token_bucket_level,
        );
        gauge(
            &mut s,
            "b2bua_overload_elu_ewma",
            "Decision INPUT: EWMA-smoothed Event Loop Utilization (0..1) published on X-Overload; the panic-ELU backstop fires above the threshold.",
            m.elu_ewma,
        );
        gauge(
            &mut s,
            "b2bua_overload_gc_fraction",
            "Decision INPUT: EWMA-smoothed GC pause fraction (0..1) published on X-Overload (structurally 0 on Rust — no managed GC).",
            m.gc_fraction_ewma,
        );
        s
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
    /// so we drive it directly — no real clock, no paused-clock wait. This pins
    /// the `sample()`-to-EWMA half in isolation; the *full*
    /// injected-value → running-100ms-task → published-header loop (the seam the
    /// live sampler alone cannot exercise, since its busy proxy reads ~0 under a
    /// paused runtime) is closed end-to-end by the `start_paused` harness test
    /// `injected_sampler_drives_published_elu_through_the_running_task`
    /// (`b2bua-harness/tests/x_overload_signal.rs`), which injects this exact
    /// `simulated()` sampler into a running `B2buaCore` via the
    /// `spawn_with_overload` seam.
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

    // ── Tier-3 admission gate (migration/09) ──────────────────────────────────
    //
    // The TS `OverloadController.shouldAdmit` / `TokenBucket` have no dedicated
    // unit test in the source (the migration item's TS-test list is empty); these
    // pin the ported behaviour directly. Where the bucket's time-based refill is
    // exercised the test is `start_paused` and drives `tokio::time::advance`, since
    // the Rust bucket rides `tokio::time::Instant` (CLAUDE.md: behaviour rides
    // `tokio::time` — no separate fake clock to keep in sync).

    /// Build a signal whose admission gate uses the given bucket/threshold knobs,
    /// over a simulated sampler whose ELU the returned control sets. The EWMA is
    /// seated by one `sample()` so `should_admit`'s panic-ELU read sees a real value.
    fn admission_sig(
        size: u32,
        rate: u32,
        panic_elu: f64,
    ) -> (OverloadSignal, SimulatedLoadControl) {
        let (sampler, ctl) = simulated();
        let sig = OverloadSignal::new(Arc::new(sampler));
        sig.configure_admission(&crate::config::B2buaConfig {
            cps_bucket_size: size,
            cps_bucket_rate: rate,
            overload_panic_elu_threshold: panic_elu,
            retry_after_base_sec: 5,
            ..Default::default()
        });
        (sig, ctl)
    }

    /// `configure_admission` seeds a full bucket at the configured capacity and a
    /// non-emergency INVITE is admitted while tokens remain (`bucket_empty` only
    /// once drained). The TS constructor path: `new TokenBucket(size, rate)`.
    #[tokio::test(start_paused = true)]
    async fn admits_until_the_cps_bucket_is_drained_then_503s_bucket_empty() {
        // Capacity 2, refill 0/s so the bucket can't top up between consumes.
        let (sig, _ctl) = admission_sig(2, 0, 1.0);
        // Two admits drain the burst capacity…
        assert_eq!(sig.should_admit(false), AdmitDecision::admitted());
        assert_eq!(sig.should_admit(false), AdmitDecision::admitted());
        // …the third finds the bucket empty → reject `bucket_empty`.
        let d = sig.should_admit(false);
        assert!(!d.admit);
        assert_eq!(d.reason, Some(AdmitReason::BucketEmpty));
        // rate 0 + empty → the TS 60 s Retry-After fallback.
        assert_eq!(d.retry_after_sec, 60);
        // The reject is tallied; no admit was counted for it.
        assert_eq!(sig.metrics().reject_bucket_empty_total, 1);
    }

    /// The bucket refills over wall time at `rate_per_sec` (lazy refill on the next
    /// consume). Drained at capacity 1 / rate 1/s, a consume one second later
    /// succeeds again — driven by `tokio::time::advance`, no real sleep.
    #[tokio::test(start_paused = true)]
    async fn the_bucket_refills_over_time() {
        let (sig, _ctl) = admission_sig(1, 1, 1.0);
        assert!(sig.should_admit(false).admit); // drains the lone token
        assert!(!sig.should_admit(false).admit); // empty immediately after
        // One second of refill at 1 token/s restores exactly one token.
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(sig.should_admit(false).admit, "a refilled token should admit");
    }

    /// Emergency callers ALWAYS admit and are NEVER counted on `adm`, but still
    /// consume a token — so the bucket can go negative and a subsequent
    /// non-emergency caller is shed. Port of the TS emergency `consumeForced` path.
    #[tokio::test(start_paused = true)]
    async fn emergency_always_admits_consumes_and_can_overdraft_the_bucket() {
        // Capacity 1, no refill: a single non-emergency token exists.
        let (sig, _ctl) = admission_sig(1, 0, 1.0);
        // Two emergency admits both succeed (the 2nd drives the level negative).
        assert_eq!(sig.should_admit(true), AdmitDecision::admitted());
        assert_eq!(sig.should_admit(true), AdmitDecision::admitted());
        // Emergency admits are NOT counted on `adm` (the caller skips the bump;
        // `should_admit` itself never touches the counter).
        assert_eq!(sig.metrics().non_emergency_admitted_total, 0);
        // The overdraft means the next NON-emergency caller finds the bucket empty.
        let d = sig.should_admit(false);
        assert!(!d.admit);
        assert_eq!(d.reason, Some(AdmitReason::BucketEmpty));
    }

    /// Once a token is consumed, an EWMA-ELU above the panic threshold sheds the
    /// (non-emergency) call with `panic_elu` + the configured Retry-After base.
    /// Port of the slice-7 panic-ELU backstop in `shouldAdmit`.
    #[tokio::test(start_paused = true)]
    async fn panic_elu_backstop_503s_after_the_token_is_consumed() {
        // Roomy bucket so the gate never trips on `bucket_empty`; panic at 0.75.
        let (sig, ctl) = admission_sig(1000, 0, 0.75);
        ctl.set_elu(0.9);
        sig.sample(); // seat the EWMA at 0.9 (> 0.75)
        let d = sig.should_admit(false);
        assert!(!d.admit);
        assert_eq!(d.reason, Some(AdmitReason::PanicElu));
        assert_eq!(d.retry_after_sec, 5); // retry_after_base_sec
        assert_eq!(sig.metrics().reject_panic_elu_total, 1);
        // A token WAS consumed before the panic check (TS spends it in step 2).
        // With ELU back below the threshold the next call admits normally.
        ctl.set_elu(0.0);
        sig.sample();
        sig.sample(); // pull the EWMA below 0.75
        assert!(sig.should_admit(false).admit);
    }

    /// The panic-ELU backstop is a NON-emergency control: an emergency caller is
    /// admitted even when the worker's ELU is pegged (it bypasses both the bucket
    /// gate's empty-check and the panic check — it only `consume_forced`s).
    #[tokio::test(start_paused = true)]
    async fn panic_elu_never_sheds_an_emergency_call() {
        let (sig, ctl) = admission_sig(1000, 0, 0.75);
        ctl.set_elu(1.0);
        sig.sample(); // EWMA pegged at 1.0
        assert_eq!(sig.should_admit(true), AdmitDecision::admitted());
        assert_eq!(sig.metrics().reject_panic_elu_total, 0);
    }

    /// `configure_admission` is what makes the operator's `B2buaConfig` knobs take
    /// effect: a capacity-0 bucket sheds the very first non-emergency INVITE.
    #[tokio::test(start_paused = true)]
    async fn configure_admission_applies_the_config_capacity() {
        let (sig, _ctl) = admission_sig(0, 0, 1.0);
        let d = sig.should_admit(false);
        assert!(!d.admit, "a zero-capacity bucket admits nothing");
        assert_eq!(d.reason, Some(AdmitReason::BucketEmpty));
    }

    /// The `AdmitReason` tags match the TS reason strings (logged/metric-keyed).
    #[test]
    fn admit_reason_tags_match_the_ts_strings() {
        assert_eq!(AdmitReason::BucketEmpty.as_str(), "bucket_empty");
        assert_eq!(AdmitReason::PanicElu.as_str(), "panic_elu");
    }

    /// `increment_emergency_admitted` advances the emergency-admit counter
    /// (sibling to the non-emergency `adm`), and it is NOT counted on `adm`.
    #[test]
    fn increment_emergency_admitted_advances_its_own_counter() {
        let (sampler, _ctl) = simulated();
        let sig = OverloadSignal::new(Arc::new(sampler));
        sig.increment_emergency_admitted();
        sig.increment_emergency_admitted();
        let m = sig.metrics();
        assert_eq!(m.emergency_admitted_total, 2);
        // Emergency admits never touch the non-emergency `adm` counter.
        assert_eq!(m.non_emergency_admitted_total, 0);
    }

    /// The Prometheus render carries every migrated overload INPUT + DECISION
    /// series with the right name/type after the counters/gauges advance.
    #[tokio::test(start_paused = true)]
    async fn prometheus_text_renders_inputs_and_decisions() {
        let (sig, ctl) = admission_sig(1, 0, 0.75);
        ctl.set_elu(0.42);
        ctl.set_gc_fraction(0.0);
        sig.sample(); // seat the ELU EWMA at 0.42
        // One non-emergency admit (drains the lone token), one emergency admit
        // (consume_forced → overdraft), then a non-emergency reject (bucket empty).
        assert!(sig.should_admit(false).admit);
        sig.increment_non_emergency_admitted();
        assert!(sig.should_admit(true).admit);
        sig.increment_emergency_admitted();
        assert!(!sig.should_admit(false).admit); // bucket_empty reject

        let txt = sig.prometheus_text();
        // admit_total = non_emergency(1) + emergency(1) = 2.
        assert!(txt.contains("b2bua_overload_admit_total 2"), "{txt}");
        assert!(txt.contains("# TYPE b2bua_overload_admit_total counter"));
        assert!(txt.contains("b2bua_overload_reject_total{reason=\"bucket_empty\"} 1"));
        assert!(txt.contains("b2bua_overload_reject_total{reason=\"panic_elu\"} 0"));
        assert!(txt.contains("b2bua_overload_non_emergency_admitted_total 1"));
        assert!(txt.contains("b2bua_emergency_admitted_total 1"));
        assert!(txt.contains("# TYPE b2bua_emergency_admitted_total counter"));
        assert!(txt.contains("# TYPE b2bua_overload_token_bucket_level gauge"));
        assert!(txt.contains("b2bua_overload_elu_ewma 0.42"));
        assert!(txt.contains("# TYPE b2bua_overload_elu_ewma gauge"));
        assert!(txt.contains("b2bua_overload_gc_fraction 0"));
    }

    /// `level()` floors the (possibly negative) overdraft at 0 for an observer,
    /// and reads the configured capacity on a fresh bucket. Port of TS `level`.
    #[tokio::test(start_paused = true)]
    async fn token_bucket_level_floors_at_zero() {
        let mut b = TokenBucket::new(3, 0);
        assert_eq!(b.level(), 3.0);
        b.consume_forced();
        b.consume_forced();
        b.consume_forced();
        b.consume_forced(); // -1 internally
        assert_eq!(b.level(), 0.0, "a negative overdraft reads as empty");
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
