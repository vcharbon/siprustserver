//! The stateless proxy's side of the final-response contract (RFC 3261 §16.11).
//!
//! The proxy is **transaction-less** (ADR-0009, ADR-0022 X4): it never
//! generates a provisional of its own and it relays every downstream response
//! by its Via, a 100 included. A caller therefore hears a 100 only when the
//! downstream sent one, and a downstream that never answers leaves the
//! caller's own Timer A/B armed (§17.1.1): *the caller*, not a proxy timer,
//! gives up on it (local 408 at 64·T1). The proxy has no Timer C and no
//! synthesized final.
//!
//! The b2bua-side counterpart (a caller that heard a 100 must get its final
//! within the decision deadline) lives in
//! `b2bua-harness/tests/it/decision_deadline.rs`.

use crate::common;
use std::net::SocketAddr;
use std::time::Duration;

use common::{forward_all, options_capabilities, spawn_proxy};
use scenario_harness::Harness;
use sip_message::generators::{
    generate_ack_for_2xx_from_invite, generate_bye_for_2xx_from_invite, generate_response,
    GenerateResponseOpts,
};
use sip_message::header::{Contact, HeaderValue};
use sip_message::parser::custom::CustomParser;
use sip_message::received::observed_source;
use sip_message::{SipMessage, SipParser, SipRequest, SipResponse, SipStr};
use sip_net::{UdpEndpoint, UdpPacket};
use tokio::time::Instant;

const ALICE_INVITE: &str = "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-alice-1\r\n\
From: <sip:alice@127.0.0.1>;tag=t1\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: stateless-contract@127.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@127.0.0.1:5060>\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n";

/// RFC 3261 §17.1.1.2: T1, the base of the INVITE client's timers.
const T1: Duration = Duration::from_millis(500);

fn parse(raw: &[u8]) -> SipMessage {
    CustomParser::new().parse(raw).expect("the message parses")
}

fn request(packet: &UdpPacket) -> SipRequest {
    match parse(&packet.raw) {
        SipMessage::Request(r) => r,
        SipMessage::Response(r) => panic!("expected a request, got {}", r.status()),
    }
}

fn response(packet: &UdpPacket) -> SipResponse {
    match parse(&packet.raw) {
        SipMessage::Response(r) => r,
        SipMessage::Request(r) => panic!("expected a response, got {}", r.method()),
    }
}

/// The next datagram `ep` receives, failing as `what` past two seconds.
async fn next(ep: &dyn UdpEndpoint, what: &str) -> UdpPacket {
    tokio::time::timeout(Duration::from_secs(2), ep.recv())
        .await
        .unwrap_or_else(|_| panic!("{what}: nothing arrived"))
        .expect("queue open")
}

/// Bob's response to the request `fwd` he received from `from` (RFC 3261
/// §8.2.6): the Via stack echoed, the proxy's on top, so the proxy's §16.7
/// response path routes it back; a 2xx to the INVITE states his Contact,
/// echoes the Record-Route and advertises his capabilities (§13.2.1).
fn bob_response(fwd: &SipRequest, from: SocketAddr, status: u16, reason: &str) -> SipResponse {
    let establishes = status > 100 && fwd.method() == "INVITE";
    let contact = establishes.then(|| {
        Contact::parse(&SipStr::from_static("<sip:bob@127.0.0.1:5070>")).expect("a Contact")
    });
    let extra_headers = match status < 300 && establishes {
        true => options_capabilities(),
        false => Vec::new(),
    };
    let opts = GenerateResponseOpts {
        to_tag: Some("b1".into()),
        contact,
        extra_headers,
        incoming_source: Some(observed_source(from)),
        ..Default::default()
    };
    generate_response(fwd, status, reason, &opts).expect("bob's response")
}

/// The proxy sends no 100 of its own: the caller hears nothing until the
/// downstream's 100, then the 100, 180 and 200 relay in order, the proxy's
/// Via popped from each. The call then completes through the proxy: the ACK
/// of the 200, and a BYE and its 200.
#[tokio::test(start_paused = true)]
async fn proxy_emits_no_100_and_relays_the_downstream_100_18x_and_final() {
    let h = Harness::with_transit_delay("stateless-no-100", 1);
    let (bob, bob_addr) = h.bind_peer("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob_addr);
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (alice, _alice_addr) = h.bind_peer("alice", "127.0.0.1:5060").await;
    let SipMessage::Request(invite) = parse(ALICE_INVITE.as_bytes()) else {
        panic!("alice's INVITE is a request");
    };

    alice.send_to(ALICE_INVITE.as_bytes(), proxy.addr()).await.unwrap();
    let packet = next(&*bob, "bob gets the forwarded INVITE").await;
    let fwd = request(&packet);

    h.advance(Duration::from_millis(100)).await;
    assert!(
        alice.try_recv().is_none(),
        "the stateless proxy must never generate a provisional of its own"
    );

    let trying = bob_response(&fwd, packet.src, 100, "Trying");
    bob.send_to(trying.image(), proxy.addr()).await.unwrap();
    let resp = response(&next(&*alice, "the downstream 100 relays upstream").await);
    assert_eq!(resp.status(), 100, "the downstream's 100 reaches the caller");
    assert_eq!(resp.via().iter().count(), 1, "the proxy's own Via is popped");

    let ringing = bob_response(&fwd, packet.src, 180, "Ringing");
    bob.send_to(ringing.image(), proxy.addr()).await.unwrap();
    let resp = response(&next(&*alice, "the 180 relays upstream").await);
    assert_eq!(resp.status(), 180, "the second upstream message is the downstream's 180");

    let ok = bob_response(&fwd, packet.src, 200, "OK");
    bob.send_to(ok.image(), proxy.addr()).await.unwrap();
    let ok = response(&next(&*alice, "the 200 relays upstream").await);
    assert_eq!(ok.status(), 200);

    let ack = generate_ack_for_2xx_from_invite(&invite, &ok, "z9hG4bK-alice-ack", None)
        .expect("alice's ACK");
    alice.send_to(ack.image(), proxy.addr()).await.unwrap();
    let relayed = request(&next(&*bob, "the ACK relays downstream").await);
    assert_eq!(relayed.method(), "ACK");

    let bye =
        generate_bye_for_2xx_from_invite(&invite, &ok, 2, "z9hG4bK-alice-bye").expect("a BYE");
    alice.send_to(bye.image(), proxy.addr()).await.unwrap();
    let packet = next(&*bob, "the BYE relays downstream").await;
    let fwd_bye = request(&packet);
    assert_eq!(fwd_bye.method(), "BYE");
    let bye_ok = bob_response(&fwd_bye, packet.src, 200, "OK");
    bob.send_to(bye_ok.image(), proxy.addr()).await.unwrap();
    let resp = response(&next(&*alice, "the BYE's 200 relays upstream").await);
    assert_eq!((resp.status(), resp.cseq().method().as_str()), (200, "BYE"));

    let _ = h.finish().await;
}

/// A worker that never answers draws no proxy-originated message toward the
/// caller, neither a 100 nor a final: each copy the caller's Timer A sends
/// (RFC 3261 §17.1.1.2) is re-forwarded to the same target on the same
/// branch, and the caller's transaction ends at Timer B (64·T1) with nothing
/// heard.
#[tokio::test(start_paused = true)]
async fn blackholed_worker_leaves_caller_silent_and_retransmits_reforward() {
    let h = Harness::with_transit_delay("stateless-blackhole", 1);
    let (bob, bob_addr) = h.bind_peer("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob_addr);
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (alice, _alice_addr) = h.bind_peer("alice", "127.0.0.1:5060").await;

    let start = Instant::now();
    alice.send_to(ALICE_INVITE.as_bytes(), proxy.addr()).await.unwrap();
    let first = request(&next(&*bob, "bob gets the forwarded INVITE").await);

    let mut interval = T1;
    let mut copy_at = T1;
    while copy_at < 64 * T1 {
        h.advance((start + copy_at).saturating_duration_since(Instant::now())).await;
        alice.send_to(ALICE_INVITE.as_bytes(), proxy.addr()).await.unwrap();
        let copy = request(&next(&*bob, "the copy is re-forwarded to the dead worker").await);
        assert_eq!(copy.method(), "INVITE");
        assert_eq!(copy.top_via().branch(), first.top_via().branch(), "on the first's branch");
        interval *= 2;
        copy_at += interval;
    }

    h.advance((start + 64 * T1).saturating_duration_since(Instant::now())).await;
    assert!(
        alice.try_recv().is_none(),
        "a blackholed worker must produce ZERO proxy-originated messages upstream \
         (no 100, no final): the caller's Timer B ends its transaction"
    );

    let _ = h.finish().await;
}
