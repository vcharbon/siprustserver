//! A stall's declared CSeq re-use (ADR-0055 §11): the stalled primary sends
//! requests on a dialog its backup never learns of; the copy that serves the
//! call after the kill sends its own at those CSeqs, each closed by the 481
//! the dialog's peer answers.

use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};
use sip_net_testkit::RecordedSipEntry;

use crate::cluster::Cluster;

/// Declare, from the stall on, the copy's requests at a CSeq the stalled
/// primary sent on the same dialog during the stall, each answered 481.
pub(super) fn accept_cseqs_sent_during_the_stall(cluster: &mut Cluster) {
    cluster.fh.accept_rfc_deviations_from_now_seeing(
        crate::RULE_CSEQ_IN_DIALOG_ORDER,
        reuses_a_stall_cseq,
        "ADR-0055 §11: a stall: the copy sends at a CSeq the stalled primary used on the \
         dialog, and the request draws a 481",
    );
}

/// Whether the request at `entries[i]` re-uses the Call-ID and CSeq number of
/// another transaction's request captured after `opened_at` and before it,
/// and a 481 to it follows.
fn reuses_a_stall_cseq(i: usize, entries: &[RecordedSipEntry], opened_at: u64) -> bool {
    let Some(key) = entries.get(i).and_then(|e| request_key(&e.raw)) else { return false };
    let (call_id, cseq, method, branch) = &key;
    let sent_during_the_stall = entries[..i].iter().filter(|e| e.seq > opened_at).any(|e| {
        request_key(&e.raw).is_some_and(|(c, n, _, b)| c == *call_id && n == *cseq && b != *branch)
    });
    let refused = entries[i + 1..].iter().any(|e| {
        let Ok(SipMessage::Response(resp)) = CustomParser::new().parse(&e.raw) else {
            return false;
        };
        resp.status() == 481
            && resp.call_id().as_str() == call_id
            && resp.cseq().seq() == *cseq
            && resp.cseq().method().as_str() == method
    });
    sent_during_the_stall && refused
}

/// The Call-ID, CSeq number, method and top Via branch of a request other
/// than an ACK or a CANCEL.
fn request_key(raw: &[u8]) -> Option<(String, u32, String, String)> {
    let Ok(SipMessage::Request(req)) = CustomParser::new().parse(raw) else { return None };
    if matches!(*req.method(), Method::Ack | Method::Cancel) {
        return None;
    }
    let branch = req.top_via().branch()?.to_string();
    Some((req.call_id().to_string(), req.cseq().seq(), req.method().as_str().to_string(), branch))
}

#[cfg(test)]
mod tests {
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
        let entries = [
            entry(5, request("BYE", 4, "z9hG4bK-dead")),
            entry(6, request("BYE", 4, "z9hG4bK-copy")),
            entry(7, response(481, "BYE", 4, "z9hG4bK-copy")),
        ];
        assert!(reuses_a_stall_cseq(1, &entries, 4));
    }

    #[test]
    fn a_reuse_without_its_481_keeps_the_finding() {
        let entries = [
            entry(5, request("BYE", 4, "z9hG4bK-dead")),
            entry(6, request("BYE", 4, "z9hG4bK-copy")),
            entry(7, response(200, "BYE", 4, "z9hG4bK-copy")),
        ];
        assert!(!reuses_a_stall_cseq(1, &entries, 4));
    }

    #[test]
    fn a_cseq_sent_before_the_stall_keeps_the_finding() {
        let entries = [
            entry(3, request("BYE", 4, "z9hG4bK-dead")),
            entry(6, request("BYE", 4, "z9hG4bK-copy")),
            entry(7, response(481, "BYE", 4, "z9hG4bK-copy")),
        ];
        assert!(!reuses_a_stall_cseq(1, &entries, 4));
    }
}
