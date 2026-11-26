//! The end of a call a test drives by raw datagrams through the proxy: the
//! callee answers the request it was forwarded, refusing an INVITE, the
//! caller ACKs the refusal, and each message is read where it lands.

use std::net::SocketAddr;
use std::time::Duration;

use sip_message::generators::{generate_ack_for_non_2xx, generate_response, GenerateResponseOpts};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::UdpEndpoint;

/// The callee's To-tag on its refusal.
const CALLEE_TAG: &str = "callee-busy";

/// The next datagram `at` reads, within 2 s (virtual time on a paused clock).
async fn next_raw(at: &dyn UdpEndpoint, what: &str) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(2), at.recv())
        .await
        .unwrap_or_else(|_| panic!("{what} arrives"))
        .expect("queue open")
        .raw
        .to_vec()
}

/// The request in `raw`.
pub fn request_of(raw: &[u8]) -> SipRequest {
    match CustomParser::new().parse(raw) {
        Ok(SipMessage::Request(req)) => req,
        other => panic!("a request: {other:?}"),
    }
}

/// The hop a response to `request` goes to: its top Via's sent-by (RFC 3261
/// §18.2.2), the proxy's own Via on a forwarded request.
fn response_hop(request: &SipRequest) -> SocketAddr {
    let (host, port) = request.top_via().host_port();
    format!("{host}:{port}").parse().expect("the top Via names an address")
}

/// The callee refuses with 486 the INVITE it read in `forward`, sending it to
/// the proxy's Via; the 486 reaches the caller, and the caller's ACK of it
/// (RFC 3261 §17.1.1.3), sent to `entry` where its INVITE went, reaches the
/// callee: the call is over. `invite` is the INVITE as the caller sent it.
/// Returns the 486 the caller read.
pub async fn busy_at_callee(
    entry: SocketAddr,
    caller: &dyn UdpEndpoint,
    invite: &[u8],
    callee: &dyn UdpEndpoint,
    forward: &[u8],
) -> SipResponse {
    let relayed = answered_at_callee(caller, callee, forward, 486, "Busy Here").await;
    ack_final(entry, caller, invite, &relayed).await;
    let acked = request_of(&next_raw(callee, "the ACK").await);
    assert_eq!(acked.method(), "ACK", "the caller's ACK reaches the callee");
    relayed
}

/// The callee answers `status` to the request it read in `forward`, sending
/// it to the proxy's Via, and the answer reaches the caller. Returns the
/// answer the caller read.
pub async fn answered_at_callee(
    caller: &dyn UdpEndpoint,
    callee: &dyn UdpEndpoint,
    forward: &[u8],
    status: u16,
    reason: &str,
) -> SipResponse {
    let forwarded = request_of(forward);
    let opts = GenerateResponseOpts { to_tag: Some(CALLEE_TAG.into()), ..Default::default() };
    let answer = generate_response(&forwarded, status, reason, &opts).expect("an answer");
    callee.send_to(answer.image(), response_hop(&forwarded)).await.expect("sent");
    let relayed = response_of(&next_raw(caller, "the callee's answer").await);
    assert_eq!(relayed.status(), status, "the callee's answer reaches the caller");
    relayed
}

/// The caller ACKs `refusal`, a non-2xx final to its `invite` (RFC 3261
/// §17.1.1.3), sending it to `entry` where the INVITE went.
pub async fn ack_final(
    entry: SocketAddr,
    caller: &dyn UdpEndpoint,
    invite: &[u8],
    refusal: &SipResponse,
) {
    let ack = generate_ack_for_non_2xx(&request_of(invite), refusal, &[]).expect("an ACK");
    caller.send_to(ack.image(), entry).await.expect("sent");
}

/// The response in `raw`.
pub fn response_of(raw: &[u8]) -> SipResponse {
    match CustomParser::new().parse(raw) {
        Ok(SipMessage::Response(resp)) => resp,
        other => panic!("a response: {other:?}"),
    }
}
