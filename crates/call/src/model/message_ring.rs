//! The per-leg message ring: one [`MessageEntry`] per distinct SIP message the
//! stack receives or sends on a leg, in handling order, kept to a configured
//! cap. Retransmissions are not messages of their own — a repeated datagram,
//! inbound or outbound, adds nothing. Append helper:
//! [`crate::helpers::record_message`].

use serde::{Deserialize, Serialize};

/// Whose message an entry records: the peer's, one this stack forwarded from
/// another leg (RFC 3261 §16 semantics on a B2BUA), or one this stack minted
/// on its own account (a UAS/UAC-authored request or response).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDirection {
    Received,
    Relayed,
    Authored,
}

/// One message of a leg's history. `seq` is per call and monotonic across
/// legs, so the rings of two legs interleave by it; `at_ms` is the clock of
/// the turn that handled the message. `headers` holds every value, in wire
/// order, of each configured header name, in the order the names are
/// configured — `(canonical name, value)` pairs, one per header line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageEntry {
    pub seq: u32,
    pub at_ms: i64,
    pub direction: MessageDirection,
    /// The request method, or the method of the CSeq a response answers.
    pub method: String,
    pub cseq: u32,
    /// The status of a response; `None` on a request.
    pub code: Option<u16>,
    pub to_tag: Option<String>,
    /// The count of decisions applied to the call when the message was
    /// handled; `0` until the decision log stamps it.
    pub decision_ordinal: u32,
    pub headers: Vec<(String, String)>,
}

/// The last `cap` entries of a leg, and how many the cap evicted before them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRing {
    pub entries: Vec<MessageEntry>,
    pub dropped: u32,
}

impl MessageRing {
    /// Append `entry`, evicting from the front until at most `cap` entries
    /// remain; every eviction counts in `dropped`. A cap of `0` records
    /// nothing.
    pub fn push(&mut self, entry: MessageEntry, cap: usize) {
        if cap == 0 {
            return;
        }
        self.entries.push(entry);
        if self.entries.len() > cap {
            let excess = self.entries.len() - cap;
            self.entries.drain(..excess);
            self.dropped = self.dropped.saturating_add(excess as u32);
        }
    }

    /// The `seq` of the most recent entry, if any survives.
    pub fn last_seq(&self) -> Option<u32> {
        self.entries.last().map(|e| e.seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u32) -> MessageEntry {
        MessageEntry {
            seq,
            at_ms: 0,
            direction: MessageDirection::Received,
            method: "INVITE".into(),
            cseq: 1,
            code: None,
            to_tag: None,
            decision_ordinal: 0,
            headers: Vec::new(),
        }
    }

    #[test]
    fn keeps_the_last_cap_entries_and_counts_the_evicted() {
        let mut ring = MessageRing::default();
        for seq in 1..=5 {
            ring.push(entry(seq), 3);
        }
        let kept: Vec<u32> = ring.entries.iter().map(|e| e.seq).collect();
        assert_eq!(kept, vec![3, 4, 5]);
        assert_eq!(ring.dropped, 2);
        assert_eq!(ring.last_seq(), Some(5));
    }

    #[test]
    fn a_zero_cap_records_nothing() {
        let mut ring = MessageRing::default();
        ring.push(entry(1), 0);
        assert!(ring.entries.is_empty());
        assert_eq!(ring.dropped, 0);
        assert_eq!(ring.last_seq(), None);
    }
}
