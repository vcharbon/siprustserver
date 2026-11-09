//! RFC 3261 §7.3.3: "A compact form MAY be substituted for the longer form of a
//! header field name at any time without changing the semantics of the
//! message." RFC 3515 §2.1 registers `r` for Refer-To, RFC 6665 §8.2.1 `o` for
//! Event: a REFER and a NOTIFY under those names go the way they go under the
//! long ones.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Dialog, Harness, InDialogTxn};
use sip_message::generators::InDialogMethod;
use sip_message::header::Event;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const CHARLIE_PORT: u16 = 5669;

/// A REFER on a call that processes REFER locally, naming its target under
/// `r`: accepted 202 and carried out as under `Refer-To`, here to a target
/// that is busy.
#[tokio::test(start_paused = true)]
async fn a_refer_under_the_compact_refer_to_is_carried_out() {
    let h = Harness::with_transit_delay("compact-refer-to", 1);
    let alice = h.agent("alice", "127.0.0.1:5964").await;
    let bob = h.agent("bob", "127.0.0.1:5974").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await.transfer_target();
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5974)
        .start(&h, "b2bua", "127.0.0.1:5984")
        .await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("r", &format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>"))
        .with_header(
            "X-Api-Call",
            &format!(
                r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#
            ),
        )
        .send()
        .await;
    refer.expect(202).await;

    let mut trying = bob.receive("NOTIFY").await;
    trying.respond(200, "OK").await;
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(486, "Busy Here").await;
    charlie.receive("ACK").await;
    let mut terminated = bob.receive("NOTIFY").await;
    let body = String::from_utf8_lossy(terminated.request().body()).into_owned();
    assert!(body.contains("SIP/2.0 486"), "the final NOTIFY reports the target's 486: {body:?}");
    terminated.respond(200, "OK").await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The two parties of a relayed REFER: the referrer's REFER crosses, its
/// recipient's NOTIFY stating the package under `o` comes back.
async fn relayed_refer(name: &str, alice_port: u16, bob_port: u16, b2bua_port: u16) -> Relayed {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent("alice", &format!("127.0.0.1:{alice_port}")).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port}")).await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob_port)
        .start(&h, "b2bua", &format!("127.0.0.1:{b2bua_port}"))
        .await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", "<sip:charlie@example.com>")
        .send()
        .await;
    alice.receive("REFER").await.respond(202, "Accepted").await;
    refer.expect(202).await;

    let notify = alice_dialog
        .send_request(InDialogMethod::Notify)
        .with_header("o", "refer")
        .with_header("Subscription-State", "terminated;reason=noresource")
        .with_body("message/sipfrag;version=2.0", b"SIP/2.0 200 OK\r\n".to_vec())
        .send()
        .await;
    Relayed { h, alice, bob, b2bua, alice_dialog, notify }
}

struct Relayed {
    h: Harness,
    alice: Agent,
    bob: Agent,
    b2bua: B2buaSut,
    alice_dialog: Dialog,
    notify: InDialogTxn,
}

/// The NOTIFY of a relayed REFER stating its package as `o` crosses as under
/// `Event`, its package readable on the far side.
#[tokio::test(start_paused = true)]
async fn a_notify_under_the_compact_event_crosses_the_relay() {
    let Relayed { h, alice: _alice, bob, b2bua, mut alice_dialog, mut notify } =
        relayed_refer("compact-event", 5967, 5977, 5987).await;
    let mut relayed = bob.receive("NOTIFY").await;
    let event = relayed.request().header::<Event>().and_then(Result::ok);
    assert!(
        event.as_ref().is_some_and(|e| e.is("refer")),
        "the relayed NOTIFY states the refer package, got {event:?}"
    );
    relayed.respond(200, "OK").await;
    notify.expect(200).await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// RFC 6665 §4.4.1: a 481 answering a NOTIFY ends the subscription, not the
/// dialog. Under `o` the NOTIFY is still a subscription's, so the call stands.
#[tokio::test(start_paused = true)]
async fn a_481_to_a_notify_under_the_compact_event_leaves_the_call_up() {
    let Relayed { h, alice: _alice, bob, b2bua, mut alice_dialog, mut notify } =
        relayed_refer("compact-event-481", 5968, 5978, 5988).await;
    bob.receive("NOTIFY").await.respond(481, "Call/Transaction Does Not Exist").await;
    notify.expect(481).await;
    h.advance(Duration::from_secs(1)).await;
    let at_bob = bob.take_queued().await.map(|m| m.cseq().method().as_str().to_string());
    assert!(at_bob.is_none(), "the call stands, bob got {at_bob:?}");

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
