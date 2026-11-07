//! Dual-face (multi-homed edge proxy) core unit tests — the per-message
//! contracts of the RFC 3261 §16 two-plane split:
//!
//!   external plane (callers)  ⇄  proxy  ⇄  internal plane (workers)
//!
//! Each test drives `route_request`/`handle_response` directly over two
//! capturing endpoints (one per face) and asserts WHICH socket egressed, WHAT
//! advertise was stamped (Via / Record-Route per face), and that in-dialog
//! requests pop BOTH self-Route halves. The end-to-end flows (real workers,
//! recorded-trace RFC audit) live in `failover-harness/tests/it/dual_face.rs`;
//! the single-face guard is the rest of the sip-proxy suite plus
//! `single_face_rr_format_is_unchanged` below.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{SendError, UdpEndpoint, UdpEndpointCounters, UdpPacket};

use crate::addr::ProxyAddr;
use crate::core::{ExternalFaceParts, ProxyCore, ProxyCoreBuilder};
use crate::observability::metrics::{Face, RoutingDecisionKind};
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerRegistry};
use crate::resolver::HostResolver;
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::{ProxyMetrics, RoutingStrategy};

// Internal plane (the workers).
pub(super) const INT_VIP: &str = "10.244.255.250";
pub(super) const INT_PORT: u16 = 5080;
pub(super) const W1: &str = "10.244.5.8";
// External plane (callers/callees).
pub(super) const EXT_VIP: &str = "192.168.60.250";
pub(super) const EXT_PORT: u16 = 5060;
pub(super) const CALLER: &str = "192.168.60.10";
pub(super) const CALLEE: &str = "192.168.60.20";

/// One captured egress datagram: which FACE sent it, to where, the bytes.
pub(super) type Sent = (&'static str, SocketAddr, Vec<u8>);

/// A capturing endpoint labelled with its face. A datagram past the largest
/// UDP payload is refused, as a socket refuses it, and not captured.
struct TapEndpoint {
    face: &'static str,
    addr: SocketAddr,
    sent: Arc<Mutex<Vec<Sent>>>,
}

#[async_trait]
impl UdpEndpoint for TapEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        if buf.len() > sip_net::MAX_UDP_PAYLOAD {
            return Err(SendError {
                message: format!("datagram of {} bytes", buf.len()),
                kind: sip_net::SendErrorKind::MessageTooLong,
            });
        }
        self.sent.lock().unwrap().push((self.face, dst, buf.to_vec()));
        Ok(())
    }
    async fn recv(&self) -> Option<UdpPacket> {
        std::future::pending().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        None
    }
    fn local_addr(&self) -> SocketAddr {
        self.addr
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

pub(super) struct Fixture {
    pub(super) core: ProxyCore,
    sent: Arc<Mutex<Vec<Sent>>>,
    metrics: Arc<ProxyMetrics>,
}

impl Fixture {
    /// Drain the captured sends.
    pub(super) fn take_sent(&self) -> Vec<Sent> {
        std::mem::take(&mut *self.sent.lock().unwrap())
    }

    pub(super) fn metrics(&self) -> &ProxyMetrics {
        &self.metrics
    }
}

pub(super) fn dual_face_core() -> Fixture {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(W1, 5060),
        )]));
    dual_face_core_over(reg, None)
}

/// A dual-face core over `reg` routing by `strategy`, resolving names through
/// `resolver` when one is given.
pub(super) fn dual_face_core_routing(
    reg: Arc<dyn WorkerRegistry>,
    strategy: Arc<dyn RoutingStrategy>,
    resolver: Option<Arc<dyn HostResolver>>,
) -> Fixture {
    let sent: Arc<Mutex<Vec<Sent>>> = Arc::new(Mutex::new(Vec::new()));
    let int_ep = TapEndpoint {
        face: "int",
        addr: format!("{INT_VIP}:{INT_PORT}").parse().unwrap(),
        sent: sent.clone(),
    };
    let ext_ep = TapEndpoint {
        face: "ext",
        addr: format!("{EXT_VIP}:{EXT_PORT}").parse().unwrap(),
        sent: sent.clone(),
    };
    let metrics = Arc::new(ProxyMetrics::new());
    let mut builder = ProxyCoreBuilder::for_test(ProxyAddr::new(INT_VIP, INT_PORT), strategy, reg)
        .clock(Clock::test_at(0))
        .metrics(metrics.clone())
        .external_face(ExternalFaceParts {
            endpoint: Box::new(ext_ep),
            advertised: ProxyAddr::new(EXT_VIP, EXT_PORT),
        });
    if let Some(resolver) = resolver {
        builder = builder.resolver(resolver);
    }
    let core = builder.build(Box::new(int_ep));
    Fixture { core, sent, metrics }
}

/// A dual-face core over `reg`, forwarding every new dialog to `W1`, resolving
/// names through `resolver` when one is given.
pub(super) fn dual_face_core_over(
    reg: Arc<dyn WorkerRegistry>,
    resolver: Option<Arc<dyn HostResolver>>,
) -> Fixture {
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
    dual_face_core_routing(reg, strategy, resolver)
}

pub(super) fn parse_msg(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

pub(super) fn caller_src() -> SocketAddr {
    format!("{CALLER}:5060").parse().unwrap()
}

fn callee_src() -> SocketAddr {
    format!("{CALLEE}:5060").parse().unwrap()
}

/// Header lines named `name` (case-insensitive), in wire order.
pub(super) fn header_lines(bytes: &[u8], name: &str) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes).to_string();
    let prefix = format!("{}:", name.to_ascii_lowercase());
    text.lines()
        .filter(|l| l.to_ascii_lowercase().starts_with(&prefix))
        .map(|l| l.split_once(':').unwrap().1.trim().to_string())
        .collect()
}

/// An initial INVITE from the external caller toward the callee's AOR.
pub(super) fn caller_invite(call_id: &str) -> SipMessage {
    parse_msg(&format!(
        "INVITE sip:bob@{CALLEE}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {CALLER}:5060;branch=z9hG4bKcaller1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{CALLER}>;tag=tag-a\r\n\
To: <sip:bob@{CALLEE}>\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{CALLER}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// A worker-originated b-leg INVITE toward the external callee (top Via = the
/// worker's registry identity; the plain preloaded `;lr` Route the b2bua sends).
pub(super) fn worker_bleg_invite(call_id: &str) -> SipMessage {
    parse_msg(&format!(
        "INVITE sip:bob@{CALLEE}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKbleg1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:bob@{CALLEE}>\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:b2bua@{W1}:5060;leg=b>\r\n\
Route: <sip:{INT_VIP}:{INT_PORT};lr>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

// ── (a) the egress face + (3) per-face Via ───────────────────────────────────

#[tokio::test]
async fn inbound_invite_egresses_internal_with_internal_via_and_per_face_rr() {
    let f = dual_face_core();
    let outcome =
        f.core.route_request(&caller_invite("df-in@test"), caller_src(), Face::External).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::SelectNew);
    assert_eq!(outcome.target, Some(ProxyAddr::new(W1, 5060)));

    let sent = f.take_sent();
    assert_eq!(sent.len(), 1, "exactly one forward");
    let (face, dst, bytes) = &sent[0];
    assert_eq!(*face, "int", "a worker-bound forward must egress the INTERNAL socket");
    assert_eq!(*dst, format!("{W1}:5060").parse::<SocketAddr>().unwrap());

    // §18.1.1: the pushed Via carries the EGRESS (internal) face's advertise.
    let vias = header_lines(bytes, "Via");
    assert!(
        vias[0].contains(&format!("{INT_VIP}:{INT_PORT}")),
        "top Via must be the internal-face advertise, got {}",
        vias[0],
    );

    // §16.6 double-RR, per-face: topmost (facing the worker) = INTERNAL face
    // with the cookie AND the `;outbound` direction marker; below it the
    // caller-facing entry = EXTERNAL face with the cookie.
    let rrs = header_lines(bytes, "Record-Route");
    assert_eq!(rrs.len(), 2, "double record-route, got {rrs:?}");
    assert!(
        rrs[0].contains(&format!("{INT_VIP}:{INT_PORT}")),
        "top RR faces the worker: {}",
        rrs[0]
    );
    assert!(
        rrs[0].contains(";outbound"),
        "worker-facing RR keeps the direction marker: {}",
        rrs[0]
    );
    assert!(
        rrs[0].contains(&format!("target={W1}:5060")),
        "cookie rides the worker-facing RR too: {}",
        rrs[0]
    );
    assert!(
        rrs[1].contains(&format!("{EXT_VIP}:{EXT_PORT}")),
        "lower RR faces the caller: {}",
        rrs[1]
    );
    assert!(
        rrs[1].contains(&format!("target={W1}:5060")),
        "cookie on the caller-facing RR: {}",
        rrs[1]
    );
    assert!(!rrs[1].contains(";outbound"), "caller-facing RR is NOT the outbound half: {}", rrs[1]);

    // Per-face egress metric.
    use crate::observability::metrics::Direction;
    assert_eq!(f.metrics.face_messages_total(Face::Internal, Direction::Outbound), 1);
    assert_eq!(f.metrics.face_messages_total(Face::External, Direction::Outbound), 0);
}

#[tokio::test]
async fn worker_outbound_invite_egresses_external_with_external_via_and_mirrored_rr() {
    let f = dual_face_core();
    let src = format!("{W1}:5060").parse().unwrap();
    let outcome =
        f.core.route_request(&worker_bleg_invite("df-out@test"), src, Face::Internal).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(outcome.target, Some(ProxyAddr::new(CALLEE, 5060)));

    let sent = f.take_sent();
    assert_eq!(sent.len(), 1);
    let (face, dst, bytes) = &sent[0];
    assert_eq!(*face, "ext", "a callee-bound forward must egress the EXTERNAL socket");
    assert_eq!(*dst, format!("{CALLEE}:5060").parse::<SocketAddr>().unwrap());

    let vias = header_lines(bytes, "Via");
    assert!(
        vias[0].contains(&format!("{EXT_VIP}:{EXT_PORT}")),
        "top Via must be the external-face advertise, got {}",
        vias[0],
    );

    // Mirrored order: topmost (facing the callee) = EXTERNAL cookie entry;
    // below it the worker-facing INTERNAL `;outbound` entry (cookie encoded
    // for the ORIGINATING worker's identity).
    let rrs = header_lines(bytes, "Record-Route");
    assert_eq!(rrs.len(), 2, "double record-route, got {rrs:?}");
    assert!(
        rrs[0].contains(&format!("{EXT_VIP}:{EXT_PORT}")),
        "top RR faces the callee: {}",
        rrs[0]
    );
    assert!(!rrs[0].contains(";outbound"), "callee-facing RR is the cookie half: {}", rrs[0]);
    assert!(
        rrs[0].contains(&format!("target={W1}:5060")),
        "cookie pins the originating worker: {}",
        rrs[0]
    );
    assert!(
        rrs[1].contains(&format!("{INT_VIP}:{INT_PORT}")),
        "lower RR faces the worker: {}",
        rrs[1]
    );
    assert!(rrs[1].contains(";outbound"), "worker-facing RR keeps the marker: {}", rrs[1]);
    assert!(
        rrs[1].contains(&format!("target={W1}:5060")),
        "cookie rides the worker-facing RR too: {}",
        rrs[1]
    );
}

// ── (a) a response's egress, both directions ────────────────────────────────

#[tokio::test]
async fn responses_route_by_next_via_face_in_both_directions() {
    let f = dual_face_core();

    // a-leg response: proxy's INTERNAL Via on top (the INVITE egressed toward
    // the worker), next Via = the external caller → relays on the EXT socket.
    let raw = format!(
        "SIP/2.0 180 Ringing\r\n\
Via: SIP/2.0/UDP {INT_VIP}:{INT_PORT};branch=z9hG4bKout1\r\n\
Via: SIP/2.0/UDP {CALLER}:5060;branch=z9hG4bKcaller1\r\n\
From: <sip:alice@{CALLER}>;tag=tag-a\r\n\
To: <sip:bob@{CALLEE}>;tag=w-1\r\n\
Call-ID: df-resp@test\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    );
    let SipMessage::Response(resp) = parse_msg(&raw) else { panic!("response") };
    f.core.handle_response(resp, format!("{W1}:5060").parse().unwrap(), Face::Internal).await;
    let sent = f.take_sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "ext", "a caller-bound response must egress the EXTERNAL socket");
    assert_eq!(sent[0].1, caller_src());

    // b-leg response: the callee's answer arrives on the EXTERNAL face, the
    // proxy's EXTERNAL Via on top (the b-leg INVITE egressed toward the
    // callee), next Via = the worker → relays on the INT socket.
    // The external-face top Via must be accepted as "us" (§16.7.3).
    let raw = format!(
        "SIP/2.0 200 OK\r\nContact: <sip:fixture@192.0.2.98>\r\n\
Via: SIP/2.0/UDP {EXT_VIP}:{EXT_PORT};branch=z9hG4bKout2\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKbleg1\r\n\
From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:bob@{CALLEE}>;tag=b-1\r\n\
Call-ID: df-resp@test\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    );
    let SipMessage::Response(resp) = parse_msg(&raw) else { panic!("response") };
    f.core.handle_response(resp, callee_src(), Face::External).await;
    let sent = f.take_sent();
    assert_eq!(sent.len(), 1, "the external-face top Via must be recognised as ours");
    assert_eq!(sent[0].0, "int", "a worker-bound response must egress the INTERNAL socket");
    assert_eq!(sent[0].1, format!("{W1}:5060").parse::<SocketAddr>().unwrap());
}

// ── (d) the relayed non-2xx hop ACK stays on the INVITE's face ──────────────

#[tokio::test]
async fn relayed_non_2xx_ack_follows_the_invite_face() {
    let f = dual_face_core();
    f.core.route_request(&caller_invite("df-ack@test"), caller_src(), Face::External).await;
    let fwd_invite = f.take_sent();
    let invite_via = header_lines(&fwd_invite[0].2, "Via");
    let proxy_branch = invite_via[0]
        .split("branch=")
        .nth(1)
        .map(|b| b.split(&[';', ',', ' '][..]).next().unwrap().to_string())
        .expect("forwarded INVITE must carry a proxy Via branch");

    let raw = format!(
        "SIP/2.0 486 Busy Here\r\n\
Via: SIP/2.0/UDP {INT_VIP}:{INT_PORT};branch={proxy_branch}\r\n\
Via: SIP/2.0/UDP {CALLER}:5060;branch=z9hG4bKcaller1\r\n\
From: <sip:alice@{CALLER}>;tag=tag-a\r\n\
To: <sip:bob@{CALLEE}>;tag=w-1\r\n\
Call-ID: df-ack@test\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    );
    let SipMessage::Response(resp) = parse_msg(&raw) else { panic!("response") };
    f.core.handle_response(resp, format!("{W1}:5060").parse().unwrap(), Face::Internal).await;

    let sent = f.take_sent();
    // No synthesized hop ACK (ADR-0022 X4: the proxy is transaction-less; the
    // worker's Timer G + the caller's own relayed ACK are the reliability
    // layer) — the 486 relay is the only send, to the caller's external face.
    assert_eq!(
        sent.len(),
        1,
        "486 relay must be the only send (no synthesized ACK), got {}",
        sent.len()
    );
    assert_eq!(sent[0].0, "ext");
    assert_eq!(sent[0].1, caller_src());

    // The caller's own §17.1.1.3 ACK (same branch as its INVITE) relays on
    // the INVITE's (internal) hop, its proxy Via stamped with THAT face's
    // advertise AND the INVITE's outbound branch so the UAS correlates it.
    let raw_ack = format!(
        "ACK sip:bob@{CALLEE}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {CALLER}:5060;branch=z9hG4bKcaller1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{CALLER}>;tag=tag-a\r\n\
To: <sip:bob@{CALLEE}>;tag=w-1\r\n\
Call-ID: df-ack@test\r\n\
CSeq: 1 ACK\r\n\
Content-Length: 0\r\n\r\n"
    );
    let outcome = f.core.route_request(&parse_msg(&raw_ack), caller_src(), Face::External).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::AckHop);

    let sent = f.take_sent();
    assert_eq!(sent.len(), 1);
    let (ack_face, ack_dst, ack_bytes) = &sent[0];
    assert_eq!(
        *ack_face, "int",
        "the relayed ACK must egress toward the worker on the internal face"
    );
    assert_eq!(*ack_dst, format!("{W1}:5060").parse::<SocketAddr>().unwrap());
    let text = String::from_utf8_lossy(ack_bytes);
    assert!(
        text.starts_with("ACK "),
        "expected an ACK, got: {}",
        text.lines().next().unwrap_or("")
    );
    let vias = header_lines(ack_bytes, "Via");
    assert!(
        vias[0].contains(&format!("{INT_VIP}:{INT_PORT}"))
            && vias[0].contains(&format!("branch={proxy_branch}")),
        "the relayed ACK's Via must carry the internal-face advertise + the INVITE's branch: {}",
        vias[0],
    );
}

// ── (c) in-dialog requests pop BOTH self-Route halves, both directions ─────

#[tokio::test]
async fn callee_in_dialog_bye_pops_both_faces_routes_and_decodes_to_worker() {
    let f = dual_face_core();
    // The callee's route set (UAS order, §12.1.1): cookie/EXT on top, then
    // outbound/INT — both are the proxy's own entries and must BOTH pop.
    let raw = format!(
        "BYE sip:b2bua@{W1}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {CALLEE}:5060;branch=z9hG4bKbye1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:bob@{CALLEE}>;tag=b-1\r\n\
To: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
Call-ID: df-bye@test\r\n\
CSeq: 2 BYE\r\n\
Route: <sip:{EXT_VIP}:{EXT_PORT};target={W1}:5060;lr>\r\n\
Route: <sip:{INT_VIP}:{INT_PORT};target={W1}:5060;outbound;lr>\r\n\
Content-Length: 0\r\n\r\n"
    );
    let outcome = f
        .core
        .route_request(&parse_msg(&raw), format!("{CALLEE}:5060").parse().unwrap(), Face::External)
        .await;
    assert_eq!(
        outcome.decision,
        RoutingDecisionKind::DecodeForward,
        "top (cookie) route decides the direction; the outbound half below must not flip it"
    );
    assert_eq!(outcome.target, Some(ProxyAddr::new(W1, 5060)));

    let sent = f.take_sent();
    assert_eq!(sent.len(), 1);
    let (face, _dst, bytes) = &sent[0];
    assert_eq!(*face, "int");
    let routes = header_lines(bytes, "Route");
    assert!(
        routes.iter().all(|r| !r.contains(EXT_VIP) && !r.contains(INT_VIP)),
        "BOTH self-Route halves must be popped before forwarding, got {routes:?}",
    );
}

#[tokio::test]
async fn worker_in_dialog_keepalive_pops_both_faces_routes_and_goes_external() {
    let f = dual_face_core();
    // The worker's route set (UAC order, §12.1.2 reversed): outbound/INT on
    // top, then cookie/EXT.
    let raw = format!(
        "OPTIONS sip:alice@{CALLER}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKka1;lg=a;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:alice@{CALLER}>;tag=tag-a\r\n\
Call-ID: df-ka@test\r\n\
CSeq: 2 OPTIONS\r\n\
Route: <sip:{INT_VIP}:{INT_PORT};target={W1}:5060;outbound;lr>\r\n\
Route: <sip:{EXT_VIP}:{EXT_PORT};target={W1}:5060;lr>\r\n\
Content-Length: 0\r\n\r\n"
    );
    let outcome = f
        .core
        .route_request(&parse_msg(&raw), format!("{W1}:5060").parse().unwrap(), Face::Internal)
        .await;
    assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(outcome.target, Some(ProxyAddr::new(CALLER, 5060)));

    let sent = f.take_sent();
    assert_eq!(sent.len(), 1);
    let (face, _dst, bytes) = &sent[0];
    assert_eq!(*face, "ext", "the keepalive must reach the caller on the external face");
    let routes = header_lines(bytes, "Route");
    assert!(
        routes.iter().all(|r| !r.contains(EXT_VIP) && !r.contains(INT_VIP)),
        "BOTH self-Route halves must be popped before forwarding, got {routes:?}",
    );
}

// ── (g) single-face regression guard: RR bytes unchanged ────────────────────

#[tokio::test]
async fn single_face_rr_format_is_unchanged() {
    // The SAME flow with NO external face: the single-face double-RR — both
    // entries at the one advertise, the outbound half param-less. Dual-face
    // plumbing must be inert here (the rest of the suite is the wider
    // guard; this pins the exact wire shape).
    let sent: Arc<Mutex<Vec<Sent>>> = Arc::new(Mutex::new(Vec::new()));
    let int_ep = TapEndpoint {
        face: "int",
        addr: format!("{INT_VIP}:{INT_PORT}").parse().unwrap(),
        sent: sent.clone(),
    };
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(W1, 5060),
        )]));
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
    let core = ProxyCoreBuilder::for_test(ProxyAddr::new(INT_VIP, INT_PORT), strategy, reg)
        .clock(Clock::test_at(0))
        .build(Box::new(int_ep));

    core.route_request_from(&caller_invite("sf-rr@test"), caller_src()).await;
    let sent = std::mem::take(&mut *sent.lock().unwrap());
    assert_eq!(sent.len(), 1);
    let rrs = header_lines(&sent[0].2, "Record-Route");
    assert_eq!(
        rrs,
        vec![
            format!("<sip:{INT_VIP}:{INT_PORT};outbound;lr>"),
            format!("<sip:{INT_VIP}:{INT_PORT};target={W1}:5060;lr>"),
        ],
        "single-face RR wire shape must be byte-identical to the pre-dual-face proxy",
    );
}

// ── §17.1.1.2: the non-2xx ACK of a worker's INVITE goes where the INVITE went ─

const CALLEE_SEND_PORT: u16 = 40000;

/// A callee's 486 to the b-leg INVITE the proxy forwarded under `proxy_branch`.
fn callee_486(call_id: &str, proxy_branch: &str) -> SipMessage {
    parse_msg(&format!(
        "SIP/2.0 486 Busy Here\r\n\
Via: SIP/2.0/UDP {EXT_VIP}:{EXT_PORT};branch={proxy_branch}\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKbleg1;rport\r\n\
From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:bob@{CALLEE}>;tag=b-1\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// The worker's §17.1.1.3 ACK of that 486: the INVITE's Request-URI, Route,
/// branch and CSeq number, the final's To.
fn worker_ack_of_the_486(call_id: &str) -> SipMessage {
    parse_msg(&format!(
        "ACK sip:bob@{CALLEE}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKbleg1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:bob@{CALLEE}>;tag=b-1\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 ACK\r\n\
Route: <sip:{INT_VIP}:{INT_PORT};lr>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

fn top_via_branch_of(bytes: &[u8]) -> String {
    let via = header_lines(bytes, "Via");
    via[0]
        .split("branch=")
        .nth(1)
        .and_then(|b| b.split(&[';', ',', ' '][..]).next())
        .expect("a branch")
        .to_string()
}

/// One instance forwards the INVITE; the callee's 486 comes back to it from a
/// send socket; the worker's ACK then crosses `ack_instance`, or the same
/// instance. Returns the ACK's destination and the branches of INVITE and ACK.
async fn ack_of_a_final_from_another_port(
    call_id: &str,
    through_another_instance: bool,
) -> (RoutingDecisionKind, Option<ProxyAddr>, SocketAddr, String, String) {
    let worker: SocketAddr = format!("{W1}:5060").parse().unwrap();
    let callee_send: SocketAddr = format!("{CALLEE}:{CALLEE_SEND_PORT}").parse().unwrap();
    let f = dual_face_core();
    let outcome = f.core.route_request(&worker_bleg_invite(call_id), worker, Face::Internal).await;
    assert_eq!(outcome.target, Some(ProxyAddr::new(CALLEE, 5060)), "the INVITE's destination");
    let forwarded = f.take_sent();
    assert_eq!(forwarded[0].1, callee_src());
    let invite_branch = top_via_branch_of(&forwarded[0].2);

    let SipMessage::Response(final_486) = callee_486(call_id, &invite_branch) else {
        panic!("response")
    };
    f.core.handle_response(final_486, callee_send, Face::External).await;
    let relayed = f.take_sent();
    assert_eq!(relayed.len(), 1, "the 486 relays to the worker");
    assert_eq!(relayed[0].1, worker);

    let other = dual_face_core();
    let instance = if through_another_instance { &other } else { &f };
    let ack =
        instance.core.route_request(&worker_ack_of_the_486(call_id), worker, Face::Internal).await;
    let sent = instance.take_sent();
    assert_eq!(sent.len(), 1, "the ACK is forwarded: {:?}", ack.decision);
    let ack_branch = top_via_branch_of(&sent[0].2);
    (ack.decision, ack.target, sent[0].1, invite_branch, ack_branch)
}

/// RFC 3261 §17.1.1.2: "The ACK MUST be sent to the same address, port, and
/// transport to which the original request was sent."
#[tokio::test]
async fn worker_non_2xx_ack_goes_to_the_invites_destination_not_the_finals_source() {
    let (decision, target, dst, invite_branch, ack_branch) =
        ack_of_a_final_from_another_port("df-ack-port@test", false).await;
    assert_eq!(invite_branch, ack_branch, "§16.11: the ACK rides the INVITE's outbound branch");
    assert_eq!(
        (target, dst),
        (Some(ProxyAddr::new(CALLEE, 5060)), callee_src()),
        "the ACK follows the INVITE to port 5060, decided {decision:?}"
    );
}

/// The instance without the memo decides from the ACK alone: its Route and
/// Request-URI (ADR-0009 X7).
#[tokio::test]
async fn the_instance_without_the_memo_sends_the_workers_ack_to_the_invites_destination() {
    let (decision, target, dst, invite_branch, ack_branch) =
        ack_of_a_final_from_another_port("df-ack-miss@test", true).await;
    assert_eq!(invite_branch, ack_branch, "§16.11: the ACK rides the INVITE's outbound branch");
    assert_eq!(
        (target, dst),
        (Some(ProxyAddr::new(CALLEE, 5060)), callee_src()),
        "the ACK follows the INVITE to port 5060, decided {decision:?}"
    );
    assert_eq!(decision, RoutingDecisionKind::WorkerOutbound);
}

/// `msg` carrying a header line past ADR-0001's first-ingress bound.
fn past_the_bound(msg: &SipMessage) -> SipMessage {
    let long = format!("X-Long: {}\r\nContent-Length:", "a".repeat(9_000));
    let text = String::from_utf8_lossy(msg.image()).replace("Content-Length:", &long);
    parse_msg(&text)
}

/// A party's new request past the bound is answered on the external face at
/// first ingress, and nothing reaches the internal plane.
#[tokio::test]
async fn a_new_request_past_the_bound_is_answered_on_the_external_face() {
    let fx = dual_face_core();
    fx.core
        .handle_request(past_the_bound(&caller_invite("past-ext")), caller_src(), Face::External)
        .await;
    let sent = fx.take_sent();
    let [(face, dst, bytes)] = sent.as_slice() else { panic!("one answer: {sent:?}") };
    assert_eq!((*face, *dst), ("ext", caller_src()));
    assert!(bytes.starts_with(b"SIP/2.0 400 "), "{}", String::from_utf8_lossy(bytes));
}

/// A worker's request arriving on the internal face reads with room: what it
/// relayed from a caller, grown by the hops, is forwarded.
#[tokio::test]
async fn a_workers_request_past_the_bound_is_forwarded_on_the_internal_face() {
    let fx = dual_face_core();
    let worker: SocketAddr = format!("{W1}:5070").parse().expect("an address");
    fx.core
        .handle_request(past_the_bound(&worker_bleg_invite("past-int")), worker, Face::Internal)
        .await;
    let sent = fx.take_sent();
    let [(face, _, bytes)] = sent.as_slice() else { panic!("one forward: {sent:?}") };
    assert_eq!(*face, "ext");
    assert!(bytes.starts_with(b"INVITE "), "{}", String::from_utf8_lossy(bytes));
}
