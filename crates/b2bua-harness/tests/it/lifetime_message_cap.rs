//! The work bound on a call: past `max_messages_per_call_lifetime` events
//! offered for it — counted at dispatch, in every state, while its handler is
//! stuck and its queue full — the call ends. Its later requests draw the
//! answer of a call that no longer exists, 481, where they are refused; the
//! responses its teardown needs still get in. With the reaper off nothing
//! would end a capped call, and the cap is not installed.

use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::helpers::TERMINATING_TIMEOUT_MS;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::WaiverScope;
use sip_message::SipMessage;

use crate::common::unrun::{DialogIds, RouteFirstThenHang};

/// bob hangs up; alice never answers the BYE and floods her call with INFOs
/// while a parked call holds the only handler permit, keeping the call
/// Terminating with its queue full. The flood crosses the call's lifetime cap
/// of 20: the call is forced terminal at the cap, long before its
/// `TerminatingTimeout`, and writes its CDR. An INFO after that draws 481.
#[tokio::test(start_paused = true)]
async fn a_terminating_call_flooded_past_its_lifetime_cap_ends_at_the_cap() {
    let s = B2buaScene::with_b2bua("b2bua-lifetime-message-cap", |bob_port| {
        B2buaSut::builder(Arc::new(RouteFirstThenHang::to("127.0.0.1", bob_port))).tune(|c| {
            c.event_dispatch_concurrency = 1;
            c.per_call_queue_depth = 1;
            c.max_messages_per_call_lifetime = 20;
            c.reaper_sweep_interval_sec = 3600;
        })
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    let answer = call.expect(200).await;
    let dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let ids = DialogIds::of(&answer);
    let mut b_dialog = uas.dialog();

    // bob hangs up; alice, the flooder, never answers the B2BUA's BYE.
    let mut bye = b_dialog.bye().await;
    bye.expect(200).await;
    let _unanswered = s.alice.receive("BYE").await;
    let terminating_since = tokio::time::Instant::now();

    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let base = dialog.local_cseq();
    for cseq in base + 1..=base + 30 {
        let info = ids.info(&s.alice, cseq, &format!("flood-{cseq}"));
        s.alice.try_send_datagram(&info, s.b2bua.addr).await.expect("the INFO leaves");
    }
    s.h.advance(Duration::from_millis(500)).await;
    assert_eq!(s.b2bua.metrics().message_cap_lifetime_crossed_total(), 1, "the cap is crossed");

    // The permit frees: the verdict, admitted past every bound, ends the call.
    parked.expect(503).await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    assert!(
        terminating_since.elapsed() < Duration::from_millis(TERMINATING_TIMEOUT_MS as u64),
        "the call ended at the cap, not at its TerminatingTimeout"
    );
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(
        cdrs.iter()
            .filter(|c| c
                .events
                .iter()
                .any(|e| e.reason.as_deref() == Some("message-cap-lifetime")))
            .count(),
        1,
        "the capped call wrote one CDR naming the cap: {cdrs:?}"
    );

    // Later traffic finds no call.
    while s.alice.take_queued().await.is_some() {}
    let late = base + 31;
    let info = ids.info(&s.alice, late, &format!("flood-{late}"));
    s.alice.try_send_datagram(&info, s.b2bua.addr).await.expect("the INFO leaves");
    s.h.advance(Duration::from_millis(500)).await;
    let mut late_finals = Vec::new();
    while let Some(msg) = s.alice.take_queued().await {
        if let SipMessage::Response(r) = msg {
            if r.cseq().seq() == late {
                late_finals.push(r.status());
            }
        }
    }
    assert_eq!(late_finals, vec![481], "a message after the cap draws 481");
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// bob rings and keeps sending provisionals until the call crosses its
/// lifetime cap of 10. The cap's teardown CANCELs him, and his 200 crosses
/// the CANCEL. A final to a request the node sent gets in past the cap, so
/// the 200 is ACKed and the dialog it opened is ended with a BYE
/// (RFC 3261 §13.2.2.4, §15). alice, never answered, is sent the 503 the
/// teardown owes her.
#[tokio::test(start_paused = true)]
async fn a_capped_calls_callee_answering_across_the_cancel_is_acked_and_sent_bye() {
    let s = B2buaScene::with_b2bua("b2bua-lifetime-cap-200-crosses-cancel", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.max_messages_per_call_lifetime = 10;
            c.reaper_sweep_interval_sec = 3600;
        })
    })
    .await;
    s.h.waive(
        WaiverScope::rule(
            "no-200-after-cancel",
            "bob answers 200 after taking the CANCEL (RFC 3261 §9.2): the crossing under test",
        )
        .on_party("bob"),
    );
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    for _ in 0..10 {
        uas.respond(180, "Ringing").await;
    }
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(s.b2bua.metrics().message_cap_lifetime_crossed_total(), 1, "the cap is crossed");

    let mut cancel = s.bob.try_receive("CANCEL").await.expect("the cap's teardown CANCELs bob");
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    cancel.respond(200, "OK").await;
    s.bob.try_receive("ACK").await.expect("bob's 200 that crossed the CANCEL is ACKed");
    let mut bye = s.bob.try_receive("BYE").await.expect("the dialog bob's 200 opened is ended");
    bye.respond(200, "OK").await;
    let _ = call.try_expect_final(503).await.expect("alice is answered the teardown's 503");

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// alice floods her confirmed call with INFOs while a parked call holds the
/// only handler permit, crossing its lifetime cap of 20. bob, who did
/// nothing wrong, then hangs up. A capped call runs no more requests, so
/// bob's BYE is answered where it is refused, 481 through its transaction,
/// not left to his Timer F.
#[tokio::test(start_paused = true)]
async fn a_bye_to_a_capped_call_is_answered_where_it_is_refused() {
    let s = B2buaScene::with_b2bua("b2bua-lifetime-cap-bye-answered", |bob_port| {
        B2buaSut::builder(Arc::new(RouteFirstThenHang::to("127.0.0.1", bob_port))).tune(|c| {
            c.event_dispatch_concurrency = 1;
            c.per_call_queue_depth = 1;
            c.max_messages_per_call_lifetime = 20;
            c.reaper_sweep_interval_sec = 3600;
        })
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    let answer = call.expect(200).await;
    let dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let ids = DialogIds::of(&answer);
    let mut b_dialog = uas.dialog();

    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let base = dialog.local_cseq();
    for cseq in base + 1..=base + 30 {
        let info = ids.info(&s.alice, cseq, &format!("flood-{cseq}"));
        s.alice.try_send_datagram(&info, s.b2bua.addr).await.expect("the INFO leaves");
    }
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(s.b2bua.metrics().message_cap_lifetime_crossed_total(), 1, "the cap is crossed");

    let mut bye = b_dialog.bye().await;
    bye.try_expect(481).await.expect("bob's BYE is answered at once, not left unanswered");
    let metrics = s.b2bua.metrics();
    assert!(metrics.capped_refusals_total() > 1, "alice's later INFOs are refused too");
    assert_eq!(
        metrics.capped_request_answered_total(),
        metrics.capped_refusals_total(),
        "every request refused past the cap is answered"
    );
    s.h.advance(Duration::from_millis(300)).await;
    while s.alice.take_queued().await.is_some() {}

    // The permit frees: the INFO queued before the cap reaches bob, whose
    // dialog is gone, then the cap's teardown BYE. alice never answers hers:
    // the call ends at its TerminatingTimeout.
    parked.expect(503).await;
    s.bob.receive("INFO").await.respond(481, "Call/Transaction Does Not Exist").await;
    s.bob.receive("BYE").await.respond(481, "Call/Transaction Does Not Exist").await;
    let _unanswered = s.alice.receive("BYE").await;
    s.h.advance(Duration::from_millis(TERMINATING_TIMEOUT_MS as u64)).await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(
        cdrs.iter()
            .filter(|c| c
                .events
                .iter()
                .any(|e| e.reason.as_deref() == Some("message-cap-lifetime")))
            .count(),
        1,
        "the capped call writes one CDR naming the cap: {cdrs:?}"
    );
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// With the reaper off nothing would end a call past its lifetime cap, so
/// the cap is not installed: a call past it hangs up as usual.
#[tokio::test(start_paused = true)]
async fn with_the_reaper_off_a_call_past_the_lifetime_cap_hangs_up_as_usual() {
    let s = B2buaScene::with_b2bua("b2bua-lifetime-cap-reaper-off", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.reaper_enabled = false;
            c.max_messages_per_call_lifetime = 3;
        })
    })
    .await;
    let mut dialog = s.establish().await;
    let mut bye = dialog.bye().await;
    let mut uas = s.bob.try_receive("BYE").await.expect("the BYE is relayed past the cap");
    uas.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    assert_eq!(s.b2bua.metrics().message_cap_lifetime_crossed_total(), 0);
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
