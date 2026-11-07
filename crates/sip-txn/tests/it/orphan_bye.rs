//! RFC 3261 §15.1.1: the BYE an orphaned initial INVITE's 2xx draws, for the
//! dialog nothing holds any more (ADR-0034).

use crate::common;
use call::TxnKind;
use common::*;
use sip_message::header::HeaderName;
use sip_message::{CustomParser, SipMessage, SipParser, SipRequest};
use sip_net::SignalingNetwork;

fn reqs(msgs: &[SipMessage], method: &str) -> Vec<SipRequest> {
    msgs.iter()
        .filter_map(|m| match m {
            SipMessage::Request(r) if r.method() == method => Some(r.clone()),
            _ => None,
        })
        .collect()
}

fn ok_tagged(cseq_method: &str, cseq: u32, branch: &str, call_id: &str, tag: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 200 OK\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:15071;branch={branch}\r\n\
         From: <sip:b2bua@127.0.0.1:15071>;tag=b2bua-b-1\r\n\
         To: <sip:bob@192.0.2.20:5060>;tag={tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} {cseq_method}\r\n\
         Contact: <sip:bob@{PEER}>\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A 2xx whose Contact names a `maddr`: the ACK and the BYE the layer owes go to it
/// (RFC 3263 §4), the name in its host never read.
#[tokio::test(start_paused = true)]
async fn an_orphans_ack_and_bye_go_to_the_maddr_of_the_2xx_contact() {
    let stack = Stack::build(5, 64, 64).await;
    let call_ref = "self|call-P2";
    let branch = "z9hG4bK-P2";
    let invite = invite_with_cr_lg(call_ref, "callid-P2", branch, "b-1");
    stack.txn.send_request(invite, addr(PEER), TxnKind::Invite).await.unwrap();
    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    elapse_ms(20).await;
    stack.drain_peer();

    let peer = addr(PEER);
    let contact = format!("<sip:bob@name.invalid:{};maddr={}>", peer.port(), peer.ip());
    let ok = String::from_utf8(ok_tagged("INVITE", 1, branch, "callid-P2", "fork-a"))
        .expect("text")
        .replace(&format!("<sip:bob@{PEER}>"), &contact);
    stack.inject(ok.as_bytes()).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    assert_eq!(reqs(&out, "ACK").len(), 1, "the ACK reaches the maddr: {out:?}");
    assert_eq!(reqs(&out, "BYE").len(), 1, "the BYE reaches the maddr: {out:?}");
}

/// RFC 6026 §7.2: a second 2xx (another fork's To-tag) to an orphaned INVITE
/// is the TU's: its own ACK and its own BYE.
#[tokio::test(start_paused = true)]
async fn a_forked_second_2xx_is_acked_and_byed_on_its_own_dialog() {
    let stack = Stack::build(5, 64, 64).await;
    let call_ref = "self|call-P1";
    let branch = "z9hG4bK-P1";
    let invite = invite_with_cr_lg(call_ref, "callid-P1", branch, "b-1");
    stack.txn.send_request(invite, addr(PEER), TxnKind::Invite).await.unwrap();
    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    elapse_ms(20).await;
    stack.drain_peer();

    stack.inject(&ok_tagged("INVITE", 1, branch, "callid-P1", "fork-a")).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    assert_eq!(reqs(&out, "BYE").len(), 1);

    stack.inject(&ok_tagged("INVITE", 1, branch, "callid-P1", "fork-b")).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    let acks = reqs(&out, "ACK");
    assert!(
        acks.iter().all(|a| a.to().tag() == Some("fork-b")),
        "ACK to fork-b's 2xx carries fork-b's tag, got {:?}",
        acks.iter().map(|a| a.to().tag().map(str::to_string)).collect::<Vec<_>>()
    );
    let byes = reqs(&out, "BYE");
    assert_eq!(byes.len(), 1, "fork-b's dialog is BYEd too");
}

/// The orphan BYE's 200 has no consumer either.
#[tokio::test(start_paused = true)]
async fn the_orphan_byes_200_reaches_no_consumer() {
    let mut stack = Stack::build(5, 64, 64).await;
    let call_ref = "self|call-P2";
    let branch = "z9hG4bK-P2";
    let invite = invite_with_cr_lg(call_ref, "callid-P2", branch, "b-1");
    stack.txn.send_request(invite, addr(PEER), TxnKind::Invite).await.unwrap();
    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    elapse_ms(20).await;
    stack.drain_peer();
    let _ = stack.drain_events();

    stack.inject(&ok_tagged("INVITE", 1, branch, "callid-P2", "t2")).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    let bye = reqs(&out, "BYE").pop().expect("bye");
    let bye_branch = bye.top_via().branch().unwrap().to_string();
    let _ = stack.drain_events();
    stack.inject(&ok_tagged("BYE", bye.cseq().seq(), &bye_branch, "callid-P2", "t2")).await;
    elapse_ms(20).await;
    let ev = stack.drain_events();
    assert!(ev.is_empty(), "the orphan BYE's 200 surfaced: {ev:?}");
}

/// The BYE's CSeq is above every request this side sent in the early dialog
/// (a PRACK at INVITE+1, RFC 3262): §12.2.1.1 / §12.2.2.
#[tokio::test(start_paused = true)]
async fn the_orphan_bye_cseq_is_above_an_early_prack() {
    let stack = Stack::build(5, 64, 64).await;
    let call_ref = "self|call-P3";
    let branch = "z9hG4bK-P3";
    let invite = invite_with_cr_lg(call_ref, "callid-P3", branch, "b-1");
    stack.txn.send_request(invite, addr(PEER), TxnKind::Invite).await.unwrap();
    let prack = parse_request(&format!(
        "PRACK sip:bob@192.0.2.20:5060 SIP/2.0\n\
         Via: SIP/2.0/UDP 127.0.0.1:15071;branch=z9hG4bK-P3-prack;cr={call_ref};lg=b-1\n\
         Max-Forwards: 70\n\
         From: <sip:b2bua@127.0.0.1:15071>;tag=b2bua-b-1\n\
         To: <sip:bob@192.0.2.20:5060>;tag=t3\n\
         Call-ID: callid-P3\n\
         CSeq: 2 PRACK\n\
         RAck: 1 1 INVITE\n\
         Content-Length: 0\n\n"
    ));
    stack.txn.send_request(prack, addr(PEER), TxnKind::NonInvite).await.unwrap();
    stack.inject(&ok_tagged("PRACK", 2, "z9hG4bK-P3-prack", "callid-P3", "t3")).await;
    elapse_ms(20).await;
    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    stack.drain_peer();

    stack.inject(&ok_tagged("INVITE", 1, branch, "callid-P3", "t3")).await;
    elapse_ms(20).await;
    let bye = reqs(&stack.drain_peer(), "BYE").pop().expect("bye");
    assert!(bye.cseq().seq() > 2, "BYE CSeq {} not above the PRACK's 2", bye.cseq().seq());
}

const FRONT: &str = "192.0.2.77:5060";

/// A 2xx to an orphaned INVITE of an element behind a front proxy: the ACK and
/// the BYE leave for the proxy, what the peer wrote choosing no first hop, and
/// the Route they carry stays the dialog's (RFC 3261 §12.1.2).
async fn an_orphans_obligations_behind_a_front_proxy(record_route: &str) -> Vec<SipMessage> {
    let config = sip_txn::TransactionConfig { first_hop: Some(addr(FRONT)), ..common::config() };
    let stack = Stack::build_with_config(5, 64, config).await;
    let front = stack
        .net
        .bind_udp(sip_net::BindUdpOpts::new(addr(FRONT), 64))
        .await
        .expect("bind the front proxy");
    let call_ref = "self|call-F1";
    let branch = "z9hG4bK-F1";
    let invite = invite_with_cr_lg(call_ref, "callid-F1", branch, "b-1");
    stack.txn.send_request(invite, addr(FRONT), TxnKind::Invite).await.unwrap();
    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    elapse_ms(20).await;
    while front.try_recv().is_some() {}

    let ok = String::from_utf8(ok_tagged("INVITE", 1, branch, "callid-F1", "fork-a"))
        .expect("text")
        .replace("Contact:", &format!("{record_route}Contact:"));
    stack.inject(ok.as_bytes()).await;
    elapse_ms(20).await;
    assert!(stack.drain_peer().is_empty(), "nothing goes straight to the peer");
    std::iter::from_fn(|| front.try_recv())
        .map(|p| CustomParser::new().parse(&p.raw).expect("a message"))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn behind_a_front_proxy_a_record_route_chooses_no_orphan_first_hop() {
    let rr = format!("Record-Route: <sip:{FRONT};outbound;lr>, <sip:{PEER};lr>\r\n");
    let out = an_orphans_obligations_behind_a_front_proxy(&rr).await;
    assert_eq!((reqs(&out, "ACK").len(), reqs(&out, "BYE").len()), (1, 1), "{out:?}");
    let routes: Vec<String> =
        reqs(&out, "ACK")[0].raw(HeaderName::Route).map(|r| r.to_string()).collect();
    assert_eq!(routes.join(", "), format!("<sip:{PEER};lr>, <sip:{FRONT};outbound;lr>"));
}

#[tokio::test(start_paused = true)]
async fn behind_a_front_proxy_a_contact_chooses_no_orphan_first_hop() {
    let out = an_orphans_obligations_behind_a_front_proxy("").await;
    assert_eq!((reqs(&out, "ACK").len(), reqs(&out, "BYE").len()), (1, 1), "{out:?}");
}
