//! A dialog whose BYE is in flight answers in-dialog requests 481 locally
//! (`post-bye-481`, RFC 3261 §15.1.2 / §12.2.2): the caller hangs up, the
//! B2BUA relays the BYE toward the callee, and the callee fires one more
//! session-refresh re-INVITE before answering that BYE. The re-INVITE is
//! answered 481 by the B2BUA itself — never relayed to the caller, who
//! already hung up — while the BYE transaction completes 200 and the call
//! tears down fully with no leaked state.

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";

#[tokio::test]
async fn reinvite_during_bye_gets_481_locally() {
    let h = Harness::with_transit_delay("b2bua-reinvite-after-bye", 0);
    let alice = h.agent("alice", "127.0.0.1:5069").await;
    let bob = h.agent("bob", "127.0.0.1:5079").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5079).start(&h, "b2bua", "127.0.0.1:5089").await;

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
    assert_eq!(b2bua.active_calls(), 1, "call established");

    // ── alice hangs up; the B2BUA relays the BYE toward bob ──
    let mut bye = alice_dialog.bye().await;
    let mut bob_bye = bob.receive("BYE").await;

    // ── before answering the BYE, bob fires a session-refresh re-INVITE: the
    //    B2BUA answers it 481 itself (`post-bye-481`) — it never reaches alice,
    //    whose dialog is already gone. ──
    let mut reinv = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    reinv.expect(481).await;

    // ── the BYE transaction completes normally ──
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;

    // ── full teardown: the 481 did not wedge or leak the call record ──
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
