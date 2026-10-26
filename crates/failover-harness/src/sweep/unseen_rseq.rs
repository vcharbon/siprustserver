//! A declared window of a kill and of a split that heals (ADR-0055 §11): a
//! copy's reliable provisional under the dialog's derived tag whose RSeq
//! cannot continue the sequence the dead primary sent (RFC 3262 §4), once per
//! dialog.

use sip_message::header::{HeaderName, RSeq};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipResponse};
use sip_net_testkit::RecordedSipEntry;

use crate::cluster::Cluster;

/// The audit rules a reliable provisional out of its RSeq sequence meets.
const RULES: [&str; 2] = ["non-contiguous-rseq", "no-prack-of-out-of-order-rseq"];

/// Declare, from the incident on, the copy's reliable provisional that cannot
/// continue the dead primary's RSeq, once per dialog.
pub(super) fn accept_unseen_rseq(cluster: &mut Cluster) {
    for rule in RULES {
        cluster.fh.accept_rfc_deviations_once_per_dialog_where(
            rule,
            from_another_node,
            "ADR-0055 §11: a copy's reliable provisional under the dialog's derived tag cannot \
             continue the RSeq the dead primary sent",
        );
    }
}

/// Whether `entry` is a reliable provisional under a To-tag a reliable
/// provisional captured before the window (`from_seq`) already carried on
/// that INVITE, stated by another node: the two name different Contacts. A
/// node derives the dialog's tag as the first owner did, and its Contact is
/// its own.
fn from_another_node(
    entry: &RecordedSipEntry,
    earlier: &[RecordedSipEntry],
    from_seq: u64,
) -> bool {
    let Some((key, contact)) = reliable_provisional(&entry.raw) else { return false };
    earlier
        .iter()
        .filter(|e| e.seq <= from_seq)
        .any(|e| reliable_provisional(&e.raw).is_some_and(|(k, c)| k == key && c != contact))
}

/// The Call-ID, To-tag and CSeq number of a reliable provisional, and its
/// Contact.
fn reliable_provisional(raw: &[u8]) -> Option<((String, String, u32), String)> {
    let Ok(SipMessage::Response(resp)) = CustomParser::new().parse(raw) else { return None };
    if !(101..200).contains(&resp.status()) || !carries_rseq(&resp) {
        return None;
    }
    let tag = resp.to().tag()?.to_string();
    let contact = resp.raw(HeaderName::Contact).next()?.to_string();
    Some(((resp.call_id().to_string(), tag, resp.cseq().seq()), contact))
}

fn carries_rseq(resp: &SipResponse) -> bool {
    resp.header::<RSeq>().is_some_and(|r| r.is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, raw: String) -> RecordedSipEntry {
        RecordedSipEntry {
            seq,
            from: "127.0.0.1:5080".parse().unwrap(),
            to: "127.0.0.1:5060".parse().unwrap(),
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

    /// A reliable 183 under the tag `tag`, numbered `rseq`, stated by `node`.
    fn reliable(tag: &str, rseq: u32, node: &str) -> String {
        format!(
            "SIP/2.0 183 Session Progress\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a1\r\n\
             From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>;tag={tag}\r\nCall-ID: c1\r\nCSeq: 1 INVITE\r\n\
             RSeq: {rseq}\r\nContact: <sip:b2bua@{node}>\r\nContent-Length: 0\r\n\r\n"
        )
    }

    const DEAD: &str = "127.0.0.1:5091";
    const COPY: &str = "127.0.0.1:5092";

    #[test]
    fn a_copys_provisional_under_the_dead_owners_tag_is_declared() {
        let earlier = [entry(1, reliable("t1", 7, DEAD))];
        assert!(from_another_node(&entry(5, reliable("t1", 1, COPY)), &earlier, 2));
    }

    #[test]
    fn the_same_nodes_provisional_keeps_the_finding() {
        let earlier = [entry(1, reliable("t1", 7, DEAD))];
        assert!(!from_another_node(&entry(5, reliable("t1", 9, DEAD)), &earlier, 2));
    }

    #[test]
    fn another_tag_or_a_provisional_after_the_incident_keeps_the_finding() {
        let before = [entry(1, reliable("t1", 7, DEAD))];
        assert!(!from_another_node(&entry(5, reliable("t2", 1, COPY)), &before, 2));
        let after = [entry(3, reliable("t1", 7, DEAD))];
        assert!(!from_another_node(&entry(5, reliable("t1", 1, COPY)), &after, 2));
    }
}
