//! **RFC 3262 §5 — the PRACK this stack sends as it CANCELs a delayed-offer
//! INVITE answers the provisional's offer.** The INVITE carried no offer, so a
//! reliable provisional carrying a description carries the OFFER, and the PRACK
//! acknowledging it MUST carry the answer. The setup is going away, so the
//! answer rejects every stream (RFC 3264 §6, port 0) — the least a dialog about
//! to end can commit to.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};

const BOB_OFFER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The callee's own `RSeq`.
const BOB_RSEQ: u32 = 913;

/// The PRACK's body rejects the one offered stream: an `m=audio 0` line.
fn assert_rejecting_answer(body: &[u8]) {
    let text = String::from_utf8_lossy(body);
    assert!(
        text.lines().any(|l| l.starts_with("m=audio 0 ")),
        "the PRACK answers the offer, rejecting its stream (RFC 3264 §6): {text:?}"
    );
}

/// The offer rides a reliable 183 that crosses this stack's CANCEL.
#[tokio::test(start_paused = true)]
async fn a_crossing_offer_is_answered_in_the_prack() {
    let h = Harness::with_transit_delay("b2bua-prack-delayed-offer-crossing", 1);
    let alice = h.agent("alice", "127.0.0.1:5211").await;
    let bob = h.agent("bob", "127.0.0.1:5212").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5212).start(&h, "b2bua", "127.0.0.1:5213").await;

    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    assert!(b_inv.request().sdp().is_none(), "a delayed offer: the INVITE carries none");
    b_inv.respond(100, "Trying").await;
    h.advance(Duration::from_millis(20)).await;
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    b_inv
        .respond(183, "Session Progress")
        .with_to_tag("bob-early")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(BOB_OFFER)
        .await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    let mut prack = bob.receive("PRACK").await;
    assert_rejecting_answer(prack.request().body());
    prack.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").with_to_tag("bob-early").await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The offer was relayed to the caller, who CANCELs instead of PRACKing: the
/// PRACK this stack owes the callee in her place answers it.
#[tokio::test(start_paused = true)]
async fn a_relayed_offer_the_caller_abandons_is_answered_in_the_prack() {
    let h = Harness::with_transit_delay("b2bua-prack-delayed-offer-relayed", 1);
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice CANCELs instead of PRACKing (and answering) the offer — the PRACK this stack \
             owes bob in her place is the subject",
        )
        .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5214").await;
    let bob = h.agent("bob", "127.0.0.1:5215").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5215).start(&h, "b2bua", "127.0.0.1:5216").await;

    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv
        .respond(183, "Session Progress")
        .with_to_tag("bob-early")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(BOB_OFFER)
        .await;
    call.expect(183).await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    let mut prack = bob.receive("PRACK").await;
    assert_rejecting_answer(prack.request().body());
    prack.respond(200, "OK").await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").with_to_tag("bob-early").await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
