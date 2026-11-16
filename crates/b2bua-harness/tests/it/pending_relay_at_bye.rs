//! A request relayed toward a party who hangs up before answering it: the
//! sender's dialog still exists until this stack's BYE ends it, so the
//! pending request is answered in it 487 Request Terminated (RFC 3261
//! §15.1.2), never 481, which tells its sender the dialog does not exist
//! (§12.2.1.2) just before a BYE arrives inside it.

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// Bob's INFO and UPDATE reach alice, who hangs up before answering them:
/// alice's BYE is answered 200, each of bob's requests 487 in his dialog,
/// then the B2BUA's BYE ends that dialog. Alice's late answers go no
/// further, and the call reaps clean.
#[tokio::test(start_paused = true)]
async fn requests_pending_toward_a_party_who_hangs_up_are_answered_487() {
    let h = Harness::with_transit_delay("pending-relay-at-bye", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut info = bob_dialog.send_request(InDialogMethod::Info).send().await;
    let mut alice_info = alice.receive("INFO").await;
    let mut update = bob_dialog.send_request(InDialogMethod::Update).send().await;
    let mut alice_update = alice.receive("UPDATE").await;

    alice_dialog.bye().await.expect(200).await;
    let terminated = info.expect(487).await;
    assert_eq!(terminated.to().tag(), Some(bob_dialog.remote_tag()), "INFO: in bob's dialog");
    let terminated = update.expect(487).await;
    assert_eq!(terminated.to().tag(), Some(bob_dialog.remote_tag()), "UPDATE: in bob's dialog");
    bob.receive("BYE").await.respond(200, "OK").await;

    alice_info.respond(200, "OK").await;
    alice_update.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
