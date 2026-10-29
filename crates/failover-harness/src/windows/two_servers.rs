//! The run-level fact a window of two serving nodes needs (ADR-0055 §11,
//! `W-off-reboot-two-servers`): one dialog took a BYE from two workers, each
//! ending the call on its own.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use sip_message::Method;
use sip_net_testkit::{CaptureIndex, RecordedSipEntry};

/// Whether some dialog drew a BYE from two of `workers`, the dialog named by
/// its Call-ID and the BYE's two tags.
pub(crate) fn byes_from_two_workers(entries: &[RecordedSipEntry], workers: &[SocketAddr]) -> bool {
    let capture = CaptureIndex::new(entries.to_vec());
    let mut senders: BTreeMap<(String, Option<String>, Option<String>), BTreeSet<SocketAddr>> =
        BTreeMap::new();
    for (i, entry) in capture.entries().iter().enumerate() {
        if !workers.contains(&entry.from) {
            continue;
        }
        let Some(bye) = capture.message(i).filter(|m| m.method() == Some(&Method::Bye)) else {
            continue;
        };
        let dialog = (bye.call_id.clone(), bye.from_tag.clone(), bye.to_tag.clone());
        senders.entry(dialog).or_default().insert(entry.from);
    }
    senders.values().any(|from| from.len() > 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const B1: &str = "127.0.0.1:5091";
    const B1_REBOOTED: &str = "127.0.2.1:5091";
    const ALICE: &str = "127.0.0.1:5060";

    fn bye(seq: u64, from: &str, cseq: u32) -> RecordedSipEntry {
        let raw = format!(
            "BYE sip:alice@x SIP/2.0\r\nVia: SIP/2.0/UDP {from};branch=z9hG4bK-{seq}\r\n\
             From: <sip:b2bua@x>;tag=ours\r\nTo: <sip:alice@x>;tag=hers\r\nCall-ID: c1\r\n\
             CSeq: {cseq} BYE\r\nContent-Length: 0\r\n\r\n"
        );
        RecordedSipEntry {
            seq,
            from: from.parse().unwrap(),
            to: "127.0.0.1:5080".parse().unwrap(),
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

    fn workers() -> Vec<SocketAddr> {
        vec![B1.parse().unwrap(), "127.0.0.1:5092".parse().unwrap(), B1_REBOOTED.parse().unwrap()]
    }

    #[test]
    fn a_dialog_byed_by_two_workers_is_two_servers() {
        let entries = [bye(1, "127.0.0.1:5092", 2), bye(2, B1_REBOOTED, 2)];
        assert!(byes_from_two_workers(&entries, &workers()));
    }

    #[test]
    fn one_workers_bye_and_its_repeat_or_a_partys_bye_is_not() {
        let entries = [bye(1, B1, 2), bye(2, B1, 2), bye(3, ALICE, 3)];
        assert!(!byes_from_two_workers(&entries, &workers()));
    }
}
