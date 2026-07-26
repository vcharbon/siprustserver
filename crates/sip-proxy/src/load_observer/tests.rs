//! Observer-driven suite: band transitions, the AIMD ladder, the token bucket,
//! counter math, the stale sweep, diagnostics, and Retry-After. Pure-function
//! tests (payload codec, `validate_bands`) live inline in their own modules.
//!
//! The AIMD ladder is a pure state machine driven by an explicit `now_ms`
//! (epoch-ms), so these tests need NO paused tokio runtime — they pass literal
//! timestamps. They are pure CPU, sub-millisecond, default-lane (CLAUDE.md
//! test-runtime policy).

use super::*;
use std::collections::HashMap;

const W: &str = "worker-A";

fn payload(elu: f64) -> OverloadPayload {
    OverloadPayload { elu, gc: 0.0, adm: 0.0 }
}
fn payload_adm(elu: f64, adm: f64) -> OverloadPayload {
    OverloadPayload { elu, gc: 0.0, adm }
}

/// Build an observer over `default()` with the given field overrides applied
/// via a closure.
fn obs_with(f: impl FnOnce(&mut LoadObserverConfig)) -> WorkerLoadObserver {
    let mut cfg = LoadObserverConfig::default();
    f(&mut cfg);
    WorkerLoadObserver::new(cfg)
}
fn obs() -> WorkerLoadObserver {
    WorkerLoadObserver::new(LoadObserverConfig::default())
}

/// Pin band thresholds so the AIMD tests land in a known band regardless of
/// operational-default tuning.
fn bands_cfg(cfg: &mut LoadObserverConfig) {
    cfg.elu_soft = 0.6;
    cfg.elu_hard = 0.8;
    cfg.elu_critical = 0.95;
}

fn snap1(o: &WorkerLoadObserver, now_ms: i64) -> AimdSnapshot {
    o.snapshot(now_ms).into_iter().next().expect("one worker")
}

// ── band derivation + hysteresis ──────────────────────────────────────────

/// Pinned to explicit thresholds (`bands_cfg`: soft 0.6 / hard 0.8 / critical
/// 0.95) so it exercises the band classifier regardless of operational-default
/// tuning.
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

/// The hold-on-decrease branch of `compute_band` for the hard band:
/// `band_thresholds` only exercises that arm in the *increasing* direction;
/// this pins the hysteresis-hold direction (once in hard_to_critical, elu must
/// drop below hard − h to exit).
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

// ── AIMD increase ladder ──────────────────────────────────────────────────

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

// ── AIMD decrease + cooldown ──────────────────────────────────────────────

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

// ── CRITICAL filter behaviour ─────────────────────────────────────────────

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

// ── token bucket ──────────────────────────────────────────────────────────

#[test]
fn unknown_worker_is_admitted_bootstrap_friendly() {
    assert!(obs().try_consume_for("unknown-worker", 1000));
}

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

// ── counter math ──────────────────────────────────────────────────────────

#[test]
fn worker_treated_rate_is_adm_delta_over_dt_in_cps() {
    let o = obs();
    o.apply_payload(W, &payload_adm(0.5, 0.0), 0);
    // 100 admits over 1s → 100 cps treated rate.
    o.apply_payload(W, &payload_adm(0.5, 100.0), 1000);
    assert!((snap1(&o, 1000).worker_treated_rate_cps - 100.0).abs() < 1e-5);
}

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

// ── stale-payload sweep ───────────────────────────────────────────────────

#[test]
fn sweep_below_stale_threshold_is_a_no_op() {
    // Seed elu=0.7 (hold band under bands_cfg) so the initial apply_payload
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

// ── diagnostics ───────────────────────────────────────────────────────────

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

// ── retry-after (powers SelectError::RateCapExhausted) ────────────────────

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
