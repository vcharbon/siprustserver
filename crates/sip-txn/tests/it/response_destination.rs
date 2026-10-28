//! One destination per server transaction (RFC 3261 §18.2.2, RFC 3581 §4):
//! the server transport stamps `received`/`rport` on the top Via of what it
//! receives (§18.2.1), and every response of the transaction leaves to that
//! Via's response target and echoes the stamp, whatever the status and
//! whatever address the TU names. The peer sends from `PEER`; its Via names
//! another address, where a decoy socket listens and must hear nothing.

use crate::common;
use common::*;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::Rport;
use sip_message::{SipMessage, SipRequest, SipResponse};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_txn::TransactionEvent;

const TRANSIT: u64 = 5;

/// A request from the peer whose top Via names `sent_by` with `via_tail`
/// after its branch.
fn request_via(
    method: &str,
    sent_by: &str,
    via_tail: &str,
    branch: &str,
    call_id: &str,
) -> Vec<u8> {
    let to_tag = if method == "ACK" { ";tag=uas-tag" } else { "" };
    format!(
        "{method} sip:b2bua@127.0.0.1:5070 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {sent_by};branch={branch}{via_tail}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:caller@10.0.0.1>;tag=caller-tag\r\n\
         To: <sip:b2bua@127.0.0.1:5070>{to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 {method}\r\n\
         Contact: <sip:caller@10.0.0.1:5555>\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A socket on `at` that must never be answered.
async fn decoy(stack: &Stack, at: &str) -> Box<dyn UdpEndpoint> {
    stack.net.bind_udp(BindUdpOpts::new(addr(at), 64)).await.expect("bind decoy")
}

/// The request the layer handed up, as the TU reads it.
fn handed_up(events: Vec<TransactionEvent>, method: &str) -> SipRequest {
    events
        .into_iter()
        .find_map(|e| match e {
            TransactionEvent::Message { message, .. } => match *message {
                SipMessage::Request(r) if r.method() == method => Some(r),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_else(|| panic!("the {method} reaches the TU"))
}

/// The TU's answer to `req`, built from the request it was handed.
fn answer(req: &SipRequest, status: u16, reason: &str) -> SipResponse {
    let opts = GenerateResponseOpts { to_tag: Some("uas-tag".into()), ..Default::default() };
    generate_response(req, status, reason, &opts).expect("the TU's answer")
}

fn responses(out: &[SipMessage], status: u16) -> Vec<&SipResponse> {
    out.iter()
        .filter_map(|m| match m {
            SipMessage::Response(r) if r.status() == status => Some(r),
            _ => None,
        })
        .collect()
}

/// The one `status` response the peer received, its top Via echoing
/// `received` and the `rport` it carries.
fn the_echoing(out: &[SipMessage], status: u16, received: &str, rport: Option<u16>) {
    let got = responses(out, status);
    assert_eq!(got.len(), 1, "one {status} at the source: {out:?}");
    let via = got[0].top_via();
    assert_eq!(via.received(), Some(received), "{status} echoes received (§18.2.1): {via:?}");
    assert_eq!(via.rport(), rport.map(Rport::Observed), "{status} echoes rport (RFC 3581 §4)");
}

#[tokio::test(start_paused = true)]
async fn an_invite_asking_rport_is_answered_at_its_source_whatever_the_tu_names() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let sent_by = "10.0.0.1:5060";
    let elsewhere = decoy(&stack, sent_by).await;
    let (branch, call_id) = ("z9hG4bK-dest-rport", "dest-rport");

    stack.inject(&request_via("INVITE", sent_by, ";rport", branch, call_id)).await;
    elapse_ms(60).await;
    the_echoing(&stack.drain_peer(), 100, "10.0.0.1", Some(5555));
    let invite = handed_up(stack.drain_events(), "INVITE");

    // The TU names the sent-by; the transaction holds the source.
    let busy = answer(&invite, 486, "Busy Here");
    stack.txn.send_response(busy, addr(sent_by)).await.unwrap();
    elapse_ms(60).await;
    the_echoing(&stack.drain_peer(), 486, "10.0.0.1", Some(5555));

    // Timer G repeats the final to the same place (§17.2.1).
    elapse_ms(600).await;
    assert_eq!(responses(&stack.drain_peer(), 486).len(), 1, "the Timer G repeat at the source");

    stack.inject(&request_via("ACK", sent_by, ";rport", branch, call_id)).await;
    elapse_ms(60).await;
    assert!(elsewhere.try_recv().is_none(), "nothing reaches the sent-by");
}

#[tokio::test(start_paused = true)]
async fn an_invite_from_another_host_than_its_sent_by_is_answered_at_received() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    // Same port as the source, another host: received + sent-by port.
    let sent_by = "192.0.2.50:5555";
    let elsewhere = decoy(&stack, sent_by).await;
    let (branch, call_id) = ("z9hG4bK-dest-received", "dest-received");

    stack.inject(&request_via("INVITE", sent_by, "", branch, call_id)).await;
    elapse_ms(60).await;
    the_echoing(&stack.drain_peer(), 100, "10.0.0.1", None);
    let invite = handed_up(stack.drain_events(), "INVITE");

    let busy = answer(&invite, 603, "Decline");
    stack.txn.send_response(busy, addr(sent_by)).await.unwrap();
    elapse_ms(60).await;
    the_echoing(&stack.drain_peer(), 603, "10.0.0.1", None);

    stack.inject(&request_via("ACK", sent_by, "", branch, call_id)).await;
    elapse_ms(60).await;
    assert!(elsewhere.try_recv().is_none(), "nothing reaches the sent-by");
}

#[tokio::test(start_paused = true)]
async fn a_cancel_and_the_487_are_answered_at_the_source() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let sent_by = "10.0.0.1:5060";
    let elsewhere = decoy(&stack, sent_by).await;
    let (branch, call_id) = ("z9hG4bK-dest-cancel", "dest-cancel");

    stack.inject(&request_via("INVITE", sent_by, ";rport", branch, call_id)).await;
    elapse_ms(60).await;
    let _ = stack.drain_peer();
    let _ = stack.drain_events();

    stack.inject(&request_via("CANCEL", sent_by, ";rport", branch, call_id)).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    the_echoing(&out, 200, "10.0.0.1", Some(5555));
    the_echoing(&out, 487, "10.0.0.1", Some(5555));
    let _ = stack.drain_events();

    stack.inject(&request_via("ACK", sent_by, ";rport", branch, call_id)).await;
    elapse_ms(60).await;
    assert!(elsewhere.try_recv().is_none(), "nothing reaches the sent-by");
}

#[tokio::test(start_paused = true)]
async fn a_non_invite_and_its_retransmission_are_answered_at_the_source() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let sent_by = "10.0.0.1:5060";
    let elsewhere = decoy(&stack, sent_by).await;
    let (branch, call_id) = ("z9hG4bK-dest-message", "dest-message");
    let message = request_via("MESSAGE", sent_by, ";rport", branch, call_id);

    stack.inject(&message).await;
    elapse_ms(60).await;
    let req = handed_up(stack.drain_events(), "MESSAGE");
    stack.txn.send_response(answer(&req, 200, "OK"), addr(sent_by)).await.unwrap();
    elapse_ms(60).await;
    the_echoing(&stack.drain_peer(), 200, "10.0.0.1", Some(5555));

    // A retransmission draws the cached final, to the same place (§17.2.2).
    stack.inject(&message).await;
    elapse_ms(60).await;
    the_echoing(&stack.drain_peer(), 200, "10.0.0.1", Some(5555));
    assert!(elsewhere.try_recv().is_none(), "nothing reaches the sent-by");
}
