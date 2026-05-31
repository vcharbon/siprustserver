//! Transit-only suite — port of `tests/sip-front-proxy/transit-only/*`. Drives a
//! **real** `ProxyCore` (ForwardAll) as a SUT on the harness fabric: the proxy
//! auto-forwards as datagrams arrive. Covers the happy INVITE/200/ACK/BYE call
//! (Via push/pop, Record-Route insertion, Route stripping), Max-Forwards → 483,
//! and malformed-datagram drop.

mod common;

use common::{forward_all, spawn_proxy};
use scenario_harness::Harness;
use sip_message::message_helpers::{get_header, get_headers};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\n";

#[tokio::test]
async fn happy_call_invite_200_ack_bye_through_real_proxy() {
    let h = Harness::with_transit_delay("transit-happy", 0)
        .describe("alice → ProxyCore(ForwardAll) → bob; the real proxy auto-forwards.");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob.addr());
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;

    // INVITE — addressed to bob, sent through the proxy. The SUT forwards it.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let recvd = uas.request();
    assert!(
        get_header(&recvd.headers, "record-route").is_some_and(|rr| rr.contains("127.0.0.1:5080") && rr.contains(";lr")),
        "proxy must insert a ;lr Record-Route"
    );
    assert_eq!(get_headers(&recvd.headers, "via").len(), 2, "bob sees alice's Via + the proxy's");

    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    let ok = call.expect(200).await;
    assert!(get_header(&ok.headers, "record-route").is_some(), "200 OK echoes the proxy RR");

    // ACK — alice's learned route set sends it to the proxy, which strips its
    // Route and forwards to bob.
    let mut dialog = call.ack().await;
    let ack = bob.receive("ACK").await;
    assert!(get_headers(&ack.request().headers, "route").is_empty(), "proxy strips its own Route from the ACK");

    // BYE — same loose-routing path.
    let mut bye = dialog.bye().await;
    let mut bob_bye = bob.receive("BYE").await;
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;

    assert!(proxy.metrics.record_route_inserted_total() >= 1);
    let report = h.finish().await;
    assert!(report.entries().iter().all(|e| e.delivered), "all hops delivered");
    assert_eq!(report.scenario().lanes.len(), 3, "alice + proxy + bob lanes");
}

#[tokio::test]
async fn max_forwards_zero_returns_483() {
    let h = Harness::with_transit_delay("transit-mf0", 0);
    let (bob_ep, bob_addr) = h.bind_sut("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob_addr);
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (client, _client_addr) = h.bind_sut("alice", "127.0.0.1:5060").await;

    let invite = "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-mf\r\n\
From: <sip:alice@127.0.0.1>;tag=t1\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: mf0-call@127.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Max-Forwards: 0\r\n\
Content-Length: 0\r\n\r\n";
    client.send_to(invite.as_bytes(), proxy.addr()).await.unwrap();

    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("client should get a reply")
        .expect("queue open");
    let SipMessage::Response(resp) = CustomParser::new().parse(&reply.raw).unwrap() else {
        panic!("expected a response");
    };
    assert_eq!(resp.status, 483, "Max-Forwards: 0 → 483 Too Many Hops");

    // Bob must not have received the over-hopped INVITE.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(bob_ep.try_recv().is_none(), "bob must see nothing on a 483");
    let _ = h.finish().await;
}

#[tokio::test]
async fn malformed_datagram_dropped_silently() {
    let h = Harness::with_transit_delay("transit-malformed", 0);
    let (bob_ep, bob_addr) = h.bind_sut("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob_addr);
    let proxy = spawn_proxy(&h, "127.0.0.1:5080", strategy, registry).await;
    let (client, _client_addr) = h.bind_sut("alice", "127.0.0.1:5060").await;

    client.send_to(b"this is not a SIP message at all\r\n\r\n", proxy.addr()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(bob_ep.try_recv().is_none(), "garbage is dropped, never forwarded");
    assert!(client.try_recv().is_none(), "garbage gets no reply (RFC 3261 §16.3)");
    let _ = h.finish().await;
}
