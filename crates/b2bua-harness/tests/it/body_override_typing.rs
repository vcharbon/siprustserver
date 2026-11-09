//! A body the stack puts on an INVITE it mints in place of the caller's,
//! typed by RFC 3261 §20.15: the Content-Type "indicates the media type of the
//! message-body sent to the recipient". An override is typed by its own bytes
//! or by what its author states, never by the framing of the caller's body,
//! and the ACK of the 2xx carries the answer to the offer that INVITE held
//! (RFC 3264 §4): none, where the INVITE made one.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{BodyUpdate, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::MediaType;
use sip_message::SipRequest;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const REPLACEMENT: &str = "v=0\r\no=svc 7 7 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 40000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const CHARLIE_ANSWER: &str = "v=0\r\no=charlie 9 8 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const CHARLIE_PORT: u16 = 5667;
const MULTIPART: &str = "multipart/mixed;boundary=X";

/// Alice's offer framed beside a part of another type (RFC 5621 §3).
fn framed_offer() -> Vec<u8> {
    format!(
        "--X\r\nContent-Type: application/sdp\r\n\r\n{OFFER}\r\n\
         --X\r\nContent-Type: application/vnd.example.data\r\n\r\nopaque\r\n--X--\r\n"
    )
    .into_bytes()
}

/// The media type `req` states for its body.
fn stated_type(req: &SipRequest) -> Option<String> {
    req.header::<MediaType>().and_then(Result::ok).map(|t| t.token().to_string())
}

/// The session description `req` holds, as text.
fn description(req: &SipRequest) -> Option<String> {
    req.sdp().map(|sdp| String::from_utf8_lossy(sdp).into_owned())
}

/// A REFER on a call whose caller framed her offer: the INVITE to the target
/// carries the held offer as a bare description, typed `application/sdp`, and
/// the ACK of the target's 2xx carries no second one.
#[tokio::test(start_paused = true)]
async fn the_transfer_targets_invite_is_typed_by_its_own_bytes() {
    let h = Harness::with_transit_delay("override-typing-transfer-of-a-framed-caller", 1);
    let alice = h.agent("alice", "127.0.0.1:5961").await;
    let bob = h.agent("bob", "127.0.0.1:5971").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await.transfer_target();
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5971)
        .start(&h, "b2bua", "127.0.0.1:5981")
        .await;

    let mut call =
        alice.invite(&bob).with_body(MULTIPART, framed_offer()).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>"))
        .with_header(
            "X-Api-Call",
            &format!(
                r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#
            ),
        )
        .send()
        .await;
    refer.expect(202).await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    let invite_type = stated_type(charlie_uas.request());
    let invite_offer = description(charlie_uas.request());
    charlie_uas.respond(200, "OK").with_sdp(CHARLIE_ANSWER).await;
    let ack = charlie.receive("ACK").await;
    let ack_body = ack.request().body().len();

    bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut c_realign = charlie.receive("INVITE").await;
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ANSWER).await;
    charlie.receive("ACK").await;
    let mut a_realign = alice.receive("INVITE").await;
    a_realign.respond(200, "OK").with_sdp(OFFER).await;
    alice.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;

    assert_eq!(invite_type.as_deref(), Some("application/sdp"), "the held offer's own type");
    assert!(
        invite_offer.is_some_and(|sdp| sdp.contains("a=inactive")),
        "the held offer reads as a session description at the target"
    );
    assert_eq!(ack_body, 0, "the INVITE made the offer: the ACK carries no answer");
}

fn replacing_engine(port: u16) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", port);
                r.update_body = BodyUpdate::Replace(REPLACEMENT.to_string());
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// `update_body` Replace on a caller whose offer is framed: bob is offered the
/// replacement, typed `application/sdp`, and his ACK carries none of alice's
/// description.
#[tokio::test(start_paused = true)]
async fn a_replaced_body_on_a_framed_caller_is_typed_by_its_own_bytes() {
    let h = Harness::with_transit_delay("override-typing-replace-on-a-framed-caller", 1);
    let alice = h.agent("alice", "127.0.0.1:5962").await;
    let bob = h.agent("bob", "127.0.0.1:5972").await;
    let b2bua =
        B2buaSut::builder(replacing_engine(5972)).start(&h, "b2bua", "127.0.0.1:5982").await;

    let mut call =
        alice.invite(&bob).with_body(MULTIPART, framed_offer()).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    let invite_type = stated_type(uas.request());
    let invite_offer = description(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    let ack = bob.receive("ACK").await;
    let ack_body = ack.request().body().len();
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;

    assert_eq!(invite_type.as_deref(), Some("application/sdp"), "the replacement's own type");
    assert_eq!(invite_offer.as_deref(), Some(REPLACEMENT), "the replaced offer reads at bob");
    assert_eq!(ack_body, 0, "the INVITE made the offer: the ACK carries no answer");
}

/// `update_body` Replace on a delayed-offer INVITE: the replacement is an
/// offer typed `application/sdp` (RFC 3261 §20.15), and alice's description
/// in her ACK answers bob's 2xx on her face only.
#[tokio::test(start_paused = true)]
async fn a_replaced_body_on_a_delayed_offer_carries_a_content_type() {
    let h = Harness::with_transit_delay("override-typing-replace-on-a-delayed-offer", 1);
    let alice = h.agent("alice", "127.0.0.1:5963").await;
    let bob = h.agent("bob", "127.0.0.1:5973").await;
    let b2bua =
        B2buaSut::builder(replacing_engine(5973)).start(&h, "b2bua", "127.0.0.1:5983").await;

    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    let invite_type = stated_type(uas.request());
    let invite_offer = description(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack_with(Some(OFFER)).await;
    let ack = bob.receive("ACK").await;
    let ack_body = ack.request().body().len();
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;

    assert_eq!(invite_type.as_deref(), Some("application/sdp"), "a body carries its type");
    assert_eq!(invite_offer.as_deref(), Some(REPLACEMENT), "the replaced offer reads at bob");
    assert_eq!(ack_body, 0, "the INVITE made the offer: the ACK carries no answer");
}
