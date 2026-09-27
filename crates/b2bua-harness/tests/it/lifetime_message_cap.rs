//! The work bound on a call: past `max_messages_per_call_lifetime` events
//! offered for it — counted at dispatch, in every state, while its handler is
//! stuck and its queue full — the call ends and its later traffic draws the
//! answers of a call that no longer exists.

use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::helpers::TERMINATING_TIMEOUT_MS;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
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
    assert_eq!(s.b2bua.metrics().message_cap_lifetime_terminated_total(), 1, "the cap is crossed");

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
