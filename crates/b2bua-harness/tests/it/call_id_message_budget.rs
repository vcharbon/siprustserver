//! The message budget of a Call-ID (ADR-0063 §3, §4): the transaction layer
//! counts every datagram it sends under a Call-ID and every one it receives
//! that matches a transaction or draws an answer. Past the budget the Call-ID
//! is muted, nothing is sent under it and what arrives under it is dropped,
//! and every call holding it ends as the message cap's call does, its CDR
//! naming the budget. The mute lasts until those calls are released and
//! 64·T1 after the last dropped message.
//!
//! Each scenario sets the budget far below the default and drives a peer that
//! answers the node's answer with a new message, a loop no per-path bound
//! stops: requests on a removed call, requests on a call the per-interval cap
//! is tearing down, INVITE copies during setup, and a relay loop across a
//! spiral, whose middle Call-ID two calls hold.

use std::time::Duration;

use b2bua::cdr::CdrRecord;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::TerminationCause;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, Inbound};
use sip_message::{SipMessage, SipResponse};
use sip_txn::timers::TIMER_B;

use crate::common::spiral::{spiral_scene_with, PROXY_PORT};
use crate::common::stateful_proxy::RecordRoute;
use crate::common::unrun::{establish_keeping_answer, DialogIds};

/// The Call-ID budget every scenario runs under: far above what one call's
/// setup and teardown put on a Call-ID, far below the flood.
const BUDGET: u64 = 60;

/// The messages a looping peer sends: enough that, answered one for one,
/// the node's answers alone pass the budget.
const FLOOD: u32 = 100;

/// The requests a looping peer sends on a call the per-interval cap is
/// tearing down: at [`PACE`] they all fit inside its 32 s
/// `TerminatingTimeout`, and answered one for one they pass the budget.
const ENDING_FLOOD: u32 = 60;

/// The gap between a looping peer's messages: longer than a round trip
/// across the B2BUA on the harness fabric (100 ms a hop, three hops across a
/// spiral), so each one follows the node's answer to the last.
const PACE: Duration = Duration::from_millis(400);

/// 64·T1: how long a mute outlives the last message it dropped.
const MUTE_WINDOW: Duration = Duration::from_millis(TIMER_B);

/// The reason a CDR ended by the budget records.
const BUDGET_REASON: &str = "message-budget";

/// The datagrams the B2BUA sent under `call_id`, in the recording so far.
fn sent_under(s: &B2buaScene, call_id: &str) -> u64 {
    let sent =
        s.h.wire_entries()
            .iter()
            .filter(|e| e.from == s.b2bua.addr)
            .filter(|e| sip_message::sniff::call_id(&e.raw).as_deref() == Some(call_id))
            .count();
    u64::try_from(sent).expect("a recording fits u64")
}

/// The Call-ID of the INVITE the B2BUA sent to the spiral's proxy: the one
/// call 1's outgoing leg and call 2's incoming leg share.
fn spiral_call_id(s: &B2buaScene) -> String {
    s.h.wire_entries()
        .iter()
        .find(|e| e.from == s.b2bua.addr && e.to.port() == PROXY_PORT)
        .filter(|e| e.raw.starts_with(b"INVITE "))
        .and_then(|e| sip_message::sniff::call_id(&e.raw))
        .expect("call 1 sent its INVITE to the proxy")
}

/// The responses queued at `agent`; a request queued there is left
/// unanswered, as a peer that ignores the node's teardown does.
async fn responses_ignoring_requests(agent: &Agent) -> Vec<SipResponse> {
    let mut responses = Vec::new();
    while let Some(msg) = agent.take_queued().await {
        if let SipMessage::Response(r) = msg {
            responses.push(r);
        }
    }
    responses
}

/// What a compliant `agent` has queued: every request is answered 200 and
/// its method returned, every response is returned.
async fn serve(agent: &Agent) -> (Vec<String>, Vec<SipResponse>) {
    let (mut methods, mut responses) = (Vec::new(), Vec::new());
    while let Some(inbound) = agent.take_queued_inbound().await {
        match inbound {
            Inbound::Request(mut txn) => {
                methods.push(txn.request().method().to_string());
                txn.respond(200, "OK").await;
            }
            Inbound::Response(r) => responses.push(*r),
        }
    }
    (methods, responses)
}

/// The CDRs whose events name the budget as their end.
fn ended_by_the_budget(cdrs: &[CdrRecord]) -> usize {
    cdrs.iter()
        .filter(|c| c.events.iter().any(|e| e.reason.as_deref() == Some(BUDGET_REASON)))
        .count()
}

/// The CDRs whose termination names the budget as the cause.
fn caused_by_the_budget(cdrs: &[CdrRecord]) -> usize {
    cdrs.iter()
        .filter(|c| {
            c.termination.as_ref().map(|t| t.cause) == Some(TerminationCause::MessageBudget)
        })
        .count()
}

/// alice's call ends with her BYE; she then sends a new INFO on her ended
/// dialog after each answer, and each draws 481 (RFC 3261 §12.2.2). The
/// B2BUA's answers stop at the budget, the flood's tail is dropped, and no
/// call is left to end. The mute outlasts every drop by 64·T1: an INFO more
/// than 64·T1 after the flood is still dropped because a probe 31 s before
/// it was, and one more than 64·T1 after the last drop is answered 481 again.
#[tokio::test(start_paused = true)]
async fn requests_answered_481_on_a_removed_call_are_muted_at_the_budget() {
    let s = B2buaScene::with_b2bua("b2bua-call-id-budget-removed-call", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| c.max_messages_per_call_id = BUDGET)
    })
    .await;
    s.h.allow_violation(
        "mid-dialog-tags",
        "alice's requests on a dialog the B2BUA no longer holds are the loop under test",
    );
    let (mut dialog, answer) = establish_keeping_answer(&s.alice, &s.bob, s.b2bua.addr).await;
    let ids = DialogIds::of(&answer);
    let call_id = answer.call_id().as_str().to_string();
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;

    let mut cseq = dialog.local_cseq();
    let mut answered = 0;
    for _ in 0..FLOOD {
        cseq += 1;
        let info = ids.info(&s.alice, cseq, &format!("late-{cseq}"));
        s.alice.try_send_datagram(&info, s.b2bua.addr).await.expect("the INFO leaves");
        s.h.advance(PACE).await;
        answered += responses_ignoring_requests(&s.alice).await.len();
    }
    let sent = sent_under(&s, &call_id);
    assert!(sent <= BUDGET, "the B2BUA's sends under alice's Call-ID stop at {BUDGET}: {sent}");
    assert!(
        answered < usize::try_from(FLOOD).expect("FLOOD fits usize"),
        "the flood's tail is dropped, not answered: {answered} of {FLOOD} answered"
    );

    let probe = |cseq: u32| ids.info(&s.alice, cseq, &format!("probe-{cseq}"));
    s.h.advance(MUTE_WINDOW - Duration::from_secs(2)).await;
    cseq += 1;
    s.alice.try_send_datagram(&probe(cseq), s.b2bua.addr).await.expect("the INFO leaves");
    s.h.advance(Duration::from_secs(1)).await;
    let early = responses_ignoring_requests(&s.alice).await;
    assert!(early.is_empty(), "within 64·T1 of the last drop the Call-ID is muted: {early:?}");

    s.h.advance(MUTE_WINDOW - Duration::from_secs(2)).await;
    cseq += 1;
    s.alice.try_send_datagram(&probe(cseq), s.b2bua.addr).await.expect("the INFO leaves");
    s.h.advance(Duration::from_secs(1)).await;
    let restarted = responses_ignoring_requests(&s.alice).await;
    assert!(
        restarted.is_empty(),
        "a drop restarts the 64·T1 window: past 64·T1 after the flood, 31 s after the dropped \
         probe, the Call-ID is still muted: {restarted:?}"
    );

    s.h.advance(MUTE_WINDOW).await;
    cseq += 1;
    s.alice.try_send_datagram(&probe(cseq), s.b2bua.addr).await.expect("the INFO leaves");
    s.h.advance(Duration::from_secs(1)).await;
    let lifted: Vec<u16> =
        responses_ignoring_requests(&s.alice).await.iter().map(SipResponse::status).collect();
    assert_eq!(lifted, vec![481], "64·T1 after the last drop the mute lifts");

    assert_eq!(
        s.b2bua.cdr_records().len(),
        1,
        "a trip after the call's removal ends no call: the BYE's CDR is the only one"
    );
    s.h.advance(MUTE_WINDOW).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// alice and bob relay INFOs through the B2BUA, each sent on the 200 to the
/// last, until the per-interval cap tears the call down. bob answers the
/// cap's BYE; alice ignores hers and goes on sending a new INFO on each
/// answer the ending call gives her. The B2BUA's sends under her Call-ID
/// stop at the budget, and the call ends at the trip, long before its
/// `TerminatingTimeout`, with a CDR naming the budget.
#[tokio::test(start_paused = true)]
async fn a_relay_loop_on_a_terminating_call_ends_the_call_at_the_budget() {
    let s = B2buaScene::with_b2bua("b2bua-call-id-budget-terminating-relay-loop", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.max_messages_per_call = 20;
            c.max_messages_per_call_id = BUDGET;
            c.reaper_sweep_interval_sec = 3600;
        })
    })
    .await;
    let (dialog, answer) = establish_keeping_answer(&s.alice, &s.bob, s.b2bua.addr).await;
    let ids = DialogIds::of(&answer);
    let call_id = answer.call_id().as_str().to_string();

    let mut cseq = dialog.local_cseq();
    let mut torn_down = false;
    for _ in 0..FLOOD {
        cseq += 1;
        let info = ids.info(&s.alice, cseq, &format!("relay-{cseq}"));
        s.alice.try_send_datagram(&info, s.b2bua.addr).await.expect("the INFO leaves");
        s.h.advance(PACE).await;
        let (methods, _) = serve(&s.bob).await;
        s.h.advance(PACE).await;
        responses_ignoring_requests(&s.alice).await;
        if methods.iter().any(|m| m == "BYE") {
            torn_down = true;
            break;
        }
    }
    assert!(torn_down, "the per-interval cap tears the relay loop down");

    let ending = PACE * ENDING_FLOOD;
    for _ in 0..ENDING_FLOOD {
        cseq += 1;
        let info = ids.info(&s.alice, cseq, &format!("ending-{cseq}"));
        s.alice.try_send_datagram(&info, s.b2bua.addr).await.expect("the INFO leaves");
        s.h.advance(PACE).await;
        responses_ignoring_requests(&s.alice).await;
    }
    let sent = sent_under(&s, &call_id);
    assert!(sent <= BUDGET, "the B2BUA's sends under alice's Call-ID stop at {BUDGET}: {sent}");
    assert!(
        s.b2bua.calls_reaped(),
        "the call ended at the budget's trip, within {ending:?} of the teardown, not at its \
         TerminatingTimeout"
    );
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(ended_by_the_budget(&cdrs), 1, "the call's CDR names the budget: {cdrs:?}");
    s.h.advance(MUTE_WINDOW + MUTE_WINDOW).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// bob rings; alice re-sends her INVITE on every 180 the B2BUA's server
/// transaction replays (RFC 3261 §17.2.1). Her Call-ID's budget trips while
/// her INVITE's transaction is the only thing that knows it: no call
/// reference, only her From-tag names the call. The B2BUA's sends under her
/// Call-ID stop at the budget; the call ends: bob is CANCELled, the 503 the
/// teardown owes alice goes to her muted Call-ID and is dropped, so her
/// INVITE ends by her Timer B, and the CDR names the budget as the cause.
#[tokio::test(start_paused = true)]
async fn invite_copies_during_setup_end_the_call_at_the_budget() {
    let s = B2buaScene::with_b2bua("b2bua-call-id-budget-invite-copies", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| c.max_messages_per_call_id = BUDGET)
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let call_id = call.call_id();
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cancelled = false;
    for _ in 0..FLOOD {
        call.retransmit().await;
        s.h.advance(PACE).await;
        while let Some(inbound) = s.bob.take_queued_inbound().await {
            let Inbound::Request(mut cancel) = inbound else { continue };
            if cancel.request().method() == "CANCEL" {
                cancel.respond(200, "OK").await;
                uas.respond(487, "Request Terminated").await;
                cancelled = true;
            }
        }
    }
    let sent = sent_under(&s, &call_id);
    assert!(sent <= BUDGET, "the B2BUA's sends under alice's Call-ID stop at {BUDGET}: {sent}");
    assert!(cancelled, "the budget's trip CANCELs bob");

    s.h.advance(MUTE_WINDOW + Duration::from_secs(1)).await;
    let finals: Vec<u16> = responses_ignoring_requests(&s.alice)
        .await
        .iter()
        .map(SipResponse::status)
        .filter(|status| *status >= 200)
        .collect();
    assert!(finals.is_empty(), "no final reaches alice's muted Call-ID: {finals:?}");

    settle_until(|| s.b2bua.is_reaped()).await;
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one call, one CDR: {cdrs:?}");
    assert_eq!(caused_by_the_budget(&cdrs), 1, "the CDR names the budget as the cause: {cdrs:?}");
    s.h.advance(MUTE_WINDOW + MUTE_WINDOW).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// A spiral (RFC 3261 §16.3) through a proxy that does not Record-Route:
/// call 1's outgoing leg and call 2's incoming leg share one Call-ID. Each
/// INFO alice sends on the 200 to the last, and its 200, cross that Call-ID
/// from the B2BUA to itself, each counted once sent and once received, so it
/// trips first, while alice's and bob's stay under the budget. Both
/// calls holding it end: alice and bob are each sent a BYE, the BYEs between
/// the calls go to the muted Call-ID and are dropped, and both CDRs name the
/// budget as the cause.
#[tokio::test(start_paused = true)]
async fn a_relay_loop_across_a_spiral_ends_both_calls_holding_its_call_id() {
    let (s, _proxy) = spiral_scene_with("b2bua-call-id-budget-spiral", RecordRoute::No, |c| {
        c.max_messages_per_call_id = BUDGET;
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    let answer = call.expect(200).await;
    let dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let ids = DialogIds::of(&answer);
    let spiral = spiral_call_id(&s);

    let mut cseq = dialog.local_cseq();
    let (mut alice_byes, mut bob_byes) = (0, 0);
    for _ in 0..FLOOD {
        cseq += 1;
        let info = ids.info(&s.alice, cseq, &format!("spiral-{cseq}"));
        s.alice.try_send_datagram(&info, s.b2bua.addr).await.expect("the INFO leaves");
        s.h.advance(PACE).await;
        let (at_bob, _) = serve(&s.bob).await;
        s.h.advance(PACE).await;
        let (at_alice, _) = serve(&s.alice).await;
        bob_byes += at_bob.iter().filter(|m| *m == "BYE").count();
        alice_byes += at_alice.iter().filter(|m| *m == "BYE").count();
        if alice_byes > 0 && bob_byes > 0 {
            break;
        }
    }
    let sent = sent_under(&s, &spiral);
    assert!(
        sent <= BUDGET,
        "the B2BUA's sends under the spiral's Call-ID stop at {BUDGET}: {sent}"
    );
    assert_eq!((alice_byes, bob_byes), (1, 1), "the trip ends both calls: alice and bob get a BYE");

    s.h.advance(MUTE_WINDOW + Duration::from_secs(1)).await;
    serve(&s.alice).await;
    serve(&s.bob).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 2, "two calls, two CDRs: {cdrs:?}");
    assert_eq!(caused_by_the_budget(&cdrs), 2, "both CDRs name the budget as the cause: {cdrs:?}");
    s.h.advance(MUTE_WINDOW + MUTE_WINDOW).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
