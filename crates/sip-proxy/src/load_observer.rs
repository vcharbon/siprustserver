//! [`WorkerLoadObserver`] — per-`(LB, worker)` AIMD state machine fed by the
//! `X-Overload` payload workers stamp on OPTIONS replies (port of
//! `WorkerLoadObserver.ts`).
//!
//! Each LB keeps one observer holding one [`WorkerState`] bucket per worker:
//!
//!   - Eats `X-Overload` payloads handed in by the OPTIONS health probe
//!     ([`apply_payload`](WorkerLoadObserver::apply_payload)) and by the 503
//!     fast path ([`note_rejection_payload`](WorkerLoadObserver::note_rejection_payload)).
//!   - Runs AIMD on each payload: additive increase on cool workers,
//!     multiplicative decrease on hot ones, and a pin-to-floor when
//!     `elu > elu_critical`.
//!   - Hysteresis on band boundaries so a worker oscillating around a threshold
//!     does not flap.
//!   - A cooldown after every decrease so the worker has time to shed in-flight
//!     load before increases resume.
//!   - Exposes [`try_consume_for`](WorkerLoadObserver::try_consume_for) for the
//!     new-dialog admit path and [`band_for`](WorkerLoadObserver::band_for) for
//!     the `above_critical` filter.
//!   - Stale payloads (older than `payload_stale_ms`) trigger one conservative
//!     decrease on the next [`sweep_stale`](WorkerLoadObserver::sweep_stale) tick.
//!
//! ## Clock — explicit `now_ms`, NOT `tokio::time`
//!
//! The TS source is *deliberately decoupled* from Effect's `Clock`: every method
//! that needs the current time takes `nowMs` explicitly, so the AIMD ladder is a
//! pure state machine the unit tests drive with literal timestamps. This port
//! keeps that contract — `now_ms: i64` is threaded in (epoch-ms, the same units
//! [`sip_clock::Clock::now_ms`] hands out). The production caller (the OPTIONS
//! health probe) passes `clock.now_ms()`; under a paused runtime that advances in
//! lockstep with `tokio::time::advance`, so there is no separate clock to keep in
//! sync (CLAUDE.md). Nothing here arms a `tokio::time` timer — the bucket refill,
//! cooldown and stale windows are all `now_ms` arithmetic, exactly as in TS. This
//! is intentionally simpler than the b2bua/`self_gate` `tokio::time::Instant`
//! bucket: those gates have no second party and refill against ambient time; this
//! observer is fed a timestamp by whoever drives the probe/sweep cadence.

use std::collections::HashMap;
use std::sync::Mutex;

/// The `(elu, gc, adm)` triple parsed off an `X-Overload` header.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverloadPayload {
    /// Event-loop utilization EWMA, clamped to `0..=1`.
    pub elu: f64,
    /// GC pause fraction EWMA, clamped to `0..=1`.
    pub gc: f64,
    /// Worker's monotonic counter of non-emergency new-dialog admits (`>= 0`).
    pub adm: f64,
}

/// AIMD band derived from `elu` with hysteresis on transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EluBand {
    BelowSoft,
    SoftToHard,
    HardToCritical,
    /// Filtered out of non-emergency new-dialog candidates.
    AboveCritical,
}

/// The last AIMD action taken on a worker — for snapshots + diagnostics (port of
/// the TS `AimdAction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AimdAction {
    /// Bucket just created, no payload applied yet.
    Init,
    /// `soft_to_hard` — cap held (neither increase nor decrease).
    Hold,
    /// `below_soft`, cooldown elapsed — additive increase.
    Increase,
    /// `hard_to_critical` — multiplicative decrease.
    Decrease,
    /// `above_critical` — cap pinned to the floor immediately.
    DecreaseCritical,
    /// `below_soft` but a cooldown is still active — increase suppressed.
    Cooldown,
    /// `sweep_stale` fired (no fresh payload within `payload_stale_ms`).
    StaleDecrease,
}

/// Full per-worker snapshot (diagnostics, dashboards, tests). Port of the TS
/// `AimdSnapshot`.
#[derive(Debug, Clone, PartialEq)]
pub struct AimdSnapshot {
    pub worker_id: String,
    pub elu: f64,
    pub gc: f64,
    pub band: EluBand,
    pub cap_cps: f64,
    pub tokens: f64,
    pub cooldown_ms_remaining: i64,
    pub last_action: AimdAction,
    pub worker_treated_rate_cps: f64,
    pub own_admitted_rate_cps: f64,
    pub share: f64,
    pub payload_age_ms: i64,
    pub payload_missing_count: u64,
}

/// Band thresholds + hysteresis + AIMD ladder tunables (port of
/// `WorkerLoadObserverConfigData`).
#[derive(Debug, Clone, Copy)]
pub struct LoadObserverConfig {
    /// AIMD increase enabled while `elu <= elu_soft`.
    pub elu_soft: f64,
    /// Multiplicative decrease while `elu > elu_hard`.
    pub elu_hard: f64,
    /// Worker filtered out of new-dialog candidates while `elu > elu_critical`.
    pub elu_critical: f64,
    /// Hysteresis applied at every band boundary (exit threshold = enter − h).
    pub band_hysteresis: f64,
    /// Additive increase step per OPTIONS tick when below soft.
    pub aimd_increase_step_cps: f64,
    /// Multiplicative decrease factor when above hard (e.g. `0.75` = ×0.75).
    pub aimd_decrease_factor: f64,
    /// No increases for this many OPTIONS ticks after any decrease.
    pub aimd_cooldown_ticks: f64,
    /// Cap each worker starts at before any payload has arrived.
    pub cap_initial_cps: f64,
    /// Cap never decreases below this.
    pub cap_floor_cps: f64,
    /// Cap never increases above this.
    pub cap_ceiling_cps: f64,
    /// Payload older than this (ms) is stale; sweep conservatively decreases.
    pub payload_stale_ms: i64,
    /// Nominal OPTIONS interval (ms) — used for the cooldown clock.
    pub options_interval_ms: i64,
}

impl LoadObserverConfig {
    /// Band-classifier ordering + hysteresis-bounds guard (the ELU half of the
    /// cross-component validator in `config-validation.ts`).
    ///
    /// This is the slice of that validator that protects the band classifier
    /// **which is actually ported here** ([`WorkerLoadObserver::compute_band`]):
    ///   - `elu_soft < elu_hard < elu_critical` — a reversed band threshold turns
    ///     the controller inside-out (a low ELU lands in `AboveCritical` and the
    ///     worker is filtered out of new-dialog selection at idle).
    ///   - `band_hysteresis ∈ [0, min band gap)` — a hysteresis wider than the
    ///     narrower of the two band gaps means `compute_band` can never exit the
    ///     higher band (the hold-on-decrease arm always re-enters it), so the
    ///     worker is trapped one band high even at zero ELU.
    ///
    /// Pure: appends a human-readable line per violation to `violations` (so a
    /// caller can collect every config problem in one boot attempt) and returns
    /// whether the band config is well-formed. The runner's boot preflight
    /// (`sip-proxy-runner`) calls this as part of the full cross-component check;
    /// it is also independently meaningful because the AIMD/cap half of the
    /// observer config is fed straight to the (unported) admit ladder while
    /// `compute_band` is live today.
    ///
    /// Mirrors the TS field-order so the messages line up with the source.
    pub fn validate_bands(&self, violations: &mut Vec<String>) -> bool {
        let start = violations.len();

        // ELU band ordering. A reversed band threshold turns the controller
        // inside-out (low ELU triggers above_critical).
        if !(self.elu_soft < self.elu_hard && self.elu_hard < self.elu_critical) {
            violations.push(format!(
                "ELU band thresholds must satisfy elu_soft < elu_hard < elu_critical \
                 (got elu_soft={}, elu_hard={}, elu_critical={}).",
                self.elu_soft, self.elu_hard, self.elu_critical,
            ));
        }

        // Hysteresis wider than a band gap means `compute_band` cannot exit the
        // higher band even at zero ELU.
        let min_band_gap = (self.elu_hard - self.elu_soft).min(self.elu_critical - self.elu_hard);
        if self.band_hysteresis < 0.0 || self.band_hysteresis >= min_band_gap {
            violations.push(format!(
                "band_hysteresis ({}) must be in [0, min band gap) — min gap is {}. \
                 A hysteresis wider than a band traps the controller in the higher \
                 band even at zero ELU.",
                self.band_hysteresis, min_band_gap,
            ));
        }

        violations.len() == start
    }
}

impl Default for LoadObserverConfig {
    /// Defaults derived from `defaultWorkerLoadObserverConfig`, made **modestly
    /// more aggressive** for the emergency-under-CPU-starvation work (migration/32):
    /// a loaded worker stops taking new non-emergency sooner so it reserves CPU
    /// for emergency + already-established (in-dialog) calls. Concretely
    /// `elu_hard` 0.6 → 0.5 (multiplicative-decrease band opens earlier) and
    /// `elu_critical` 0.75 → 0.65 (a worker is dropped from non-emergency
    /// new-dialog candidates earlier); `aimd_decrease_factor` stays aggressive
    /// (×0.5 per decrease tick).
    ///
    /// **These are CALIBRATION STARTING POINTS, not final values.** They are to
    /// be tuned empirically against the cluster overload sweep (the all-worker
    /// 0.30-core `overloadall` case); the runner exposes every band/AIMD field as
    /// an env var (`LB_ELU_*`, `LB_AIMD_*`, `LB_CAP_*`) so the sweep can retune
    /// without a rebuild. Any change must keep `elu_soft < elu_hard < elu_critical`
    /// and `band_hysteresis < min band gap` (here min gap = 0.5−0.4 = 0.1 >
    /// 0.05) so `validate_bands` / the runner preflight accept the config.
    ///
    /// At 2 workers × ~50 CAPS sustained, `cap_initial_cps = 30` admits 30 cps per
    /// worker initially and AIMDs up while ELU stays low; a burst drains the
    /// bucket and the per-tick decreases collapse it further. `payload_stale_ms`
    /// (8000) MUST exceed one full HealthProbe cycle (`interval_ms + timeout_ms`)
    /// or `sweep_stale` halves the cap every cycle down to the floor, silently —
    /// see the TS module note.
    fn default() -> Self {
        Self {
            elu_soft: 0.4,
            elu_hard: 0.5,
            elu_critical: 0.65,
            band_hysteresis: 0.05,
            aimd_increase_step_cps: 2.0,
            aimd_decrease_factor: 0.5,
            aimd_cooldown_ticks: 5.0,
            cap_initial_cps: 30.0,
            cap_floor_cps: 1.0,
            cap_ceiling_cps: 200.0,
            payload_stale_ms: 8000,
            options_interval_ms: 1000,
        }
    }
}

/// Parse a worker's `X-Overload: v=1; elu=…; gc=…; adm=…` header value. Returns
/// `None` for missing/malformed/unknown-version headers (callers tick a
/// `payload_missing` counter on a miss). Forward-compatible: `v` other than `1`
/// → `None`; unknown params ignored.
pub fn parse_x_overload_header(value: Option<&str>) -> Option<OverloadPayload> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    let mut params: HashMap<&str, &str> = HashMap::new();
    for segment in value.split(';') {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            params.insert(k.trim(), v.trim());
        }
    }
    if params.get("v") != Some(&"1") {
        return None;
    }
    let elu: f64 = params.get("elu")?.parse().ok()?;
    let gc: f64 = params.get("gc")?.parse().ok()?;
    let adm: f64 = params.get("adm")?.parse().ok()?;
    if !elu.is_finite() || !gc.is_finite() || !adm.is_finite() || adm < 0.0 {
        return None;
    }
    let clamp01 = |n: f64| n.clamp(0.0, 1.0);
    Some(OverloadPayload { elu: clamp01(elu), gc: clamp01(gc), adm })
}

// ---------------------------------------------------------------------------
// Internal state (port of the TS `WorkerState`)
// ---------------------------------------------------------------------------

/// Per-`(LB, worker)` AIMD bucket. One per worker the LB has observed a payload
/// from (or that a `try_consume`/`sweep` first touched). All time fields are
/// epoch-ms, fed in via `now_ms` — never read from the wall clock here.
#[derive(Debug, Clone)]
struct WorkerState {
    cap: f64,
    tokens: f64,
    last_refill_at_ms: i64,
    cooldown_until_ms: i64,
    last_action: AimdAction,

    elu: f64,
    gc: f64,
    band: EluBand,

    last_adm: f64,
    last_adm_at_ms: i64,
    worker_treated_rate_cps: f64,

    own_admitted_since_last_tick: u64,
    own_admitted_rate_cps: f64,
    last_own_rate_tick_at_ms: i64,

    last_payload_at_ms: i64,
    payload_missing_count: u64,
}

impl WorkerState {
    fn fresh(config: &LoadObserverConfig, now_ms: i64) -> Self {
        Self {
            cap: config.cap_initial_cps,
            tokens: config.cap_initial_cps,
            last_refill_at_ms: now_ms,
            cooldown_until_ms: 0,
            last_action: AimdAction::Init,
            elu: 0.0,
            gc: 0.0,
            band: EluBand::BelowSoft,
            last_adm: 0.0,
            last_adm_at_ms: now_ms,
            worker_treated_rate_cps: 0.0,
            own_admitted_since_last_tick: 0,
            own_admitted_rate_cps: 0.0,
            last_own_rate_tick_at_ms: now_ms,
            last_payload_at_ms: now_ms,
            payload_missing_count: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Observer
// ---------------------------------------------------------------------------

/// Tracks one AIMD [`WorkerState`] per worker. Interior mutability via `Mutex` —
/// the background OPTIONS path writes (`apply_payload`/`sweep_stale`) and the LB
/// select path reads + consumes (`band_for`/`try_consume_for`), all CPU-only.
pub struct WorkerLoadObserver {
    config: LoadObserverConfig,
    /// Cached `aimd_cooldown_ticks * options_interval_ms` (ms a decrease blocks
    /// the next increase). Computed once at construction like the TS `cooldownMs`.
    cooldown_ms: i64,
    workers: Mutex<HashMap<String, WorkerState>>,
}

impl WorkerLoadObserver {
    pub fn new(config: LoadObserverConfig) -> Self {
        let cooldown_ms = (config.aimd_cooldown_ticks * config.options_interval_ms as f64) as i64;
        Self { config, cooldown_ms, workers: Mutex::new(HashMap::new()) }
    }

    /// Walk down the bands with hysteresis: once in a higher band, `elu` must
    /// drop below `enter − h` before transitioning out.
    fn compute_band(&self, elu: f64, prev: EluBand) -> EluBand {
        let c = &self.config;
        let h = c.band_hysteresis;
        if elu > c.elu_critical || (prev == EluBand::AboveCritical && elu > c.elu_critical - h) {
            return EluBand::AboveCritical;
        }
        if elu > c.elu_hard || (prev == EluBand::HardToCritical && elu > c.elu_hard - h) {
            return EluBand::HardToCritical;
        }
        if elu > c.elu_soft || (prev == EluBand::SoftToHard && elu > c.elu_soft - h) {
            return EluBand::SoftToHard;
        }
        EluBand::BelowSoft
    }

    /// Lazy refill: accrue `cap` tokens/sec for the elapsed time since the last
    /// refill, capped at `cap`. A no-op when no time has passed (the TS
    /// `dtSec <= 0` guard) so reading the snapshot or consuming repeatedly at the
    /// same `now_ms` doesn't over-fill.
    fn refill_bucket(state: &mut WorkerState, now_ms: i64) {
        let dt_sec = (now_ms - state.last_refill_at_ms) as f64 / 1000.0;
        if dt_sec <= 0.0 {
            return;
        }
        state.tokens = state.cap.min(state.tokens + state.cap * dt_sec);
        state.last_refill_at_ms = now_ms;
    }

    /// EWMA-smooth this LB's own admit rate to the worker so a single quiet tick
    /// doesn't crash the rate to 0 (TS `updateOwnRate`, α = 0.3 on the new sample).
    fn update_own_rate(state: &mut WorkerState, now_ms: i64) {
        let dt_sec = (now_ms - state.last_own_rate_tick_at_ms) as f64 / 1000.0;
        if dt_sec <= 0.0 {
            return;
        }
        let observed = state.own_admitted_since_last_tick as f64 / dt_sec;
        state.own_admitted_rate_cps = 0.7 * state.own_admitted_rate_cps + 0.3 * observed;
        state.own_admitted_since_last_tick = 0;
        state.last_own_rate_tick_at_ms = now_ms;
    }

    /// The AIMD step (port of TS `applyAimdStep`). Recomputes the band, then:
    /// `above_critical` → pin to floor + arm cooldown; `hard_to_critical` →
    /// multiplicative decrease + arm cooldown; `soft_to_hard` → hold; `below_soft`
    /// → additive increase iff the cooldown has elapsed (else suppress).
    fn apply_aimd_step(&self, state: &mut WorkerState, elu: f64, now_ms: i64) {
        let c = &self.config;
        let new_band = self.compute_band(elu, state.band);
        state.band = new_band;

        match new_band {
            EluBand::AboveCritical => {
                state.cap = c.cap_floor_cps;
                state.cooldown_until_ms = now_ms + self.cooldown_ms;
                state.last_action = AimdAction::DecreaseCritical;
            }
            EluBand::HardToCritical => {
                state.cap = c.cap_floor_cps.max(state.cap * c.aimd_decrease_factor);
                state.cooldown_until_ms = now_ms + self.cooldown_ms;
                state.last_action = AimdAction::Decrease;
            }
            EluBand::SoftToHard => {
                state.last_action = AimdAction::Hold;
            }
            EluBand::BelowSoft => {
                // Increase only if the cooldown has elapsed.
                if now_ms < state.cooldown_until_ms {
                    state.last_action = AimdAction::Cooldown;
                } else {
                    state.cap = c.cap_ceiling_cps.min(state.cap + c.aimd_increase_step_cps);
                    state.last_action = AimdAction::Increase;
                }
            }
        }
    }

    /// Diff the worker's monotonic `adm` counter into a treated-rate in cps. A
    /// counter that went *down* means the worker process restarted (per-process
    /// counter), so the baseline is reset and the rate zeroed (TS
    /// `updateCounterRate`).
    fn update_counter_rate(state: &mut WorkerState, adm: f64, now_ms: i64) {
        if adm < state.last_adm {
            state.last_adm = adm;
            state.last_adm_at_ms = now_ms;
            state.worker_treated_rate_cps = 0.0;
            return;
        }
        let dt_sec = (now_ms - state.last_adm_at_ms) as f64 / 1000.0;
        if dt_sec > 0.0 {
            state.worker_treated_rate_cps = (adm - state.last_adm) / dt_sec;
        }
        state.last_adm = adm;
        state.last_adm_at_ms = now_ms;
    }

    /// Shared body of [`apply_payload`](Self::apply_payload) and
    /// [`note_rejection_payload`](Self::note_rejection_payload) (TS
    /// `applyPayloadCommon`): refresh the rate diffs, stash `elu`/`gc`, mark the
    /// payload fresh, then run one AIMD step.
    fn apply_payload_common(&self, worker_id: &str, payload: &OverloadPayload, now_ms: i64) {
        let mut workers = self.workers.lock().unwrap();
        let state = workers
            .entry(worker_id.to_string())
            .or_insert_with(|| WorkerState::fresh(&self.config, now_ms));
        Self::update_counter_rate(state, payload.adm, now_ms);
        Self::update_own_rate(state, now_ms);
        state.elu = payload.elu;
        state.gc = payload.gc;
        state.last_payload_at_ms = now_ms;
        self.apply_aimd_step(state, payload.elu, now_ms);
    }

    /// Process an `X-Overload` payload from a worker (OPTIONS reply path): diff
    /// counters, recompute the band, and run one AIMD step. Port of the TS
    /// `applyPayload`.
    pub fn apply_payload(&self, worker_id: &str, payload: &OverloadPayload, now_ms: i64) {
        self.apply_payload_common(worker_id, payload, now_ms);
    }

    /// Fast path for an `X-Overload` payload that rode a 503 reply to a forwarded
    /// INVITE. Same AIMD step as [`apply_payload`](Self::apply_payload); a distinct
    /// entry point so the call site is explicit about why it knows the worker is
    /// hot (and metrics can label it separately). Port of TS `noteRejectionPayload`.
    pub fn note_rejection_payload(&self, worker_id: &str, payload: &OverloadPayload, now_ms: i64) {
        self.apply_payload_common(worker_id, payload, now_ms);
    }

    /// An OPTIONS reply arrived without a usable `X-Overload` header — tracked as
    /// a per-worker counter but NO AIMD step is taken (the band/cap are left where
    /// the last good payload put them). Port of TS `notePayloadMissing`.
    pub fn note_payload_missing(&self, worker_id: &str, now_ms: i64) {
        let mut workers = self.workers.lock().unwrap();
        let state = workers
            .entry(worker_id.to_string())
            .or_insert_with(|| WorkerState::fresh(&self.config, now_ms));
        state.payload_missing_count += 1;
    }

    /// The LB calls this when it has just forwarded a non-emergency new-dialog
    /// INVITE to a worker, so `own_admitted_rate` / `share` can be derived
    /// independently of the worker's own report. A no-op for an unobserved worker
    /// (no bucket yet — bootstrap admits without one). Port of TS `recordOwnAdmitted`.
    pub fn record_own_admitted(&self, worker_id: &str) {
        if let Some(state) = self.workers.lock().unwrap().get_mut(worker_id) {
            state.own_admitted_since_last_tick += 1;
        }
    }

    /// Attempt to consume one token from the worker's bucket. `true` ⇒ admitted
    /// (token spent); `false` ⇒ the bucket is empty. An **unknown worker is
    /// admitted** (we don't gate workers we've never observed a payload from —
    /// bootstrap-friendly). Port of TS `tryConsumeFor`.
    pub fn try_consume_for(&self, worker_id: &str, now_ms: i64) -> bool {
        let mut workers = self.workers.lock().unwrap();
        let Some(state) = workers.get_mut(worker_id) else {
            return true; // bootstrap-friendly
        };
        Self::refill_bucket(state, now_ms);
        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            true
        } else {
            // Bucket empty — the actual 503 (+ Retry-After) is synthesized at the
            // strategy boundary as `SelectError::RateCapExhausted`; this only
            // reports the consume outcome. The aggregate
            // `sip_proxy_overload_rejections_total{reason="bucket_empty"}` IS
            // counted there (load_balancer.rs); the TS per-worker-labelled
            // `rejectionsCounter{reason=bucket_empty}` push awaits a per-worker
            // Prometheus surface (ProxyMetrics is registry-aggregate today — same
            // deferral as the band-only port).
            false
        }
    }

    /// Seconds until ≥ 1 token will be available for this worker (`0` if available
    /// now, or for an unknown worker). With an empty bucket and a non-positive cap
    /// (the floor is `>= 1` by config, so this is defensive) returns `60`, the TS
    /// `retryAfterSec` fallback.
    ///
    /// **Deliberate deviation from the source:** the TS `LoadBalancer` hard-codes
    /// `retryAfterSec: 1` at the `RateCapExhausted` raise site
    /// (`LoadBalancer.ts:345`); the bare `tryConsumeFor` returns only a boolean.
    /// We compute a *real* per-bucket Retry-After from the worker's own cap/fill
    /// rate, which is strictly better signalling for the UAC (a near-full bucket
    /// retries in ~1 s; a floored one waits longer). `select_for_new_dialog` feeds
    /// this into `SelectError::RateCapExhausted` (clamped to `>= 1`, so the wire
    /// value is never a no-op `Retry-After: 0`).
    pub fn retry_after_sec_for(&self, worker_id: &str, now_ms: i64) -> u32 {
        let mut workers = self.workers.lock().unwrap();
        let Some(state) = workers.get_mut(worker_id) else {
            return 0;
        };
        Self::refill_bucket(state, now_ms);
        if state.tokens >= 1.0 {
            return 0;
        }
        if state.cap <= 0.0 {
            return 60;
        }
        ((1.0 - state.tokens) / state.cap).ceil() as u32
    }

    /// The worker's current band, or `None` if no payload has ever arrived.
    pub fn band_for(&self, worker_id: &str) -> Option<EluBand> {
        self.workers.lock().unwrap().get(worker_id).map(|s| s.band)
    }

    /// Periodic sweep — call every ~`options_interval_ms`. Each worker whose last
    /// payload is older than `payload_stale_ms` gets one conservative
    /// multiplicative decrease (floored), a re-armed cooldown, and
    /// `payload_missing_count++`. Port of TS `sweepStale`.
    ///
    /// Returns the number of workers floored this sweep, so the caller can feed a
    /// coarse `stale_decrease` aggregate counter — the observer itself stays pure
    /// (no `ProxyMetrics` dependency, no clock), mirroring how the per-worker
    /// `bucket_empty` rejection is counted at the strategy boundary, not here. The
    /// per-worker push (TS `staleDecreaseCounter` / `payloadMissingCounter`,
    /// labelled by `worker_id`) is a deferred slice; the per-worker smoking gun is
    /// preserved meanwhile in the snapshot's `last_action = StaleDecrease` +
    /// `payload_missing_count`.
    pub fn sweep_stale(&self, now_ms: i64) -> u64 {
        let c = &self.config;
        let mut workers = self.workers.lock().unwrap();
        let mut floored = 0u64;
        for state in workers.values_mut() {
            let age = now_ms - state.last_payload_at_ms;
            if age <= c.payload_stale_ms {
                continue;
            }
            state.cap = c.cap_floor_cps.max(state.cap * c.aimd_decrease_factor);
            state.cooldown_until_ms = now_ms + self.cooldown_ms;
            state.last_action = AimdAction::StaleDecrease;
            state.payload_missing_count += 1;
            floored += 1;
        }
        floored
    }

    /// Full per-worker snapshot (diagnostics, dashboards, tests). Reading the
    /// snapshot also lazily refills each bucket so the `tokens` field isn't stale
    /// (the same refill the next consume would do). Port of TS `snapshot`.
    pub fn snapshot(&self, now_ms: i64) -> Vec<AimdSnapshot> {
        let mut workers = self.workers.lock().unwrap();
        let mut out = Vec::with_capacity(workers.len());
        for (worker_id, state) in workers.iter_mut() {
            Self::refill_bucket(state, now_ms);
            let total = state.worker_treated_rate_cps;
            let share = if total > 0.0 { state.own_admitted_rate_cps / total } else { 0.0 };
            out.push(AimdSnapshot {
                worker_id: worker_id.clone(),
                elu: state.elu,
                gc: state.gc,
                band: state.band,
                cap_cps: state.cap,
                tokens: state.tokens,
                cooldown_ms_remaining: (state.cooldown_until_ms - now_ms).max(0),
                last_action: state.last_action,
                worker_treated_rate_cps: total,
                own_admitted_rate_cps: state.own_admitted_rate_cps,
                share,
                payload_age_ms: now_ms - state.last_payload_at_ms,
                payload_missing_count: state.payload_missing_count,
            });
        }
        out
    }

    /// Drop state for workers that left the registry — the map stays bounded under
    /// worker churn (nothing else ever removes an entry).
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        self.workers.lock().unwrap().retain(|id, _| keep(id));
    }

    /// Forget a worker's state entirely. A recreated pod (same ordinal, new host)
    /// must be judged from scratch — inheriting the dead pod's band (e.g.
    /// `AboveCritical` at the moment it crashed) excluded the idle fresh pod from
    /// new-dialog selection until its first `X-Overload` reply.
    pub fn reset(&self, worker_id: &str) {
        self.workers.lock().unwrap().remove(worker_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The AIMD ladder is a pure state machine driven by an explicit `now_ms`
    // (epoch-ms), faithfully to the TS source which decouples from Effect's Clock
    // and passes `nowMs` to every method. These tests therefore need NO paused
    // tokio runtime — they pass literal timestamps exactly as the TS suite does
    // (`obs.applyPayload(W, payload(...), 1000)` → `obs.snapshot(1000)`). They are
    // pure CPU, sub-millisecond, default-lane (CLAUDE.md test-runtime policy).

    const W: &str = "worker-A";

    fn payload(elu: f64) -> OverloadPayload {
        OverloadPayload { elu, gc: 0.0, adm: 0.0 }
    }
    fn payload_adm(elu: f64, adm: f64) -> OverloadPayload {
        OverloadPayload { elu, gc: 0.0, adm }
    }

    /// Build an observer over `default()` with the given field overrides applied
    /// via a closure — mirrors the TS `withObserver(body, overrides)` fixture.
    fn obs_with(f: impl FnOnce(&mut LoadObserverConfig)) -> WorkerLoadObserver {
        let mut cfg = LoadObserverConfig::default();
        f(&mut cfg);
        WorkerLoadObserver::new(cfg)
    }
    fn obs() -> WorkerLoadObserver {
        WorkerLoadObserver::new(LoadObserverConfig::default())
    }

    /// Pin band thresholds so the AIMD tests land in a known band regardless of
    /// operational-default tuning (the TS `BANDS_CFG` / `AIMD_CFG`).
    fn bands_cfg(cfg: &mut LoadObserverConfig) {
        cfg.elu_soft = 0.6;
        cfg.elu_hard = 0.8;
        cfg.elu_critical = 0.95;
    }

    fn snap1(o: &WorkerLoadObserver, now_ms: i64) -> AimdSnapshot {
        o.snapshot(now_ms).into_iter().next().expect("one worker")
    }

    // ── X-Overload header parse (kept from the band-only port) ────────────

    #[test]
    fn parses_x_overload_header() {
        let p = parse_x_overload_header(Some("v=1; elu=0.5; gc=0.1; adm=42")).unwrap();
        assert_eq!(p.elu, 0.5);
        assert_eq!(p.gc, 0.1);
        assert_eq!(p.adm, 42.0);
    }

    #[test]
    fn rejects_bad_overload_headers() {
        assert!(parse_x_overload_header(None).is_none());
        assert!(parse_x_overload_header(Some("")).is_none());
        assert!(parse_x_overload_header(Some("v=2; elu=0.5; gc=0; adm=0")).is_none());
        assert!(parse_x_overload_header(Some("v=1; elu=x; gc=0; adm=0")).is_none());
        assert!(parse_x_overload_header(Some("v=1; elu=0.5; gc=0; adm=-1")).is_none());
    }

    #[test]
    fn clamps_elu_and_gc() {
        let p = parse_x_overload_header(Some("v=1; elu=1.5; gc=-0.2; adm=3")).unwrap();
        assert_eq!(p.elu, 1.0);
        assert_eq!(p.gc, 0.0);
    }

    // ── band derivation + hysteresis (kept; now over WorkerState) ─────────

    /// Pinned to explicit thresholds (`bands_cfg`: soft 0.6 / hard 0.8 / critical
    /// 0.95) so it exercises the band classifier regardless of operational-default
    /// tuning — the migration/32 default-band move (hard 0.6→0.5, critical
    /// 0.75→0.65) does not perturb it.
    #[test]
    fn band_thresholds() {
        let o = obs_with(bands_cfg);
        o.apply_payload("w", &payload(0.3), 1000);
        assert_eq!(o.band_for("w"), Some(EluBand::BelowSoft));
        o.apply_payload("w", &payload(0.7), 2000);
        assert_eq!(o.band_for("w"), Some(EluBand::SoftToHard));
        o.apply_payload("w", &payload(0.85), 3000);
        assert_eq!(o.band_for("w"), Some(EluBand::HardToCritical));
        o.apply_payload("w", &payload(0.97), 4000);
        assert_eq!(o.band_for("w"), Some(EluBand::AboveCritical));
    }

    /// Hysteresis-hold in the increasing→holding direction, pinned to explicit
    /// thresholds (`bands_cfg`) so it is independent of the default tuning.
    #[test]
    fn hysteresis_holds_higher_band_until_below_enter_minus_h() {
        let o = obs_with(bands_cfg); // critical = 0.95, h = 0.05 (default)
        o.apply_payload("w", &payload(0.97), 1000); // above_critical
        // stays above_critical until elu <= elu_critical − h = 0.90.
        o.apply_payload("w", &payload(0.93), 2000);
        assert_eq!(o.band_for("w"), Some(EluBand::AboveCritical));
        o.apply_payload("w", &payload(0.89), 3000);
        assert_eq!(o.band_for("w"), Some(EluBand::HardToCritical));
    }

    /// it("once in hard_to_critical, elu must drop below hard − h to exit") — the
    /// hold-on-decrease branch of `compute_band` for the hard band (TS
    /// WorkerLoadObserver.test.ts:88-104). `band_thresholds` only exercises that
    /// arm in the *increasing* direction; this pins the hysteresis-hold direction.
    #[test]
    fn hysteresis_holds_hard_to_critical_until_below_hard_minus_h() {
        let o = obs_with(|c| {
            c.elu_soft = 0.6;
            c.elu_hard = 0.8;
            c.elu_critical = 0.95;
            c.band_hysteresis = 0.02;
        });
        // Enter hard_to_critical.
        o.apply_payload("w", &payload(0.82), 1000);
        assert_eq!(o.band_for("w"), Some(EluBand::HardToCritical));
        // Inside the hysteresis zone (elu_hard − h = 0.78 < 0.79 <= 0.80) — hold.
        o.apply_payload("w", &payload(0.79), 2000);
        assert_eq!(o.band_for("w"), Some(EluBand::HardToCritical));
        // Past the exit threshold (0.77 < 0.78) — drop to soft_to_hard.
        o.apply_payload("w", &payload(0.77), 3000);
        assert_eq!(o.band_for("w"), Some(EluBand::SoftToHard));
    }

    #[test]
    fn unknown_worker_has_no_band() {
        assert!(obs().band_for("nope").is_none());
    }

    #[test]
    fn retain_drops_departed_workers() {
        let o = obs();
        o.apply_payload("w0", &payload(0.5), 1000);
        o.apply_payload("w1", &payload(0.5), 1000);
        o.retain(|id| id == "w0");
        assert!(o.band_for("w0").is_some());
        assert!(o.band_for("w1").is_none(), "departed worker state must be dropped");
    }

    #[test]
    fn reset_clears_inherited_state_for_a_recreated_pod() {
        let o = obs();
        o.apply_payload("w0", &payload(0.99), 1000); // crashed while AboveCritical
        assert_eq!(o.band_for("w0"), Some(EluBand::AboveCritical));
        o.reset("w0");
        assert!(o.band_for("w0").is_none(), "the fresh pod must be judged from scratch");
    }

    // ── AIMD increase ladder (TS "AIMD increase ladder") ──────────────────

    /// it("additive increase when below_soft, no cooldown")
    #[test]
    fn additive_increase_when_below_soft_no_cooldown() {
        let o = obs_with(|c| {
            c.cap_initial_cps = 100.0;
            c.aimd_increase_step_cps = 5.0;
        });
        o.apply_payload(W, &payload(0.1), 1000);
        let snap = snap1(&o, 1000);
        assert_eq!(snap.last_action, AimdAction::Increase);
        assert_eq!(snap.cap_cps, 105.0);

        o.apply_payload(W, &payload(0.1), 2000);
        assert_eq!(snap1(&o, 2000).cap_cps, 110.0);

        o.apply_payload(W, &payload(0.1), 3000);
        assert_eq!(snap1(&o, 3000).cap_cps, 115.0);
    }

    /// it("cap never exceeds capCeilingCps")
    #[test]
    fn cap_never_exceeds_cap_ceiling_cps() {
        let o = obs_with(|c| {
            c.cap_initial_cps = 100.0;
            c.cap_ceiling_cps = 110.0;
            c.aimd_increase_step_cps = 5.0;
        });
        // 20 increases × 5 = +100 ⇒ would hit 120, but the ceiling is 110.
        for i in 1..=20 {
            o.apply_payload(W, &payload(0.1), i * 1000);
        }
        assert_eq!(snap1(&o, 20_000).cap_cps, 110.0);
    }

    // ── AIMD decrease + cooldown (TS "AIMD decrease + cooldown") ──────────

    /// it("multiplicative decrease when in hard_to_critical")
    #[test]
    fn multiplicative_decrease_when_in_hard_to_critical() {
        let o = obs_with(|c| {
            bands_cfg(c);
            c.cap_initial_cps = 100.0;
            c.aimd_decrease_factor = 0.75;
        });
        o.apply_payload(W, &payload(0.85), 1000);
        let snap = snap1(&o, 1000);
        assert_eq!(snap.last_action, AimdAction::Decrease);
        assert_eq!(snap.cap_cps, 75.0); // 100 × 0.75
    }

    /// it("decrease arms a cooldown that blocks subsequent increases")
    #[test]
    fn decrease_arms_a_cooldown_that_blocks_subsequent_increases() {
        let o = obs_with(|c| {
            bands_cfg(c);
            c.cap_initial_cps = 100.0;
            c.aimd_decrease_factor = 0.75;
            c.aimd_cooldown_ticks = 3.0;
            c.options_interval_ms = 1000;
            c.aimd_increase_step_cps = 5.0;
        });
        // Decrease at t=1000ms. Cooldown = 3 ticks × 1000ms = 3000ms.
        o.apply_payload(W, &payload(0.85), 1000);
        assert_eq!(snap1(&o, 1000).cooldown_ms_remaining, 3000);

        // t=2000ms: elu drops to 0.1 — would normally increase, but cooldown holds.
        o.apply_payload(W, &payload(0.1), 2000);
        let snap = snap1(&o, 2000);
        assert_eq!(snap.last_action, AimdAction::Cooldown);
        assert_eq!(snap.cap_cps, 75.0); // unchanged

        // t=5000ms: cooldown elapsed — increase re-enabled.
        o.apply_payload(W, &payload(0.1), 5000);
        let snap = snap1(&o, 5000);
        assert_eq!(snap.last_action, AimdAction::Increase);
        assert_eq!(snap.cap_cps, 80.0); // 75 + 5
    }

    /// it("decrease never goes below capFloorCps")
    #[test]
    fn decrease_never_goes_below_cap_floor_cps() {
        let o = obs_with(|c| {
            bands_cfg(c);
            c.cap_initial_cps = 100.0;
            c.cap_floor_cps = 1.0;
            c.aimd_decrease_factor = 0.75;
            c.aimd_cooldown_ticks = 3.0;
            c.options_interval_ms = 1000;
        });
        // Force many decreases past the cooldown window so they all land.
        for i in 0..20 {
            o.apply_payload(W, &payload(0.85), 1000 + i * 4000);
        }
        let snap = snap1(&o, 100_000);
        assert!(snap.cap_cps >= 1.0);
        assert_eq!(snap.cap_cps, 1.0); // floor pinned
    }

    // ── CRITICAL filter behaviour (TS "above_critical pins cap at floor") ─

    /// it("above_critical pins cap at floor immediately")
    #[test]
    fn above_critical_pins_cap_at_floor_immediately() {
        let o = obs_with(|c| {
            bands_cfg(c);
            c.cap_initial_cps = 100.0;
            c.cap_floor_cps = 1.0;
        });
        o.apply_payload(W, &payload(0.99), 1000);
        let snap = snap1(&o, 1000);
        assert_eq!(snap.last_action, AimdAction::DecreaseCritical);
        assert_eq!(snap.cap_cps, 1.0);
        assert_eq!(snap.band, EluBand::AboveCritical);
    }

    // ── token bucket (TS "token bucket") ──────────────────────────────────

    /// it("unknown worker is admitted (bootstrap-friendly)")
    #[test]
    fn unknown_worker_is_admitted_bootstrap_friendly() {
        assert!(obs().try_consume_for("unknown-worker", 1000));
    }

    /// it("bucket starts full at capInitialCps tokens")
    #[test]
    fn bucket_starts_full_at_cap_initial_cps_tokens() {
        // Seed elu=0.7 lands in soft_to_hard ⇒ AIMD `hold` ⇒ cap stays at
        // cap_initial_cps (no increase to confuse the refill arithmetic).
        let o = obs_with(|c| {
            bands_cfg(c);
            c.cap_initial_cps = 10.0;
        });
        o.apply_payload(W, &payload(0.7), 1000);
        for _ in 0..10 {
            assert!(o.try_consume_for(W, 1000));
        }
        assert!(!o.try_consume_for(W, 1000));
    }

    /// it("bucket refills at cap tokens/sec over elapsed time")
    #[test]
    fn bucket_refills_at_cap_tokens_per_sec_over_elapsed_time() {
        let o = obs_with(|c| {
            bands_cfg(c);
            c.cap_initial_cps = 10.0;
        });
        o.apply_payload(W, &payload(0.7), 0);
        // Drain the bucket.
        for _ in 0..10 {
            o.try_consume_for(W, 0);
        }
        assert!(!o.try_consume_for(W, 0));
        // 500ms later → bucket gains 10 × 0.5 = 5 tokens.
        assert!(o.try_consume_for(W, 500));
        for _ in 0..4 {
            assert!(o.try_consume_for(W, 500));
        }
        assert!(!o.try_consume_for(W, 500));
    }

    // ── counter math (TS "counter math") ──────────────────────────────────

    /// it("worker_treated_rate is (adm_delta / dt) in cps")
    #[test]
    fn worker_treated_rate_is_adm_delta_over_dt_in_cps() {
        let o = obs();
        o.apply_payload(W, &payload_adm(0.5, 0.0), 0);
        // 100 admits over 1s → 100 cps treated rate.
        o.apply_payload(W, &payload_adm(0.5, 100.0), 1000);
        assert!((snap1(&o, 1000).worker_treated_rate_cps - 100.0).abs() < 1e-5);
    }

    /// it("adm counter decrease (worker restart) resets baseline")
    #[test]
    fn adm_counter_decrease_worker_restart_resets_baseline() {
        let o = obs();
        o.apply_payload(W, &payload_adm(0.5, 1000.0), 0);
        o.apply_payload(W, &payload_adm(0.5, 1100.0), 1000);
        // Worker restarted — adm dropped back to 50.
        o.apply_payload(W, &payload_adm(0.5, 50.0), 2000);
        assert_eq!(snap1(&o, 2000).worker_treated_rate_cps, 0.0); // reset
        // From here forward, normal rate derivation resumes.
        o.apply_payload(W, &payload_adm(0.5, 100.0), 3000);
        assert!((snap1(&o, 3000).worker_treated_rate_cps - 50.0).abs() < 1e-5);
    }

    /// it("recordOwnAdmitted and share metric")
    #[test]
    fn record_own_admitted_and_share_metric() {
        let o = obs();
        o.apply_payload(W, &payload_adm(0.5, 0.0), 0);
        // This LB admits 30 in the first second; worker total is 100.
        for _ in 0..30 {
            o.record_own_admitted(W);
        }
        o.apply_payload(W, &payload_adm(0.5, 100.0), 1000);
        let snap = snap1(&o, 1000);
        // own_admitted_rate EWMA-smoothed: 0.7×0 + 0.3×30 = 9
        assert!((snap.own_admitted_rate_cps - 9.0).abs() < 1e-5);
        // share = 9 / 100 = 0.09
        assert!((snap.share - 0.09).abs() < 1e-5);
    }

    // ── stale-payload sweep (TS "stale-payload sweep") ────────────────────

    /// it("sweep below stale threshold is a no-op")
    #[test]
    fn sweep_below_stale_threshold_is_a_no_op() {
        // Seed elu=0.7 (hold band under bands_cfg) so the initial applyPayload
        // does not mutate the cap — isolates the sweep's behaviour.
        let o = obs_with(|c| {
            bands_cfg(c);
            c.payload_stale_ms = 5000;
        });
        o.apply_payload(W, &payload(0.7), 1000);
        assert_eq!(o.sweep_stale(1500), 0, "no worker floored below the stale threshold");
        let snap = snap1(&o, 1500);
        assert_eq!(snap.last_action, AimdAction::Hold);
        assert_eq!(snap.payload_missing_count, 0);
    }

    /// it("sweep above stale threshold triggers conservative decrease")
    #[test]
    fn sweep_above_stale_threshold_triggers_conservative_decrease() {
        let o = obs_with(|c| {
            bands_cfg(c);
            c.cap_initial_cps = 100.0;
            c.aimd_decrease_factor = 0.75;
            c.payload_stale_ms = 5000;
        });
        o.apply_payload(W, &payload(0.7), 0);
        assert_eq!(o.sweep_stale(6000), 1, "the one stale worker is floored (feeds the aggregate counter)");
        let snap = snap1(&o, 6000);
        assert_eq!(snap.last_action, AimdAction::StaleDecrease);
        assert_eq!(snap.cap_cps, 75.0); // 100 × 0.75 — cap unchanged by seed (hold band)
        assert_eq!(snap.payload_missing_count, 1);
    }

    // ── diagnostics (TS "diagnostics") ────────────────────────────────────

    /// it("notePayloadMissing increments counter without an AIMD step")
    #[test]
    fn note_payload_missing_increments_counter_without_an_aimd_step() {
        let o = obs_with(bands_cfg);
        o.apply_payload(W, &payload(0.7), 1000); // hold band — cap pinned
        let before = snap1(&o, 1000);
        o.note_payload_missing(W, 2000);
        o.note_payload_missing(W, 2500);
        let after = snap1(&o, 2500);
        assert_eq!(after.payload_missing_count, 2);
        assert_eq!(after.cap_cps, before.cap_cps); // unchanged
    }

    /// it("snapshot returns one entry per known worker")
    #[test]
    fn snapshot_returns_one_entry_per_known_worker() {
        let o = obs_with(bands_cfg);
        o.apply_payload("worker-A", &payload(0.3), 1000);
        o.apply_payload("worker-B", &payload(0.9), 1000);
        o.apply_payload("worker-C", &payload(0.5), 1000);
        let snap = o.snapshot(1000);
        assert_eq!(snap.len(), 3);
        let by_id: HashMap<&str, &AimdSnapshot> =
            snap.iter().map(|s| (s.worker_id.as_str(), s)).collect();
        assert_eq!(by_id["worker-A"].band, EluBand::BelowSoft);
        assert_eq!(by_id["worker-B"].band, EluBand::HardToCritical);
        assert_eq!(by_id["worker-C"].band, EluBand::BelowSoft);
    }

    // ── retry-after (powers SelectError::RateCapExhausted) ────────────────

    /// An empty bucket reports a finite, ceil'd Retry-After; an unknown worker
    /// reports 0 (bootstrap admits, no rate-cap). Pins the value the strategy
    /// feeds into `SelectError::RateCapExhausted`.
    #[test]
    fn retry_after_is_finite_when_capped_and_zero_when_unknown() {
        let o = obs_with(|c| {
            bands_cfg(c);
            c.cap_initial_cps = 10.0;
        });
        assert_eq!(o.retry_after_sec_for("unknown", 1000), 0);
        o.apply_payload(W, &payload(0.7), 1000);
        for _ in 0..10 {
            o.try_consume_for(W, 1000);
        }
        assert!(!o.try_consume_for(W, 1000)); // drained
        // empty bucket, cap=10/s → (1-0)/10 = 0.1 → ceil = 1s.
        assert_eq!(o.retry_after_sec_for(W, 1000), 1);
    }

    // ── band-classifier config guard (validate_bands) ─────────────────────
    // The ELU half of config-validation.ts that protects the ported
    // `compute_band`. Pure, no clock — the same shape as the parse tests.

    #[test]
    fn validate_bands_accepts_shipped_defaults() {
        let mut v = Vec::new();
        assert!(LoadObserverConfig::default().validate_bands(&mut v));
        assert!(v.is_empty());
    }

    /// Pin the migration/32 more-aggressive default band thresholds (a loaded
    /// worker sheds non-emergency sooner). These are calibration starting points;
    /// the runner exposes them as env vars (`LB_ELU_*`) so the cluster sweep can
    /// retune without a rebuild. The test guards an accidental revert and a
    /// retuning that would break `validate_bands` (it asserts the ordering +
    /// hysteresis-bound invariants hold for the shipped values).
    #[test]
    fn default_bands_are_the_aggressive_calibration_starting_points() {
        let c = LoadObserverConfig::default();
        assert_eq!(c.elu_soft, 0.4);
        assert_eq!(c.elu_hard, 0.5, "elu_hard lowered 0.6 → 0.5 (migration/32)");
        assert_eq!(c.elu_critical, 0.65, "elu_critical lowered 0.75 → 0.65 (migration/32)");
        assert_eq!(c.aimd_decrease_factor, 0.5, "decrease stays aggressive (×0.5)");
        // Invariants that keep validate_bands / the runner preflight happy.
        assert!(c.elu_soft < c.elu_hard && c.elu_hard < c.elu_critical);
        let min_gap = (c.elu_hard - c.elu_soft).min(c.elu_critical - c.elu_hard);
        assert!(c.band_hysteresis < min_gap, "hysteresis {} must be < min gap {}", c.band_hysteresis, min_gap);
    }

    /// Mirrors TS "rejects eluSoft >= eluHard".
    #[test]
    fn validate_bands_rejects_elu_soft_ge_elu_hard() {
        let mut cfg = LoadObserverConfig::default();
        cfg.elu_soft = 0.6;
        cfg.elu_hard = 0.6;
        let mut v = Vec::new();
        assert!(!cfg.validate_bands(&mut v));
        assert!(v.iter().any(|m| m.contains("elu_soft < elu_hard")));
    }

    /// Mirrors TS "rejects eluHard >= eluCritical".
    #[test]
    fn validate_bands_rejects_elu_hard_ge_elu_critical() {
        let mut cfg = LoadObserverConfig::default();
        cfg.elu_hard = 0.75;
        cfg.elu_critical = 0.75;
        let mut v = Vec::new();
        assert!(!cfg.validate_bands(&mut v));
    }

    /// Mirrors TS "rejects hysteresis wider than a band gap". Default gaps are
    /// hard−soft = 0.1 and critical−hard = 0.15 → min gap 0.1; hysteresis 0.25
    /// traps the controller in the higher band.
    #[test]
    fn validate_bands_rejects_hysteresis_wider_than_a_band_gap() {
        let mut cfg = LoadObserverConfig::default();
        cfg.band_hysteresis = 0.25;
        let mut v = Vec::new();
        assert!(!cfg.validate_bands(&mut v));
        assert!(v.iter().any(|m| m.contains("band_hysteresis")));
    }

    /// Hysteresis exactly equal to the min band gap is rejected (half-open
    /// upper bound `[0, min gap)`): the exit threshold then coincides with the
    /// lower band's enter threshold, so `compute_band` never leaves the band.
    /// Uses thresholds whose gaps are exactly f64-representable so the boundary
    /// is tested cleanly (0.75−0.6 is 0.15000000000000002 in f64, which would
    /// make `band_hysteresis = 0.15` strictly *less* than the min gap and admit).
    #[test]
    fn validate_bands_rejects_hysteresis_equal_to_min_gap() {
        let mut cfg = LoadObserverConfig::default();
        cfg.elu_soft = 0.1;
        cfg.elu_hard = 0.3; // hard−soft = 0.2
        cfg.elu_critical = 0.5; // critical−hard = 0.2 → min gap = 0.2
        cfg.band_hysteresis = 0.2;
        let mut v = Vec::new();
        assert!(!cfg.validate_bands(&mut v));
    }

    /// Mirrors TS "rejects negative hysteresis".
    #[test]
    fn validate_bands_rejects_negative_hysteresis() {
        let mut cfg = LoadObserverConfig::default();
        cfg.band_hysteresis = -0.01;
        let mut v = Vec::new();
        assert!(!cfg.validate_bands(&mut v));
    }

    /// A fully reversed ordering trips the ordering rule (it does not silently
    /// pass). Both elu ordering and hysteresis errors can surface together.
    #[test]
    fn validate_bands_rejects_fully_reversed_ordering() {
        let mut cfg = LoadObserverConfig::default();
        cfg.elu_soft = 0.9;
        cfg.elu_hard = 0.6;
        cfg.elu_critical = 0.3;
        let mut v = Vec::new();
        assert!(!cfg.validate_bands(&mut v));
        assert!(v.iter().any(|m| m.contains("elu_soft < elu_hard < elu_critical")));
    }
}
