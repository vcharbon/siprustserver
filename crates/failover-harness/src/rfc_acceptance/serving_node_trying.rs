//! The extra 100 Trying a change of serving node explains (ADR-0070): a node
//! that takes an undecided call over, or back, sends the caller a 100 as it
//! asks the routing decision again, since it cannot tell whether the other
//! node's left. Each node's server transaction sent one 100 (RFC 3261
//! §17.2.1) and the front proxy is stateless (§16.11), which §16.7 step 5
//! does not bind; the caller's `Proceeding` client transaction absorbs it
//! (§17.1.1.2). The failover audit records it as informative, never gating.

use std::net::SocketAddr;

use sip_message::Method;
use sip_net_testkit::{CaptureIndex, Captured};

/// The audit rule a caller taking more 100s than the INVITEs she sent meets.
const RULE_EXTRA_TRYING: &str = "proxy-100-trying-not-forwarded";

/// Whether the finding of `rule` whose offending message is the 1-based
/// `offending` index into `capture` is a caller's extra 100 a change of
/// serving node explains ([`one_more_per_serving_node`]).
pub(super) fn explained(rule: &str, offending: Option<usize>, capture: &CaptureIndex) -> bool {
    let i = offending.and_then(|i| i.checked_sub(1));
    rule == RULE_EXTRA_TRYING && i.is_some_and(|i| one_more_per_serving_node(i, capture))
}

/// Whether the 100 Trying at index `i` of `capture`, sent by a hop to the
/// caller of an initial INVITE, is within the caller's budget once each change
/// of the node sending that INVITE's 100s to the hop counts one more: she
/// takes at most one 100 per INVITE she sent plus one per change of node,
/// and at least one change happened.
fn one_more_per_serving_node(i: usize, capture: &CaptureIndex) -> bool {
    let (Some(entry), Some(trying)) = (capture.entry(i), capture.message(i)) else { return false };
    if trying.status() != Some(100) || trying.cseq_method != Method::Invite {
        return false;
    }
    let (caller, hop) = (entry.to, entry.from);
    let of_it = |m: &Captured| m.call_id == trying.call_id && m.cseq == trying.cseq;
    let invites: Vec<&Captured> = capture
        .iter()
        .filter(|(_, e, m)| e.from == caller && m.method() == Some(&Method::Invite) && of_it(m))
        .map(|(_, _, m)| m)
        .collect();
    if invites.is_empty() || invites.iter().any(|m| m.to_tag.is_some()) {
        return false;
    }
    let tryings = |to: SocketAddr| {
        capture
            .iter()
            .filter(move |(_, e, m)| e.to == to && m.status() == Some(100) && of_it(m))
            .map(|(_, e, _)| e.from)
    };
    let taken = tryings(caller).count();
    let senders: Vec<SocketAddr> = tryings(hop).filter(|from| *from != caller).collect();
    let changes = senders.windows(2).filter(|pair| pair[0] != pair[1]).count();
    changes > 0 && taken <= invites.len() + changes
}

#[cfg(test)]
mod tests {
    use sip_net_testkit::RecordedSipEntry;

    use super::*;

    const ALICE: &str = "127.0.0.1:5060";
    const PROXY: &str = "127.0.0.1:5080";
    const B1: &str = "127.0.0.1:5091";
    const B2: &str = "127.0.0.1:5092";

    fn hop(seq: u64, from: &str, to: &str, raw: String) -> RecordedSipEntry {
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

    fn invite(seq: u64, from: &str, to: &str, to_tag: &str) -> RecordedSipEntry {
        let raw = format!(
            "INVITE sip:bob@x SIP/2.0\r\nMax-Forwards: 70\r\nVia: SIP/2.0/UDP {from};branch=z9hG4bK-a\r\n\
             From: <sip:alice@x>;tag=a\r\nTo: <sip:bob@x>{to_tag}\r\nCall-ID: c1\r\n\
             CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        );
        hop(seq, from, to, raw)
    }

    fn trying(seq: u64, from: &str, to: &str) -> RecordedSipEntry {
        let raw = "SIP/2.0 100 Trying\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a\r\n\
                   From: <sip:alice@x>;tag=a\r\nTo: <sip:bob@x>\r\nCall-ID: c1\r\n\
                   CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
            .to_string();
        hop(seq, from, to, raw)
    }

    /// The INVITE through the proxy to `first`, its 100 back, and then the
    /// worker-side 100s of `then` each relayed to the caller.
    fn served(first: &str, then: &[&str], to_tag: &str) -> Vec<RecordedSipEntry> {
        let mut entries = vec![
            invite(1, ALICE, PROXY, to_tag),
            invite(2, PROXY, first, to_tag),
            trying(3, first, PROXY),
            trying(4, PROXY, ALICE),
        ];
        for (n, node) in then.iter().enumerate() {
            let seq = 10 + 2 * n as u64;
            entries.push(trying(seq, node, PROXY));
            entries.push(trying(seq + 1, PROXY, ALICE));
        }
        entries
    }

    fn last_admitted(entries: Vec<RecordedSipEntry>) -> bool {
        let i = entries.len() - 1;
        one_more_per_serving_node(i, &CaptureIndex::new(entries))
    }

    #[test]
    fn a_takeovers_100_is_admitted() {
        assert!(last_admitted(served(B2, &[B1], "")));
    }

    #[test]
    fn a_takeover_then_a_reclaim_each_send_one() {
        assert!(last_admitted(served(B2, &[B1, B2], "")));
    }

    #[test]
    fn a_second_100_from_the_same_node_is_not() {
        assert!(!last_admitted(served(B2, &[B2], "")));
    }

    #[test]
    fn two_100s_from_the_node_taking_over_are_not() {
        assert!(!last_admitted(served(B2, &[B1, B1], "")));
    }

    #[test]
    fn a_re_invites_extra_100_is_not() {
        assert!(!last_admitted(served(B2, &[B1], ";tag=b")));
    }
}
