//! upstreamneed-055 — a routing decision naming an address no reader accepts is
//! answered as a call outcome, never by inventing a destination.
//!
//! The failure mode these pin out: the B2BUA used to answer a malformed decision
//! field by building an *opaque* URI whose host was the whole raw text, and then
//! originating a b-leg toward it. The call died later as an unattributable
//! resolution/transport error — or, if the fabricated host happened to resolve,
//! reached a destination nobody named. Every scenario here asserts BOTH halves:
//! the caller gets a final (ADR-0022's guarantee), and the callee is never dialled.

use std::sync::Arc;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// An address RFC 3261 §19.1.1 refuses: an IPv6 literal written without its
/// brackets. `Uri::opaque` would have made its "host" the whole string, so a
/// router that resolved it would have looked up `2001` — the concrete way a
/// fabricated default reaches a destination the decision never stated.
const UNREADABLE: &str = "sip:2001:db8::1";

fn plan_engine() -> Arc<ScriptedDecisionEngine> {
    Arc::new(ScriptedDecisionEngine::numbering_plan())
}

/// The b-leg destination every scenario would have dialled had the decision been
/// readable — so "bob received nothing" is a real negative, not a mis-addressing.
async fn assert_bob_was_never_dialled(bob: &scenario_harness::Agent) {
    assert!(
        bob.try_receive("INVITE").await.is_err(),
        "the callee must never be dialled on a refused routing decision"
    );
}

// An unreadable `new_ruri` refuses the route: alice gets a 500 naming the
// offending field, and no b-leg INVITE is ever originated.
#[tokio::test]
async fn unreadable_new_ruri_answers_500_and_dials_nobody() {
    let h = Harness::with_transit_delay("055-ruri", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(plan_engine()).start(&h, "b2bua", "127.0.0.1:5080").await;

    let plan = serde_json::json!({
        "action": "route",
        "destination": {"host": "127.0.0.1", "port": 5070},
        "new_ruri": UNREADABLE,
    })
    .to_string();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(b2bua.addr)
        .send()
        .await;

    let resp = call.expect(500).await;
    assert_eq!(
        resp.reason(),
        "Unreadable Routing Address (new_ruri)",
        "the refusal names the field, so the defect is in the trace"
    );
    assert_bob_was_never_dialled(&bob).await;

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert!(cdrs[0].b_legs.is_empty(), "no b-leg is created for a refused route");
    b2bua.assert_fully_reaped();
    let _r = h.finish().await;
}

// Same refusal for the identity rewrites: a From/To URI the B2BUA cannot read is
// not silently projected onto a host with an empty user.
#[tokio::test]
async fn unreadable_identity_rewrites_answer_500_and_dial_nobody() {
    for (field, key) in [("new_from", "new_from"), ("new_to", "new_to")] {
        let h = Harness::with_transit_delay(format!("055-{field}"), 1);
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let b2bua = B2buaSut::builder(plan_engine()).start(&h, "b2bua", "127.0.0.1:5080").await;

        let plan = serde_json::json!({
            "action": "route",
            "destination": {"host": "127.0.0.1", "port": 5070},
            key: UNREADABLE,
        })
        .to_string();

        let mut call = alice
            .invite(&bob)
            .with_sdp(OFFER)
            .with_header("X-Api-Call", &plan)
            .through(b2bua.addr)
            .send()
            .await;

        let resp = call.expect(500).await;
        assert_eq!(resp.reason(), format!("Unreadable Routing Address ({field})"));
        assert_bob_was_never_dialled(&bob).await;

        settle_until(|| !b2bua.cdr_records().is_empty()).await;
        b2bua.assert_fully_reaped();
        let _r = h.finish().await;
    }
}

// A destination port the plan STATES but that is no port refuses the route at
// the decision layer. The bug this replaces: `unwrap_or(5060)` collapsed it onto
// the DEFAULT port of the same host — here `127.0.0.1:5060`, which in this
// scenario is alice's own socket. A stated-but-unreadable port must never become
// "whatever is listening on 5060".
#[tokio::test]
async fn out_of_range_destination_port_refuses_the_route() {
    let h = Harness::with_transit_delay("055-port", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(plan_engine()).start(&h, "b2bua", "127.0.0.1:5080").await;

    let plan = serde_json::json!({
        "action": "route",
        "destination": {"host": "127.0.0.1", "port": 88161},
    })
    .to_string();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(b2bua.addr)
        .send()
        .await;

    // The plan yields no route at all, so the adapter's own no-route treatment
    // answers — not a route to :5060.
    call.expect(404).await;
    assert_bob_was_never_dialled(&bob).await;

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    b2bua.assert_fully_reaped();
    let _r = h.finish().await;
}

// A 3xx is a routing instruction the CALLER executes, so an unreadable redirect
// target is refused whole rather than emitted as an opaque Contact. The caller
// must not be handed an address to dial that the decision never stated.
#[tokio::test]
async fn unreadable_redirect_target_answers_500_without_a_contact() {
    let h = Harness::with_transit_delay("055-redirect", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(plan_engine()).start(&h, "b2bua", "127.0.0.1:5080").await;

    let plan = serde_json::json!({
        "action": "redirect",
        "code": 302,
        "contacts": [
            {"uri": "sip:primary@alt1.example", "q": 1.0},
            {"uri": UNREADABLE, "q": 0.5}
        ]
    })
    .to_string();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(b2bua.addr)
        .send()
        .await;

    let resp = call.expect(500).await;
    assert_eq!(resp.reason(), "Unreadable Routing Address (contact)");
    // The whole redirect is refused, never partially authored: handing the caller
    // only the targets that happened to parse would silently re-route the call.
    assert_eq!(
        resp.raw(HeaderName::Contact).next(),
        None,
        "a refused redirect carries no Contact at all"
    );
    assert_bob_was_never_dialled(&bob).await;

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    b2bua.assert_fully_reaped();
    let _r = h.finish().await;
}

// The refusal is exact, not a blanket one: the same plan shape with readable
// addresses still routes, rewrites and answers. Without this pin, "refuse
// everything" would pass every assertion above.
#[tokio::test]
async fn readable_addresses_still_route_and_answer() {
    let h = Harness::with_transit_delay("055-control", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(plan_engine()).start(&h, "b2bua", "127.0.0.1:5080").await;

    let plan = serde_json::json!({
        "action": "route",
        "destination": {"host": "127.0.0.1", "port": 5070},
        "new_ruri": "sip:+18001234@carrier.example",
        "new_from": "sip:+15551000@trunk.example",
        "new_to": "sip:[2001:db8::1]:5060",
    })
    .to_string();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(b2bua.addr)
        .send()
        .await;

    let mut bob_uas = bob.receive("INVITE").await;
    let req = bob_uas.request();
    assert_eq!(req.request_uri().text(), "sip:+18001234@carrier.example");
    assert_eq!(req.from().uri().user(), Some("+15551000"));
    // The BRACKETED IPv6 form reads and rides — only the unbracketed spelling
    // §19.1.1 forbids is refused.
    assert_eq!(req.to().uri().host(), "2001:db8::1");

    bob_uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
    let _r = h.finish().await;
}
