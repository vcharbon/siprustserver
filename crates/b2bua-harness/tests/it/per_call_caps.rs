//! A turn past a per-call cap fails as a panicked turn does (ADR-0057 §4):
//! its result is discarded, and the call ends from the record it had before
//! the event, torn down on the wire. During setup the caller is answered the
//! stack's bare 503 and a ringing callee is CANCELled; an answered call's legs
//! are BYEd and the in-dialog request in hand is answered 500. Every call
//! writes one CDR naming the cap, releases what it held on the limiter, and
//! nothing on the wire names the cap.

use std::sync::Arc;

use b2bua::config::{AgeCap, CallCaps, Cap};
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::LimiterEntry;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;
use sip_message::SipResponse;

/// Routes to `bob_port` holding `trunk-a`, with a callback context; a
/// failure fails over to carol (5071), holding `trunk-b`.
fn holding_routes(bob_port: u16) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", bob_port);
                r.callback_context = Some("ctx".into());
                r.call_limiter = vec![LimiterEntry { id: "trunk-a".into(), limit: 10 }];
                NewCallResponse::Route(r)
            })
            .on_failure(|_| {
                let mut r = route_to("127.0.0.1", 5071);
                r.call_limiter = vec![LimiterEntry { id: "trunk-b".into(), limit: 10 }];
                CallTreatment::Route(r)
            })
            .build(),
    )
}

/// The scene under `caps`.
async fn scene(name: &str, caps: fn(&mut CallCaps)) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| {
        B2buaSut::builder(holding_routes(bob_port)).tune(move |c| caps(&mut c.caps))
    })
    .await
}

/// The stack's own 503: nothing says why.
fn assert_bare_503(resp: &SipResponse) {
    assert_eq!(resp.status(), 503);
    assert!(!resp.has(&HeaderName::Reason), "no Reason: {resp:?}");
    assert!(!resp.has(&HeaderName::Warning), "no Warning: {resp:?}");
}

/// The call is reaped with one CDR naming `cap`, `cap` tripped `trips`
/// times, and no discharge forced the call terminal.
async fn assert_ended_by(s: &B2buaScene, cap: Cap, trips: u64) {
    settle_until(|| s.b2bua.is_reaped()).await;
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR: {cdrs:?}");
    let reasons: Vec<_> = cdrs[0].events.iter().filter_map(|e| e.reason.as_deref()).collect();
    assert!(reasons.contains(&cap.cdr_reason()), "the CDR names the cap: {reasons:?}");
    let m = s.b2bua.metrics();
    assert_eq!(m.cap_trips_of(cap), trips, "{cap:?} tripped");
    assert_eq!(m.reaper_discharged_total(), 0, "the verdict's teardown ended the call");
}

/// A route that would dial past the legs' cap trips the initial turn: bob is
/// never dialled, alice is answered the bare 503, and the admit the route
/// sent is released.
#[tokio::test(start_paused = true)]
async fn a_setup_trip_before_any_callee_answers_the_caller_a_bare_503() {
    let s = scene("cap-setup-trip", |c| c.legs = AgeCap { base: 0, per_hour: 0 }).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let (resp, _) = call.try_expect_final(503).await.expect("alice is answered 503");
    assert_bare_503(&resp);
    assert!(s.bob.try_receive_tolerating("INVITE", &[]).await.is_none(), "bob is never dialled");
    assert_ended_by(&s, Cap::Legs, 1).await;
    let _ = s.finish().await;
}

/// bob forks past the dialogs' cap while ringing: the turn of his second
/// fork is discarded, bob is CANCELled and alice is answered the bare 503.
#[tokio::test(start_paused = true)]
async fn a_setup_trip_with_the_callee_ringing_cancels_him() {
    let s = scene("cap-setup-trip-ringing", |c| c.dialogs = 1).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    call.expect(180).await;
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;

    let mut cancel = s.bob.try_receive("CANCEL").await.expect("bob is CANCELled");
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").with_to_tag("bobfork1").await;
    let (resp, _) = call.try_expect_final(503).await.expect("alice is answered 503");
    assert_bare_503(&resp);
    assert_ended_by(&s, Cap::Dialogs, 1).await;
    let _ = s.finish().await;
}

/// bob refuses, and the failure consult's route would dial carol past the
/// legs' cap: its fold's turn is discarded, carol is never dialled, alice is
/// answered the bare 503, and both routes' holds are released.
#[tokio::test(start_paused = true)]
async fn an_off_turn_route_fold_past_the_legs_cap_releases_every_hold() {
    let s = scene("cap-failover-fold-trip", |c| c.legs = AgeCap { base: 1, per_hour: 0 }).await;
    let carol = s.h.agent("carol", "127.0.0.1:5071").await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.bob.receive("INVITE").await.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;
    let (resp, _) = call.try_expect_final(503).await.expect("alice is answered 503");
    assert_bare_503(&resp);
    assert!(carol.try_receive_tolerating("INVITE", &[]).await.is_none(), "carol is never dialled");
    assert_ended_by(&s, Cap::Legs, 1).await;
    let _ = s.finish().await;
}

/// alice's INFO on her answered call would hold a request record past the
/// requests' cap: it is answered 500, both legs are BYEd.
#[tokio::test(start_paused = true)]
async fn a_trip_on_an_answered_call_byes_both_legs_and_answers_the_request_500() {
    let s = scene("cap-answered-trip", |c| c.requests = 0).await;
    let mut dialog = s.establish().await;
    let mut info = dialog.request(InDialogMethod::Info, None).await;
    info.expect(500).await;
    s.alice.receive("BYE").await.respond(200, "OK").await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    assert_ended_by(&s, Cap::Requests, 1).await;
    let _ = s.finish().await;
}

/// alice's first INFO is relayed and left pending at bob, holding the call at
/// the requests' cap; her second crosses it and is answered 500. The verdict's
/// teardown, run with the collection caps off, BYEs both legs: no second
/// strike, no discharge. bob's answer to the first INFO still reaches alice.
#[tokio::test(start_paused = true)]
async fn an_answered_call_at_the_requests_cap_with_a_relay_pending_is_byed() {
    let s = scene("cap-answered-relay-pending", |c| c.requests = 1).await;
    let mut dialog = s.establish().await;
    let mut first = dialog.request(InDialogMethod::Info, None).await;
    let mut pending = s.bob.receive("INFO").await;
    let mut second = dialog.request(InDialogMethod::Info, None).await;
    second.expect(500).await;

    let mut bob_bye = s.bob.receive("BYE").await;
    pending.respond(200, "OK").await;
    bob_bye.respond(200, "OK").await;
    let mut alice_bye = s.alice.receive("BYE").await;
    first.expect(200).await;
    alice_bye.respond(200, "OK").await;
    assert_ended_by(&s, Cap::Requests, 1).await;
    let _ = s.finish().await;
}

/// Any answered call over the default caps lands every turn: the caps are a
/// failsafe that shapes no valid call.
#[tokio::test(start_paused = true)]
async fn the_default_caps_trip_no_valid_call() {
    let s = scene("cap-defaults", |_| {}).await;
    let mut dialog = s.establish().await;
    let mut info = dialog.request(InDialogMethod::Info, None).await;
    let mut at_bob = s.bob.receive("INFO").await;
    at_bob.respond(200, "OK").await;
    info.expect(200).await;
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    assert!(Cap::ALL.iter().all(|c| s.b2bua.metrics().cap_trips_of(*c) == 0));
    let _ = s.finish().await;
}

/// alice sends two INFOs at once on her answered call at the requests' cap
/// of 0: both turns trip before the first trip's verdict runs. The scenario
/// depends on that order: both INFOs reach the call's FIFO in one instant,
/// ahead of the verdict the first turn queues, and the two trips counted
/// below prove it held. The second trip strikes nothing, so the verdict's teardown
/// BYEs both legs: no discharge.
#[tokio::test(start_paused = true)]
async fn a_second_trip_behind_the_queued_verdict_strikes_nothing() {
    let s = scene("cap-two-trips", |c| c.requests = 0).await;
    let mut dialog = s.establish().await;
    let mut first = dialog.request(InDialogMethod::Info, None).await;
    let mut second = dialog.request(InDialogMethod::Info, None).await;
    first.expect(500).await;
    second.expect(500).await;
    s.alice.receive("BYE").await.respond(200, "OK").await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    assert_ended_by(&s, Cap::Requests, 2).await;
    let _ = s.finish().await;
}

/// With the reaper's sweep off, a trip still ends the call: the failure hook
/// sends its verdict either way.
#[tokio::test(start_paused = true)]
async fn a_trip_ends_the_call_with_the_sweep_off() {
    let s = B2buaScene::with_b2bua("cap-trip-sweep-off", |bob_port| {
        B2buaSut::builder(holding_routes(bob_port)).tune(|c| {
            c.reaper_enabled = false;
            c.caps.requests = 0;
        })
    })
    .await;
    let mut dialog = s.establish().await;
    let mut info = dialog.request(InDialogMethod::Info, None).await;
    info.expect(500).await;
    s.alice.receive("BYE").await.respond(200, "OK").await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    assert_ended_by(&s, Cap::Requests, 1).await;
    let _ = s.finish().await;
}

/// bob answers 200 straight away and his answer's dialogs cross the cap of 0:
/// the turn is discarded with bob's leg still unanswered on the record, and
/// the teardown's CANCEL finds his INVITE transaction already answered. bob
/// repeats his 200, unACKed: it draws the ACK and the BYE an answer on an
/// ending call is owed (RFC 3261 §13.2.2.4, §15), and once his leg ends alice
/// is answered the bare 503.
#[tokio::test(start_paused = true)]
async fn a_trip_on_the_callee_s_answer_ends_his_dialog() {
    let s = scene("cap-trip-on-2xx", |c| c.dialogs = 0).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    s.h.advance(std::time::Duration::from_millis(500)).await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    s.bob.try_receive("ACK").await.expect("bob's 200 is ACKed");
    s.bob.try_receive("BYE").await.expect("bob's dialog is ended").respond(200, "OK").await;
    let (resp, _) = call.try_expect_final(503).await.expect("alice is answered 503");
    assert_bare_503(&resp);
    assert_ended_by(&s, Cap::Dialogs, 1).await;
    let _ = s.finish().await;
}

/// The trip's teardown carries the call past its CDR events' cap, and bob
/// never answers his BYE: the turns of the terminating call run with the
/// collection caps off, so its `TerminatingTimeout` ends it.
#[tokio::test(start_paused = true)]
async fn a_terminating_call_past_a_cap_still_ends_at_its_terminating_timeout() {
    let s = scene("cap-terminating", |c| {
        c.requests = 0;
        c.cdr_events = AgeCap { base: CDR_EVENTS_ANSWERED, per_hour: 0 };
    })
    .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);
    let mut call = s
        .alice
        .invite(&s.bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let live = s.b2bua.live_call(&call_ref).expect("the call is live");
    assert_eq!(live.cdr_events.len() as u64, CDR_EVENTS_ANSWERED, "the call sits at its cap");

    let mut info = dialog.request(InDialogMethod::Info, None).await;
    info.expect(500).await;
    s.alice.receive("BYE").await.respond(200, "OK").await;
    let _unanswered = s.bob.receive("BYE").await;
    s.h.advance(std::time::Duration::from_millis(call::helpers::TERMINATING_TIMEOUT_MS as u64))
        .await;
    assert_ended_by(&s, Cap::Requests, 1).await;
    let _ = s.finish().await;
}

const CALL_ID: &str = "cap-terminating@127.0.0.1";
const FROM_TAG: &str = "cap-terminating-tag";

/// The CDR events an answered call of this scene holds: the cap that holds it
/// exactly there.
const CDR_EVENTS_ANSWERED: u64 = 3;

/// A turn that grows a collection across half its cap is counted, and lands.
#[tokio::test(start_paused = true)]
async fn a_turn_crossing_half_a_cap_is_counted() {
    let s = scene("cap-near", |c| c.requests = 2).await;
    let mut dialog = s.establish().await;
    let mut first = dialog.request(InDialogMethod::Info, None).await;
    let mut at_bob = s.bob.receive("INFO").await;
    let mut second = dialog.request(InDialogMethod::Info, None).await;
    let mut second_at_bob = s.bob.receive("INFO").await;
    at_bob.respond(200, "OK").await;
    second_at_bob.respond(200, "OK").await;
    first.expect(200).await;
    second.expect(200).await;
    assert_eq!(s.b2bua.metrics().cap_near_of(Cap::Requests), 1, "0 → 1 → 2 crosses 1 once");
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    let _ = s.finish().await;
}
