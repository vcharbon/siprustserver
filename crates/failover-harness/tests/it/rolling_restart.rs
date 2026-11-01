//! **A rolling restart loses no call born in any of its gaps** (ADR-0055 §3,
//! §8). Workers restart one after the other. Every call admitted while one of
//! them is away names it as its backup; each return bootstraps the calls named
//! on it, and each graceful drain then exits on the other worker holding its
//! live calls.
//!
//! ```text
//!   twice    a restarts (call 1 on b), returns; a restarts again (call 2 on
//!            b), returns; b drains ⇒ caught_up, exits; both BYEs ─▶ a ─▶ 200
//!   rolling  a restarts (call 1 on b), returns; b drains ⇒ caught_up and
//!            exits, its pod re-created (call 2 on a names b); b returns and
//!            reclaims call 1; a drains ⇒ caught_up, exits; both BYEs ─▶ b ─▶ 200
//!   reclaim  b reclaims a quiet call after its restart, writing nothing; a
//!            parks its flow to b, and b drains to its grace: the call is
//!            left to a's copy, no CDR from b; b exits, BYE ─▶ a ─▶ 200
//! ```
//!
//! Every call ends with the caller's BYE answered 200 and one CDR.
//!
//! Beyond the failover sweep: consecutive restarts and a rolling restart of
//! both workers, with calls born in every gap.

use std::time::Duration;

use failover_harness::{total_cdrs_for, DrainExit, FailoverHarness};

use crate::away_backup_support::{
    answered_on, bring_up, caller_hangs_up, come_back, drain, drain_caught_up, exits,
    restart_in_place, settle, ALICE, BOB,
};

#[tokio::test(start_paused = true)]
async fn calls_born_in_two_consecutive_restarts_of_the_backup_survive_the_primarys_drain() {
    let name = "backup-restarts-twice";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    // ── first gap ───────────────────────────────────────────────────────────
    restart_in_place(&mut fh, &mut a);
    fh.advance(Duration::from_secs(1)).await;
    let mut first = answered_on(&fh, &alice, &bob, &proxy, &b).await;
    assert_eq!(first.cookie, ("b2".to_string(), "b1".to_string()), "first gap names a");
    come_back(&mut fh, &mut a, &proxy).await;

    // ── second gap ──────────────────────────────────────────────────────────
    restart_in_place(&mut fh, &mut a);
    fh.advance(Duration::from_secs(1)).await;
    let mut second = answered_on(&fh, &alice, &bob, &proxy, &b).await;
    assert_eq!(second.cookie, ("b2".to_string(), "b1".to_string()), "second gap names a");
    come_back(&mut fh, &mut a, &proxy).await;
    let held = a.scan_backed_up("b2");
    for r in [&first.call_ref, &second.call_ref] {
        assert!(held.contains(r), "the last bootstrap carried {r}: {held:?}");
    }

    drain_caught_up(&mut fh, &b).await;
    exits(&mut fh, &mut b).await;
    caller_hangs_up(&mut first.dialog, &bob).await;
    caller_hangs_up(&mut second.dialog, &bob).await;
    let refs = [first.call_ref.as_str(), second.call_ref.as_str()];
    settle(&fh, &[&a, &b], &refs, &[&alice, &bob]).await;
    for r in refs {
        assert_eq!(total_cdrs_for(&[&a, &b], r), 1, "one CDR for {r}");
    }
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn a_rolling_restart_of_both_workers_loses_no_call_born_in_either_gap() {
    let name = "rolling-restart-of-both-workers";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    // ── a restarts; call 1 is born on b ─────────────────────────────────────
    restart_in_place(&mut fh, &mut a);
    fh.advance(Duration::from_secs(1)).await;
    let mut first = answered_on(&fh, &alice, &bob, &proxy, &b).await;
    assert_eq!(first.cookie, ("b2".to_string(), "b1".to_string()), "call 1 names a");
    come_back(&mut fh, &mut a, &proxy).await;
    assert!(a.scan_backed_up("b2").contains(&first.call_ref), "a holds call 1");

    // ── b restarts gracefully; call 2 is born on a ──────────────────────────
    drain_caught_up(&mut fh, &b).await;
    exits(&mut fh, &mut b).await;
    fh.advance(Duration::from_secs(1)).await;
    let mut second = answered_on(&fh, &alice, &bob, &proxy, &a).await;
    assert_eq!(second.cookie, ("b1".to_string(), "b2".to_string()), "call 2 names b");

    // ── b returns: it reclaims call 1 and bootstraps call 2 ─────────────────
    come_back(&mut fh, &mut b, &proxy).await;
    let reclaimed = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            b.serves(&first.call_ref)
        })
        .await;
    assert!(reclaimed, "b reclaimed call 1");
    assert!(b.scan_backed_up("b1").contains(&second.call_ref), "b holds call 2");

    // ── a drains on b holding call 2, and exits ─────────────────────────────
    drain_caught_up(&mut fh, &a).await;
    exits(&mut fh, &mut a).await;

    caller_hangs_up(&mut first.dialog, &bob).await;
    caller_hangs_up(&mut second.dialog, &bob).await;
    let refs = [first.call_ref.as_str(), second.call_ref.as_str()];
    settle(&fh, &[&a, &b], &refs, &[&alice, &bob]).await;
    for r in refs {
        assert_eq!(total_cdrs_for(&[&a, &b], r), 1, "one CDR for {r}");
    }
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

/// A call its restarted primary reclaimed write-free has no entry in the
/// primary's new change log, yet its backup holds it: a drain of that primary
/// reaching its grace leaves the call to the backup's copy.
#[tokio::test(start_paused = true)]
async fn a_call_its_restarted_primary_reclaimed_is_left_to_its_backup_at_the_grace() {
    let name = "reclaimed-call-left-to-its-backup";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    // ── a restarts so the call is born on b; a returns and holds it ─────────
    restart_in_place(&mut fh, &mut a);
    fh.advance(Duration::from_secs(1)).await;
    let mut call = answered_on(&fh, &alice, &bob, &proxy, &b).await;
    assert_eq!(call.cookie, ("b2".to_string(), "b1".to_string()));
    come_back(&mut fh, &mut a, &proxy).await;
    assert!(a.scan_backed_up("b2").contains(&call.call_ref), "a holds the call");

    // ── b restarts gracefully and reclaims the quiet call from a ────────────
    drain_caught_up(&mut fh, &b).await;
    exits(&mut fh, &mut b).await;
    come_back(&mut fh, &mut b, &proxy).await;
    let reclaimed = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            b.serves(&call.call_ref)
        })
        .await;
    assert!(reclaimed, "b reclaimed the call");

    // ── a parks its flows to b; b drains to its grace ───────────────────────
    a.simulate_peer_removed("b2");
    fh.advance(Duration::from_millis(300)).await;
    let out = drain(&mut fh, &b).await;
    assert_eq!(out.exit, DrainExit::GracePeersBehind, "got {out:?}");
    assert_eq!(out.residual, 1, "the call is reported");
    assert_eq!(
        b.cdr_records().iter().filter(|r| r.call_ref == call.call_ref).count(),
        0,
        "a call its backup holds is left to it: no CDR from the draining node"
    );
    assert!(b.serves(&call.call_ref), "the drain leaves the call to a's copy");

    // ── a re-adds b; b exits; the caller's BYE reaches a ────────────────────
    a.simulate_peer_added("b2");
    fh.advance(Duration::from_millis(500)).await;
    exits(&mut fh, &mut b).await;
    caller_hangs_up(&mut call.dialog, &bob).await;
    settle(&fh, &[&a, &b], &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&[&a, &b], &call.call_ref), 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
