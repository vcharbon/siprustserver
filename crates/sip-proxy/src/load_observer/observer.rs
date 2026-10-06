//! [`WorkerLoadObserver`] — one AIMD [`WorkerState`] bucket per worker: payload
//! ingestion, the AIMD cap ladder, the token-bucket admit gate, the stale
//! sweep, and the diagnostics snapshot. Band classification does NOT live here
//! — see [`super::band`]; the `X-Overload` value codec is [`super::payload`].

use std::collections::HashMap;
use std::sync::Mutex;

use load_shed::{at_ms, Ewma, TokenBucket};

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

/// One AIMD bucket's state, as the tests read it.
#[cfg(test)]
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

/// Per-`(LB, worker)` AIMD bucket. One per worker an OPTIONS reply came from,
/// with a payload ([`apply_payload`](WorkerLoadObserver::apply_payload)) or
/// without one ([`note_payload_missing`](WorkerLoadObserver::note_payload_missing)). All time fields are
/// epoch-ms, fed in via `now_ms` — never read from the wall clock here.
#[derive(Debug, Clone)]
struct WorkerState {
    /// The admit gate. Its capacity is the AIMD cap, which is also its refill
    /// rate per second; [`WorkerState::set_cap`] is the one write.
    bucket: TokenBucket,
    cooldown_until_ms: i64,
    last_action: AimdAction,

    elu: f64,
    gc: f64,
    band: EluBand,

    last_adm: f64,
    last_adm_at_ms: i64,
    worker_treated_rate_cps: f64,

    own_admitted_since_last_tick: u64,
    /// This LB's own admit rate to the worker, smoothed so a single quiet tick
    /// does not crash it to 0 (α = 0.3 on the new sample).
    own_admitted_rate_cps: Ewma,
    last_own_rate_tick_at_ms: i64,

    last_payload_at_ms: i64,
    payload_missing_count: u64,
}

impl WorkerState {
    fn fresh(config: &LoadObserverConfig, now_ms: i64) -> Self {
        Self {
            bucket: TokenBucket::full(
                config.cap_initial_cps,
                config.cap_initial_cps,
                at_ms(now_ms),
            ),
            cooldown_until_ms: 0,
            last_action: AimdAction::Init,
            elu: 0.0,
            gc: 0.0,
            band: EluBand::BelowSoft,
            last_adm: 0.0,
            last_adm_at_ms: now_ms,
            worker_treated_rate_cps: 0.0,
            own_admitted_since_last_tick: 0,
            own_admitted_rate_cps: Ewma::starting_at(0.3, 0.0),
            last_own_rate_tick_at_ms: now_ms,
            last_payload_at_ms: now_ms,
            payload_missing_count: 0,
        }
    }

    /// The AIMD cap (cps).
    fn cap(&self) -> f64 {
        self.bucket.capacity()
    }

    /// Set the AIMD cap at `now_ms`: the bucket's capacity and refill rate.
    /// Time before `now_ms` is credited at the old cap; the level is clamped
    /// to the new one.
    fn set_cap(&mut self, cap: f64, now_ms: i64) {
        self.bucket.set_rate(cap, cap, at_ms(now_ms));
    }
}

/// Tracks one AIMD `WorkerState` per worker. Interior mutability via `Mutex` —
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

    /// Fold this LB's admits since the last tick into its smoothed own rate.
    fn update_own_rate(state: &mut WorkerState, now_ms: i64) {
        let dt_sec = (now_ms - state.last_own_rate_tick_at_ms) as f64 / 1000.0;
        if dt_sec <= 0.0 {
            return;
        }
        let observed = state.own_admitted_since_last_tick as f64 / dt_sec;
        state.own_admitted_rate_cps.observe(observed);
        state.own_admitted_since_last_tick = 0;
        state.last_own_rate_tick_at_ms = now_ms;
    }

    /// The AIMD step. Recomputes the band, then: `above_critical` → pin to
    /// floor + arm cooldown; `hard_to_critical` → multiplicative decrease + arm
    /// cooldown; `soft_to_hard` → hold; `below_soft` → additive increase iff
    /// the cooldown has elapsed (else suppress).
    fn apply_aimd_step(&self, worker_id: &str, state: &mut WorkerState, elu: f64, now_ms: i64) {
        let c = &self.config;
        let previous = state.band;
        let new_band = compute_band(c, elu, state.band);
        state.band = new_band;

        match new_band {
            EluBand::AboveCritical => {
                state.set_cap(c.cap_floor_cps, now_ms);
                state.cooldown_until_ms = now_ms + self.cooldown_ms;
                state.last_action = AimdAction::DecreaseCritical;
            }
            EluBand::HardToCritical => {
                state.set_cap(c.cap_floor_cps.max(state.cap() * c.aimd_decrease_factor), now_ms);
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
                    let cap = c.cap_ceiling_cps.min(state.cap() + c.aimd_increase_step_cps);
                    state.set_cap(cap, now_ms);
                    state.last_action = AimdAction::Increase;
                }
            }
        }

        // A band change is a rare state transition (hysteresis keeps it from
        // flapping), so it gets its own line with the load that caused it —
        // emitted AFTER the AIMD step, because `cap_cps` is the number the line
        // exists to explain and the step above is what sets it. Read before the
        // step, an `AboveCritical` entry would report the cap the slam replaced.
        if new_band != previous {
            tracing::info!(
                node = observe::node(),
                worker = worker_id,
                from = ?previous,
                to = ?new_band,
                elu,
                cap_cps = state.cap(),
                "worker load band change"
            );
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
        self.apply_aimd_step(worker_id, state, payload.elu, now_ms);
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

    /// Take one token from the worker's bucket. `Err` carries the seconds
    /// until a token, read from the same refill as the failed take: at least
    /// 1, [`ZERO_RATE_WAIT_SEC`](load_shed::ZERO_RATE_WAIT_SEC) at a zero rate.
    /// A worker no payload has arrived from has no bucket and is admitted.
    /// The strategy answers an empty bucket and counts it
    /// (`SelectError::RateCapExhausted`).
    pub fn try_consume_for(&self, worker_id: &str, now_ms: i64) -> Result<(), u32> {
        let mut workers = self.workers.lock().unwrap();
        match workers.get_mut(worker_id) {
            Some(state) => state.bucket.try_take(at_ms(now_ms)),
            None => Ok(()),
        }
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
    /// `bucket_empty` rejection is counted at the strategy boundary, not here.
    /// The per-worker smoking gun is the snapshot's
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
            state.set_cap(c.cap_floor_cps.max(state.cap() * c.aimd_decrease_factor), now_ms);
            state.cooldown_until_ms = now_ms + self.cooldown_ms;
            state.last_action = AimdAction::StaleDecrease;
            state.payload_missing_count += 1;
            floored += 1;
        }
        floored
    }

    /// Every bucket's state. Reading it refills each bucket, as the next
    /// consume would, so `tokens` is current.
    #[cfg(test)]
    pub fn snapshot(&self, now_ms: i64) -> Vec<AimdSnapshot> {
        let mut workers = self.workers.lock().unwrap();
        let mut out = Vec::with_capacity(workers.len());
        for (worker_id, state) in workers.iter_mut() {
            let tokens = state.bucket.level(at_ms(now_ms));
            let own_admitted_rate_cps = state.own_admitted_rate_cps.get();
            let total = state.worker_treated_rate_cps;
            let share = if total > 0.0 { own_admitted_rate_cps / total } else { 0.0 };
            out.push(AimdSnapshot {
                worker_id: worker_id.clone(),
                elu: state.elu,
                gc: state.gc,
                band: state.band,
                cap_cps: state.cap(),
                tokens,
                cooldown_ms_remaining: (state.cooldown_until_ms - now_ms).max(0),
                last_action: state.last_action,
                worker_treated_rate_cps: total,
                own_admitted_rate_cps,
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
