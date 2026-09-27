//! A call whose dispatch overflow reaches its ceiling is torn down by the
//! call reaper. Only a peer that breaks RFC 3261 §14.1 (at most one pending
//! re-INVITE per direction) can get there, so the other side did nothing
//! wrong and is alive: it hears the teardown on the wire — a BYE on a
//! confirmed leg — and the call writes its CDR, even while the flood goes on.

use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::helpers::TERMINATING_TIMEOUT_MS;
use scenario_harness::agent::WaiverScope;
use scenario_harness::callflow::OFFER_SDP;
use scenario_harness::{ClientInvite, Dialog};
use sip_message::generators::InDialogMethod;
use sip_message::SipMessage;

use crate::common::unrun::{establish_keeping_answer, DialogIds, RouteFirstThenHang};

/// One handler permit, a per-call queue one deep, and no reaper sweep inside
/// the test: only the verdict the ceiling sends at once can end the call.
async fn scene(name: &str) -> B2buaScene {
    let s = B2buaScene::with_b2bua(name, |bob_port| {
        B2buaSut::builder(Arc::new(RouteFirstThenHang::to("127.0.0.1", bob_port))).tune(|c| {
            c.event_dispatch_concurrency = 1;
            c.per_call_queue_depth = 1;
            c.reaper_sweep_interval_sec = 3600;
        })
    })
    .await;
    s.h.waive(
        WaiverScope::rule(
            "no-re-invite-while-invite-in-progress",
            "alice's burst of re-INVITEs pending at once (RFC 3261 §14.1) is the deviation \
             under test",
        )
        .on_party("alice"),
    );
    // The rule names no offending message, so it cannot be scoped to a party;
    // it is waived by rule, and an unused waiver still fails the run.
    s.h.waive(WaiverScope::rule(
        "concurrent-re-invite-500-or-491",
        "validator false positive: alice's own CANCEL of each concurrent re-INVITE is \
         matched first, so it draws 487 (RFC 3261 §9.2), not 500",
    ));
    s
}

/// alice's call, flooded past its overflow ceiling while a parked call holds
/// the only handler permit.
struct Flooded {
    s: B2buaScene,
    dialog: Dialog,
    ids: DialogIds,
    /// carol's call, parked on its decision, holding the permit.
    parked: ClientInvite,
}

/// alice's two INFOs fill her call's worker and queue; then three re-INVITEs,
/// each CANCELed at once. Every CANCEL is matched before the discard answer
/// to its re-INVITE leaves, so three `Cancelled` events arrive for a call
/// whose overflow holds one: the second is refused at the ceiling and the
/// reaper condemns the call.
async fn flood_past_the_ceiling(name: &str) -> Flooded {
    let s = scene(name).await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let (mut dialog, answer) = establish_keeping_answer(&s.alice, &s.bob, s.b2bua.addr).await;
    let ids = DialogIds::of(&answer);

    let parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let _info1 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let _info2 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;

    let base = dialog.local_cseq();
    burst(&s, &ids, base).await;
    assert!(s.b2bua.metrics().overflow_refused_total() >= 1, "the ceiling refused a Cancelled");
    Flooded { s, dialog, ids, parked }
}

/// Three re-INVITEs from alice at CSeq `base + 1 ..= base + 3`, each CANCELed
/// at once, and the hop-by-hop ACK to each re-INVITE's final.
async fn burst(s: &B2buaScene, ids: &DialogIds, base: u32) {
    for cseq in base + 1..=base + 3 {
        let branch = format!("burst-{cseq}");
        let reinvite = ids.reinvite(&s.alice, cseq, &branch);
        let cancel = ids.cancel(&s.alice, cseq, &branch);
        s.alice.try_send_datagram(&reinvite, s.b2bua.addr).await.expect("the re-INVITE leaves");
        s.alice.try_send_datagram(&cancel, s.b2bua.addr).await.expect("the CANCEL leaves");
    }
    // The answers arrive before their first Timer G copy (T1).
    s.h.advance(Duration::from_millis(450)).await;
    let mut finals = Vec::new();
    while let Some(msg) = s.alice.take_queued().await {
        if let SipMessage::Response(r) = msg {
            if r.cseq().method().as_str() == "INVITE" && r.status() >= 300 {
                finals.push(r.cseq().seq());
            }
        }
    }
    for cseq in &finals {
        let ack = ids.ack_non_2xx(&s.alice, *cseq, &format!("burst-{cseq}"));
        s.alice.try_send_datagram(&ack, s.b2bua.addr).await.expect("the ACK leaves");
    }
    assert_eq!(finals.len(), 3, "every re-INVITE drew its final: {finals:?}");
}

/// Once the permit frees: the INFOs relayed and answered, the Cancelled the
/// overflow kept, then the reaper's verdict — a BYE to bob, answered.
async fn bob_hears_the_teardown(f: &mut Flooded) {
    f.parked.expect(503).await;
    // bob answers one message at a time: the call's queue is one deep.
    for _ in 0..2 {
        let mut uas = f.s.bob.receive("INFO").await;
        uas.respond(200, "OK").await;
        f.s.h.advance(Duration::from_millis(300)).await;
    }
    let mut b_bye = f.s.bob.try_receive("BYE").await.expect("bob hears the teardown");
    b_bye.respond(200, "OK").await;
    f.s.h.advance(Duration::from_millis(300)).await;
}

/// The CDR the flooded call wrote, naming its teardown.
fn assert_one_overflow_cdr(s: &B2buaScene) {
    let cdrs = s.b2bua.cdr_records();
    let flooded: Vec<_> = cdrs
        .iter()
        .filter(|c| c.events.iter().any(|e| e.reason.as_deref() == Some("dispatch-overflow")))
        .collect();
    assert_eq!(flooded.len(), 1, "the flooded call wrote one CDR naming its teardown: {cdrs:?}");
}

/// The condemned call is torn down on the wire: bob hears a BYE, alice hears
/// a BYE, both INFOs are answered, and the CDR is written.
#[tokio::test(start_paused = true)]
async fn a_call_flooded_past_its_overflow_ceiling_is_torn_down_on_the_wire() {
    let mut f = flood_past_the_ceiling("b2bua-overflow-condemned-teardown").await;
    bob_hears_the_teardown(&mut f).await;
    let s = &f.s;
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
    assert_one_overflow_cdr(s);
    s.b2bua.assert_fully_reaped();
    let Flooded { s, .. } = f;
    let _ = s.finish().await;
}

/// alice never answers the teardown's BYE and floods again when the
/// `TerminatingTimeout` and her BYE's Timer F are due: a second parked call
/// holds the permit and her burst fills the call's queue. Both events are
/// kept past the full queue, so once the permit frees the call is forced
/// terminal and writes its CDR — not left Terminating while the flood goes on.
#[tokio::test(start_paused = true)]
async fn a_condemned_call_ends_at_its_terminating_timeout_while_the_flood_goes_on() {
    let mut f = flood_past_the_ceiling("b2bua-overflow-teardown-under-flood").await;
    bob_hears_the_teardown(&mut f).await;
    let torn_down_at = tokio::time::Instant::now();

    // The permit is held again across the TerminatingTimeout and Timer F.
    let dave = f.s.h.agent("dave", "127.0.0.1:5063").await;
    let until_due = TERMINATING_TIMEOUT_MS as u64 - 3_000;
    f.s.h.advance(Duration::from_millis(until_due)).await;
    let mut parked = dave.invite(&f.s.bob).with_sdp(OFFER_SDP).through(f.s.b2bua.addr).send().await;
    f.s.h.advance(Duration::from_millis(300)).await;
    let base = f.dialog.local_cseq() + 3;
    burst(&f.s, &f.ids, base).await;
    f.s.h.advance(Duration::from_millis(4_000)).await;
    assert!(f.s.b2bua.active_calls() >= 1, "the flooded call waits on the held permit");

    parked.expect(503).await;
    let s = &f.s;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    assert!(
        torn_down_at.elapsed() < Duration::from_millis(TERMINATING_TIMEOUT_MS as u64 + 10_000),
        "the call ended within TerminatingTimeout of its teardown, give or take the held permit"
    );
    assert_one_overflow_cdr(s);
    s.b2bua.assert_fully_reaped();
    let Flooded { s, .. } = f;
    let _ = s.finish().await;
}
