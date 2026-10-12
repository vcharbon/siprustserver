//! Reverse-path failover on a single-face proxy: every response arrives on the
//! internal face and is sent to its `received`/`rport`. The external face's
//! response rule is pinned in `tests/it/ingress_trust.rs`.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{SendError, UdpEndpoint, UdpEndpointCounters, UdpPacket};

use crate::addr::ProxyAddr;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::registry::simulated::SimulatedWorkerRegistry;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::tombstone::DEPARTED_ADDRESS_TTL_MS;
use crate::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use crate::strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};

const W1_POD: &str = "10.244.5.8";
const W2_POD: &str = "10.244.5.9";
const W3_POD: &str = "10.244.5.10";
const W4_POD: &str = "10.244.5.11";
const UAC: &str = "10.244.7.13";
const PROXY_VIP: &str = "192.0.2.250";
const SNAT_NODE: &str = "192.0.2.11";

/// Endpoint double recording every send's destination.
#[derive(Default)]
struct CapturingEndpoint {
    sent: Mutex<Vec<SocketAddr>>,
}

#[async_trait]
impl UdpEndpoint for CapturingEndpoint {
    async fn send_to(&self, _buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        self.sent.lock().unwrap().push(dst);
        Ok(())
    }
    async fn recv(&self) -> Option<UdpPacket> {
        std::future::pending().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        None
    }
    fn local_addr(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5060))
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

/// Strategy double whose cookie decode fails over to w2.
struct BackupStrategy;

#[async_trait]
impl RoutingStrategy for BackupStrategy {
    fn name(&self) -> &str {
        "Backup"
    }
    async fn select_for_new_dialog(
        &self,
        _msg: &SipMessage,
        _opts: SelectOpts,
    ) -> Result<ProxyAddr, SelectError> {
        Err(SelectError::NoTarget { reason: "unused".into() })
    }
    async fn decode_stickiness(&self, _params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
        DecodeResult::ForwardBackup {
            target: ProxyAddr::new(W2_POD, 5060),
            is_emergency: false,
            promotion: None,
        }
    }
    fn encode_stickiness(&self, _target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
        None
    }
    fn stickiness_primary<'p>(&self, params: &'p RouteParams) -> Option<&'p str> {
        params.get("w_pri").map(String::as_str)
    }
}

/// A strategy double whose cookie decode finds no usable backup.
struct NoBackupStrategy;

#[async_trait]
impl RoutingStrategy for NoBackupStrategy {
    fn name(&self) -> &str {
        "NoBackup"
    }
    async fn select_for_new_dialog(
        &self,
        _msg: &SipMessage,
        _opts: SelectOpts,
    ) -> Result<ProxyAddr, SelectError> {
        Err(SelectError::NoTarget { reason: "unused".into() })
    }
    async fn decode_stickiness(&self, _params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
        DecodeResult::Unknown { is_emergency: false }
    }
    fn encode_stickiness(&self, _target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
        None
    }
    fn stickiness_primary<'p>(&self, params: &'p RouteParams) -> Option<&'p str> {
        params.get("w_pri").map(String::as_str)
    }
}

fn core_over(
    reg: Arc<dyn WorkerRegistry>,
    strategy: Arc<dyn RoutingStrategy>,
    clock: Clock,
) -> (ProxyCore, Arc<CapturingEndpoint>) {
    let ep = Arc::new(CapturingEndpoint::default());
    let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(clock)
        .build(Box::new(CapturingEndpointHandle(ep.clone())));
    (core, ep)
}

fn core_with(w1_health: WorkerHealth) -> (ProxyCore, Arc<CapturingEndpoint>) {
    let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![
        WorkerEntry {
            id: "w1".into(),
            address: ProxyAddr::new(W1_POD, 5060),
            health: w1_health,
            draining_since: None,
            first_seen_at_ms: None,
        },
        WorkerEntry::alive("w2", ProxyAddr::new(W2_POD, 5060)),
    ]));
    core_over(reg, Arc::new(BackupStrategy), Clock::test_at(0))
}

/// A pool `w1` has left: the address it was last known at is tombstoned, so
/// the response path reads a response from it as a departed worker's.
fn registry_without_w1(clock: Clock) -> Arc<dyn WorkerRegistry> {
    let reg = SimulatedWorkerRegistry::with_clock(
        vec![
            WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060)),
            WorkerEntry::alive("w2", ProxyAddr::new(W2_POD, 5060)),
        ],
        clock,
    );
    reg.remove("w1");
    Arc::new(reg)
}

/// Box-able forwarding wrapper (the builder takes `Box<dyn UdpEndpoint>`;
/// the test keeps the `Arc` to read captures).
struct CapturingEndpointHandle(Arc<CapturingEndpoint>);

#[async_trait]
impl UdpEndpoint for CapturingEndpointHandle {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        self.0.send_to(buf, dst).await
    }
    async fn recv(&self) -> Option<UdpPacket> {
        self.0.recv().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        self.0.try_recv()
    }
    fn local_addr(&self) -> SocketAddr {
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

/// A 200 heading back to the worker that sent the request: top Via = the
/// proxy, next Via = the worker's sent-by, SNAT'd received/rport stamped by
/// the request path, `record_route` as the UAS echoes it back (empty for a
/// request that copies none).
fn worker_bound_200_routed(
    cseq: &str,
    record_route: &str,
    extra_headers: &str,
) -> sip_message::types::SipResponse {
    let raw = format!(
        "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKout;rport\r\n\
Via: SIP/2.0/UDP {W1_POD}:5060;branch=z9hG4bKka;received={SNAT_NODE};rport=63522\r\n\
{record_route}From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
Call-ID: dlg-1@{UAC}\r\n\
CSeq: {cseq}\r\n\
{extra_headers}Content-Length: 0\r\n\r\n"
    );
    let SipMessage::Response(resp) = CustomParser::default().parse(raw.as_bytes()).unwrap() else {
        unreachable!()
    };
    resp
}

/// The same 200 carrying the proxy's own cookie Record-Route, echoed by the
/// UAS — the shape a response to a Record-Routed request has.
fn worker_bound_200(cseq: &str, extra_headers: &str) -> sip_message::types::SipResponse {
    worker_bound_200_routed(
        cseq,
        &format!("Record-Route: <sip:{PROXY_VIP}:5060;w_pri=w1;w_bak=w2;lr>\r\n"),
        extra_headers,
    )
}

/// The 2xx answering a worker-originated INVITE — the response class
/// reverse-path failover serves.
fn invite_200() -> sip_message::types::SipResponse {
    worker_bound_200("1 INVITE", &format!("Contact: <sip:sipp@{UAC}:5060>\r\n"))
}

/// The 200 to a worker's in-dialog keepalive OPTIONS.
fn keepalive_200() -> sip_message::types::SipResponse {
    worker_bound_200("2 OPTIONS", "")
}

/// The 200 the callee sends for a worker's CANCEL (§9.2) — a client
/// transaction only the worker that sent the CANCEL holds.
fn cancel_200() -> sip_message::types::SipResponse {
    worker_bound_200("1 CANCEL", "")
}

/// The 200 to a worker's in-dialog BYE.
fn bye_200() -> sip_message::types::SipResponse {
    worker_bound_200("3 BYE", "")
}

/// The CANCEL's 200 as it arrives on the wire: a CANCEL copies no
/// Record-Route (§9.1), so nothing echoes the proxy's cookie back and the
/// response path finds no route params to decode.
fn cancel_200_without_record_route() -> sip_message::types::SipResponse {
    worker_bound_200_routed("1 CANCEL", "", "")
}

// A Dead-worker lookup keyed on the received/rport-derived address — behind
// the VIP that is the SNAT node IP + ephemeral port — matches no registry
// entry, so the failover branch is unreachable and a dead worker's responses
// are blackholed at its stale SNAT address. The worker must be identified by
// its Via SENT-BY (registry identity).
#[tokio::test]
async fn response_to_a_dead_worker_fails_over_to_the_backup() {
    let (core, ep) = core_with(WorkerHealth::Dead);
    core.handle_response_from(invite_200(), format!("{W1_POD}:5060").parse().unwrap()).await;

    let sent = ep.sent.lock().unwrap();
    assert_eq!(sent.as_slice(), &[format!("{W2_POD}:5060").parse::<SocketAddr>().unwrap()]);
}

/// Where the cookie doubles send a backup: the pod of the worker `w_bak`
/// names.
fn pod_of(worker: &str) -> &'static str {
    match worker {
        "w2" => W2_POD,
        "w4" => W4_POD,
        other => panic!("no pod for {other}"),
    }
}

/// Strategy double that fails over to the backup the decoded cookie names,
/// and finds none in an entry carrying no cookie.
struct CookieBackupStrategy;

#[async_trait]
impl RoutingStrategy for CookieBackupStrategy {
    fn name(&self) -> &str {
        "CookieBackup"
    }
    async fn select_for_new_dialog(
        &self,
        _msg: &SipMessage,
        _opts: SelectOpts,
    ) -> Result<ProxyAddr, SelectError> {
        Err(SelectError::NoTarget { reason: "unused".into() })
    }
    async fn decode_stickiness(&self, params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
        match params.get("w_bak") {
            Some(backup) => DecodeResult::ForwardBackup {
                target: ProxyAddr::new(pod_of(backup), 5060),
                is_emergency: false,
                promotion: None,
            },
            None => DecodeResult::Unknown { is_emergency: false },
        }
    }
    fn encode_stickiness(&self, _target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
        None
    }
    fn stickiness_primary<'p>(&self, params: &'p RouteParams) -> Option<&'p str> {
        params.get("w_pri").map(String::as_str)
    }
}

/// w1 dead, the other workers alive, cookies decoded by
/// [`CookieBackupStrategy`].
fn spiral_core() -> (ProxyCore, Arc<CapturingEndpoint>) {
    let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![
        WorkerEntry {
            id: "w1".into(),
            address: ProxyAddr::new(W1_POD, 5060),
            health: WorkerHealth::Dead,
            draining_since: None,
            first_seen_at_ms: None,
        },
        WorkerEntry::alive("w2", ProxyAddr::new(W2_POD, 5060)),
        WorkerEntry::alive("w3", ProxyAddr::new(W3_POD, 5060)),
        WorkerEntry::alive("w4", ProxyAddr::new(W4_POD, 5060)),
    ]));
    core_over(reg, Arc::new(CookieBackupStrategy), Clock::test_at(0))
}

/// The 200 answering w1's INVITE after it spiralled (§16.3): w1 → proxy →
/// a third-party proxy → this proxy again → another call. The UAS echoes
/// every Record-Route the request gathered, topmost first: the pair this
/// proxy added relaying it in (its worker-facing `;outbound` half on top),
/// the third party's, and the pair this proxy added relaying it out of w1
/// (its cookie half on top).
fn spiralled_invite_200(inbound_pair: &str) -> sip_message::types::SipResponse {
    worker_bound_200_routed(
        "1 INVITE",
        &format!(
            "Record-Route: {inbound_pair}\r\n\
Record-Route: <sip:10.244.9.1:5060;lr>\r\n\
Record-Route: <sip:{PROXY_VIP}:5060;w_pri=w1;w_bak=w2;lr>, <sip:{PROXY_VIP}:5060;outbound;lr>\r\n"
        ),
        &format!("Contact: <sip:sipp@{UAC}:5060>\r\n"),
    )
}

// A spiral puts two of this proxy's Record-Route pairs on one response.
// Single-face, the topmost own entry is the inbound pair's cookie-less
// `;outbound` half: read by position, the dead worker's response finds no
// cookie and is dropped.
#[tokio::test]
async fn a_spiralled_response_to_a_dead_worker_fails_over_on_its_own_cookie() {
    let (core, ep) = spiral_core();
    let inbound =
        format!("<sip:{PROXY_VIP}:5060;outbound;lr>, <sip:{PROXY_VIP}:5060;w_pri=w3;w_bak=w4;lr>");
    core.handle_response_from(
        spiralled_invite_200(&inbound),
        format!("{W1_POD}:5060").parse().unwrap(),
    )
    .await;

    let sent = ep.sent.lock().unwrap();
    assert_eq!(sent.as_slice(), &[format!("{W2_POD}:5060").parse::<SocketAddr>().unwrap()]);
}

// Cross-face, both halves of the inbound pair carry its cookie, which names
// the worker the spiral reached (w3): read by position, w1's response goes
// to w3's backup. The cookie that names w1 is the one for this hop.
#[tokio::test]
async fn a_spiralled_response_to_a_dead_worker_ignores_the_other_calls_cookie() {
    let (core, ep) = spiral_core();
    let inbound = format!(
        "<sip:{PROXY_VIP}:5060;w_pri=w3;w_bak=w4;outbound;lr>, \
             <sip:{PROXY_VIP}:5060;w_pri=w3;w_bak=w4;lr>"
    );
    core.handle_response_from(
        spiralled_invite_200(&inbound),
        format!("{W1_POD}:5060").parse().unwrap(),
    )
    .await;

    let sent = ep.sent.lock().unwrap();
    assert_eq!(sent.as_slice(), &[format!("{W2_POD}:5060").parse::<SocketAddr>().unwrap()]);
}

// Both passes reached w1, and the alive set changed between them: the
// later pass recorded another backup (w4). The pair recorded relaying w1's
// request out is the bottom one, and its backup (w2) takes the response.
#[tokio::test]
async fn a_spiralled_response_to_a_dead_worker_reads_the_bottommost_matching_cookie() {
    let (core, ep) = spiral_core();
    let inbound =
        format!("<sip:{PROXY_VIP}:5060;outbound;lr>, <sip:{PROXY_VIP}:5060;w_pri=w1;w_bak=w4;lr>");
    core.handle_response_from(
        spiralled_invite_200(&inbound),
        format!("{W1_POD}:5060").parse().unwrap(),
    )
    .await;

    let sent = ep.sent.lock().unwrap();
    assert_eq!(sent.as_slice(), &[format!("{W2_POD}:5060").parse::<SocketAddr>().unwrap()]);
}

// Control: an Alive worker keeps its response, delivered to the SNAT'd
// received/rport return path per §18.2.2/RFC 3581 — identity changes the
// LOOKUP key, never the send target.
#[tokio::test]
async fn response_to_an_alive_worker_keeps_the_received_rport_target() {
    let (core, ep) = core_with(WorkerHealth::Alive);
    core.handle_response_from(invite_200(), format!("{W1_POD}:5060").parse().unwrap()).await;

    let sent = ep.sent.lock().unwrap();
    assert_eq!(sent.as_slice(), &[format!("{SNAT_NODE}:63522").parse::<SocketAddr>().unwrap()]);
}

// A worker that left the pool can still be bound and still answering. Its
// address stays resolvable as `Dead` for Timer H, so its response follows the
// cookie's backup instead of returning to a node the pool no longer routes to.
#[tokio::test(start_paused = true)]
async fn response_from_a_departed_address_fails_over_to_the_backup() {
    let (core, ep) = core_over(
        registry_without_w1(Clock::test_at(0)),
        Arc::new(BackupStrategy),
        Clock::test_at(0),
    );
    core.handle_response_from(invite_200(), format!("{W1_POD}:5060").parse().unwrap()).await;

    let sent = ep.sent.lock().unwrap();
    assert_eq!(sent.as_slice(), &[format!("{W2_POD}:5060").parse::<SocketAddr>().unwrap()]);
}

// Past Timer H the departed address is a stranger again — no transaction there
// can still be answering — and the response goes where the Via says.
#[tokio::test(start_paused = true)]
async fn response_from_an_expired_departed_address_follows_the_via() {
    let (core, ep) = core_over(
        registry_without_w1(Clock::test_at(0)),
        Arc::new(BackupStrategy),
        Clock::test_at(0),
    );
    tokio::time::advance(std::time::Duration::from_millis(DEPARTED_ADDRESS_TTL_MS)).await;
    core.handle_response_from(invite_200(), format!("{W1_POD}:5060").parse().unwrap()).await;

    let sent = ep.sent.lock().unwrap();
    assert_eq!(sent.as_slice(), &[format!("{SNAT_NODE}:63522").parse::<SocketAddr>().unwrap()]);
}

// An address the pool never knew is not a worker at all (Alice, Bob, an
// external SBC): its response is relayed by the Via, unconditionally.
#[tokio::test]
async fn response_from_an_address_the_pool_never_knew_follows_the_via() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w2",
            ProxyAddr::new(W2_POD, 5060),
        )]));
    let (core, ep) = core_over(reg, Arc::new(BackupStrategy), Clock::test_at(0));
    core.handle_response_from(invite_200(), format!("{W1_POD}:5060").parse().unwrap()).await;

    let sent = ep.sent.lock().unwrap();
    assert_eq!(sent.as_slice(), &[format!("{SNAT_NODE}:63522").parse::<SocketAddr>().unwrap()]);
}

// A departed sender whose cookie names no usable backup: the response is
// dropped rather than delivered to the node that left — the same outcome a
// confirmed-`Dead` sender has.
#[tokio::test(start_paused = true)]
async fn a_departed_sender_with_no_usable_backup_drops_the_response() {
    let (core, ep) = core_over(
        registry_without_w1(Clock::test_at(0)),
        Arc::new(NoBackupStrategy),
        Clock::test_at(0),
    );
    core.handle_response_from(invite_200(), format!("{W1_POD}:5060").parse().unwrap()).await;

    assert!(ep.sent.lock().unwrap().is_empty());
}

// §17.1.2: a non-INVITE response answers a client transaction only its
// sender holds — no other node can consume it — so it follows the Via to
// the received/rport target even when the sender's address has departed,
// cookie echoed back or not (a CANCEL copies no Record-Route, §9.1).
#[tokio::test(start_paused = true)]
async fn a_non_invite_response_from_a_departed_address_follows_the_via() {
    for resp in [cancel_200(), cancel_200_without_record_route(), keepalive_200(), bye_200()] {
        let (core, ep) = core_over(
            registry_without_w1(Clock::test_at(0)),
            Arc::new(BackupStrategy),
            Clock::test_at(0),
        );
        core.handle_response_from(resp, format!("{W1_POD}:5060").parse().unwrap()).await;

        let sent = ep.sent.lock().unwrap();
        assert_eq!(
            sent.as_slice(),
            &[format!("{SNAT_NODE}:63522").parse::<SocketAddr>().unwrap()],
            "neither the backup nor a drop"
        );
    }
}

// The same for a worker the health probe confirmed Dead: its own CANCEL's
// 200 reaches it, so its client transaction completes instead of
// retransmitting the CANCEL to Timer F.
#[tokio::test]
async fn a_non_invite_response_to_a_dead_worker_follows_the_via() {
    for resp in [cancel_200(), keepalive_200(), bye_200()] {
        let (core, ep) = core_with(WorkerHealth::Dead);
        core.handle_response_from(resp, format!("{W1_POD}:5060").parse().unwrap()).await;

        let sent = ep.sent.lock().unwrap();
        assert_eq!(
            sent.as_slice(),
            &[format!("{SNAT_NODE}:63522").parse::<SocketAddr>().unwrap()],
            "neither the backup nor a drop"
        );
    }
}

// A departed sender whose cookie names no usable backup: a non-INVITE
// response still follows the Via — the cookie decides nothing for it.
#[tokio::test(start_paused = true)]
async fn a_non_invite_response_needs_no_usable_backup() {
    let (core, ep) = core_over(
        registry_without_w1(Clock::test_at(0)),
        Arc::new(NoBackupStrategy),
        Clock::test_at(0),
    );
    core.handle_response_from(cancel_200(), format!("{W1_POD}:5060").parse().unwrap()).await;

    let sent = ep.sent.lock().unwrap();
    assert_eq!(sent.as_slice(), &[format!("{SNAT_NODE}:63522").parse::<SocketAddr>().unwrap()]);
}

/// The pool after w1 rebooted at w3's pod: the address it left is tombstoned
/// and the ordinal resolves at its new one.
fn registry_with_w1_moved(clock: Clock) -> Arc<dyn WorkerRegistry> {
    let reg = SimulatedWorkerRegistry::with_clock(
        vec![
            WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060)),
            WorkerEntry::alive("w2", ProxyAddr::new(W2_POD, 5060)),
        ],
        clock,
    );
    reg.set_address("w1", ProxyAddr::new(W3_POD, 5060));
    Arc::new(reg)
}

/// An INVITE response to the address a worker left reaches that worker at
/// its new address — the rebooted pod reclaimed the transaction it answers —
/// rather than the cookie's backup, and with no cookie at all.
#[tokio::test]
async fn response_from_a_departed_address_reaches_its_worker_at_its_new_address() {
    let at_w3 = [format!("{W3_POD}:5060").parse::<SocketAddr>().unwrap()];
    for resp in [invite_200(), worker_bound_200_routed("1 INVITE", "", "")] {
        let (core, ep) = core_over(
            registry_with_w1_moved(Clock::test_at(0)),
            Arc::new(BackupStrategy),
            Clock::test_at(0),
        );
        core.handle_response(resp, format!("{W1_POD}:5060").parse().unwrap()).await;
        assert_eq!(ep.sent.lock().unwrap().as_slice(), &at_w3);
    }
}
