//! The table at the top of the absorption module, row by row, over the pure
//! classification. The end-to-end ladders (both views over a real
//! UA-to-UA exchange) live in [`crate::agent`]'s tests.

use super::*;
use sip_message::parser::custom::CustomParser;
use sip_message::SipParser;

const INVITE: &str = "INVITE sip:bob@10.0.0.2 SIP/2.0\r\n\
    Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
    From: <sip:alice@10.0.0.1>;tag=a1\r\n\
    To: <sip:bob@10.0.0.2>\r\n\
    Call-ID: c1\r\n\
    CSeq: 1 INVITE\r\n\
    Content-Length: 0\r\n\r\n";

const OK: &str = "SIP/2.0 200 OK\r\n\
    Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
    From: <sip:alice@10.0.0.1>;tag=a1\r\n\
    To: <sip:bob@10.0.0.2>;tag=b1\r\n\
    Call-ID: c1\r\n\
    CSeq: 1 INVITE\r\n\
    Content-Length: 0\r\n\r\n";

const BUSY: &str = "SIP/2.0 486 Busy Here\r\n\
    Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
    From: <sip:alice@10.0.0.1>;tag=a1\r\n\
    To: <sip:bob@10.0.0.2>;tag=b1\r\n\
    Call-ID: c1\r\n\
    CSeq: 1 INVITE\r\n\
    Content-Length: 0\r\n\r\n";

const HOP_ACK: &str = "ACK sip:bob@10.0.0.2 SIP/2.0\r\n\
    Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
    From: <sip:alice@10.0.0.1>;tag=a1\r\n\
    To: <sip:bob@10.0.0.2>;tag=b1\r\n\
    Call-ID: c1\r\n\
    CSeq: 1 ACK\r\n\
    Content-Length: 0\r\n\r\n";

fn parse(raw: &str) -> SipMessage {
    CustomParser::new().parse(raw.as_bytes()).expect("the fixture parses")
}

fn sight(a: &Absorption, raw: &str) -> Sighting {
    a.sight(raw.as_bytes(), &parse(raw))
}

#[test]
fn a_retransmitted_invite_is_wire_only_and_the_tu_sees_one() {
    let a = Absorption::transaction_view();
    assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::Both);
    assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::WireOnly);
    assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::WireOnly);
    assert_eq!(a.wire_view().len(), 3);
    assert_eq!(a.tu_view().len(), 1);
}

#[test]
fn a_retransmitted_non_2xx_final_and_its_hop_ack_are_wire_only() {
    let a = Absorption::transaction_view();
    assert_eq!(sight(&a, BUSY).seen_by(), SeenBy::Both);
    assert_eq!(sight(&a, BUSY).seen_by(), SeenBy::WireOnly);
    // The ACK is hop-by-hop only once the INVITE server txn armed it.
    a.arm_hop_ack("c1".into(), "z9hG4bK-1".into());
    let ack = sight(&a, HOP_ACK);
    assert_eq!(ack.owner, Owner::TxnHopAck);
    assert_eq!(ack.seen_by(), SeenBy::WireOnly);
    assert_eq!(a.wire_view().len(), 3);
    assert_eq!(a.tu_view().len(), 1, "only the first 486 reached the TU");
}

#[test]
fn a_retransmitted_2xx_to_invite_is_seen_by_both_views() {
    let a = Absorption::transaction_view();
    assert_eq!(sight(&a, OK).seen_by(), SeenBy::Both);
    let again = sight(&a, OK);
    assert!(again.repeat, "it IS a byte-identical repeat");
    assert_eq!(again.seen_by(), SeenBy::Both, "2xx retransmission is end-to-end");
    assert_eq!(a.tu_view().len(), 2);
}

#[test]
fn every_ack_to_a_2xx_is_the_tus_including_a_re_sent_one() {
    let a = Absorption::transaction_view();
    assert_eq!(sight(&a, OK).seen_by(), SeenBy::Both);
    let ack = sight(&a, HOP_ACK);
    assert_eq!(ack.owner, Owner::Tu);
    // The core re-ACKs a retransmitted 2xx with the SAME ACK: a repeat on
    // the wire, and still the TU's — nothing absorbs an ACK to a 2xx.
    let again = sight(&a, HOP_ACK);
    assert!(again.repeat);
    assert_eq!(again.seen_by(), SeenBy::Both);
    assert_eq!(a.tu_view().len(), 3);
}

#[test]
fn a_2xx_and_the_ack_it_draws_key_on_the_same_dialog() {
    let (SipMessage::Response(ok), SipMessage::Request(ack)) = (parse(OK), parse(HOP_ACK)) else {
        panic!("the fixtures parse as a response and a request")
    };
    let key = ack_key_of_final(&ok).expect("a 2xx to an INVITE with a To-tag");
    assert_eq!(key, ("c1".to_string(), 1, "b1".to_string()));
    assert_eq!(ack_key_of_ack(&ack), Some(key), "one key, read from either end");
    // Nothing else draws a core-sent ACK: not a non-2xx, not another method.
    let SipMessage::Response(busy) = parse(BUSY) else { panic!("a response") };
    assert_eq!(ack_key_of_final(&busy), None);
    let SipMessage::Request(invite) = parse(INVITE) else { panic!("a request") };
    assert_eq!(ack_key_of_ack(&invite), None);
}

#[test]
fn the_key_separates_calls_transactions_and_methods() {
    let a = Absorption::transaction_view();
    assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::Both);
    // A branch reused across two calls is not one transaction.
    let other_call = INVITE.replace("Call-ID: c1", "Call-ID: c2");
    assert_eq!(sight(&a, &other_call).seen_by(), SeenBy::Both);
    // A fresh branch on a new CSeq is a new transaction (the same CSeq is a
    // merged request, §8.2.2.2).
    let next = INVITE.replace("z9hG4bK-1", "z9hG4bK-2").replace("CSeq: 1 ", "CSeq: 2 ");
    assert_eq!(sight(&a, &next).seen_by(), SeenBy::Both);
    // A CANCEL sharing the INVITE's branch is its own server transaction.
    let cancel = INVITE
        .replace("INVITE sip:bob@10.0.0.2 SIP/2.0", "CANCEL sip:bob@10.0.0.2 SIP/2.0")
        .replace("CSeq: 1 INVITE", "CSeq: 1 CANCEL");
    assert_eq!(sight(&a, &cancel).seen_by(), SeenBy::Both);
}

#[test]
fn same_key_different_bytes_surfaces_and_then_dedups_on_its_own() {
    let a = Absorption::transaction_view();
    assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::Both);
    let mutated = INVITE.replace("Call-ID: c1", "Call-ID: c1\r\nX-Mutant: yes");
    assert_eq!(sight(&a, &mutated).seen_by(), SeenBy::Both);
    assert_eq!(sight(&a, &mutated).seen_by(), SeenBy::WireOnly);
}

#[test]
fn a_forked_2xx_is_a_real_signal_not_a_repeat() {
    let a = Absorption::transaction_view();
    assert_eq!(sight(&a, OK).seen_by(), SeenBy::Both);
    let fork = sight(&a, &OK.replace(";tag=b1", ";tag=b2"));
    assert!(!fork.repeat);
    assert_eq!(fork.seen_by(), SeenBy::Both);
}

#[test]
fn a_provisional_is_never_deduped_because_ring_again_is_observable() {
    let a = Absorption::transaction_view();
    let ringing = OK.replace("SIP/2.0 200 OK", "SIP/2.0 180 Ringing");
    assert!(!sight(&a, &ringing).repeat);
    assert!(!sight(&a, &ringing).repeat);
    assert_eq!(a.tu_view().len(), 2);
}

#[test]
fn dedup_off_puts_every_repeat_back_in_the_tu_view() {
    let a = Absorption::transaction_view();
    a.disable_dedup();
    assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::Both);
    assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::Both);
    assert_eq!(a.wire_view().len(), a.tu_view().len());
}

#[test]
fn hop_ack_ownership_survives_dedup_being_off() {
    let a = Absorption::transaction_view();
    a.disable_dedup();
    a.arm_hop_ack("c1".into(), "z9hG4bK-1".into());
    assert_eq!(sight(&a, HOP_ACK).owner, Owner::TxnHopAck);
    assert!(a.hop_ack_is_fulfilled("c1", "z9hG4bK-1"));
}

const RINGING_SENT: &str = "SIP/2.0 180 Ringing\r\n\
    Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
    From: <sip:alice@10.0.0.1>;tag=a1\r\n\
    To: <sip:bob@10.0.0.2>;tag=b1\r\n\
    Call-ID: c1\r\n\
    CSeq: 1 INVITE\r\n\
    Content-Length: 0\r\n\r\n";

/// A copy of an INVITE this endpoint answered — its branch and sent-by,
/// whatever else a proxy stamped on it — draws the latest provisional again
/// (RFC 3261 §17.2.1), and nothing once a 2xx left (RFC 6026 §7.1).
#[test]
fn a_copy_of_an_answered_invite_draws_its_latest_provisional_again() {
    let a = Absorption::transaction_view();
    assert_eq!(sight(&a, INVITE).owner, Owner::Tu);
    let dst: SocketAddr = "10.0.0.1:5060".parse().unwrap();
    a.note_sent(&parse(RINGING_SENT), RINGING_SENT.as_bytes(), dst, 0);
    let restamped = INVITE.replace("Call-ID: c1", "Call-ID: c1\r\nX-Restamped: yes");
    assert_eq!(sight(&a, &restamped).owner, Owner::TxnCompleted);
    assert_eq!(a.take_repeats(), vec![(RINGING_SENT.as_bytes().to_vec(), dst)]);
    a.note_sent(&parse(OK), OK.as_bytes(), dst, 0);
    assert_eq!(sight(&a, &restamped).owner, Owner::TxnDuplicate, "Accepted absorbs it");
    assert!(a.take_repeats().is_empty(), "a 2xx is the TU's to repeat");
}

/// A copy that arrived after the provisional but before the 2xx left, and is
/// read only once the 2xx is out, is absorbed with nothing drawn: the 2xx
/// already answers it, and no provisional follows a final.
#[test]
fn a_copy_queued_behind_a_later_answer_draws_nothing() {
    let a = Absorption::transaction_view();
    assert_eq!(sight(&a, INVITE).owner, Owner::Tu);
    let dst: SocketAddr = "10.0.0.1:5060".parse().unwrap();
    a.note_sent(&parse(RINGING_SENT), RINGING_SENT.as_bytes(), dst, 0);
    a.note_sent(&parse(OK), OK.as_bytes(), dst, 1);
    let restamped = INVITE.replace("Call-ID: c1", "Call-ID: c1\r\nX-Restamped: yes");
    assert_eq!(sight(&a, &restamped).owner, Owner::TxnDuplicate);
    assert!(a.take_repeats().is_empty(), "nothing drawn after the 2xx");
}

/// An INVITE of another transaction carrying the Call-ID, From-tag and
/// CSeq of one received is a merged request: answered 482, below the TU
/// (RFC 3261 §8.2.2.2).
#[test]
fn a_merged_invite_is_answered_482() {
    let a = Absorption::transaction_view();
    assert_eq!(sight(&a, INVITE).owner, Owner::Tu);
    let merged = INVITE.replace("z9hG4bK-1", "z9hG4bK-merged");
    assert_eq!(sight(&a, &merged).owner, Owner::TxnCompleted);
    let repeats = a.take_repeats();
    assert_eq!(repeats.len(), 1, "one answer");
    let answer = String::from_utf8_lossy(&repeats[0].0).to_string();
    assert!(answer.starts_with("SIP/2.0 482 "), "{answer}");
    assert!(answer.contains("branch=z9hG4bK-merged"), "on the merged transaction: {answer}");
    let next = INVITE.replace("z9hG4bK-1", "z9hG4bK-next").replace("CSeq: 1 ", "CSeq: 2 ");
    assert_eq!(sight(&a, &next).owner, Owner::Tu, "a new CSeq is a new request");
}

const INFO: &str = "INFO sip:bob@10.0.0.2 SIP/2.0\r\n\
    Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-2;rport=5060\r\n\
    From: <sip:alice@10.0.0.1>;tag=a1\r\n\
    To: <sip:bob@10.0.0.2>;tag=b1\r\n\
    Call-ID: c1\r\n\
    CSeq: 2 INFO\r\n\
    Content-Length: 0\r\n\r\n";

const INFO_OK: &str = "SIP/2.0 200 OK\r\n\
    Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-2;rport=5060\r\n\
    From: <sip:alice@10.0.0.1>;tag=a1\r\n\
    To: <sip:bob@10.0.0.2>;tag=b1\r\n\
    Call-ID: c1\r\n\
    CSeq: 2 INFO\r\n\
    Content-Length: 0\r\n\r\n";

/// A copy of a non-INVITE request this endpoint received — its branch,
/// sent-by and method, whatever a proxy stamped on its Via — is the
/// transaction layer's: absorbed while no final left, drawing the final
/// again once one did (RFC 3261 §17.2.2); a request of another
/// transaction is the TU's.
#[tokio::test(start_paused = true)]
async fn a_copy_of_an_answered_non_invite_request_draws_its_final_again() {
    let a = Absorption::transaction_view();
    assert_eq!(sight(&a, INFO).owner, Owner::Tu);
    let dst: SocketAddr = "10.0.0.1:5060".parse().unwrap();
    a.note_sent(&parse(INFO_OK), INFO_OK.as_bytes(), dst, 0);
    let restamped = INFO.replace("rport=5060", "rport=5061");
    let copy = sight(&a, &restamped);
    assert_eq!(copy.owner, Owner::TxnCompleted);
    assert_eq!(
        a.take_repeats(),
        vec![(INFO_OK.as_bytes().to_vec(), dst)],
        "the final, byte for byte, where it went",
    );
    let other = INFO.replace("z9hG4bK-2", "z9hG4bK-3");
    assert_eq!(sight(&a, &other).owner, Owner::Tu, "another branch is another transaction");

    // A copy that arrived before the final — while the transaction was
    // open, or still queued when the final left — is absorbed with
    // nothing sent.
    let early = Absorption::transaction_view();
    sight(&early, INFO);
    assert_eq!(sight(&early, &restamped).owner, Owner::TxnDuplicate, "open: absorbed");
    early.note_sent(&parse(INFO_OK), INFO_OK.as_bytes(), dst, 1);
    assert_eq!(sight(&early, &restamped).owner, Owner::TxnDuplicate, "queued before it");
    assert!(early.take_repeats().is_empty());
    assert_eq!(sight(&early, &restamped).owner, Owner::TxnCompleted, "arrived after it");
    tokio::time::sleep(std::time::Duration::from_millis(64 * 500)).await;
    let later = INFO.replace("rport=5060", "rport=5062");
    assert_eq!(sight(&a, &later).owner, Owner::Tu, "Timer J ends the transaction");
}
