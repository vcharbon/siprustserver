//! [`ProxySelfGate`] — proxy self-overload admission gate.
//!
//! The front proxy is a single task with its own event-loop budget. Under flood
//! the proxy itself saturates before any worker sees the traffic. This gate
//! sheds **external, new-dialog, non-emergency** INVITEs on two cheap binary
//! checks:
//!
//!   - smoothed intake age `> elu_critical` — hard rejection by intake
//!     pressure (`proxy_overload_intake_age`): the reading is packet age at
//!     dequeue, normalized by [`IntakeAgeSampler`]. The `elu_*` names of the
//!     config, the EWMA and its gauge are the [`LoadSampler`] seam's.
//!   - a per-class CPS token bucket — hard cap on the external new-dialog
//!     non-emergency rate (`proxy_overload_cps`).
//!
//! Caller classification (emergency / in-dialog / worker-originated) happens in
//! `crate::core::request` *before* this gate; internal traffic from workers
//! always bypasses (rejecting a worker's B-leg INVITE causes re-routing churn
//! rather than load shedding).
//!
//! Unlike the per-worker AIMD on the LB-side [`crate::load_observer`], this gate
//! is binary: the proxy's ELU is its own, with no second party to converge with.
//!
//! The token bucket, EWMA and [`LoadSampler`] seam are the [`load_shed`]
//! primitives the worker sheds with too; `sip-proxy` does not depend on
//! `b2bua`.
//!
//! ## Clock — rides `tokio::time`
//!
//! The [`TokenBucket`] refills on a `tokio::time::Instant` timeline and the
//! EWMA is fed by an explicit [`EluCpsGate::sample`] tick driven by a
//! `tokio::time::interval` task in the runner — so a `start_paused` test
//! advances both with `tokio::time::advance` like every other behaviour timer
//! (CLAUDE.md: behaviour rides `tokio::time` directly — there is no separate
//! fake clock to keep in sync).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use load_shed::{clamp01, Ewma, LoadSampler, TokenBucket};

use crate::poison::{locked, SELF_GATE};

// ---------------------------------------------------------------------------
// Decision / bypass types
// ---------------------------------------------------------------------------

/// The outcome of an admission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitDecision {
    Admit,
    /// Rejected for `reason`, which names the reject in counters and traces, with a
    /// `Retry-After` hint the wire floors at 1 s.
    Reject {
        reason: ShedReason,
        retry_after_sec: u32,
    },
}

impl AdmitDecision {
    /// Whether the request is admitted.
    pub fn is_admit(&self) -> bool {
        matches!(self, AdmitDecision::Admit)
    }
}

/// Why the gate rejected: the reject's metric label and trace reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShedReason {
    /// The smoothed intake reading (packet age at dequeue, normalized by
    /// [`AGE_CRITICAL_MS`]) is past `elu_critical`.
    IntakeAge,
    /// The CPS bucket is empty.
    Cps,
}

impl ShedReason {
    /// Every reason, in declaration order.
    pub const ALL: [ShedReason; 2] = [ShedReason::IntakeAge, ShedReason::Cps];

    /// The metric label and trace reason.
    pub const fn label(self) -> &'static str {
        match self {
            ShedReason::IntakeAge => "proxy_overload_intake_age",
            ShedReason::Cps => "proxy_overload_cps",
        }
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
        AdmitDecision::Admit
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Tunables for [`EluCpsGate`].
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
    /// [`EluCpsGate::sample`].
    pub sampler_interval: std::time::Duration,
}

impl Default for ProxySelfGateConfig {
    /// Aggressive defaults (calibrated against a perf test). `cps_bucket_size = 50`,
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
// IntakeAgeSampler — the production LoadSampler
// ---------------------------------------------------------------------------

/// Dequeue age (ms) at which [`IntakeAgeSampler::elu`] reads `1.0`. Past ~500 ms
/// of queueing the caller's Timer A (T1 = 500 ms) has already fired, so the
/// retransmit storm is compounding the overload — full shed pressure.
pub const AGE_CRITICAL_MS: u64 = 500;

/// Shared max-dequeue-age cell: every `ProxyCore` recv shard (both faces) folds
/// the age of each packet it dequeues in via [`record`](Self::record); the
/// [`IntakeAgeSampler`] drains the max per sample window. ONE instance across
/// all shards, so the window max is the whole-intake saturation signal.
/// Clone-cheap (shares one atomic).
#[derive(Clone, Debug, Default)]
pub struct IntakeAgeRecorder {
    max_age_ms: Arc<AtomicU64>,
}

impl IntakeAgeRecorder {
    /// Fold one packet's dequeue age into the window max (one `fetch_max`).
    pub fn record(&self, age_ms: u64) {
        self.max_age_ms.fetch_max(age_ms, Ordering::Relaxed);
    }

    /// Take the window max and reset it to `0` (the per-window drain).
    pub fn drain_max(&self) -> u64 {
        self.max_age_ms.swap(0, Ordering::Relaxed)
    }
}

/// Production [`LoadSampler`]: intake saturation measured as the **age of
/// packets at dequeue** (`Clock::now_ms − UdpPacket::arrival_ms`, recorded by
/// every recv shard into one shared [`IntakeAgeRecorder`]). Each `elu()` read
/// drains the window max and normalizes it against [`AGE_CRITICAL_MS`], so a
/// saturated serial recv shard reads busy even with idle runtime workers.
pub struct IntakeAgeSampler {
    recorder: IntakeAgeRecorder,
}

impl IntakeAgeSampler {
    /// Build over the recorder the recv shards feed.
    pub fn new(recorder: IntakeAgeRecorder) -> Self {
        Self { recorder }
    }
}

impl LoadSampler for IntakeAgeSampler {
    fn elu(&self) -> f64 {
        clamp01(self.recorder.drain_max() as f64 / AGE_CRITICAL_MS as f64)
    }

    fn gc_fraction(&self) -> f64 {
        // Rust has no managed stop-the-world GC; structurally 0 (not a stub).
        0.0
    }
}

// ---------------------------------------------------------------------------
// EluCpsGate — the real proxy-self admission gate
// ---------------------------------------------------------------------------

/// The intake-shed episode log, keyed by rejection reason (ADR-0026): shedding
/// is a per-call event class, so it is aggregated — one rising line, a ~5 s
/// summary carrying the shed count, one falling line with the totals.
fn shed_waves() -> Arc<observe::WaveSet> {
    observe::WaveSet::new(|reason: &str, r: &observe::WaveReport| {
        tracing::info!(
            node = observe::node(),
            reason,
            edge = %r.edge,
            elapsed_ms = r.elapsed_ms,
            totals = %r.tally,
            "proxy intake shed"
        );
    })
}

/// Snapshot of the gate's published state, for `/status` + Prometheus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProxySelfGateMetrics {
    /// EWMA-smoothed proxy ELU (`0..=1`) — crosses `elu_critical` → 503.
    pub elu_ewma: f64,
    /// Most recent raw GC fraction (informational only).
    pub gc_fraction: f64,
    /// Token-bucket fill level (tokens remaining, floored at 0).
    pub cps_bucket_level: f64,
    /// Configured bucket capacity (constant per config; a gauge for ratios).
    pub cps_bucket_max: f64,
    /// External new-dialog non-emergency INVITEs admitted by the gate.
    pub external_admitted_total: u64,
    /// Rejections by intake age (`proxy_overload_intake_age`).
    pub rejected_intake_age_total: u64,
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
    /// The origin of the bucket's timeline.
    epoch: tokio::time::Instant,
}

impl GateInner {
    /// Now, on the bucket's timeline.
    fn now(&self) -> Duration {
        tokio::time::Instant::now().saturating_duration_since(self.epoch)
    }
}

/// The real ELU/CPS proxy-self gate. Clone-cheap
/// (shares one `Arc`); construct one with [`EluCpsGate::new`]/[`EluCpsGate::intake`],
/// hand it to `ProxyCoreBuilder::self_gate`, and drive [`sample`](EluCpsGate::sample)
/// from a periodic task at [`ProxySelfGateConfig::sampler_interval`].
#[derive(Clone)]
pub struct EluCpsGate {
    inner: Arc<Mutex<GateInner>>,
    elu_critical: f64,
    /// Sampler cadence, re-exported for the runner task that drives `sample()`.
    sampler_interval: std::time::Duration,
    /// Intake-shed aggregation keyed by rejection reason (ADR-0026): a shedding
    /// window is ONE episode — shed-on line, ~5 s summaries with the shed
    /// count, shed-off line with the totals — ended once admits have run clean
    /// for the idle window, so a bucket at its cap does not print per call.
    shed: Arc<observe::WaveSet>,
    // Lock-free counters — bumped off the EWMA lock on the admission/bypass paths.
    external_admitted: Arc<AtomicU64>,
    rejected_intake_age: Arc<AtomicU64>,
    rejected_cps: Arc<AtomicU64>,
    internal_bypassed: Arc<AtomicU64>,
    emergency_bypassed: Arc<AtomicU64>,
}

impl EluCpsGate {
    /// Build over a [`LoadSampler`] with the given config. The EWMA starts at `0`
    /// (uninitialised), so the gate admits until the first [`sample`](EluCpsGate::sample)
    /// fires, and the bucket starts full at
    /// `cps_bucket_size`.
    pub fn new(sampler: Arc<dyn LoadSampler>, config: ProxySelfGateConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(GateInner {
                sampler,
                elu_ewma: Ewma::new(config.elu_smoothing_alpha),
                gc_fraction: 0.0,
                bucket: TokenBucket::full(
                    f64::from(config.cps_bucket_size),
                    f64::from(config.cps_bucket_rate),
                    Duration::ZERO,
                ),
                epoch: tokio::time::Instant::now(),
            })),
            elu_critical: config.elu_critical,
            sampler_interval: config.sampler_interval,
            shed: shed_waves(),
            external_admitted: Arc::new(AtomicU64::new(0)),
            rejected_intake_age: Arc::new(AtomicU64::new(0)),
            rejected_cps: Arc::new(AtomicU64::new(0)),
            internal_bypassed: Arc::new(AtomicU64::new(0)),
            emergency_bypassed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Production gate: an [`IntakeAgeSampler`] over `recorder` — the shared
    /// cell every recv shard's dequeue path feeds. The EWMA only moves once a
    /// sampler task drives [`sample`](EluCpsGate::sample); without one the ELU
    /// reads `0` (admit) — the correct "no signal yet" classification.
    pub fn intake(recorder: IntakeAgeRecorder, config: ProxySelfGateConfig) -> Self {
        Self::new(Arc::new(IntakeAgeSampler::new(recorder)), config)
    }

    /// The sampler cadence — the period a runner task should `tick` before each
    /// [`sample`](EluCpsGate::sample).
    pub fn sampler_interval(&self) -> std::time::Duration {
        self.sampler_interval
    }

    /// One sampler tick: read the sampler and feed the ELU EWMA (and stash the raw
    /// GC fraction). Called by the periodic task every
    /// [`sampler_interval`](EluCpsGate::sampler_interval).
    pub fn sample(&self) {
        let mut inner = locked(&self.inner, SELF_GATE);
        let elu = inner.sampler.elu();
        let gc = inner.sampler.gc_fraction();
        inner.elu_ewma.observe(elu);
        inner.gc_fraction = gc;
    }

    /// Current EWMA-smoothed proxy ELU (`0..=1`) — exposed for tests/diagnostics.
    pub fn elu_ewma(&self) -> f64 {
        locked(&self.inner, SELF_GATE).elu_ewma.get()
    }

    /// Snapshot of the gate's published state. A read changes nothing:
    /// `cps_bucket_level` is the level projected to now.
    pub fn metrics(&self) -> ProxySelfGateMetrics {
        let (elu_ewma, gc_fraction, cps_bucket_level, cps_bucket_max) = {
            let inner = locked(&self.inner, SELF_GATE);
            let level = inner.bucket.projected_level(inner.now());
            (inner.elu_ewma.get(), inner.gc_fraction, level, inner.bucket.capacity())
        };
        ProxySelfGateMetrics {
            elu_ewma,
            gc_fraction,
            cps_bucket_level,
            cps_bucket_max,
            external_admitted_total: self.external_admitted.load(Ordering::Relaxed),
            rejected_intake_age_total: self.rejected_intake_age.load(Ordering::Relaxed),
            rejected_cps_total: self.rejected_cps.load(Ordering::Relaxed),
            internal_bypassed_total: self.internal_bypassed.load(Ordering::Relaxed),
            emergency_bypassed_total: self.emergency_bypassed.load(Ordering::Relaxed),
        }
    }
}

impl ProxySelfGate for EluCpsGate {
    /// Try to admit one external new-dialog non-emergency INVITE. Order:
    /// 1. **Intake age** — `elu_ewma > elu_critical` → reject
    ///    `proxy_overload_intake_age`, `Retry-After: 1` (the bucket is NOT
    ///    touched).
    /// 2. **CPS bucket** — `try_take`; on empty → reject `proxy_overload_cps`
    ///    with the time-to-token of that same failed consume as `Retry-After`.
    /// 3. Otherwise **admit** (a token was consumed in step 2).
    fn try_admit_external(&self) -> AdmitDecision {
        let mut inner = locked(&self.inner, SELF_GATE);

        // 1. Intake pressure — hard reject before spending a token.
        if inner.elu_ewma.get() > self.elu_critical {
            drop(inner);
            self.rejected_intake_age.fetch_add(1, Ordering::Relaxed);
            self.shed.record(ShedReason::IntakeAge.label(), "shed", 1);
            return AdmitDecision::Reject {
                reason: ShedReason::IntakeAge,
                retry_after_sec: load_shed::retry_after::MIN_SEC,
            };
        }

        // 2. Hard CPS gate.
        let now = inner.now();
        if let Err(retry) = inner.bucket.try_take(now) {
            drop(inner);
            self.rejected_cps.fetch_add(1, Ordering::Relaxed);
            self.shed.record(ShedReason::Cps.label(), "shed", 1);
            return AdmitDecision::Reject { reason: ShedReason::Cps, retry_after_sec: retry };
        }

        // 3. Admit (a token has been consumed above). An admit reports the
        // recovery of whatever shedding window was open; the episode itself
        // ends on quiet, so a bucket flapping at its cap stays one episode.
        // One relaxed load per admit while nothing is shedding.
        drop(inner);
        self.external_admitted.fetch_add(1, Ordering::Relaxed);
        if self.shed.is_active() {
            self.shed.recovered_all();
        }
        AdmitDecision::Admit
    }

    fn note_bypass(&self, kind: BypassKind) {
        match kind {
            BypassKind::Internal => self.internal_bypassed.fetch_add(1, Ordering::Relaxed),
            BypassKind::Emergency => self.emergency_bypassed.fetch_add(1, Ordering::Relaxed),
        };
    }
}

#[cfg(test)]
mod tests;
