//! A request whose handler ran and died before answering it: the call is
//! torn down on the wire around it (ADR-0020, a BYE to each confirmed leg),
//! and once the call is released nothing will answer the request. A non-INVITE's server transaction is forgotten at
//! the release, so the peer's retransmission (RFC 3261 §17.1.2.2) reaches the
//! router afresh and draws 481, instead of being absorbed until the peer's
//! Timer F. A re-INVITE's 100 Trying stopped the peer's retransmissions, so
//! the release answers it 481 through its transaction.
//!
//! The same holds for a request that reaches the B2BUA through a URI-rewriting
//! element, its Request-URI stating none of the parameters the B2BUA's
//! Contact carries: the B2BUA finds its call by dialog identity, and the
//! release reaches its server transaction all the same.
//!
//! ```text
//!   alice ──▶ b2bua ──▶ proxy (rewrites in-dialog Request-URIs) ──▶ bob
//! ```

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua::rules::{
    Match, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef, ServiceSeed,
    SERVICE_LAYER,
};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, B2BUA_PORT, BOB_PORT};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::common::stateful_proxy::{spawn_stateful_proxy_routing, DialogRouting, StatefulProxy};

/// The URI-rewriting element between the B2BUA and bob.
const PROXY_PORT: u16 = 5090;

fn no_init(_: &RuleCall) -> Option<ServiceSeed> {
    None
}

/// Outranks the core INFO relay and panics: the handler dies before the INFO
/// is answered or relayed.
fn panic_on_info(_: &RuleContext) -> Option<RuleHandleResult> {
    panic!("probe: handler panic on INFO");
}

/// Outranks the core re-INVITE relay and panics on an INVITE in a dialog:
/// the handler dies before any answer, its 100 Trying included.
fn panic_on_reinvite(ctx: &RuleContext) -> Option<RuleHandleResult> {
    let in_dialog = ctx.request().is_some_and(|r| r.to().tag().is_some());
    if in_dialog {
        panic!("probe: handler panic on re-INVITE");
    }
    None
}

fn panic_on_reinvite_rules() -> Vec<RuleDefinition> {
    vec![RuleDefinition::core(
        "probe-panic-on-reinvite",
        SERVICE_LAYER,
        &[],
        Match::request().method("INVITE"),
        panic_on_reinvite,
    )]
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

/// alice's INFO kills its handler; the reaper's BYEs end both dialogs and
/// the call is released with the INFO unanswered. alice's Timer E copy then
/// draws 481.
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

    let (mut info, info_req) = dialog
        .send_request(InDialogMethod::Info)
        .try_send_with_request()
        .await
        .expect("the INFO leaves");

    let mut a_bye = s.alice.try_receive("BYE").await.expect("the reaper sends alice a BYE");
    let mut b_bye = s.bob.try_receive("BYE").await.expect("the reaper sends bob a BYE");
    a_bye.respond(200, "OK").await;
    b_bye.respond(200, "OK").await;
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_eq!(s.b2bua.metrics().handler_panics_total(), 1);
    assert_eq!(s.b2bua.txn_metrics().released_unanswered_forgotten(), 1);

    // alice's Timer E copy of the INFO.
    s.alice.try_send_datagram(info_req.image(), s.b2bua.addr).await.expect("the INFO leaves");
    s.h.advance(Duration::from_millis(300)).await;
    info.try_expect(481)
        .await
        .expect("the retransmitted INFO reaches the orphan path, not a transaction absorbing it");
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// alice's re-INVITE kills its handler before any answer; the reaper's BYEs
/// end both dialogs and the call is released with the re-INVITE unanswered. The release answers it 481
/// through its server transaction: alice's ACK is absorbed there and the
/// 481's Timer G copies stop.
#[tokio::test(start_paused = true)]
async fn a_reinvite_whose_handler_died_is_answered_481_when_the_call_is_released() {
    let s = B2buaScene::with_b2bua("b2bua-reinvite-unanswered-at-release", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).services(vec![ServiceDef {
            id: "probe-panic-reinvite",
            init: no_init,
            rules: panic_on_reinvite_rules,
        }])
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut reinvite = dialog.reinvite(Some(OFFER_SDP)).await;
    let mut a_bye = s.alice.try_receive("BYE").await.expect("the reaper sends alice a BYE");
    let mut b_bye = s.bob.try_receive("BYE").await.expect("the reaper sends bob a BYE");
    a_bye.respond(200, "OK").await;
    b_bye.respond(200, "OK").await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    assert_eq!(s.b2bua.metrics().handler_panics_total(), 1);
    reinvite
        .try_expect(481)
        .await
        .expect("the released call answers the re-INVITE its handler never answered");
    assert_eq!(s.b2bua.txn_metrics().released_unanswered_invites_answered(), 1);

    // The ACK ends the 481's Timer G ladder: no copy follows.
    s.h.advance(Duration::from_secs(5)).await;
    let mut repeats = 0;
    while let Some(msg) = s.alice.take_queued().await {
        if let SipMessage::Response(r) = msg {
            if r.cseq().method().as_str() == "INVITE" && r.status() >= 200 {
                repeats += 1;
            }
        }
    }
    assert_eq!(repeats, 0, "alice's ACK is absorbed by the 481's transaction");
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// A B2BUA whose `service` rules panic, routing every call to bob through a
/// URI-rewriting proxy that Record-Routes the dialog: bob's in-dialog
/// requests reach the B2BUA with the proxy's bare address as Request-URI.
async fn rewritten_scene(
    name: &str,
    service: &'static str,
    rules: fn() -> Vec<RuleDefinition>,
) -> (B2buaScene, StatefulProxy) {
    let s = B2buaScene::with_b2bua(name, |_| {
        let engine = ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", PROXY_PORT);
                r.new_ruri = Some(format!("sip:bob@127.0.0.1:{BOB_PORT}"));
                NewCallResponse::Route(r)
            })
            .build();
        B2buaSut::builder(Arc::new(engine)).services(vec![ServiceDef {
            id: service,
            init: no_init,
            rules,
        }])
    })
    .await;
    let proxy = spawn_stateful_proxy_routing(
        &s.h,
        "proxy",
        &format!("127.0.0.1:{PROXY_PORT}"),
        DialogRouting::RewriteRequestUri,
    )
    .await;
    (s, proxy)
}

/// Every in-dialog `method` the proxy forwarded to the B2BUA states no
/// `callRef` in its Request-URI, and there is at least one.
fn assert_reached_without_call_ref(h: &Harness, method: &str) {
    let proxy = format!("127.0.0.1:{PROXY_PORT}").parse().unwrap();
    let b2bua = format!("127.0.0.1:{B2BUA_PORT}").parse().unwrap();
    let forwarded: Vec<_> = h
        .wire_entries()
        .iter()
        .filter(|e| e.from == proxy && e.to == b2bua)
        .filter_map(|e| match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Request(r)) if r.method() == method && r.to().tag().is_some() => Some(r),
            _ => None,
        })
        .collect();
    assert!(!forwarded.is_empty(), "the proxy forwarded bob's {method} to the B2BUA");
    for r in forwarded {
        assert!(
            r.request_uri().param("callRef").is_none(),
            "the rewriter delivers no callRef: {}",
            r.request_uri().text()
        );
    }
}

/// bob's re-INVITE reaches the B2BUA with no `callRef` and kills its handler
/// before any answer; the reaper's BYEs end both dialogs and the call is
/// released with the re-INVITE unanswered. The release answers it 481
/// through its server transaction, which the router found by the dialog
/// identity (RFC 3261 §12.2.2), and the proxy relays that final to bob.
#[tokio::test(start_paused = true)]
async fn a_reinvite_reaching_the_b2bua_without_its_uri_params_is_answered_481_at_release() {
    let (s, _proxy) = rewritten_scene(
        "b2bua-rewritten-reinvite-unanswered-at-release",
        "probe-panic-reinvite",
        panic_on_reinvite_rules,
    )
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut reinvite = bob_dialog.reinvite(Some(ANSWER_SDP)).await;
    reinvite.expect(100).await;
    let mut a_bye = s.alice.try_receive("BYE").await.expect("the reaper sends alice a BYE");
    let mut b_bye = s.bob.try_receive("BYE").await.expect("the reaper sends bob a BYE");
    a_bye.respond(200, "OK").await;
    b_bye.respond(200, "OK").await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    assert_eq!(s.b2bua.metrics().handler_panics_total(), 1);
    assert_reached_without_call_ref(&s.h, "INVITE");
    reinvite
        .try_expect(481)
        .await
        .expect("the released call answers the re-INVITE its handler never answered");
    assert_eq!(s.b2bua.txn_metrics().released_unanswered_invites_answered(), 1);
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// bob's INFO reaches the B2BUA with no `callRef` and kills its handler; the
/// reaper's BYEs end both dialogs and the call is released with the INFO
/// unanswered. The release forgets its server transaction, so the proxy's
/// Timer E copy of the INFO (RFC 3261 §17.1.2.2) meets the orphan path
/// instead of a transaction absorbing it (§17.2.2): it draws 481, which the
/// proxy relays to bob.
#[tokio::test(start_paused = true)]
async fn an_info_reaching_the_b2bua_without_its_uri_params_is_forgotten_at_release() {
    let (s, _proxy) = rewritten_scene(
        "b2bua-rewritten-info-unanswered-at-release",
        "probe-panic-info",
        panic_on_info_rules,
    )
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut info = bob_dialog.send_request(InDialogMethod::Info).send().await;
    let mut a_bye = s.alice.try_receive("BYE").await.expect("the reaper sends alice a BYE");
    let mut b_bye = s.bob.try_receive("BYE").await.expect("the reaper sends bob a BYE");
    a_bye.respond(200, "OK").await;
    b_bye.respond(200, "OK").await;
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_eq!(s.b2bua.metrics().handler_panics_total(), 1);
    assert_reached_without_call_ref(&s.h, "INFO");
    assert_eq!(
        s.b2bua.txn_metrics().released_unanswered_forgotten(),
        1,
        "the release forgets the INFO's server transaction"
    );
    info.try_expect(481).await.expect(
        "the proxy's copy of the INFO reaches the orphan path, not a transaction absorbing it",
    );
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
