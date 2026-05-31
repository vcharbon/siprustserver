//! In-dialog re-INVITE relay through the B2BUA (port of
//! `tests/scenarios/reinvite.ts`). The B2BUA bridges two independent dialogs;
//! a re-INVITE from either side is relayed transparently onto the peer dialog
//! and its response correlated back via the pending-relay snapshot
//! (`relay-reinvite-response`). When both peers re-INVITE at once, the loser is
//! rejected 491 Request Pending (`reinvite-glare`, RFC 3261 §14.1).
//!
//! Each test first establishes the call (INVITE → 180 → 200 → ACK), mirroring
//! the TS `callSetup` fragment the scenarios build on.
//!
//! ```text
//!   alice_reinvite          re-INVITE (no SDP) → 200(offer) → ACK(answer)
//!   bob_reinvite            re-INVITE (offer)  → 200(answer) → ACK
//!   crossing_reinvite_glare alice re-INVITE relayed; bob's crosses → 491
//! ```

use b2bua_harness::B2buaSut;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// re-INVITE from the caller with a delayed offer: alice sends a bodyless
/// re-INVITE, bob answers 200 with the SDP offer, alice ACKs with the answer.
#[tokio::test]
async fn alice_reinvite() {
    let h = Harness::with_transit_delay("b2bua-reinvite-alice", 0);
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5081", "127.0.0.1", 5071).await;

    // ── call setup ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice re-INVITEs with no SDP (delayed offer) ──
    let mut reinv = dialog.request(InDialogMethod::Invite, None).await;

    // Bob receives the relayed re-INVITE with no body.
    let mut bob_uas = bob.receive("INVITE").await;
    assert!(bob_uas.request().body.is_empty(), "re-INVITE relayed to bob with no body");

    // Bob answers 200 with the SDP offer.
    bob_uas.respond(200, "OK").with_sdp(REOFFER).await;

    // Alice receives the 200 with the SDP offer.
    let ok = reinv.expect(200).await;
    assert!(!ok.body.is_empty(), "re-INVITE 200 with offer relayed to alice");

    // Alice ACKs the re-INVITE 2xx with the SDP answer; bob receives the relayed
    // ACK carrying the answer.
    dialog.ack(Some(REANSWER)).await;
    let bob_ack = bob.receive("ACK").await;
    assert!(!bob_ack.request().body.is_empty(), "ACK answer relayed to bob");

    // ── teardown: alice hangs up ──
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}

/// re-INVITE from the callee with the offer in the re-INVITE: bob sends a
/// re-INVITE with SDP, alice answers 200 with the SDP answer, bob ACKs (no SDP).
#[tokio::test]
async fn bob_reinvite() {
    let h = Harness::with_transit_delay("b2bua-reinvite-bob", 0);
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", "127.0.0.1:5072").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5082", "127.0.0.1", 5072).await;

    // ── call setup ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // bob's confirmed (UAS-side) dialog — the callee originates the re-INVITE.
    let mut bob_dialog = uas.dialog();

    // ── bob re-INVITEs with the SDP offer ──
    let mut reinv = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;

    // Alice receives the relayed re-INVITE carrying the offer.
    let mut alice_uas = alice.receive("INVITE").await;
    assert!(!alice_uas.request().body.is_empty(), "re-INVITE offer relayed to alice");

    // Alice answers 200 with the SDP answer.
    alice_uas.respond(200, "OK").with_sdp(REANSWER).await;

    // Bob receives the 200 with the answer.
    let ok = reinv.expect(200).await;
    assert!(!ok.body.is_empty(), "re-INVITE 200 with answer relayed to bob");

    // Bob ACKs (offer/answer already complete, no SDP); alice receives the ACK.
    bob_dialog.ack(None).await;
    alice.receive("ACK").await;

    // ── teardown: bob hangs up ──
    let mut bye = bob_dialog.bye().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}

/// Crossing re-INVITEs (glare). Alice's re-INVITE is relayed to bob; before bob
/// answers it he sends his own re-INVITE, which the B2BUA detects as glare (a
/// pending inbound INVITE already sits on bob's dialog) and rejects 491 Request
/// Pending (RFC 3261 §14.1 / §3.1). Alice's re-INVITE then completes normally.
#[tokio::test]
async fn crossing_reinvite_glare() {
    let h = Harness::with_transit_delay("b2bua-reinvite-crossing", 0);
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5084", "127.0.0.1", 5074).await;

    // ── call setup ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    // ── alice re-INVITEs first (this one wins) ──
    let mut alice_reinv = alice_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;

    // Bob receives alice's relayed re-INVITE (the pending inbound INVITE now
    // sits on bob's dialog).
    let mut bob_uas = bob.receive("INVITE").await;

    // ── bob sends his own re-INVITE before answering — it loses the glare ──
    let mut bob_reinv = bob_dialog.request(InDialogMethod::Invite, Some(REANSWER)).await;

    // The B2BUA rejects bob's crossing re-INVITE 491 Request Pending (it never
    // reaches alice).
    bob_reinv.expect(491).await;

    // Bob now answers alice's re-INVITE 200 OK with the SDP answer.
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;

    // Alice receives the 200 for her re-INVITE.
    let ok = alice_reinv.expect(200).await;
    assert!(!ok.body.is_empty(), "alice's re-INVITE 200 carries the answer");

    // Alice ACKs; bob receives the ACK (the real one for alice's re-INVITE).
    alice_dialog.ack(None).await;
    bob.receive("ACK").await;

    // ── teardown: alice hangs up ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}
