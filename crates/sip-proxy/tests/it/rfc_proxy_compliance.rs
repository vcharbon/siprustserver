//! RFC 3261 §16 proxy compliance: the MUST and SHOULD proxy requirements the
//! transit and load-balancer suites do not assert directly, on a real
//! `ProxyCore` (ForwardAll) over the harness fabric, mostly by raw datagrams
//! for byte-level control.
//!
//! Covers:
//!   * §16.6 step 3  — Max-Forwards decrement by exactly one on forward
//!   * §16.6 step 8  — proxy-inserted Via branch carries the z9hG4bK magic cookie
//!   * §16.7.3       — response with a foreign top Via is discarded
//!   * §16.7.3       — response with fewer than two Via values is not forwarded
//!   * §16.7.4       — proxy removes its own (top) Via before relaying a response
//!   * §18.2.2       — a relayed response goes to the received/rport target
//!   * §16.3 step 5  — Proxy-Require with an unsupported option-tag → 420 + Unsupported
//!   * §16.3 step 5, §25.1 — a Proxy-Require that is no option-tag list → 400

use crate::common;
use std::time::Duration;

use common::{forward_all, spawn_proxy};
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::OutOfDialogMethod;
use sip_message::header::{MaxForwards, Unsupported, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::HeaderName;
use sip_message::{SipMessage, SipParser};

/// RFC 3261 §16.6 steps 3 and 8: a forwarded request's Max-Forwards drops by
/// exactly one, and the proxy's own Via branch carries the z9hG4bK magic cookie.
#[tokio::test]
async fn forwarded_invite_decrements_max_forwards_and_stamps_magic_cookie_via() {
    let h = Harness::with_transit_delay("rfc-mf-decrement", 0);
    let (bob_ep, bob_addr) = h.bind_sut("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob_addr);
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (client, _client_addr) = h.bind_sut("alice", "127.0.0.1:5060").await;

    let invite = "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-alice\r\n\
From: <sip:alice@127.0.0.1>;tag=t1\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: mf-decr-call@127.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@127.0.0.1:5060>\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n";
    client.send_to(invite.as_bytes(), proxy.addr()).await.unwrap();

    let fwd = tokio::time::timeout(Duration::from_secs(2), bob_ep.recv())
        .await
        .expect("bob should get the forwarded INVITE")
        .expect("queue open");
    let SipMessage::Request(req) = CustomParser::new().parse(&fwd.raw).unwrap() else {
        panic!("expected a request");
    };

    let mf = req.header::<MaxForwards>().expect("Max-Forwards present").expect("reads");
    assert_eq!(mf.value(), 69, "proxy must decrement Max-Forwards by one (70 → 69)");

    let vias = req.via();
    assert_eq!(vias.len(), 2, "bob sees the proxy's Via + alice's");
    let top = req.top_via();
    assert!(
        top.is_rfc3261_branch(),
        "proxy Via branch must begin with the z9hG4bK magic cookie, got {:?}",
        top.branch()
    );
    let _ = h.finish().await;
}

/// RFC 3261 §16.7.3: a response whose top Via is not the proxy's is
/// discarded, never relayed.
#[tokio::test]
async fn response_with_foreign_top_via_is_discarded() {
    let h = Harness::with_transit_delay("rfc-foreign-via", 0);
    h.allow_violation("mid-dialog-tags", "raw-injected response; proxy is the SUT, not a real UA");
    h.waive(
        scenario_harness::WaiverScope::rule(
            "response-destination",
            "raw-injected response sent past its top Via; the proxy is the subject",
        )
        .on_party("alice"),
    );
    let (strategy, registry) = forward_all("127.0.0.1:5070".parse().unwrap());
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (alice, _alice_addr) = h.bind_sut("alice", "127.0.0.1:5060").await;

    let resp = "SIP/2.0 180 Ringing\r\n\
Via: SIP/2.0/UDP 9.9.9.9:5999;branch=z9hG4bK-foreign\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-alice\r\n\
From: <sip:alice@127.0.0.1>;tag=t1\r\n\
To: <sip:bob@127.0.0.1>;tag=b1\r\n\
Call-ID: foreign-via@127.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
    alice.send_to(resp.as_bytes(), proxy.addr()).await.unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        alice.try_recv().is_none(),
        "a response whose top Via is not the proxy must be discarded, never relayed (§16.7.3)"
    );
    let _ = h.finish().await;
}

/// RFC 3261 §16.7.3: a response that carries only the proxy's Via was meant
/// for the proxy and is forwarded nowhere.
#[tokio::test]
async fn response_with_single_via_is_not_forwarded() {
    let h = Harness::with_transit_delay("rfc-single-via", 0);
    h.allow_violation("mid-dialog-tags", "raw-injected response; proxy is the SUT, not a real UA");
    let (bob_ep, bob_addr) = h.bind_sut("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob_addr);
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (alice, _alice_addr) = h.bind_sut("alice", "127.0.0.1:5060").await;

    let resp = "SIP/2.0 180 Ringing\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-only\r\n\
From: <sip:alice@127.0.0.1>;tag=t1\r\n\
To: <sip:bob@127.0.0.1>;tag=b1\r\n\
Call-ID: single-via@127.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
    alice.send_to(resp.as_bytes(), proxy.addr()).await.unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(alice.try_recv().is_none(), "single-Via response is not relayed back");
    assert!(bob_ep.try_recv().is_none(), "single-Via response is not relayed onward");
    let _ = h.finish().await;
}

/// RFC 3261 §16.7.4: the proxy removes its own top Via before relaying a
/// response, which goes to the next Via's received/rport target.
#[tokio::test]
async fn relayed_response_has_proxy_top_via_removed() {
    let h = Harness::with_transit_delay("rfc-via-pop", 0);
    h.waive(
        WaiverScope::rule(
            "mid-dialog-tags",
            "the test injects the deviant tags; the proxy relays them as RFC 3261 §16.7 requires",
        )
        .on_party("proxy"),
    );
    let (strategy, registry) = forward_all("127.0.0.1:5070".parse().unwrap());
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (alice, _alice_addr) = h.bind_sut("alice", "127.0.0.1:5060").await;

    let resp = "SIP/2.0 180 Ringing\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-proxy\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-alice\r\n\
From: <sip:alice@127.0.0.1>;tag=t1\r\n\
To: <sip:bob@127.0.0.1>;tag=b1\r\n\
Call-ID: via-pop@127.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
    alice.send_to(resp.as_bytes(), proxy.addr()).await.unwrap();

    let got = tokio::time::timeout(Duration::from_secs(2), alice.recv())
        .await
        .expect("alice should get the relayed response")
        .expect("queue open");
    let SipMessage::Response(r) = CustomParser::new().parse(&got.raw).unwrap() else {
        panic!("expected a response");
    };
    let vias: Vec<Via> = r.via().iter().cloned().collect();
    assert_eq!(vias.len(), 1, "proxy must remove its own (top) Via before relaying");
    assert_eq!(
        vias[0].host_port(),
        ("127.0.0.1", 5060),
        "the remaining top Via is now alice's, got {:?}",
        vias[0]
    );
    let _ = h.finish().await;
}

/// RFC 3261 §16.3 step 5: a proxy that cannot satisfy an option-tag listed in
/// Proxy-Require rejects the request 420 Bad Extension, naming the tags in
/// Unsupported. The proxy supports no extension, so every tag is unsupported.
/// The proxy absorbs the ACK of its own 420, and nothing reaches bob.
#[tokio::test]
async fn proxy_require_unsupported_option_tag_returns_420() {
    let h = Harness::with_transit_delay("rfc-proxy-require", 0);
    let (bob_ep, bob_addr) = h.bind_sut("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob_addr);
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (client, _client_addr) = h.bind_sut("alice", "127.0.0.1:5060").await;

    let invite = "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-pr\r\n\
From: <sip:alice@127.0.0.1>;tag=t1\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: proxy-require@127.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@127.0.0.1:5060>\r\n\
Proxy-Require: bogus-extension-xyz\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n";
    client.send_to(invite.as_bytes(), proxy.addr()).await.unwrap();

    let reply = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("client should get a 420 for an unsupported Proxy-Require")
        .expect("queue open");
    let SipMessage::Response(resp) = CustomParser::new().parse(&reply.raw).unwrap() else {
        panic!("expected a response");
    };
    assert_eq!(resp.status(), 420, "unsupported Proxy-Require → 420 Bad Extension");
    let unsupported =
        resp.token_set::<Unsupported>().expect("420 carries Unsupported").expect("reads");
    assert!(
        unsupported.contains("bogus-extension-xyz"),
        "Unsupported header must list the offending option-tag, got {unsupported:?}"
    );

    let ack = format!(
        "ACK sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-pr\r\n\
From: <sip:alice@127.0.0.1>;tag=t1\r\n\
To: {to}\r\n\
Call-ID: proxy-require@127.0.0.1\r\n\
CSeq: 1 ACK\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n",
        to = resp.raw(HeaderName::To).next().expect("420 carries To"),
    );
    client.send_to(ack.as_bytes(), proxy.addr()).await.unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(bob_ep.try_recv().is_none(), "bob must see nothing on a 420");
    let _ = h.finish().await;
}

/// RFC 3261 §16.3 step 5, §25.1: `option-tag = token`, so a Proxy-Require
/// entry that is no token names no extension the proxy could look up or list
/// in Unsupported: the request is answered 400 and never forwarded.
#[tokio::test]
async fn an_unreadable_proxy_require_is_answered_400_and_not_forwarded() {
    let h = Harness::with_transit_delay("rfc-proxy-require-unreadable", 0);
    let (bob_ep, bob_addr) = h.bind_sut("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob_addr);
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (client, _client_addr) = h.bind_sut("alice", "127.0.0.1:5060").await;

    let invite = "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-pr-bad\r\n\
From: <sip:alice@127.0.0.1>;tag=t1\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: proxy-require-bad@127.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@127.0.0.1:5060>\r\n\
Proxy-Require: foo bar\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n";
    client.send_to(invite.as_bytes(), proxy.addr()).await.unwrap();

    let reply = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("client should get a 400 for an unreadable Proxy-Require")
        .expect("queue open");
    let SipMessage::Response(resp) = CustomParser::new().parse(&reply.raw).unwrap() else {
        panic!("expected a response");
    };
    assert_eq!(resp.status(), 400, "an unreadable Proxy-Require → 400 Bad Request");

    let ack = format!(
        "ACK sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-pr-bad\r\n\
From: <sip:alice@127.0.0.1>;tag=t1\r\n\
To: {to}\r\n\
Call-ID: proxy-require-bad@127.0.0.1\r\n\
CSeq: 1 ACK\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n",
        to = resp.raw(HeaderName::To).next().expect("400 carries To"),
    );
    client.send_to(ack.as_bytes(), proxy.addr()).await.unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(bob_ep.try_recv().is_none(), "bob must see nothing on a 400");
    let _ = h.finish().await;
}

/// RFC 3261 §18.2.2, RFC 3581 §4: alice behind a NAT names an address she
/// does not listen on as her Via sent-by and asks for `rport`: the proxy
/// stamps her source on that Via, and bob's 200 relayed back reaches her
/// there, as `response-destination` gates.
#[tokio::test(start_paused = true)]
async fn a_relayed_response_reaches_a_caller_behind_nat_at_her_source() {
    let h = Harness::new("rfc-relayed-response-behind-nat");
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob.addr());
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let alice = h
        .agent("alice", "127.0.0.1:5060")
        .await
        .behind_nat("192.0.2.77:5099".parse().expect("an address"));
    let mut options =
        alice.request(OutOfDialogMethod::Options, &bob).through(proxy.addr()).send().await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;
    options.expect(200).await;
    let _ = h.finish().await;
}
