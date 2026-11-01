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
//! ```
//!
//! Every call ends with the caller's BYE answered 200 and one CDR.
//!
//! Beyond the failover sweep: consecutive restarts and a rolling restart of
//! both workers, with calls born in every gap.

use std::time::Duration;

use failover_harness::{total_cdrs_for, FailoverHarness};

use crate::away_backup_support::{
    answered_on, bring_up, caller_hangs_up, come_back, drain_caught_up, exits, restart_in_place,
    settle, ALICE, BOB,
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
