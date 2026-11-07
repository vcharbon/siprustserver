//! Every final the proxy answers itself leaves to the response target of its
//! stamped top Via (RFC 3261 §18.2.2, RFC 3581 §4), as a relayed response does.
//! The peer here sends from one socket and listens on another, so the sent-by
//! port and the source port tell the two rules apart.

use crate::common;
use std::sync::Arc;
use std::time::Duration;

use common::{spawn_proxy, FailingSelect, ProxySut};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{HeaderName, SipMessage, SipParser, SipResponse};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::strategy::SelectError;
use sip_proxy::{ProxyAddr, ProxyCoreBuilder, RoutingStrategy};
use sip_txn::IdGen;

const PROXY: &str = "127.0.0.1:5060";
/// The socket the peer sends from.
const SEND: &str = "127.0.0.1:40000";
/// The socket the peer listens on, the sent-by of its Via.
const LISTEN: &str = "127.0.0.1:5070";

struct Rig {
    h: Harness,
    proxy: ProxySut,
    send: Box<dyn UdpEndpoint>,
    listen: Box<dyn UdpEndpoint>,
}

async fn rig(name: &str) -> Rig {
    let h = Harness::with_transit_delay(name, 1);
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(FailingSelect(|| SelectError::NoTarget { reason: "no worker alive".into() }));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let proxy = spawn_proxy(&h, PROXY, strategy, registry).await;
    let (send, _) = h.bind_sut("peer", SEND).await;
    let (listen, _) = h.bind_sut("peer-listen", LISTEN).await;
    Rig { h, proxy, send, listen }
}

/// `method` from the peer, its top Via naming `via_tail` after the branch.
fn request(method: &str, branch: &str, via_tail: &str, to: &str, extra: &str) -> String {
    format!(
        "{method} sip:bob@192.0.2.30:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {LISTEN};branch={branch}{via_tail}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@127.0.0.1>;tag=t-{branch}\r\n\
To: {to}\r\n\
Call-ID: {branch}@127.0.0.1\r\n\
CSeq: 1 {method}\r\n\
Contact: <sip:alice@{LISTEN}>\r\n\
{extra}Content-Length: 0\r\n\r\n"
    )
}

const BOB: &str = "<sip:bob@192.0.2.30>";

impl Rig {
    async fn send(&self, raw: &str) {
        self.send.send_to(raw.as_bytes(), self.proxy.addr()).await.expect("sent");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    /// The one response at `at`, whose status is `status`, and its bytes.
    fn answered(&self, at: &dyn UdpEndpoint, status: u16) -> (SipResponse, Vec<u8>) {
        let packet = at.try_recv().unwrap_or_else(|| panic!("the {status} arrives here"));
        let Ok(SipMessage::Response(resp)) = CustomParser::new().parse(&packet.raw) else {
            panic!("a response");
        };
        assert_eq!(resp.status(), status);
        assert!(at.try_recv().is_none(), "one {status}");
        (resp, packet.raw.to_vec())
    }

    /// The §17.1.1.3 ACK of `resp`, a final to the INVITE on `branch`.
    async fn ack(&self, branch: &str, via_tail: &str, resp: &SipResponse) {
        let to = resp.raw(HeaderName::To).next().expect("the final carries To").to_string();
        self.send(&request("ACK", branch, via_tail, &to, "")).await;
    }
}

/// An INVITE and a copy of it, with no `rport`: each answer goes to the source
/// address at the sent-by port, the socket that sent hears none.
#[tokio::test(start_paused = true)]
async fn without_rport_a_refusal_and_its_repeat_reach_the_sent_by_port() {
    let r = rig("own-final-dest-sent-by").await;
    let branch = "z9hG4bK-sent-by";
    let invite = request("INVITE", branch, "", BOB, "");

    r.send(&invite).await;
    let (refusal, first) = r.answered(r.listen.as_ref(), 503);
    assert!(r.send.try_recv().is_none(), "nothing reaches the source port (§18.2.2)");

    r.send(&invite).await;
    let (_, repeat) = r.answered(r.listen.as_ref(), 503);
    assert_eq!(repeat, first, "a repeat is the first refusal's bytes");
    assert!(r.send.try_recv().is_none(), "nothing reaches the source port (§18.2.2)");

    r.ack(branch, "", &refusal).await;
    assert!(r.listen.try_recv().is_none(), "the ACK ends at the proxy");
    let _ = r.h.finish().await;
}

/// The finals of the §16.3 checks, on the one seam: no `rport`, the sent-by port.
#[tokio::test(start_paused = true)]
async fn without_rport_the_preflight_finals_reach_the_sent_by_port() {
    let r = rig("own-final-dest-preflight").await;
    let too_many = request("OPTIONS", "z9hG4bK-483", "", BOB, "")
        .replace("Max-Forwards: 70", "Max-Forwards: 0");
    r.send(&too_many).await;
    r.answered(r.listen.as_ref(), 483);

    r.send(&request("OPTIONS", "z9hG4bK-420", "", BOB, "Proxy-Require: bogus-extension-xyz\r\n"))
        .await;
    r.answered(r.listen.as_ref(), 420);

    assert!(r.send.try_recv().is_none(), "nothing reaches the source port (§18.2.2)");
    let _ = r.h.finish().await;
}

/// RFC 3581 §4: with `rport` every answer leaves to the source port.
#[tokio::test(start_paused = true)]
async fn with_rport_a_refusal_and_its_repeat_reach_the_source_port() {
    let r = rig("own-final-dest-rport").await;
    let branch = "z9hG4bK-rport";
    let invite = request("INVITE", branch, ";rport", BOB, "");

    r.send(&invite).await;
    let (refusal, first) = r.answered(r.send.as_ref(), 503);
    r.send(&invite).await;
    let (_, repeat) = r.answered(r.send.as_ref(), 503);
    assert_eq!(repeat, first, "a repeat is the first refusal's bytes");
    assert!(r.listen.try_recv().is_none(), "nothing reaches the sent-by port");

    r.ack(branch, ";rport", &refusal).await;
    let _ = r.h.finish().await;
}

/// A sent-by of another host: `received` at the sent-by port, and nothing
/// reaches the address the sender wrote.
#[tokio::test(start_paused = true)]
async fn a_sent_by_of_another_host_is_answered_at_received_and_its_port() {
    let r = rig("own-final-dest-received").await;
    let (elsewhere, _) = r.h.bind_sut("elsewhere", "192.0.2.44:5070").await;
    let branch = "z9hG4bK-received";
    let invite = request("INVITE", branch, "", BOB, "")
        .replace(&format!("Via: SIP/2.0/UDP {LISTEN}"), "Via: SIP/2.0/UDP 192.0.2.44:5070");

    r.send(&invite).await;
    let (refusal, _) = r.answered(r.listen.as_ref(), 503);
    assert_eq!(refusal.top_via().received(), Some("127.0.0.1"));
    assert!(elsewhere.try_recv().is_none(), "nothing reaches the sent-by host");
    assert!(r.send.try_recv().is_none(), "nothing reaches the source port");

    let to = refusal.raw(HeaderName::To).next().expect("To").to_string();
    r.send(
        &request("ACK", branch, "", &to, "")
            .replace(&format!("Via: SIP/2.0/UDP {LISTEN}"), "Via: SIP/2.0/UDP 192.0.2.44:5070"),
    )
    .await;
    let _ = r.h.finish().await;
}

/// A sent-by in the IPv4-mapped form names the IPv4 address, which an IPv4
/// socket reaches.
#[tokio::test(start_paused = true)]
async fn a_mapped_sent_by_reaches_its_ipv4_address() {
    let r = rig("own-final-dest-mapped").await;
    let branch = "z9hG4bK-mapped";
    let via = "Via: SIP/2.0/UDP [::ffff:127.0.0.1]:5070";
    let invite =
        request("INVITE", branch, "", BOB, "").replace(&format!("Via: SIP/2.0/UDP {LISTEN}"), via);

    r.send(&invite).await;
    let (refusal, _) = r.answered(r.listen.as_ref(), 503);
    assert_eq!(refusal.top_via().received(), None, "the sent-by is the source address");

    let to = refusal.raw(HeaderName::To).next().expect("To").to_string();
    r.send(
        &request("ACK", branch, "", &to, "").replace(&format!("Via: SIP/2.0/UDP {LISTEN}"), via),
    )
    .await;
    let _ = r.h.finish().await;
}

/// The 200 to the CANCEL of a refused INVITE (§9.2) takes the same seam. On a
/// bare network: only a caller breaking §9.1 CANCELs an INVITE no provisional
/// answered, and the RFC audit reads each socket of this peer as a UA of its own.
#[tokio::test(start_paused = true)]
async fn without_rport_the_200_to_a_cancel_reaches_the_sent_by_port() {
    let net = SimulatedSignalingNetwork::new(1);
    let bind = |at: &str| net.bind_udp(BindUdpOpts::new(at.parse().expect("an address"), 64));
    let endpoint = bind(PROXY).await.expect("bind the proxy");
    let send = bind(SEND).await.expect("bind the sender");
    let listen = bind(LISTEN).await.expect("bind the listener");
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(FailingSelect(|| SelectError::NoTarget { reason: "no worker alive".into() }));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let core = ProxyCoreBuilder::new(
        ProxyAddr::new("127.0.0.1", 5060),
        strategy,
        registry,
        Arc::new(IdGen::seeded(0x846)),
        sip_message::CustomParser::new(),
    )
    .clock(Clock::test_at(0))
    .build(endpoint);
    let task = tokio::spawn(core.run());
    let proxy = PROXY.parse().expect("an address");
    let branch = "z9hG4bK-cancel";

    for (raw, status) in [
        (request("INVITE", branch, "", BOB, ""), 503),
        (request("CANCEL", branch, "", BOB, ""), 200),
    ] {
        send.send_to(raw.as_bytes(), proxy).await.expect("sent");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let packet = listen.try_recv().unwrap_or_else(|| panic!("the {status} arrives"));
        let Ok(SipMessage::Response(resp)) = CustomParser::new().parse(&packet.raw) else {
            panic!("a response");
        };
        assert_eq!(resp.status(), status);
        assert!(send.try_recv().is_none(), "nothing reaches the source port");
    }
    task.abort();
}
