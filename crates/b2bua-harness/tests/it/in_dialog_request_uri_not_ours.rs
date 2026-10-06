//! In-dialog requests that reach the B2BUA through a third-party proxy which
//! does not hand back the Request-URI the B2BUA's Contact stated:
//!
//! - a strict router (RFC 2543; RFC 3261 §12.2.1.1, §16.4): its Record-Route
//!   carries no `;lr`, so each peer addresses it in the Request-URI and carries
//!   the remote target in the last Route; it moves that Route back into the
//!   Request-URI before forwarding;
//! - a URI-rewriting element: each in-dialog request it forwards is aimed at
//!   the bare address of its next hop.
//!
//! The B2BUA finds the call and the leg a request arrived on by its dialog
//! identity (Call-ID and the sender's tag) wherever the Request-URI states
//! neither. Two shapes of call:
//!
//! ```text
//!   direct:  alice ──▶ proxy ──▶ b2bua ──▶ proxy ──▶ bob
//!   spiral:  alice ──▶ b2bua (call 1) ──▶ proxy ──▶ b2bua (call 2) ──▶ bob
//! ```
//!
//! In each, a re-INVITE and a BYE come from the caller and from the callee,
//! and reach the party at the other end with no 481 on the way. Each call
//! records its answer and the BYE on the leg it arrived on: the incoming leg
//! for the caller's, the outgoing leg for the callee's. A callee BYE naming
//! the incoming leg's tag is still refused 481 (§12.2.2): the index names the
//! leg, the To-tag must belong to it.
//!
//! A route set mixing a loose and a strict router (§12.1.1 order on the
//! B2BUA's incoming leg) puts the strict hop in the Request-URI and the loose
//! one, then the target, in Route:
//!
//! ```text
//!   alice ──▶ p1 (loose) ──▶ p2 (strict) ──▶ b2bua ──▶ bob
//! ```

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, ALICE_PORT, B2BUA_PORT, BOB_PORT};
use call::CdrEventType;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, Harness};
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

use crate::common::stateful_proxy::{spawn_stateful_proxy_routing, DialogRouting, StatefulProxy};

const PROXY_PORT: u16 = 5090;
/// The strict router of the mixed route set; `PROXY_PORT` is its loose one.
const STRICT_PORT: u16 = 5095;
/// The Request-URI call 1 sends to the proxy in a spiral; it comes back to the
/// B2BUA as call 2.
const SPIRAL_USER: &str = "spiral";

const ALICE_REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const BOB_REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20004 RTP/AVP 0\r\n";
const ALICE_REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 0\r\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// The proxy sits in front of each leg of one call.
    Direct,
    /// The proxy sits between two calls of the same B2BUA.
    Spiral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Caller,
    Callee,
}

/// Direct: the call is routed to bob through the proxy. Spiral: call 1 is
/// routed through the proxy back to the B2BUA, call 2 to bob.
fn decision(shape: Shape) -> Arc<dyn CallDecisionEngine> {
    let through_proxy = move |ruri: String| {
        let mut r = route_to("127.0.0.1", PROXY_PORT);
        r.new_ruri = Some(ruri);
        NewCallResponse::Route(r)
    };
    match shape {
        Shape::Direct => Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_| through_proxy(format!("sip:bob@127.0.0.1:{BOB_PORT}")))
                .build(),
        ),
        Shape::Spiral => Arc::new(
            ScriptedDecisionEngine::builder()
                .on(move |req| {
                    req.ruri.starts_with(&format!("sip:{SPIRAL_USER}@")).then(|| {
                        let mut r = route_to("127.0.0.1", BOB_PORT);
                        r.new_ruri = Some(format!("sip:bob@127.0.0.1:{BOB_PORT}"));
                        NewCallResponse::Route(r)
                    })
                })
                .fallback(move |_| {
                    through_proxy(format!("sip:{SPIRAL_USER}@127.0.0.1:{B2BUA_PORT}"))
                })
                .build(),
        ),
    }
}

async fn scene(name: &str, routing: DialogRouting, shape: Shape) -> (B2buaScene, StatefulProxy) {
    let s = B2buaScene::with_b2bua(name, |_| B2buaSut::builder(decision(shape))).await;
    let proxy =
        spawn_stateful_proxy_routing(&s.h, "proxy", &format!("127.0.0.1:{PROXY_PORT}"), routing)
            .await;
    (s, proxy)
}

fn addr(port: u16) -> std::net::SocketAddr {
    format!("127.0.0.1:{port}").parse().unwrap()
}

/// The in-dialog requests (`method`, or any when `None`) that crossed
/// `from → to`.
fn in_dialog(h: &Harness, from: u16, to: u16, method: Option<&str>) -> Vec<SipRequest> {
    let (from, to) = (addr(from), addr(to));
    h.wire_entries()
        .iter()
        .filter(|e| e.from == from && e.to == to)
        .filter_map(|e| match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Request(r)) => Some(r),
            _ => None,
        })
        .filter(|r| r.to().tag().is_some() && method.is_none_or(|m| r.method() == m))
        .collect()
}

/// What the B2BUA got from the proxy: its Request-URI states none of the
/// parameters the B2BUA's Contact carries.
fn assert_not_ours(h: &Harness, method: &str) {
    let seen = in_dialog(h, PROXY_PORT, B2BUA_PORT, Some(method));
    assert!(!seen.is_empty(), "the proxy forwarded an in-dialog {method} to the B2BUA");
    for r in seen {
        assert!(
            r.request_uri().param("callRef").is_none(),
            "this shape delivers no callRef: {}",
            r.request_uri().text()
        );
    }
}

/// A dialog the proxy Record-Routed is followed through the proxy: the B2BUA
/// sends none of its in-dialog requests past it (RFC 3261 §12.2.1.1, §8.1.2).
fn assert_through_the_proxy(h: &Harness, shape: Shape) {
    let past: &[u16] = match shape {
        Shape::Direct => &[ALICE_PORT, BOB_PORT],
        Shape::Spiral => &[B2BUA_PORT],
    };
    for &to in past {
        let bypassed = in_dialog(h, B2BUA_PORT, to, None);
        assert!(
            bypassed.is_empty(),
            "in-dialog requests sent to {to} past the proxy: {:?}",
            bypassed.iter().map(|r| r.request_uri().text().into_owned()).collect::<Vec<_>>()
        );
    }
}

/// No request on the wire drew a 481.
fn assert_no_481(h: &Harness) {
    let refused = h
        .wire_entries()
        .iter()
        .filter(|e| {
            matches!(CustomParser::new().parse(&e.raw),
                Ok(SipMessage::Response(r)) if r.status() == 481)
        })
        .count();
    assert_eq!(refused, 0, "no in-dialog request is refused as naming no dialog");
}

/// Each call answered, and the BYE recorded on the leg it came from.
fn assert_recorded(s: &B2buaScene, shape: Shape, side: Side) {
    let cdrs = s.b2bua.cdr_records();
    let calls = if shape == Shape::Spiral { 2 } else { 1 };
    assert_eq!(cdrs.len(), calls, "one CDR per call: {cdrs:?}");
    for cdr in &cdrs {
        assert!(cdr.events.iter().any(|e| e.event_type == CdrEventType::Answer), "{cdr:?}");
        let bye = cdr
            .events
            .iter()
            .find(|e| e.event_type == CdrEventType::Bye)
            .unwrap_or_else(|| panic!("the call records the BYE: {cdr:?}"));
        match side {
            Side::Caller => assert_eq!(bye.leg_id, "a", "the caller's BYE is the incoming leg's"),
            Side::Callee => assert!(
                cdr.b_legs.iter().any(|b| b.leg_id == bye.leg_id),
                "the callee's BYE is the outgoing leg's, not {:?}: {cdr:?}",
                bye.leg_id
            ),
        }
    }
}

async fn run(name: &str, routing: DialogRouting, shape: Shape, side: Side, reinvite: bool) {
    let (s, proxy) = scene(name, routing, shape).await;
    let invite = s.alice.invite(&s.bob).with_sdp(OFFER_SDP);
    let mut call = match shape {
        Shape::Direct => {
            invite.ruri(format!("sip:bob@127.0.0.1:{B2BUA_PORT}")).through(proxy.addr).send().await
        }
        Shape::Spiral => invite.through(s.b2bua.addr).send().await,
    };
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let bob_dialog = uas.dialog();

    let (mut from, to): (_, &Agent) = match side {
        Side::Caller => (alice_dialog, &s.bob),
        Side::Callee => (bob_dialog, &s.alice),
    };
    if reinvite {
        let (offer, answer) = match side {
            Side::Caller => (ALICE_REOFFER, BOB_REANSWER),
            Side::Callee => (BOB_REOFFER, ALICE_REANSWER),
        };
        let mut reinv = from.request(InDialogMethod::Invite, Some(offer)).await;
        let mut far = to.receive("INVITE").await;
        assert!(!far.request().body().is_empty(), "the re-offer reaches the far end");
        far.respond(200, "OK").with_sdp(answer).await;
        let ok = reinv.expect(200).await;
        assert!(!ok.body().is_empty(), "the far end's answer comes back");
        from.ack(None).await;
        to.receive("ACK").await;
    }
    let mut bye = from.bye().await;
    to.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let calls = if shape == Shape::Spiral { 2 } else { 1 };
    settle_until(|| s.b2bua.cdr_records().len() == calls && s.b2bua.is_reaped()).await;
    assert_recorded(&s, shape, side);
    assert_through_the_proxy(&s.h, shape);
    assert_no_481(&s.h);
    if routing == DialogRouting::RewriteRequestUri {
        assert_not_ours(&s.h, "BYE");
    }
    let _ = s.finish().await;
}

macro_rules! scenarios {
    ($($test:ident: $routing:ident, $shape:ident, $side:ident, $reinvite:expr;)*) => {$(
        #[tokio::test(start_paused = true)]
        async fn $test() {
            run(
                stringify!($test),
                DialogRouting::$routing,
                Shape::$shape,
                Side::$side,
                $reinvite,
            )
            .await;
        }
    )*};
}

scenarios! {
    strict_direct_caller_bye: Strict, Direct, Caller, false;
    strict_direct_callee_bye: Strict, Direct, Callee, false;
    strict_direct_caller_reinvite: Strict, Direct, Caller, true;
    strict_direct_callee_reinvite: Strict, Direct, Callee, true;
    strict_spiral_caller_bye: Strict, Spiral, Caller, false;
    strict_spiral_callee_bye: Strict, Spiral, Callee, false;
    strict_spiral_caller_reinvite: Strict, Spiral, Caller, true;
    strict_spiral_callee_reinvite: Strict, Spiral, Callee, true;
    rewrite_direct_caller_bye: RewriteRequestUri, Direct, Caller, false;
    rewrite_direct_callee_bye: RewriteRequestUri, Direct, Callee, false;
    rewrite_direct_caller_reinvite: RewriteRequestUri, Direct, Caller, true;
    rewrite_direct_callee_reinvite: RewriteRequestUri, Direct, Callee, true;
    rewrite_spiral_caller_bye: RewriteRequestUri, Spiral, Caller, false;
    rewrite_spiral_callee_bye: RewriteRequestUri, Spiral, Callee, false;
    rewrite_spiral_caller_reinvite: RewriteRequestUri, Spiral, Caller, true;
    rewrite_spiral_callee_reinvite: RewriteRequestUri, Spiral, Callee, true;
}

/// The callee's BYE comes through the rewriter under the To-tag the B2BUA
/// gave the caller: the index finds the call by bob's tag on the outgoing
/// leg, and that leg holds no such tag, so the BYE is refused 481 and the
/// call stays up until bob's own dialog ends it.
#[tokio::test(start_paused = true)]
async fn a_callee_bye_under_the_incoming_legs_tag_is_refused_481() {
    let (s, proxy) =
        scene("rewrite_callee_bye_incoming_tag", DialogRouting::RewriteRequestUri, Shape::Direct)
            .await;
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER_SDP)
        .ruri(format!("sip:bob@127.0.0.1:{B2BUA_PORT}"))
        .through(proxy.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    let answer = call.expect(200).await;
    let incoming_tag = answer.to().tag().expect("the 2xx carries the B2BUA's tag").to_string();
    let _alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    s.h.allow_violation(
        "mid-dialog-tags",
        "the BYE under the incoming leg's To-tag is the deviation under test (RFC 3261 §12.2.2)",
    );
    let cseq_before = bob_dialog.local_cseq();
    let mut foreign =
        bob_dialog.send_request(InDialogMethod::Bye).with_to_tag(&incoming_tag).send().await;
    bob_dialog.set_local_cseq(cseq_before);
    foreign.expect(481).await;
    s.h.advance(std::time::Duration::from_millis(500)).await;
    assert!(
        s.alice.try_receive_tolerating("BYE", &[]).await.is_none(),
        "a BYE naming no dialog of the outgoing leg must not reach the caller"
    );
    assert_eq!(s.b2bua.metrics().removals_total(), 0, "the call was not torn down");

    let mut bye = bob_dialog.bye().await;
    s.alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1 && s.b2bua.is_reaped()).await;
    assert_recorded(&s, Shape::Direct, Side::Callee);
    let _ = s.finish().await;
}

/// alice reaches the B2BUA through a loose then a strict router; a preloaded
/// Route takes the INVITE through both (§8.1.2).
async fn mixed_route_scene(name: &str) -> (B2buaScene, StatefulProxy, StatefulProxy, Dialogs) {
    let s = B2buaScene::new(name).await;
    let loose = spawn_stateful_proxy_routing(
        &s.h,
        "p1",
        &format!("127.0.0.1:{PROXY_PORT}"),
        DialogRouting::Loose,
    )
    .await;
    let strict = spawn_stateful_proxy_routing(
        &s.h,
        "p2",
        &format!("127.0.0.1:{STRICT_PORT}"),
        DialogRouting::Strict,
    )
    .await;
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER_SDP)
        .ruri(format!("sip:bob@127.0.0.1:{B2BUA_PORT}"))
        .with_header(
            "Route",
            &format!("<sip:127.0.0.1:{PROXY_PORT};lr>, <sip:127.0.0.1:{STRICT_PORT};lr>"),
        )
        .through(loose.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let alice = call.ack().await;
    s.bob.receive("ACK").await;
    let bob = uas.dialog();
    (s, loose, strict, Dialogs { alice, bob })
}

struct Dialogs {
    alice: scenario_harness::Dialog,
    bob: scenario_harness::Dialog,
}

/// The B2BUA's in-dialog request toward alice leaves for the strict router,
/// which it names in the Request-URI, with the loose router then alice's
/// Contact in Route (§12.1.1 order, §12.2.1.1).
#[tokio::test(start_paused = true)]
async fn a_mixed_route_set_puts_the_strict_hop_in_the_request_uri() {
    let (s, _loose, _strict, mut dialogs) = mixed_route_scene("mixed_route_callee_bye").await;
    let mut bye = dialogs.bob.bye().await;
    s.alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let sent = in_dialog(&s.h, B2BUA_PORT, STRICT_PORT, Some("BYE"));
    assert_eq!(sent.len(), 1, "the B2BUA's BYE leaves for the strict router");
    let bye_out = &sent[0];
    assert_eq!(bye_out.request_uri().text(), format!("sip:127.0.0.1:{STRICT_PORT}"));
    let routes: Vec<String> = bye_out
        .route_set()
        .expect("readable Route")
        .iter()
        .map(|r| r.uri().text().into_owned())
        .collect();
    assert_eq!(routes.len(), 2, "{routes:?}");
    assert_eq!(routes[0], format!("sip:127.0.0.1:{PROXY_PORT};lr"), "the loose router next");
    assert_eq!(addr_of(&routes[1]), ALICE_PORT, "alice's Contact last: {routes:?}");

    settle_until(|| s.b2bua.cdr_records().len() == 1 && s.b2bua.is_reaped()).await;
    assert_recorded(&s, Shape::Direct, Side::Callee);
    assert_no_481(&s.h);
    let _ = s.finish().await;
}

/// alice's own requests cross both routers the other way: the loose router
/// lifts the strict hop into the Request-URI (§16.6 step 6) and the strict
/// router restores the B2BUA's Contact (§16.4).
#[tokio::test(start_paused = true)]
async fn a_mixed_route_set_carries_the_callers_bye() {
    let (s, _loose, _strict, mut dialogs) = mixed_route_scene("mixed_route_caller_bye").await;
    let mut bye = dialogs.alice.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let arrived = in_dialog(&s.h, STRICT_PORT, B2BUA_PORT, Some("BYE"));
    assert_eq!(arrived.len(), 1, "the BYE reaches the B2BUA from the strict router");
    assert!(arrived[0].request_uri().param("callRef").is_some(), "its Contact is restored");

    settle_until(|| s.b2bua.cdr_records().len() == 1 && s.b2bua.is_reaped()).await;
    assert_recorded(&s, Shape::Direct, Side::Caller);
    assert_no_481(&s.h);
    let _ = s.finish().await;
}

/// The port a route URI names.
fn addr_of(uri: &str) -> u16 {
    uri.rsplit(':').next().and_then(|p| p.split(';').next()?.parse().ok()).expect("a port")
}
