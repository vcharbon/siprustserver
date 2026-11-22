//! Observer-driven suite: band transitions, the AIMD ladder, the token bucket,
//! counter math, the stale sweep, diagnostics and Retry-After. The payload
//! codec and `validate_bands` tests live in their own modules.
//!
//! The observer is a pure state machine driven by an explicit `now_ms`
//! (epoch-ms), so these tests pass literal timestamps and need no paused
//! runtime.

use super::*;
use std::collections::HashMap;

const W: &str = "worker-A";

fn payload(elu: f64) -> OverloadPayload {
    OverloadPayload { elu, gc: 0.0, adm: 0.0 }
}
fn payload_adm(elu: f64, adm: f64) -> OverloadPayload {
    OverloadPayload { elu, gc: 0.0, adm }
}

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

#[test]
fn hysteresis_holds_higher_band_until_below_enter_minus_h() {
    let o = obs_with(bands_cfg);
    o.apply_payload("w", &payload(0.97), 1000);
    o.apply_payload("w", &payload(0.93), 2000);
    assert_eq!(o.band_for("w"), Some(EluBand::AboveCritical));
    o.apply_payload("w", &payload(0.89), 3000);
    assert_eq!(o.band_for("w"), Some(EluBand::HardToCritical));
}

#[test]
fn hysteresis_holds_hard_to_critical_until_below_hard_minus_h() {
    let o = obs_with(|c| {
        c.elu_soft = 0.6;
        c.elu_hard = 0.8;
        c.elu_critical = 0.95;
        c.band_hysteresis = 0.02;
    });
    o.apply_payload("w", &payload(0.82), 1000);
    assert_eq!(o.band_for("w"), Some(EluBand::HardToCritical));
    o.apply_payload("w", &payload(0.79), 2000);
    assert_eq!(o.band_for("w"), Some(EluBand::HardToCritical));
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
    o.apply_payload("w0", &payload(0.99), 1000);
    assert_eq!(o.band_for("w0"), Some(EluBand::AboveCritical));
    o.reset("w0");
    assert!(o.band_for("w0").is_none(), "the fresh pod must be judged from scratch");
}

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
    for i in 1..=20 {
        o.apply_payload(W, &payload(0.1), i * 1000);
    }
    assert_eq!(snap1(&o, 20_000).cap_cps, 110.0);
}

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
    assert_eq!(snap.cap_cps, 75.0);
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
    o.apply_payload(W, &payload(0.85), 1000);
    assert_eq!(snap1(&o, 1000).cooldown_ms_remaining, 3000);

    o.apply_payload(W, &payload(0.1), 2000);
    let snap = snap1(&o, 2000);
    assert_eq!(snap.last_action, AimdAction::Cooldown);
    assert_eq!(snap.cap_cps, 75.0);

    o.apply_payload(W, &payload(0.1), 5000);
    let snap = snap1(&o, 5000);
    assert_eq!(snap.last_action, AimdAction::Increase);
    assert_eq!(snap.cap_cps, 80.0);
}

/// A cooldown of 2.5 ticks ends half a tick before the third reply is due, so
/// a third reply up to 500 ms early still increases: from the floor, two
/// increases of 50 land on replies 3 and 4.
#[test]
fn a_half_tick_cooldown_lets_an_early_third_reply_increase() {
    let o = obs_with(|c| {
        bands_cfg(c);
        c.cap_floor_cps = 1.0;
        c.aimd_cooldown_ticks = 2.5;
        c.options_interval_ms = 1000;
        c.aimd_increase_step_cps = 50.0;
    });
    o.apply_payload(W, &payload(0.99), 1000);
    assert_eq!(snap1(&o, 1000).cap_cps, 1.0);
    o.apply_payload(W, &payload(0.1), 2100);
    o.apply_payload(W, &payload(0.1), 2900);
    assert_eq!(snap1(&o, 2900).last_action, AimdAction::Cooldown);
    o.apply_payload(W, &payload(0.1), 3600);
    assert_eq!(snap1(&o, 3600).cap_cps, 51.0);
    o.apply_payload(W, &payload(0.1), 4900);
    assert_eq!(snap1(&o, 4900).cap_cps, 101.0);
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
    for i in 0..20 {
        o.apply_payload(W, &payload(0.85), 1000 + i * 4000);
    }
    let snap = snap1(&o, 100_000);
    assert!(snap.cap_cps >= 1.0);
    assert_eq!(snap.cap_cps, 1.0);
}

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

#[test]
fn unknown_worker_is_admitted_bootstrap_friendly() {
    assert!(obs().try_consume_for("unknown-worker", 1000).is_ok());
}

#[test]
fn bucket_starts_full_at_cap_initial_cps_tokens() {
    let o = obs_with(|c| {
        bands_cfg(c);
        c.cap_initial_cps = 10.0;
    });
    o.apply_payload(W, &payload(0.7), 1000);
    for _ in 0..10 {
        assert!(o.try_consume_for(W, 1000).is_ok());
    }
    assert!(o.try_consume_for(W, 1000).is_err());
}

#[test]
fn bucket_refills_at_cap_tokens_per_sec_over_elapsed_time() {
    let o = obs_with(|c| {
        bands_cfg(c);
        c.cap_initial_cps = 10.0;
    });
    o.apply_payload(W, &payload(0.7), 0);
    for _ in 0..10 {
        let _ = o.try_consume_for(W, 0);
    }
    assert!(o.try_consume_for(W, 0).is_err());
    assert!(o.try_consume_for(W, 500).is_ok());
    for _ in 0..4 {
        assert!(o.try_consume_for(W, 500).is_ok());
    }
    assert!(o.try_consume_for(W, 500).is_err());
}

/// Every snapshot's bucket level is within the cap the AIMD ladder set.
fn snap_within_cap(o: &WorkerLoadObserver, now_ms: i64) -> AimdSnapshot {
    let snap = snap1(o, now_ms);
    assert!(snap.tokens <= snap.cap_cps, "tokens {} above cap {}", snap.tokens, snap.cap_cps);
    snap
}

/// A slam to the floor binds the bucket at once: a full bucket of the old cap
/// admits one call, not a hundred.
#[test]
fn a_slam_to_the_floor_binds_the_bucket_at_once() {
    let o = obs_with(|c| {
        bands_cfg(c);
        c.cap_initial_cps = 100.0;
        c.cap_floor_cps = 1.0;
    });
    o.apply_payload(W, &payload(0.99), 1000);
    assert_eq!(snap_within_cap(&o, 1000).cap_cps, 1.0);
    assert!(o.try_consume_for(W, 1000).is_ok());
    assert!(o.try_consume_for(W, 1000).is_err(), "the floor holds one token");
    snap_within_cap(&o, 1000);
}

/// A stale sweep's decrease caps the bucket: six seconds refill a drained
/// bucket to the old cap, the decrease then cuts it to the new one.
#[test]
fn a_stale_decrease_caps_the_bucket() {
    let o = obs_with(|c| {
        bands_cfg(c);
        c.cap_initial_cps = 100.0;
        c.aimd_decrease_factor = 0.75;
        c.payload_stale_ms = 5000;
    });
    o.apply_payload(W, &payload(0.7), 0);
    while o.try_consume_for(W, 0).is_ok() {}
    assert_eq!(o.sweep_stale(6000, |_| true), 1);
    let snap = snap_within_cap(&o, 6000);
    assert_eq!(snap.cap_cps, 75.0);
    assert_eq!(snap.tokens, 75.0);
}

/// An increase credits the time before it at the old cap: one second after a
/// drain at cap 105 holds 105 tokens, though the cap is now 110.
#[test]
fn an_increase_credits_the_time_before_it_at_the_old_cap() {
    let o = obs_with(|c| {
        bands_cfg(c);
        c.cap_initial_cps = 100.0;
        c.aimd_increase_step_cps = 5.0;
    });
    o.apply_payload(W, &payload(0.1), 0);
    assert_eq!(snap_within_cap(&o, 0).cap_cps, 105.0);
    while o.try_consume_for(W, 0).is_ok() {}
    o.apply_payload(W, &payload(0.1), 1000);
    let snap = snap_within_cap(&o, 1000);
    assert_eq!(snap.cap_cps, 110.0);
    assert_eq!(snap.tokens, 105.0);
}

#[test]
fn worker_treated_rate_is_adm_delta_over_dt_in_cps() {
    let o = obs();
    o.apply_payload(W, &payload_adm(0.5, 0.0), 0);
    o.apply_payload(W, &payload_adm(0.5, 100.0), 1000);
    assert!((snap1(&o, 1000).worker_treated_rate_cps - 100.0).abs() < 1e-5);
}

#[test]
fn adm_counter_decrease_worker_restart_resets_baseline() {
    let o = obs();
    o.apply_payload(W, &payload_adm(0.5, 1000.0), 0);
    o.apply_payload(W, &payload_adm(0.5, 1100.0), 1000);
    o.apply_payload(W, &payload_adm(0.5, 50.0), 2000);
    assert_eq!(snap1(&o, 2000).worker_treated_rate_cps, 0.0);
    o.apply_payload(W, &payload_adm(0.5, 100.0), 3000);
    assert!((snap1(&o, 3000).worker_treated_rate_cps - 50.0).abs() < 1e-5);
}

#[test]
fn record_own_admitted_and_share_metric() {
    let o = obs();
    o.apply_payload(W, &payload_adm(0.5, 0.0), 0);
    for _ in 0..30 {
        o.record_own_admitted(W);
    }
    o.apply_payload(W, &payload_adm(0.5, 100.0), 1000);
    let snap = snap1(&o, 1000);
    assert!((snap.own_admitted_rate_cps - 9.0).abs() < 1e-5);
    assert!((snap.share - 0.09).abs() < 1e-5);
}

#[test]
fn sweep_below_stale_threshold_is_a_no_op() {
    let o = obs_with(|c| {
        bands_cfg(c);
        c.payload_stale_ms = 5000;
    });
    o.apply_payload(W, &payload(0.7), 1000);
    assert_eq!(o.sweep_stale(1500, |_| true), 0, "no worker floored below the stale threshold");
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
    assert_eq!(
        o.sweep_stale(6000, |_| true),
        1,
        "the one stale worker is floored (feeds the aggregate counter)"
    );
    let snap = snap1(&o, 6000);
    assert_eq!(snap.last_action, AimdAction::StaleDecrease);
    assert_eq!(snap.cap_cps, 75.0);
    assert_eq!(snap.payload_missing_count, 1);
}

/// A stale payload floors only a worker read alive: a worker not alive gets
/// no new call, and keeps the cap its last payload set.
#[test]
fn sweep_leaves_a_worker_not_alive_as_it_is() {
    let o = obs_with(|c| {
        bands_cfg(c);
        c.cap_initial_cps = 100.0;
        c.payload_stale_ms = 5000;
    });
    o.apply_payload(W, &payload(0.7), 0);
    assert_eq!(o.sweep_stale(6000, |_| false), 0, "nothing floored");
    let snap = snap1(&o, 6000);
    assert_eq!(snap.last_action, AimdAction::Hold);
    assert_eq!(snap.cap_cps, 100.0);
    assert_eq!(snap.payload_missing_count, 0);
}

#[test]
fn note_payload_missing_increments_counter_without_an_aimd_step() {
    let o = obs_with(bands_cfg);
    o.apply_payload(W, &payload(0.7), 1000);
    let before = snap1(&o, 1000);
    o.note_payload_missing(W, 2000);
    o.note_payload_missing(W, 2500);
    let after = snap1(&o, 2500);
    assert_eq!(after.payload_missing_count, 2);
    assert_eq!(after.cap_cps, before.cap_cps);
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

/// An empty bucket's refusal carries a finite, ceil'd Retry-After; an
/// unknown worker is admitted. Pins the value the strategy feeds into
/// `SelectError::RateCapExhausted`.
#[test]
fn an_empty_bucket_refuses_with_its_time_to_a_token() {
    let o = obs_with(|c| {
        bands_cfg(c);
        c.cap_initial_cps = 10.0;
    });
    assert_eq!(o.try_consume_for("unknown", 1000), Ok(()));
    o.apply_payload(W, &payload(0.7), 1000);
    for _ in 0..10 {
        assert_eq!(o.try_consume_for(W, 1000), Ok(()));
    }
    assert_eq!(o.try_consume_for(W, 1000), Err(1));
}

/// The band-change line names the cap the transition produced, read after the
/// AIMD step: an `AboveCritical` entry reports the floor it slammed to, not the
/// cap it replaced.
#[test]
fn the_band_change_line_reports_the_cap_the_transition_produced() {
    let (_guard, log) = observe_testkit::test_buffer();
    let o = obs_with(|c| {
        bands_cfg(c);
        c.cap_initial_cps = 100.0;
        c.cap_floor_cps = 1.0;
    });

    o.apply_payload(W, &payload(0.99), 1000);
    assert_eq!(snap1(&o, 1000).cap_cps, 1.0, "the slam pinned the cap at the floor");

    let lines = log.matching("worker load band change");
    assert_eq!(lines.len(), 1, "one line per transition");
    assert!(lines[0].contains("to=AboveCritical"), "naming the band entered");
    assert!(
        lines[0].line().ends_with("cap_cps=1.0"),
        "the cap AFTER the slam, not the 100.0 it replaced: {}",
        lines[0].line(),
    );
}

/// A worker whose cap the AIMD halved to 3.75 calls/s under the load it
/// reports is handed no more by turning `Alive` again: the restart lowers a
/// cap, never raises one.
#[test]
fn a_cap_restart_never_raises_a_cap_the_aimd_holds_lower() {
    let o = obs();
    for tick in 0..3 {
        o.apply_payload(W, &payload(0.6), tick * 1_000);
    }
    assert_eq!(snap1(&o, 3_000).cap_cps, 3.75, "three decreases from 30");
    o.restart_cap(W, 3_000);
    assert_eq!(snap1(&o, 3_000).cap_cps, 3.75, "the 6 calls/s restart is above it");
}

/// A restarted cap holds at its start share while no new dialog is placed on
/// the worker, however many cool payloads it reports; the first placed
/// releases the AIMD steps.
#[test]
fn a_restarted_cap_climbs_only_once_a_new_dialog_is_placed() {
    let o = obs();
    o.apply_payload(W, &payload(0.1), 0);
    assert_eq!(snap1(&o, 0).cap_cps, 32.0);
    o.restart_cap(W, 1_000);
    for tick in 2..=10 {
        o.apply_payload(W, &payload(0.1), tick * 1_000);
    }
    let held = snap1(&o, 10_000);
    assert_eq!((held.cap_cps, held.last_action), (6.0, AimdAction::Restart));
    assert_eq!(o.try_consume_for(W, 10_500), Ok(()));
    o.apply_payload(W, &payload(0.1), 11_000);
    assert_eq!(snap1(&o, 11_000).cap_cps, 8.0, "one increase step once a dialog was placed");
}
