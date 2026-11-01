//! **A backup away for long is named and admitted for every call of its
//! absence** (ADR-0055 §3, §8). Its container restarts behind a not-ready
//! endpoint for a minute, past the backlog's high-water age (5 s) and past
//! the proxy's tombstone of its address (32 s): the endpoint still lists the
//! ordinal, so the proxy names it, and a backup with no live flow is no slow
//! consumer, so the primary admits each call backed. On its return the
//! backup's bootstrap carries every one of them, and the primary's drain exits
//! on it holding them. The same holds past the primary's dead-peer TTL
//! (300 s), once the change log toward the away peer was dropped.
//!
//! ```text
//!   a away 60 s        calls at 1 s, 10 s and 40 s of the gap, each w_bak=a
//!   a returns          its bootstrap carries the three calls
//!   b drains ⇒ caught_up, exits; each caller's BYE ─▶ a ─▶ 200; one CDR each
//!   a away 310 s       one call after the dead-peer TTL, the same outcome
//!   a away 95 s        a call born before: b's log toward a went at the
//!                      TTL; a returns and bootstraps it, a parks its flow,
//!                      b drains to its grace and leaves the call to a
//! ```
//!
//! Beyond the failover sweep: the admission of calls across a long absence of
//! their backup.

use std::time::Duration;

use failover_harness::{total_cdrs_for, DrainExit, FailoverHarness, Partition};

use crate::away_backup_support::{
    answered_on, answered_with, bring_up, caller_hangs_up, come_back, drain, drain_caught_up,
    exits, restart_in_place, settle, Answered, ALICE, BOB,
};
use crate::departure_support::call_id_placed_on;

/// Advance to `at` past `t0` (ms on the harness clock).
async fn advance_to(fh: &FailoverHarness, t0: i64, at: Duration) {
    let left = at.as_millis() as i64 - (fh.now_ms() - t0);
    assert!(left > 0, "the timeline is already past {at:?}");
    fh.advance(Duration::from_millis(left as u64)).await;
}

#[tokio::test(start_paused = true)]
async fn every_call_of_a_minute_long_backup_absence_survives_the_primarys_drain() {
    let name = "backup-away-a-minute";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    restart_in_place(&mut fh, &mut a);
    let t0 = fh.now_ms();
    let mut calls: Vec<Answered> = Vec::new();
    for at in [1, 10, 40] {
        advance_to(&fh, t0, Duration::from_secs(at)).await;
        let call = answered_on(&fh, &alice, &bob, &proxy, &b).await;
        assert_eq!(
            call.cookie,
            ("b2".to_string(), "b1".to_string()),
            "the call admitted {at} s into the absence names the away worker"
        );
        calls.push(call);
    }
    assert_eq!(
        b.metrics().repl_new_calls_unbacked_total(),
        0,
        "no call was left unbacked for the away backup's backlog"
    );

    advance_to(&fh, t0, Duration::from_secs(60)).await;
    come_back(&mut fh, &mut a, &proxy).await;
    let held = a.scan_backed_up("b2");
    for call in &calls {
        assert!(held.contains(&call.call_ref), "the bootstrap carried {}: {held:?}", call.call_ref);
    }

    drain_caught_up(&mut fh, &b).await;
    exits(&mut fh, &mut b).await;
    for call in &mut calls {
        caller_hangs_up(&mut call.dialog, &bob).await;
    }
    let refs: Vec<&str> = calls.iter().map(|c| c.call_ref.as_str()).collect();
    settle(&fh, &[&a, &b], &refs, &[&alice, &bob]).await;
    for r in refs {
        assert_eq!(total_cdrs_for(&[&a, &b], r), 1, "one CDR for {r}");
    }
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn a_call_admitted_past_the_dead_peer_ttl_survives_the_primarys_drain() {
    let name = "backup-away-past-the-dead-peer-ttl";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    restart_in_place(&mut fh, &mut a);
    let t0 = fh.now_ms();
    let past_ttl = Duration::from_millis(b2bua::repl::DEFAULT_DEAD_PEER_TTL_MS as u64)
        + Duration::from_secs(10);
    advance_to(&fh, t0, past_ttl).await;
    let mut call = answered_on(&fh, &alice, &bob, &proxy, &b).await;
    assert_eq!(
        call.cookie,
        ("b2".to_string(), "b1".to_string()),
        "the not-ready member is named past the dead-peer TTL"
    );

    come_back(&mut fh, &mut a, &proxy).await;
    assert!(
        a.scan_backed_up("b2").contains(&call.call_ref),
        "the bootstrap carried the call admitted after the log toward the peer was dropped"
    );
    drain_caught_up(&mut fh, &b).await;
    exits(&mut fh, &mut b).await;
    caller_hangs_up(&mut call.dialog, &bob).await;
    settle(&fh, &[&a, &b], &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&[&a, &b], &call.call_ref), 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

/// A call its backup applied, then the backup away past the dead-peer TTL:
/// the primary's log toward it went, and the call has no entry in the log
/// opened when the backup came back and bootstrapped it. A drain of the
/// primary reaching its grace leaves the call to the backup's copy.
#[tokio::test(start_paused = true)]
async fn a_call_its_backup_bootstrapped_after_the_dead_peer_ttl_is_left_to_it_at_the_grace() {
    let name = "backup-back-past-the-ttl-holds-the-call";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    let call_id = call_id_placed_on("b2", "old-call");
    let mut call = answered_with(&fh, &alice, &bob, &proxy, &b, Some(&call_id)).await;
    assert_eq!(call.cookie, ("b2".to_string(), "b1".to_string()), "both alive: a backs it");
    assert!(a.scan_backed_up("b2").contains(&call.call_ref), "a applied the call");

    // ── a away past the TTL, back, bootstrapped ─────────────────────────────
    restart_in_place(&mut fh, &mut a);
    // Past the TTL and the replica reap pass (every 30 s) that drops the log.
    let past_ttl = Duration::from_millis(b2bua::repl::DEFAULT_DEAD_PEER_TTL_MS as u64)
        + Duration::from_secs(35);
    fh.advance(past_ttl).await;
    come_back(&mut fh, &mut a, &proxy).await;
    assert!(a.scan_backed_up("b2").contains(&call.call_ref), "a's bootstrap carried the call");
    assert!(
        !b.logs_toward("b1", Partition::Bak, &call.call_ref),
        "the call has no entry in b's log toward a opened since: the no-entry case"
    );

    // ── a parks its flows to b; b drains to its grace ───────────────────────
    a.simulate_peer_removed("b2");
    fh.advance(Duration::from_millis(300)).await;
    let out = drain(&mut fh, &b).await;
    assert_eq!(out.exit, DrainExit::GracePeersBehind, "got {out:?}");
    assert_eq!(
        b.cdr_records().iter().filter(|r| r.call_ref == call.call_ref).count(),
        0,
        "the call its backup holds is left to it: no CDR from the draining node"
    );
    assert!(b.serves(&call.call_ref), "the call lives on");

    a.simulate_peer_added("b2");
    fh.advance(Duration::from_millis(500)).await;
    exits(&mut fh, &mut b).await;
    caller_hangs_up(&mut call.dialog, &bob).await;
    settle(&fh, &[&a, &b], &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&[&a, &b], &call.call_ref), 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
