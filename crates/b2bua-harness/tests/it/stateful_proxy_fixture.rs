//! The third-party proxy fixture (`common::stateful_proxy`) behaves as an
//! RFC 3261 §16 transaction-stateful proxy:
//!
//!   alice ──▶ proxy ──▶ bob
//!
//! An INVITE repeated by the caller's Timer A (§17.1.1.2) is absorbed by the
//! proxy's server transaction, which replays its 100, and is forwarded once.
//! A request whose hop count is spent is answered 483 and not forwarded; an
//! ACK whose hop count is spent is dropped (§16.3 step 3).

use std::time::Duration;

use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::Harness;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::common::stateful_proxy::spawn_stateful_proxy;

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5090";
/// One hop's transit: long enough that the proxy's 100 reaches alice after
/// her Timer A first fires.
const TRANSIT_MS: u64 = 300;
/// The UDP T1: Timer A's first interval.
const T1: Duration = Duration::from_millis(500);

/// How many requests `method` crossed `from → to`.
fn requests(h: &Harness, from: &str, to: &str, method: &str) -> usize {
    let (from, to) = (from.parse().unwrap(), to.parse().unwrap());
    h.wire_entries()
        .iter()
        .filter(|e| e.from == from && e.to == to)
        .filter(|e| {
            matches!(CustomParser::new().parse(&e.raw),
                Ok(SipMessage::Request(r)) if r.method() == method)
        })
        .count()
}

/// How many 100 Trying `from` sent `to`.
fn tryings(h: &Harness, from: &str, to: &str) -> usize {
    let (from, to) = (from.parse().unwrap(), to.parse().unwrap());
    h.wire_entries()
        .iter()
        .filter(|e| e.from == from && e.to == to)
        .filter(|e| {
            matches!(CustomParser::new().parse(&e.raw),
                Ok(SipMessage::Response(r)) if r.status() == 100)
        })
        .count()
}

/// alice's INVITE is repeated by Timer A before the proxy's 100 reaches her:
/// bob gets one INVITE, alice gets the 100 again, and the call ends cleanly
/// on bob's 486, ACKed hop by hop.
#[tokio::test(start_paused = true)]
async fn an_invite_repeated_by_timer_a_is_forwarded_once() {
    let h = Harness::with_transit_delay("stateful-proxy-timer-a", TRANSIT_MS);
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let proxy = spawn_stateful_proxy(&h, "proxy", PROXY).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).through(proxy.addr).send().await;
    h.advance(T1).await;
    call.retransmit().await;
    let mut uas = bob.receive("INVITE").await;
    h.advance(Duration::from_millis(4 * TRANSIT_MS)).await;
    assert_eq!(requests(&h, PROXY, BOB, "INVITE"), 1, "the repeat is not forwarded");
    assert_eq!(tryings(&h, PROXY, ALICE), 2, "the repeat draws the 100 again");

    uas.respond(486, "Busy Here").await;
    uas.expect_ack().await;
    call.expect(486).await;
    h.advance(Duration::from_millis(4 * TRANSIT_MS)).await;
    assert_eq!(requests(&h, PROXY, BOB, "INVITE"), 1, "bob saw one INVITE");
    let _ = h.finish().await;
}

/// alice's INVITE arrives with Max-Forwards 0: the proxy answers 483, absorbs
/// alice's ACK, and forwards nothing to bob.
#[tokio::test(start_paused = true)]
async fn a_spent_hop_count_is_answered_483() {
    let h = Harness::with_transit_delay("stateful-proxy-483", TRANSIT_MS);
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let proxy = spawn_stateful_proxy(&h, "proxy", PROXY).await;

    let mut call =
        alice.invite(&bob).with_sdp(OFFER_SDP).max_forwards(0).through(proxy.addr).send().await;
    call.expect(483).await;
    h.advance(Duration::from_millis(4 * TRANSIT_MS)).await;
    assert_eq!(requests(&h, PROXY, BOB, "INVITE"), 0, "nothing is forwarded");
    assert_eq!(requests(&h, PROXY, BOB, "ACK"), 0, "the hop ACK stays at the proxy");
    let _ = h.finish().await;
}

/// alice ACKs bob's 2xx with Max-Forwards 0: the proxy drops it, as an ACK
/// admits no 483. Her ACK with a live hop count then reaches bob, and she
/// hangs up through the proxy.
#[tokio::test(start_paused = true)]
async fn an_ack_with_a_spent_hop_count_is_dropped() {
    let h = Harness::with_transit_delay("stateful-proxy-ack-hops", TRANSIT_MS);
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let proxy = spawn_stateful_proxy(&h, "proxy", PROXY).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).through(proxy.addr).send().await;
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    let ok = call.expect(200).await;

    let spent = format!(
        "ACK {ruri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {ALICE};branch=z9hG4bK-alice-spent-ack\r\n\
         Max-Forwards: 0\r\n\
         Route: <sip:{PROXY};lr>\r\n\
         From: <{from_uri}>;tag={from_tag}\r\n\
         To: <{to_uri}>;tag={to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} ACK\r\n\
         Content-Length: 0\r\n\r\n",
        ruri = call.ruri(),
        from_uri = ok.from().uri(),
        from_tag = ok.from().tag().unwrap_or_default(),
        to_uri = ok.to().uri(),
        to_tag = ok.to().tag().unwrap_or_default(),
        call_id = ok.call_id(),
        cseq = ok.cseq().seq(),
    );
    alice.try_send_datagram(spent.as_bytes(), proxy.addr).await.expect("the fabric takes it");
    h.advance(Duration::from_millis(4 * TRANSIT_MS)).await;
    assert_eq!(requests(&h, PROXY, BOB, "ACK"), 0, "the spent ACK is dropped");

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    scenario_harness::hangup(&mut dialog, &bob).await;
    let _ = h.finish().await;
}
