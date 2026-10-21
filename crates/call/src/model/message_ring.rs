//! The per-leg message ring: one [`MessageEntry`] per distinct SIP message the
//! stack receives or sends on a leg, in handling order, kept to a configured
//! cap. Retransmissions are not messages of their own — a repeated datagram,
//! inbound or outbound, adds nothing. The leg's opening (its initial INVITE
//! transaction, [`MessageRing::push`]) is never evicted by what follows it.
//! Append helper: [`crate::helpers::record_message`].

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
/// the turn that handled the message and `turn` its number, which tells apart
/// two turns sharing a millisecond. `headers` holds every value, in wire
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
    /// handled (`Call::decision_ordinal` at the append); `0` before the
    /// first decision. The decision current at handling, not the leg's: a
    /// late message of an earlier leg — its 487 arriving after a failover
    /// folded — is stamped with the failover's ordinal.
    pub decision_ordinal: u32,
    pub headers: Vec<(String, String)>,
    /// The turn that handled the message (`Call::message_turn` at the
    /// append): the message handled, what the transaction layer answered it
    /// with on its own and what the turn sent share one number. Turns count
    /// from `1` per call; `0` is no turn.
    pub turn: u32,
}

/// A leg's opening and the last `cap` entries after it, in handling order,
/// and how many entries the cap evicted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRing {
    pub entries: Vec<MessageEntry>,
    pub dropped: u32,
}

impl MessageRing {
    /// Append `entry` under `cap`; every eviction counts in `dropped`. A cap
    /// of `0` records nothing.
    ///
    /// A leg whose first entry is its initial INVITE (a request with no
    /// To-tag) keeps that INVITE's transaction as its opening: every entry up
    /// to and including the first final response to it, whatever early
    /// dialogs came before (RFC 3261 §12.1), and the ACK of a 2xx final
    /// (§13.2.2.4). Evictions take the oldest entry after the opening; an
    /// opening still waiting for its final keeps the INVITE and its last
    /// `cap - 1` entries.
    pub fn push(&mut self, entry: MessageEntry, cap: usize) {
        if cap == 0 {
            return;
        }
        self.entries.push(entry);
        let opening = self.opening();
        let evicted: Vec<usize> = if opening.closed {
            let pinned = opening.len + usize::from(opening.ack.is_some());
            let excess = self.entries.len().saturating_sub(pinned + cap);
            (opening.len..self.entries.len())
                .filter(|i| Some(*i) != opening.ack)
                .take(excess)
                .collect()
        } else {
            (1..self.entries.len().saturating_sub(cap - 1)).collect()
        };
        if evicted.is_empty() {
            return;
        }
        let mut index = 0;
        self.entries.retain(|_| {
            let kept = !evicted.contains(&index);
            index += 1;
            kept
        });
        let count = u32::try_from(evicted.len()).unwrap_or(u32::MAX);
        self.dropped = self.dropped.saturating_add(count);
    }

    /// The `seq` of the most recent entry, if any survives.
    pub fn last_seq(&self) -> Option<u32> {
        self.entries.last().map(|e| e.seq)
    }

    /// The leg's opening as the entries state it.
    fn opening(&self) -> Opening {
        let Some(invite) = self.entries.first().filter(|e| is_initial_invite(e)) else {
            return Opening { len: 0, closed: true, ack: None };
        };
        let final_at = self.entries.iter().enumerate().find(|(_, e)| is_final_to(e, invite));
        let Some((at, response)) = final_at else {
            return Opening { len: self.entries.len(), closed: false, ack: None };
        };
        let len = at + 1;
        let answered = response.code.is_some_and(|c| (200..300).contains(&c));
        let ack = answered
            .then(|| self.entries.iter().enumerate().skip(len).find(|(_, e)| is_ack_of(e, invite)))
            .flatten()
            .map(|(i, _)| i);
        Opening { len, closed: true, ack }
    }
}

/// The leading entries no eviction takes, and the index of the 2xx's ACK.
struct Opening {
    len: usize,
    closed: bool,
    ack: Option<usize>,
}

fn is_initial_invite(e: &MessageEntry) -> bool {
    e.method == "INVITE" && e.code.is_none() && e.to_tag.is_none()
}

/// Whether the travel of `a` and `b` is the same: both taken from the peer,
/// or both sent to it.
fn same_way(a: &MessageEntry, b: &MessageEntry) -> bool {
    (a.direction == MessageDirection::Received) == (b.direction == MessageDirection::Received)
}

/// A final response to `invite`: its method and CSeq, travelling the other
/// way (RFC 3261 §8.1.3.3).
fn is_final_to(e: &MessageEntry, invite: &MessageEntry) -> bool {
    e.method == "INVITE"
        && e.cseq == invite.cseq
        && e.code.is_some_and(|c| c >= 200)
        && !same_way(e, invite)
}

/// The ACK of a 2xx to `invite`: its CSeq number, sent the way it was.
fn is_ack_of(e: &MessageEntry, invite: &MessageEntry) -> bool {
    e.method == "ACK" && e.code.is_none() && e.cseq == invite.cseq && same_way(e, invite)
}

#[cfg(test)]
mod tests {
    use super::MessageDirection::{Authored, Received, Relayed};
    use super::*;

    fn msg(
        seq: u32,
        direction: MessageDirection,
        method: &str,
        cseq: u32,
        code: Option<u16>,
        to_tag: Option<&str>,
    ) -> MessageEntry {
        MessageEntry {
            seq,
            at_ms: 0,
            direction,
            method: method.into(),
            cseq,
            code,
            to_tag: to_tag.map(str::to_string),
            decision_ordinal: 0,
            headers: Vec::new(),
            turn: 1,
        }
    }

    fn seqs(ring: &MessageRing) -> Vec<u32> {
        ring.entries.iter().map(|e| e.seq).collect()
    }

    /// The caller's INVITE, the 180 and 200 sent back, the caller's ACK:
    /// seqs 1 to 4.
    fn answered_caller_leg(cap: usize) -> MessageRing {
        let mut ring = MessageRing::default();
        ring.push(msg(1, Received, "INVITE", 1, None, None), cap);
        ring.push(msg(2, Authored, "INVITE", 1, Some(180), Some("t")), cap);
        ring.push(msg(3, Authored, "INVITE", 1, Some(200), Some("t")), cap);
        ring.push(msg(4, Received, "ACK", 1, None, Some("t")), cap);
        ring
    }

    /// `count` caller INFO transactions from seq `from`, CSeq 2 on.
    fn infos(ring: &mut MessageRing, from: u32, count: u32, cap: usize) {
        for n in 0..count {
            let (seq, cseq) = (from + 2 * n, 2 + n);
            ring.push(msg(seq, Received, "INFO", cseq, None, Some("t")), cap);
            ring.push(msg(seq + 1, Relayed, "INFO", cseq, Some(200), Some("t")), cap);
        }
    }

    #[test]
    fn a_leg_with_no_opening_keeps_its_last_cap_entries() {
        let mut ring = MessageRing::default();
        for seq in 1..=5 {
            ring.push(msg(seq, Received, "INFO", seq, None, Some("t")), 3);
        }
        assert_eq!(seqs(&ring), [3, 4, 5]);
        assert_eq!(ring.dropped, 2);
        assert_eq!(ring.last_seq(), Some(5));
    }

    #[test]
    fn a_zero_cap_records_nothing() {
        let mut ring = MessageRing::default();
        ring.push(msg(1, Received, "INVITE", 1, None, None), 0);
        assert!(ring.entries.is_empty());
        assert_eq!(ring.dropped, 0);
        assert_eq!(ring.last_seq(), None);
    }

    /// The INVITE, its answers and the ACK of the 2xx outlive any number of
    /// later transactions; the cap applies to what follows them.
    #[test]
    fn the_opening_outlives_the_traffic_after_it() {
        let mut ring = answered_caller_leg(4);
        infos(&mut ring, 5, 50, 4);
        assert_eq!(seqs(&ring), [1, 2, 3, 4, 101, 102, 103, 104]);
        assert_eq!(ring.dropped, 96);
    }

    /// Early dialogs under two To-tags: the opening runs to the first final,
    /// whichever early dialog it answers, past the cap.
    #[test]
    fn the_opening_holds_every_early_dialog_up_to_the_first_final() {
        let cap = 3;
        let mut ring = MessageRing::default();
        ring.push(msg(1, Relayed, "INVITE", 7, None, None), cap);
        ring.push(msg(2, Received, "INVITE", 7, Some(183), Some("x")), cap);
        ring.push(msg(3, Received, "INVITE", 7, Some(180), Some("y")), cap);
        ring.push(msg(4, Received, "INVITE", 7, Some(200), Some("y")), cap);
        ring.push(msg(5, Authored, "ACK", 7, None, Some("y")), cap);
        assert_eq!(seqs(&ring), [1, 2, 3, 4, 5], "an opening of two early dialogs, cap {cap}");
        ring.push(msg(6, Received, "BYE", 1, None, Some("y")), cap);
        ring.push(msg(7, Authored, "BYE", 1, Some(200), Some("y")), cap);
        ring.push(msg(8, Received, "INFO", 2, None, Some("y")), cap);
        ring.push(msg(9, Authored, "INFO", 2, Some(200), Some("y")), cap);
        assert_eq!(seqs(&ring), [1, 2, 3, 4, 5, 7, 8, 9]);
    }

    /// The 2xx's ACK stays wherever it lands after the answer; the entries
    /// before it are the ring's to evict.
    #[test]
    fn a_late_ack_of_the_2xx_is_kept() {
        let cap = 2;
        let mut ring = MessageRing::default();
        ring.push(msg(1, Received, "INVITE", 1, None, None), cap);
        ring.push(msg(2, Authored, "INVITE", 1, Some(200), Some("t")), cap);
        ring.push(msg(3, Authored, "INFO", 9, None, Some("caller")), cap);
        ring.push(msg(4, Received, "INFO", 9, Some(200), Some("caller")), cap);
        ring.push(msg(5, Received, "ACK", 1, None, Some("t")), cap);
        infos(&mut ring, 6, 3, cap);
        assert_eq!(seqs(&ring), [1, 2, 5, 10, 11]);
        assert_eq!(ring.dropped, 6);
    }

    /// A failure final closes the opening; the ACK of a non-2xx is the
    /// transaction's (RFC 3261 §17.1.1.3), not the opening's.
    #[test]
    fn a_failure_final_closes_the_opening_without_its_ack() {
        let cap = 1;
        let mut ring = MessageRing::default();
        ring.push(msg(1, Relayed, "INVITE", 3, None, None), cap);
        ring.push(msg(2, Received, "INVITE", 3, Some(486), Some("x")), cap);
        ring.push(msg(3, Authored, "ACK", 3, None, Some("x")), cap);
        ring.push(msg(4, Authored, "CANCEL", 3, None, None), cap);
        assert_eq!(seqs(&ring), [1, 2, 4]);
        assert_eq!(ring.dropped, 1);
    }

    /// An opening waiting for its final keeps the INVITE and its last
    /// `cap - 1` entries; the final then joins it.
    #[test]
    fn an_opening_waiting_for_its_final_is_capped_after_the_invite() {
        let cap = 3;
        let mut ring = MessageRing::default();
        ring.push(msg(1, Received, "INVITE", 1, None, None), cap);
        for seq in 2..=9 {
            ring.push(msg(seq, Authored, "INVITE", 1, Some(180), Some("t")), cap);
        }
        assert_eq!(seqs(&ring), [1, 8, 9]);
        ring.push(msg(10, Authored, "INVITE", 1, Some(487), Some("t")), cap);
        assert_eq!(seqs(&ring), [1, 8, 9, 10]);
        assert_eq!(ring.dropped, 6);
    }

    /// A 200 travelling the INVITE's way under its number answers a request
    /// the other side sent: it is no final of the INVITE.
    #[test]
    fn a_response_the_invites_way_does_not_close_the_opening() {
        let cap = 2;
        let mut ring = MessageRing::default();
        ring.push(msg(1, Received, "INVITE", 1, None, None), cap);
        ring.push(msg(2, Received, "INVITE", 1, Some(200), Some("caller")), cap);
        ring.push(msg(3, Authored, "INVITE", 1, Some(180), Some("t")), cap);
        assert_eq!(seqs(&ring), [1, 3], "still open: capped after the INVITE");
    }
}
