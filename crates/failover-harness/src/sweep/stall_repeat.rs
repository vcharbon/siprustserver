//! A stall's declared repeat (ADR-0055 §11): the copy sends again a CANCEL
//! the stalled primary sent, the two the same message but for the Reason
//! header field each node states on its own.

use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};
use sip_net_testkit::RecordedSipEntry;

use crate::cluster::Cluster;

/// The audit rule a retransmission that is not byte-identical meets.
const RULE_RUNG_BYTE_IDENTICAL: &str = "rung-byte-identical";

/// Declare, from the stall on, the copy's repeat of a CANCEL that differs
/// from the dead owner's only in its Reason header field.
pub(super) fn accept_reason_only_cancel_repeat(cluster: &mut Cluster) {
    cluster.fh.accept_rfc_deviations_from_now_where(
        RULE_RUNG_BYTE_IDENTICAL,
        |copy, earlier, _| reason_only_cancel_repeat(copy, earlier),
        "ADR-0055 §11: a stall: the copy's repeat of a CANCEL differs from the dead owner's \
         only in its Reason",
    );
}

/// Whether `copy` is a CANCEL whose first copy in `earlier` — same top Via
/// branch, Call-ID and CSeq number, sent to the same address — differs from
/// it in the Reason header field and nowhere else.
fn reason_only_cancel_repeat(copy: &RecordedSipEntry, earlier: &[RecordedSipEntry]) -> bool {
    let Some(key) = cancel_key_of(&copy.raw) else { return false };
    let first =
        earlier.iter().find(|e| e.to == copy.to && cancel_key_of(&e.raw) == Some(key.clone()));
    let Some(first) = first else { return false };
    let reason = |raw: &[u8]| without(raw, &["reason"]).map(|rest| unstamped(&rest));
    let (theirs, ours) = (reason(&first.raw), reason(&copy.raw));
    theirs.is_some() && theirs == ours && first.raw != copy.raw
}

/// `head` with the `received` and `rport` values a relaying hop stamps on
/// every Via below its own taken out: the source each copy came from
/// (RFC 3261 §18.2.1, RFC 3581 §4), as the retransmission audit reads them.
fn unstamped(head: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(head);
    let mut vias = 0;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            let name = line.split(':').next().unwrap_or_default().trim();
            if !(name.eq_ignore_ascii_case("via") || name.eq_ignore_ascii_case("v")) {
                return line.to_string();
            }
            vias += 1;
            if vias == 1 {
                return line.to_string();
            }
            line.trim_end_matches('\r')
                .split(';')
                .filter(|p| !p.starts_with("received=") && !p.starts_with("rport="))
                .collect::<Vec<_>>()
                .join(";")
                + if line.ends_with('\r') { "\r" } else { "" }
        })
        .collect();
    lines.join("\n").into_bytes()
}

/// The top Via branch, Call-ID and CSeq number of a CANCEL.
fn cancel_key_of(raw: &[u8]) -> Option<(String, String, u32)> {
    let Ok(SipMessage::Request(req)) = CustomParser::new().parse(raw) else { return None };
    if *req.method() != Method::Cancel {
        return None;
    }
    let branch = req.top_via().branch()?.to_string();
    Some((branch, req.call_id().to_string(), req.cseq().seq()))
}

/// `raw` with every header line named one of `names` (lowercase, long or
/// compact form) left out; `None` when it holds no header section.
fn without(raw: &[u8], names: &[&str]) -> Option<Vec<u8>> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let (head, body) = raw.split_at(end);
    let named = |line: &[u8]| {
        let name = line.split(|b| *b == b':').next().unwrap_or_default();
        let name = String::from_utf8_lossy(name);
        names.iter().any(|n| name.trim().eq_ignore_ascii_case(n))
    };
    let lines: Vec<&[u8]> = head.split(|b| *b == b'\n').collect();
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    for line in lines.iter().filter(|l| !named(l)) {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.extend_from_slice(body);
    Some(out)
}

#[cfg(test)]
mod tests {
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
        assert!(reason_only_cancel_repeat(&copy, &[first]));
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
        assert!(reason_only_cancel_repeat(&copy, &[first]));
    }

    #[test]
    fn a_cancel_differing_elsewhere_keeps_the_finding() {
        let first = entry(1, &cancel(None, "b@x"));
        let copy = entry(2, &cancel(Some("Q.850;cause=16"), "c@x"));
        assert!(!reason_only_cancel_repeat(&copy, &[first]));
    }
}
