//! RFC 6026 §7.2: every INVITE client transaction that takes a 2xx is
//! Accepted until Timer M. While its call holds it, the TU owns every ACK and
//! each later 2xx on the branch reaches it as a repeat. Once the call is
//! released the layer owns them (RFC 3261 §13.2.2.4): a dialog whose ACK the
//! TU sent gets that ACK again; one no ACK left for gets an ACK built from the
//! INVITE, and its BYE when the INVITE opened it.

use crate::common;
use call::TxnKind;
use common::*;
use sip_message::{SipMessage, SipRequest};
use sip_txn::timers::TIMER_M;
use sip_txn::TransactionEvent;

/// Where every INVITE goes: the peer the tests read the wire at.
const CALLEE: &str = PEER;

fn reqs(msgs: &[SipMessage], method: &str) -> Vec<SipRequest> {
    msgs.iter()
        .filter_map(|m| match m {
            SipMessage::Request(r) if r.method() == method => Some(r.clone()),
            _ => None,
        })
        .collect()
}

/// The responses among `events`, each with whether it matched a client
/// transaction.
fn responses(events: &[TransactionEvent]) -> Vec<(u16, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, matched_client_txn, .. } => match &**message {
                SipMessage::Response(r) => Some((r.status(), *matched_client_txn)),
                SipMessage::Request(_) => None,
            },
            _ => None,
        })
        .collect()
}

/// A 2xx to the INVITE on `branch`, from the callee under `tag`, carrying
/// `extra` header lines and `body`.
fn ok(cseq: u32, branch: &str, call_id: &str, tag: &str, extra: &[&str], body: &str) -> Vec<u8> {
    provisional_or_final("200 OK", cseq, branch, call_id, tag, extra, body)
}

fn provisional_or_final(
    status: &str,
    cseq: u32,
    branch: &str,
    call_id: &str,
    tag: &str,
    extra: &[&str],
    body: &str,
) -> Vec<u8> {
    let mut out = format!(
        "SIP/2.0 {status}\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:15071;branch={branch}\r\n\
         From: <sip:b2bua@127.0.0.1:15071>;tag=b2bua-b-1\r\n\
         To: <sip:bob@192.0.2.20:5060>;tag={tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} INVITE\r\n\
         Contact: <sip:bob@192.0.2.20:5060>\r\n"
    );
    for line in extra {
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    out.into_bytes()
}

const OFFER: &str = "v=0\r\no=bob 7 7 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";

/// An initial INVITE carrying no offer (a delayed offer, RFC 3261 §13.2.1).
fn offerless_invite(call_ref: &str, call_id: &str, branch: &str) -> SipRequest {
    invite_with_cr_lg(call_ref, call_id, branch, "b-1")
}

/// A re-INVITE on the dialog `tag` names, through `route`.
fn reinvite(call_ref: &str, call_id: &str, branch: &str, tag: &str, route: &str) -> SipRequest {
    parse_request(&format!(
        "INVITE sip:bob@192.0.2.20:5060 SIP/2.0\n\
         Via: SIP/2.0/UDP 127.0.0.1:15071;branch={branch};cr={call_ref};lg=b-1\n\
         Max-Forwards: 70\n\
         Route: {route}\n\
         From: <sip:b2bua@127.0.0.1:15071>;tag=b2bua-b-1\n\
         To: <sip:bob@192.0.2.20:5060>;tag={tag}\n\
         Call-ID: {call_id}\n\
         CSeq: 2 INVITE\n\
         Contact: <sip:b2bua@127.0.0.1:15071>\n\
         Content-Length: 0\n\n"
    ))
}

/// The ACK the TU sends for the 2xx `tag` gave the INVITE `cseq`.
fn tu_ack(call_ref: &str, call_id: &str, cseq: u32, tag: &str) -> SipRequest {
    parse_request(&format!(
        "ACK sip:bob@192.0.2.20:5060 SIP/2.0\n\
         Via: SIP/2.0/UDP 127.0.0.1:15071;branch=z9hG4bK-tu-ack-{tag};cr={call_ref};lg=b-1\n\
         Max-Forwards: 70\n\
         From: <sip:b2bua@127.0.0.1:15071>;tag=b2bua-b-1\n\
         To: <sip:bob@192.0.2.20:5060>;tag={tag}\n\
         Call-ID: {call_id}\n\
         CSeq: {cseq} ACK\n\
         X-Tu: sent-by-the-tu\n\
         Content-Length: 0\n\n"
    ))
}

/// The first 2xx reaches the TU as the transaction's final and leaves the
/// transaction Accepted, off its call's books, until Timer M. A repeat of it
/// reaches the TU as a repeat, matched to nothing, and the layer sends
/// nothing: the TU owns the ACK while the call holds the transaction.
#[tokio::test(start_paused = true)]
async fn a_2xx_holds_the_transaction_accepted_until_timer_m_and_the_tu_owns_the_ack() {
    let mut stack = Stack::build(5, 64, 64).await;
    let (call_ref, call_id, branch) = ("self|call-A1", "callid-A1", "z9hG4bK-A1");
    let invite = offerless_invite(call_ref, call_id, branch);
    stack.txn.send_request(invite, addr(CALLEE), TxnKind::Invite).await.unwrap();
    elapse_ms(20).await;
    stack.drain_peer();

    stack.inject(&ok(1, branch, call_id, "t1", &[], "")).await;
    elapse_ms(20).await;
    assert_eq!(responses(&stack.drain_events()), vec![(200, true)], "the 2xx is the final");
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "Accepted holds the transaction");
    assert_eq!(
        stack.txn.active_txn_count_for_call(call_ref).await.unwrap(),
        0,
        "the 2xx met the transaction's obligation to its call"
    );

    stack.inject(&ok(1, branch, call_id, "t1", &[], "")).await;
    elapse_ms(20).await;
    assert_eq!(responses(&stack.drain_events()), vec![(200, false)], "a repeat, matched to none");
    assert!(stack.drain_peer().is_empty(), "the layer sends nothing: the TU owns the ACK");

    elapse_ms(TIMER_M - 200).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "held inside Timer M");
    elapse_ms(300).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "Timer M purges it");
}

/// After its call is released, a repeat of a 2xx whose ACK the TU sent
/// draws that very ACK again and nothing else: the dialog was the TU's, and
/// its release ended it.
#[tokio::test(start_paused = true)]
async fn after_release_a_repeat_draws_the_ack_the_tu_sent_and_no_bye() {
    let mut stack = Stack::build(5, 64, 64).await;
    let (call_ref, call_id, branch) = ("self|call-A2", "callid-A2", "z9hG4bK-A2");
    let invite = offerless_invite(call_ref, call_id, branch);
    stack.txn.send_request(invite, addr(CALLEE), TxnKind::Invite).await.unwrap();
    stack.inject(&ok(1, branch, call_id, "t2", &[], "")).await;
    elapse_ms(20).await;
    let ack = tu_ack(call_ref, call_id, 1, "t2");
    stack.txn.send_request(ack, addr(CALLEE), TxnKind::Invite).await.unwrap();
    elapse_ms(20).await;
    let sent = reqs(&stack.drain_peer(), "ACK").pop().expect("the TU's ACK left");
    let _ = stack.drain_events();

    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    stack.inject(&ok(1, branch, call_id, "t2", &[], "")).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    let acks = reqs(&out, "ACK");
    assert_eq!(acks.len(), 1, "the repeat is ACKed once: {out:?}");
    assert_eq!(acks[0].image(), sent.image(), "with the TU's own ACK, byte for byte");
    assert!(reqs(&out, "BYE").is_empty(), "the TU's dialog draws no BYE");
    assert!(stack.drain_events().is_empty(), "nothing reaches a consumer the call is gone from");
}

/// After its call is released, a 2xx to an initial INVITE the TU never
/// ACKed is ACKed from the INVITE on a fresh branch and its dialog BYEd; a
/// further repeat draws the same ACK and no second BYE.
#[tokio::test(start_paused = true)]
async fn after_release_an_unacked_initial_2xx_draws_an_ack_and_a_bye_once() {
    let mut stack = Stack::build(5, 64, 64).await;
    let (call_ref, call_id, branch) = ("self|call-A3", "callid-A3", "z9hG4bK-A3");
    let invite = offerless_invite(call_ref, call_id, branch);
    stack.txn.send_request(invite, addr(CALLEE), TxnKind::Invite).await.unwrap();
    stack.inject(&ok(1, branch, call_id, "t3", &[], "")).await;
    elapse_ms(20).await;
    stack.drain_peer();
    let _ = stack.drain_events();

    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    stack.inject(&ok(1, branch, call_id, "t3", &[], "")).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    let ack = reqs(&out, "ACK").pop().expect("the repeat is ACKed");
    assert_ne!(ack.top_via().branch(), Some(branch), "§13.2.2.4: the 2xx ACK is its own branch");
    assert_eq!(ack.to().tag(), Some("t3"));
    assert_eq!(ack.cseq().seq(), 1);
    let bye = reqs(&out, "BYE").pop().expect("the dialog nothing holds is BYEd");
    assert_eq!(bye.to().tag(), Some("t3"));

    stack.inject(&ok(1, branch, call_id, "t3", &[], "")).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    let again = reqs(&out, "ACK").pop().expect("a further repeat is ACKed");
    assert_eq!(again.image(), ack.image(), "with the same ACK");
    assert!(reqs(&out, "BYE").is_empty(), "and no second BYE");
}

/// After its call is released, a 2xx to a re-INVITE the TU never ACKed is
/// ACKed from the INVITE, through the INVITE's route set, and draws no BYE:
/// the dialog was the call's, which ended it.
#[tokio::test(start_paused = true)]
async fn after_release_an_unacked_reinvite_2xx_is_acked_through_its_route_set_and_draws_no_bye() {
    let mut stack = Stack::build(5, 64, 64).await;
    let (call_ref, call_id, branch) = ("self|call-A4", "callid-A4", "z9hG4bK-A4");
    let route = "<sip:192.0.2.30:5060;lr>";
    let invite = reinvite(call_ref, call_id, branch, "t4", route);
    stack.txn.send_request(invite, addr(CALLEE), TxnKind::Invite).await.unwrap();
    stack.inject(&ok(2, branch, call_id, "t4", &[], "")).await;
    elapse_ms(20).await;
    stack.drain_peer();
    let _ = stack.drain_events();

    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    stack.inject(&ok(2, branch, call_id, "t4", &[], "")).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    let ack = reqs(&out, "ACK").pop().expect("the repeat is ACKed");
    assert_eq!(ack.raw(sip_message::header::HeaderName::Route).collect::<Vec<_>>(), vec![route]);
    assert_eq!(ack.cseq().seq(), 2);
    assert!(reqs(&out, "BYE").is_empty(), "a re-INVITE's dialog draws no BYE");
}

/// After its call is released, a 2xx under another To-tag (a fork, RFC 6026
/// §7.2) is ACKed and BYEd on its own dialog, while a repeat of the 2xx the TU
/// ACKed still draws the TU's ACK alone.
#[tokio::test(start_paused = true)]
async fn after_release_a_forked_2xx_is_acked_and_byed_on_its_own_dialog() {
    let mut stack = Stack::build(5, 64, 64).await;
    let (call_ref, call_id, branch) = ("self|call-A5", "callid-A5", "z9hG4bK-A5");
    let invite = offerless_invite(call_ref, call_id, branch);
    stack.txn.send_request(invite, addr(CALLEE), TxnKind::Invite).await.unwrap();
    stack.inject(&ok(1, branch, call_id, "won", &[], "")).await;
    elapse_ms(20).await;
    stack
        .txn
        .send_request(tu_ack(call_ref, call_id, 1, "won"), addr(CALLEE), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(20).await;
    let tu_sent = reqs(&stack.drain_peer(), "ACK").pop().expect("the TU's ACK left");
    let _ = stack.drain_events();

    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    stack.inject(&ok(1, branch, call_id, "fork", &[], "")).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    let ack = reqs(&out, "ACK").pop().expect("the fork's 2xx is ACKed");
    assert_eq!(ack.to().tag(), Some("fork"));
    let bye = reqs(&out, "BYE").pop().expect("the fork's dialog is BYEd");
    assert_eq!(bye.to().tag(), Some("fork"));

    stack.inject(&ok(1, branch, call_id, "won", &[], "")).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    assert_eq!(
        reqs(&out, "ACK").iter().map(|a| a.image().clone()).collect::<Vec<_>>(),
        vec![tu_sent.image().clone()],
        "the TU's dialog draws its own ACK"
    );
    assert!(reqs(&out, "BYE").is_empty(), "and no BYE");
}

/// After its call is released, a 2xx carrying the offer of a delayed-offer
/// INVITE is answered in the ACK the layer builds, every stream rejected
/// (RFC 3264 §6: port 0).
#[tokio::test(start_paused = true)]
async fn after_release_a_delayed_offer_2xx_is_answered_with_every_stream_rejected() {
    let mut stack = Stack::build(5, 64, 64).await;
    let (call_ref, call_id, branch) = ("self|call-A6", "callid-A6", "z9hG4bK-A6");
    let invite = offerless_invite(call_ref, call_id, branch);
    stack.txn.send_request(invite, addr(CALLEE), TxnKind::Invite).await.unwrap();
    let sdp = ["Content-Type: application/sdp"];
    stack.inject(&ok(1, branch, call_id, "t6", &sdp, OFFER)).await;
    elapse_ms(20).await;
    stack.drain_peer();
    let _ = stack.drain_events();

    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    stack.inject(&ok(1, branch, call_id, "t6", &sdp, OFFER)).await;
    elapse_ms(20).await;
    let ack = reqs(&stack.drain_peer(), "ACK").pop().expect("the repeat is ACKed");
    let answer =
        String::from_utf8_lossy(ack.sdp().expect("the ACK carries the answer")).to_string();
    assert!(answer.contains("m=audio 0 RTP/AVP 0"), "the stream is rejected: {answer}");
}

/// A delayed-offer INVITE whose offer came in a reliable provisional was
/// answered in its PRACK (RFC 3262 §5): after release the ACK the layer
/// builds for its 2xx carries no body.
#[tokio::test(start_paused = true)]
async fn after_release_a_2xx_after_a_reliable_offer_is_acked_bare() {
    let mut stack = Stack::build(5, 64, 64).await;
    let (call_ref, call_id, branch) = ("self|call-A7", "callid-A7", "z9hG4bK-A7");
    let invite = offerless_invite(call_ref, call_id, branch);
    stack.txn.send_request(invite, addr(CALLEE), TxnKind::Invite).await.unwrap();
    let reliable = ["Require: 100rel", "RSeq: 1", "Content-Type: application/sdp"];
    let progress =
        provisional_or_final("183 Session Progress", 1, branch, call_id, "t7", &reliable, OFFER);
    stack.inject(&progress).await;
    elapse_ms(20).await;
    let sdp = ["Content-Type: application/sdp"];
    stack.inject(&ok(1, branch, call_id, "t7", &sdp, OFFER)).await;
    elapse_ms(20).await;
    stack.drain_peer();
    let _ = stack.drain_events();

    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    stack.inject(&ok(1, branch, call_id, "t7", &sdp, OFFER)).await;
    elapse_ms(20).await;
    let ack = reqs(&stack.drain_peer(), "ACK").pop().expect("the repeat is ACKed");
    assert!(ack.body().is_empty(), "no answer is owed: {:?}", ack.body());
}

/// A 2xx on a branch this layer sent no INVITE on matches nothing: it is
/// handed up unmatched and nothing is sent for it.
#[tokio::test(start_paused = true)]
async fn an_unsolicited_2xx_draws_no_ack() {
    let mut stack = Stack::build(5, 64, 64).await;
    stack.inject(&ok(1, "z9hG4bK-never-sent", "callid-A8", "t8", &[], "")).await;
    elapse_ms(20).await;
    assert_eq!(responses(&stack.drain_events()), vec![(200, false)]);
    assert!(stack.drain_peer().is_empty(), "no ACK for a 2xx to an INVITE never sent");
}

/// Relinquishing a call takes its Accepted transactions with it: nothing is
/// sent for a later repeat, which reaches the consumer as a new arrival.
#[tokio::test(start_paused = true)]
async fn relinquishing_a_call_deletes_its_accepted_transactions() {
    let mut stack = Stack::build(5, 64, 64).await;
    let (call_ref, call_id, branch) = ("self|call-A9", "callid-A9", "z9hG4bK-A9");
    let invite = offerless_invite(call_ref, call_id, branch);
    stack.txn.send_request(invite, addr(CALLEE), TxnKind::Invite).await.unwrap();
    stack.inject(&ok(1, branch, call_id, "t9", &[], "")).await;
    elapse_ms(20).await;
    stack.drain_peer();
    let _ = stack.drain_events();

    assert_eq!(stack.txn.relinquish_call(call_ref).await.unwrap(), 1, "the Accepted one goes");
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
    stack.inject(&ok(1, branch, call_id, "t9", &[], "")).await;
    elapse_ms(20).await;
    assert_eq!(responses(&stack.drain_events()), vec![(200, false)]);
    assert!(stack.drain_peer().is_empty());
}
