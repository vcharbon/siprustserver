//! **A rolling restart of three workers, one down at a time, loses no call**
//! (ADR-0055 §3, §8). Each worker drains, exits and returns before the next
//! one goes: a call born in a gap names a live backup, each return
//! reclaims and bootstraps the calls named on it, and each drain exits on
//! backups holding every live call. A call its primary's drain left to its
//! backup, then served there as a takeover copy, is reclaimed by its
//! returned primary and survives the backup's own restart.
//!
//! ```text
//!   rollout    one call on each worker · b1, then b2, then b3 drain ⇒
//!              caught_up, exit, return; two calls born in each gap ·
//!              each call ends by its caller's or its callee's BYE ─▶ 200
//!   copy       x on b2 backed by b3 · b2 drains, exits; alice INFO ─▶ b3
//!              (takeover copy) · b2 returns, reclaims x · b3 drains,
//!              exits, returns · bob BYE ─▶ b2 ─▶ 200
//!   overlap    as copy, but b3 drains while b2's reclaim of x is in flight
//!              (a double fault, outcome recorded): the drain waits, b2's
//!              reclaim stands the copy down, b3 exits quiescent inside its
//!              grace · bob BYE ─▶ b2 ─▶ 200
//! ```
//!
//! Every call writes one CDR.
//!
//! Beyond the failover sweep: rolling restarts with three workers.

use std::time::Duration;

use failover_harness::{total_cdrs_for, DrainExit, FailoverHarness, ReplicatedB2buaSut};
use scenario_harness::Agent;
use sip_message::generators::InDialogMethod;

use crate::away_backup_support::{
    answered_with, callee_hangs_up, caller_hangs_up, come_back, drain, drain_caught_up, exits,
    settle, Answered, ALICE, BOB, GRACE,
};
use crate::three_worker_support::{
    bring_up_three, call_id_owned_by, call_id_placed_on, drain_clean, published_once_ready,
    returns_listed_not_ready,
};

/// A call through the proxy whose Call-ID ranks `primary` first among the
/// workers `alive`.
async fn call_on(
    fh: &FailoverHarness,
    parties: (&Agent, &Agent),
    proxy: &failover_harness::ProxySut,
    primary: &ReplicatedB2buaSut,
    alive: &[&'static str],
    label: &str,
) -> Answered {
    let ordinal = primary.ordinal();
    let call_id = call_id_placed_on(ordinal, alive, label);
    let call = answered_with(fh, parties.0, parties.1, proxy, primary, Some(&call_id)).await;
    assert_eq!(call.cookie.0, ordinal, "{label} is placed on {ordinal}");
    assert_ne!(call.cookie.1, "", "{label} names a backup");
    call
}

/// Each call ends by a BYE answered 200: the caller hangs up the even ones,
/// the callee the odd ones.
async fn hang_up_alternately(calls: &mut [Answered], alice: &Agent, bob: &Agent) {
    for (i, call) in calls.iter_mut().enumerate() {
        if i % 2 == 0 {
            caller_hangs_up(&mut call.dialog, bob).await;
        } else {
            callee_hangs_up(&mut call.bob_dialog, alice).await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_rolling_restart_of_three_workers_loses_no_call() {
    let name = "three-worker-rollout";
    let mut fh = FailoverHarness::new(name, &["b1", "b2", "b3"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let parties = (&alice, &bob);
    let (proxy, mut b1, mut b2, mut b3) = bring_up_three(&mut fh).await;
    let all = ["b1", "b2", "b3"];

    // ── before the rollout: one call on each worker ─────────────────────────
    let mut calls = vec![
        call_on(&fh, parties, &proxy, &b1, &all, "before-b1").await,
        call_on(&fh, parties, &proxy, &b2, &all, "before-b2").await,
        call_on(&fh, parties, &proxy, &b3, &all, "before-b3").await,
    ];

    // ── b1 restarts; two calls in its gap ───────────────────────────────────
    drain_caught_up(&mut fh, &b1).await;
    exits(&mut fh, &mut b1).await;
    let gap = ["b2", "b3"];
    calls.push(call_on(&fh, parties, &proxy, &b2, &gap, "gap-b1-on-b2").await);
    calls.push(call_on(&fh, parties, &proxy, &b3, &gap, "gap-b1-on-b3").await);
    come_back(&mut fh, &mut b1, &proxy).await;

    // ── b2 restarts; two calls in its gap ───────────────────────────────────
    drain_caught_up(&mut fh, &b2).await;
    exits(&mut fh, &mut b2).await;
    let gap = ["b1", "b3"];
    calls.push(call_on(&fh, parties, &proxy, &b1, &gap, "gap-b2-on-b1").await);
    calls.push(call_on(&fh, parties, &proxy, &b3, &gap, "gap-b2-on-b3").await);
    come_back(&mut fh, &mut b2, &proxy).await;

    // ── b3 restarts; two calls in its gap ───────────────────────────────────
    drain_caught_up(&mut fh, &b3).await;
    exits(&mut fh, &mut b3).await;
    let gap = ["b1", "b2"];
    calls.push(call_on(&fh, parties, &proxy, &b1, &gap, "gap-b3-on-b1").await);
    calls.push(call_on(&fh, parties, &proxy, &b2, &gap, "gap-b3-on-b2").await);
    come_back(&mut fh, &mut b3, &proxy).await;

    // ── every call ends by a BYE answered 200, with one CDR ─────────────────
    hang_up_alternately(&mut calls, &alice, &bob).await;
    let refs: Vec<&str> = calls.iter().map(|c| c.call_ref.as_str()).collect();
    let nodes = [&b1, &b2, &b3];
    settle(&fh, &nodes, &refs, &[&alice, &bob]).await;
    for r in refs {
        assert_eq!(total_cdrs_for(&nodes, r), 1, "one CDR for {r}");
    }
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

/// alice sends INFO in `call`'s dialog; bob answers 200.
async fn info(call: &mut Answered, bob: &Agent) {
    let mut info = call.dialog.request(InDialogMethod::Info, None).await;
    bob.receive_tolerating("INFO", &["OPTIONS"]).await.respond(200, "OK").await;
    info.expect(200).await;
}

#[tokio::test(start_paused = true)]
async fn a_call_served_by_its_backups_copy_survives_the_backups_restart_after_the_reclaim() {
    let name = "three-worker-copy-meets-the-next-restart";
    let mut fh = FailoverHarness::new(name, &["b1", "b2", "b3"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, b1, mut b2, mut b3) = bring_up_three(&mut fh).await;
    let all = ["b1", "b2", "b3"];

    let x_id = call_id_owned_by("b2", "b3", &all, "copy-x");
    let mut x = answered_with(&fh, &alice, &bob, &proxy, &b2, Some(&x_id)).await;
    assert_eq!(x.cookie, ("b2".to_string(), "b3".to_string()));

    // ── b2 restarts; x's in-dialog traffic meets b3's copy ──────────────────
    drain_caught_up(&mut fh, &b2).await;
    exits(&mut fh, &mut b2).await;
    info(&mut x, &bob).await;
    assert!(b3.serves(&x.call_ref), "b3 serves x as a takeover copy");
    come_back(&mut fh, &mut b2, &proxy).await;
    let reclaimed = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            b2.serves(&x.call_ref) && !b3.serves(&x.call_ref)
        })
        .await;
    assert!(reclaimed, "b2 reclaimed x and b3's copy stood down");

    // ── b3 restarts; it returns holding x as b2's backup ────────────────────
    drain_clean(&mut fh, &b3).await;
    exits(&mut fh, &mut b3).await;
    come_back(&mut fh, &mut b3, &proxy).await;
    assert!(b3.scan_backed_up("b2").contains(&x.call_ref), "b3 ready holds x");

    callee_hangs_up(&mut x.bob_dialog, &alice).await;
    let nodes = [&b1, &b2, &b3];
    settle(&fh, &nodes, &[&x.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&nodes, &x.call_ref), 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

/// The backup serving a taken-over call drains while the call's returned
/// primary still reclaims it: two owners of one call down in turn, a double
/// fault. Only its outcome is recorded.
#[tokio::test(start_paused = true)]
async fn a_copys_backup_draining_before_the_reclaim_completes_is_recorded() {
    let name = "three-worker-copy-drains-before-the-reclaim";
    let mut fh = FailoverHarness::new(name, &["b1", "b2", "b3"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, b1, mut b2, mut b3) = bring_up_three(&mut fh).await;
    let all = ["b1", "b2", "b3"];

    let x_id = call_id_owned_by("b2", "b3", &all, "overlap-x");
    let mut x = answered_with(&fh, &alice, &bob, &proxy, &b2, Some(&x_id)).await;

    drain_caught_up(&mut fh, &b2).await;
    exits(&mut fh, &mut b2).await;
    info(&mut x, &bob).await;
    assert!(b3.serves(&x.call_ref), "b3 serves x as a takeover copy");

    // ── b2 returns behind a slow link from b3; b3 drains at once ────────────
    fh.delay("b3", "b2", 1_500);
    returns_listed_not_ready(&mut fh, &mut b2).await;
    fh.advance(Duration::from_millis(200)).await;
    assert!(!b2.serves(&x.call_ref), "b2's reclaim of x is in flight");
    let out = drain(&mut fh, &b3).await;
    fh.mark("b3", None, "outcome", &format!("drain of the copy's node: {out:?}"));
    exits(&mut fh, &mut b3).await;
    fh.heal("b3", "b2");
    published_once_ready(&mut fh, &b2, &proxy).await;
    come_back(&mut fh, &mut b3, &proxy).await;

    // ── the recorded outcome ────────────────────────────────────────────────
    assert_eq!(out.exit, DrainExit::Quiescent, "b2's reclaim stood b3's copy down: {out:?}");
    assert!(
        out.elapsed >= Duration::from_millis(1_500) && out.elapsed < GRACE,
        "b3's drain waited for the reclaim, inside its grace: {out:?}"
    );
    assert!(b2.serves(&x.call_ref), "b2 reclaimed x from the copy's writes");
    callee_hangs_up(&mut x.bob_dialog, &alice).await;
    let nodes = [&b1, &b2, &b3];
    settle(&fh, &nodes, &[&x.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&nodes, &x.call_ref), 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
