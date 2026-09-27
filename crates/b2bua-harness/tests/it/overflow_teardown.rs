//! A call whose dispatch overflow reaches its ceiling is torn down by the
//! call reaper. Only a peer that breaks RFC 3261 §14.1 (at most one pending
//! re-INVITE per direction) can get there, so the other side did nothing
//! wrong and is alive: it hears the teardown on the wire — a BYE on a
//! confirmed leg — and the call writes its CDR.

use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::OFFER_SDP;
use sip_message::generators::InDialogMethod;
use sip_message::SipMessage;

use crate::common::unrun::{establish_keeping_answer, DialogIds, RouteFirstThenHang};

/// alice's two INFOs fill her call's worker and one-deep queue while a parked
/// call holds the only handler permit. She then sends three re-INVITEs, each
/// CANCELed at once: every CANCEL is matched before the discard answer to its
/// re-INVITE leaves, so three `Cancelled` events arrive for a call whose
/// overflow holds one. The second is refused at the ceiling and the reaper
/// condemns the call. Once the permit frees, the call is torn down: bob hears
/// a BYE, alice hears a BYE, and the CDR is written.
#[tokio::test(start_paused = true)]
async fn a_call_flooded_past_its_overflow_ceiling_is_torn_down_on_the_wire() {
    let s = B2buaScene::with_b2bua("b2bua-overflow-condemned-teardown", |bob_port| {
        B2buaSut::builder(Arc::new(RouteFirstThenHang::to("127.0.0.1", bob_port))).tune(|c| {
            c.event_dispatch_concurrency = 1;
            c.per_call_queue_depth = 1;
            // No sweep inside the test: only the verdict the ceiling sends at
            // once can end the call, so it must get past the full queue.
            c.reaper_sweep_interval_sec = 3600;
        })
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let (mut dialog, answer) = establish_keeping_answer(&s.alice, &s.bob, s.b2bua.addr).await;
    let ids = DialogIds::of(&answer);

    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let _info1 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let _info2 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;

    // The non-compliant burst: three re-INVITEs pending at once, each CANCELed.
    s.h.allow_violation(
        "no-re-invite-while-invite-in-progress",
        "a peer with several re-INVITEs pending at once (RFC 3261 §14.1) is the deviation \
         under test",
    );
    s.h.allow_violation(
        "concurrent-re-invite-500-or-491",
        "the peer's burst of concurrent re-INVITEs is the deviation under test; its own \
         CANCEL of each is matched first, so each draws 487 (RFC 3261 §9.2), not 500",
    );
    let base = dialog.local_cseq();
    for n in 1..=3 {
        let branch = format!("burst-{n}");
        let reinvite = ids.reinvite(&s.alice, base + n, &branch);
        let cancel = ids.cancel(&s.alice, base + n, &branch);
        s.alice.try_send_datagram(&reinvite, s.b2bua.addr).await.expect("the re-INVITE leaves");
        s.alice.try_send_datagram(&cancel, s.b2bua.addr).await.expect("the CANCEL leaves");
    }
    // The answers arrive before their first Timer G copy (T1).
    s.h.advance(Duration::from_millis(450)).await;
    assert!(s.b2bua.metrics().overflow_refused_total() >= 1, "the ceiling refused a Cancelled");

    // alice ACKs each re-INVITE's final hop by hop.
    let mut finals = Vec::new();
    while let Some(msg) = s.alice.take_queued().await {
        if let SipMessage::Response(r) = msg {
            if r.cseq().method().as_str() == "INVITE" && r.status() >= 300 {
                finals.push((r.cseq().seq(), r.status()));
            }
        }
    }
    for (cseq, _) in &finals {
        let n = cseq - base;
        let ack = ids.ack_non_2xx(&s.alice, *cseq, &format!("burst-{n}"));
        s.alice.try_send_datagram(&ack, s.b2bua.addr).await.expect("the ACK leaves");
    }
    assert_eq!(finals.len(), 3, "every re-INVITE drew its final: {finals:?}");

    // carol's decision deadline frees the permit: the INFOs, the Cancelled the
    // overflow kept, then the reaper's verdict.
    parked.expect(503).await;
    // bob answers one message at a time: the call's queue is one deep.
    for _ in 0..2 {
        let mut uas = s.bob.receive("INFO").await;
        uas.respond(200, "OK").await;
        s.h.advance(Duration::from_millis(300)).await;
    }
    let mut b_bye = s.bob.try_receive("BYE").await.expect("bob hears the teardown");
    b_bye.respond(200, "OK").await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut a_bye = s.alice.try_receive("BYE").await.expect("alice hears the teardown");
    a_bye.respond(200, "OK").await;

    // Each of alice's INFOs drew its final, relayed or refused by the teardown.
    s.h.advance(Duration::from_millis(500)).await;
    let mut info_finals = Vec::new();
    while let Some(msg) = s.alice.take_queued().await {
        if let SipMessage::Response(r) = msg {
            if r.cseq().method().as_str() == "INFO" && r.status() >= 200 {
                info_finals.push(r.cseq().seq());
            }
        }
    }
    info_finals.sort_unstable();
    info_finals.dedup();
    assert_eq!(info_finals.len(), 2, "both INFOs answered: {info_finals:?}");

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    let cdrs = s.b2bua.cdr_records();
    let flooded: Vec<_> = cdrs
        .iter()
        .filter(|c| c.events.iter().any(|e| e.reason.as_deref() == Some("dispatch-overflow")))
        .collect();
    assert_eq!(flooded.len(), 1, "the flooded call wrote one CDR naming its teardown: {cdrs:?}");
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
