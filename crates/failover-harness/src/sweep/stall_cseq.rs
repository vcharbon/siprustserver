//! A stall's declared CSeq re-use (ADR-0055 §11): the stalled primary sends
//! requests on a dialog its backup never learns of; the copy that serves the
//! call after the kill sends its own at those CSeqs, each closed by the 481
//! the dialog's peer answers.

use sip_message::Method;
use sip_net_testkit::CaptureIndex;

use crate::cluster::Cluster;

/// Declare, from the stall on, the copy's requests at a CSeq the stalled
/// primary sent on the same dialog during the stall, each answered 481.
pub(super) fn accept_cseqs_sent_during_the_stall(cluster: &mut Cluster) {
    cluster.fh.accept_rfc_deviations_from_now_where(
        crate::RULE_CSEQ_IN_DIALOG_ORDER,
        reuses_a_stall_cseq,
        "ADR-0055 §11: a stall: the copy sends at a CSeq the stalled primary used on the \
         dialog, and the request draws a 481",
    );
}

/// The requests whose CSeq a stall's copy re-uses: every one but an ACK or a
/// CANCEL, which carry their INVITE's.
const NOT_REUSED: [Method; 2] = [Method::Ack, Method::Cancel];

/// Whether the request at index `i` of `capture` re-uses the Call-ID and CSeq
/// number of another transaction's request captured after `opened_at` and
/// before it, and a 481 to it follows.
fn reuses_a_stall_cseq(i: usize, capture: &CaptureIndex, opened_at: u64) -> bool {
    let Some(key) = capture.message(i).and_then(|m| m.request_txn(&NOT_REUSED)) else {
        return false;
    };
    let sent_during_the_stall =
        capture.before(i).filter(|(_, e, _)| e.seq > opened_at).any(|(_, _, m)| {
            m.request_txn(&NOT_REUSED).is_some_and(|other| {
                other.call_id == key.call_id && other.cseq == key.cseq && other.branch != key.branch
            })
        });
    let refused = capture.after(i).any(|(_, _, m)| {
        m.status() == Some(481)
            && m.call_id == key.call_id
            && m.cseq == key.cseq
            && m.cseq_method == key.method
    });
    sent_during_the_stall && refused
}

#[cfg(test)]
mod tests {
    use sip_net_testkit::RecordedSipEntry;

    use super::*;

    fn entry(seq: u64, raw: String) -> RecordedSipEntry {
        RecordedSipEntry {
            seq,
            from: "127.0.0.1:5080".parse().unwrap(),
            to: "127.0.0.1:5070".parse().unwrap(),
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

    fn request(method: &str, cseq: u32, branch: &str) -> String {
        format!(
            "{method} sip:b@x SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>;tag=b\r\nCall-ID: c1\r\n\
             CSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
        )
    }

    fn response(status: u16, method: &str, cseq: u32, branch: &str) -> String {
        format!(
            "SIP/2.0 {status} Whatever\r\nVia: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>;tag=b\r\nCall-ID: c1\r\n\
             CSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
        )
    }

    #[test]
    fn a_cseq_the_stalled_primary_sent_drawing_a_481_is_declared() {
        let capture = CaptureIndex::new(vec![
            entry(5, request("BYE", 4, "z9hG4bK-dead")),
            entry(6, request("BYE", 4, "z9hG4bK-copy")),
            entry(7, response(481, "BYE", 4, "z9hG4bK-copy")),
        ]);
        assert!(reuses_a_stall_cseq(1, &capture, 4));
    }

    #[test]
    fn a_reuse_without_its_481_keeps_the_finding() {
        let capture = CaptureIndex::new(vec![
            entry(5, request("BYE", 4, "z9hG4bK-dead")),
            entry(6, request("BYE", 4, "z9hG4bK-copy")),
            entry(7, response(200, "BYE", 4, "z9hG4bK-copy")),
        ]);
        assert!(!reuses_a_stall_cseq(1, &capture, 4));
    }

    #[test]
    fn a_cseq_sent_before_the_stall_keeps_the_finding() {
        let capture = CaptureIndex::new(vec![
            entry(3, request("BYE", 4, "z9hG4bK-dead")),
            entry(6, request("BYE", 4, "z9hG4bK-copy")),
            entry(7, response(481, "BYE", 4, "z9hG4bK-copy")),
        ]);
        assert!(!reuses_a_stall_cseq(1, &capture, 4));
    }
}
