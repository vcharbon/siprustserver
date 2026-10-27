//! A stall's declared repeat (ADR-0055 §11): the copy sends again a CANCEL
//! the stalled primary sent, the two the same message but for the Reason
//! header field each node states on its own.

use sip_message::sniff::{equal_except, Except};
use sip_message::Method;
use sip_net_testkit::CaptureIndex;

use crate::cluster::Cluster;

/// The audit rule a retransmission that is not byte-identical meets.
const RULE_RUNG_BYTE_IDENTICAL: &str = "rung-byte-identical";

/// Declare, from the stall on, the copy's repeat of a CANCEL that differs
/// from the dead owner's only in its Reason header field.
pub(super) fn accept_reason_only_cancel_repeat(cluster: &mut Cluster) {
    cluster.fh.accept_rfc_deviations_from_now_where(
        RULE_RUNG_BYTE_IDENTICAL,
        |i, capture, _| reason_only_cancel_repeat(i, capture),
        "ADR-0055 §11: a stall: the copy's repeat of a CANCEL differs from the dead owner's \
         only in its Reason",
    );
}

/// Whether the entry at index `i` of `capture` is a CANCEL whose first copy
/// before it — same transaction, sent to the same address — differs from it
/// in the Reason header field and nowhere else, the stamps a relaying hop
/// leaves on the Via below its own (RFC 3261 §18.2.1, RFC 3581 §4) apart.
fn reason_only_cancel_repeat(i: usize, capture: &CaptureIndex) -> bool {
    let cancel_txn = |i: usize| {
        capture
            .message(i)
            .filter(|m| m.method() == Some(&Method::Cancel))
            .and_then(|m| m.txn.as_ref())
    };
    let (Some(copy), Some(key)) = (capture.entry(i), cancel_txn(i)) else { return false };
    let first = capture.before(i).find(|(j, e, _)| e.to == copy.to && cancel_txn(*j) == Some(key));
    let Some((_, first, _)) = first else { return false };
    first.raw != copy.raw
        && equal_except(&first.raw, &copy.raw, &[Except::Header("Reason"), Except::RelayStamps])
}

#[cfg(test)]
mod tests {
    use sip_net_testkit::RecordedSipEntry;

    use super::super::parties::ALICE;
    use super::*;

    fn entry(seq: u64, raw: &str) -> RecordedSipEntry {
        RecordedSipEntry {
            seq,
            from: "127.0.0.1:5080".parse().unwrap(),
            to: ALICE.parse().unwrap(),
            raw: raw.as_bytes().to_vec(),
            sent_ms: seq,
            received_ms: None,
            delivered: true,
            recv_note: None,
            reemit: None,
            from_lane: None,
            to_lane: None,
        }
    }

    fn cancel(reason: Option<&str>, uri: &str) -> String {
        let reason = reason.map(|r| format!("Reason: {r}\r\n")).unwrap_or_default();
        format!(
            "CANCEL sip:{uri} SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5092;branch=z9hG4bK-c1\r\n\
             From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>\r\nCall-ID: c1\r\nCSeq: 1 CANCEL\r\nMax-Forwards: 70\r\n\
             {reason}Content-Length: 0\r\n\r\n"
        )
    }

    #[test]
    fn only_the_reason_of_a_cancel_differs() {
        let first = entry(1, &cancel(None, "b@x"));
        let copy = entry(2, &cancel(Some("Q.850;cause=16"), "b@x"));
        assert!(reason_only_cancel_repeat(1, &CaptureIndex::new(vec![first, copy])));
    }

    #[test]
    fn the_source_a_relaying_hop_stamps_is_no_difference() {
        let relayed = |rport: &str, reason| {
            let raw = cancel(reason, "b@x");
            raw.replace(
                "branch=z9hG4bK-c1\r\n",
                &format!("branch=z9hG4bK-c1\r\nVia: SIP/2.0/UDP 127.0.0.1:5091;branch=z9hG4bK-b;rport={rport}\r\n"),
            )
        };
        let first = entry(1, &relayed("5091", None));
        let copy = entry(2, &relayed("5092", Some("Q.850;cause=16")));
        assert!(reason_only_cancel_repeat(1, &CaptureIndex::new(vec![first, copy])));
    }

    #[test]
    fn a_cancel_differing_elsewhere_keeps_the_finding() {
        let first = entry(1, &cancel(None, "b@x"));
        let copy = entry(2, &cancel(Some("Q.850;cause=16"), "c@x"));
        assert!(!reason_only_cancel_repeat(1, &CaptureIndex::new(vec![first, copy])));
    }
}
