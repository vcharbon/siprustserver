//! **A setup still ringing at a drain's grace gets its final** (ADR-0022,
//! ADR-0055 §3). The call's backup refused its replica, so no peer holds it,
//! and its callee rings past the grace of its primary's drain. At the grace
//! the primary ends the setup as any shutdown of a B2BUA does: the caller's
//! INVITE draws a final, the callee's INVITE a CANCEL, and the call writes its
//! CDR. Nothing is left waiting.
//!
//! ```text
//!   alice ─INVITE─▶ primary ─INVITE─▶ bob ─180─▶ … (the backup refused the replica)
//!   the primary drains; grace ⇒ primary ─503─▶ alice ─ACK─▶
//!                                primary ─CANCEL─▶ bob ─200─▶, ─487─▶ ─ACK─▶
//! ```
//!
//! Beyond the failover sweep: a setup no peer holds at a drain's grace.

use std::time::Duration;

use b2bua::capacity::BackupCeilings;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, DrainExit, FailoverHarness,
    ReplicatedB2buaSut,
};

use crate::away_backup_support::{Limiter, ALICE, B1, B2, BOB, BOUNDS, GRACE, OFFER, PROXY};

#[tokio::test(start_paused = true)]
async fn a_setup_ringing_at_the_drains_grace_gets_a_final_and_its_callee_a_cancel() {
    let replica = b2bua::ReplicaConfig {
        backup: BackupCeilings { calls: Some(0), rss_bytes: None },
        ..Default::default()
    };
    let name = "drain-grace-ringing-setup";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]).with_worker_replica(replica);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let limiter = Limiter::serve().await;
    let mut w_b1 = limiter.worker(&mut fh, "b1", B1, &["b2"]).await;
    let mut w_b2 = limiter.worker(&mut fh, "b2", B2, &["b1"]).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── a ringing setup whose backup refused its replica ─────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _) = worker_ordinals(uas.request());
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;
    let primary: &mut ReplicatedB2buaSut = if pri_ord == "b1" { &mut w_b1 } else { &mut w_b2 };
    let call_ref = primary.scan_primary(&pri_ord).pop().expect("the primary stores the call");

    // ── the primary drains to its grace ──────────────────────────────────────
    let t0 = fh.now_ms();
    fh.withdraw_routing(&pri_ord);
    let mut drain = fh.begin_drain_pending(primary, BOUNDS);
    while fh.now_ms() - t0 < GRACE.as_millis() as i64 {
        let early = drain.poll();
        assert!(early.is_none(), "no exit while the setup rings, got {early:?}");
        fh.advance(Duration::from_millis(100)).await;
    }

    // ── at the grace: the caller's final, the callee's CANCEL ───────────────
    assert!(drain.poll().is_none(), "the grace ends the setup and waits for it to leave");
    call.expect(503).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    let mut outcome = None;
    for _ in 0..40 {
        if let Some(o) = drain.poll() {
            outcome = Some(o);
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    drop(drain);
    let outcome = outcome.expect("the drain returned");
    assert_eq!(outcome.exit, DrainExit::GracePeersBehind, "an unbacked setup is never held");
    assert_eq!(outcome.residual, 1, "the setup no peer held is the residual");
    assert_eq!(
        primary.cdr_records().iter().filter(|r| r.call_ref == call_ref).count(),
        1,
        "the setup's CDR is written at the grace"
    );
    assert_eq!(limiter.held(), 0, "the setup's limiter set is released at the grace");

    // ── the primary exits ────────────────────────────────────────────────────
    fh.depart(&pri_ord);
    fh.mark(&pri_ord, None, "crash", "the drained process exits");
    primary.crash();
    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "both nodes release the call");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
