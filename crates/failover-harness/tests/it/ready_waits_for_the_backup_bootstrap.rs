//! **A returned worker reads ready only once it holds the calls it backs up**
//! (ADR-0011 X5, ADR-0014 boot order). Its readiness waits for both flows of
//! each ready peer: the Reclaim flow (its own calls back) and the Backup flow
//! (the peer's calls it backs up). An orchestrator that waits for ready before
//! the next disruption then never takes down a primary whose calls have no
//! copy. A peer unreachable since the return holds readiness no longer than
//! the bootstrap hard timer, both flows included.
//!
//! ```text
//!   two      a restarts · three calls on b name a · b ─▶ a delayed 300 ms
//!            a returns: at its first ready instant it holds the three
//!            b drains ⇒ caught_up, exits; each caller's BYE ─▶ a ─▶ 200
//!   three    x on b2 and y on b3, both backed by b1 · b1 restarts, cut off
//!            from b3 · b1 returns: ready inside the 10 s hard timer and
//!            holding x · the cut heals: b1 holds y · BYEs ─▶ 200
//! ```
//!
//! Every call ends with the caller's BYE answered 200 and one CDR.
//!
//! Beyond the failover sweep: the readiness of a returned worker.

use std::time::Duration;

use failover_harness::{total_cdrs_for, FailoverHarness, WorkerHealth};

use crate::away_backup_support::{
    answered_on, answered_with, bring_up, caller_hangs_up, drain_caught_up, exits,
    restart_in_place, settle, Answered, ALICE, BOB,
};
use crate::three_worker_support::{bring_up_three, call_id_owned_by};

/// How late the primary's frames reach the returning worker: each of its
/// flows completes that long after it opens, well under the puller's idle
/// bound (2 s).
const FLOW_DELAY_MS: u64 = 300;

/// The pullers' bootstrap hard timer (`PullerConfig::default`).
const HARD_TIMER: Duration = Duration::from_secs(10);

#[tokio::test(start_paused = true)]
async fn a_returned_worker_reads_ready_only_once_it_holds_the_calls_it_backs_up() {
    let name = "ready-holds-the-backed-up-calls";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    // ── a away; three calls on b name it ────────────────────────────────────
    restart_in_place(&mut fh, &mut a);
    fh.advance(Duration::from_secs(1)).await;
    let mut calls: Vec<Answered> = Vec::new();
    for _ in 0..3 {
        let call = answered_on(&fh, &alice, &bob, &proxy, &b).await;
        assert_eq!(call.cookie, ("b2".to_string(), "b1".to_string()), "each call names a");
        calls.push(call);
    }

    // ── a returns behind a slow link: ready only once it holds them ─────────
    fh.delay("b2", "b1", FLOW_DELAY_MS);
    fh.mark("b1", None, "reboot", "a fresh incarnation at a new address");
    let addr = a.reboot().await;
    fh.note_worker_rebound("b1", addr);
    let ready = fh
        .pump_until(Duration::from_millis(10), Duration::from_secs(30), async || a.is_ready())
        .await;
    assert!(ready, "a finished its return");
    let held = a.scan_backed_up("b2");
    for call in &calls {
        assert!(held.contains(&call.call_ref), "a ready holds {}: {held:?}", call.call_ref);
    }

    // ── a published; b drains on it holding every call ──────────────────────
    fh.heal("b2", "b1");
    fh.readmit("b1", addr);
    proxy.set_health("b1", WorkerHealth::Alive);
    fh.advance(Duration::from_millis(200)).await;
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
async fn a_peer_unreachable_since_the_return_holds_readiness_no_longer_than_the_hard_timer() {
    let name = "ready-bounded-by-the-hard-timer";
    let mut fh = FailoverHarness::new(name, &["b1", "b2", "b3"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut b1, b2, b3) = bring_up_three(&mut fh).await;
    let all = ["b1", "b2", "b3"];

    // ── x on b2 and y on b3, both backed by b1 ──────────────────────────────
    let x_id = call_id_owned_by("b2", "b1", &all, "unreachable-x");
    let mut x = answered_with(&fh, &alice, &bob, &proxy, &b2, Some(&x_id)).await;
    assert_eq!(x.cookie, ("b2".to_string(), "b1".to_string()));
    let y_id = call_id_owned_by("b3", "b1", &all, "unreachable-y");
    let mut y = answered_with(&fh, &alice, &bob, &proxy, &b3, Some(&y_id)).await;
    assert_eq!(y.cookie, ("b3".to_string(), "b1".to_string()));

    // ── b1 restarts, cut off from b3 for its whole return ───────────────────
    restart_in_place(&mut fh, &mut b1);
    fh.partition("b1", "b3");
    fh.advance(Duration::from_secs(1)).await;
    fh.mark("b1", None, "reboot", "a fresh incarnation at a new address");
    let addr = b1.reboot().await;
    fh.note_worker_rebound("b1", addr);
    let t0 = fh.now_ms();
    let ready =
        fh.pump_until(Duration::from_millis(100), HARD_TIMER * 3, async || b1.is_ready()).await;
    assert!(ready, "b1 returned");
    let waited = Duration::from_millis((fh.now_ms() - t0) as u64);
    assert!(
        waited <= HARD_TIMER + Duration::from_secs(1),
        "the unreachable peer holds readiness no longer than the hard timer, waited {waited:?}"
    );
    assert!(b1.scan_backed_up("b2").contains(&x.call_ref), "b1 ready holds b2's call");

    // ── the cut heals: b1 takes up y ────────────────────────────────────────
    fh.heal("b1", "b3");
    fh.readmit("b1", addr);
    proxy.set_health("b1", WorkerHealth::Alive);
    let holds_y = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(10), async || {
            b1.scan_backed_up("b3").contains(&y.call_ref)
        })
        .await;
    assert!(holds_y, "b1 holds b3's call once the cut heals");

    caller_hangs_up(&mut x.dialog, &bob).await;
    caller_hangs_up(&mut y.dialog, &bob).await;
    let refs = [x.call_ref.as_str(), y.call_ref.as_str()];
    let nodes = [&b1, &b2, &b3];
    settle(&fh, &nodes, &refs, &[&alice, &bob]).await;
    for r in refs {
        assert_eq!(total_cdrs_for(&nodes, r), 1, "one CDR for {r}");
    }
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
