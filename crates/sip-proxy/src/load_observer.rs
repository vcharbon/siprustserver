//! [`WorkerLoadObserver`] — per-worker ELU-band classification fed by the
//! `X-Overload` payload workers stamp on OPTIONS replies (port of the
//! **band-classification half** of `WorkerLoadObserver.ts`).
//!
//! **Scope (ADR-0009):** band classification only. The per-worker AIMD
//! token-bucket rate cap (`try_consume_for` / `apply_aimd_step` / cooldown) is
//! **deferred** — the LB filters `above_critical` workers from new-dialog
//! selection but does not rate-cap. The band has hysteresis so a worker
//! oscillating around a threshold doesn't flap.

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

/// Band thresholds + hysteresis (source defaults).
#[derive(Debug, Clone, Copy)]
pub struct LoadObserverConfig {
    pub elu_soft: f64,
    pub elu_hard: f64,
    pub elu_critical: f64,
    pub band_hysteresis: f64,
}

impl Default for LoadObserverConfig {
    fn default() -> Self {
        Self { elu_soft: 0.4, elu_hard: 0.6, elu_critical: 0.75, band_hysteresis: 0.05 }
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

#[derive(Debug, Clone, Copy)]
struct WorkerState {
    elu: f64,
    gc: f64,
    band: EluBand,
    last_payload_at_ms: u64,
}

/// Tracks the latest band per worker. Interior mutability via `Mutex` — only the
/// background OPTIONS path writes and only the LB select reads, both CPU-only.
pub struct WorkerLoadObserver {
    config: LoadObserverConfig,
    workers: Mutex<HashMap<String, WorkerState>>,
}

impl WorkerLoadObserver {
    pub fn new(config: LoadObserverConfig) -> Self {
        Self { config, workers: Mutex::new(HashMap::new()) }
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

    /// Process an `X-Overload` payload from a worker (OPTIONS reply path):
    /// recompute its band and record the observation time.
    pub fn apply_payload(&self, worker_id: &str, payload: &OverloadPayload, now_ms: u64) {
        let mut workers = self.workers.lock().unwrap();
        let prev = workers.get(worker_id).map(|s| s.band).unwrap_or(EluBand::BelowSoft);
        let band = self.compute_band(payload.elu, prev);
        workers.insert(
            worker_id.to_string(),
            WorkerState { elu: payload.elu, gc: payload.gc, band, last_payload_at_ms: now_ms },
        );
    }

    /// The worker's current band, or `None` if no payload has ever arrived.
    pub fn band_for(&self, worker_id: &str) -> Option<EluBand> {
        self.workers.lock().unwrap().get(worker_id).map(|s| s.band)
    }

    /// Latest observed `(elu, gc)` for a worker — for metrics/tests.
    pub fn observed(&self, worker_id: &str) -> Option<(f64, f64)> {
        self.workers.lock().unwrap().get(worker_id).map(|s| (s.elu, s.gc))
    }

    /// ms since the last payload from a worker (for staleness metrics).
    pub fn payload_age_ms(&self, worker_id: &str, now_ms: u64) -> Option<u64> {
        self.workers.lock().unwrap().get(worker_id).map(|s| now_ms.saturating_sub(s.last_payload_at_ms))
    }

    /// Drop state for workers that left the registry — the map stays bounded
    /// under worker churn (nothing else ever removes an entry).
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        self.workers.lock().unwrap().retain(|id, _| keep(id));
    }

    /// Forget a worker's state entirely. A recreated pod (same ordinal, new
    /// host) must be judged from scratch — inheriting the dead pod's band
    /// (e.g. `AboveCritical` at the moment it crashed) excluded the idle fresh
    /// pod from new-dialog selection until its first `X-Overload` reply.
    pub fn reset(&self, worker_id: &str) {
        self.workers.lock().unwrap().remove(worker_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs() -> WorkerLoadObserver {
        WorkerLoadObserver::new(LoadObserverConfig::default())
    }

    fn payload(elu: f64) -> OverloadPayload {
        OverloadPayload { elu, gc: 0.0, adm: 0.0 }
    }

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

    #[test]
    fn band_thresholds() {
        let o = obs();
        o.apply_payload("w", &payload(0.3), 0);
        assert_eq!(o.band_for("w"), Some(EluBand::BelowSoft));
        o.apply_payload("w", &payload(0.5), 1);
        assert_eq!(o.band_for("w"), Some(EluBand::SoftToHard));
        o.apply_payload("w", &payload(0.7), 2);
        assert_eq!(o.band_for("w"), Some(EluBand::HardToCritical));
        o.apply_payload("w", &payload(0.9), 3);
        assert_eq!(o.band_for("w"), Some(EluBand::AboveCritical));
    }

    #[test]
    fn hysteresis_holds_higher_band_until_below_enter_minus_h() {
        let o = obs();
        o.apply_payload("w", &payload(0.9), 0); // above_critical
        // elu_critical=0.75, h=0.05 → stays above_critical until elu <= 0.70.
        o.apply_payload("w", &payload(0.73), 1);
        assert_eq!(o.band_for("w"), Some(EluBand::AboveCritical));
        o.apply_payload("w", &payload(0.69), 2);
        assert_eq!(o.band_for("w"), Some(EluBand::HardToCritical));
    }

    #[test]
    fn unknown_worker_has_no_band() {
        assert!(obs().band_for("nope").is_none());
    }

    #[test]
    fn retain_drops_departed_workers() {
        let o = obs();
        o.apply_payload("w0", &payload(0.5), 1);
        o.apply_payload("w1", &payload(0.5), 1);
        o.retain(|id| id == "w0");
        assert!(o.band_for("w0").is_some());
        assert!(o.band_for("w1").is_none(), "departed worker state must be dropped");
    }

    #[test]
    fn reset_clears_inherited_state_for_a_recreated_pod() {
        let o = obs();
        o.apply_payload("w0", &payload(0.99), 1); // crashed while AboveCritical
        assert_eq!(o.band_for("w0"), Some(EluBand::AboveCritical));
        o.reset("w0");
        assert!(o.band_for("w0").is_none(), "the fresh pod must be judged from scratch");
    }
}
