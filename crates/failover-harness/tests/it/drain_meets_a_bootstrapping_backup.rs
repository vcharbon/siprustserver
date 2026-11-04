//! **A primary draining while its returned backup still bootstraps waits for
//! that bootstrap, up to its grace** (ADR-0031 D2, ADR-0055 §3). b2's calls
//! are backed up on b3; b3 restarts and returns, listed not ready, and b2
//! drains before b3 has received them. The drain's caught-up exit waits for
//! b3's Backup flow to report holding every live call: a bootstrap done
//! inside the grace carries every call; one not done by the grace leaves the
//! calls no peer holds to the drain's backstop, which ends each off the wire
//! with its CDR.
//!
//! ```text
//!   inside   x1..x3 on b2 backed by b3 · b3 restarts, returns behind a
//!            slow link from b2 (2 s to bootstrap) · b2 drains ⇒ caught_up
//!            once b3 holds the three, inside the grace · b2 exits ·
//!            BYEs ─▶ b3 ─▶ 200
//!   past     as inside, nothing from b2 reaches b3 · b2 drains to its grace
//!            ⇒ grace_peers_behind, three CDRs, no SIP · b2 exits · the
//!            caller's and the callee's BYEs ─▶ b3 ─▶ 481
//! ```
//!
//! Every call writes one CDR.
//!
//! Beyond the failover sweep: a rollout step that does not wait for the
//! previous worker's readiness.

use std::time::Duration;

use failover_harness::{total_cdrs_for, DrainExit, FailoverHarness};

use crate::away_backup_support::{
    answered_with, caller_hangs_up, drain, exits, settle, Answered, ALICE, BOB, GRACE,
};
use crate::three_worker_support::{
    bring_up_three, call_id_owned_by, drain_clean, published_once_ready, returns_listed_not_ready,
};

/// How late each of b2's frames reaches the returned b3, under the puller's
/// idle bound. The fabric delivers a stream's frames one after the other, so
/// b3's return takes five of them: the Reclaim flow's catch-up `Noop`, then
/// the Backup flow's three calls and its `Noop`, 2 s in all.
const FLOW_DELAY_MS: u64 = 400;

/// The least a drain started 200 ms into b3's return waits for its bootstrap.
const BOOTSTRAP_WAIT: Duration = Duration::from_millis(1_500);

/// Three calls on b2 backed by b3, then b3 restarts gracefully and returns
/// listed not ready, its flows from b2 held back by `hold`.
async fn b3_returns_behind(
    name: &str,
    hold: impl FnOnce(&mut FailoverHarness),
) -> (
    FailoverHarness,
    scenario_harness::Agent,
    scenario_harness::Agent,
    failover_harness::ProxySut,
    [failover_harness::ReplicatedB2buaSut; 3],
    Vec<Answered>,
) {
    let mut fh = FailoverHarness::new(name, &["b1", "b2", "b3"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, b1, b2, mut b3) = bring_up_three(&mut fh).await;
    let all = ["b1", "b2", "b3"];
    let mut calls = Vec::new();
    for i in 0..3 {
        let id = call_id_owned_by("b2", "b3", &all, &format!("{name}-x{i}"));
        let call = answered_with(&fh, &alice, &bob, &proxy, &b2, Some(&id)).await;
        assert_eq!(call.cookie, ("b2".to_string(), "b3".to_string()));
        calls.push(call);
    }

    drain_clean(&mut fh, &b3).await;
    exits(&mut fh, &mut b3).await;
    hold(&mut fh);
    returns_listed_not_ready(&mut fh, &mut b3).await;
    fh.advance(Duration::from_millis(200)).await;
    assert!(!b3.is_ready(), "b3's bootstrap is in flight");
    assert!(b3.scan_backed_up("b2").is_empty(), "b3 holds none of b2's calls yet");
    (fh, alice, bob, proxy, [b1, b2, b3], calls)
}

#[tokio::test(start_paused = true)]
async fn a_primarys_drain_waits_for_its_returned_backups_bootstrap() {
    let name = "drain-waits-for-the-backups-bootstrap";
    let (mut fh, alice, bob, proxy, [b1, mut b2, b3], mut calls) =
        b3_returns_behind(name, |fh| fh.delay("b2", "b3", FLOW_DELAY_MS)).await;

    let out = drain(&mut fh, &b2).await;
    assert_eq!(out.exit, DrainExit::CaughtUp, "b3's bootstrap carried every call: {out:?}");
    assert!(
        out.elapsed >= BOOTSTRAP_WAIT && out.elapsed < GRACE,
        "the drain waited for the bootstrap, inside its grace: {out:?}"
    );
    let held = b3.scan_backed_up("b2");
    assert!(calls.iter().all(|c| held.contains(&c.call_ref)), "b3 holds every call: {held:?}");
    exits(&mut fh, &mut b2).await;
    fh.heal("b2", "b3");
    published_once_ready(&mut fh, &b3, &proxy).await;

    for call in &mut calls {
        caller_hangs_up(&mut call.dialog, &bob).await;
    }
    let refs: Vec<&str> = calls.iter().map(|c| c.call_ref.as_str()).collect();
    let nodes = [&b1, &b2, &b3];
    settle(&fh, &nodes, &refs, &[&alice, &bob]).await;
    for r in refs {
        assert_eq!(total_cdrs_for(&nodes, r), 1, "one CDR for {r}");
    }
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn calls_a_returned_backup_has_not_received_by_the_grace_are_ended_off_the_wire() {
    let name = "drain-grace-before-the-backups-bootstrap";
    let (mut fh, alice, bob, proxy, [b1, mut b2, b3], mut calls) =
        b3_returns_behind(name, |fh| fh.stall("b2", "b3")).await;

    let t0 = fh.now_ms();
    let out = drain(&mut fh, &b2).await;
    assert_eq!(out.exit, DrainExit::GracePeersBehind, "b3 never reported holding: {out:?}");
    assert_eq!(out.residual, 3, "the three calls no peer holds");
    for call in &calls {
        let cdrs = b2.cdr_records().iter().filter(|r| r.call_ref == call.call_ref).count();
        assert_eq!(cdrs, 1, "b2 wrote {}'s CDR at the grace", call.call_ref);
    }
    let sent_by_b2 = fh
        .sip_entries()
        .iter()
        .filter(|e| e.from == b2.sip_addr() && e.sent_ms >= t0 as u64)
        .count();
    assert_eq!(sent_by_b2, 0, "the drain ends the calls off the wire");
    exits(&mut fh, &mut b2).await;
    fh.heal("b2", "b3");
    published_once_ready(&mut fh, &b3, &proxy).await;

    for call in &mut calls {
        let mut bye = call.dialog.bye().await;
        bye.expect(481).await;
        let mut bye = call.bob_dialog.bye().await;
        bye.expect(481).await;
    }
    let refs: Vec<&str> = calls.iter().map(|c| c.call_ref.as_str()).collect();
    let nodes = [&b1, &b2, &b3];
    settle(&fh, &nodes, &refs, &[&alice, &bob]).await;
    for r in refs {
        assert_eq!(total_cdrs_for(&nodes, r), 1, "one CDR for {r}, the drain's");
    }
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
