//! A non-INVITE request the B2BUA took off the transaction layer but discarded
//! before any handler ran — the call's queue full, or the call cap reached —
//! leaves no transaction behind to absorb its retransmissions. The peer's
//! Timer E copy (RFC 3261 §17.1.2.2) is admitted afresh and draws its answer
//! once the B2BUA has room, instead of silence until the peer's Timer F.

use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{establish, settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::OFFER_SDP;
use scenario_harness::{Agent, Harness};
use sip_message::generators::InDialogMethod;
use sip_message::SipMessage;

use crate::common::unrun::{establish_keeping_answer, DialogIds, RouteFirstThenHang};

/// The final responses to a BYE queued at `agent` after `wait`.
async fn bye_finals(h: &Harness, agent: &Agent, wait: Duration) -> Vec<u16> {
    h.advance(wait).await;
    let mut out = Vec::new();
    while let Some(msg) = agent.take_queued().await {
        if let SipMessage::Response(r) = msg {
            if r.cseq().method().as_str() == "BYE" && r.status() >= 200 {
                out.push(r.status());
            }
        }
    }
    out
}

/// One handler permit, held by a second call parked on its decision; a
/// per-call queue one deep. alice's two INFOs fill the call's worker and
/// queue, so her BYE is dropped at dispatch. Its retransmission after the
/// permit frees is processed: the BYE is answered and relayed, the call ends.
#[tokio::test(start_paused = true)]
async fn a_bye_the_full_per_call_queue_dropped_is_answered_on_its_retransmission() {
    let s = B2buaScene::with_b2bua("b2bua-bye-dropped-queue-full", |bob_port| {
        B2buaSut::builder(Arc::new(RouteFirstThenHang::to("127.0.0.1", bob_port))).tune(|c| {
            c.event_dispatch_concurrency = 1;
            c.per_call_queue_depth = 1;
        })
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let (mut dialog, answer) = establish_keeping_answer(&s.alice, &s.bob, s.b2bua.addr).await;
    let ids = DialogIds::of(&answer);

    // carol's call parks on its decision and holds the one handler permit.
    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;

    // The first INFO waits on the permit in the call's worker, the second
    // fills the call's queue, the BYE is dropped.
    let mut info1 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut info2 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let bye = ids.bye(&s.alice, dialog.local_cseq() + 1, "bye-queue-full");
    s.alice.try_send_datagram(&bye, s.b2bua.addr).await.expect("the BYE leaves");
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 1, "the BYE is dropped at dispatch");

    // Timer E copies while the call is still blocked: dropped again, silent.
    s.alice.try_send_datagram(&bye, s.b2bua.addr).await.expect("the BYE leaves");
    assert!(bye_finals(&s.h, &s.alice, Duration::from_millis(1_000)).await.is_empty());
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 2, "the copy is admitted and dropped");
    assert_eq!(s.b2bua.txn_metrics().unanswered_forgotten(), 2, "one forget per dropped copy");

    // carol's decision deadline frees the permit; the INFOs are relayed.
    parked.expect(503).await;
    for info in [&mut info1, &mut info2] {
        let mut uas = s.bob.receive("INFO").await;
        uas.respond(200, "OK").await;
        info.expect(200).await;
    }

    // The next Timer E copy reaches the call and is answered.
    s.alice.try_send_datagram(&bye, s.b2bua.addr).await.expect("the BYE leaves");
    assert_eq!(
        bye_finals(&s.h, &s.alice, Duration::from_millis(500)).await,
        vec![200],
        "the retransmitted BYE is answered, not absorbed"
    );
    let mut relayed = s.bob.receive("BYE").await;
    relayed.respond(200, "OK").await;

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// The call cap is one. A late BYE for a call that has ended finds no queue
/// while another call holds the cap, and is dropped at dispatch. Once that
/// call ends, the BYE's retransmission reaches the orphan path and draws 481
/// (RFC 3261 §15.1.2).
#[tokio::test(start_paused = true)]
async fn a_late_bye_dropped_at_the_call_cap_draws_481_once_the_cap_frees() {
    let s = B2buaScene::with_b2bua("b2bua-bye-dropped-at-cap", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| c.per_call_queue_cap = 1)
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let (mut ended, answer) = establish_keeping_answer(&s.alice, &s.bob, s.b2bua.addr).await;
    let ids = DialogIds::of(&answer);
    let bye_cseq = ended.local_cseq() + 2;
    s.hangup(&mut ended).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.h.allow_violation(
        "mid-dialog-tags",
        "a BYE in a dialog the B2BUA no longer holds is the deviation under test",
    );

    let mut holding = establish(&carol, &s.bob, s.b2bua.addr).await;
    settle_until(|| s.b2bua.active_calls() == 1).await;

    let bye = ids.bye(&s.alice, bye_cseq, "late-bye-at-cap");
    s.alice.try_send_datagram(&bye, s.b2bua.addr).await.expect("the BYE leaves");
    assert!(bye_finals(&s.h, &s.alice, Duration::from_millis(300)).await.is_empty());
    assert_eq!(s.b2bua.metrics().cap_drops_total(), 1, "the BYE is dropped at the cap");
    s.alice.try_send_datagram(&bye, s.b2bua.addr).await.expect("the BYE leaves");
    assert!(bye_finals(&s.h, &s.alice, Duration::from_millis(1_000)).await.is_empty());
    assert_eq!(s.b2bua.metrics().cap_drops_total(), 2, "the copy is admitted and dropped");
    assert_eq!(s.b2bua.txn_metrics().unanswered_forgotten(), 2, "one forget per dropped copy");

    // Freeing the cap: carol hangs up.
    scenario_harness::callflow::hangup(&mut holding, &s.bob).await;
    settle_until(|| s.b2bua.is_reaped()).await;

    s.alice.try_send_datagram(&bye, s.b2bua.addr).await.expect("the BYE leaves");
    assert_eq!(
        bye_finals(&s.h, &s.alice, Duration::from_millis(500)).await,
        vec![481],
        "the retransmitted BYE reaches the orphan path, not a transaction absorbing it"
    );
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
