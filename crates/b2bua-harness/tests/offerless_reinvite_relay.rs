//! An offerless (bodyless) in-dialog INVITE relays bodyless, from EITHER face.
//!
//! RFC 3261 §14.2 / RFC 3264 §5: an INVITE with no session description hands the
//! offer to its RECIPIENT — the offer travels in the 2xx and the answer in the
//! ACK. Substituting a body of the B2BUA's own on the relayed leg would flip
//! offer ownership, so the recipient answers constraints it never stated.
//!
//! `reinvite.rs::alice_reinvite` pins the originator→originated direction; this
//! pins the originated→originator one, where the relay target is the a-leg.

use b2bua_harness::B2buaSut;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// The callee re-INVITEs with a delayed offer: bob sends a bodyless in-dialog
/// INVITE, alice answers 200 with the offer, bob ACKs with the answer. Every
/// relayed hop carries exactly the body ownership its originator stated.
#[tokio::test]
async fn callee_offerless_reinvite_relays_offerless() {
    let h = Harness::with_transit_delay("b2bua-offerless-reinvite-callee", 0);
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5073).start(&h, "b2bua", "127.0.0.1:5083").await;

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

    // ── bob re-INVITEs with NO body (delayed offer) ──
    let mut reinv = bob_dialog.request(InDialogMethod::Invite, None).await;

    // The relayed re-INVITE must reach alice bodyless — the offer is HERS to make.
    let mut alice_uas = alice.receive("INVITE").await;
    assert!(
        alice_uas.request().body().is_empty(),
        "offerless re-INVITE relayed to alice with a substituted body"
    );

    // ── alice answers 200 carrying her offer; bob sees it ──
    alice_uas.respond(200, "OK").with_sdp(REOFFER).await;
    let ok = reinv.expect(200).await;
    assert!(!ok.body().is_empty(), "the 200 must carry alice's offer to bob");

    // ── bob ACKs with his answer; alice sees it on her leg ──
    bob_dialog.ack(Some(REANSWER)).await;
    let alice_ack = alice.receive("ACK").await;
    assert!(!alice_ack.request().body().is_empty(), "the ACK must carry bob's answer to alice");

    // ── teardown ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    b2bua.assert_fully_reaped();
}
