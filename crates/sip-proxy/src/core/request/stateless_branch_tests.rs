//! RFC 3261 §16.11: the branch a stateless proxy pushes on its Via is a
//! function of the message. A retransmission of a request, and the CANCEL and
//! the non-2xx ACK for it, therefore map to the branch the first forward
//! carried — at ANY instance of the proxy, with no memo of the original.
//! Two distinct transactions never merge onto one branch.

use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::header::{HeaderValue, BRANCH_MAGIC_COOKIE};
use sip_message::parser::custom::CustomParser;
use sip_message::parser::SipParserLimits;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork, UdpEndpoint};

use crate::addr::ProxyAddr;
use crate::cancel_lru::{call_id_cseq_key, INVITE_ENTRY_TTL_MS};
use crate::core::ProxyCore;
use crate::core::ProxyCoreBuilder;
use crate::observability::metrics::RoutingDecisionKind;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerRegistry};
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::RoutingStrategy;

/// The worker that originates the requests under test (its Via sent-by, which
/// the registry knows) and the downstream UA the R-URI names.
const WORKER: &str = "10.244.5.8";
const DOWNSTREAM: &str = "10.0.0.50";
/// The address BOTH instances advertise — a VIP pair is one sent-by to the
/// downstream, whose server transaction matches on branch + sent-by + method
/// (§17.2.3). Each instance still owns its own socket.
const PROXY_VIP: &str = "172.20.255.250";
const NODE_A: &str = "10.244.1.11";
const NODE_B: &str = "10.244.1.12";

/// One simulated fabric: the downstream UA reads whatever any proxy on it
/// forwards, so the pushed top-Via branch is read off the wire, not off an
/// internal memo.
struct Fabric {
    net: SimulatedSignalingNetwork,
    downstream: Box<dyn UdpEndpoint>,
}

async fn fabric() -> Fabric {
    let net = SimulatedSignalingNetwork::new(1);
    let downstream = net
        .bind_udp(BindUdpOpts::new(format!("{DOWNSTREAM}:5060").parse().unwrap(), 64))
        .await
        .unwrap();
    Fabric { net, downstream }
}

impl Fabric {
    /// One instance of the VIP pair, bound on its own node address and
    /// advertising the shared VIP. Each gets its own `IdGen`, so a branch two
    /// instances agree on can only come from the message.
    async fn proxy(&self, node: &str) -> ProxyCore {
        let ep = self
            .net
            .bind_udp(BindUdpOpts::new(format!("{node}:5060").parse().unwrap(), 64))
            .await
            .unwrap();
        let reg: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
                "w1",
                ProxyAddr::new(WORKER, 5060),
            )]));
        let strategy: Arc<dyn RoutingStrategy> =
            Arc::new(ForwardAllStrategy::new(ProxyAddr::new(WORKER, 5060)));
        ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
            .clock(Clock::test_at(0))
            .build(ep)
    }

    /// The WHOLE top Via of the next datagram the downstream UA receives —
    /// branch AND sent-by, the pair §17.2.3 matches a server transaction on.
    async fn pushed_via(&self) -> String {
        let pkt = self.downstream.recv().await.expect("the proxy forwards a datagram");
        let SipMessage::Request(req) = CustomParser::default().parse(&pkt.raw).unwrap() else {
            panic!("a forwarded request")
        };
        req.top_via().to_wire()
    }

    /// Just its `branch` token, for the shape and no-merge assertions.
    async fn pushed_branch(&self) -> String {
        let pkt = self.downstream.recv().await.expect("the proxy forwards a datagram");
        let SipMessage::Request(req) = CustomParser::default().parse(&pkt.raw).unwrap() else {
            panic!("a forwarded request")
        };
        req.top_via().branch().expect("the proxy pushes a branch").to_string()
    }
}

fn parse_req(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

/// A worker-originated INVITE toward the downstream UA: top Via sent-by is the
/// worker, R-URI is the UA, so the request leaves at the R-URI.
fn invite(call_id: &str, from_tag: &str, cseq: u32, branch: &str) -> SipMessage {
    parse_req(&format!(
        "INVITE sip:bob@{DOWNSTREAM}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {WORKER}:5060;branch={branch};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{WORKER}>;tag={from_tag}\r\n\
To: <sip:bob@{DOWNSTREAM}>\r\n\
Call-ID: {call_id}\r\n\
CSeq: {cseq} INVITE\r\n\
Contact: <sip:alice@{WORKER}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// The CANCEL for that INVITE. RFC 3261 §9.1: same Call-ID, From tag, CSeq
/// number and top-Via branch as the INVITE it cancels.
fn cancel(call_id: &str, from_tag: &str, cseq: u32, branch: &str) -> SipMessage {
    parse_req(&format!(
        "CANCEL sip:bob@{DOWNSTREAM}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {WORKER}:5060;branch={branch};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{WORKER}>;tag={from_tag}\r\n\
To: <sip:bob@{DOWNSTREAM}>\r\n\
Call-ID: {call_id}\r\n\
CSeq: {cseq} CANCEL\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// A pre-RFC-3261 upstream: a top Via with no `branch` parameter at all. The
/// §8.1.1.7 magic-cookie gate is a wire-grammar rule, so such a message is
/// hydrated rather than read off the wire.
fn branchless_options(call_id: &str, cseq: u32) -> SipMessage {
    let limits = SipParserLimits { wire_grammar: false, ..SipParserLimits::default() };
    let raw = format!(
        "OPTIONS sip:bob@{DOWNSTREAM}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {WORKER}:5060;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{WORKER}>;tag=legacy-a\r\n\
To: <sip:bob@{DOWNSTREAM}>;tag=legacy-b\r\n\
Call-ID: {call_id}\r\n\
CSeq: {cseq} OPTIONS\r\n\
Content-Length: 0\r\n\r\n"
    );
    CustomParser::with_limits(limits).parse(raw.as_bytes()).unwrap()
}

fn worker_src() -> std::net::SocketAddr {
    format!("{WORKER}:5060").parse().unwrap()
}

// §16.11's whole point: a second proxy instance that never saw the INVITE
// still puts the CANCEL on the INVITE's branch. A branch drawn at random
// would not match the downstream server transaction (§17.2.3 keys on branch
// + sent-by + method), which answers 481 and keeps ringing.
#[tokio::test(start_paused = true)]
async fn cancel_at_a_second_proxy_instance_carries_the_invites_branch() {
    let f = fabric().await;
    let first = f.proxy(NODE_A).await;
    let second = f.proxy(NODE_B).await;

    first.route_request(&invite("split-cancel@test", "tag-a", 1, "z9hG4bKup1"), worker_src()).await;
    let invite_via = f.pushed_via().await;

    let out = second
        .route_request(&cancel("split-cancel@test", "tag-a", 1, "z9hG4bKup1"), worker_src())
        .await;
    let cancel_via = f.pushed_via().await;

    assert_eq!(out.decision, RoutingDecisionKind::Cancel);
    assert_eq!(
        out.target,
        Some(ProxyAddr::new(DOWNSTREAM, 5060)),
        "the CANCEL goes where the INVITE went"
    );
    assert_eq!(
        cancel_via, invite_via,
        "§17.2.3 matches on branch + sent-by: both must be the INVITE's, memo or no memo"
    );
}

// The same mapping when the INVITE's memo has aged out of the one instance
// that wrote it: a CANCEL is legal for the whole INVITE window and beyond.
#[tokio::test(start_paused = true)]
async fn cancel_after_the_invite_memo_expired_carries_the_same_branch() {
    let f = fabric().await;
    let core = f.proxy(NODE_A).await;

    core.route_request(&invite("expired-memo@test", "tag-a", 4, "z9hG4bKup2"), worker_src()).await;
    let invite_via = f.pushed_via().await;

    tokio::time::advance(Duration::from_millis(INVITE_ENTRY_TTL_MS + 1_000)).await;
    assert!(
        core.cancel_lru.lookup(&call_id_cseq_key("expired-memo@test", Some("tag-a"), 4)).is_none(),
        "the INVITE entry must be gone, or this proves nothing"
    );

    let out = core
        .route_request(&cancel("expired-memo@test", "tag-a", 4, "z9hG4bKup2"), worker_src())
        .await;
    let cancel_via = f.pushed_via().await;

    assert_eq!(
        out.target,
        Some(ProxyAddr::new(DOWNSTREAM, 5060)),
        "the CANCEL follows the R-URI the INVITE took"
    );
    assert_eq!(
        cancel_via, invite_via,
        "§16.11: an expired memo cannot change what the branch is a function of"
    );
}

// §16.11's retransmission clause across instances: the re-sent copy reaching
// a different proxy still rides the branch the first copy got, so the
// downstream transaction absorbs it as a retransmission rather than opening a
// second transaction at the same CSeq.
#[tokio::test(start_paused = true)]
async fn retransmission_through_a_second_proxy_carries_the_same_branch() {
    let f = fabric().await;
    let first = f.proxy(NODE_A).await;
    let second = f.proxy(NODE_B).await;
    let req = invite("split-rtx@test", "tag-a", 2, "z9hG4bKup3");

    first.route_request(&req, worker_src()).await;
    let original = f.pushed_via().await;
    second.route_request(&req, worker_src()).await;
    let repeat = f.pushed_via().await;

    assert_eq!(
        repeat, original,
        "§16.11: the retransmission reaches the same server transaction as the first copy"
    );
}

// A branch function of the RECEIVED branch alone would merge two distinct
// transactions whose upstream spent one token twice (a UA that restarts and
// violates §8.1.1.7 uniqueness). Folding Call-ID, From tag and CSeq number in
// keeps them apart while leaving the §16.11 mapping intact.
#[tokio::test(start_paused = true)]
async fn one_received_branch_token_on_two_call_ids_maps_to_two_branches() {
    let f = fabric().await;
    let core = f.proxy(NODE_A).await;

    core.route_request(&invite("collide-a@test", "tag-a", 1, "z9hG4bKdup"), worker_src()).await;
    let first = f.pushed_branch().await;
    core.route_request(&invite("collide-b@test", "tag-a", 1, "z9hG4bKdup"), worker_src()).await;
    let second = f.pushed_branch().await;

    assert_ne!(first, second, "two transactions must not merge onto one downstream branch");
}

// §8.1.1.7 shape: the magic cookie, then an opaque token. It is this proxy's
// own value, never the upstream's.
#[tokio::test(start_paused = true)]
async fn the_pushed_branch_carries_the_magic_cookie_and_is_opaque_hex() {
    let f = fabric().await;
    let core = f.proxy(NODE_A).await;

    core.route_request(&invite("shape-1@test", "tag-a", 1, "z9hG4bKup4"), worker_src()).await;
    let branch = f.pushed_branch().await;

    let token = branch.strip_prefix(BRANCH_MAGIC_COOKIE).expect("§8.1.1.7 magic cookie");
    assert_eq!(token.len(), 16, "16 opaque characters after the cookie, got {branch}");
    assert!(token.bytes().all(|c| c.is_ascii_hexdigit()), "hex token, got {branch}");
    assert_ne!(branch, "z9hG4bKup4", "the pushed branch is ours, not the received one");
}

// The §16.11 alternate input set, for a received Via with no branch: the
// topmost Via, the To and From tags, the Call-ID, the CSeq NUMBER and the
// Request-URI. Two copies of one request map together; a different CSeq
// number is a different transaction and must not.
#[tokio::test(start_paused = true)]
async fn a_branchless_received_via_maps_by_the_alternate_input_set() {
    let f = fabric().await;
    let core = f.proxy(NODE_A).await;

    core.route_request(&branchless_options("legacy-1@test", 3), worker_src()).await;
    let original = f.pushed_branch().await;
    core.route_request(&branchless_options("legacy-1@test", 3), worker_src()).await;
    let repeat = f.pushed_branch().await;
    core.route_request(&branchless_options("legacy-1@test", 4), worker_src()).await;
    let other_cseq = f.pushed_branch().await;

    assert_eq!(repeat, original, "§16.11: a retransmission maps to the first copy's branch");
    assert_ne!(other_cseq, original, "a different CSeq number is a different transaction");
}
