//! `TransactionLayer::seed` for non-INVITE transactions (ADR-0055 §2): a
//! client non-INVITE a call taken over from a peer names retransmits its
//! request under its branch and takes the final the UAS repeats for it; a
//! server non-INVITE absorbs copies of its request until the consumer's final
//! leaves on it.

use crate::common;
use common::*;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};
use sip_txn::{TransactionEvent, TxnSeed};

const TRANSIT: u64 = 5;
const CALL_REF: &str = "w0|seeded-call@10.0.0.9|atag";

/// The `Message` events in a drained batch, as `(is_response, status_or_0,
/// matched_client_txn)`.
fn messages(events: &[TransactionEvent]) -> Vec<(bool, u16, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, matched_client_txn, .. } => {
                Some(match message.as_ref() {
                    SipMessage::Response(r) => (true, r.status(), *matched_client_txn),
                    SipMessage::Request(_) => (false, 0, *matched_client_txn),
                })
            }
            _ => None,
        })
        .collect()
}

fn request_bytes(raw: &[u8]) -> SipRequest {
    match CustomParser::new().parse(raw).expect("parse request") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

/// An in-dialog INFO the dead node sent on `branch`, attributed to
/// `CALL_REF` through its Via.
fn client_info(branch: &str, call_id: &str) -> SipRequest {
    let invite = invite_with_cr_lg(CALL_REF, call_id, branch, "b-1");
    let text = String::from_utf8(invite.image().to_vec()).expect("utf-8");
    let info = text
        .replace("INVITE sip:", "INFO sip:")
        .replace("CSeq: 1 INVITE", "CSeq: 2 INFO")
        .replace("To: <sip:bob@192.0.2.20:5060>", "To: <sip:bob@192.0.2.20:5060>;tag=peer-tag");
    request_bytes(info.as_bytes())
}

/// A seeded client non-INVITE retransmits its request under its branch on
/// Timer E from the seed, the first copy T1 after it (RFC 3261 §17.1.2.2),
/// and hands the final it draws up once, matched, for its call.
#[tokio::test(start_paused = true)]
async fn a_seeded_client_non_invite_retransmits_its_request_and_takes_its_final() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-seed-info", "seed-info");
    let seed = TxnSeed::ClientNonInvite { request: client_info(branch, call_id), dest: addr(PEER) };
    assert_eq!(stack.txn.seed(CALL_REF, vec![seed]).await.unwrap(), 1);
    assert_eq!(stack.txn.active_txn_count_for_call(CALL_REF).await.unwrap(), 1);
    elapse_ms(400).await;
    assert!(stack.drain_peer().is_empty(), "nothing leaves before T1");
    elapse_ms(200).await;
    let out = stack.drain_peer();
    assert_eq!(count_requests(&out, "INFO"), 1, "the request leaves again at T1");
    assert_eq!(request_branch(&out, "INFO").as_deref(), Some(branch), "under its own branch");

    stack.inject(&response_bytes(200, "OK", "INFO", branch, call_id, true)).await;
    elapse_ms(60).await;
    assert_eq!(
        messages(&stack.drain_events()),
        vec![(true, 200, true)],
        "the final reaches the consumer once, matched"
    );
    elapse_ms(3_000).await;
    assert!(stack.drain_peer().is_empty(), "a final ends the retransmissions");
}

/// A seeded client non-INVITE no final answers gives up on Timer F from the
/// seed, as a transaction this layer opened does.
#[tokio::test(start_paused = true)]
async fn a_seeded_client_non_invite_gives_up_on_timer_f() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-seed-info-f", "seed-info-f");
    let seed = TxnSeed::ClientNonInvite { request: client_info(branch, call_id), dest: addr(PEER) };
    stack.txn.seed(CALL_REF, vec![seed]).await.unwrap();
    elapse_ms(sip_txn::timers::TIMER_F - 100).await;
    let _ = stack.drain_peer();
    assert!(
        !stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "no timeout inside Timer F"
    );
    elapse_ms(200).await;
    assert!(
        stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "Timer F from the seed ends it"
    );
}

/// A seeded server non-INVITE absorbs a copy of its request with nothing
/// sent and nothing handed up, carries the consumer's final, and repeats
/// that final to a later copy (RFC 3261 §17.2.2).
#[tokio::test(start_paused = true)]
async fn a_seeded_server_non_invite_absorbs_copies_until_its_final() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-seed-srv-info", "seed-srv-info");
    let raw = inbound_request("INFO", branch, call_id, Some("peer-tag"));
    let seed = TxnSeed::ServerNonInvite { request: request_bytes(&raw), leg_id: Some("a".into()) };
    assert_eq!(stack.txn.seed(CALL_REF, vec![seed]).await.unwrap(), 1);
    assert_eq!(stack.txn.active_txn_count_for_call(CALL_REF).await.unwrap(), 1);

    stack.inject(&raw).await;
    elapse_ms(60).await;
    assert!(stack.drain_peer().is_empty(), "the copy draws nothing");
    assert!(stack.drain_events().is_empty(), "and is not handed up");

    let ok = parse_response(&response_bytes(200, "OK", "INFO", branch, call_id, true));
    stack.txn.send_response(ok, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "the final leaves on the seed");
    stack.inject(&raw).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "a later copy draws it again");
    assert!(stack.drain_events().is_empty());
}
