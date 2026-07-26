//! [`OverloadSignal`] — the per-worker publish surface: EWMA state + admit/
//! reject counters, the `X-Overload` header builder, and the Tier-3
//! [`should_admit`](OverloadSignal::should_admit) gate. The Prometheus render
//! lives in `prometheus`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::admission::{
    AdmissionConfig, AdmitDecision, AdmitReason, DEFAULT_CPS_BUCKET_RATE, DEFAULT_CPS_BUCKET_SIZE,
};
use super::bucket::TokenBucket;
use super::ewma::Ewma;
use super::sampler::{LiveLoadSampler, LoadSampler};

/// Snapshot of the published EWMAs + the counters and gate tallies, for
/// `/status` and Prometheus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverloadMetrics {
    /// EWMA-smoothed Event Loop Utilization — the `elu` published on X-Overload.
    pub elu_ewma: f64,
    /// EWMA-smoothed GC pause fraction — the `gc` published on X-Overload.
    pub gc_fraction_ewma: f64,
    /// Monotonic count of non-emergency new-dialog INVITEs admitted by this
    /// worker — the `adm` published on X-Overload.
    pub non_emergency_admitted_total: u64,
    /// Tier-3 rejects because the hard CPS token bucket was empty.
    pub reject_bucket_empty_total: u64,
    /// Tier-3 rejects because the worker's EWMA-ELU exceeded the panic backstop.
    pub reject_panic_elu_total: u64,
    /// Current CPS token-bucket level. A negative emergency overdraft reads as 0.
    pub token_bucket_level: f64,
    /// Monotonic count of EMERGENCY new-dialog INVITEs this worker admitted
    /// (Resource-Priority esnet/wps/q735 or an admitted `;emerg`/`;em` marker).
    /// These ALWAYS admit (bypassing the bucket-empty + panic-ELU checks, only
    /// `consume_forced`-ing a token) and are NOT counted on `adm`/the
    /// non-emergency total — so without this counter the emergency-admit branch
    /// would be entirely uncounted. The sum `non_emergency_admitted_total +
    /// emergency_admitted_total` is the worker's total admit rate.
    pub emergency_admitted_total: u64,
}

struct OverloadInner {
    sampler: Arc<dyn LoadSampler>,
    elu_ewma: Ewma,
    gc_fraction_ewma: Ewma,
    /// Tier-3 hard CPS gate. Seeded with the `B2buaConfig` defaults;
    /// reconfigured to the operator's values by
    /// [`configure_admission`](OverloadSignal::configure_admission) at ctx build.
    bucket: TokenBucket,
    /// Live-read admission knobs (panic-ELU threshold + Retry-After base).
    admission: AdmissionConfig,
}

/// Worker-side overload signal. Clone-cheap (shares one `Arc`); wire one into
/// [`RouterCtx`](crate::router::RouterCtx) and read it on the OPTIONS-200 path.
///
/// The EWMAs advance only when [`sample`](OverloadSignal::sample) is called — by
/// the periodic sampler task (see [`SAMPLE_PERIOD`](OverloadSignal::SAMPLE_PERIOD)).
/// The `adm` counter advances on
/// [`increment_non_emergency_admitted`](OverloadSignal::increment_non_emergency_admitted).
#[derive(Clone)]
pub struct OverloadSignal {
    inner: Arc<Mutex<OverloadInner>>,
    /// Lock-free `adm` counter — read on the header hot path without taking the
    /// EWMA lock. Monotonic.
    non_emergency_admitted: Arc<AtomicU64>,
    /// Tier-3 reject tallies, split by [`AdmitReason`]. Lock-free so the
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
    /// The sampler cadence. The periodic task in `b2bua_core` calls
    /// [`sample`](OverloadSignal::sample) once per period.
    pub const SAMPLE_PERIOD: std::time::Duration = std::time::Duration::from_millis(100);

    /// Build over a [`LoadSampler`]. The EWMAs start at `0` (uninitialised) and
    /// the `adm` counter at `0`, so the first header reads
    /// `v=1; elu=0.000; gc=0.000; adm=0` until the sampler fires.
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

    /// Default signal: a live busy-ratio sampler at the standard cadence. The
    /// EWMAs only move once a sampler task drives [`sample`](OverloadSignal::sample);
    /// without one the header is constant `elu=0.000; gc=0.000` — harmless to
    /// the proxy band (BelowSoft), which is the correct "no signal yet"
    /// classification.
    pub fn live() -> Self {
        Self::new(Arc::new(LiveLoadSampler::new()))
    }

    /// One sampler tick: read the sampler and feed both EWMAs. Called by the
    /// periodic task every [`SAMPLE_PERIOD`](OverloadSignal::SAMPLE_PERIOD).
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
    /// successive samples.
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

    /// Configure the Tier-3 admission gate from the worker's
    /// [`B2buaConfig`](crate::config::B2buaConfig). Called once by `b2bua_core`
    /// at ctx build, after the config is final (the harness `tune` seam has
    /// run), so the bucket capacity/rate and the live panic-ELU / Retry-After
    /// knobs reflect the operator's settings.
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

    /// Decide whether to admit a new INVITE — Tier 3 of the overload model. The
    /// caller MUST pass `is_emergency` for an emergency Resource-Priority
    /// request (`sip_message::is_emergency_request`).
    ///
    /// Order:
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
    /// The token is spent in step 2 before step 3 runs, so a `panic_elu` reject
    /// consumes a token too — sustained panic rejects deplete the CPS budget.
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
    /// OPTIONS-reply hot path.
    pub fn x_overload_header_value(&self) -> String {
        let (elu, gc) = {
            let inner = self.inner.lock().unwrap();
            (inner.elu_ewma.get(), inner.gc_fraction_ewma.get())
        };
        let adm = self.non_emergency_admitted.load(Ordering::Relaxed);
        // `{:.3}` — exactly three fractional digits, the schema the proxy parser
        // (`parse_x_overload_header`) and the schema test both expect.
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
}
