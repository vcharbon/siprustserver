//! A non-INVITE request whose handler ran and died before answering it: the
//! call is torn down around it (ADR-0020), and once the call is released
//! nothing will answer the request. Its server transaction is forgotten at
//! the release, so the peer's retransmission (RFC 3261 §17.1.2.2) reaches the
//! router afresh and draws 481, instead of being absorbed until the peer's
//! Timer F.

use std::time::Duration;

use b2bua::rules::{
    Match, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef, ServiceSeed,
    SERVICE_LAYER,
};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use sip_message::generators::InDialogMethod;

fn no_init(_: &RuleCall) -> Option<ServiceSeed> {
    None
}

/// Outranks the core INFO relay and panics: the handler dies before the INFO
/// is answered or relayed.
fn panic_on_info(_: &RuleContext) -> Option<RuleHandleResult> {
    panic!("probe: handler panic on INFO");
}

fn panic_on_info_rules() -> Vec<RuleDefinition> {
    vec![RuleDefinition::core(
        "probe-panic-on-info",
        SERVICE_LAYER,
        &[],
        Match::request().method("INFO"),
        panic_on_info,
    )]
}

/// alice's INFO kills its handler; the reaper tears the call down and it is
/// released with the INFO unanswered. alice's Timer E copy draws 481; both
/// sides then close the dialog the B2BUA no longer holds.
#[tokio::test(start_paused = true)]
async fn an_info_whose_handler_died_draws_481_on_its_retransmission_once_the_call_is_released() {
    let s = B2buaScene::with_b2bua("b2bua-info-unanswered-at-release", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).services(vec![ServiceDef {
            id: "probe-panic-info",
            init: no_init,
            rules: panic_on_info_rules,
        }])
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut b_dialog = uas.dialog();

    let (mut info, info_req) = dialog
        .send_request(InDialogMethod::Info)
        .try_send_with_request()
        .await
        .expect("the INFO leaves");

    // The reaper's fatal-error verdict forces the call terminal; it sends
    // no BYE (ADR-0020).
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    assert_eq!(s.b2bua.metrics().handler_panics_total(), 1);
    assert_eq!(s.b2bua.txn_metrics().released_unanswered_forgotten(), 1);

    // alice's Timer E copy of the INFO.
    s.alice.try_send_datagram(info_req.image(), s.b2bua.addr).await.expect("the INFO leaves");
    s.h.advance(Duration::from_millis(300)).await;
    info.try_expect(481)
        .await
        .expect("the retransmitted INFO reaches the orphan path, not a transaction absorbing it");

    // Both sides close their dialogs; the B2BUA no longer holds them.
    let mut a_bye = dialog.bye().await;
    a_bye.expect(481).await;
    let mut b_bye = b_dialog.bye().await;
    b_bye.expect(481).await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
