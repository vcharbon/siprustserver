//! **A re-INVITE answered after the node serving the call died.** The caller's
//! re-INVITE is relayed to the callee and the serving node's 100 reaches the
//! caller, so the caller's client transaction is `Proceeding` and sends no
//! retransmission that could take the call over (RFC 3261 §17.1.1.2). The
//! callee's 2xx answers an in-dialog request and carries no Record-Route
//! (§12.1.1, §16.6 step 4: only a dialog-creating response does): the front
//! proxy fails it over on the stickiness cookie of the Via it pushed on the
//! request, and the survivor takes the call over on it and completes the round
//! — the round is in the replica — so the caller gets its 200, both ACKs flow,
//! and the call ends with one CDR.

use std::time::Duration;

use failover_harness::FailoverHarness;
use sip_message::generators::InDialogMethod;

use crate::inflight_resend::{
    bring_up, establish, kill_serving_node, reboot_and_reclaim, settle_to_one_cdr, split_mut,
    survivor_of, synchronized_call_ref, REANSWER, REOFFER, REPLICATE,
};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

#[tokio::test(start_paused = true)]
async fn a_reinvite_answered_after_the_kill_completes_on_the_survivor() {
    let mut fh = FailoverHarness::new("reinvite-answer-failover", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── The re-INVITE is relayed, its 100 heard, the round replicated ────────
    let mut reinvite = dialog.reinvite(Some(REOFFER)).await;
    reinvite.expect(100).await;
    let mut at_bob = bob.receive("INVITE").await;
    fh.advance(REPLICATE).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;

    // ── The callee answers; the proxy fails the 200 over to the survivor ─────
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    reinvite.expect(200).await;
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_before + 1,
        "the failed-over 200 took the call over on the survivor",
    );
    dialog.ack(None).await;
    bob.receive("ACK").await;

    // ── The session lives on, then ends cleanly ─────────────────────────────
    let mut info = dialog.send_request(InDialogMethod::Info).send().await;
    bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
