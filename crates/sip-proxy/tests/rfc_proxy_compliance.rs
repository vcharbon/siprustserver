//! RFC 3261 §16 proxy-compliance suite — first-pass coverage for the MUST/SHOULD
//! proxy requirements that the existing transit/load-balancer suites do not
//! assert directly. Drives a **real** `ProxyCore` (ForwardAll) on the harness
//! fabric, mostly via raw datagrams for byte-level control.
//!
//! Covers:
//!   * §16.6 step 3  — Max-Forwards decrement by exactly one on forward
//!   * §16.6 step 8  — proxy-inserted Via branch carries the z9hG4bK magic cookie
//!   * §16.7.3       — response with a foreign top Via is discarded
//!   * §16.7.3       — response with fewer than two Via values is not forwarded
//!   * §16.7.4       — proxy removes its own (top) Via before relaying a response
//!   * §16.3 step 5  — Proxy-Require with an unsupported option-tag → 420 + Unsupported
//!                     (EXPECTED GAP — currently unimplemented; see test note)

mod common;

use std::time::Duration;

use common::{forward_all, spawn_proxy};
use scenario_harness::Harness;
use sip_message::message_helpers::{get_header, get_headers};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

/// Pull the `branch=` token out of a Via header value.
fn via_branch(via: &str) -> Option<&str> {
    via.split(';')
        .find_map(|p| p.trim().strip_prefix("branch="))
        .map(str::trim)
}

// ── §16.6 step 3 + step 8 ───────────────────────────────────────────────────
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

    // §16.6 step 3: Max-Forwards decremented by exactly one.
    let mf = get_header(&req.headers, "max-forwards").expect("Max-Forwards present");
    assert_eq!(mf.trim(), "69", "proxy must decrement Max-Forwards by one (70 → 69)");

    // §16.6 step 8: proxy adds its own Via on top, branch begins with z9hG4bK.
    let vias = get_headers(&req.headers, "via");
    assert_eq!(vias.len(), 2, "bob sees the proxy's Via + alice's");
    let top_branch = via_branch(&vias[0]).expect("top Via has a branch");
    assert!(
        top_branch.starts_with("z9hG4bK"),
        "proxy Via branch must begin with the z9hG4bK magic cookie, got {top_branch:?}"
    );
    let _ = h.finish().await;
}

// ── §16.7.3 — foreign top Via is discarded ──────────────────────────────────
#[tokio::test]
async fn response_with_foreign_top_via_is_discarded() {
    let h = Harness::with_transit_delay("rfc-foreign-via", 0);
    // We inject a raw response from a bind that never sent the matching request,
    // so the locally-minted-tag audit legitimately fires on the fixture. The
    // proxy (not the UA) is the SUT here; waive that UA-side rule.
    h.allow_violation("rfc3261.tags", "raw-injected response; proxy is the SUT, not a real UA");
    let (strategy, registry) = forward_all("127.0.0.1:5070".parse().unwrap());
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (alice, _alice_addr) = h.bind_sut("alice", "127.0.0.1:5060").await;

    // Top Via is NOT the proxy — a response that never transited us.
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

// ── §16.7.3 — fewer than two Via values is not forwarded ─────────────────────
#[tokio::test]
async fn response_with_single_via_is_not_forwarded() {
    let h = Harness::with_transit_delay("rfc-single-via", 0);
    h.allow_violation("rfc3261.tags", "raw-injected response; proxy is the SUT, not a real UA");
    let (bob_ep, bob_addr) = h.bind_sut("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob_addr);
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (alice, _alice_addr) = h.bind_sut("alice", "127.0.0.1:5060").await;

    // Only the proxy's own Via — nothing left upstream to forward to.
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

// ── §16.7.4 — proxy pops its own Via before relaying (positive control) ──────
#[tokio::test]
async fn relayed_response_has_proxy_top_via_removed() {
    let h = Harness::with_transit_delay("rfc-via-pop", 0);
    h.allow_violation("rfc3261.tags", "raw-injected response; proxy is the SUT, not a real UA");
    let (strategy, registry) = forward_all("127.0.0.1:5070".parse().unwrap());
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (alice, _alice_addr) = h.bind_sut("alice", "127.0.0.1:5060").await;

    // Top Via is the proxy; next is alice. The proxy must strip its own Via and
    // relay to alice (received/rport precedence → 127.0.0.1:5060).
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
    let vias = get_headers(&r.headers, "via");
    assert_eq!(vias.len(), 1, "proxy must remove its own (top) Via before relaying");
    assert!(
        vias[0].contains("127.0.0.1:5060"),
        "the remaining top Via is now alice's, got {:?}",
        vias[0]
    );
    let _ = h.finish().await;
}

// ── §16.3 step 5 — Proxy-Require unsupported option-tag → 420 + Unsupported ──
//
// EXPECTED GAP: the proxy does not yet validate Proxy-Require. RFC 3261 §16.3
// check 5 makes this a MUST: a proxy that cannot satisfy an option-tag listed in
// Proxy-Require MUST reject the request with 420 Bad Extension and list the
// offending tags in an Unsupported header. The proxy advertises no extensions,
// so ANY Proxy-Require tag is unsupported. This test is expected to FAIL until
// the validation is implemented; it is the executable spec for that fix.
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
    assert_eq!(resp.status, 420, "unsupported Proxy-Require → 420 Bad Extension");
    let unsupported = get_header(&resp.headers, "unsupported").expect("420 carries Unsupported");
    assert!(
        unsupported.contains("bogus-extension-xyz"),
        "Unsupported header must list the offending option-tag, got {unsupported:?}"
    );

    // The over-required INVITE must not reach bob.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(bob_ep.try_recv().is_none(), "bob must see nothing on a 420");
    let _ = h.finish().await;
}
