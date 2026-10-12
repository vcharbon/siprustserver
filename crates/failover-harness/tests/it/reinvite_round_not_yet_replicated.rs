//! **A re-INVITE round answered after the serving node died, before the round
//! reached the backup.** The caller heard the serving node's relayed 100, so it
//! is `Proceeding` and sends nothing more (RFC 3261 §17.1.1.2). The callee's 200
//! fails over to the survivor on the cookie of the proxy's Via, and the survivor
//! takes the call over on it — but the replica it holds predates the round, so
//! no transaction there takes the 200: nothing reaches the caller, the 200 is
//! never ACKed, and the copy releases itself. The callee retransmits its 200 on
//! the §13.3.1.4 ladder, and each copy takes the call over again and is released
//! again, until the ladder ends at 64·T1. This is the in-dialog twin of a new
//! call's first-replica window (`new_call_replication_window.rs`): from the
//! serving node's 100 until the round's replica lands, its death leaves the
//! round unresolved. The caller's pending re-INVITE gets no final from the
//! platform (its client transaction is `Proceeding`, which runs no Timer B, and
//! the survivor answers the caller's BYE from a replica that holds no round to
//! end with a 487, §15.1.2): only the caller's own application timer ends it.
//! The caller then ends the dialog with a BYE, which takes the call over again
//! and reaches the callee.

use std::time::Duration;

use failover_harness::{FailoverHarness, PartitionRole};
use scenario_harness::agent::Agent;

use crate::inflight_resend::{
    bring_up, establish, kill_serving_node, reboot_and_reclaim, settle_to_one_cdr, split_mut,
    survivor_of, synchronized_call_ref, REANSWER, REOFFER, REPLICATE, T1,
};

/// RFC 3261 §17.1.2.2's T2: where the §13.3.1.4 2xx ladder stops doubling.
const T2: Duration = Duration::from_secs(4);

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

    // ── The callee's 200 and its §13.3.1.4 ladder: each copy takes the call ─
    // ── over, reaches no one, and is released ────────────────────────────────
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    let (answer, toward) = fh
        .sip_entries()
        .into_iter()
        .rfind(|e| e.from == bob.addr() && e.raw.starts_with(b"SIP/2.0 200"))
        .map(|e| (e.raw, e.to))
        .expect("the callee's 200 is on the wire");
    let mut copies: u64 = 1;
    let mut gap = T1;
    let mut elapsed = Duration::ZERO;
    while elapsed + gap < 64 * T1 {
        fh.advance(gap).await;
        elapsed += gap;
        bob.try_send_datagram(&answer, toward).await.expect("the retransmitted 200 leaves");
        copies += 1;
        gap = (gap * 2).min(T2);
    }
    fh.advance(Duration::from_millis(200)).await;
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_before + copies,
        "each copy of the 200 took the call over again",
    );
    assert!(!survivor.serves(&call_ref), "every copy released itself");
    assert_eq!(heard(&alice).await, 0, "the caller hears nothing of the round");
    assert_eq!(heard(&bob).await, 0, "the callee's 200 is never ACKed");

    // ── The caller ends the dialog; its re-INVITE never gets a final ────────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    fh.advance(Duration::from_secs(1)).await;
    assert_eq!(heard(&alice).await, 0, "no final for the pending re-INVITE, after the BYE either");
    assert_eq!(
        survivor_of(&pri_ord, &w_b1, &w_b2).metrics().repl_takeover_hydrated_total(),
        hydrated_before + copies + 1,
        "the BYE took the call over once more",
    );

    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
