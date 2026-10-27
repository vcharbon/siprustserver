//! A declared window of a kill and of a split that heals (ADR-0055 §11): a
//! copy's reliable provisional under the dialog's derived tag whose RSeq
//! cannot continue the sequence the dead primary sent (RFC 3262 §4), once per
//! dialog.

use std::net::SocketAddr;

use sip_net_testkit::{CaptureIndex, Captured};

use crate::cluster::Cluster;

/// The audit rules a reliable provisional out of its RSeq sequence meets.
const RULES: [&str; 2] = ["non-contiguous-rseq", "no-prack-of-out-of-order-rseq"];

/// Declare, from the incident on, the copy's reliable provisional that cannot
/// continue the dead primary's RSeq, once per dialog.
pub(super) fn accept_unseen_rseq(cluster: &mut Cluster) {
    for rule in RULES {
        cluster.fh.accept_rfc_deviations_once_per_dialog_where(
            rule,
            |i, capture, _| from_another_node(i, capture),
            "ADR-0055 §11: a copy's reliable provisional under the dialog's derived tag cannot \
             continue the RSeq the dead primary sent",
        );
    }
}

/// Whether the entry at index `i` of `capture` is a reliable provisional
/// under a To-tag an earlier reliable provisional carried on that INVITE,
/// stated by another node — a node derives the dialog's tag as the first
/// owner did — or the caller's PRACK of such a provisional. The window it
/// falls in opened at the incident.
fn from_another_node(i: usize, capture: &CaptureIndex) -> bool {
    if let Some(j) = acknowledged(i, capture) {
        return from_another_node(j, capture);
    }
    let provisional = capture.message(i).and_then(reliable_provisional);
    let (Some(entry), Some(provisional)) = (capture.entry(i), provisional) else { return false };
    let node = stating_node(i, entry.from, &provisional, capture);
    capture.before(i).any(|(j, e, m)| {
        reliable_provisional(m).is_some_and(|other| {
            other.0 == provisional.0 && stating_node(j, e.from, &other, capture) != node
        })
    })
}

/// The node that stated the reliable provisional the entry at index `i`,
/// sent by `from`, carries: the sender of its first hop, the last entry
/// before it carrying the same provisional (a copy may number its own as the
/// dead owner did), or `from`. Every copy of a call states the same Contact
/// (ADR-0055 §14), so the hop, not the message, names the node.
fn stating_node(
    i: usize,
    from: SocketAddr,
    provisional: &Provisional,
    capture: &CaptureIndex,
) -> SocketAddr {
    capture
        .before(i)
        .rev()
        .find(|(_, _, m)| reliable_provisional(m).as_ref() == Some(provisional))
        .map_or(from, |(_, first, _)| first.from)
}

/// The index of the reliable provisional before index `i` of `capture` that
/// the PRACK there acknowledges: the last one of its Call-ID and INVITE whose
/// RSeq its RAck names.
fn acknowledged(i: usize, capture: &CaptureIndex) -> Option<usize> {
    let prack = capture.message(i).filter(|m| m.method().is_some())?;
    let (rseq, cseq) = prack.rack?;
    capture
        .before(i)
        .rev()
        .find(|(_, _, resp)| {
            resp.status().is_some()
                && resp.call_id == prack.call_id
                && resp.cseq == cseq
                && resp.rseq == Some(rseq)
        })
        .map(|(j, _, _)| j)
}

/// A reliable provisional: its Call-ID, To-tag and CSeq number, and its RSeq.
type Provisional = ((String, String, u32), u32);

fn reliable_provisional(m: &Captured) -> Option<Provisional> {
    if !m.status().is_some_and(|s| (101..200).contains(&s)) {
        return None;
    }
    let rseq = m.rseq?;
    let tag = m.to_tag.clone()?;
    Some(((m.call_id.clone(), tag, m.cseq), rseq))
}

#[cfg(test)]
mod tests {
    use sip_net_testkit::RecordedSipEntry;

    use super::*;

    const PROXY: &str = "127.0.0.1:5080";

    fn entry(seq: u64, raw: String) -> RecordedSipEntry {
        hop(seq, PROXY, raw)
    }

    /// `raw` sent by `from` to the caller.
    fn hop(seq: u64, from: &str, raw: String) -> RecordedSipEntry {
        RecordedSipEntry {
            seq,
            from: from.parse().unwrap(),
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

    /// A reliable 183 under the tag `tag`, numbered `rseq`, every copy's
    /// Contact naming the proxy.
    fn reliable(tag: &str, rseq: u32) -> String {
        format!(
            "SIP/2.0 183 Session Progress\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a1\r\n\
             From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>;tag={tag}\r\nCall-ID: c1\r\nCSeq: 1 INVITE\r\n\
             RSeq: {rseq}\r\nContact: <sip:b2bua@{PROXY}>\r\nContent-Length: 0\r\n\r\n"
        )
    }

    /// The provisional `node` states under `tag` numbered `rseq`: its hop to
    /// the proxy at `seq`, and the proxy's to the caller just after.
    fn stated(seq: u64, node: &str, tag: &str, rseq: u32) -> [RecordedSipEntry; 2] {
        [hop(seq, node, reliable(tag, rseq)), entry(seq + 1, reliable(tag, rseq))]
    }

    /// Whether the last of `earlier` then `last` is declared.
    fn declared(earlier: Vec<RecordedSipEntry>, last: RecordedSipEntry) -> bool {
        let i = earlier.len();
        let mut capture = earlier;
        capture.push(last);
        from_another_node(i, &CaptureIndex::new(capture))
    }

    const DEAD: &str = "127.0.0.1:5091";
    const COPY: &str = "127.0.0.1:5092";

    /// The trace before the caller receives the last of `provisionals`, each
    /// `(node, tag, rseq)`: every hop of the others, the last one's first hop.
    fn trace(provisionals: &[(&str, &str, u32)]) -> (Vec<RecordedSipEntry>, RecordedSipEntry) {
        let mut earlier: Vec<RecordedSipEntry> = provisionals
            .iter()
            .enumerate()
            .flat_map(|(i, (node, tag, rseq))| stated(10 * i as u64, node, tag, *rseq))
            .collect();
        let last = earlier.pop().expect("one provisional");
        (earlier, last)
    }

    #[test]
    fn a_copys_provisional_under_the_dead_owners_tag_is_declared() {
        let (earlier, copy) = trace(&[(DEAD, "t1", 7), (COPY, "t1", 1)]);
        assert!(declared(earlier, copy));
    }

    #[test]
    fn a_copys_provisional_numbered_as_the_dead_owners_is_declared() {
        let (earlier, copy) = trace(&[(DEAD, "t1", 7), (DEAD, "t1", 8), (COPY, "t1", 8)]);
        assert!(declared(earlier, copy));
    }

    #[test]
    fn the_same_nodes_provisional_keeps_the_finding() {
        let (earlier, again) = trace(&[(DEAD, "t1", 7), (DEAD, "t1", 9)]);
        assert!(!declared(earlier, again));
    }

    /// The caller's PRACK of the provisional numbered `rseq`.
    fn prack(rseq: u32) -> String {
        format!(
            "PRACK sip:b2bua@x SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-p1\r\n\
             From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>;tag=t1\r\nCall-ID: c1\r\nCSeq: 2 PRACK\r\n\
             RAck: {rseq} 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        )
    }

    #[test]
    fn the_callers_prack_of_a_copys_provisional_is_declared() {
        let answered = || {
            let (mut earlier, last) = trace(&[(DEAD, "t1", 7), (COPY, "t1", 1)]);
            earlier.push(last);
            earlier
        };
        assert!(declared(answered(), entry(30, prack(1))));
        assert!(!declared(answered(), entry(30, prack(7))), "the dead owner's");
    }

    #[test]
    fn a_provisional_under_another_tag_keeps_the_finding() {
        let (earlier, other) = trace(&[(DEAD, "t1", 7), (COPY, "t2", 1)]);
        assert!(!declared(earlier, other));
    }
}
