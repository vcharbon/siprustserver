//! RFC 3261 §12.2.2: a request matching a dialog is first held to the rules
//! for requests outside a dialog (§8.2), and a request §8.2 rejects performs
//! no state change. A method this stack carries in no dialog is refused by its
//! method: 405 with the node's `Allow` (§8.2.1), or 489 to a SUBSCRIBE, as
//! this stack serves no event package (RFC 6665 §4.2.1.2). So such a request
//! draws its refusal even when a later request of its sender overtook it, and
//! its refusal leaves the dialog's remote CSeq where it was: a request
//! numbered below it but above the last one taken is still in order.
//!
//! The fabric holds the caller's first request of one method in flight, so
//! her next request, sent after it, reaches the stack first.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut, ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, Dialog, Harness, Inbound, SIMULATED_TRANSIT_DELAY_MS};
use sip_message::generators::{
    generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts, InDialogMethod,
    OutOfDialogMethod,
};
use sip_message::header::{self, HeaderName, Uri, Via};
use sip_message::{SipHeader, SipMessage, SipResponse, SipStr};

/// How much later than the transit the held request arrives: past the next
/// request's arrival, short of a retransmission (RFC 3261 §17.1.2.2, T1).
const HELD: Duration = Duration::from_millis(150);

/// Past a request's two hops through the stack to the peer and short of its
/// first retransmission.
const RELAY_WINDOW: Duration = Duration::from_millis(300);

/// The fabric's hold on the first datagram whose start line is a request of
/// `method`, [`HELD`]; nothing else is held.
fn the_first_held(method: &'static str) -> sip_net_testkit::SendDelay {
    let held = AtomicBool::new(false);
    let start = format!("{method} ");
    Arc::new(move |_src, _dst, datagram| {
        let first = datagram.starts_with(start.as_bytes()) && !held.swap(true, Ordering::Relaxed);
        first.then_some(HELD)
    })
}

/// A confirmed call alice ↔ b2bua ↔ bob, alice's dialog and its Call-ID.
struct Confirmed {
    h: Harness,
    alice: Agent,
    bob: Agent,
    b2bua: B2buaSut,
    dialog: Dialog,
    call_id: String,
}

/// A call confirmed over a fabric that holds the first request of `held`.
async fn confirmed(name: &str, held: &'static str) -> Confirmed {
    let h = Harness::with_transit_delay_and_send_delay(
        name,
        SIMULATED_TRANSIT_DELAY_MS,
        the_first_held(held),
    );
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", bob.addr().port()).start(&h, "b2bua", &at).await;
    let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).through(b2bua.addr).send().await;
    let call_id = call.call_id();
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    Confirmed { h, alice, bob, b2bua, dialog, call_id }
}

fn uri(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).expect("readable URI")
}

fn line(name: &'static str, value: &'static str) -> SipHeader {
    SipHeader { name: SipStr::from_static(name), value: SipStr::from_static(value) }
}

/// alice sends `method`, a method this stack carries in no dialog, as a
/// request of her dialog (RFC 3261 §12.2.1.1) numbered next in it.
async fn send_unrelayable(c: &mut Confirmed, method: OutOfDialogMethod) {
    let cseq = c.dialog.local_cseq() + 1;
    let request = generate_out_of_dialog_request(
        method,
        &GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri(&format!("sip:{}", c.b2bua.addr))),
            call_id: c.call_id.clone(),
            from: Some(
                header::From::from_uri(uri(&format!("sip:alice@{}", c.alice.addr().ip())))
                    .with_tag(SipStr::owned(c.dialog.local_tag())),
            ),
            to: Some(
                header::To::from_uri(uri(&format!("sip:bob@{}", c.bob.addr().ip())))
                    .with_tag(SipStr::owned(c.dialog.remote_tag())),
            ),
            cseq,
            via: Some(
                Via::udp(c.alice.addr().ip().to_string(), c.alice.addr().port())
                    .with_branch(SipStr::from_static("z9hG4bK-overtaken-1")),
            ),
            contact: Some(header::Contact::from_uri(uri(&format!("sip:alice@{}", c.alice.addr())))),
            max_forwards: Some(70),
            body: Vec::new(),
            content_type: None,
            extra_headers: vec![line("Event", "dialog"), line("Expires", "600")],
        },
    )
    .expect("a request of the method");
    c.dialog.set_local_cseq(cseq);
    let wire = sip_message::serializer::serialize(&SipMessage::Request(request));
    c.alice.try_send_datagram(&wire, c.b2bua.addr).await.expect("the request leaves");
}

/// The final alice reads next, which answers no transaction of her dialog's.
async fn final_read(alice: &Agent) -> SipResponse {
    match alice.recv_any().await.expect("the request draws its final") {
        Inbound::Response(r) => *r,
        Inbound::Request(r) => panic!("alice expected a final, got {}", r.request().method()),
    }
}

/// alice hangs up; the call ends with its one CDR, fully reaped.
async fn hang_up(mut c: Confirmed) {
    let mut bye = c.dialog.bye().await;
    c.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| c.b2bua.cdr_records().len() == 1).await;
    assert_eq!(c.b2bua.cdr_records().len(), 1, "one call, one CDR");
    settle_until(|| c.b2bua.is_reaped()).await;
    c.alice.drain().await;
    c.bob.drain().await;
    c.b2bua.assert_fully_reaped();
    let _report = c.h.finish().await;
}

/// alice's PUBLISH is overtaken by her next request, an INFO, which reaches
/// bob and is answered: her dialog's last CSeq is now above the PUBLISH's.
/// The PUBLISH still draws 405 with the node's `Allow`, not the out-of-order
/// 500, and bob is sent nothing of it.
#[tokio::test(start_paused = true)]
async fn an_overtaken_publish_draws_405_with_allow() {
    let mut c = confirmed("overtaken-publish-405", "PUBLISH").await;
    send_unrelayable(&mut c, OutOfDialogMethod::Publish).await;
    let mut info = c.dialog.send_request(InDialogMethod::Info).send().await;
    c.bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    let answer = final_read(&c.alice).await;
    assert_eq!(answer.status(), 405, "the overtaken PUBLISH is refused by its method");
    let allow = answer.raw(HeaderName::Allow).next().map(|v| v.to_string());
    assert!(allow.is_some_and(|a| a.contains("INVITE")), "the 405 states the node's Allow");
    c.h.advance(RELAY_WINDOW).await;
    let relayed = c.bob.take_queued().await;
    assert!(relayed.is_none(), "nothing of the PUBLISH reaches bob, got {relayed:?}");
    hang_up(c).await;
}

/// alice's INFO is overtaken by her next request, a SUBSCRIBE, which draws
/// 489. The refusal left her dialog's remote CSeq as it was, so the INFO
/// arriving after it is in order: it reaches bob and his 200 reaches her.
#[tokio::test(start_paused = true)]
async fn a_subscribe_refused_489_leaves_the_order_to_the_request_it_overtook() {
    let mut c = confirmed("overtaken-by-a-489", "INFO").await;
    let mut info = c.dialog.send_request(InDialogMethod::Info).send().await;
    send_unrelayable(&mut c, OutOfDialogMethod::Subscribe).await;
    let answer = final_read(&c.alice).await;
    assert_eq!(answer.status(), 489, "an event package this stack serves no subscription of");
    match c.bob.try_receive("INFO").await {
        Ok(mut relayed) => relayed.respond(200, "OK").await,
        Err(e) => panic!("the overtaken INFO, taken in order, reaches bob: {e:?}"),
    }
    info.expect(200).await;
    hang_up(c).await;
}
