//! Request-path trace tier: what a traced call's span records, and when it
//! closes (ADR-0026).
//!
//! Three seams the per-packet emission alone cannot cover: a call that reaches
//! activation twice must record each datagram exactly once, a self-generated
//! final must show up as the `sip.out` it is, and a rejected call's span must
//! close at the ACK that ends it rather than at the 15-minute idle TTL.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use observe::{RateDraw, SampleAdmission, TokenBucket};
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{SendError, UdpEndpoint, UdpEndpointCounters, UdpPacket};

use crate::addr::ProxyAddr;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::WorkerRegistry;
use crate::self_gate::{AdmitDecision, ProxySelfGate};
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::trace::ProxyTraces;
use crate::RoutingStrategy;

const UAC: &str = "10.0.0.1";
const W1: &str = "10.0.0.9";
const PROXY: &str = "172.20.255.250";

/// Endpoint double keeping every datagram the proxy sent.
#[derive(Default)]
struct Sent(Mutex<Vec<Vec<u8>>>);

#[async_trait]
impl UdpEndpoint for Sent {
    async fn send_to(&self, buf: &[u8], _dst: std::net::SocketAddr) -> Result<(), SendError> {
        self.0.lock().expect("sent buffer").push(buf.to_vec());
        Ok(())
    }
    async fn recv(&self) -> Option<UdpPacket> {
        std::future::pending().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        None
    }
    fn local_addr(&self) -> std::net::SocketAddr {
        format!("{PROXY}:5060").parse().expect("fixture address")
    }
    fn queue_depth(&self) -> usize {
        0
    }
    fn queue_max(&self) -> usize {
        0
    }
    fn counters(&self) -> UdpEndpointCounters {
        UdpEndpointCounters::default()
    }
}

/// Boxable handle over the shared `Arc<Sent>`: the builder takes ownership, the
/// test keeps the handle.
struct SentHandle(Arc<Sent>);

#[async_trait]
impl UdpEndpoint for SentHandle {
    async fn send_to(&self, buf: &[u8], dst: std::net::SocketAddr) -> Result<(), SendError> {
        self.0.send_to(buf, dst).await
    }
    async fn recv(&self) -> Option<UdpPacket> {
        self.0.recv().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
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
    fn counters(&self) -> UdpEndpointCounters {
        self.0.counters()
    }
}

/// A gate that sheds every external new-dialog INVITE.
struct SheddingGate;

impl ProxySelfGate for SheddingGate {
    fn try_admit_external(&self) -> AdmitDecision {
        AdmitDecision {
            admit: false,
            reason: Some("proxy_overload_elu".to_string()),
            retry_after_sec: 7,
        }
    }
}

/// A trace gate that samples every call.
fn sample_everything() -> Arc<ProxyTraces> {
    Arc::new(ProxyTraces::new(
        SampleAdmission::new(true, 1.0, 200, RateDraw::seeded(1), TokenBucket::default_at(0)),
        false,
    ))
}

fn core_with(
    traces: Arc<ProxyTraces>,
    gate: Option<Arc<dyn ProxySelfGate>>,
) -> (ProxyCore, Arc<Sent>) {
    let ep = Arc::new(Sent::default());
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let mut builder = ProxyCoreBuilder::new(ProxyAddr::new(PROXY, 5060), strategy, registry)
        .clock(Clock::test_at(0))
        .traces(traces);
    if let Some(gate) = gate {
        builder = builder.self_gate(gate);
    }
    let core = builder.build(Box::new(SentHandle(ep.clone())));
    (core, ep)
}

fn invite(call_id: &str, branch: &str, cseq: u32) -> SipMessage {
    let raw = format!(
        "INVITE sip:bob@{W1}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch={branch};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=alice1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: {call_id}\r\n\
CSeq: {cseq} INVITE\r\n\
Contact: <sip:alice@{UAC}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    );
    CustomParser::default().parse(raw.as_bytes()).expect("fixture INVITE")
}

fn src() -> std::net::SocketAddr {
    format!("{UAC}:5060").parse().expect("fixture address")
}

/// The proxy's outbound Via branch on the datagram it just forwarded — what the
/// downstream's response must echo for the relay to match.
fn forwarded_branch(sent: &Arc<Sent>) -> String {
    let wire = {
        let sent = sent.0.lock().expect("sent buffer");
        String::from_utf8_lossy(sent.last().expect("a forwarded datagram")).to_string()
    };
    wire.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("via:"))
        .and_then(|l| l.split("branch=").nth(1))
        .map(|b| b.split(&[';', ',', ' '][..]).next().unwrap_or_default().to_string())
        .expect("the forwarded request carries a proxy Via branch")
}

// Regression: `activate` reported success for a call that ALREADY had a span,
// so the second INVITE of a digest-auth retry (and a retransmit whose memo has
// been evicted) was recorded twice — once by the per-packet seam on the map hit
// and once again by the activation that thought it had opened the span.
#[tokio::test]
async fn a_call_that_reaches_activation_twice_records_each_datagram_once() {
    let (_guard, log) = observe::test_buffer();
    let traces = sample_everything();
    let (core, _ep) = core_with(traces.clone(), None);

    // The 401-challenged INVITE and the credentialed retry: same Call-ID and
    // From-tag, fresh branch and CSeq, so nothing about it is a retransmission.
    core.handle_request(invite("dup@10.0.0.1", "z9hG4bK-first", 1), src()).await;
    core.handle_request(invite("dup@10.0.0.1", "z9hG4bK-retry", 2), src()).await;

    assert_eq!(traces.active(), 1, "one root span per call, however many INVITEs it takes");
    let sip_in = log.matching("kind=sip.in");
    assert_eq!(
        sip_in.len(),
        2,
        "exactly one record per datagram: {:?}",
        sip_in.iter().map(|e| e.line()).collect::<Vec<_>>(),
    );
    assert!(sip_in.iter().any(|e| e.contains("CSeq: 1 INVITE")));
    assert!(sip_in.iter().any(|e| e.contains("CSeq: 2 INVITE")));

    // Terminate: the caller's ACK to the proxy-generated final is absorbed here
    // (no downstream exists), and the span is already closed by the reject.
    traces.close("dup@10.0.0.1");
    assert_eq!(traces.active(), 0);
}

// The shed 503 is synthesized by `reply` and never reaches a downstream hop, so
// `reply` is the only place it can be recorded. Without it a traced shed shows
// an INVITE arriving, a reason, and nothing leaving.
#[tokio::test]
async fn a_shed_call_records_the_503_it_was_answered_with() {
    let (_guard, log) = observe::test_buffer();
    let traces = sample_everything();
    let (core, ep) = core_with(traces.clone(), Some(Arc::new(SheddingGate)));

    core.handle_request(invite("shed@10.0.0.1", "z9hG4bK-shed", 1), src()).await;

    assert!(
        log.matching("kind=sip.in").iter().any(|e| e.contains("INVITE sip:")),
        "the INVITE as it arrived",
    );
    assert!(
        log.matching("kind=route.shed").iter().any(|e| e.contains("proxy_overload_elu")),
        "the reason nothing was forwarded",
    );
    let out = log.matching("kind=sip.out");
    assert_eq!(out.len(), 1, "the synthesized final is the call's only datagram out");
    assert!(out[0].contains("503 Service Unavailable"), "with its status line");
    assert!(out[0].contains("Retry-After: 7"), "and the hints the caller acts on");
    assert!(out[0].contains("proxy_overload_elu"), "and the Reason that explains it");

    assert_eq!(ep.0.lock().expect("sent buffer").len(), 1, "nothing was forwarded downstream");
    assert_eq!(traces.active(), 0, "a refused initial INVITE closes the span at once");
    assert!(!traces.any_sampled());
}

// Only a BYE final used to close a span, so a 486-rejected call held its root
// span, its active-cap slot and the process-wide sampled flag for the full idle
// TTL — keeping every shard on the map-lookup path for a call that was over.
// The ACK relayed on the remembered non-2xx final is the last fact this hop
// sees; the CANCEL flow reaches the same seam through its 487's ACK.
#[tokio::test]
async fn a_rejected_call_closes_its_span_when_the_ack_relays() {
    let traces = sample_everything();
    let (core, ep) = core_with(traces.clone(), None);
    const CALL_ID: &str = "reject@10.0.0.1";

    core.handle_request(invite(CALL_ID, "z9hG4bK-rej", 1), src()).await;
    assert_eq!(traces.active(), 1);
    let proxy_branch = forwarded_branch(&ep);

    let raw_486 = format!(
        "SIP/2.0 486 Busy Here\r\n\
Via: SIP/2.0/UDP {PROXY}:5060;branch={proxy_branch}\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bK-rej\r\n\
From: <sip:alice@{UAC}>;tag=alice1\r\n\
To: <sip:bob@example.com>;tag=bob1\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    );
    let SipMessage::Response(resp) = CustomParser::default().parse(raw_486.as_bytes()).expect("fixture 486")
    else {
        panic!("expected a response")
    };
    core.handle_response(resp).await;
    assert_eq!(traces.active(), 1, "the call is not over until its ACK is on the wire");

    let raw_ack = format!(
        "ACK sip:bob@{W1}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bK-rej;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=alice1\r\n\
To: <sip:bob@example.com>;tag=bob1\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 ACK\r\n\
Content-Length: 0\r\n\r\n"
    );
    let ack = CustomParser::default().parse(raw_ack.as_bytes()).expect("fixture ACK");
    core.handle_request(ack, src()).await;

    assert_eq!(traces.active(), 0, "the relayed ACK ends the rejected transaction at this hop");
    assert!(!traces.any_sampled(), "and the process-wide flag drops with the last span");
}
