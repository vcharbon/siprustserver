//! The trust boundary is the ingress face. A request arriving on the external
//! face never claims the internal plane: its `;outbound` Route marker, a
//! worker's Via sent-by and a foreign loose Route never make the proxy relay it
//! to a destination the sender chose. The internal face keeps worker-outbound,
//! including for a worker on a pod address the registry does not hold. A
//! response arriving on the external face is relayed only to a worker.
//!
//! The core runs its real recv loop on the simulated fabric, so each datagram
//! arrives on the face whose socket it was sent to.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_net::types::BindUdpOpts;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork, UdpEndpoint};
use sip_proxy::observability::metrics::UntrustedClaim;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::self_gate::{AdmitDecision, BypassKind, ProxySelfGate, ShedReason};
use sip_proxy::{
    ExternalFaceParts, FaceCidrs, ForwardAllStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics,
    RoutingStrategy,
};

const INT_CIDRS: &str = "10.244.0.0/16";
const INT_VIP: &str = "10.244.255.250";
const INT_PORT: u16 = 5080;
const EXT_VIP: &str = "192.168.60.250";
const EXT_PORT: u16 = 5060;
/// The registry's one worker, and the only target the strategy selects.
const W1: &str = "10.244.5.8";
/// A worker on a pod address the registry does not hold.
const UNLISTED_WORKER: &str = "10.244.9.99";
/// A sender on the external plane: not a worker, not internal.
const OUTSIDER: &str = "203.0.113.66";
/// The Request-URI host: neither a worker nor the proxy.
const FAR: &str = "198.51.100.7";

/// A gate with a fixed verdict that counts its consultations and bypasses.
struct CountingGate {
    admit: bool,
    consulted: AtomicU32,
    bypass_internal: AtomicU32,
}

impl ProxySelfGate for CountingGate {
    fn try_admit_external(&self) -> AdmitDecision {
        self.consulted.fetch_add(1, Ordering::SeqCst);
        if self.admit {
            AdmitDecision::Admit
        } else {
            AdmitDecision::Reject { reason: ShedReason::Cps, retry_after_sec: 1 }
        }
    }
    fn note_bypass(&self, kind: BypassKind) {
        if let BypassKind::Internal = kind {
            self.bypass_internal.fetch_add(1, Ordering::SeqCst);
        }
    }
}

struct Lab {
    worker: Box<dyn UdpEndpoint>,
    unlisted_worker: Box<dyn UdpEndpoint>,
    outsider: Box<dyn UdpEndpoint>,
    far: Box<dyn UdpEndpoint>,
    gate: Arc<CountingGate>,
    metrics: Arc<ProxyMetrics>,
    core: tokio::task::JoinHandle<()>,
}

impl Drop for Lab {
    fn drop(&mut self) {
        self.core.abort();
    }
}

fn sa(host: &str, port: u16) -> SocketAddr {
    format!("{host}:{port}").parse().unwrap()
}

fn ext_vip() -> SocketAddr {
    sa(EXT_VIP, EXT_PORT)
}

fn int_vip() -> SocketAddr {
    sa(INT_VIP, INT_PORT)
}

/// A dual-face proxy (or, `dual == false`, a single-face one on the external
/// VIP) in front of `W1`, with a gate that admits or refuses every new call.
async fn proxy_lab(dual: bool, admit: bool) -> Lab {
    let net = SimulatedSignalingNetwork::new(1);
    let bind = |addr: SocketAddr| {
        let net = net.clone();
        async move { net.bind_udp(BindUdpOpts::new(addr, 64)).await.unwrap() }
    };
    let worker = bind(sa(W1, 5060)).await;
    let unlisted_worker = bind(sa(UNLISTED_WORKER, 5060)).await;
    let outsider = bind(sa(OUTSIDER, 5060)).await;
    let far = bind(sa(FAR, 5060)).await;
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(W1, 5060),
        )]));
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
    let gate = Arc::new(CountingGate {
        admit,
        consulted: AtomicU32::new(0),
        bypass_internal: AtomicU32::new(0),
    });
    let metrics = Arc::new(ProxyMetrics::new());
    let core = if dual {
        let int_ep = bind(int_vip()).await;
        let ext_ep = bind(ext_vip()).await;
        ProxyCoreBuilder::new(ProxyAddr::new(INT_VIP, INT_PORT), strategy, reg)
            .clock(Clock::test_at(0))
            .metrics(metrics.clone())
            .self_gate(gate.clone())
            .external_face(ExternalFaceParts {
                endpoint: ext_ep,
                advertised: ProxyAddr::new(EXT_VIP, EXT_PORT),
                int_cidrs: FaceCidrs::parse(INT_CIDRS).unwrap(),
            })
            .build(int_ep)
    } else {
        let ep = bind(ext_vip()).await;
        ProxyCoreBuilder::new(ProxyAddr::new(EXT_VIP, EXT_PORT), strategy, reg)
            .clock(Clock::test_at(0))
            .metrics(metrics.clone())
            .self_gate(gate.clone())
            .build(ep)
    };
    let core = tokio::spawn(core.run());
    Lab { worker, unlisted_worker, outsider, far, gate, metrics, core }
}

fn header_lines(bytes: &[u8], name: &str) -> Vec<String> {
    let prefix = format!("{}:", name.to_ascii_lowercase());
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with(&prefix))
        .map(|l| l.trim_end().to_string())
        .collect()
}

fn start_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).lines().next().unwrap_or("").to_string()
}

/// The 200 the UAS at `FAR` answers a request with (§8.2.6), sent to its top
/// Via, the proxy hop.
fn ok_for(req: &[u8]) -> (Vec<u8>, SocketAddr) {
    let vias = header_lines(req, "Via");
    let top = vias[0].split_once(':').unwrap().1.trim().to_string();
    let sent_by = top.split_whitespace().nth(1).unwrap().split(';').next().unwrap();
    let mut out = String::from("SIP/2.0 200 OK\r\n");
    for line in &vias {
        out.push_str(&format!("{line}\r\n"));
    }
    for name in ["From", "Call-ID", "CSeq"] {
        out.push_str(&format!("{}\r\n", header_lines(req, name)[0]));
    }
    let to = &header_lines(req, "To")[0];
    let tag = if to.contains("tag=") { "" } else { ";tag=far-tag" };
    out.push_str(&format!("{to}{tag}\r\n"));
    out.push_str(&format!("Contact: <sip:uas@{FAR}:5060>\r\nContent-Length: 0\r\n\r\n"));
    (out.into_bytes(), sent_by.parse().unwrap())
}

/// Who received what after one request.
struct Seen {
    to_worker: Vec<(SocketAddr, Vec<u8>)>,
    to_far: Option<(SocketAddr, Vec<u8>)>,
    back_to_sender: Vec<Vec<u8>>,
    consulted: u32,
    bypass_internal: u32,
}

fn drain(ep: &dyn UdpEndpoint) -> Vec<(SocketAddr, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some(p) = ep.try_recv() {
        out.push((p.src, p.raw));
    }
    out
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// `sender` sends `request` to `entry`; a request reaching `FAR` is answered
/// 200, and whatever comes back to `sender` is collected.
async fn exchange(lab: &Lab, sender: &dyn UdpEndpoint, entry: SocketAddr, request: String) -> Seen {
    sender.send_to(request.as_bytes(), entry).await.unwrap();
    settle().await;
    let to_worker = drain(&*lab.worker);
    let to_far = drain(&*lab.far).pop();
    if let Some((_, req)) = &to_far {
        let (ok, dst) = ok_for(req);
        lab.far.send_to(&ok, dst).await.unwrap();
        settle().await;
    }
    let back_to_sender = drain(sender).into_iter().map(|(_, b)| b).collect();
    Seen {
        to_worker,
        to_far,
        back_to_sender,
        consulted: lab.gate.consulted.load(Ordering::SeqCst),
        bypass_internal: lab.gate.bypass_internal.load(Ordering::SeqCst),
    }
}

/// A request toward `FAR`: an INVITE, or with a To-tag a `method` in dialog.
fn request(
    method: &str,
    call_id: &str,
    top_via: &str,
    route: Option<&str>,
    to_tag: Option<&str>,
) -> String {
    let cseq = if to_tag.is_some() { 2 } else { 1 };
    let to_tag = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
    let route = route.map(|r| format!("Route: {r}\r\n")).unwrap_or_default();
    let host = top_via.split(':').next().unwrap();
    format!(
        "{method} sip:+15550100@{FAR}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {top_via};branch=z9hG4bK{call_id};rport\r\n\
{route}Max-Forwards: 70\r\n\
From: <sip:caller@{host}>;tag=c1\r\n\
To: <sip:+15550100@{FAR}>{to_tag}\r\n\
Call-ID: {call_id}@{host}\r\n\
CSeq: {cseq} {method}\r\n\
Contact: <sip:caller@{top_via}>\r\n\
Content-Length: 0\r\n\r\n"
    )
}

fn outsider_via() -> String {
    format!("{OUTSIDER}:5060")
}

fn worker_via() -> String {
    format!("{W1}:5060")
}

fn outbound_route(host: &str, port: u16) -> String {
    format!("<sip:{host}:{port};lr;outbound>")
}

fn counted(lab: &Lab, claim: UntrustedClaim) -> u64 {
    lab.metrics.external_untrusted_total(claim)
}

fn got_status(seen: &Seen, status: &str) -> bool {
    seen.back_to_sender.iter().any(|b| start_line(b).starts_with(&format!("SIP/2.0 {status}")))
}

/// Forwarded to the Request-URI out of the external face, no worker on the
/// path, the new call taken as the internal plane's (no gate).
fn assert_worker_outbound(seen: &Seen) {
    assert!(seen.to_worker.is_empty(), "no worker sees a worker-outbound request");
    let (src, _) = seen.to_far.as_ref().expect("forwarded to the Request-URI host");
    assert_eq!(*src, ext_vip(), "it leaves on the external face");
    assert_eq!(seen.consulted, 0, "a worker-outbound new call does not meet the gate");
    assert_eq!(seen.bypass_internal, 1);
}

/// Never relayed to the Request-URI: the new call met the gate (closed here,
/// so the sender has its 503) as any inbound one does.
fn assert_met_the_gate_and_not_relayed(seen: &Seen) {
    assert!(seen.to_far.is_none(), "the Request-URI host never sees it");
    assert!(seen.to_worker.is_empty(), "the closed gate refused it");
    assert_eq!(seen.consulted, 1, "the new call meets the gate");
    assert_eq!(seen.bypass_internal, 0, "nothing marks it as the internal plane's");
    assert!(got_status(seen, "503"), "the sender has the gate's 503");
}

#[tokio::test(start_paused = true)]
async fn an_outbound_marker_on_the_external_face_is_ignored() {
    let lab = proxy_lab(true, false).await;
    let route = outbound_route(EXT_VIP, EXT_PORT);
    let invite = request("INVITE", "ext-marker", &outsider_via(), Some(&route), None);
    let seen = exchange(&lab, &*lab.outsider, ext_vip(), invite).await;
    assert_met_the_gate_and_not_relayed(&seen);
    assert_eq!(counted(&lab, UntrustedClaim::OutboundMarker), 1);
}

#[tokio::test(start_paused = true)]
async fn a_worker_via_on_the_external_face_is_ignored() {
    let lab = proxy_lab(true, false).await;
    let invite = request("INVITE", "ext-via", &worker_via(), None, None);
    let seen = exchange(&lab, &*lab.outsider, ext_vip(), invite).await;
    assert_met_the_gate_and_not_relayed(&seen);
    assert_eq!(counted(&lab, UntrustedClaim::WorkerVia), 1);
}

#[tokio::test(start_paused = true)]
async fn an_in_dialog_request_with_an_outbound_marker_on_the_external_face_goes_to_a_worker() {
    let lab = proxy_lab(true, false).await;
    let route = outbound_route(EXT_VIP, EXT_PORT);
    let bye = request("BYE", "ext-bye", &outsider_via(), Some(&route), Some("far-tag"));
    let seen = exchange(&lab, &*lab.outsider, ext_vip(), bye).await;
    assert!(seen.to_far.is_none(), "the Request-URI host never sees it");
    assert_eq!(seen.to_worker.len(), 1, "it is routed as an inbound request");
}

#[tokio::test(start_paused = true)]
async fn a_foreign_loose_route_on_the_external_face_is_removed() {
    for route in [
        format!("<sip:{FAR}:5060;lr>"),
        format!("<sip:{EXT_VIP}:{EXT_PORT};lr>, <sip:{FAR}:5060;lr>"),
    ] {
        let lab = proxy_lab(true, true).await;
        let invite = request("INVITE", "ext-route", &outsider_via(), Some(&route), None);
        let seen = exchange(&lab, &*lab.outsider, ext_vip(), invite).await;
        assert!(seen.to_far.is_none(), "{route}: the Route's host never sees it");
        assert_eq!(seen.to_worker.len(), 1, "{route}: it goes to a worker");
        let (_, forwarded) = &seen.to_worker[0];
        assert!(header_lines(forwarded, "Route").is_empty(), "{route}: no Route rides on");
        assert_eq!(seen.consulted, 1);
        assert_eq!(counted(&lab, UntrustedClaim::ForeignRoute), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn a_request_without_a_marker_meets_the_gate_and_goes_to_a_worker() {
    for dual in [false, true] {
        let lab = proxy_lab(dual, false).await;
        let invite = request("INVITE", "plain-closed", &outsider_via(), None, None);
        let seen = exchange(&lab, &*lab.outsider, ext_vip(), invite).await;
        assert_met_the_gate_and_not_relayed(&seen);
        drop(lab);

        let lab = proxy_lab(dual, true).await;
        let invite = request("INVITE", "plain-open", &outsider_via(), None, None);
        let seen = exchange(&lab, &*lab.outsider, ext_vip(), invite).await;
        assert_eq!(seen.to_worker.len(), 1, "dual={dual}: it goes to the worker");
        assert!(seen.to_far.is_none());
        assert_eq!(seen.consulted, 1);
    }
}

#[tokio::test(start_paused = true)]
async fn a_registered_worker_on_the_internal_face_is_worker_outbound() {
    let lab = proxy_lab(true, false).await;
    let invite = request("INVITE", "int-worker", &worker_via(), None, None);
    let seen = exchange(&lab, &*lab.worker, int_vip(), invite).await;
    assert_worker_outbound(&seen);
    assert!(got_status(&seen, "200"), "the far end's 200 comes back to the worker");
}

#[tokio::test(start_paused = true)]
async fn an_outbound_marker_on_the_internal_face_is_worker_outbound_from_an_unlisted_worker() {
    let lab = proxy_lab(true, false).await;
    let route = outbound_route(INT_VIP, INT_PORT);
    let via = format!("{UNLISTED_WORKER}:5060");
    let invite = request("INVITE", "int-marker", &via, Some(&route), None);
    let seen = exchange(&lab, &*lab.unlisted_worker, int_vip(), invite).await;
    assert_worker_outbound(&seen);
}

#[tokio::test(start_paused = true)]
async fn a_response_on_the_external_face_whose_next_via_is_no_worker_is_dropped() {
    let lab = proxy_lab(true, true).await;
    let forged = format!(
        "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP {EXT_VIP}:{EXT_PORT};branch=z9hG4bKforged\r\n\
Via: SIP/2.0/UDP {FAR}:5060;branch=z9hG4bKfar;rport=5060\r\n\
From: <sip:caller@{FAR}>;tag=c1\r\n\
To: <sip:callee@{EXT_VIP}>;tag=c2\r\n\
Call-ID: reflect@{FAR}\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    );
    lab.outsider.send_to(forged.as_bytes(), ext_vip()).await.unwrap();
    settle().await;
    assert!(drain(&*lab.far).is_empty(), "the proxy reflects no response to a non-worker");
    assert_eq!(counted(&lab, UntrustedClaim::ResponseNotToWorker), 1);
}

#[tokio::test(start_paused = true)]
async fn a_forged_to_tag_invite_on_the_external_face_meets_the_gate() {
    let lab = proxy_lab(true, false).await;
    let invite = request("INVITE", "forged-tag", &outsider_via(), None, Some("forged"));
    let seen = exchange(&lab, &*lab.outsider, ext_vip(), invite).await;
    assert_met_the_gate_and_not_relayed(&seen);
}
