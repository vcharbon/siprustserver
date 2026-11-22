//! [`ProxySelfGate`]: the proxy's self-overload admission gate.
//!
//! The gate sheds external, new-dialog, non-emergency INVITEs on two checks:
//!
//!   - smoothed intake age `> elu_critical`: rejected `proxy_overload_intake_age`.
//!     The reading is packet age at dequeue, normalized by [`IntakeAgeSampler`];
//!     the `elu_*` names of the config, the EWMA and its gauge are the
//!     [`LoadSampler`] seam's.
//!   - the CPS token bucket empty: rejected `proxy_overload_cps`.
//!
//! An intake ramp ([`EluCpsGate::begin_ramp`], ADR-0069) scales the bucket's
//! rate and size to `ramp_start_pct` percent and back to the whole over
//! `ramp` from the first external INVITE it meets, while the POP returns to
//! rotation.
//!
//! `crate::core::request` classifies emergency, in-dialog and
//! worker-originated requests before the gate; a worker's request always
//! bypasses it. The token bucket, EWMA and [`LoadSampler`] seam are the
//! [`load_shed`] primitives; `sip-proxy` does not depend on `b2bua`.
//!
//! The bucket refills on `tokio::time::Instant` and the EWMA moves only on
//! [`EluCpsGate::sample`], driven by a `tokio::time::interval` task, so a
//! `start_paused` test advances both with `tokio::time::advance`.
//!
//! See ADR-0009 X4.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use load_shed::{clamp01, Ewma, LoadSampler, TokenBucket};

use crate::poison::{locked, SELF_GATE};

mod ramp;

use ramp::IntakeRamp;

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
    pub fn is_admit(&self) -> bool {
        matches!(self, AdmitDecision::Admit)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShedReason {
    /// The smoothed intake reading (packet age at dequeue, normalized by
    /// [`AGE_CRITICAL_MS`]) is past `elu_critical`.
    IntakeAge,
    /// The CPS bucket is empty.
    Cps,
}

impl ShedReason {
    pub const ALL: [ShedReason; 2] = [ShedReason::IntakeAge, ShedReason::Cps];

    /// The metric label and trace reason.
    pub const fn label(self) -> &'static str {
        match self {
            ShedReason::IntakeAge => "proxy_overload_intake_age",
            ShedReason::Cps => "proxy_overload_cps",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassKind {
    Internal,
    Emergency,
}

pub trait ProxySelfGate: Send + Sync {
    /// Decide whether to admit an external, non-emergency, new-dialog request.
    fn try_admit_external(&self) -> AdmitDecision;
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

#[derive(Debug, Clone, Copy)]
pub struct ProxySelfGateConfig {
    /// Above this smoothed intake reading (`0..=1`, see [`AGE_CRITICAL_MS`])
    /// the gate rejects every external new-dialog non-emergency INVITE.
    pub elu_critical: f64,
    pub cps_bucket_size: u32,
    /// Bucket refill rate (tokens/sec).
    pub cps_bucket_rate: u32,
    pub elu_smoothing_alpha: f64,
    /// Sampler cadence — the period of the runner task that drives
    /// [`EluCpsGate::sample`].
    pub sampler_interval: std::time::Duration,
    /// Percent of the bucket's rate and size an intake ramp starts at
    /// (`PROXY_RAMP_START_PCT`), in (0, 100].
    pub ramp_start_pct: f64,
    /// How long an intake ramp takes to reach the whole rate
    /// (`PROXY_RAMP_MS`); zero ramps nothing.
    pub ramp: std::time::Duration,
}

impl Default for ProxySelfGateConfig {
    /// `cps_bucket_size = 50`, `cps_bucket_rate = 100`: at 200 new calls/s the
    /// bucket drains in about 500 ms, then admits at the refill rate.
    /// `elu_critical = 0.8` sheds once the smoothed intake reading passes 0.8,
    /// a window-max dequeue age held above 400 ms (see [`AGE_CRITICAL_MS`]).
    fn default() -> Self {
        Self {
            elu_critical: 0.8,
            cps_bucket_size: 50,
            cps_bucket_rate: 100,
            elu_smoothing_alpha: 0.2,
            sampler_interval: std::time::Duration::from_millis(100),
            ramp_start_pct: 20.0,
            ramp: std::time::Duration::from_secs(30),
        }
    }
}

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
    pub fn record(&self, age_ms: u64) {
        self.max_age_ms.fetch_max(age_ms, Ordering::Relaxed);
    }

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
    pub fn new(recorder: IntakeAgeRecorder) -> Self {
        Self { recorder }
    }
}

impl LoadSampler for IntakeAgeSampler {
    fn elu(&self) -> f64 {
        clamp01(self.recorder.drain_max() as f64 / AGE_CRITICAL_MS as f64)
    }

    fn gc_fraction(&self) -> f64 {
        0.0
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProxySelfGateMetrics {
    /// The smoothed intake reading (`0..=1`); above `elu_critical` a new
    /// external call is refused 503.
    pub elu_ewma: f64,
    /// Most recent raw GC fraction (informational only).
    pub gc_fraction: f64,
    /// Token-bucket fill level (tokens remaining, floored at 0).
    pub cps_bucket_level: f64,
    pub cps_bucket_max: f64,
    /// External new-dialog non-emergency INVITEs admitted by the gate.
    pub external_admitted_total: u64,
    pub rejected_intake_age_total: u64,
    pub rejected_cps_total: u64,
    /// Worker-originated INVITEs that bypassed the gate (`is_worker_outbound`).
    pub internal_bypassed_total: u64,
    pub emergency_bypassed_total: u64,
    /// An intake ramp holds the bucket below its whole rate.
    pub ramp_active: bool,
}

struct GateInner {
    sampler: Arc<dyn LoadSampler>,
    elu_ewma: Ewma,
    gc_fraction: f64,
    bucket: TokenBucket,
    epoch: tokio::time::Instant,
    /// The bucket's whole size and rate, which a ramp scales.
    size: f64,
    rate: f64,
    ramp: Option<IntakeRamp>,
}

impl GateInner {
    fn now(&self) -> Duration {
        tokio::time::Instant::now().saturating_duration_since(self.epoch)
    }

    /// Set the bucket to the share of its whole size and rate the ramp admits
    /// at `now`, the whole once it ran out; a size of at least one token.
    fn follow_ramp(&mut self, now: Duration) {
        let Some(ramp) = self.ramp else { return };
        let share = ramp.share(now);
        if share.is_none() {
            self.ramp = None;
            tracing::info!(node = observe::node(), "intake ramp done");
        }
        let share = share.unwrap_or(1.0);
        let (size, rate) = ((self.size * share).max(1.0), self.rate * share);
        self.bucket.set_rate(size, rate, now);
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
    sampler_interval: std::time::Duration,
    ramp_start_pct: f64,
    ramp: Duration,
    /// Intake-shed aggregation keyed by rejection reason (ADR-0026): a shedding
    /// window is ONE episode — shed-on line, ~5 s summaries with the shed
    /// count, shed-off line with the totals — ended once admits have run clean
    /// for the idle window, so a bucket at its cap does not print per call.
    shed: Arc<observe::WaveSet>,
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
                size: f64::from(config.cps_bucket_size),
                rate: f64::from(config.cps_bucket_rate),
                ramp: None,
            })),
            elu_critical: config.elu_critical,
            sampler_interval: config.sampler_interval,
            ramp_start_pct: config.ramp_start_pct,
            ramp: config.ramp,
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

    pub fn sampler_interval(&self) -> std::time::Duration {
        self.sampler_interval
    }

    pub fn sample(&self) {
        let mut inner = locked(&self.inner, SELF_GATE);
        let elu = inner.sampler.elu();
        let gc = inner.sampler.gc_fraction();
        inner.elu_ewma.observe(elu);
        inner.gc_fraction = gc;
    }

    /// Start an intake ramp now (ADR-0069): the bucket admits
    /// `ramp_start_pct` percent of its rate, its size scaled alike, until the
    /// first external INVITE, then rises linearly to the whole over `ramp`
    /// from it; one under way starts again. A ramp of zero, or a start of the
    /// whole rate, does nothing.
    pub fn begin_ramp(&self) {
        let mut inner = locked(&self.inner, SELF_GATE);
        let now = inner.now();
        let Some(ramp) = IntakeRamp::starting(self.ramp_start_pct, self.ramp) else { return };
        inner.ramp = Some(ramp);
        inner.follow_ramp(now);
        tracing::info!(
            node = observe::node(),
            start_pct = self.ramp_start_pct,
            ramp_ms = u64::try_from(self.ramp.as_millis()).unwrap_or(u64::MAX),
            "intake ramp: the POP returns to rotation"
        );
    }

    /// The current smoothed intake reading (`0..=1`).
    pub fn elu_ewma(&self) -> f64 {
        locked(&self.inner, SELF_GATE).elu_ewma.get()
    }

    /// Snapshot of the gate's published state. A read changes nothing:
    /// `cps_bucket_level` is the level projected to now.
    pub fn metrics(&self) -> ProxySelfGateMetrics {
        let (elu_ewma, gc_fraction, cps_bucket_level, cps_bucket_max, ramp_active) = {
            let inner = locked(&self.inner, SELF_GATE);
            let now = inner.now();
            let level = inner.bucket.projected_level(now);
            let ramp_active = inner.ramp.is_some_and(|r| r.share(now).is_some());
            (inner.elu_ewma.get(), inner.gc_fraction, level, inner.bucket.capacity(), ramp_active)
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
            ramp_active,
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
        let now = inner.now();
        if let Some(ramp) = inner.ramp.as_mut() {
            ramp.offered(now);
        }

        if inner.elu_ewma.get() > self.elu_critical {
            drop(inner);
            self.rejected_intake_age.fetch_add(1, Ordering::Relaxed);
            self.shed.record(ShedReason::IntakeAge.label(), "shed", 1);
            return AdmitDecision::Reject {
                reason: ShedReason::IntakeAge,
                retry_after_sec: load_shed::retry_after::MIN_SEC,
            };
        }

        inner.follow_ramp(now);
        if let Err(retry) = inner.bucket.try_take(now) {
            drop(inner);
            self.rejected_cps.fetch_add(1, Ordering::Relaxed);
            self.shed.record(ShedReason::Cps.label(), "shed", 1);
            return AdmitDecision::Reject { reason: ShedReason::Cps, retry_after_sec: retry };
        }

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
