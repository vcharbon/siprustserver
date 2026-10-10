use super::*;
use crate::timers::TimerService;
use b2bua_sdk::event::CallEvent;
use sip_clock::Clock;
use std::time::Duration;

fn keepalive(fire_at: i64) -> TimerEntry {
    TimerEntry { id: "Keepalive".into(), timer_type: TimerType::Keepalive, fire_at, leg_id: None }
}
fn keepalive_timeout(leg: &str, fire_at: i64) -> TimerEntry {
    TimerEntry {
        id: format!("KeepaliveTimeout:{leg}"),
        timer_type: TimerType::KeepaliveTimeout,
        fire_at,
        leg_id: Some(leg.into()),
    }
}

// The reclaim/takeover hygiene: a snapshot caught mid-keepalive-round-trip
// carries an armed `KeepaliveTimeout`; restoring it verbatim onto the
// reclaiming/taking-over node fires it (its guarded OPTIONS died with the
// crashed node) and BYEs a healthy long hold. The hygiene strips it; the next
// `Keepalive` re-probes fresh. Asserts both the stripping AND that the
// remaining `Keepalive` survives.
#[test]
fn drop_stale_keepalive_timeout_strips_only_the_timeout() {
    let mut timers = vec![
        keepalive(300_000),
        keepalive_timeout("a", 35_000),
        keepalive_timeout("b-1", 35_000),
        TimerEntry {
            id: "GlobalDuration".into(),
            timer_type: TimerType::GlobalDuration,
            fire_at: 3_600_000,
            leg_id: None,
        },
    ];
    drop_stale_keepalive_timeout(&mut timers);
    assert!(
        timers.iter().all(|t| !matches!(t.timer_type, TimerType::KeepaliveTimeout)),
        "every KeepaliveTimeout is stripped from the reclaimed snapshot",
    );
    assert!(
        timers.iter().any(|t| matches!(t.timer_type, TimerType::Keepalive)),
        "the Keepalive (re-probe) timer is kept — the call re-probes on its own schedule",
    );
    assert!(
        timers.iter().any(|t| matches!(t.timer_type, TimerType::GlobalDuration)),
        "unrelated timers (GlobalDuration) are kept",
    );
}

/// A service-owned timer (`TimerType::Service`) rides the restore-hygiene
/// seam GENERICALLY: it is kept (never stripped like `KeepaliveTimeout`),
/// re-anchored by the persisted skew offset like every timer, and left out
/// of the keepalive-only floor/smoothing — so a takeover node restores a
/// downstream service's watchdog (e.g. the 18x deadline) with the same
/// clock-skew bounds as core timers.
#[test]
fn service_timer_survives_restore_hygiene_and_is_reanchored() {
    let svc = TimerType::service(call::MachineId::new("routing"), "deadline18x");
    let now = 1_000_000;
    let skew = 45_000; // receiver_now − origin_now (clears the deadband)
    let raw_fire_at = now - skew + 5_000; // origin-frame deadline
    let mut timers = vec![
        TimerEntry {
            id: svc.timer_id(None),
            timer_type: svc.clone(),
            fire_at: raw_fire_at,
            leg_id: None,
        },
        keepalive_timeout("b-1", now - skew + 1_000),
    ];
    let smoothing = Some(Smoothing { now_ms: now, l_max: 0, speedup: 10, cap_ms: None });
    sanitize_restored_timers(&mut timers, "w1|svc|svc", now, Some(skew), 300_000, smoothing);
    assert_eq!(timers.len(), 1, "service timer kept; stale KeepaliveTimeout stripped");
    assert_eq!(timers[0].timer_type, svc);
    assert_eq!(
        timers[0].fire_at,
        now + 5_000,
        "re-anchored into this node's clock frame exactly like a core timer",
    );
}

// At the timer layer: a *past-due* `KeepaliveTimeout` restored from a
// pre-crash snapshot fires IMMEDIATELY (`restore` clamps `fire_at <= now` to
// a next-tick fire) — this is the event that drives `keepalive-timeout` → BYE
// on a healthy reclaimed call. The stripped set restores NOTHING that fires
// at now=reclaim.
#[tokio::test(start_paused = true)]
async fn past_due_keepalive_timeout_fires_on_restore_without_the_fix() {
    let clock = Clock::test_at(0);
    // now is well past the snapshot's KeepaliveTimeout deadline (the dead
    // node armed it +120 s before its clock, which is in our past).
    tokio::time::advance(Duration::from_millis(200_000)).await;

    // Unstripped: the verbatim snapshot includes the past-due timeout.
    let (timers, mut fire_rx) = TimerService::spawn(clock.clone());
    let snapshot = vec![keepalive(500_000), keepalive_timeout("b-1", 120_000)];
    timers.restore(snapshot.clone(), "w0|cid|tag".into(), "w0|cid|tag#k").await;
    tokio::time::advance(Duration::from_millis(1)).await;
    let fired = fire_rx.recv().await.unwrap();
    match fired {
        CallEvent::Timer { timer_type, .. } => assert_eq!(
            timer_type,
            TimerType::KeepaliveTimeout,
            "BUG: the stale past-due KeepaliveTimeout fires on reclaim → spurious BYE",
        ),
        _ => panic!("expected a timer event"),
    }

    // Stripped: the same snapshot fires NOTHING at reclaim time (the future
    // Keepalive is the only survivor and is far off).
    let (timers2, mut fire_rx2) = TimerService::spawn(clock);
    let mut fixed = snapshot;
    drop_stale_keepalive_timeout(&mut fixed);
    timers2.restore(fixed, "w0|cid|tag2".into(), "w0|cid|tag2#k").await;
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(
        fire_rx2.try_recv().is_err(),
        "FIX: no spurious keepalive-timeout fires on reclaim; the call survives",
    );
}

/// A restored `Keepalive` never lands more than one interval out, on EVERY
/// hydration path. `fire_at` is an absolute deadline minted in a FOREIGN clock
/// frame, so a re-anchor residual over-future-dates it; `smooth_keepalives`
/// then spreads the cohort across `[now, fire_at]`, and every call landing past
/// `now + interval` goes a full extra cadence unprobed — the UAC's keepalive
/// tolerance expires and it BYEs a healthy hold. The ceiling bounds the future
/// side; the defensive floor covers the past-due side.
#[test]
fn restored_keepalive_is_capped_at_one_interval() {
    let now = 1_000_000;
    let interval = 300_000;
    let ceiling = now + interval;
    // Residual frame error: deadlines land 1.5 intervals out instead of ≤ 1.
    let deadline = now + interval + interval / 2;
    let smoothing = Smoothing { now_ms: now, l_max: 0, speedup: 10, cap_ms: None };

    // Bulk reboot sweep (`reclaim_all`): offset already applied + smoothing.
    let mut late = 0;
    let mut fire_ats = std::collections::HashSet::new();
    for i in 0..1000 {
        let call_ref = format!("w1|call-{i}|tag-{i}");
        let mut timers = vec![keepalive(deadline)];
        sanitize_restored_timers(&mut timers, &call_ref, now, Some(0), interval, Some(smoothing));
        if timers[0].fire_at > ceiling {
            late += 1;
        }
        fire_ats.insert(timers[0].fire_at);
    }
    assert_eq!(
        late, 0,
        "{late}/1000 restored keepalives scheduled beyond one interval — each is a \
             cadence of silence the UAC tolerance does not cover",
    );
    // Clamped BEFORE the spread, not after: clamping after would collapse the
    // whole over-ceiling tail onto `ceiling` — a re-correlated OPTIONS burst.
    assert!(
        fire_ats.len() > 900,
        "clamped cohort is still de-correlated: {} distinct fire_at over 1000 calls",
        fire_ats.len(),
    );

    // Reactive takeover / on-demand straggler: no cohort, so no smoothing —
    // the ceiling still applies, exactly.
    let mut t = vec![keepalive(deadline)];
    sanitize_restored_timers(&mut t, "w1|solo|solo", now, Some(0), interval, None);
    assert_eq!(t[0].fire_at, ceiling, "un-smoothed restore is clamped to the ceiling");

    // A deadline already within one interval is trusted token-for-token (the
    // single-clock transparency property, future-side counterpart of the floor).
    let inside = now + interval / 3;
    let mut t = vec![keepalive(inside)];
    sanitize_restored_timers(&mut t, "w1|inside|inside", now, Some(0), interval, None);
    assert_eq!(t[0].fire_at, inside, "a within-interval deadline is left untouched");

    // Keepalive-only: a policy deadline legitimately outlives one keepalive
    // interval (GlobalDuration caps an hour-long call) and must NOT be clamped.
    let duration_at = now + 3_600_000;
    let mut t = vec![
        keepalive(deadline),
        TimerEntry {
            id: TimerType::GlobalDuration.timer_id(None),
            timer_type: TimerType::GlobalDuration,
            fire_at: duration_at,
            leg_id: None,
        },
    ];
    sanitize_restored_timers(&mut t, "w1|mixed|mixed", now, Some(0), interval, None);
    assert_eq!(t[0].fire_at, ceiling, "the keepalive is clamped");
    assert_eq!(t[1].fire_at, duration_at, "GlobalDuration keeps its absolute deadline");
}

/// The past-due catch-up drains within one interval too. `smooth_keepalives`
/// schedules an overdue keepalive at `now + (l_max - l)/speedup`, and `l_max`
/// is the whole batch's worst gap with no cap by default — so ONE poisoned
/// deadline pushes every barely-overdue healthy call past a cadence. Same loss
/// as the un-clamped future side, reached through the other branch.
#[test]
fn catchup_drain_stays_within_one_interval() {
    let now = 1_000_000;
    let interval = 300_000;
    // One entry overdue by 40 intervals poisons the batch-wide l_max; this call
    // is overdue by 1 s, so it lands at the far end of the catch-up band.
    let smoothing = Smoothing { now_ms: now, l_max: 40 * interval, speedup: 10, cap_ms: None };
    let mut timers = vec![keepalive(now - 1_000)];
    sanitize_restored_timers(&mut timers, "w1|od|od", now, Some(0), interval, Some(smoothing));
    assert!(
        timers[0].fire_at <= now + interval,
        "catch-up offset bounded by the ceiling: {} > {}",
        timers[0].fire_at,
        now + interval,
    );
}

// The throughput collapse at the smoothing layer: a clean reboot rehydrates
// the whole partition at one instant with future-dated keepalive deadlines
// clustered in one interval. WITHOUT the future-dated branch they all keep
// the SAME `fire_at` and fire as one burst a cadence later (an OPTIONS spike
// that saturates the front proxy); WITH it each call's keepalive is spread
// into `[now, fire_at]` by a per-call hash, so a 1000-call cohort no longer
// shares a single deadline.
#[test]
fn future_dated_keepalive_cohort_is_de_correlated() {
    let now = 1_000_000;
    let deadline = now + 300_000; // whole cohort clustered at +300 s (one interval)
    let speedup = 10;
    let mut fire_ats = std::collections::HashSet::new();
    for i in 0..1000 {
        // Distinct call_ref per call, identical clustered keepalive deadline.
        let call_ref = format!("w1|call-{i}|tag-{i}");
        let mut timers = vec![keepalive(deadline)];
        smooth_keepalives(&mut timers, &call_ref, now, 0, speedup, None);
        let fa = timers[0].fire_at;
        assert!(
            (now..=deadline).contains(&fa),
            "spread keepalive stays in [now, original deadline] (never delayed past it): {fa}",
        );
        fire_ats.insert(fa);
    }
    // De-correlation: a synchronised cohort would collapse to ONE deadline;
    // the spread scatters them across the interval (allow a few hash collisions).
    assert!(
        fire_ats.len() > 900,
        "cohort de-correlated: {} distinct fire_at over 1000 calls (1 if synchronised)",
        fire_ats.len(),
    );
    // Determinism: re-running the same ref yields the SAME slot (idempotent
    // reboot re-pass — a second reclaim scan must not re-scatter live timers).
    let mut a = vec![keepalive(deadline)];
    let mut b = vec![keepalive(deadline)];
    smooth_keepalives(&mut a, "w1|call-7|tag-7", now, 0, speedup, None);
    smooth_keepalives(&mut b, "w1|call-7|tag-7", now, 0, speedup, None);
    assert_eq!(a[0].fire_at, b[0].fire_at, "stable_jitter is deterministic per call_ref");
}

// The past-due (overdue) path: oldest-first, bounded to speedup× cadence —
// most-overdue fires first (smallest offset).
#[test]
fn past_due_keepalives_keep_oldest_first_schedule() {
    let now = 1_000_000;
    let l_max = 200_000; // most-overdue gap across the batch
    let speedup = 10;
    // Most-overdue (fire_at = now - 200s, l = l_max): offset (l_max-l)/speedup = 0.
    let mut oldest = vec![keepalive(now - 200_000)];
    smooth_keepalives(&mut oldest, "w1|a|a", now, l_max, speedup, None);
    assert_eq!(oldest[0].fire_at, now, "most-overdue re-probes first (offset 0)");
    // Least-overdue of the batch (fire_at = now - 100s, l = 100s): offset
    // (l_max - l)/speedup = (200s - 100s)/10 = 10s later.
    let mut newer = vec![keepalive(now - 100_000)];
    smooth_keepalives(&mut newer, "w1|b|b", now, l_max, speedup, None);
    assert_eq!(newer[0].fire_at, now + 10_000, "less-overdue drains later, bounded by speedup");
}

// ── clock-skew hardening: the restore-hygiene seam ──────────────────────

/// smooth_keepalives / l_max classification over SKEW-CORRECTED offsets. A
/// reclaimer anchored +45 s AHEAD of the origin reads a keepalive that is
/// genuinely FUTURE-dated (fire_at = origin_now + 300 s) as if it were
/// past-due, because the raw `fire_at` is in the reclaimer's past-frame. WITHOUT
/// re-anchoring, `smooth_keepalives` would classify it past-due and compress it
/// into the catch-up band. The seam re-anchors first (+45 s), so it is correctly
/// seen as future-dated and de-correlated into `[now, fire_at]` — never crushed
/// to `now`.
#[test]
fn seam_reanchor_keeps_future_cohort_out_of_the_catchup_band() {
    // The reclaimer's clock frame.
    let now = 1_000_000;
    // Origin minted the keepalive 300 s out on ITS clock, which is 45 s BEHIND
    // the reclaimer → the reclaimer received it with skew_offset = +45_000, and
    // the raw fire_at (origin frame) reads as `now - 45_000 + 300_000` once we
    // subtract the offset back out. Model the raw (pre-correction) fire_at:
    let skew = 45_000; // receiver_now − origin_now
    let raw_fire_at = now - skew + 300_000; // origin-frame deadline as stored
                                            // Before correction this is `now + 255_000` → looks 45 s "closer" but still
                                            // future; a LARGER skew would flip it past-due. Use a skew big enough to
                                            // flip it: origin minted it only 30 s out.
    let raw_fire_at_flip = now - skew + 30_000; // = now - 15_000 → PAST-DUE raw!
    let mut timers = vec![keepalive(raw_fire_at_flip)];
    // l_max computed over the CORRECTED deadline (as reclaim_all does):
    // corrected = raw + skew = now + 30_000 (future) → not past-due → l_max 0.
    let smoothing = Some(Smoothing { now_ms: now, l_max: 0, speedup: 10, cap_ms: None });
    sanitize_restored_timers(&mut timers, "w1|c|c", now, Some(skew), 300_000, smoothing);
    let fa = timers[0].fire_at;
    assert!(
        fa >= now,
        "corrected future keepalive is NOT crushed to a past-due catch-up slot: {fa} < {now}",
    );
    assert!(
        fa <= now + 30_000,
        "de-correlated within [now, corrected deadline], not the raw past-due frame: {fa}",
    );
    // Control: the SAME raw timers WITHOUT re-anchoring (offset None) would be
    // seen past-due and smoothed toward the catch-up band at `now`.
    let mut raw_timers = vec![keepalive(raw_fire_at_flip)];
    smooth_keepalives(&mut raw_timers, "w1|c|c", now, 15_000, 10, None);
    assert!(
        raw_timers[0].fire_at <= now,
        "uncorrected: the future keepalive IS mis-compressed to the catch-up band",
    );
    let _ = raw_fire_at;
}

/// The defensive floor. When the offset is UNKNOWN (`None`, a path that
/// could not re-anchor), a keepalive past-due by ≥ 1× interval is re-based
/// to within one interval of `now` (not fired immediately, which would race a
/// failed-over transaction). Deterministic per (call_ref, timer id).
#[test]
fn defensive_floor_rebases_deep_past_due_keepalive_within_one_interval() {
    let now = 1_000_000;
    let interval = 300_000;
    // Past-due by 2× interval — deep skew/backlog pathology.
    let make = || vec![keepalive(now - 2 * interval)];

    let mut t = make();
    // Unknown offset (None) → floor engages; no smoothing.
    sanitize_restored_timers(&mut t, "w1|d|d", now, None, interval, None);
    let fa = t[0].fire_at;
    assert!(
        (now..now + interval).contains(&fa),
        "deep-past-due keepalive re-based into [now, now+interval): {fa}",
    );
    assert!(fa > now, "not fired immediately at now (which would race the failed-over txn)");

    // Determinism: same (call_ref, id) → same slot (idempotent reboot re-pass).
    let mut t2 = make();
    sanitize_restored_timers(&mut t2, "w1|d|d", now, None, interval, None);
    assert_eq!(t2[0].fire_at, fa, "floor is deterministic per call_ref/id");
    // Distinct call_ref → (very likely) a different slot.
    let mut t3 = make();
    sanitize_restored_timers(&mut t3, "w1|different|x", now, None, interval, None);
    // (Not asserting inequality hard — hash collisions are possible — but the
    // slot must still be within the interval.)
    assert!((now..now + interval).contains(&t3[0].fire_at));

    // KNOWN offset (Some, even 0) → floor SKIPPED: a well-anchored past-due
    // keepalive fires promptly (single-clock transparency).
    let mut t4 = make();
    sanitize_restored_timers(&mut t4, "w1|d|d", now, Some(0), interval, None);
    assert_eq!(
        t4[0].fire_at,
        now - 2 * interval,
        "known offset trusts the deadline — past-due fires promptly, floor does NOT engage",
    );
}

/// SetupTimeout (a non-keepalive policy deadline) under skew. A ringing call
/// partway through its setup window fails over; the restored SetupTimeout
/// must land at its TRUE remaining time in the takeover node's clock frame,
/// NOT reaped early (skew-ahead) nor extended by the skew (skew-behind). The
/// seam re-anchors ALL timer classes, so this holds for the SetupTimeout
/// ledger timer exactly as for the keepalive.
#[test]
fn setup_timeout_is_reanchored_not_reaped_early_or_extended() {
    // Origin minted SetupTimeout 150 s out at ring-start; the call is 100 s in,
    // so 50 s of setup window remains (origin-frame fire_at = origin_ring + 150).
    // Model the restored entry as it arrives on the takeover node, whose `now`
    // is 100 s past the (origin-frame) ring start.
    let setup = |fire_at: i64| TimerEntry {
        id: format!("{:?}", TimerType::SetupTimeout),
        timer_type: TimerType::SetupTimeout,
        fire_at,
        leg_id: None,
    };

    // ── skew-AHEAD (+40 s): the takeover node's clock is 40 s ahead of the
    //    origin. Raw fire_at (origin frame) = now_local − 40_000 + 50_000 (still
    //    50 s of true window). WITHOUT re-anchor it would read 10 s out (reaped
    //    40 s early); WITH +40 s re-anchor it lands at the true 50 s. ──────────
    let now = 1_000_000;
    let skew_ahead = 40_000;
    let raw_fire_at = now - skew_ahead + 50_000; // origin-frame deadline
    let mut t = vec![setup(raw_fire_at)];
    sanitize_restored_timers(&mut t, "w1|s|s", now, Some(skew_ahead), 300_000, None);
    assert_eq!(
        t[0].fire_at,
        now + 50_000,
        "skew-ahead: SetupTimeout lands at the TRUE remaining 50 s, not reaped 40 s early",
    );

    // ── skew-BEHIND (−40 s): the takeover node's clock is 40 s behind the
    //    origin. Raw fire_at (origin frame) = now_local + 40_000 + 50_000.
    //    WITHOUT re-anchor it would read 90 s out (extended by the skew); WITH
    //    −40 s re-anchor it lands at the true 50 s. ─────────────────────────────
    let skew_behind = -40_000;
    let raw_fire_at_b = now - skew_behind + 50_000; // = now + 90_000 (origin frame)
    let mut tb = vec![setup(raw_fire_at_b)];
    sanitize_restored_timers(&mut tb, "w1|s|s", now, Some(skew_behind), 300_000, None);
    assert_eq!(
        tb[0].fire_at,
        now + 50_000,
        "skew-behind: SetupTimeout lands at the TRUE remaining 50 s, not extended by 40 s",
    );
}

/// The re-anchor itself: a known offset shifts every timer's absolute deadline
/// into the local clock frame; a sub-deadband offset is a no-op (latency, not
/// skew — keeps the single-clock harness transparent).
#[test]
fn reanchor_applies_above_deadband_and_ignores_below() {
    let mut t = vec![keepalive(1_000_000), keepalive_timeout("a", 900_000)];
    reanchor_timers(&mut t, 30_000); // +30 s real skew
    assert_eq!(t[0].fire_at, 1_030_000);
    assert_eq!(t[1].fire_at, 930_000, "ALL timer classes re-anchored, not just keepalive");

    let mut small = vec![keepalive(1_000_000)];
    reanchor_timers(&mut small, 200); // 200 ms — below the 1 s deadband
    assert_eq!(small[0].fire_at, 1_000_000, "sub-deadband latency offset is a no-op");
}
