//! The stateless proxy's side of the final-response contract (RFC 3261 §16.11).
//!
//! The proxy is **transaction-less** (ADR-0009, ADR-0022 X4): it never
//! generates a provisional of its own and it relays every downstream response
//! by its Via, a 100 included. A caller therefore hears a 100 only when the
//! downstream sent one, and a downstream that never answers leaves the
//! caller's own Timer A/B armed (§17.1.1): *the caller*, not a proxy timer,
//! gives up on it (local 408 at 64·T1). The proxy has no Timer C and no
//! synthesized final. These tests pin both halves:
//!
//!  1. `proxy_emits_no_100_and_relays_the_downstream_100_18x_and_final` — the
//!     caller hears nothing until the downstream's 100, then the 100, 180 and
//!     200 relay in order.
//!  2. `blackholed_worker_leaves_caller_silent_and_retransmits_reforward` —
//!     a worker that never answers produces ZERO proxy-originated messages
//!     toward the caller (no 100, no 408/5xx); the caller's Timer-A INVITE
//!     retransmits are re-forwarded to the SAME target (the rtx memo), so the
//!     caller's own transaction timers own the give-up.
//!
//! The b2bua-side counterpart (a caller that heard a 100 must get its final
//! within the decision deadline) lives in
//! `b2bua-harness/tests/decision_deadline.rs`.

use crate::common;
use std::time::Duration;

use common::{forward_all, spawn_proxy};
use scenario_harness::Harness;
use sip_message::parser::custom::CustomParser;
use sip_message::HeaderName;
use sip_message::{SipMessage, SipParser, SipRequest};

const ALICE_INVITE: &str = "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-alice-1\r\n\
From: <sip:alice@127.0.0.1>;tag=t1\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: stateless-contract@127.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@127.0.0.1:5060>\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n";

fn parse_request(raw: &[u8]) -> SipRequest {
    match CustomParser::new().parse(raw).expect("forwarded message parses") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

/// Build bob's response to the *forwarded* INVITE: echo the full Via stack
/// (proxy's Via on top, alice's below) so the proxy's §16.7 response path
/// routes it back upstream.
fn bob_response(fwd: &SipRequest, status: u16, reason: &str, to_tag: Option<&str>) -> String {
    let vias: Vec<String> = fwd.raw(HeaderName::Via).map(|v| format!("Via: {v}\r\n")).collect();
    let to = match to_tag {
        Some(t) => format!("<sip:bob@127.0.0.1>;tag={t}"),
        None => "<sip:bob@127.0.0.1>".to_string(),
    };
    format!(
        "SIP/2.0 {status} {reason}\r\n\
         {vias}\
         From: <sip:alice@127.0.0.1>;tag=t1\r\n\
         To: {to}\r\n\
         Call-ID: stateless-contract@127.0.0.1\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:bob@127.0.0.1:5070>\r\n\
         Content-Length: 0\r\n\r\n",
        vias = vias.concat(),
    )
}

// ── 1. no proxy 100; the downstream's 100, 180 and 200 relay ─────────────
#[tokio::test]
async fn proxy_emits_no_100_and_relays_the_downstream_100_18x_and_final() {
    let h = Harness::with_transit_delay("stateless-no-100", 0);
    // Raw-injected responses: bob is a bare bind, not a real UA — the audit
    // rules for a full UA dialog legitimately fire on the fixture (same spirit
    // as the sibling §16.7 tests). The proxy is the SUT; waive the UA-side
    // rules: locally-minted tag, the 200-terminus that is never ACKed/BYE'd,
    // and Allow/Supported absence on the bare INVITE.
    h.allow_violation("mid-dialog-tags", "raw-injected responses; proxy is the SUT, not a real UA");
    h.allow_violation(
        "unacked-2xx-not-cleared",
        "bob's 200 is a fixture terminus; testing relay, not the dialog",
    );
    h.allow_violation(
        "no-ack-to-dialog-creating-2xx",
        "alice never ACKs the fixture terminus; testing relay, not the dialog",
    );
    h.allow_violation("allow-supported-on-invite", "bare INVITE fixture; proxy relay is the SUT");
    let (bob_ep, bob_addr) = h.bind_peer("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob_addr);
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (alice, _alice_addr) = h.bind_peer("alice", "127.0.0.1:5060").await;

    alice.send_to(ALICE_INVITE.as_bytes(), proxy.addr()).await.unwrap();
    let fwd = tokio::time::timeout(Duration::from_secs(2), bob_ep.recv())
        .await
        .expect("bob gets the forwarded INVITE")
        .expect("queue open");
    let fwd = parse_request(&fwd.raw);

    // The proxy forwarded WITHOUT answering: no proxy-originated 100 upstream.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        alice.try_recv().is_none(),
        "the stateless proxy must never generate a provisional of its own"
    );

    // Bob's 100 is relayed by its Via (§16.11): the first thing alice hears.
    bob_ep.send_to(bob_response(&fwd, 100, "Trying", None).as_bytes(), proxy.addr()).await.unwrap();
    let relayed = tokio::time::timeout(Duration::from_secs(2), alice.recv())
        .await
        .expect("the downstream 100 relays upstream")
        .expect("queue open");
    let SipMessage::Response(resp) = CustomParser::new().parse(&relayed.raw).unwrap() else {
        panic!("expected the relayed 100");
    };
    assert_eq!(resp.status(), 100, "the downstream's 100 reaches the caller");
    assert_eq!(resp.via().iter().count(), 1, "the proxy's own Via is popped");

    // The 180 relays next, then the 200.
    bob_ep
        .send_to(bob_response(&fwd, 180, "Ringing", Some("b1")).as_bytes(), proxy.addr())
        .await
        .unwrap();
    let relayed = tokio::time::timeout(Duration::from_secs(2), alice.recv())
        .await
        .expect("the 180 relays upstream")
        .expect("queue open");
    let SipMessage::Response(resp) = CustomParser::new().parse(&relayed.raw).unwrap() else {
        panic!("expected the relayed 180");
    };
    assert_eq!(resp.status(), 180, "the second upstream message is the downstream's 180");

    bob_ep
        .send_to(bob_response(&fwd, 200, "OK", Some("b1")).as_bytes(), proxy.addr())
        .await
        .unwrap();
    let relayed = tokio::time::timeout(Duration::from_secs(2), alice.recv())
        .await
        .expect("the 200 relays upstream")
        .expect("queue open");
    let SipMessage::Response(resp) = CustomParser::new().parse(&relayed.raw).unwrap() else {
        panic!("expected the relayed 200");
    };
    assert_eq!(resp.status(), 200);

    let _ = h.finish().await;
}

// ── 2. blackholed worker: proxy stays silent; caller's timers decide ────────
#[tokio::test]
async fn blackholed_worker_leaves_caller_silent_and_retransmits_reforward() {
    let h = Harness::with_transit_delay("stateless-blackhole", 0);
    let (bob_ep, bob_addr) = h.bind_sut("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob_addr);
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (alice, _alice_addr) = h.bind_sut("alice", "127.0.0.1:5060").await;

    alice.send_to(ALICE_INVITE.as_bytes(), proxy.addr()).await.unwrap();
    let _first = tokio::time::timeout(Duration::from_secs(2), bob_ep.recv())
        .await
        .expect("bob gets the forwarded INVITE")
        .expect("queue open");
    // Bob never responds — crashed / blackholed worker.

    // Alice's Timer-A retransmit (same branch) re-forwards to the SAME target
    // via the rtx memo — the proxy keeps pumping, it does not answer.
    alice.send_to(ALICE_INVITE.as_bytes(), proxy.addr()).await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), bob_ep.recv())
        .await
        .expect("the retransmit is re-forwarded to the same (dead) worker")
        .expect("queue open");
    let second = parse_request(&second.raw);
    assert_eq!(second.method(), "INVITE");

    // The contract under test: over the whole window the proxy sent alice
    // NOTHING — no 100, no synthesized 408/5xx. With zero provisionals
    // received, alice's own Timer B stays armed and the CALLER owns the
    // give-up (local 408 at 64·T1). No proxy timer exists, by design.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        alice.try_recv().is_none(),
        "a blackholed worker must produce ZERO proxy-originated messages upstream \
         (no 100, no final) — the caller's transaction timers own the give-up"
    );

    let _ = h.finish().await;
}
