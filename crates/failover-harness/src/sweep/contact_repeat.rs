//! A stall's declared repeat (ADR-0055 §11): the copy relays the callee's
//! repeated 2xx to the caller after the stalled primary relayed it, the two
//! the same message but for the Contact each node states.

use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};
use sip_net_testkit::RecordedSipEntry;

use super::parties::ALICE;
use crate::cluster::Cluster;

/// The audit rule a retransmission that is not byte-identical meets.
const RULE_RUNG_BYTE_IDENTICAL: &str = "rung-byte-identical";

/// Declare, from the stall on, the copy's repeat of the caller's 2xx that
/// differs from the dead owner's only in its own Contact.
pub(super) fn accept_contact_only_2xx_repeat(cluster: &mut Cluster) {
    cluster.fh.accept_rfc_deviations_from_now_where(
        RULE_RUNG_BYTE_IDENTICAL,
        |copy, earlier, _| contact_only_2xx_repeat(copy, earlier),
        "ADR-0055 §11: a stall: the copy's repeat of the caller's 2xx differs from the dead \
         owner's only in its own Contact",
    );
}

/// Whether `copy` is a 2xx to an INVITE sent to the caller whose first copy
/// in `earlier` — same top Via branch, Call-ID, CSeq and status, sent to her
/// — differs from it in the Contact header field and nowhere else.
fn contact_only_2xx_repeat(copy: &RecordedSipEntry, earlier: &[RecordedSipEntry]) -> bool {
    let Ok(alice) = ALICE.parse::<std::net::SocketAddr>() else { return false };
    if copy.to != alice {
        return false;
    }
    let Some(key) = key_of(&copy.raw) else { return false };
    let Some(first) = earlier.iter().find(|e| e.to == alice && key_of(&e.raw) == Some(key.clone()))
    else {
        return false;
    };
    let (theirs, ours) = (without_contact(&first.raw), without_contact(&copy.raw));
    theirs.is_some() && theirs == ours && first.raw != copy.raw
}

/// The top Via branch, Call-ID, CSeq number and status of a 2xx to an INVITE.
fn key_of(raw: &[u8]) -> Option<(String, String, u32, u16)> {
    let Ok(SipMessage::Response(resp)) = CustomParser::new().parse(raw) else { return None };
    if !(200..300).contains(&resp.status()) || resp.cseq().method() != Method::Invite {
        return None;
    }
    let branch = resp.top_via().branch()?.to_string();
    Some((branch, resp.call_id().to_string(), resp.cseq().seq(), resp.status()))
}

/// `raw` with its Contact header line (long or compact form) left out; `None`
/// when it holds no header section or not exactly one Contact line.
fn without_contact(raw: &[u8]) -> Option<Vec<u8>> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let (head, body) = raw.split_at(end);
    let is_contact = |line: &[u8]| {
        let name = line.split(|b| *b == b':').next().unwrap_or_default();
        let name = String::from_utf8_lossy(name);
        let name = name.trim();
        name.eq_ignore_ascii_case("contact") || name.eq_ignore_ascii_case("m")
    };
    let lines: Vec<&[u8]> = head.split(|b| *b == b'\n').collect();
    if lines.iter().filter(|l| is_contact(l)).count() != 1 {
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    for line in lines.iter().filter(|l| !is_contact(l)) {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.extend_from_slice(body);
    Some(out)
}

#[cfg(test)]
mod tests {
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

    fn ok(contact: &str, body: &str) -> String {
        format!(
            "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP {ALICE};branch=z9hG4bK-a1\r\n\
             From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>;tag=b\r\nCall-ID: c1\r\nCSeq: 1 INVITE\r\n\
             Contact: <sip:b2bua@{contact}>\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn only_the_contact_differs() {
        let first = entry(1, &ok("127.0.0.1:5091", "v=0\r\n"));
        let copy = entry(2, &ok("127.0.0.1:5092", "v=0\r\n"));
        assert!(contact_only_2xx_repeat(&copy, &[first]));
    }

    #[test]
    fn another_difference_keeps_the_finding() {
        let first = entry(1, &ok("127.0.0.1:5091", "v=0\r\n"));
        let copy = entry(2, &ok("127.0.0.1:5092", "v=1\r\n"));
        assert!(!contact_only_2xx_repeat(&copy, &[first]));
    }

    #[test]
    fn a_repeat_toward_another_party_keeps_the_finding() {
        let first = entry(1, &ok("127.0.0.1:5091", "v=0\r\n"));
        let mut copy = entry(2, &ok("127.0.0.1:5092", "v=0\r\n"));
        copy.to = "127.0.0.1:5070".parse().unwrap();
        assert!(!contact_only_2xx_repeat(&copy, &[first]));
    }
}
