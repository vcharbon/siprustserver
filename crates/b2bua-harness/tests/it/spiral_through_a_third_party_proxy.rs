//! A spiral (RFC 3261 §16.3): the B2BUA's outgoing INVITE reaches a
//! third-party stateful proxy, which forwards it back to the same B2BUA
//! instance with a new Request-URI. The proxy keeps From, To, Call-ID and
//! CSeq, puts its own Via above the B2BUA's and decrements Max-Forwards, so
//! the returning INVITE carries the first call's outgoing leg identity as its
//! own incoming identity. The B2BUA takes it as a second, independent call:
//! no 482, no loop refusal, each call routed, ended and recorded on its own.
//!
//!   alice ──▶ b2bua (call 1) ──▶ proxy ──▶ b2bua (call 2) ──▶ bob
//!
//! Answered (either end hangs up), rejected and caller-cancelled spirals end
//! cleanly: the proxy's CANCEL reaches call 2 by its incoming leg, never call 1
//! by the outgoing leg that carries the same Call-ID and From-tag.
//!
//! A proxy that does not Record-Route (§16.6 step 4) leaves the dialog
//! between the two calls Contact to Contact: an in-dialog request goes from
//! the B2BUA to its own address, under the branch of its own client
//! transaction, and is a new server transaction there (§17.2.3).
//!
//! The proxy is `common::stateful_proxy`, a harness actor rather than the
//! load-balancing front proxy: it holds no B2BUA cookie and routes by Route
//! and Request-URI only, which is what a proxy outside the deployment does.

use b2bua_harness::settle_until;
use call::CdrEventType;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use sip_message::header::MaxForwards;
use sip_message::SipRequest;

use crate::common::spiral::{assert_two_calls, kinds, spiral_scene};
use crate::common::stateful_proxy::RecordRoute;

/// What alice's INVITE states (RFC 3261 §8.1.1.6).
const ALICE_HOPS: u32 = 70;

fn hops(req: &SipRequest) -> u32 {
    req.header::<MaxForwards>().expect("Max-Forwards stated").expect("readable").value()
}

/// bob's INVITE is call 2's outgoing leg: three hops spent (call 1, the
/// proxy, call 2), and an identity of its own.
fn assert_reached_bob_through_the_spiral(invite: &SipRequest) {
    assert_eq!(hops(invite), ALICE_HOPS - 3, "Max-Forwards only decrements along the spiral");
}

/// (a) answered; alice hangs up and her BYE crosses both calls.
#[tokio::test(start_paused = true)]
async fn a_spiraled_call_answered_then_ended_by_the_caller() {
    answered_then_ended_by_the_caller("spiral-answered-caller-bye", RecordRoute::Yes).await;
}

/// (a) through a proxy that does not Record-Route: the BYE goes from call 1
/// to call 2 directly.
#[tokio::test(start_paused = true)]
async fn a_spiraled_call_without_record_route_answered_then_ended_by_the_caller() {
    answered_then_ended_by_the_caller("spiral-no-rr-caller-bye", RecordRoute::No).await;
}

async fn answered_then_ended_by_the_caller(name: &str, record_route: RecordRoute) {
    let (s, _proxy) = spiral_scene(name, record_route).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    assert_reached_bob_through_the_spiral(uas.request());
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    let (first, second) = assert_two_calls(&s, &call.call_id());
    for cdr in [&first, &second] {
        let k = kinds(cdr);
        assert!(k.contains(&CdrEventType::Answer) && k.contains(&CdrEventType::Bye), "{k:?}");
    }
    let _ = s.finish().await;
}

/// (a) answered; bob hangs up and his BYE crosses both calls the other way.
#[tokio::test(start_paused = true)]
async fn a_spiraled_call_answered_then_ended_by_the_callee() {
    answered_then_ended_by_the_callee("spiral-answered-callee-bye", RecordRoute::Yes).await;
}

/// (a) through a proxy that does not Record-Route: the BYE goes from call 2
/// to call 1 directly.
#[tokio::test(start_paused = true)]
async fn a_spiraled_call_without_record_route_answered_then_ended_by_the_callee() {
    answered_then_ended_by_the_callee("spiral-no-rr-callee-bye", RecordRoute::No).await;
}

async fn answered_then_ended_by_the_callee(name: &str, record_route: RecordRoute) {
    let (s, _proxy) = spiral_scene(name, record_route).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    assert_reached_bob_through_the_spiral(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut bob_dialog = uas.dialog();
    let mut bye = bob_dialog.bye().await;
    s.alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    let (first, second) = assert_two_calls(&s, &call.call_id());
    for cdr in [&first, &second] {
        let k = kinds(cdr);
        assert!(k.contains(&CdrEventType::Answer) && k.contains(&CdrEventType::Bye), "{k:?}");
    }
    let _ = s.finish().await;
}

/// (b) bob rejects; the 486 is relayed back through the proxy to alice, each
/// hop ACKing the one below it.
#[tokio::test(start_paused = true)]
async fn a_spiraled_call_rejected_by_the_callee() {
    let (s, _proxy) = spiral_scene("spiral-rejected", RecordRoute::Yes).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    assert_reached_bob_through_the_spiral(uas.request());
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(486, "Busy Here").await;
    uas.expect_ack().await;
    call.expect(486).await;

    settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    let (first, second) = assert_two_calls(&s, &call.call_id());
    for cdr in [&first, &second] {
        assert!(!kinds(cdr).contains(&CdrEventType::Answer), "neither call is answered");
    }
    let _ = s.finish().await;
}

/// (c) alice CANCELs while bob rings: call 1 CANCELs its outgoing leg, the
/// proxy answers that CANCEL and CANCELs its own forwarded INVITE, and call 2
/// CANCELs bob. Every INVITE ends 487.
#[tokio::test(start_paused = true)]
async fn a_spiraled_call_cancelled_by_the_caller_while_ringing() {
    let (s, _proxy) = spiral_scene("spiral-cancelled", RecordRoute::Yes).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    assert_reached_bob_through_the_spiral(uas.request());
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    let mut b_cxl =
        s.bob.try_receive("CANCEL").await.expect("the CANCEL crosses the spiral to bob");
    b_cxl.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    uas.expect_ack().await;

    settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    let (first, second) = assert_two_calls(&s, &call.call_id());
    for cdr in [&first, &second] {
        assert!(!kinds(cdr).contains(&CdrEventType::Answer), "neither call is answered");
    }
    let _ = s.finish().await;
}
