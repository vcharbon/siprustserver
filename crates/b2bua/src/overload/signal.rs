//! [`OverloadSignal`] — the per-worker publish surface: EWMA state, the CPS
//! token bucket, the admit counters, and the `X-Overload` header builder. The
//! panic-ELU and bucket rungs of the admission ladder ([`crate::admission`])
//! read their inputs here; the Prometheus render lives in `prometheus`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use load_shed::{Ewma, LoadSampler, TokenBucket};

use super::sampler::LiveLoadSampler;

/// Seed defaults, in lock-step with the `B2buaConfig` defaults (`b2bua-sdk`),
/// so a signal built without config still gates with them;
/// [`configure_admission`](OverloadSignal::configure_admission) installs the
/// operator's.
const DEFAULT_CPS_BUCKET_SIZE: u32 = 1000;
const DEFAULT_CPS_BUCKET_RATE: u32 = 500;
const DEFAULT_PANIC_ELU_THRESHOLD: f64 = 0.75;

/// Snapshot of the published EWMAs, the bucket level and the `adm` counter,
/// for `/status` and Prometheus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverloadMetrics {
    /// EWMA-smoothed Event Loop Utilization — the `elu` published on X-Overload.
    pub elu_ewma: f64,
    /// EWMA-smoothed GC pause fraction — the `gc` published on X-Overload.
    pub gc_fraction_ewma: f64,
    /// Monotonic count of non-emergency new-dialog INVITEs admitted by this
    /// worker — the `adm` published on X-Overload.
    pub non_emergency_admitted_total: u64,
    /// Current CPS token-bucket level, in `[0, capacity]`.
    pub token_bucket_level: f64,
}

struct OverloadInner {
    sampler: Arc<dyn LoadSampler>,
    elu_ewma: Ewma,
    gc_fraction_ewma: Ewma,
    /// The CPS token bucket. Seeded with the `B2buaConfig` defaults;
    /// reconfigured to the operator's values by
    /// [`configure_admission`](OverloadSignal::configure_admission) at ctx build.
    bucket: TokenBucket,
    /// The origin of the bucket's timeline: it refills on `tokio::time`, so a
    /// paused-clock test drives it with `tokio::time::advance`.
    epoch: tokio::time::Instant,
    /// The EWMA-ELU above which the panic backstop refuses a new call.
    panic_elu_threshold: f64,
}

impl OverloadInner {
    /// Now, on the bucket's timeline.
    fn now(&self) -> std::time::Duration {
        tokio::time::Instant::now().saturating_duration_since(self.epoch)
    }
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
                bucket: TokenBucket::full(
                    f64::from(DEFAULT_CPS_BUCKET_SIZE),
                    f64::from(DEFAULT_CPS_BUCKET_RATE),
                    std::time::Duration::ZERO,
                ),
                epoch: tokio::time::Instant::now(),
                panic_elu_threshold: DEFAULT_PANIC_ELU_THRESHOLD,
            })),
            non_emergency_admitted: Arc::new(AtomicU64::new(0)),
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
    /// successive samples. A setup its caller CANCELed before its turn ran is
    /// never treated and not counted.
    pub fn increment_non_emergency_admitted(&self) {
        self.non_emergency_admitted.fetch_add(1, Ordering::Relaxed);
    }

    /// Configure the bucket and the panic backstop from the worker's
    /// [`B2buaConfig`](crate::config::B2buaConfig). Called once by `b2bua_core`
    /// at ctx build, after the config is final (the harness `tune` seam has
    /// run), so the bucket capacity/rate and the panic-ELU threshold reflect
    /// the operator's settings.
    ///
    /// Replaces the bucket wholesale (resetting it to full at the configured
    /// capacity): it is a boot-time call before any admission decision, so there
    /// is no in-flight token state to preserve.
    pub fn configure_admission(&self, cfg: &crate::config::B2buaConfig) {
        let mut inner = self.inner.lock().unwrap();
        let now = inner.now();
        inner.bucket =
            TokenBucket::full(f64::from(cfg.cps_bucket_size), f64::from(cfg.cps_bucket_rate), now);
        inner.panic_elu_threshold = cfg.overload_panic_elu_threshold;
    }

    /// The panic-ELU rung's input: the worker's EWMA-ELU and the backstop
    /// above which it refuses a new normal call. The LB-side AIMD is the
    /// primary loop; the backstop catches an absent, misconfigured or
    /// overloaded LB.
    pub fn panic_elu(&self) -> (f64, f64) {
        let inner = self.inner.lock().unwrap();
        (inner.elu_ewma.get(), inner.panic_elu_threshold)
    }

    /// The bucket rung's input: seconds until the bucket holds a token, 0
    /// when it holds one now. Takes nothing.
    pub fn token_wait_sec(&self) -> u32 {
        let mut inner = self.inner.lock().unwrap();
        let now = inner.now();
        inner.bucket.wait_sec(now)
    }

    /// Spend a token for an admitted new call, when one is there: an
    /// emergency call admitted past an empty bucket owes nothing, so it never
    /// delays a later call (RFC 7339 §5.10.1 local preference).
    pub fn spend_token(&self) {
        let mut inner = self.inner.lock().unwrap();
        let now = inner.now();
        let _ = inner.bucket.try_take(now);
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

    /// Snapshot of the published EWMAs, the `adm` counter and the bucket
    /// level (for `/status`). Reading `token_bucket_level` refills the bucket
    /// as a side effect (lazy refill), which is harmless — it is the same
    /// refill the next peek would do.
    pub fn metrics(&self) -> OverloadMetrics {
        let (elu_ewma, gc_fraction_ewma, token_bucket_level) = {
            let mut inner = self.inner.lock().unwrap();
            let now = inner.now();
            let level = inner.bucket.level(now);
            (inner.elu_ewma.get(), inner.gc_fraction_ewma.get(), level)
        };
        OverloadMetrics {
            elu_ewma,
            gc_fraction_ewma,
            non_emergency_admitted_total: self.non_emergency_admitted.load(Ordering::Relaxed),
            token_bucket_level,
        }
    }
}
