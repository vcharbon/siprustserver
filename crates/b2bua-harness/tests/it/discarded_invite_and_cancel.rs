//! An INVITE or a CANCEL the B2BUA took off the transaction layer and whose
//! handler body it did not run. The layer already sent the INVITE's
//! 100 Trying, which stopped the caller's retransmissions, and already
//! answered the CANCEL 200 + 487, so neither is ever sent again: the discard
//! site answers the INVITE, and a `Cancelled` is never discarded for want of
//! room.

use std::time::Duration;

use b2bua_harness::{establish, settle_until, stated_by_response, B2buaScene, B2buaSut};
use scenario_harness::callflow::OFFER_SDP;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::{SipMessage, SipResponse};

use crate::common::unrun::{establish_keeping_answer, one_permit_one_deep, DialogIds};

/// The Retry-After seconds `resp` states.
fn retry_after(resp: &SipResponse) -> u32 {
    stated_by_response(resp, "Retry-After")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| panic!("the {} carries a Retry-After", resp.status()))
}

/// The final responses to an INVITE queued at `agent` after `wait`.
async fn invite_finals(
    h: &Harness,
    agent: &scenario_harness::Agent,
    wait: Duration,
) -> Vec<SipResponse> {
    h.advance(wait).await;
    let mut out = Vec::new();
    while let Some(msg) = agent.take_queued().await {
        if let SipMessage::Response(r) = msg {
            if r.cseq().method().as_str() == "INVITE" && r.status() >= 200 {
                out.push(r);
            }
        }
    }
    out
}

/// alice's two INFOs fill her call's worker and queue; her re-INVITE is
/// dropped at dispatch. Its 100 Trying silenced her retransmissions, so the
/// drop is answered at once: 500 with a Retry-After of at least 1 s
/// (RFC 3261 §14.2), ACKed on its branch. The call carries on.
#[tokio::test(start_paused = true)]
async fn a_reinvite_the_full_per_call_queue_drops_is_answered_500_with_retry_after() {
    let s = one_permit_one_deep("b2bua-reinvite-dropped-queue-full").await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut dialog = s.establish().await;

    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut info1 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut info2 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;

    let mut reinvite = dialog.reinvite(Some(OFFER_SDP)).await;
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 1, "the re-INVITE is dropped at dispatch");
    let refused = reinvite
        .try_expect(500)
        .await
        .expect("the dropped re-INVITE is answered 500, not left on its 100 Trying");
    assert!(retry_after(&refused) >= 1, "Retry-After is at least 1 s");
    assert_eq!(s.b2bua.metrics().invite_discard_answered_total(), 1);

    // carol's decision deadline frees the permit; the INFOs are relayed.
    parked.expect(503).await;
    for info in [&mut info1, &mut info2] {
        let mut uas = s.bob.receive("INFO").await;
        uas.respond(200, "OK").await;
        info.expect(200).await;
    }

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// The call cap is one. A late re-INVITE for a call that has ended finds no
/// queue while another call holds the cap, and is dropped at dispatch after
/// its 100 Trying. It is answered 500 + Retry-After; its ACK ends at the
/// B2BUA's transaction layer.
#[tokio::test(start_paused = true)]
async fn a_late_reinvite_dropped_at_the_call_cap_is_answered_500_with_retry_after() {
    let s = B2buaScene::with_b2bua("b2bua-reinvite-dropped-at-cap", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| c.per_call_queue_cap = 1)
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let (mut ended, answer) = establish_keeping_answer(&s.alice, &s.bob, s.b2bua.addr).await;
    let ids = DialogIds::of(&answer);
    let cseq = ended.local_cseq() + 2;
    s.hangup(&mut ended).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.h.allow_violation(
        "mid-dialog-tags",
        "a re-INVITE in a dialog the B2BUA no longer holds is the deviation under test",
    );

    let mut holding = establish(&carol, &s.bob, s.b2bua.addr).await;
    settle_until(|| s.b2bua.active_calls() == 1).await;

    let reinvite = ids.reinvite(&s.alice, cseq, "late-reinvite-at-cap");
    s.alice.try_send_datagram(&reinvite, s.b2bua.addr).await.expect("the re-INVITE leaves");
    // The 500 arrives before its first Timer G copy (T1).
    let finals = invite_finals(&s.h, &s.alice, Duration::from_millis(600)).await;
    assert_eq!(s.b2bua.metrics().cap_drops_total(), 1, "the re-INVITE is dropped at the cap");
    let statuses: Vec<u16> = finals.iter().map(SipResponse::status).collect();
    assert_eq!(statuses, vec![500], "the dropped re-INVITE is answered, not left on its 100");
    assert!(retry_after(&finals[0]) >= 1, "Retry-After is at least 1 s");
    let ack = ids.ack_non_2xx(&s.alice, cseq, "late-reinvite-at-cap");
    s.alice.try_send_datagram(&ack, s.b2bua.addr).await.expect("the ACK leaves");
    let _ = invite_finals(&s.h, &s.alice, Duration::from_millis(1_000)).await;
    assert!(
        invite_finals(&s.h, &s.alice, Duration::from_millis(5_000)).await.is_empty(),
        "the ACK stops the 500's Timer G retransmission"
    );

    scenario_harness::callflow::hangup(&mut holding, &s.bob).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// bob rings; two of his provisionals fill alice's call's worker and queue.
/// alice CANCELs: the layer answers 200 + 487 and hands `Cancelled` up, which
/// finds the queue full. It is kept, not dropped: once the permit frees, the
/// call runs it after the provisionals and CANCELs bob.
#[tokio::test(start_paused = true)]
async fn a_cancel_arriving_on_a_full_per_call_queue_still_cancels_the_callee() {
    let s = one_permit_one_deep("b2bua-cancel-queue-full").await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut b_invite = s.bob.receive("INVITE").await;
    b_invite.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    b_invite.respond(183, "Session Progress").await;
    s.h.advance(Duration::from_millis(300)).await;
    b_invite.respond(180, "Ringing").await;
    s.h.advance(Duration::from_millis(300)).await;

    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    call.expect(487).await;
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 0, "the Cancelled is not dropped");
    assert_eq!(s.b2bua.metrics().past_bound_total(), 1, "it waits past the full queue");

    // carol's decision deadline frees the permit; the provisionals run, then
    // the Cancelled.
    parked.expect(503).await;
    let mut b_cancel =
        s.bob.try_receive("CANCEL").await.expect("the callee is CANCELed, not left ringing");
    b_cancel.respond(200, "OK").await;
    // The queue is still one deep: bob's answers arrive one at a time.
    s.h.advance(Duration::from_millis(300)).await;
    b_invite.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
