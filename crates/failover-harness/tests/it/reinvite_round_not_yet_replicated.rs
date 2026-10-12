//! **A re-INVITE round answered after the serving node died, before the round
//! reached the backup.** The caller heard the serving node's relayed 100, so it
//! is `Proceeding` and sends nothing more (RFC 3261 §17.1.1.2). The callee's 200
//! fails over to the survivor on the cookie of the proxy's Via, and the survivor
//! takes the call over on it — but the replica it holds predates the round, so
//! no transaction there takes the 200: nothing reaches the caller, the 200 is
//! never ACKed, and the copy releases itself. This is the in-dialog twin of a
//! new call's first-replica window (`new_call_replication_window.rs`): from the
//! serving node's 100 until the round's replica lands, its death leaves the
//! round to the parties' own timers. The parties then end the dialog: the
//! caller's BYE takes the call over again and reaches the callee.

use std::time::Duration;

use failover_harness::{FailoverHarness, PartitionRole};
use scenario_harness::agent::Agent;

use crate::inflight_resend::{
    bring_up, establish, kill_serving_node, reboot_and_reclaim, settle_to_one_cdr, split_mut,
    survivor_of, synchronized_call_ref, REANSWER, REOFFER, REPLICATE,
};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// How many datagrams reached `agent`'s TU, drained without waiting.
async fn heard(agent: &Agent) -> usize {
    agent.drain_tu().await
}

#[tokio::test(start_paused = true)]
async fn a_round_answered_before_it_replicated_reaches_no_one() {
    let mut fh = FailoverHarness::new("reinvite-round-not-yet-replicated", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;
    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;
    let before_round = survivor_of(&pri_ord, &w_b1, &w_b2)
        .get(PartitionRole::Backup, &pri_ord, &call_ref)
        .await
        .expect("the established call's replica");

    // ── The round is relayed and its 100 heard; its replica never lands ─────
    let mut reinvite = dialog.reinvite(Some(REOFFER)).await;
    reinvite.expect(100).await;
    let mut at_bob = bob.receive("INVITE").await;
    at_bob.respond(100, "Trying").await;
    fh.advance(REPLICATE).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    survivor_of(&pri_ord, &w_b1, &w_b2)
        .rewind_replica(PartitionRole::Backup, &pri_ord, &call_ref, before_round)
        .await;

    // ── The callee's 200 takes the call over, and reaches no one ────────────
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    fh.advance(Duration::from_secs(3)).await;
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_before + 1,
        "the failed-over 200 took the call over",
    );
    assert!(!survivor.serves(&call_ref), "the copy released itself");
    assert_eq!(heard(&alice).await, 0, "the caller hears nothing of the round");
    assert_eq!(heard(&bob).await, 0, "the callee's 200 is never ACKed");

    // ── The parties end the dialog ──────────────────────────────────────────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
