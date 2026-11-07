//! ADR-0001's first-ingress bound on the proxy. A party's new request meets it
//! here, and one past it is answered here. Everything after it reads with room:
//! what this proxy forwards reads at the next hop whatever the forward added,
//! and a response reads on its way back whatever a hop added to its lines.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::MaxForwards;
use sip_message::{CustomParser, Exemptions, SipMessage, SipParser, SipParserLimits, SipRequest};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::{ForwardAllStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics, RoutingStrategy};
use sip_txn::IdGen;
use tokio::task::JoinHandle;

const PROXY: &str = "192.0.2.1:5060";
const CALLER: &str = "192.0.2.20:5060";
const WORKER: &str = "10.0.0.2:5070";

/// The bound on a line and on a Request-URI.
const LINE_BOUND: usize = 8_192;
/// The bound on a header block.
const LINES_BOUND: usize = 100;

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("an address")
}

struct Rig {
    task: JoinHandle<()>,
    metrics: Arc<ProxyMetrics>,
    caller: Box<dyn UdpEndpoint>,
    worker: Box<dyn UdpEndpoint>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Rig {
    async fn new() -> Rig {
        Self::reading(SipParserLimits::DEFAULT).await
    }

    /// A proxy whose parser reads under `limits`.
    async fn reading(limits: SipParserLimits) -> Rig {
        let net = SimulatedSignalingNetwork::new(1);
        let bind = |at| net.bind_udp(BindUdpOpts::new(addr(at), 64));
        let endpoint = bind(PROXY).await.expect("bind the proxy");
        let caller = bind(CALLER).await.expect("bind the caller");
        let worker = bind(WORKER).await.expect("bind the worker");
        let strategy: Arc<dyn RoutingStrategy> =
            Arc::new(ForwardAllStrategy::new(ProxyAddr::from(addr(WORKER))));
        let registry: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
                "w0",
                ProxyAddr::from(addr(WORKER)),
            )]));
        let metrics = Arc::new(ProxyMetrics::new());
        let core = ProxyCoreBuilder::new(
            ProxyAddr::from(addr(PROXY)),
            strategy,
            registry,
            Arc::new(IdGen::seeded(0x847)),
            CustomParser::with_limits(limits),
        )
        .clock(Clock::test_at(0))
        .metrics(metrics.clone())
        .build(endpoint);
        Rig { task: tokio::spawn(core.run()), metrics, caller, worker }
    }

    /// What the worker receives once the caller sent `invite`, as bytes.
    async fn forward_of(&self, invite: &str) -> Option<Vec<u8>> {
        self.caller.send_to(invite.as_bytes(), addr(PROXY)).await.expect("sent");
        settle().await;
        self.worker.try_recv().map(|p| p.raw.to_vec())
    }
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// The lines a datagram's head holds.
fn lines_of(raw: &[u8]) -> usize {
    let text = String::from_utf8_lossy(raw);
    text.split("\r\n\r\n").next().unwrap_or_default().lines().count()
}

/// An INVITE from the caller: `via` on its top Via line, `to` as its To line,
/// with or without a Max-Forwards, padded to `at_least` lines.
fn invite(via: &str, to: &str, max_forwards: bool, at_least: usize) -> String {
    let mut lines = vec![
        "INVITE sip:bob@192.0.2.30 SIP/2.0".to_string(),
        format!("Via: {via}"),
        "From: <sip:alice@192.0.2.20>;tag=alice-tag".to_string(),
        to.to_string(),
        "Call-ID: bound@192.0.2.20".to_string(),
        "CSeq: 1 INVITE".to_string(),
        "Contact: <sip:alice@192.0.2.20:5060>".to_string(),
        "Content-Length: 0".to_string(),
    ];
    if max_forwards {
        lines.insert(2, "Max-Forwards: 70".to_string());
    }
    while lines.len() < at_least {
        lines.insert(2, "X-Pad: p".to_string());
    }
    format!("{}\r\n\r\n", lines.join("\r\n"))
}

fn plain_via() -> String {
    format!("SIP/2.0/UDP {CALLER};branch=z9hG4bK-bound;rport")
}

const PLAIN_TO: &str = "To: <sip:bob@192.0.2.30>";

/// A top Via line of exactly `len` bytes.
fn via_of(len: usize) -> String {
    let head = format!("{};x=", plain_via());
    format!("{head}{}", "a".repeat(len - "Via: ".len() - head.len()))
}

/// A To line of exactly `len` bytes.
fn to_of(len: usize) -> String {
    let frame = "To: <sip:@192.0.2.30>".len();
    format!("To: <sip:{}@192.0.2.30>", "b".repeat(len - frame))
}

/// The request a worker reads off `forward`: this stack's reader, no setting.
fn read(forward: &[u8]) -> SipRequest {
    match CustomParser::new().parse(forward) {
        Ok(SipMessage::Request(req)) => req,
        Ok(other) => panic!("a request: {other:?}"),
        Err(refused) => {
            panic!("{} lines on the forward: {}", lines_of(forward), refused.reason)
        }
    }
}

/// `final_` of the worker's, for the request it read in `forward`, relayed by
/// the proxy to the caller.
async fn answered_through(rig: &Rig, forward: &[u8], status: u16) -> SipMessage {
    let opts = GenerateResponseOpts { to_tag: Some("own-tag-123".into()), ..Default::default() };
    let answer = generate_response(&read(forward), status, "As Stated", &opts).expect("the final");
    rig.worker.send_to(answer.image(), addr(PROXY)).await.expect("sent");
    settle().await;
    let packet = rig.caller.try_recv().expect("the final reaches the caller");
    CustomParser::new().parse(&packet.raw).expect("the caller reads it")
}

/// The one answer the caller holds to a request past the bound: nothing forwarded.
#[track_caller]
fn answered_400_at_first_ingress(rig: &Rig, forward: Option<Vec<u8>>) {
    assert!(forward.is_none(), "nothing is forwarded");
    let packet = rig.caller.try_recv().expect("the caller is answered at first ingress");
    match CustomParser::new().parse(&packet.raw) {
        Ok(SipMessage::Response(r)) => assert_eq!(r.status(), 400, "{r:?}"),
        other => panic!("a response: {other:?}"),
    }
    assert_eq!(rig.metrics.parse_refusals_answered(), 1);
}

/// On a deployment naming `max-forwards:absent`, a request with no hop count at
/// the line bound gains the proxy's Max-Forwards of 70 with its other lines
/// (RFC 3261 §16.6 step 3), and the next hop still reads it.
#[tokio::test(start_paused = true)]
async fn an_invite_at_the_line_bound_with_no_max_forwards_reads_at_the_next_hop() {
    let exemptions = Exemptions::read(Some("max-forwards:absent")).expect("named");
    let rig = Rig::reading(SipParserLimits { exemptions, ..SipParserLimits::DEFAULT }).await;
    let request = invite(&plain_via(), PLAIN_TO, false, LINES_BOUND);
    assert_eq!(lines_of(request.as_bytes()), LINES_BOUND);
    let forward = rig.forward_of(&request).await.expect("the proxy forwards it");
    let hops = read(&forward).header::<MaxForwards>().expect("stated").expect("reads");
    assert_eq!(hops.value(), 70, "RFC 3261 §16.6 step 3: an absent count is added as 70");
    assert!(lines_of(&forward) > LINES_BOUND, "the forward carries the proxy's additions");
}

/// A comma-folded top Via stays one line through the proxy's stamp: a forward
/// adds its own lines, never one per value (RFC 3261 §7.3.1 reads them alike).
#[tokio::test(start_paused = true)]
async fn a_folded_top_via_at_the_line_bound_stays_one_line_on_the_forward() {
    let rig = Rig::new().await;
    let hop = "SIP/2.0/UDP 192.0.2.40:5060;branch=z9hG4bK-lower";
    let via = format!("{}, {hop}, {hop}, {hop}", plain_via());
    let request = invite(&via, PLAIN_TO, true, LINES_BOUND);
    assert_eq!(lines_of(request.as_bytes()), LINES_BOUND);
    let forward = rig.forward_of(&request).await.expect("the proxy forwards it");
    let read = read(&forward);
    assert_eq!(read.via().iter().count(), 5, "the proxy's and the caller's four");
    assert_eq!(lines_of(&forward), LINES_BOUND + 3, "its Via and its two Record-Routes");
}

#[tokio::test(start_paused = true)]
async fn a_top_via_at_the_line_length_bound_draws_its_final() {
    let rig = Rig::new().await;
    let request = invite(&via_of(LINE_BOUND), PLAIN_TO, true, 0);
    let forward = rig.forward_of(&request).await.expect("the proxy forwards it");
    let line = String::from_utf8_lossy(&forward).to_string();
    assert!(line.lines().any(|l| l.starts_with("Via:") && l.len() > LINE_BOUND), "stamped");
    let answer = answered_through(&rig, &forward, 486).await;
    assert!(matches!(answer, SipMessage::Response(r) if r.status() == 486));
}

#[tokio::test(start_paused = true)]
async fn a_to_at_the_line_length_bound_draws_its_final() {
    let rig = Rig::new().await;
    let request = invite(&plain_via(), &to_of(LINE_BOUND), true, 0);
    let forward = rig.forward_of(&request).await.expect("the proxy forwards it");
    let answer = answered_through(&rig, &forward, 486).await;
    assert!(matches!(answer, SipMessage::Response(r) if r.status() == 486));
}

#[tokio::test(start_paused = true)]
async fn a_request_one_past_the_bound_is_answered_at_first_ingress() {
    for request in [
        invite(&plain_via(), &to_of(LINE_BOUND + 1), true, 0),
        invite(&via_of(LINE_BOUND + 1), PLAIN_TO, true, 0),
        invite(&plain_via(), PLAIN_TO, true, LINES_BOUND + 1),
        invite(&plain_via(), PLAIN_TO, true, 0).replace(
            "INVITE sip:bob@192.0.2.30 ",
            &format!(
                "INVITE sip:{}@192.0.2.30 ",
                "b".repeat(LINE_BOUND + 1 - "sip:@192.0.2.30".len())
            ),
        ),
    ] {
        let rig = Rig::new().await;
        let forward = rig.forward_of(&request).await;
        answered_400_at_first_ingress(&rig, forward);
    }
}

/// An in-dialog request carries what a hop added to the dialog's lines, a tag
/// on its To among them: only a new request meets the bound.
#[tokio::test(start_paused = true)]
async fn an_in_dialog_request_past_the_bound_is_forwarded() {
    let rig = Rig::new().await;
    let request = invite(&plain_via(), &format!("{};tag=dialog", to_of(LINE_BOUND)), true, 0);
    let forward = rig.forward_of(&request).await.expect("the proxy forwards it");
    read(&forward);
}

/// A worker's own request grows what the caller sent: it reads with room.
#[tokio::test(start_paused = true)]
async fn a_workers_request_past_the_bound_is_forwarded() {
    let rig = Rig::new().await;
    let via = "SIP/2.0/UDP 10.0.0.2:5070;branch=z9hG4bK-worker";
    let request = invite(via, &to_of(LINE_BOUND + 40), true, LINES_BOUND + 2)
        .replace("INVITE sip:bob@192.0.2.30", "INVITE sip:bob@192.0.2.30;own");
    rig.worker.send_to(request.as_bytes(), addr(PROXY)).await.expect("sent");
    settle().await;
    assert_eq!(rig.metrics.parse_errors(), 0, "the request reads");
    assert_eq!(rig.metrics.parse_refusals_answered(), 0, "and is not refused");
}

/// A worker whose source address is translated on its way to the proxy is named
/// by its top Via's sent-by: its request reads with room too.
#[tokio::test(start_paused = true)]
async fn a_request_whose_top_via_names_a_worker_past_the_bound_is_forwarded() {
    let rig = Rig::new().await;
    let via = format!("SIP/2.0/UDP {WORKER};branch=z9hG4bK-translated");
    let request = invite(&via, &to_of(LINE_BOUND + 40), true, 0);
    rig.caller.send_to(request.as_bytes(), addr(PROXY)).await.expect("sent");
    settle().await;
    assert_eq!(rig.metrics.parse_refusals_answered(), 0, "the request is not refused");
    assert!(rig.caller.try_recv().is_none(), "nothing answers it here");
}

/// A response is no request at first ingress, whatever its size.
#[tokio::test(start_paused = true)]
async fn a_response_past_the_bound_is_relayed() {
    let rig = Rig::new().await;
    let request = invite(&plain_via(), PLAIN_TO, true, 0);
    let forward = rig.forward_of(&request).await.expect("the proxy forwards it");
    let mut opts = GenerateResponseOpts { to_tag: Some("t".into()), ..Default::default() };
    opts.extra_headers = vec![sip_message::SipHeader {
        name: "X-Long".to_string().into(),
        value: "a".repeat(LINE_BOUND + 100).into(),
    }];
    let answer = generate_response(&read(&forward), 486, "As Stated", &opts).expect("the final");
    rig.worker.send_to(answer.image(), addr(PROXY)).await.expect("sent");
    settle().await;
    let packet = rig.caller.try_recv().expect("the final reaches the caller");
    assert!(matches!(
        CustomParser::new().parse(&packet.raw),
        Ok(SipMessage::Response(r)) if r.status() == 486
    ));
}
