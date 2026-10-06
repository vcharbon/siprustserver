//! Events of a call that arrive after the caller re-offered its INVITE on
//! that call's Call-ID and From tag. Both calls share one callRef, so an event
//! of the first call addressed by callRef alone could act on the second. It
//! must not: the second call's ring deadline, its outgoing leg and its CDR are
//! its own. Each event of the first call names the first call's incarnation —
//! a timer fire or a callout result on the event, a callee's message on the
//! Via or Contact the first call stamped — and the second call never reads
//! one. The re-offer reuses the first INVITE's CSeq on a new branch, as a
//! caller retrying after its own CANCEL does.

use std::sync::Arc;
use std::time::Duration;

use b2bua::admission::Class;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua::CallEvent;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::{CdrEventType, TimerType};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{ClientInvite, ServerTxn, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const CALL_ID: &str = "reoffer-late@127.0.0.1";
const FROM_TAG: &str = "reoffer-late-from-tag";
/// The first call's ring deadline, from its routing.
const NO_ANSWER: Duration = Duration::from_secs(30);
/// When the caller gives up on the first call.
const CANCEL_AT: Duration = Duration::from_secs(10);
/// When the caller re-offers, after the first call's release.
const REOFFER_AT: Duration = Duration::from_secs(12);
/// The UDP Timer D (RFC 3261 §17.1.1.2): the first call's client transaction
/// absorbs its callee's repeated final this long after the first one.
const TIMER_D: Duration = Duration::from_secs(32);

/// Every call is routed to bob; `no_answer` arms the ring deadline. alice's
/// same-CSeq re-offer after the first INVITE's 487 is the caller behaviour
/// under test, and the audit reads it as a CSeq that did not advance.
async fn scene(name: &str, no_answer: Option<Duration>) -> B2buaScene {
    let s = B2buaScene::with_b2bua(name, move |bob_port| {
        B2buaSut::builder(Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_| {
                    let mut r = route_to("127.0.0.1", bob_port);
                    r.no_answer_timeout_sec = no_answer.map(|d| d.as_secs() as i64);
                    NewCallResponse::Route(r)
                })
                .build(),
        ))
    })
    .await;
    s.h.waive(
        WaiverScope::rule(
            "cseq-in-dialog-order",
            "alice re-offers her cancelled INVITE on its Call-ID, From tag and CSeq, the \
             request under test",
        )
        .on_party("alice"),
    );
    s
}

/// alice's INVITE under the fixed identity at CSeq 1, on a fresh branch.
async fn offer(s: &B2buaScene) -> ClientInvite {
    s.alice
        .invite(&s.bob)
        .identity(CALL_ID, FROM_TAG)
        .cseq(1)
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await
}

/// Advance the paused clock to `at` after the scene started.
async fn advance_to(s: &B2buaScene, start: tokio::time::Instant, at: Duration) {
    let now = start.elapsed();
    assert!(now <= at, "the scenario is already past {at:?}: {now:?}");
    s.h.advance(at - now).await;
}

/// The callRef both calls are born on.
fn call_ref() -> String {
    call::derive_call_ref("w0", CALL_ID, FROM_TAG)
}

/// What the first call left behind: the callee's 487 as sent, the first
/// call's incarnation, and the caller's side of its INVITE.
struct FirstCall {
    rejected: Vec<u8>,
    incarnation: String,
    invite: ClientInvite,
}

/// The first call rings; the caller CANCELs it at [`CANCEL_AT`]; the callee
/// answers 487 and the call is released.
async fn first_call_cancelled(s: &B2buaScene, start: tokio::time::Instant) -> FirstCall {
    let mut first = offer(s).await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    first.expect(180).await;
    let incarnation =
        s.b2bua.live_call(&call_ref()).expect("the first call is live").incarnation().to_string();
    advance_to(s, start, CANCEL_AT).await;
    let mut cxl = first.cancel().await;
    cxl.expect(200).await;
    first.expect(487).await;
    s.bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    uas.expect_ack().await;
    settle_until(|| s.b2bua.cdr_records().len() == 1 && s.b2bua.is_reaped()).await;
    assert_eq!(s.b2bua.active_calls(), 0, "the first call is released");
    FirstCall { rejected: callee_487(s), incarnation, invite: first }
}

/// The callee's 487 as it went on the wire.
fn callee_487(s: &B2buaScene) -> Vec<u8> {
    s.h.wire_entries()
        .into_iter()
        .find(|e| {
            e.from == s.bob.addr()
                && matches!(CustomParser::new().parse(&e.raw),
                    Ok(SipMessage::Response(r)) if r.status() == 487)
        })
        .expect("bob's 487 is on the wire")
        .raw
        .to_vec()
}

/// The re-offer at [`REOFFER_AT`] rings the callee.
async fn reoffer_rings(s: &B2buaScene, start: tokio::time::Instant) -> (ClientInvite, ServerTxn) {
    advance_to(s, start, REOFFER_AT).await;
    let mut reoffer = offer(s).await;
    let mut uas = s.bob.try_receive("INVITE").await.expect("the re-offer is routed to the callee");
    uas.respond(180, "Ringing").await;
    reoffer.expect(180).await;
    (reoffer, uas)
}

/// Neither end has heard anything of the second call since it rang, and it
/// is still up. A hop ACK repeated to the first call's 487 is the callee's
/// transaction layer's, not news of the second call.
async fn second_call_still_ringing(s: &B2buaScene, what: &str) {
    s.bob.sight_queued().await;
    s.alice.sight_queued().await;
    assert!(s.bob.take_queued_inbound().await.is_none(), "{what}: nothing reaches the callee");
    assert!(s.alice.take_queued_inbound().await.is_none(), "{what}: nothing reaches the caller");
    assert_eq!(s.b2bua.active_calls(), 1, "{what}: the second call is up");
}

/// How many ACKs bob has sighted.
fn acks_seen(s: &B2buaScene) -> usize {
    s.bob.wire_view().iter().filter(|e| e.start_line().starts_with("ACK ")).count()
}

/// The callee answers the second call; the caller hangs up.
async fn answer_and_hang_up(s: &B2buaScene, mut reoffer: ClientInvite, mut uas: ServerTxn) {
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    reoffer.expect(200).await;
    let mut dialog = reoffer.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    assert_eq!(s.b2bua.new_calls().accepted(Class::Normal), 2, "two calls");
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 2, "one CDR per call: {cdrs:?}");
    assert!(
        cdrs[1].events.iter().any(|e| e.event_type == CdrEventType::Answer),
        "the second call is answered: {:?}",
        cdrs[1].events
    );
}

/// The first call's ring deadline falls while the second call rings: it was
/// the first call's, and the second call rings on to its own. Covers a timer
/// cancelled at the first call's release only, not a fire already queued
/// before that cancel.
#[tokio::test(start_paused = true)]
async fn the_first_calls_ring_deadline_does_not_end_the_reoffer() {
    let s = scene("reoffer-late-no-answer", Some(NO_ANSWER)).await;
    let start = tokio::time::Instant::now();
    let first = first_call_cancelled(&s, start).await;
    let (reoffer, uas) = reoffer_rings(&s, start).await;
    assert_ne!(
        s.b2bua.live_call(&call_ref()).expect("the second call is live").incarnation(),
        first.incarnation,
        "the second call is another incarnation"
    );

    advance_to(&s, start, NO_ANSWER + Duration::from_secs(1)).await;
    second_call_still_ringing(&s, "past the first call's ring deadline").await;
    answer_and_hang_up(&s, reoffer, uas).await;
    let _ = s.finish().await;
}

/// The callee repeats its 487 to the first call while the second call rings,
/// inside the first call's Timer D: the transaction layer answers it with the
/// ACK again and the second call never sees it.
#[tokio::test(start_paused = true)]
async fn a_repeated_487_of_the_first_call_inside_timer_d_is_absorbed() {
    let s = scene("reoffer-late-487-inside-timer-d", None).await;
    let start = tokio::time::Instant::now();
    let rejected = first_call_cancelled(&s, start).await.rejected;
    let (reoffer, uas) = reoffer_rings(&s, start).await;

    s.bob.sight_queued().await;
    let acks = acks_seen(&s);
    s.bob.try_send_datagram(&rejected, s.b2bua.addr).await.expect("the fabric takes it");
    s.h.advance(Duration::from_secs(1)).await;
    s.bob.sight_queued().await;
    assert_eq!(acks_seen(&s), acks + 1, "the repeated 487 draws the ACK again");
    second_call_still_ringing(&s, "after the repeated 487").await;
    answer_and_hang_up(&s, reoffer, uas).await;
    let _ = s.finish().await;
}

/// The callee's 487 to the first call reaches the node after the first
/// call's Timer D, while the second call rings: it is a stray of an ended
/// transaction and must not end the second call's outgoing leg.
#[tokio::test(start_paused = true)]
async fn a_straggling_487_of_the_first_call_after_timer_d_does_not_end_the_reoffer() {
    let s = scene("reoffer-late-487-after-timer-d", None).await;
    let start = tokio::time::Instant::now();
    let rejected = first_call_cancelled(&s, start).await.rejected;
    let (reoffer, uas) = reoffer_rings(&s, start).await;

    advance_to(&s, start, CANCEL_AT + TIMER_D + Duration::from_secs(1)).await;
    second_call_still_ringing(&s, "before the straggler").await;
    s.bob.try_send_datagram(&rejected, s.b2bua.addr).await.expect("the fabric takes it");
    s.h.advance(Duration::from_secs(1)).await;
    second_call_still_ringing(&s, "after the straggler").await;
    assert_eq!(s.b2bua.metrics().other_incarnation_dropped_total(), 1, "the straggler is dropped");
    answer_and_hang_up(&s, reoffer, uas).await;
    let _ = s.finish().await;
}

/// The first call's ring deadline fired as the first call was released, its
/// fire already queued past the release's cancel: it reaches the router while
/// the second call rings, and the second call rings on to its own deadline.
#[tokio::test(start_paused = true)]
async fn a_ring_deadline_fire_of_the_first_call_queued_past_its_release_does_not_end_the_reoffer() {
    let s = scene("reoffer-late-queued-no-answer", Some(NO_ANSWER)).await;
    let start = tokio::time::Instant::now();
    let first = first_call_cancelled(&s, start).await;
    let (reoffer, uas) = reoffer_rings(&s, start).await;

    s.b2bua.post_event(CallEvent::Timer {
        timer_type: TimerType::NoAnswer,
        call_ref: call_ref(),
        leg_id: Some("b-1".into()),
        incarnation: Some(first.incarnation),
    });
    s.h.advance(Duration::from_secs(1)).await;
    second_call_still_ringing(&s, "after the first call's queued fire").await;
    assert_eq!(s.b2bua.metrics().other_incarnation_dropped_total(), 1, "the fire is dropped");
    answer_and_hang_up(&s, reoffer, uas).await;
    let _ = s.finish().await;
}

/// A `/call/failure` answer the first call sent for and never read reaches
/// the router while the second call rings: a reject of the first call's leg
/// is no decision of the second call's, which rings on.
#[tokio::test(start_paused = true)]
async fn a_callout_result_of_the_first_call_does_not_end_the_reoffer() {
    let s = scene("reoffer-late-callout-result", None).await;
    let start = tokio::time::Instant::now();
    let first = first_call_cancelled(&s, start).await;
    let (reoffer, uas) = reoffer_rings(&s, start).await;

    s.b2bua.post_event(CallEvent::InternalEvent {
        call_ref: call_ref(),
        topic: "call-failure-result".into(),
        outcome: "reject".into(),
        payload: serde_json::json!({ "code": 486, "failed_leg_id": "b-1" }),
        body: Vec::new(),
        incarnation: Some(first.incarnation),
    });
    s.h.advance(Duration::from_secs(1)).await;
    second_call_still_ringing(&s, "after the first call's callout result").await;
    assert_eq!(s.b2bua.metrics().other_incarnation_dropped_total(), 1, "the result is dropped");
    answer_and_hang_up(&s, reoffer, uas).await;
    let _ = s.finish().await;
}

/// The caller's BYE of the first call's early dialog (RFC 3261 §15) reaches
/// the node only while the second call rings: it names the first call's
/// incarnation on the Request-URI the first call's Contact stated, draws the
/// 481 a request of no dialog here is owed (§12.2.2), and the second call
/// rings on.
#[tokio::test(start_paused = true)]
async fn a_late_bye_of_the_first_call_is_answered_481_and_does_not_end_the_reoffer() {
    let s = scene("reoffer-late-bye", None).await;
    let start = tokio::time::Instant::now();
    let mut first = first_call_cancelled(&s, start).await;
    let (reoffer, uas) = reoffer_rings(&s, start).await;

    let mut bye = first.invite.send_request(InDialogMethod::Bye).send().await;
    bye.expect(481).await;
    s.h.advance(Duration::from_secs(1)).await;
    second_call_still_ringing(&s, "after the first call's late BYE").await;
    assert_eq!(s.b2bua.metrics().other_incarnation_dropped_total(), 1, "the BYE is dropped");
    answer_and_hang_up(&s, reoffer, uas).await;
    let _ = s.finish().await;
}
