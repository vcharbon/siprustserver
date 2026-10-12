//! **A taken-over re-INVITE round the callee never answers.** The caller's
//! re-INVITE is relayed to the callee and replicated; its serving node's 100 is
//! lost on the way to the caller, so the caller's re-send takes the call over
//! on the survivor (RFC 3261 §17.1.1.2). The callee answered the relayed
//! re-INVITE with a 100 at once, to the serving node, and says nothing more:
//! the survivor's seeded client transaction gives up at the INVITE bound, the
//! survivor ends the dialog it got no answer on (§12.2.1.2) — the caller's
//! pending re-INVITE draws its 487 (§15.1.2), the callee draws no CANCEL (the
//! record holds no provisional for the round, §9.1) and both parties draw a
//! BYE, the callee answering its pending re-INVITE 487 (§15.1.2) — and the
//! call ends with one CDR and full release on both nodes.
//!
//! Two ways to the takeover: the caller's re-send with the kill unannounced to
//! the peers (ADR-0031 cases 3a and 4), or — the caller having heard the 100,
//! so sending nothing more — the departure announced, on which the survivor
//! takes over the call whose round is in flight (ADR-0046).

use std::time::Duration;

use failover_harness::FailoverHarness;
use scenario_harness::agent::Inbound;
use sip_message::generators::InDialogMethod;
use sip_message::{Method, SipMessage};

use crate::departure_support::kill_announced;
use crate::inflight_resend::{
    assert_not_taken_over, assert_taken_over_once, bring_up, establish, kill_serving_node,
    last_request_sent, reboot_and_reclaim, round_invites_sent_to, settle_to_one_cdr, split_mut,
    survivor_of, synchronized_call_ref, KILL_SETTLE, REOFFER, REPLICATE, T1,
};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// The INVITE bound the workers run under — the give-up of a seeded client
/// INVITE, measured from the seed.
fn invite_bound() -> Duration {
    Duration::from_secs(b2bua::B2buaConfig::default().invite_txn_timeout_sec as u64)
}

#[tokio::test(start_paused = true)]
async fn a_taken_over_reinvite_round_the_callee_never_answers_gives_up_at_its_bound() {
    let mut fh = FailoverHarness::new("reinvite-round-gives-up", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── In flight, replicated; the serving node's 100 lost on the way ───────
    let reinvite = dialog.send_request(InDialogMethod::Invite).with_sdp(REOFFER).send().await;
    fh.cut_signalling("alice", alice.addr());
    let mut at_bob = bob.receive("INVITE").await;
    at_bob.respond(100, "Trying").await;
    fh.advance(REPLICATE).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    fh.restore_signalling("alice", alice.addr());
    fh.advance(3 * T1 - REPLICATE - KILL_SETTLE).await;
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    // ── The caller's re-send takes the call over ────────────────────────────
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Invite);
    alice.try_send_datagram(&wire, dst).await.expect("the peer's re-send leaves");
    fh.advance(Duration::from_millis(200)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);
    assert_eq!(round_invites_sent_to(&fh, bob.addr(), REOFFER), 1, "one b-leg INVITE");

    // ── The seeded round gives up at its bound, not before ──────────────────
    fh.advance(invite_bound() - Duration::from_secs(1)).await;
    while let Some(msg) = alice.take_queued().await {
        let provisional = matches!(&msg, SipMessage::Response(r) if r.status() < 200);
        assert!(provisional, "nothing but a provisional reaches the caller before the bound");
    }
    fh.advance(Duration::from_millis(1_500)).await;
    let round_final = loop {
        match alice.recv_any().await.expect("the caller draws a response to its re-INVITE") {
            Inbound::Response(r) if r.status() < 200 => continue,
            Inbound::Response(r) => break r,
            Inbound::Request(txn) => {
                panic!("expected the re-INVITE's final, got a {}", txn.request().method())
            }
        }
    };
    assert_eq!(round_final.status(), 487, "the pending re-INVITE ends with the dialog");
    reinvite.ack_non_2xx(&round_final).await.expect("the caller hop-ACKs the 487");
    alice.receive("BYE").await.respond(200, "OK").await;
    // The callee's next request is the BYE: no CANCEL comes first (§9.1).
    let mut bye = bob.receive("BYE").await;
    bye.respond(200, "OK").await;
    at_bob.respond(487, "Request Terminated").await;
    fh.advance(Duration::from_millis(500)).await;

    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_round_in_flight_at_its_nodes_departure_gives_up_at_its_bound() {
    let mut fh = FailoverHarness::new("reinvite-round-gives-up-departure", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── In flight and replicated; the caller heard the 100 ─────────────────
    let mut reinvite = dialog.reinvite(Some(REOFFER)).await;
    reinvite.expect(100).await;
    let mut at_bob = bob.receive("INVITE").await;
    at_bob.respond(100, "Trying").await;
    fh.advance(REPLICATE).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);
    assert_eq!(round_invites_sent_to(&fh, bob.addr(), REOFFER), 1, "one b-leg INVITE");

    // ── The seeded round gives up at its bound, not before ──────────────────
    fh.advance(invite_bound() - Duration::from_secs(1)).await;
    while let Some(msg) = alice.take_queued().await {
        let provisional = matches!(&msg, SipMessage::Response(r) if r.status() < 200);
        assert!(provisional, "nothing but a provisional reaches the caller before the bound");
    }
    fh.advance(Duration::from_millis(1_500)).await;
    let round_final = reinvite.expect(487).await;
    assert_eq!(round_final.status(), 487, "the pending re-INVITE ends with the dialog");
    alice.receive("BYE").await.respond(200, "OK").await;
    let mut bye = bob.receive("BYE").await;
    bye.respond(200, "OK").await;
    at_bob.respond(487, "Request Terminated").await;
    fh.advance(Duration::from_millis(500)).await;

    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
