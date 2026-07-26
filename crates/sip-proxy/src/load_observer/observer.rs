//! [`WorkerLoadObserver`] — one AIMD [`WorkerState`] bucket per worker: payload
//! ingestion, the AIMD cap ladder, the token-bucket admit gate, the stale
//! sweep, and the diagnostics snapshot. Band classification does NOT live here
//! — see [`super::band`]; the `X-Overload` value codec is [`super::payload`].

use std::collections::HashMap;
use std::sync::Mutex;

use super::band::{compute_band, EluBand};
use super::config::LoadObserverConfig;
use super::payload::OverloadPayload;

/// The last AIMD action taken on a worker — for snapshots + diagnostics.
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

/// Full per-worker snapshot — the diagnostics view of one AIMD bucket. Only
/// tests consume it today; the per-worker Prometheus surface is a deferred
/// slice (`ProxyMetrics` is registry-aggregate).
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

/// Tracks one AIMD [`WorkerState`] per worker. Interior mutability via `Mutex` —
/// the background OPTIONS path writes (`apply_payload`/`sweep_stale`) and the LB
/// select path reads + consumes (`band_for`/`try_consume_for`), all CPU-only.
pub struct WorkerLoadObserver {
    config: LoadObserverConfig,
    /// Cached `aimd_cooldown_ticks * options_interval_ms` (ms a decrease blocks
    /// the next increase). Computed once at construction.
    cooldown_ms: i64,
    workers: Mutex<HashMap<String, WorkerState>>,
}

impl WorkerLoadObserver {
    pub fn new(config: LoadObserverConfig) -> Self {
        let cooldown_ms = (config.aimd_cooldown_ticks * config.options_interval_ms as f64) as i64;
        Self { config, cooldown_ms, workers: Mutex::new(HashMap::new()) }
    }

    /// Lazy refill: accrue `cap` tokens/sec for the elapsed time since the last
    /// refill, capped at `cap`. A no-op when no time has passed, so reading the
    /// snapshot or consuming repeatedly at the same `now_ms` doesn't over-fill.
    fn refill_bucket(state: &mut WorkerState, now_ms: i64) {
        let dt_sec = (now_ms - state.last_refill_at_ms) as f64 / 1000.0;
        if dt_sec <= 0.0 {
            return;
        }
        state.tokens = state.cap.min(state.tokens + state.cap * dt_sec);
        state.last_refill_at_ms = now_ms;
    }

    /// EWMA-smooth this LB's own admit rate to the worker so a single quiet tick
    /// doesn't crash the rate to 0 (α = 0.3 on the new sample).
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

    /// The AIMD step. Recomputes the band, then: `above_critical` → pin to
    /// floor + arm cooldown; `hard_to_critical` → multiplicative decrease + arm
    /// cooldown; `soft_to_hard` → hold; `below_soft` → additive increase iff
    /// the cooldown has elapsed (else suppress).
    fn apply_aimd_step(&self, state: &mut WorkerState, elu: f64, now_ms: i64) {
        let c = &self.config;
        let new_band = compute_band(c, elu, state.band);
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
    /// counter), so the baseline is reset and the rate zeroed.
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

    /// Process an `X-Overload` payload from a worker (OPTIONS reply path):
    /// refresh the rate diffs, stash `elu`/`gc`, mark the payload fresh, then
    /// run one AIMD step.
    pub fn apply_payload(&self, worker_id: &str, payload: &OverloadPayload, now_ms: i64) {
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

    /// An OPTIONS reply arrived without a usable `X-Overload` header — tracked as
    /// a per-worker counter but NO AIMD step is taken (the band/cap are left where
    /// the last good payload put them).
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
    /// (no bucket yet — bootstrap admits without one).
    pub fn record_own_admitted(&self, worker_id: &str) {
        if let Some(state) = self.workers.lock().unwrap().get_mut(worker_id) {
            state.own_admitted_since_last_tick += 1;
        }
    }

    /// Attempt to consume one token from the worker's bucket. `true` ⇒ admitted
    /// (token spent); `false` ⇒ the bucket is empty. An **unknown worker is
    /// admitted** (we don't gate workers we've never observed a payload from —
    /// bootstrap-friendly).
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
            // counted there (load_balancer.rs); a per-worker-labelled rejection
            // counter awaits a per-worker Prometheus surface (`ProxyMetrics` is
            // registry-aggregate today).
            false
        }
    }

    /// Seconds until ≥ 1 token will be available for this worker (`0` if available
    /// now, or for an unknown worker). With an empty bucket and a non-positive cap
    /// (the floor is `>= 1` by config, so this is defensive) returns `60` as a
    /// fallback.
    ///
    /// This is a *real* per-bucket Retry-After derived from the worker's own
    /// cap/fill rate — the strongest signal the LB can give the UAC (a near-full
    /// bucket retries in ~1 s; a floored one waits longer), rather than a
    /// hard-coded constant. `select_for_new_dialog` feeds this into
    /// `SelectError::RateCapExhausted` (clamped to `>= 1`, so the wire value is
    /// never a no-op `Retry-After: 0`).
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
    /// `payload_missing_count++`.
    ///
    /// Returns the number of workers floored this sweep, so the caller can feed a
    /// coarse `stale_decrease` aggregate counter — the observer itself stays pure
    /// (no `ProxyMetrics` dependency, no clock), mirroring how the per-worker
    /// `bucket_empty` rejection is counted at the strategy boundary, not here. The
    /// per-worker `worker_id`-labelled push is a deferred slice; the per-worker
    /// smoking gun is preserved meanwhile in the snapshot's
    /// `last_action = StaleDecrease` + `payload_missing_count`.
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

    /// Full per-worker snapshot (diagnostics, tests). Reading the snapshot also
    /// lazily refills each bucket so the `tokens` field isn't stale (the same
    /// refill the next consume would do).
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
