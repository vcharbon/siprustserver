//! A request the B2BUA relays in no dialog is answered here, never left to run
//! out the sender's Timer F (RFC 3261 §8.2, §17.2.2), and the call goes on:
//! an in-dialog SUBSCRIBE names an event package this stack serves no
//! subscription of, 489 Bad Event (RFC 6665 §4.2.1.2); any other such method,
//! a PUBLISH here, draws 405 with the node's `Allow` (§8.2.1).

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut, ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Harness, Inbound};
use sip_message::generators::{
    generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
};
use sip_message::header::{self, Uri, Via};
use sip_message::{SipHeader, SipMessage, SipResponse, SipStr};

fn uri(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).expect("readable URI")
}

fn line(name: &'static str, value: &'static str) -> SipHeader {
    SipHeader { name: SipStr::from_static(name), value: SipStr::from_static(value) }
}

/// alice sends `method` in her confirmed dialog, as a request of that dialog
/// (RFC 3261 §12.2.1.1); the final the B2BUA answers it with. The callee is
/// never offered it, and the call ends with alice's BYE.
async fn answered_in_dialog(name: &str, method: OutOfDialogMethod) -> SipResponse {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).through(b2bua.addr).send().await;
    let call_id = call.call_id();
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let cseq = dialog.local_cseq() + 1;
    let request = generate_out_of_dialog_request(
        method,
        &GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri(&format!("sip:{}", b2bua.addr))),
            call_id,
            from: Some(
                header::From::from_uri(uri(&format!("sip:alice@{}", alice.addr().ip())))
                    .with_tag(SipStr::owned(dialog.local_tag())),
            ),
            to: Some(
                header::To::from_uri(uri(&format!("sip:bob@{}", bob.addr().ip())))
                    .with_tag(SipStr::owned(dialog.remote_tag())),
            ),
            cseq,
            via: Some(
                Via::udp(alice.addr().ip().to_string(), alice.addr().port())
                    .with_branch(SipStr::from_static("z9hG4bK-unrelayed-1")),
            ),
            contact: Some(header::Contact::from_uri(uri(&format!("sip:alice@{}", alice.addr())))),
            max_forwards: Some(70),
            body: Vec::new(),
            content_type: None,
            extra_headers: vec![line("Event", "dialog"), line("Expires", "600")],
        },
    );
    dialog.set_local_cseq(cseq);
    let wire = sip_message::serializer::serialize(&SipMessage::Request(request));
    alice.try_send_datagram(&wire, b2bua.addr).await.expect("the request leaves");
    let answer = match alice.recv_any().await.expect("the request draws its final") {
        Inbound::Response(r) => r,
        Inbound::Request(r) => panic!("alice expected a final, got {}", r.request().method()),
    };
    h.advance(Duration::from_millis(500)).await;
    assert!(
        bob.try_receive_tolerating(method.as_str(), &[]).await.is_none(),
        "the {} is not relayed",
        method.as_str()
    );

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
    answer
}

#[tokio::test(start_paused = true)]
async fn an_in_dialog_subscribe_draws_489_and_the_call_goes_on() {
    let answer =
        answered_in_dialog("b2bua-in-dialog-subscribe", OutOfDialogMethod::Subscribe).await;
    assert_eq!(answer.status(), 489, "an event package this stack serves no subscription of");
}

#[tokio::test(start_paused = true)]
async fn an_in_dialog_publish_draws_405_with_allow_and_the_call_goes_on() {
    let answer = answered_in_dialog("b2bua-in-dialog-publish", OutOfDialogMethod::Publish).await;
    assert_eq!(answer.status(), 405, "a method this stack relays in no dialog");
    let allow = answer.raw(sip_message::header::HeaderName::Allow).next().map(|v| v.to_string());
    assert!(allow.is_some_and(|a| a.contains("INVITE")), "the 405 states the node's Allow");
}
