//! RFC 3261 §17.2.3 / §17.1.3: a request matches a SERVER transaction only
//! (branch + top-Via sent-by + method, ACK matching its INVITE), a response a
//! CLIENT transaction only (branch + CSeq method). A request that comes back
//! to the instance that sent it with no other hop's Via on top carries the
//! branch of that instance's own client transaction: it is a new server
//! transaction, surfaced to the consumer, and its answer completes the client
//! transaction. A retransmission of a request still draws its server
//! transaction's cached response and nothing else, and a request naming a
//! resident server branch from another sent-by or with another method is not
//! that transaction's.

use crate::common;

use common::*;
use sip_message::generators::{
    generate_cancel, generate_response, GenerateResponseOpts, InviteClientTransactionHandle,
};
use sip_message::{SipMessage, SipRequest};
use sip_txn::timers::{TIMER_H, TIMER_J};
use sip_txn::{TransactionEvent, TxnKind};

const TRANSIT: u64 = 5;
const CALLEE_TAG: &str = "loop-callee";

/// A request from this instance to itself: its own address in the Request-URI,
/// the top (and only) Via and the Contact.
fn request_to_self(method: &str, branch: &str, to_tag: Option<&str>) -> SipRequest {
    let to = match to_tag {
        Some(t) => format!("<sip:loop@{B2BUA}>;tag={t}"),
        None => format!("<sip:loop@{B2BUA}>"),
    };
    parse_request(&format!(
        "{method} sip:loop@{B2BUA} SIP/2.0\n\
         Via: SIP/2.0/UDP {B2BUA};branch={branch}\n\
         Max-Forwards: 70\n\
         From: <sip:self@{B2BUA}>;tag=loop-caller\n\
         To: {to}\n\
         Call-ID: loop-{branch}\n\
         CSeq: 1 {method}\n\
         Contact: <sip:self@{B2BUA}>\n\
         Content-Length: 0\n\n"
    ))
}

/// Inbound requests of `method` handed up, in order.
fn requests(events: &[TransactionEvent], method: &str) -> Vec<(SipRequest, std::net::SocketAddr)> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, src, .. } => match message.as_ref() {
                SipMessage::Request(r) if r.method() == method => Some((r.clone(), *src)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Responses of `status` handed up, each with whether a client transaction
/// matched it.
fn responses(events: &[TransactionEvent], status: u16) -> Vec<bool> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, matched_client_txn, .. } => {
                match message.as_ref() {
                    SipMessage::Response(r) if r.status() == status => Some(*matched_client_txn),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

fn answer(req: &SipRequest, status: u16, reason: &str) -> sip_message::SipResponse {
    let opts = GenerateResponseOpts { to_tag: Some(CALLEE_TAG.to_string()), ..Default::default() };
    generate_response(req, status, reason, &opts)
}

/// An INVITE sent to this instance's own address reaches the consumer as a
/// new server transaction (auto-100, one `Message`); the TU's 486 on it is
/// matched by the INVITE client transaction, which ACKs it, and that ACK is
/// absorbed by the server transaction. Every transaction ends on its timers.
#[tokio::test(start_paused = true)]
async fn an_invite_sent_to_self_opens_a_server_transaction() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let invite = request_to_self("INVITE", "z9hG4bK-loop-invite", None);
    stack.txn.send_request(invite, addr(B2BUA), TxnKind::Invite).await.unwrap();

    elapse_ms(100).await;
    let events = stack.drain_events();
    let received = requests(&events, "INVITE");
    assert_eq!(received.len(), 1, "the INVITE back to its sender surfaces once: {events:?}");
    assert_eq!(stack.txn.metrics().active_transactions(), 2, "one client, one server");

    let (req, src) = received.into_iter().next().unwrap();
    stack.txn.send_response(answer(&req, 486, "Busy Here"), src).await.unwrap();
    elapse_ms(100).await;
    let events = stack.drain_events();
    assert_eq!(responses(&events, 486), vec![true], "the 486 completes the client txn");
    assert!(requests(&events, "ACK").is_empty(), "the hop ACK is absorbed: {events:?}");

    elapse_ms(TIMER_H + 1_000).await;
    assert!(stack.drain_events().is_empty(), "nothing further surfaces");
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// A BYE sent to this instance's own address reaches the consumer once, its
/// Timer E copies are absorbed by the server transaction it opened, and the
/// TU's 200 on it is matched by the BYE client transaction.
#[tokio::test(start_paused = true)]
async fn a_bye_sent_to_self_opens_a_server_transaction() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let bye = request_to_self("BYE", "z9hG4bK-loop-bye", Some("loop-dialog"));
    stack.txn.send_request(bye, addr(B2BUA), TxnKind::NonInvite).await.unwrap();

    // Timer E fires at 0.5 s and 1.5 s before the TU answers.
    elapse_ms(2_000).await;
    let events = stack.drain_events();
    let received = requests(&events, "BYE");
    assert_eq!(received.len(), 1, "the BYE surfaces once, its copies absorbed: {events:?}");

    let (req, src) = received.into_iter().next().unwrap();
    stack.txn.send_response(answer(&req, 200, "OK"), src).await.unwrap();
    elapse_ms(100).await;
    let events = stack.drain_events();
    assert_eq!(responses(&events, 200), vec![true], "the 200 completes the client txn");

    elapse_ms(TIMER_J + 1_000).await;
    assert!(stack.drain_events().is_empty(), "nothing further surfaces");
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// A CANCEL of an INVITE sent to self matches the server INVITE transaction
/// that INVITE opened (RFC 3261 §9.2): 200 to the CANCEL, 487 to the INVITE,
/// `Cancelled` up, and the INVITE client transaction takes the 487.
#[tokio::test(start_paused = true)]
async fn a_cancel_sent_to_self_matches_the_server_invite() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let invite = request_to_self("INVITE", "z9hG4bK-loop-cancel", None);
    stack.txn.send_request(invite.clone(), addr(B2BUA), TxnKind::Invite).await.unwrap();
    elapse_ms(100).await;
    assert_eq!(requests(&stack.drain_events(), "INVITE").len(), 1);

    let cancel = generate_cancel(&InviteClientTransactionHandle { original_invite: invite }, &[]);
    stack.txn.send_request(cancel, addr(B2BUA), TxnKind::NonInvite).await.unwrap();
    elapse_ms(100).await;
    let events = stack.drain_events();
    let cancelled =
        events.iter().filter(|e| matches!(e, TransactionEvent::Cancelled { .. })).count();
    assert_eq!(cancelled, 1, "the CANCEL matched the server INVITE: {events:?}");
    assert_eq!(responses(&events, 487), vec![true], "the 487 completes the client INVITE");

    elapse_ms(TIMER_H + 1_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// A retransmitted INVITE and a retransmitted BYE from a peer each draw their
/// server transaction's cached response and never surface a second time.
#[tokio::test(start_paused = true)]
async fn a_retransmission_from_a_peer_is_still_absorbed() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let invite = inbound_request("INVITE", "z9hG4bK-peer-invite", "peer-invite@u", None);
    stack.inject(&invite).await;
    elapse_ms(50).await;
    stack.inject(&invite).await;
    elapse_ms(50).await;
    let events = stack.drain_events();
    assert_eq!(requests(&events, "INVITE").len(), 1, "one INVITE surfaces: {events:?}");
    assert_eq!(count_responses(&stack.drain_peer(), 100), 2, "each copy draws the 100");

    let (req, src) = requests(&events, "INVITE").into_iter().next().unwrap();
    stack.txn.send_response(answer(&req, 486, "Busy Here"), src).await.unwrap();
    stack
        .inject(&inbound_request("ACK", "z9hG4bK-peer-invite", "peer-invite@u", Some(CALLEE_TAG)))
        .await;

    let bye = inbound_request("BYE", "z9hG4bK-peer-bye", "peer-bye@u", Some("dialog"));
    stack.inject(&bye).await;
    elapse_ms(50).await;
    let events = stack.drain_events();
    let (req, src) = requests(&events, "BYE").into_iter().next().expect("the BYE surfaces");
    stack.txn.send_response(answer(&req, 200, "OK"), src).await.unwrap();
    stack.inject(&bye).await;
    elapse_ms(50).await;
    assert!(requests(&stack.drain_events(), "BYE").is_empty(), "the copy is absorbed");
    let peer = stack.drain_peer();
    assert_eq!(count_responses(&peer, 200), 2, "the 200 and its replay to the copy");

    elapse_ms(TIMER_H + 1_000).await;
    assert!(stack.drain_events().is_empty());
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// A request on a resident server branch from another sent-by is another
/// client's transaction (§17.2.3): it surfaces, and the resident
/// transaction's cached response is never sent for it.
#[tokio::test(start_paused = true)]
async fn a_request_from_another_sent_by_is_not_absorbed() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-shared-branch";
    stack.inject(&inbound_request("BYE", branch, "first@u", Some("dialog"))).await;
    elapse_ms(50).await;
    let events = stack.drain_events();
    let (req, src) = requests(&events, "BYE").into_iter().next().expect("the BYE surfaces");
    stack.txn.send_response(answer(&req, 200, "OK"), src).await.unwrap();
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1);

    let other = String::from_utf8(inbound_request("BYE", branch, "second@u", Some("dialog")))
        .unwrap()
        .replace("Via: SIP/2.0/UDP 10.0.0.1:5555", "Via: SIP/2.0/UDP 10.0.0.2:5555");
    stack.inject(other.as_bytes()).await;
    elapse_ms(50).await;
    let events = stack.drain_events();
    let surfaced = requests(&events, "BYE");
    assert_eq!(surfaced.len(), 1, "the other sender's BYE surfaces: {events:?}");
    assert_eq!(surfaced[0].0.call_id().as_str(), "second@u");
    assert_eq!(count_responses(&stack.drain_peer(), 200), 0, "no replay of the cached 200");

    let (req, src) = surfaced.into_iter().next().unwrap();
    stack.txn.send_response(answer(&req, 200, "OK"), src).await.unwrap();
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "its own answer leaves");

    elapse_ms(TIMER_J + 1_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// A request on a resident server branch with another method is another
/// transaction (§17.2.3): it surfaces, and the resident transaction's cached
/// response is never sent for it.
#[tokio::test(start_paused = true)]
async fn a_request_with_another_method_is_not_absorbed() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-reused-branch";
    stack.inject(&inbound_request("BYE", branch, "reuse@u", Some("dialog"))).await;
    elapse_ms(50).await;
    let events = stack.drain_events();
    let (req, src) = requests(&events, "BYE").into_iter().next().expect("the BYE surfaces");
    stack.txn.send_response(answer(&req, 200, "OK"), src).await.unwrap();
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1);

    stack.inject(&inbound_request("OPTIONS", branch, "reuse@u", Some("dialog"))).await;
    elapse_ms(50).await;
    let events = stack.drain_events();
    let surfaced = requests(&events, "OPTIONS");
    assert_eq!(surfaced.len(), 1, "the OPTIONS surfaces: {events:?}");
    assert_eq!(count_responses(&stack.drain_peer(), 200), 0, "no replay of the BYE's 200");

    let (req, src) = surfaced.into_iter().next().unwrap();
    stack.txn.send_response(answer(&req, 200, "OK"), src).await.unwrap();
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "its own answer leaves");

    elapse_ms(TIMER_J + 1_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// A response on a client branch whose CSeq names another method is not that
/// client transaction's (RFC 3261 §17.1.3): it is handed up unmatched, and the
/// transaction completes on its own method's final.
#[tokio::test(start_paused = true)]
async fn a_response_with_another_cseq_method_is_not_matched() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-client-bye";
    stack
        .txn
        .send_request(outbound_request("BYE", branch), addr(PEER), TxnKind::NonInvite)
        .await
        .unwrap();
    elapse_ms(10).await;

    stack.inject(&response_bytes(200, "OK", "INVITE", branch, "handle-shape-test", true)).await;
    elapse_ms(10).await;
    assert_eq!(responses(&stack.drain_events(), 200), vec![false], "handed up unmatched");
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "the BYE txn stays");

    stack.inject(&response_bytes(200, "OK", "BYE", branch, "handle-shape-test", true)).await;
    elapse_ms(10).await;
    assert_eq!(responses(&stack.drain_events(), 200), vec![true], "the BYE's 200 matches");
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "and ends the transaction");
}
