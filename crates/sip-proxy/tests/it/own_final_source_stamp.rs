//! The finals the proxy answers itself record where the request came from on
//! the topmost Via: `received` when the sent-by is not the source (RFC 3261
//! §18.2.1), `rport` set to the source port when the sender asked for it (RFC
//! 3581 §4). Each request here names a sent-by other than its source and asks
//! for `rport`.

use crate::common;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::{spawn_proxy, FailingSelect, ProxySut};
use scenario_harness::Harness;
use sip_message::header::{HeaderValue, Rport, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::HeaderName;
use sip_message::{SipMessage, SipParser, SipResponse};
use sip_net::UdpEndpoint;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::strategy::SelectError;
use sip_proxy::RoutingStrategy;

const PROXY: &str = "127.0.0.1:5060";
const ALICE: &str = "127.0.0.1:5061";
/// The sent-by alice writes: an address she does not send from.
const SENT_BY: &str = "192.0.2.44:5999";

async fn proxy(h: &Harness) -> ProxySut {
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(FailingSelect(|| SelectError::NoTarget { reason: "no worker alive".into() }));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    spawn_proxy(h, PROXY, strategy, registry).await
}

/// A request alice sends with `extra` lines, its Via naming [`SENT_BY`].
fn request(method: &str, uri: &str, branch: &str, cseq: u32, extra: &str) -> String {
    format!(
        "{method} {uri} SIP/2.0\r\n\
Via: SIP/2.0/UDP {SENT_BY};branch={branch};rport\r\n\
From: <sip:alice@127.0.0.1>;tag=t-{branch}\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: {branch}@127.0.0.1\r\n\
CSeq: {cseq} {method}\r\n\
Contact: <sip:alice@{ALICE}>\r\n\
{extra}Content-Length: 0\r\n\r\n"
    )
}

/// The §17.1.1.3 ACK of the proxy's own non-2xx final to `invite`.
fn ack_for(invite_branch: &str, resp: &SipResponse) -> String {
    format!(
        "ACK sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP {SENT_BY};branch={invite_branch};rport\r\n\
From: <sip:alice@127.0.0.1>;tag=t-{invite_branch}\r\n\
To: {to}\r\n\
Call-ID: {invite_branch}@127.0.0.1\r\n\
CSeq: 1 ACK\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n",
        to = resp.raw(HeaderName::To).next().expect("the final carries To"),
    )
}

async fn exchange(client: &dyn UdpEndpoint, proxy: SocketAddr, raw: &str) -> SipResponse {
    client.send_to(raw.as_bytes(), proxy).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("the proxy answers")
        .expect("queue open");
    let SipMessage::Response(resp) = CustomParser::new().parse(&reply.raw).unwrap() else {
        panic!("expected a response");
    };
    resp
}

fn assert_stamped(resp: &SipResponse, source: SocketAddr) {
    let top: &Via = resp.top_via();
    assert_eq!(
        top.received(),
        Some(source.ip().to_string().as_str()),
        "{} carries received=<source ip>: {}",
        resp.status(),
        top.to_wire()
    );
    assert_eq!(
        top.rport(),
        Some(Rport::Observed(source.port())),
        "{} carries rport=<source port>: {}",
        resp.status(),
        top.to_wire()
    );
}

#[tokio::test]
async fn a_refused_invite_503_carries_received_and_rport() {
    let h = Harness::with_transit_delay("own-final-stamp-503", 0);
    let proxy = proxy(&h).await;
    let (client, alice) = h.bind_sut("alice", ALICE).await;
    let branch = "z9hG4bK-stamp503";
    let invite = request("INVITE", "sip:bob@127.0.0.1:5070", branch, 1, "Max-Forwards: 70\r\n");

    let resp = exchange(client.as_ref(), proxy.addr(), &invite).await;
    assert_eq!(resp.status(), 503);
    assert_stamped(&resp, alice);

    client.send_to(ack_for(branch, &resp).as_bytes(), proxy.addr()).await.unwrap();
    let _ = h.finish().await;
}

#[tokio::test]
async fn an_exhausted_max_forwards_483_carries_received_and_rport() {
    let h = Harness::with_transit_delay("own-final-stamp-483", 0);
    let proxy = proxy(&h).await;
    let (client, alice) = h.bind_sut("alice", ALICE).await;
    let branch = "z9hG4bK-stamp483";
    let invite = request("INVITE", "sip:bob@127.0.0.1:5070", branch, 1, "Max-Forwards: 0\r\n");

    let resp = exchange(client.as_ref(), proxy.addr(), &invite).await;
    assert_eq!(resp.status(), 483);
    assert_stamped(&resp, alice);

    client.send_to(ack_for(branch, &resp).as_bytes(), proxy.addr()).await.unwrap();
    let _ = h.finish().await;
}

#[tokio::test]
async fn an_unsupported_proxy_require_420_carries_received_and_rport() {
    let h = Harness::with_transit_delay("own-final-stamp-420", 0);
    let proxy = proxy(&h).await;
    let (client, alice) = h.bind_sut("alice", ALICE).await;
    let options = request(
        "OPTIONS",
        "sip:bob@127.0.0.1:5070",
        "z9hG4bK-stamp420",
        1,
        "Max-Forwards: 70\r\nProxy-Require: bogus-extension-xyz\r\n",
    );

    let resp = exchange(client.as_ref(), proxy.addr(), &options).await;
    assert_eq!(resp.status(), 420);
    assert_stamped(&resp, alice);
    let _ = h.finish().await;
}

#[tokio::test]
async fn an_unsupported_uri_scheme_416_carries_received_and_rport() {
    let h = Harness::with_transit_delay("own-final-stamp-416", 0);
    let proxy = proxy(&h).await;
    let (client, alice) = h.bind_sut("alice", ALICE).await;
    // A request on the worker-outbound path goes to its Request-URI, and
    // this one names no host.
    let options = request(
        "OPTIONS",
        "urn:service:sos",
        "z9hG4bK-stamp416",
        1,
        &format!("Max-Forwards: 70\r\nRoute: <sip:{PROXY};outbound;lr>\r\n"),
    );

    let resp = exchange(client.as_ref(), proxy.addr(), &options).await;
    assert_eq!(resp.status(), 416);
    assert_stamped(&resp, alice);
    let _ = h.finish().await;
}
