//! A name that resolves to a registry worker's address, on a dual-face proxy:
//! a request arriving internal from a sender that is no worker leaves on the
//! face a name takes, and so do its retransmissions and its CANCEL, though
//! each goes to the address its first copy left for. One transaction states
//! one sent-by (RFC 3261 §17.2.3, §9.1), at an instance that holds the memo as
//! at one that does not.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sip_message::SipMessage;
use tokio::time::{sleep, Instant};

use super::dual_face_tests::{
    dual_face_core_routing, header_lines, parse_msg, Fixture, EXT_PORT, EXT_VIP, INT_PORT, INT_VIP,
    W1,
};
use crate::addr::ProxyAddr;
use crate::observability::metrics::Face;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerRegistry};
use crate::resolver::HostResolver;
use crate::strategies::forward_all::ForwardAllStrategy;

/// The name the requests reach.
const NAME: &str = "callee.example";
/// A sender on the internal face that is no registry worker.
const UNLISTED: &str = "10.244.9.99";
/// Long enough for a send to a name to leave from the resolver's task.
const SETTLE: Duration = Duration::from_millis(50);

fn sa(host: &str, port: u16) -> SocketAddr {
    format!("{host}:{port}").parse().unwrap()
}

/// Resolves every name to W1's address.
struct ToTheWorker;

#[async_trait]
impl HostResolver for ToTheWorker {
    async fn resolve(&self, _host: &str, port: u16, _deadline: Instant) -> Vec<SocketAddr> {
        vec![sa(W1, port)]
    }
}

/// A dual-face proxy in front of W1, whose strategy places every call on
/// [`NAME`], which resolves to W1's address.
fn proxy_to_the_name() -> Fixture {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(W1, 5060),
        )]));
    let strategy = Arc::new(ForwardAllStrategy::new(ProxyAddr::new(NAME, 5060)));
    dual_face_core_routing(reg, strategy, Some(Arc::new(ToTheWorker)))
}

/// The unlisted sender's INVITE or its CANCEL (§9.1), one transaction.
fn unlisted_request(method: &str) -> SipMessage {
    parse_msg(&format!(
        "{method} sip:bob@{NAME} SIP/2.0\r\n\
Via: SIP/2.0/UDP {UNLISTED}:5060;branch=z9hG4bKunlisted1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=a\r\n\
To: <sip:bob@{NAME}>\r\n\
Call-ID: named-worker-face@example.com\r\n\
CSeq: 1 {method}\r\n\
Contact: <sip:alice@{UNLISTED}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// The unlisted sender's BYE of a dialog whose route set is the proxy's own
/// Route, its cookie naming [`NAME`].
fn unlisted_bye_under_the_cookie() -> SipMessage {
    parse_msg(&format!(
        "BYE sip:bob@{NAME} SIP/2.0\r\n\
Via: SIP/2.0/UDP {UNLISTED}:5060;branch=z9hG4bKunlistedbye;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=a\r\n\
To: <sip:bob@{NAME}>;tag=b\r\n\
Call-ID: named-worker-face-bye@example.com\r\n\
CSeq: 2 BYE\r\n\
Route: <sip:{INT_VIP}:{INT_PORT};target={NAME}:5060;lr>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// The proxy routes `request` from the unlisted sender; returns the one
/// datagram it sent, by the face that sent it.
async fn forwarded(fx: &Fixture, request: &SipMessage) -> (&'static str, SocketAddr, Vec<u8>) {
    fx.core.route_request(request, sa(UNLISTED, 5060), Face::Internal).await;
    sleep(SETTLE).await;
    let mut sent = fx.take_sent();
    assert_eq!(sent.len(), 1, "one datagram: {sent:?}");
    sent.remove(0)
}

/// The top Via line of `bytes`.
fn top_via(bytes: &[u8]) -> String {
    header_lines(bytes, "Via").remove(0)
}

/// The first copy of `request` leaves for the name's address on the face a
/// name takes, under that face's sent-by.
async fn first_copy(fx: &Fixture, request: &SipMessage) -> Vec<u8> {
    let (face, dst, first) = forwarded(fx, request).await;
    assert_eq!((face, dst), ("ext", sa(W1, 5060)), "it leaves for the name's address");
    let sent_by = format!("SIP/2.0/UDP {EXT_VIP}:{EXT_PORT};");
    assert!(top_via(&first).starts_with(&sent_by), "{}", top_via(&first));
    first
}

/// The unlisted sender's Timer A copy of an INVITE the strategy placed on the
/// name (§17.1.1.2) is its first forward again (§16.11).
#[tokio::test(start_paused = true)]
async fn a_retransmitted_invite_placed_on_a_name_resolving_to_a_worker_keeps_its_face() {
    let fx = proxy_to_the_name();
    let invite = first_copy(&fx, &unlisted_request("INVITE")).await;

    let (face, dst, copy) = forwarded(&fx, &unlisted_request("INVITE")).await;
    assert_eq!((face, dst), ("ext", sa(W1, 5060)), "{}", String::from_utf8_lossy(&copy));
    assert_eq!(copy, invite, "§16.11: the copy is the first forward again");
}

/// The unlisted sender's CANCEL goes where the INVITE went, under the
/// INVITE's top Via (§9.1). The sender cancels before any provisional, which
/// §9.1 discourages but allows: this row relays no response to it (a final
/// arriving external for a sender that is no worker is dropped).
#[tokio::test(start_paused = true)]
async fn the_cancel_of_an_invite_to_a_name_resolving_to_a_worker_keeps_its_face() {
    let fx = proxy_to_the_name();
    let invite = first_copy(&fx, &unlisted_request("INVITE")).await;

    let (face, dst, cancel) = forwarded(&fx, &unlisted_request("CANCEL")).await;
    assert!(cancel.starts_with(b"CANCEL "), "{}", String::from_utf8_lossy(&cancel));
    assert_eq!((face, dst), ("ext", sa(W1, 5060)), "the CANCEL goes where the INVITE went");
    assert_eq!(top_via(&cancel), top_via(&invite), "§9.1: the INVITE's top Via");
}

/// The unlisted sender's Timer E copy of a BYE its Route's cookie places on
/// the name (§17.1.2.2) is its first forward again (§16.11).
#[tokio::test(start_paused = true)]
async fn a_retransmitted_request_under_a_cookie_naming_a_name_resolving_to_a_worker_keeps_its_face()
{
    let fx = proxy_to_the_name();
    let bye = unlisted_bye_under_the_cookie();
    let first = first_copy(&fx, &bye).await;

    let (face, dst, copy) = forwarded(&fx, &bye).await;
    assert_eq!((face, dst), ("ext", sa(W1, 5060)), "{}", String::from_utf8_lossy(&copy));
    assert_eq!(copy, first, "§16.11: the copy is the first forward again");
}
