use super::*;

#[test]
fn default_config_validates() {
    assert!(B2buaConfig::default().validate().is_ok());
}

#[test]
fn the_limiter_refresh_and_release_queue_bounds_are_checked() {
    let with = |f: fn(&mut B2buaConfig)| {
        let mut c = B2buaConfig::default();
        f(&mut c);
        c.validate()
    };
    let e = with(|c| c.limiter_refresh_sec = 0).expect_err("zero refresh period");
    assert!(e.contains("limiter_refresh_sec=0"), "{e}");
    let e = with(|c| c.limiter_refresh_batch_ms = 0).expect_err("zero tick");
    assert!(e.contains("limiter_refresh_batch_ms=0"), "{e}");
    let e = with(|c| c.limiter_refresh_batch_ms = 40_000).expect_err("tick at the period");
    assert!(e.contains("limiter_refresh_batch_ms=40000"), "{e}");
    assert!(
        with(|c| c.limiter_refresh_sec = 600).is_ok(),
        "the lease is the limiter's: boot does not know it"
    );
    let e = with(|c| c.limiter_refresh_batch_max = 0).expect_err("zero batch");
    assert!(e.contains("limiter_refresh_batch_max=0"), "{e}");
    let e = with(|c| c.limiter_release_queue_cap = 0).expect_err("zero cap");
    assert!(e.contains("limiter_release_queue_cap=0"), "{e}");
    let e = with(|c| c.limiter_breaker_failures = 0).expect_err("zero failures");
    assert!(e.contains("limiter_breaker_failures=0"), "{e}");
    let e = with(|c| c.limiter_breaker_probe_ms = 0).expect_err("zero probe period");
    assert!(e.contains("limiter_breaker_probe_ms=0"), "{e}");
}

/// New calls hold half the handler permits and stop 5 % below the queue
/// cap by default; each share is a bounded percentage.
#[test]
fn the_new_call_thresholds_are_shares_of_their_pools() {
    let c = B2buaConfig::default();
    assert_eq!((c.new_call_permits(), c.new_call_queue_headroom()), (512, 10_000));
    let tiny = B2buaConfig { event_dispatch_concurrency: 1, per_call_queue_cap: 1, ..c.clone() };
    assert_eq!(
        (tiny.new_call_permits(), tiny.new_call_queue_headroom()),
        (1, 0),
        "a new call keeps one permit and the last queue"
    );
    for (share, headroom, ok) in [(1, 0, true), (100, 99, true), (0, 5, false), (101, 5, false)] {
        let c = B2buaConfig {
            new_call_permit_share_percent: share,
            new_call_queue_headroom_percent: headroom,
            ..Default::default()
        };
        assert_eq!(c.validate().is_ok(), ok, "share {share} headroom {headroom}");
    }
    let c = B2buaConfig { new_call_queue_headroom_percent: 100, ..Default::default() };
    let e = c.validate().expect_err("a headroom of the whole cap");
    assert!(e.contains("new_call_queue_headroom_percent=100"), "{e}");
}

#[test]
fn rejects_nonpositive_or_nan_panic_elu() {
    for bad in [0.0, -0.5, f64::NAN, f64::INFINITY] {
        let c = B2buaConfig { overload_panic_elu_threshold: bad, ..Default::default() };
        let e = c.validate().expect_err("non-positive/NaN panic-ELU must be rejected");
        assert!(e.contains("overload_panic_elu_threshold"), "msg was: {e}");
    }
}

#[test]
fn allows_panic_elu_at_or_above_one_as_disable() {
    // `>= 1.0` is the documented way to disable the backstop (clamped ELU
    // never exceeds 1) — must NOT be rejected.
    for ok in [1.0, 1.5] {
        let c = B2buaConfig { overload_panic_elu_threshold: ok, ..Default::default() };
        assert!(c.validate().is_ok(), "{ok} should validate");
    }
}

#[test]
fn rejects_zero_cps_rate_when_gate_enabled() {
    let c = B2buaConfig { cps_bucket_size: 1000, cps_bucket_rate: 0, ..Default::default() };
    let e = c.validate().expect_err("zero refill with gate on must be rejected");
    assert!(e.contains("cps_bucket_rate"), "msg was: {e}");
}

#[test]
fn allows_zero_cps_rate_when_gate_disabled() {
    // size == 0 disables the hard CPS gate, so a 0 refill rate is moot.
    let c = B2buaConfig { cps_bucket_size: 0, cps_bucket_rate: 0, ..Default::default() };
    assert!(c.validate().is_ok());
}

#[test]
fn rejects_setup_deadline_at_or_above_the_txn_bound() {
    // 300 s >= 158 s: the txn layer would CANCEL the b-leg before the app
    // gives up (the ordering inversion) — refuse to start.
    let c = B2buaConfig {
        setup_timeout_ms: 300_000,
        invite_txn_timeout_sec: 158,
        ..Default::default()
    };
    let e = c.validate().expect_err("setup >= txn bound must be rejected");
    assert!(e.contains("setup_timeout_ms"), "msg was: {e}");
}

#[test]
fn rejects_txn_bound_outside_the_supported_range() {
    for bad in [700, 20] {
        let c =
            B2buaConfig { invite_txn_timeout_sec: bad, setup_timeout_ms: 0, ..Default::default() };
        let e = c.validate().expect_err("out-of-range invite_txn_timeout_sec must be rejected");
        assert!(e.contains("invite_txn_timeout_sec"), "msg was: {e}");
    }
}

#[test]
fn first_response_bound_is_a_deadline_between_two_rungs_and_timer_b() {
    // Default is Timer B itself: no deviation until a deployment asks for
    // one.
    assert_eq!(B2buaConfig::default().invite_first_response_timeout_sec, 32);
    assert_eq!(B2buaConfig::default().invite_first_response_timeout_ms(), 32_000);
    // 1 s would send the INVITE effectively once; 33 s is above the RFC
    // value the class already gives up at.
    for bad in [1, 33, 0, -5] {
        let c = B2buaConfig { invite_first_response_timeout_sec: bad, ..Default::default() };
        let e = c
            .validate()
            .expect_err("out-of-range invite_first_response_timeout_sec must be rejected");
        assert!(e.contains("invite_first_response_timeout_sec"), "msg was: {e}");
    }
    // The floor (two re-sends) and the ceiling (Timer B) are both allowed.
    for ok in [2, 5, 32] {
        let c = B2buaConfig { invite_first_response_timeout_sec: ok, ..Default::default() };
        assert!(c.validate().is_ok(), "{ok} s must be accepted");
        assert_eq!(c.invite_first_response_timeout_ms(), ok as u64 * 1000);
    }
    // A harness config that writes a non-positive value anyway arms the
    // Timer B default, never a degenerate bound.
    let c = B2buaConfig { invite_first_response_timeout_sec: 0, ..Default::default() };
    assert_eq!(c.invite_first_response_timeout_ms(), 32_000);
}

#[test]
fn allows_setup_deadline_strictly_below_a_raised_txn_bound() {
    let c = B2buaConfig {
        setup_timeout_ms: 300_000,
        invite_txn_timeout_sec: 400,
        ..Default::default()
    };
    assert!(c.validate().is_ok());
}

#[test]
fn disabled_setup_deadline_skips_the_ordering_check() {
    // <= 0 disables the app deadline; only the range check applies.
    let c = B2buaConfig { setup_timeout_ms: 0, invite_txn_timeout_sec: 158, ..Default::default() };
    assert!(c.validate().is_ok());
}

#[test]
fn clamps_no_answer_above_the_margin_ceiling() {
    let c = B2buaConfig { invite_txn_timeout_sec: 200, ..Default::default() };
    // Above `bound − margin` → held to the ceiling; at/under it → untouched.
    assert_eq!(c.clamp_no_answer_ms(250_000), 192_000);
    assert_eq!(c.clamp_no_answer_ms(200_000), 192_000);
    // Inside the margin band (192 < 195 < 200): equally clamped — a value
    // there would strand the CANCEL→487 exchange just the same.
    assert_eq!(c.clamp_no_answer_ms(195_000), 192_000);
    assert_eq!(c.clamp_no_answer_ms(192_000), 192_000);
    assert_eq!(c.clamp_no_answer_ms(191_000), 191_000);
    assert_eq!(c.clamp_no_answer_ms(30_000), 30_000);
}

#[test]
fn derived_reaper_idle_window_is_floored_above_the_txn_bound() {
    // 3 × keepalive (360 s) would undercut a 400 s ring: the derived window
    // must floor at bound + one keepalive interval.
    let c = B2buaConfig {
        invite_txn_timeout_sec: 400,
        keepalive_interval_sec: 120,
        reaper_idle_max_sec: 0,
        ..Default::default()
    };
    assert_eq!(c.reaper_idle_max_ms(), 520_000);
    // Default bound (158 s): the 3× derivation already clears it — unchanged.
    let d = B2buaConfig::default();
    assert_eq!(d.reaper_idle_max_ms(), 3 * d.keepalive_interval_sec * 1000);
}

#[test]
fn rejects_explicit_reaper_idle_window_at_or_under_the_txn_bound() {
    // keepalive 150 keeps the 2×-keepalive rule satisfied (400 ≥ 300), so
    // the txn-bound rule is what rejects the 400 s window against a 400 s
    // ring.
    let c = B2buaConfig {
        invite_txn_timeout_sec: 400,
        setup_timeout_ms: 300_000,
        keepalive_interval_sec: 150,
        reaper_idle_max_sec: 400,
        ..Default::default()
    };
    let e = c.validate().expect_err("an idle window a ringing call can outlive must be rejected");
    assert!(e.contains("invite_txn_timeout_sec"), "msg was: {e}");
    let ok = B2buaConfig { reaper_idle_max_sec: 401, ..c };
    assert!(ok.validate().is_ok());
}

#[test]
fn rejects_a_nonpositive_ack_deadline() {
    // RFC 3261 §13.3.1.4: the give-up is a deadline, not a switch — no
    // value may configure an un-ACKed 2xx that never ends the session.
    for bad in [0, -5] {
        let c = B2buaConfig { ack_timeout_sec: bad, ..Default::default() };
        let e = c.validate().expect_err("a non-positive ACK deadline must be rejected");
        assert!(e.contains("ack_timeout_sec"), "msg was: {e}");
    }
    assert!(B2buaConfig { ack_timeout_sec: 1, ..Default::default() }.validate().is_ok());
}

#[test]
fn nonpositive_ack_deadline_falls_back_to_the_default_ms() {
    // A harness config that writes 0 anyway arms the 32 s default, never a
    // ladder whose give-up tears nothing down.
    for bad in [0, -5] {
        let c = B2buaConfig { ack_timeout_sec: bad, ..Default::default() };
        assert_eq!(c.ack_timeout_ms(), 32_000);
    }
    assert_eq!(B2buaConfig { ack_timeout_sec: 6, ..Default::default() }.ack_timeout_ms(), 6_000);
}

#[test]
fn nonpositive_txn_bound_falls_back_to_the_default_ms() {
    // 0 is NOT "disabled" for the transaction bound (validate refuses it at
    // boot); a harness config that writes it anyway gets the 158 s default,
    // never a degenerate near-zero bound.
    let c = B2buaConfig { invite_txn_timeout_sec: 0, ..Default::default() };
    assert_eq!(c.invite_txn_timeout_ms(), 158_000);
}

fn with_capacity(capacity: CapacityConfig) -> B2buaConfig {
    B2buaConfig { capacity, ..Default::default() }
}

#[test]
fn capacity_ceilings_are_off_by_default_and_validate() {
    assert_eq!(B2buaConfig::default().capacity, CapacityConfig::default());
    let ok = CapacityConfig {
        calls: Ceilings { normal: Some(10), emergency: Some(12) },
        transactions: Ceilings { normal: Some(100), emergency: None },
        rss_bytes: Ceilings { normal: None, emergency: Some(1 << 30) },
        backup_calls: Some(10),
        backup_rss_bytes: Some(1 << 29),
    };
    assert_eq!(with_capacity(ok).validate(), Ok(()));
}

#[test]
fn an_emergency_ceiling_below_the_normal_one_is_refused() {
    let c = with_capacity(CapacityConfig {
        calls: Ceilings { normal: Some(10), emergency: Some(9) },
        ..Default::default()
    });
    let err = c.validate().unwrap_err();
    assert!(err.contains("capacity.calls"), "{err}");
}

#[test]
fn with_the_reaper_off_the_lifetime_message_cap_is_not_checked() {
    let c = B2buaConfig {
        reaper_enabled: false,
        max_messages_per_call_lifetime: 0,
        ..Default::default()
    };
    assert!(c.validate().is_ok(), "the cap is not installed without the reaper");
}

#[test]
fn a_lifetime_message_cap_of_zero_is_refused() {
    let c = B2buaConfig { max_messages_per_call_lifetime: 0, ..Default::default() };
    let err = c.validate().unwrap_err();
    assert!(err.contains("max_messages_per_call_lifetime"), "{err}");
}

#[test]
fn a_zero_ceiling_is_refused() {
    for capacity in [
        CapacityConfig {
            rss_bytes: Ceilings { normal: Some(0), emergency: None },
            ..Default::default()
        },
        CapacityConfig { backup_calls: Some(0), ..Default::default() },
    ] {
        assert!(with_capacity(capacity).validate().is_err(), "{capacity:?}");
    }
}

#[test]
fn a_class_meets_its_own_ceiling() {
    let both = Ceilings { normal: Some(10), emergency: Some(12) };
    assert_eq!(both.for_class(false), Some(10));
    assert_eq!(both.for_class(true), Some(12));
    let hard_only = Ceilings { normal: None, emergency: Some(12) };
    assert_eq!(hard_only.for_class(false), Some(12), "the hard ceiling bounds everyone");
    let soft_only = Ceilings { normal: Some(10), emergency: None };
    assert_eq!(soft_only.for_class(true), None);
}

#[test]
fn a_retry_after_range_picks_inside_its_bounds() {
    let range = RetryAfterRange { min_sec: 0, max_sec: 10 };
    assert_eq!(range.pick(0), 0);
    assert_eq!(range.pick(10), 10);
    assert_eq!(range.pick(11), 0, "the roll wraps over the 11 values");
    for roll in [1u64, 7, 12_345, u64::MAX] {
        assert!((0..=10).contains(&range.pick(roll)), "roll={roll}");
    }
    assert_eq!(RetryAfterRange { min_sec: 2, max_sec: 2 }.pick(u64::MAX), 2);
    let widest = RetryAfterRange { min_sec: 0, max_sec: u32::MAX };
    assert_eq!(widest.pick(u64::from(u32::MAX)), u32::MAX, "no overflow at the widest");
}

#[test]
fn an_empty_glare_retry_after_range_is_refused() {
    let c = B2buaConfig {
        glare_retry_after: Some(RetryAfterRange { min_sec: 5, max_sec: 4 }),
        ..B2buaConfig::default()
    };
    let err = c.validate().expect_err("an empty range");
    assert!(err.contains("glare_retry_after"), "{err}");
    let c = B2buaConfig {
        glare_retry_after: Some(RetryAfterRange { min_sec: 0, max_sec: 10 }),
        ..B2buaConfig::default()
    };
    assert!(c.validate().is_ok(), "{:?}", c.validate());
}
