//! A client transaction's outcome — the final the transaction layer matched
//! to a request the B2BUA sent, or its Timeout — is delivered once: the
//! layer ACKs a non-2xx INVITE final and absorbs its repeats, forgets a
//! transaction on its 2xx or non-INVITE final, and gives up once. So the
//! per-call dispatcher never drops one for want of room: it waits past a
//! full queue, and the call acts on it once its worker frees.

use std::time::Duration;

use b2bua_harness::settle_until;
use scenario_harness::callflow::OFFER_SDP;
use sip_message::generators::InDialogMethod;

use crate::common::unrun::one_permit_one_deep;

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
