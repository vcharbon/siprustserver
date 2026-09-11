//! The hop count across the back-to-back UA (RFC 3261 §16.6 step 3).
//!
//! A B2BUA is not a proxy, but it starts a leg *because* a request arrived, so
//! the hop budget must cross it rather than refill at it. Three contracts:
//!
//!   * the received count is a **decision input**: it reaches the decision
//!     engine, which may refuse a call whose budget is spent (an INVITE at 0 is
//!     a liveness probe the backend answers, not something the stack
//!     short-circuits);
//!   * the stack **owns** what a leg it originates states: a decision restating
//!     `Max-Forwards` changes nothing on the wire;
//!   * a route for a spent count is refused **483 Too Many Hops** rather than
//!     carried out — the brake that keeps a routing loop through this element
//!     finite.

use std::time::Duration;

use std::sync::Arc;

use b2bua::decision::test_adapter::{reject, route_to};
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine, SipHeaderUpdates};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::{HeaderName, MaxForwards};
use sip_message::SipRequest;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

/// The hop count `req` states, or the assertion fails naming `what`.
fn hops(req: &SipRequest, what: &str) -> u32 {
    req.header::<MaxForwards>()
        .unwrap_or_else(|| panic!("{what} must state Max-Forwards (RFC 3261 §8.1.1.6)"))
        .unwrap_or_else(|e| panic!("{what} states an unreadable Max-Forwards: {e}"))
        .value()
}

/// The received count is a DECISION INPUT: it reaches the decision engine under
/// its own name, and a backend that refuses on it produces an ordinary reject —
/// caller finalled, callee never dialled, one CDR. This is what makes an INVITE
/// at `Max-Forwards: 0` a probe of the whole chain rather than something the
/// stack answers on the backend's behalf.
#[tokio::test]
async fn a_spent_hop_count_reaches_the_decision_engine() {
    let h = Harness::with_transit_delay("b2bua-max-forwards-decides", 0)
        .describe("the hop count reaches the decision engine, which refuses a spent one");
    let alice = h.agent("alice", "127.0.0.1:5067").await;
    let bob = h.agent("bob", "127.0.0.1:5077").await;
    let bob_port = 5077;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |req| {
                match req.sip_headers.get("Max-Forwards").and_then(|v| v.first()) {
                    Some(hops) if hops.trim() == "0" => reject(483, "Too Many Hops"),
                    // A backend that never sees the count could not tell these
                    // apart, so the ROUTE arm is what fails this test.
                    _ => NewCallResponse::Route(route_to("127.0.0.1", bob_port)),
                }
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5087").await;

    let mut call =
        alice.invite(&bob).with_sdp(OFFER).max_forwards(0).through(b2bua.addr).send().await;

    let resp = call.expect(483).await;
    assert!(resp.to().tag().is_some(), "non-100 final carries a To-tag (RFC 3261 §8.2.6.2)");
    // Strict: a straggler INVITE toward the callee would PANIC here.
    bob.drain_expecting(Duration::from_millis(50), &[]).await;

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "the refused call is a call that was born and released");
    assert!(cdrs[0].b_legs.is_empty(), "a reject creates no b-leg");
    b2bua.assert_fully_reaped();

    let _r = h.finish().await;
}

/// The count is READ-ONLY across the decision seam: `Max-Forwards` is
/// structural, so a decision restating it is dropped rather than carried — the
/// b-leg states ONE count, the stack's own decrement. Without the drop the
/// decision's line would ride BESIDE the generator's, and a backend could refill
/// a budget it does not own.
#[tokio::test]
async fn a_decision_cannot_restate_the_hop_count() {
    let h = Harness::with_transit_delay("b2bua-max-forwards-read-only", 0)
        .describe("a decision restating Max-Forwards changes nothing on the wire");
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", "127.0.0.1:5072").await;
    let bob_port = 5072;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", bob_port);
                let mut updates = SipHeaderUpdates::new();
                // A backend trying to refill the budget, and one trying to
                // withhold the header entirely.
                updates.insert("Max-Forwards".into(), Some("70".into()));
                updates.insert("Content-Length".into(), None);
                r.update_headers = Some(updates);
                NewCallResponse::Route(r)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5082").await;

    let mut call =
        alice.invite(&bob).with_sdp(OFFER).max_forwards(20).through(b2bua.addr).send().await;

    let mut uas = bob.receive("INVITE").await;
    assert_eq!(
        uas.request().raw(HeaderName::MaxForwards).count(),
        1,
        "the b-leg states exactly ONE Max-Forwards, not the stack's beside the decision's",
    );
    assert_eq!(
        hops(uas.request(), "the b-leg INVITE"),
        19,
        "and it is the stack's decrement, not the 70 the decision asked for",
    );

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    b2bua.assert_fully_reaped();

    let _r = h.finish().await;
}

/// The brake. A decision that routes a spent-hop INVITE anyway cannot be carried
/// out — originating a leg IS passing the request on, which RFC 3261 §16.3
/// forbids at 0 — so the caller is refused 483 and the callee is never dialled.
/// Without it a routing loop through this element has no bound at all in a
/// topology with no proxy in front.
#[tokio::test]
async fn a_route_for_a_spent_hop_count_is_refused_483() {
    let h = Harness::with_transit_delay("b2bua-max-forwards-brake", 0)
        .describe("a ROUTE for a spent hop count is refused 483 instead of originating a leg");
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    // `route_all_to` routes EVERY call, hop count included — the misconfigured
    // backend the brake exists for.
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5073).start(&h, "b2bua", "127.0.0.1:5083").await;

    let mut call =
        alice.invite(&bob).with_sdp(OFFER).max_forwards(0).through(b2bua.addr).send().await;

    let resp = call.expect(483).await;
    assert!(resp.to().tag().is_some(), "non-100 final carries a To-tag (RFC 3261 §8.2.6.2)");
    bob.drain_expecting(Duration::from_millis(50), &[]).await;

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the refused call");
    assert!(cdrs[0].b_legs.is_empty(), "the leg the decision asked for is never originated");
    b2bua.assert_fully_reaped();

    let _r = h.finish().await;
}

/// The last hop legitimately receives a spent count: a caller with ONE hop left
/// still reaches the callee, because the callee is the request's destination and
/// forwards nothing. This is the composition the two halves meet at — the b-leg
/// leaves at exactly 0, and a call still establishes and tears down cleanly.
#[tokio::test]
async fn a_b_leg_may_leave_with_its_last_hop_spent() {
    let h = Harness::with_transit_delay("b2bua-max-forwards-last-hop", 0)
        .describe("a caller with one hop left still reaches the callee, at Max-Forwards 0");
    let alice = h.agent("alice", "127.0.0.1:5069").await;
    let bob = h.agent("bob", "127.0.0.1:5079").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5079).start(&h, "b2bua", "127.0.0.1:5089").await;

    let mut call =
        alice.invite(&bob).with_sdp(OFFER).max_forwards(1).through(b2bua.addr).send().await;

    let mut uas = bob.receive("INVITE").await;
    assert_eq!(
        hops(uas.request(), "the b-leg INVITE"),
        0,
        "one hop left spends the last one; the count never wraps",
    );

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    b2bua.assert_fully_reaped();

    let _r = h.finish().await;
}

/// The stack is the UAS of the requests it answers itself, so the hop gate must
/// NOT refuse them: a caller whose BYE arrives with a spent count has already
/// released the session (RFC 3261 §15.1.1), and a 483 there would hold both legs
/// up until a keepalive or duration timer reaped them. The teardown still
/// crosses to the callee, at the spent count rather than a refilled one.
#[tokio::test]
async fn a_bye_at_zero_hops_still_tears_the_call_down() {
    let h = Harness::with_transit_delay("b2bua-max-forwards-bye-zero", 0)
        .describe("a BYE with Max-Forwards: 0 is answered and relayed, never refused");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5071).start(&h, "b2bua", "127.0.0.1:5081").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.send_request(InDialogMethod::Bye).max_forwards(0).send().await;
    let mut bob_bye = bob.receive("BYE").await;
    assert_eq!(
        hops(bob_bye.request(), "the relayed BYE"),
        0,
        "a spent count crosses spent; the next element stops it, not this one",
    );
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    b2bua.assert_fully_reaped();

    let _r = h.finish().await;
}

#[tokio::test]
async fn every_relayed_request_states_one_hop_less() {
    let h = Harness::with_transit_delay("b2bua-max-forwards-decrement", 0)
        .describe("the b-leg INVITE, a relayed re-INVITE and the relayed BYE each spend one hop");
    let alice = h.agent("alice", "127.0.0.1:5068").await;
    let bob = h.agent("bob", "127.0.0.1:5078").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5078).start(&h, "b2bua", "127.0.0.1:5088").await;

    // Alice dials with a count of her own, so the assertions below cannot pass
    // by coincidence with the default.
    let mut call =
        alice.invite(&bob).with_sdp(OFFER).max_forwards(20).through(b2bua.addr).send().await;

    let mut uas = bob.receive("INVITE").await;
    assert_eq!(hops(uas.request(), "the b-leg INVITE"), 19, "the originated leg spends one hop");

    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── a re-INVITE relayed across the bridge spends a hop of ITS OWN count ──
    let mut reinv = dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut bob_uas = bob.receive("INVITE").await;
    assert_eq!(hops(bob_uas.request(), "the relayed re-INVITE"), 69, "the relay spends one hop");
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    reinv.expect(200).await;
    bob.receive("ACK").await;

    // ── and so does the teardown the caller sends ──
    let mut bye = dialog.bye().await;
    let mut bob_bye = bob.receive("BYE").await;
    assert_eq!(hops(bob_bye.request(), "the relayed BYE"), 69, "the relay spends one hop");
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    b2bua.assert_fully_reaped();

    let _r = h.finish().await;
}
