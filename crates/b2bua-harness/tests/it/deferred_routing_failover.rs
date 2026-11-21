//! Deferred routing (ADR-0016) across a limiter failover: whether a route's
//! dial is deferred to a service is the route's own statement. A deferring
//! route the limiter refuses leaves the failover route that does not defer
//! to dial its own destination, with no media leg; a route that does not defer,
//! refused, leaves a deferring failover route to its service.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallTreatment, NewCallResponse, RouteDecision, ScriptedDecisionEngine};
use b2bua::limiter::LimiterEntry;
use b2bua_harness::limiter::doubles::{fail_open, refuse_on};
use call::DecisionKind;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

use crate::decision_log::{marks, Sut, ALICE, BOB, CAROL, OFFER};

const MRF_SDP: &str = "v=0\r\no=mrf 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const DEST_SDP: &str = "v=0\r\no=dest 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const MRF: &str = "127.0.0.1:5670";
const MRF_PORT: u16 = 5670;
const CAROL_PORT: u16 = 5070;
const BOB_PORT: u16 = 5071;

/// A route to `port` deferring its dial to the announcement service, which
/// plays from the media server and then dials `port`.
fn deferring_route(port: u16) -> RouteDecision {
    let mut r = route_to("127.0.0.1", port);
    r.service_ext.insert(
        announcement::EXT_KEY.into(),
        serde_json::json!({
            "clip_id": "clip-1",
            "mrf_host": "127.0.0.1",
            "mrf_port": MRF_PORT,
            "dest_host": "127.0.0.1",
            "dest_port": port,
            "defer_routing": true,
        }),
    );
    r
}

/// `route` capped by the limiter entry the scene refuses.
fn refused(mut route: RouteDecision) -> NewCallResponse {
    route.callback_context = Some("ctx".into());
    route.call_limiter = vec![LimiterEntry { id: "cap".into(), limit: 1 }];
    NewCallResponse::Route(route)
}

/// The SUT whose first route, `first`, the limiter refuses and whose failover
/// consult answers `failover`, labelled "failover"; the announcement service
/// registered.
async fn spawn(h: &Harness, first: fn() -> RouteDecision, failover: fn() -> RouteDecision) -> Sut {
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| refused(first()))
            .on_failure(move |req| {
                assert_eq!(req.failure.origin, "call_limiter");
                let mut r = failover();
                r.label = Some("failover".into());
                CallTreatment::Route(r)
            })
            .build(),
    );
    Sut::spawn_with(h, decision, Some(refuse_on("cap", fail_open())), vec![announcement::service()])
        .await
}

/// The refused route's slice stays on the call; the failover route, stating
/// none, dials its destination and the service stays dormant.
#[tokio::test(start_paused = true)]
async fn a_refused_deferring_route_leaves_the_failover_route_to_dial_its_destination() {
    let h = Harness::new("deferred-routing-failover-direct");
    let alice = h.agent("alice", ALICE).await;
    let mrf = h.agent("mrf", MRF).await;
    let carol = h.agent("carol", CAROL).await;
    let bob = h.agent("bob", BOB).await;
    let sut = spawn(&h, || deferring_route(CAROL_PORT), || route_to("127.0.0.1", BOB_PORT)).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(uas.request().body(), OFFER.as_bytes(), "the destination gets the caller's offer");
    uas.respond(200, "OK").with_sdp(DEST_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    assert_eq!(mrf.drain().await, 0, "no media leg: the failover route does not defer");
    assert_eq!(carol.drain().await, 0, "the refused route's destination is never dialled");
    let done = sut.assert_reaped().await;
    assert_eq!(marks(&done), vec![(DecisionKind::FailoverRoute, None, Some("failover"))]);
    assert_eq!(done.b_legs.len(), 1, "one leg, the failover route's destination");

    let _report = h.finish().await;
}

/// The failover route's slice defers its dial: the service plays from the
/// media server, then dials the failover route's destination.
#[tokio::test(start_paused = true)]
async fn a_refused_route_leaves_a_deferring_failover_route_to_its_service() {
    let h = Harness::new("deferred-routing-failover-deferred");
    let alice = h.agent("alice", ALICE).await;
    let mrf = h.agent("mrf", MRF).await;
    let carol = h.agent("carol", CAROL).await;
    let bob = h.agent("bob", BOB).await;
    let sut = spawn(&h, || route_to("127.0.0.1", CAROL_PORT), || deferring_route(BOB_PORT)).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut mrf_uas = mrf.receive("INVITE").await;
    assert_eq!(mrf_uas.request().body(), OFFER.as_bytes(), "the media server gets the offer");
    mrf_uas.respond(200, "OK").with_sdp(MRF_SDP).await;
    mrf.receive("ACK").await;
    let mut mrf_dialog = mrf_uas.dialog();
    call.expect(183).await;
    mrf.receive("INFO").await.respond(200, "OK").await;
    let mut played = mrf_dialog
        .send_request(InDialogMethod::Info)
        .with_body(announcement::mscml::CONTENT_TYPE, announcement::mscml::build_response(200))
        .send()
        .await;
    played.expect(200).await;
    mrf.receive("BYE").await.respond(200, "OK").await;

    let mut uas = bob.receive("INVITE").await;
    assert_eq!(uas.request().body(), OFFER.as_bytes(), "the destination gets the caller's offer");
    uas.respond(200, "OK").with_sdp(DEST_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    assert_eq!(carol.drain().await, 0, "the refused route's destination is never dialled");
    let done = sut.assert_reaped().await;
    assert_eq!(marks(&done), vec![(DecisionKind::FailoverRoute, None, Some("failover"))]);

    let _report = h.finish().await;
}
