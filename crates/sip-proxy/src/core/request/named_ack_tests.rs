//! A worker's INVITE to a name the peer answers with a non-2xx final, the
//! name resolving elsewhere by then: the ACK of that final goes to the
//! address, port and transport the INVITE was sent to (RFC 3261 §17.1.1.3,
//! RFC 3263 §4), on the instance that holds the INVITE's entry.
//!
//! Sends to a name leave from the resolver's task when it looks the name up,
//! so each step lets that task run ([`SETTLE`]) before reading the wire.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sip_message::SipMessage;
use sip_txn::timers::{T1, T2, TIMER_H};
use tokio::time::{sleep, Instant};

use crate::core::dual_face_tests::{
    dual_face_core_over, header_lines, parse_msg, Fixture, INT_PORT, INT_VIP, W1,
};
use crate::observability::metrics::{Face, RoutingDecisionKind};
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::WorkerEntry;
use crate::resolver::{HostResolver, ResolverConfig};
use crate::ProxyAddr;

/// The peer's name, which the worker's INVITE and ACK carry in their
/// Request-URI.
const PEER_NAME: &str = "peer.example.net";
const PEER_PORT: u16 = 5060;
/// Where the name resolves when the INVITE leaves: the peer's address.
const AT_INVITE: &str = "192.0.2.1";
/// Where the name resolves from right after the INVITE left.
const MOVED_TO: &str = "192.0.2.2";
/// Long enough for a send to a name to leave from the resolver's task.
const SETTLE: Duration = Duration::from_millis(20);

/// Resolves [`PEER_NAME`] to the address the test last moved it to, and
/// nothing else.
struct MovingName(Mutex<IpAddr>);

impl MovingName {
    fn at(host: &str) -> Self {
        Self(Mutex::new(host.parse().expect("an address")))
    }

    fn move_to(&self, host: &str) {
        *self.0.lock().unwrap() = host.parse().expect("an address");
    }
}

#[async_trait]
impl HostResolver for MovingName {
    async fn resolve(&self, host: &str, port: u16, _deadline: Instant) -> Vec<SocketAddr> {
        if host != PEER_NAME {
            return Vec::new();
        }
        vec![SocketAddr::new(*self.0.lock().unwrap(), port)]
    }
}

fn peer() -> SocketAddr {
    SocketAddr::new(AT_INVITE.parse().expect("an address"), PEER_PORT)
}

fn worker() -> SocketAddr {
    format!("{W1}:5060").parse().expect("an address")
}

/// The worker's B-leg INVITE to the peer's name, or its §17.1.1.3 ACK of a
/// non-2xx final (the INVITE's Request-URI, top Via, Route and CSeq number,
/// the final's To).
fn worker_request(method: &str, call_id: &str) -> SipMessage {
    let to_tag = if method == "ACK" { ";tag=peer-1" } else { "" };
    let contact = if method == "INVITE" {
        format!("Contact: <sip:b2bua@{W1}:5060;leg=b>\r\n")
    } else {
        String::new()
    };
    parse_msg(&format!(
        "{method} sip:bob@{PEER_NAME}:{PEER_PORT} SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKnamed1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:bob@{PEER_NAME}>{to_tag}\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 {method}\r\n\
{contact}\
Route: <sip:{INT_VIP}:{INT_PORT};lr>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// The peer's answer `status` to the forwarded INVITE `invite`: its Via lines
/// echoed (§8.2.6.2), its To tagged; a provisional also carries the peer's
/// Contact and the INVITE's Record-Route (§12.1.1).
fn peer_response(status: &str, invite: &[u8], call_id: &str) -> SipMessage {
    let mut head = format!("SIP/2.0 {status}\r\n");
    for via in header_lines(invite, "Via") {
        head.push_str(&format!("Via: {via}\r\n"));
    }
    if status.starts_with('1') {
        for rr in header_lines(invite, "Record-Route") {
            head.push_str(&format!("Record-Route: {rr}\r\n"));
        }
        head.push_str(&format!("Contact: <sip:bob@{AT_INVITE}:{PEER_PORT}>\r\n"));
    }
    parse_msg(&format!(
        "{head}From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:bob@{PEER_NAME}>;tag=peer-1\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

fn top_via_branch(bytes: &[u8]) -> String {
    let vias = header_lines(bytes, "Via");
    let branch = vias[0].split("branch=").nth(1).expect("a branch");
    branch.split([';', ',', ' ']).next().expect("a branch token").to_string()
}

/// The peer relays `response` to the proxy from its address; the proxy
/// relays it to the worker, and nothing else.
async fn peer_answers(fx: &Fixture, response: SipMessage) {
    let SipMessage::Response(response) = response else { panic!("a response") };
    let status = response.status();
    fx.core.handle_response(response, peer(), Face::External).await;
    let sent = fx.take_sent();
    let [(face, dst, bytes)] = sent.as_slice() else { panic!("{status} relays once: {sent:?}") };
    assert_eq!((*face, *dst), ("int", worker()), "the {status} relays to the worker");
    assert!(bytes.starts_with(format!("SIP/2.0 {status} ").as_bytes()));
}

/// A proxy in front of one worker, resolving [`PEER_NAME`] at [`AT_INVITE`]
/// until the test moves it.
fn proxy_to_the_name() -> (Fixture, Arc<MovingName>) {
    let reg = Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
        "w1",
        ProxyAddr::new(W1, 5060),
    )]));
    let name = Arc::new(MovingName::at(AT_INVITE));
    (dual_face_core_over(reg, Some(name.clone())), name)
}

/// The worker's INVITE leaves for the name at [`AT_INVITE`]; the name then
/// resolves to [`MOVED_TO`]. The peer rings for `ring`, then answers 486 and
/// retransmits it on Timer G until it sees the ACK or Timer H fires (§17.2.1);
/// each copy relays to the worker, which ACKs each one (§17.1.1.2). Returns
/// where each of the worker's ACKs was sent. `prewarmed`: the name is pinned
/// before the INVITE, so the INVITE is a cache hit and the pinned refresh
/// moves the entry; else the INVITE's send resolves the name and the entry
/// expires during the ring.
async fn acks_of_a_busy_peer(call_id: &str, prewarmed: bool, ring: Duration) -> Vec<SocketAddr> {
    let (fx, name) = proxy_to_the_name();
    if prewarmed {
        fx.core.prewarm_named_targets(vec![ProxyAddr::new(PEER_NAME, PEER_PORT)]);
        sleep(Duration::from_secs(1)).await;
    }

    fx.core.route_request(&worker_request("INVITE", call_id), worker(), Face::Internal).await;
    sleep(SETTLE).await;
    let sent = fx.take_sent();
    let [(face, dst, invite)] = sent.as_slice() else { panic!("one INVITE: {sent:?}") };
    assert_eq!((*face, *dst), ("ext", peer()), "the INVITE leaves for the name's address");
    let invite = invite.clone();
    let invite_branch = top_via_branch(&invite);
    name.move_to(MOVED_TO);

    peer_answers(&fx, peer_response("180 Ringing", &invite, call_id)).await;
    sleep(ring).await;

    let mut acks = Vec::new();
    let (mut since_final, mut interval) = (0, T1);
    loop {
        peer_answers(&fx, peer_response("486 Busy Here", &invite, call_id)).await;
        let ack = worker_request("ACK", call_id);
        let outcome = fx.core.route_request(&ack, worker(), Face::Internal).await;
        assert_eq!(outcome.decision, RoutingDecisionKind::AckHop, "the ACK reads the INVITE's hop");
        sleep(SETTLE).await;
        for (face, dst, bytes) in fx.take_sent() {
            assert!(bytes.starts_with(b"ACK "), "{}", String::from_utf8_lossy(&bytes));
            assert_eq!(face, "ext");
            assert_eq!(top_via_branch(&bytes), invite_branch, "§16.11: the INVITE's branch");
            acks.push(dst);
        }
        if acks.contains(&peer()) || since_final + interval >= TIMER_H {
            break;
        }
        sleep(Duration::from_millis(interval) - SETTLE).await;
        since_final += interval;
        interval = (interval * 2).min(T2);
    }

    sleep(Duration::from_millis(TIMER_H) + Duration::from_secs(1)).await;
    let sent = fx.take_sent();
    assert!(sent.is_empty(), "nothing follows the transaction's end: {sent:?}");
    acks
}

/// The INVITE's send resolves the name; the entry expires while the peer
/// rings, so the name resolves anew after the final.
#[tokio::test(start_paused = true)]
async fn the_ack_of_a_non_2xx_goes_where_the_invite_went_after_the_names_entry_expired() {
    let past_the_entry =
        Duration::from_millis(ResolverConfig::default().positive_ttl_ms) + Duration::from_secs(1);
    let acks = acks_of_a_busy_peer("named-ack-expired", false, past_the_entry).await;
    assert_eq!(acks, vec![peer()], "one ACK, to the INVITE's address; the peer stops");
}

/// The INVITE is a cache hit on a pinned name, whose refresh during the ring
/// (5 s before the entry's TTL) stores the name's new address; the final
/// comes between that refresh and the TTL.
#[tokio::test(start_paused = true)]
async fn the_ack_of_a_non_2xx_goes_where_the_invite_went_after_the_names_refresh() {
    let past_the_refresh =
        Duration::from_millis(ResolverConfig::default().positive_ttl_ms) - Duration::from_secs(3);
    let acks = acks_of_a_busy_peer("named-ack-refreshed", true, past_the_refresh).await;
    assert_eq!(acks, vec![peer()], "one ACK, to the INVITE's address; the peer stops");
}

/// The worker's INVITE leaves for the pinned name 300 ms before the name's
/// refresh stores its new address; the worker's Timer A copy (§17.1.1.2),
/// after that refresh, goes where the first copy went: one transaction, one
/// destination (§17.1.1.3). The peer rings, answers 486 and sees the ACK.
#[tokio::test(start_paused = true)]
async fn a_retransmitted_invite_goes_where_its_first_copy_went_after_the_names_refresh() {
    let call_id = "named-invite-rtx";
    let (fx, name) = proxy_to_the_name();
    fx.core.prewarm_named_targets(vec![ProxyAddr::new(PEER_NAME, PEER_PORT)]);
    let refresh = Duration::from_millis(ResolverConfig::default().refresh_after_ms());
    sleep(refresh - Duration::from_millis(300)).await;

    let invite = worker_request("INVITE", call_id);
    fx.core.route_request(&invite, worker(), Face::Internal).await;
    sleep(SETTLE).await;
    let sent = fx.take_sent();
    let [(face, dst, first)] = sent.as_slice() else { panic!("one INVITE: {sent:?}") };
    assert_eq!((*face, *dst), ("ext", peer()), "the INVITE leaves for the name's address");
    let first = first.clone();
    name.move_to(MOVED_TO);

    sleep(Duration::from_millis(T1) - SETTLE).await;
    fx.core.route_request(&invite, worker(), Face::Internal).await;
    sleep(SETTLE).await;
    let sent = fx.take_sent();
    let [(face, dst, copy)] = sent.as_slice() else { panic!("one INVITE copy: {sent:?}") };
    assert_eq!(copy, &first, "§16.11: the copy is the first forward again");
    assert_eq!((*face, *dst), ("ext", peer()), "the copy goes where the first went");

    peer_answers(&fx, peer_response("180 Ringing", &first, call_id)).await;
    peer_answers(&fx, peer_response("486 Busy Here", &first, call_id)).await;
    fx.core.route_request(&worker_request("ACK", call_id), worker(), Face::Internal).await;
    sleep(SETTLE).await;
    let sent = fx.take_sent();
    let [(face, dst, ack)] = sent.as_slice() else { panic!("one ACK: {sent:?}") };
    assert!(ack.starts_with(b"ACK "), "{}", String::from_utf8_lossy(ack));
    assert_eq!((*face, *dst), ("ext", peer()), "the ACK goes where the INVITE went");

    sleep(Duration::from_millis(TIMER_H) + Duration::from_secs(1)).await;
    let sent = fx.take_sent();
    assert!(sent.is_empty(), "nothing follows the transaction's end: {sent:?}");
}
