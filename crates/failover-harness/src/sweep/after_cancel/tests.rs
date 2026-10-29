use sip_net_testkit::RecordedSipEntry;

use super::*;

const ALICE: &str = "127.0.0.1:5060";
const PROXY: &str = "127.0.0.1:5080";
const BOB: &str = "127.0.0.1:5070";

fn entry(seq: u64, from: &str, to: &str, raw: String) -> RecordedSipEntry {
    RecordedSipEntry {
        seq,
        from: from.parse().unwrap(),
        to: to.parse().unwrap(),
        raw: raw.into_bytes(),
        sent_ms: seq,
        received_ms: None,
        delivered: true,
        recv_note: None,
        reemit: None,
        from_lane: None,
        to_lane: None,
    }
}

fn message(start: &str, to_tag: &str, method: &str) -> String {
    format!(
        "{start}\r\nVia: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-1\r\n\
         From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>{to_tag}\r\nCall-ID: c1\r\n\
         CSeq: 1 {method}\r\nContent-Length: 0\r\n\r\n"
    )
}

fn invite_to_bob(seq: u64) -> RecordedSipEntry {
    entry(seq, PROXY, BOB, message("INVITE sip:b@x SIP/2.0", "", "INVITE"))
}

fn cancelled_487(seq: u64) -> RecordedSipEntry {
    entry(seq, PROXY, ALICE, message("SIP/2.0 487 Request Terminated", ";tag=t", "INVITE"))
}

fn judged(entries: Vec<RecordedSipEntry>) -> bool {
    let callees = [BOB.parse().unwrap()];
    dialled_after_the_callers_487(&CaptureIndex::new(entries), ALICE.parse().unwrap(), &callees)
}

#[test]
fn a_callee_dialled_after_the_callers_487_on_an_undialled_call_is_read() {
    assert!(judged(vec![cancelled_487(1), invite_to_bob(2)]));
}

#[test]
fn a_callee_dialled_before_the_487_is_a_plain_cancel() {
    assert!(!judged(vec![invite_to_bob(1), cancelled_487(2), invite_to_bob(3)]));
}

#[test]
fn a_487_with_no_dial_after_it_is_not_read() {
    assert!(!judged(vec![cancelled_487(1)]));
}
