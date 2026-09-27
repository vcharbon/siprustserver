//! A client transaction's outcome — the final the transaction layer matched
//! to a request the B2BUA sent, or its Timeout — is delivered once: the
//! layer ACKs a non-2xx INVITE final and absorbs its repeats, forgets a
//! transaction on its 2xx or non-INVITE final, and gives up once. So the
//! per-call dispatcher never drops one for want of room: it waits past a
//! full queue, and the call acts on it once its worker frees.

use std::time::Duration;

use b2bua_harness::settle_until;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use sip_message::generators::InDialogMethod;

use crate::common::unrun::{one_permit_one_deep, one_permit_one_deep_with};

/// bob rings; while a parked call holds the only handler permit, two more
/// of his provisionals fill alice's call's worker and queue, and he rejects
/// the call 486. The layer ACKs the 486 and will not deliver it again, so it
/// waits past the full queue: once the permit frees, alice hears it.
#[tokio::test(start_paused = true)]
async fn a_callee_reject_arriving_on_a_full_per_call_queue_reaches_the_caller() {
    let s = one_permit_one_deep("b2bua-reject-queue-full").await;
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
    b_invite.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 0, "the 486 is not dropped");
    assert_eq!(s.b2bua.metrics().past_bound_total(), 1, "it waits past the full queue");

    // carol's decision deadline frees the permit; the provisionals run, then
    // the 486.
    parked.expect(503).await;
    call.expect(183).await;
    call.expect(180).await;
    call.try_expect(486).await.expect("the callee's reject reaches the caller");

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// alice's INFO is relayed to bob. While a parked call holds the only
/// handler permit, two more INFOs fill her call's worker and queue, and bob
/// answers the first 200. The layer forgets a non-INVITE client transaction
/// on its final, so the 200 waits past the full queue: once the permit
/// frees, alice's first INFO is answered.
#[tokio::test(start_paused = true)]
async fn a_relayed_requests_final_arriving_on_a_full_per_call_queue_is_relayed_back() {
    let s = one_permit_one_deep("b2bua-info-final-queue-full").await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut dialog = s.establish().await;

    let mut info1 = dialog.send_request(InDialogMethod::Info).send().await;
    let mut relayed1 = s.bob.receive("INFO").await;

    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut info2 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut info3 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    relayed1.respond(200, "OK").await;
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 0, "the 200 is not dropped");
    assert_eq!(s.b2bua.metrics().past_bound_total(), 1, "it waits past the full queue");

    // carol's decision deadline frees the permit: INFO 2 and 3 are relayed,
    // then the 200 reaches alice.
    parked.expect(503).await;
    for _ in 0..2 {
        let mut uas = s.bob.receive("INFO").await;
        uas.respond(200, "OK").await;
    }
    info1.try_expect(200).await.expect("the callee's 200 to the first INFO reaches the caller");
    info2.expect(200).await;
    info3.expect(200).await;

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// alice's re-INVITE is relayed to bob, who never answers it. While a
/// parked call holds the only handler permit, two INFOs fill her call's
/// worker and queue, and the relayed re-INVITE's Timer B expires there: the
/// layer gives up once, so its Timeout waits past the full queue, and once
/// the permit frees alice's re-INVITE draws its failure final.
#[tokio::test(start_paused = true)]
async fn a_relayed_reinvite_timeout_on_a_full_per_call_queue_reaches_the_call() {
    let s = one_permit_one_deep_with("b2bua-reinvite-timeout-queue-full", |c| {
        c.call_control_timeout_ms = 40_000;
        // No keepalive fires inside the window.
        c.keepalive_interval_sec = 300;
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut dialog = s.establish().await;

    let mut reinvite = dialog.reinvite(Some(OFFER_SDP)).await;
    // bob never answers the re-INVITE: the silence under test.
    let _b_reinvite = s.bob.receive("INVITE").await;

    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut info1 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut info2 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_secs(33)).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 0, "the Timeout is not dropped");
    assert_eq!(s.b2bua.metrics().past_bound_total(), 1, "it waits past the full queue");
    s.h.advance(Duration::from_secs(6)).await;

    // The permit frees: the INFOs are relayed, then the Timeout, which fails
    // the re-INVITE to alice, CANCELs bob's and ends the dialog whose peer
    // went silent.
    parked.expect(503).await;
    for _ in 0..2 {
        s.bob.receive("INFO").await.respond(200, "OK").await;
    }
    reinvite.try_expect(487).await.expect("the Timeout reaches the call");
    let mut cancel = s.bob.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    s.alice.receive("BYE").await.respond(200, "OK").await;
    info1.expect(200).await;
    info2.expect(200).await;

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// alice's INFO is relayed to bob, who never answers it. While a parked call
/// holds the only handler permit, two more INFOs fill her call's worker and
/// queue, and the relayed INFO's Timer F expires there: its Timeout waits
/// past the full queue, and once the permit frees the call acts on it,
/// ending the dialog whose peer went silent.
#[tokio::test(start_paused = true)]
async fn a_relayed_requests_timeout_on_a_full_per_call_queue_reaches_the_call() {
    let s = one_permit_one_deep_with("b2bua-info-timeout-queue-full", |c| {
        c.call_control_timeout_ms = 40_000;
        // No keepalive fires inside the window.
        c.keepalive_interval_sec = 300;
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut dialog = s.establish().await;

    let mut info1 = dialog.send_request(InDialogMethod::Info).send().await;
    let _unanswered = s.bob.receive("INFO").await;

    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut info2 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut info3 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_secs(33)).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 0, "the Timeout is not dropped");
    assert_eq!(s.b2bua.metrics().past_bound_total(), 1, "it waits past the full queue");
    s.h.advance(Duration::from_secs(6)).await;

    // The permit frees: INFO 2 and 3 are relayed, then the Timeout, which
    // ends the call on the wire.
    parked.expect(503).await;
    for _ in 0..2 {
        s.bob.receive("INFO").await.respond(200, "OK").await;
    }
    s.bob.try_receive("BYE").await.expect("the Timeout reaches the call").respond(200, "OK").await;
    s.alice.receive("BYE").await.respond(200, "OK").await;
    info2.expect(200).await;
    info3.expect(200).await;
    info1.try_expect(481).await.expect("the timed-out INFO is answered");

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// bob rings; while a parked call holds the only handler permit, two more
/// of his provisionals fill alice's call's worker and queue, and he answers
/// 200. The layer forgets the transaction on its 2xx, so the 200 waits past
/// the full queue rather than on bob's retransmission: once the permit
/// frees, bob is ACKed and alice answered, with no 2xx repeat on the wire.
#[tokio::test(start_paused = true)]
async fn a_callee_answer_on_a_full_per_call_queue_is_acked_without_its_retransmission() {
    let s = one_permit_one_deep("b2bua-answer-queue-full").await;
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
    b_invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 0, "the 200 is not dropped");
    assert_eq!(s.b2bua.metrics().past_bound_total(), 1, "it waits past the full queue");

    parked.expect(503).await;
    call.expect(183).await;
    call.expect(180).await;
    call.try_expect(200).await.expect("the callee's answer reaches the caller");
    let mut dialog = call.ack().await;
    s.bob.try_receive("ACK").await.expect("bob is ACKed without repeating his 200");

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
