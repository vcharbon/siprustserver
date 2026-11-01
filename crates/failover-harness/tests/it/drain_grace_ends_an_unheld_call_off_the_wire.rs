//! **At its grace a drain ends, off the wire, each established call no live
//! peer holds** (ADR-0055 §3, §5). The call's named backup is still away when
//! its primary drains: no flow reports holding the call, so the drain waits
//! its grace, then ends the call where it stands: its CDR is written, its
//! holds released, and nothing is sent, so the media of a call that still
//! flows is never cut by the node. The parties' later BYEs reach a worker that
//! holds no copy and draw `481`, which ends each dialog (RFC 3261 §12.2.1.2).
//! A backup that returns inside the grace and bootstraps the call holds it
//! instead: the drain exits caught up and the call lives on.
//!
//! ```text
//!   away     a away · call on b names a · b drains: no exit before the grace
//!            grace   b ends the call: one CDR, no SIP, nothing resident
//!            b exits; a returns; alice ─BYE─▶ a ─▶ 481; bob ─BYE─▶ a ─▶ 481
//!   returns  a away · call on b names a · b drains · a returns at +1.5 s
//!            ⇒ caught_up inside the grace; b exits; alice ─BYE─▶ a ─▶ 200
//! ```
//!
//! Beyond the failover sweep: the end of a call no peer holds at a drain's grace.

use std::time::Duration;

use failover_harness::{total_cdrs_for, DrainExit, FailoverHarness};

use crate::away_backup_support::{
    answered_on, bring_up, caller_hangs_up, come_back, drain, exits, restart_in_place, settle,
    ALICE, BOB, BOUNDS, FLOOR, GRACE,
};

#[tokio::test(start_paused = true)]
async fn a_call_whose_backup_is_still_away_at_the_grace_is_ended_off_the_wire() {
    let name = "drain-grace-backup-still-away";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    restart_in_place(&mut fh, &mut a);
    fh.advance(Duration::from_secs(1)).await;
    let mut call = answered_on(&fh, &alice, &bob, &proxy, &b).await;

    // ── b drains with the call's backup away: the grace, then the end ───────
    let t0 = fh.now_ms();
    let out = drain(&mut fh, &b).await;
    assert_eq!(out.exit, DrainExit::GracePeersBehind, "no peer reported holding the call");
    assert!(out.elapsed >= GRACE, "the whole grace was waited, got {:?}", out.elapsed);
    assert_eq!(
        b.cdr_records().iter().filter(|r| r.call_ref == call.call_ref).count(),
        1,
        "the draining primary wrote the call's CDR at the grace"
    );
    assert!(!b.serves(&call.call_ref), "the call is released on the draining primary");
    assert!(b.memory_clean(), "nothing of the call stays resident");
    let sent_by_b = fh
        .sip_entries()
        .iter()
        .filter(|e| e.from == b.sip_addr() && e.sent_ms >= t0 as u64)
        .count();
    assert_eq!(sent_by_b, 0, "the draining primary sent no SIP to end the call");

    // ── b exits, a returns; the parties' BYEs draw 481 ───────────────────────
    exits(&mut fh, &mut b).await;
    come_back(&mut fh, &mut a, &proxy).await;
    let mut bye = call.dialog.bye().await;
    bye.expect(481).await;
    let mut bye = call.bob_dialog.bye().await;
    bye.expect(481).await;

    settle(&fh, &[&a, &b], &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&[&a, &b], &call.call_ref), 1, "one CDR, the drain's");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn a_backup_returning_inside_the_grace_holds_the_call() {
    let name = "drain-grace-backup-returns-inside";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    restart_in_place(&mut fh, &mut a);
    fh.advance(Duration::from_secs(1)).await;
    let mut call = answered_on(&fh, &alice, &bob, &proxy, &b).await;

    // ── b drains; a returns at +1.5 s and bootstraps the call ───────────────
    let t0 = fh.now_ms();
    fh.withdraw_routing("b2");
    let mut pending = fh.begin_drain_pending(&b, BOUNDS);
    while fh.now_ms() - t0 < 1500 {
        let early = pending.poll();
        assert!(early.is_none(), "no exit while the backup is away, got {early:?}");
        fh.advance(Duration::from_millis(100)).await;
    }
    come_back(&mut fh, &mut a, &proxy).await;
    assert!(a.scan_backed_up("b2").contains(&call.call_ref), "a bootstrapped the call");
    let mut out = None;
    while fh.now_ms() - t0 < GRACE.as_millis() as i64 {
        out = pending.poll();
        if out.is_some() {
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    drop(pending);
    let out = out.expect("the drain exited inside the grace");
    assert_eq!(out.exit, DrainExit::CaughtUp, "the returned backup holds the call: {out:?}");
    assert!(out.elapsed >= FLOOR && out.elapsed < GRACE, "inside the grace: {:?}", out.elapsed);
    assert!(b.serves(&call.call_ref), "the call lives on");

    exits(&mut fh, &mut b).await;
    caller_hangs_up(&mut call.dialog, &bob).await;
    settle(&fh, &[&a, &b], &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&[&a, &b], &call.call_ref), 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
