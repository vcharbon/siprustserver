use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::addr::ProxyAddr;
use crate::core::ProxyCoreBuilder;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::WorkerRegistry;
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::{ProxyMetrics, RoutingStrategy};

const UAC: &str = "10.244.7.13";
const W1: &str = "10.0.0.1";
const PROXY_VIP: &str = "192.0.2.250";

// RFC 3261 §16.11: a stateless proxy forwards every response by its Via, a
// 100 included — the §16.7 rule that absorbs a 100 binds a stateful proxy
// only. The relayed 100 loses the proxy's Via and reaches the next Via's
// address, counted as a forwarded message.
#[tokio::test]
async fn trying_100_is_relayed_upstream_by_its_via() {
    let ep = Arc::new(ByteCapturingEndpoint::default());
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
    let metrics = Arc::new(ProxyMetrics::new());
    let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(Clock::test_at(0))
        .metrics(metrics.clone())
        .build(Box::new(EpHandle(ep.clone())));

    let raw = format!(
        "SIP/2.0 100 Trying\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKout\r\n\
Via: SIP/2.0/UDP {UAC}:5070;branch=z9hG4bKin\r\n\
From: <sip:alice@{UAC}>;tag=t\r\n\
To: <sip:bob@10.0.0.50>\r\n\
Call-ID: t100-1@test\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    );
    let SipMessage::Response(resp) = CustomParser::default().parse(raw.as_bytes()).unwrap() else {
        panic!("expected response")
    };
    core.handle_response_from(resp, format!("{W1}:5060").parse().unwrap()).await;

    let sent = ep.sent.lock().unwrap();
    assert_eq!(sent.len(), 1, "the 100 must be relayed exactly once");
    let (dst, bytes) = &sent[0];
    assert_eq!(
        *dst,
        format!("{UAC}:5070").parse::<std::net::SocketAddr>().unwrap(),
        "the 100 goes to the next Via's address"
    );
    let relayed = String::from_utf8_lossy(bytes).to_string();
    assert!(relayed.starts_with("SIP/2.0 100 Trying\r\n"), "got: {relayed}");
    let vias: Vec<&str> =
        relayed.lines().filter(|l| l.to_ascii_lowercase().starts_with("via:")).collect();
    assert_eq!(vias.len(), 1, "the proxy's own Via is popped: {vias:?}");
    assert!(vias[0].contains("branch=z9hG4bKin"), "the upstream's Via stays: {vias:?}");
    let txt = metrics.prometheus_text();
    assert!(txt.contains("sip_messages_total{label=\"outbound:forwarded\"} 1\n"), "{txt}");
    assert!(!txt.contains("sip_messages_total{label=\"outbound:dropped\"} 1"), "{txt}");
}

/// Endpoint double capturing every sent datagram's destination + bytes.
#[derive(Default)]
struct ByteCapturingEndpoint {
    sent: std::sync::Mutex<Vec<(std::net::SocketAddr, Vec<u8>)>>,
}

#[async_trait::async_trait]
impl sip_net::UdpEndpoint for ByteCapturingEndpoint {
    async fn send_to(
        &self,
        buf: &[u8],
        dst: std::net::SocketAddr,
    ) -> Result<(), sip_net::SendError> {
        self.sent.lock().unwrap().push((dst, buf.to_vec()));
        Ok(())
    }
    async fn recv(&self) -> Option<sip_net::UdpPacket> {
        std::future::pending().await
    }
    fn try_recv(&self) -> Option<sip_net::UdpPacket> {
        None
    }
    fn local_addr(&self) -> std::net::SocketAddr {
        format!("{PROXY_VIP}:5060").parse().unwrap()
    }
    fn queue_depth(&self) -> usize {
        0
    }
    fn queue_max(&self) -> usize {
        0
    }
    fn counters(&self) -> sip_net::UdpEndpointCounters {
        sip_net::UdpEndpointCounters::default()
    }
}

// RFC 3261 §16.11 / ADR-0022 X4 — loss-recovery of a rejected call. The
// proxy must NOT synthesize the §17.1.1.3 hop ACK when relaying a non-2xx
// INVITE final: that would quench the downstream UAS's Timer G (the only
// retransmitter in the system) while the upstream relay stays
// exactly-once, so one lost relayed copy would wedge the caller until Timer B
// / the 32 s safety timer. Reliability is end-to-end instead — the
// upstream's own ACK is relayed downstream to the node the final arrived
// from, on the INVITE's outbound Via branch (so the UAS's server
// transaction matches it, §17.2.3), the caller's message otherwise
// verbatim (R-URI included — the downstream demux keys on it).
#[tokio::test]
async fn relayed_non_2xx_final_is_never_hop_acked_and_the_upstream_ack_relays() {
    let ep = Arc::new(ByteCapturingEndpoint::default());
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
    let metrics = Arc::new(ProxyMetrics::new());
    let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(Clock::test_at(0))
        .metrics(metrics.clone())
        .build(Box::new(EpHandle(ep.clone())));

    // A full callee-shaped R-URI: user-part + `;user=phone`, as a worker's
    // b-leg INVITE carries it through the LB.
    let invite_ruri = "sip:+155501113301000@uas.example:6001;user=phone";
    let raw_invite = format!(
        "INVITE {invite_ruri} SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKackuri;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:worker@{UAC}>;tag=w1\r\n\
To: <sip:+155501113301000@uas.example>\r\n\
Call-ID: ackuri-1@test\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    );
    let msg = CustomParser::default().parse(raw_invite.as_bytes()).unwrap();
    core.handle_request_from(msg, format!("{UAC}:5060").parse().unwrap()).await;

    // The branch the proxy stamped on its forwarded INVITE — the relayed
    // ACK must repeat it exactly.
    let forwarded_invite = {
        let sent = ep.sent.lock().unwrap();
        String::from_utf8_lossy(&sent[0].1).to_string()
    };
    let proxy_branch = forwarded_invite
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("via:"))
        .and_then(|l| l.split("branch=").nth(1))
        .map(|b| b.split(&[';', ',', ' '][..]).next().unwrap().to_string())
        .expect("forwarded INVITE must carry a proxy Via branch");
    ep.sent.lock().unwrap().clear();

    let raw_486 = format!(
        "SIP/2.0 486 Busy Here\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch={proxy_branch}\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKackuri\r\n\
From: <sip:worker@{UAC}>;tag=w1\r\n\
To: <sip:+155501113301000@uas.example>;tag=callee-1\r\n\
Call-ID: ackuri-1@test\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    );
    let SipMessage::Response(resp) = CustomParser::default().parse(raw_486.as_bytes()).unwrap()
    else {
        panic!("expected response")
    };
    core.handle_response_from(resp, format!("{W1}:5060").parse().unwrap()).await;
    {
        let sent = ep.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "the 486 relay must be the ONLY send — no synthesized ACK");
        let relayed = String::from_utf8_lossy(&sent[0].1).to_string();
        assert!(
            relayed.starts_with("SIP/2.0 486"),
            "expected the relayed 486, got: {}",
            relayed.lines().next().unwrap_or("")
        );
    }
    ep.sent.lock().unwrap().clear();

    // The upstream's own §17.1.1.3 ACK (same branch as its INVITE) is
    // relayed downstream on the INVITE's hop.
    let raw_ack = format!(
        "ACK {invite_ruri} SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKackuri;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:worker@{UAC}>;tag=w1\r\n\
To: <sip:+155501113301000@uas.example>;tag=callee-1\r\n\
Call-ID: ackuri-1@test\r\n\
CSeq: 1 ACK\r\n\
Content-Length: 0\r\n\r\n"
    );
    let ack_msg = CustomParser::default().parse(raw_ack.as_bytes()).unwrap();
    core.handle_request_from(ack_msg, format!("{UAC}:5060").parse().unwrap()).await;

    let sent = ep.sent.lock().unwrap();
    assert_eq!(sent.len(), 1, "the upstream's ACK must be relayed, not absorbed");
    let (dst, bytes) = &sent[0];
    assert_eq!(
        *dst,
        format!("{W1}:5060").parse::<std::net::SocketAddr>().unwrap(),
        "the ACK goes to the node the final came from (the INVITE's target here)"
    );
    let ack = String::from_utf8_lossy(bytes).to_string();
    assert_eq!(
        ack.lines().next().unwrap(),
        format!("ACK {invite_ruri} SIP/2.0"),
        "the relayed ACK must keep the upstream's Request-URI verbatim"
    );
    let top_via = ack.lines().find(|l| l.to_ascii_lowercase().starts_with("via:")).unwrap();
    assert!(
        top_via.contains(&format!("branch={proxy_branch}")),
        "the relayed ACK must reuse the INVITE's outbound branch (got: {top_via})"
    );
}

/// Thin `UdpEndpoint` wrapper so the shared `Arc<ByteCapturingEndpoint>` can
/// be both handed to the builder (boxed) and inspected by the test.
struct EpHandle(Arc<ByteCapturingEndpoint>);

#[async_trait::async_trait]
impl sip_net::UdpEndpoint for EpHandle {
    async fn send_to(
        &self,
        buf: &[u8],
        dst: std::net::SocketAddr,
    ) -> Result<(), sip_net::SendError> {
        self.0.send_to(buf, dst).await
    }
    async fn recv(&self) -> Option<sip_net::UdpPacket> {
        self.0.recv().await
    }
    fn try_recv(&self) -> Option<sip_net::UdpPacket> {
        self.0.try_recv()
    }
    fn local_addr(&self) -> std::net::SocketAddr {
        self.0.local_addr()
    }
    fn queue_depth(&self) -> usize {
        self.0.queue_depth()
    }
    fn queue_max(&self) -> usize {
        self.0.queue_max()
    }
    fn counters(&self) -> sip_net::UdpEndpointCounters {
        self.0.counters()
    }
}
