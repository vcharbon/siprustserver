//! **The caller's BYE and a drain's grace end the call once, in either
//! order** (ADR-0055 §3). The call's named backup is away, so no peer holds
//! it, and its primary drains without being withdrawn from routing (a
//! shutdown the orchestrator did not announce), so the caller's BYE still
//! reaches it. Whichever turn runs first ends the call; the other finds it
//! ended or ending.
//!
//! ```text
//!   grace first  the grace ends the call off the wire; the caller's BYE in
//!                flight reaches the primary, which holds no call: 481;
//!                the callee's BYE: 481; one CDR
//!   BYE first    the BYE makes the call end through its teardown; the
//!                grace finds it ending and leaves it; bob ─200─▶; one CDR
//! ```
//!
//! Beyond the failover sweep: a party's BYE racing a drain's grace.

use std::time::Duration;

use failover_harness::{total_cdrs_for, DrainExit, FailoverHarness, PendingDrain};
use sip_clock::testkit::advance_settled;

use crate::away_backup_support::{
    answered_on, bring_up_limited, exits, restart_in_place, settle, Answered, Limiter, ALICE, BOB,
    BOUNDS, GRACE,
};

/// The cluster with b1 away and one call on b2 no peer holds, b2 draining
/// without withdrawal: the harness at `GRACE - lead` past the drain's start,
/// the drain not polled since.
struct Racing {
    fh: FailoverHarness,
    alice: scenario_harness::Agent,
    bob: scenario_harness::Agent,
    proxy: failover_harness::ProxySut,
    a: failover_harness::ReplicatedB2buaSut,
    b: failover_harness::ReplicatedB2buaSut,
    limiter: Limiter,
    call: Answered,
    drain: PendingDrain,
}

async fn racing(name: &str, lead: Duration) -> Racing {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, b, limiter) = bring_up_limited(&mut fh).await;
    restart_in_place(&mut fh, &mut a);
    fh.advance(Duration::from_secs(1)).await;
    let call = answered_on(&fh, &alice, &bob, &proxy, &b).await;
    let t0 = fh.now_ms();
    let mut drain = fh.begin_drain_pending(&b, BOUNDS);
    while fh.now_ms() - t0 < (GRACE - lead - Duration::from_millis(200)).as_millis() as i64 {
        assert!(drain.poll().is_none(), "no exit before the grace");
        fh.advance(Duration::from_millis(100)).await;
    }
    let left = (GRACE - lead).as_millis() as i64 - (fh.now_ms() - t0);
    advance_settled(Duration::from_millis(left as u64)).await;
    Racing { fh, alice, bob, proxy, a, b, limiter, call, drain }
}

/// Poll the drain every 100 ms until it returns.
async fn returned(
    fh: &FailoverHarness,
    drain: &mut PendingDrain,
) -> failover_harness::DrainOutcome {
    for _ in 0..120 {
        if let Some(out) = drain.poll() {
            return out;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    panic!("the drain returned");
}

#[tokio::test(start_paused = true)]
async fn the_grace_ending_the_call_first_answers_the_callers_bye_481() {
    let name = "grace-first-then-the-callers-bye";
    let Racing { mut fh, alice, bob, proxy, a, mut b, limiter, mut call, mut drain } =
        racing(name, Duration::ZERO).await;

    // ── the BYE leaves as the grace's turn is queued ahead of it ────────────
    let mut bye = call.dialog.bye().await;
    assert!(drain.poll().is_none(), "the grace ends the call and waits for it to leave");
    bye.expect(481).await;
    let out = returned(&fh, &mut drain).await;
    drop(drain);
    assert_eq!(out.exit, DrainExit::Grace, "a drain not withdrawn reaches its grace: {out:?}");
    assert_eq!(out.residual, 1);
    assert_eq!(limiter.held(), 0, "the call's limiter set is released");
    let mut bye = call.bob_dialog.bye().await;
    bye.expect(481).await;

    exits(&mut fh, &mut b).await;
    settle(&fh, &[&a, &b], &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&[&a, &b], &call.call_ref), 1, "one CDR, the grace's");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn the_callers_bye_ending_the_call_first_is_left_its_teardown_by_the_grace() {
    let name = "callers-bye-first-then-the-grace";
    let Racing { mut fh, alice, bob, proxy, a, mut b, limiter, mut call, mut drain } =
        racing(name, Duration::from_millis(3)).await;

    // ── the BYE reaches the primary just before the grace ───────────────────
    let mut bye = call.dialog.bye().await;
    advance_settled(Duration::from_millis(3)).await;
    let at_grace = drain.poll();
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect(200).await;
    let out = match at_grace {
        Some(out) => out,
        None => returned(&fh, &mut drain).await,
    };
    drop(drain);
    assert_eq!(out.exit, DrainExit::Grace, "the grace found the call ending, not ended: {out:?}");
    assert_eq!(b.metrics().drain_exits("grace"), 1);
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(limiter.held(), 0, "the BYE's end released the call's limiter set");

    exits(&mut fh, &mut b).await;
    settle(&fh, &[&a, &b], &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&[&a, &b], &call.call_ref), 1, "one CDR, the BYE's");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
