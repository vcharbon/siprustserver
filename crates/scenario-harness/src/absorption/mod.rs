//! RFC 3261 §17.2 absorption — THE one classification in this tree — and the
//! two views of the datagram stream it separates (issue 22).
//!
//! A receiving stack sees one stream of datagrams and owes two answers about
//! each: "did it arrive?" and "did the logic see it?". They differ, and the
//! difference is per datagram:
//!
//! | datagram | wire view | TU view |
//! |---|---|---|
//! | retransmitted INVITE / non-2xx final | seen | absorbed by the transaction layer |
//! | ACK to a non-2xx final | seen | absorbed — hop-by-hop, owned by the INVITE server transaction |
//! | retransmitted 2xx to INVITE | seen | seen — 2xx retransmission is end-to-end, the TU re-sends |
//! | ACK to a 2xx (each one) | seen | seen — the UAC core, not the transaction layer, sends every one |
//!
//! [`Absorption::sight`] is the single seam: every inbound datagram passes it
//! exactly once, is stamped [`SeenBy`], and lands in the wire log. The **wire
//! view** ([`Absorption::wire_view`]) is every datagram in arrival order; the
//! **TU view** ([`Absorption::tu_view`]) is the `SeenBy::Both` subset. Both
//! agent modes — the fluent harness UA and the pivot interpreter's raw-datagram
//! driver — read this module, so retransmit counting, `repeat_of` attribution
//! and the harness's own waits can never disagree.
//!
//! **The absorption rule: absorb only a PROVABLE duplicate.** A datagram is a
//! repeat iff it is byte-identical to one already sighted under the same key.
//! There is no absorb-set, no window and no method-name tolerance list: those
//! over-approximate "a repeat of what I already saw" into "any message of this
//! method", which can swallow a genuine signal.

mod hop_ack;
mod two_xx_ack;
mod wire_log;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sip_net::repeat::{repeat_belongs_to_tu, RepeatTables};

use sip_message::{SipMessage, SipRequest};

pub(crate) use two_xx_ack::TwoXxAcks;
pub use two_xx_ack::{ack_key_of_ack, ack_key_of_final, AckKey};
pub use wire_log::WireEntry;

/// Which view of the stream a datagram belongs to — the per-datagram tag the
/// table above assigns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeenBy {
    /// The wire view only: the transaction layer absorbed it below the TU.
    WireOnly,
    /// Both views: the transaction user sees this datagram.
    Both,
}

/// Which layer owns an inbound datagram — the reason behind its [`SeenBy`] tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// The transaction user's: it surfaces.
    Tu,
    /// A byte-identical repeat the transaction layer absorbs (§17.2).
    TxnDuplicate,
    /// The hop ACK of an INVITE server transaction that answered non-2xx
    /// (§17.1.1.3): the transaction layer's to claim, never the TU's.
    TxnHopAck,
}

/// What [`Absorption::sight`] decided about one inbound datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sighting {
    pub owner: Owner,
    /// Byte-identical to a datagram already sighted under the same key — the
    /// retransmission count's unit, whichever view consumes it.
    pub repeat: bool,
}

impl Sighting {
    pub fn seen_by(&self) -> SeenBy {
        match self.owner {
            Owner::Tu => SeenBy::Both,
            Owner::TxnDuplicate | Owner::TxnHopAck => SeenBy::WireOnly,
        }
    }
}

/// The receive-side transaction layer of one logical endpoint: the §17.2
/// classification, the §17.1.1.3 hop-ACK ledger, and the wire log both views
/// project from.
///
/// Shared across clones of one UA (one transaction table per endpoint), and
/// held directly by a driver that IS the stack (the pivot interpreter).
pub struct Absorption {
    /// Whether the §17.2 dedup applies. Off in the load lane, where
    /// `loadgen::mux` dedups ahead of the agent, and on an agent a test dropped
    /// to the raw wire surface. Hop-ACK ownership is independent of it.
    dedup: AtomicBool,
    /// Whether sighted datagrams are logged. Off in the load lane, where the
    /// volume is unbounded and no consumer reads the views.
    logging: bool,
    /// The shared §17.2 classification ([`sip_net::repeat`] — the same code
    /// the recording decorator stamps arrivals with at event production).
    tables: RepeatTables,
    /// Token source for [`RepeatTables::note`]; this seam never reads the
    /// first-sighting token back, it only asks "is this a repeat".
    sighted: AtomicU64,
    hop_acks: hop_ack::HopAcks,
    log: wire_log::WireLog,
}

impl Absorption {
    /// The functional default: dedup on, both views recorded.
    pub fn transaction_view() -> Self {
        Absorption {
            dedup: AtomicBool::new(true),
            logging: true,
            tables: RepeatTables::default(),
            sighted: AtomicU64::new(0),
            hop_acks: hop_ack::HopAcks::default(),
            log: wire_log::WireLog::default(),
        }
    }

    /// The load-lane default: no dedup (the mux owns it) and no log.
    pub fn raw_wire() -> Self {
        Absorption {
            dedup: AtomicBool::new(false),
            logging: false,
            tables: RepeatTables::default(),
            sighted: AtomicU64::new(0),
            hop_acks: hop_ack::HopAcks::default(),
            log: wire_log::WireLog::default(),
        }
    }

    /// Stop absorbing: every repeat becomes the TU's again. Both views stay
    /// populated — the wire view is how a test reads retransmissions now, so
    /// this is only for a body that must PULL each repeat itself.
    pub fn disable_dedup(&self) {
        self.dedup.store(false, Ordering::Relaxed);
    }

    /// Classify one arriving datagram and record it in the wire log. Called
    /// exactly once per datagram, by the receive core of whichever mode owns
    /// the socket.
    pub fn sight(&self, raw: &[u8], msg: &SipMessage) -> Sighting {
        let repeat = self.dedup.load(Ordering::Relaxed)
            && self.tables.note(raw, msg, self.sighted.fetch_add(1, Ordering::Relaxed)).is_some();
        let owner = if self.hop_ack_claims_msg(msg) {
            Owner::TxnHopAck
        } else if repeat && !repeat_belongs_to_tu(msg) {
            Owner::TxnDuplicate
        } else {
            Owner::Tu
        };
        let sighting = Sighting { owner, repeat };
        if self.logging {
            self.log.push(raw, sighting);
        }
        sighting
    }

    /// Every datagram sighted, in arrival order — what §14 of the pivot spec
    /// records and what retransmission ladders are counted from.
    pub fn wire_view(&self) -> Vec<WireEntry> {
        self.log.entries()
    }

    /// What the transaction user saw: the `SeenBy::Both` subset of
    /// [`wire_view`](Self::wire_view), in the same order.
    pub fn tu_view(&self) -> Vec<WireEntry> {
        self.log.entries().into_iter().filter(|e| e.seen_by() == SeenBy::Both).collect()
    }

    /// Open (or refresh) the §17.1.1.3 obligation for one INVITE server
    /// transaction this endpoint answered non-2xx: its hop ACK is now the
    /// transaction layer's.
    pub fn arm_hop_ack(&self, call_id: String, branch: String) {
        self.hop_acks.arm(call_id, branch);
    }

    /// Whether `r` is the hop ACK of an armed obligation on this endpoint,
    /// marking it fulfilled (idempotent). A receive path that would otherwise
    /// ERROR on an unexpected ACK calls this and absorbs instead.
    pub fn hop_ack_claims(&self, r: &SipRequest) -> bool {
        r.method().as_str() == "ACK"
            && crate::agent::top_via_branch(r)
                .is_some_and(|b| self.hop_acks.note(r.call_id().as_str(), &b))
    }

    pub(crate) fn hop_ack_is_fulfilled(&self, call_id: &str, branch: &str) -> bool {
        self.hop_acks.is_fulfilled(call_id, branch)
    }

    /// Park until the hop ACK for `(call_id, branch)` has been sighted, without
    /// pulling from the inbox. Never times out; callers bound it.
    pub(crate) async fn hop_ack_fulfilled(&self, call_id: &str, branch: &str) {
        self.hop_acks.fulfilled(call_id, branch).await
    }

    fn hop_ack_claims_msg(&self, msg: &SipMessage) -> bool {
        matches!(msg, SipMessage::Request(r) if self.hop_ack_claims(r))
    }
}

#[cfg(test)]
mod tests {
    //! The table at the top of this module, row by row, over the pure
    //! classification. The end-to-end ladders (both views over a real
    //! UA-to-UA exchange) live in [`crate::agent`]'s tests.

    use super::*;
    use sip_message::parser::custom::CustomParser;
    use sip_message::SipParser;

    const INVITE: &str = "INVITE sip:bob@10.0.0.2 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
        From: <sip:alice@10.0.0.1>;tag=a1\r\n\
        To: <sip:bob@10.0.0.2>\r\n\
        Call-ID: c1\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

    const OK: &str = "SIP/2.0 200 OK\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
        From: <sip:alice@10.0.0.1>;tag=a1\r\n\
        To: <sip:bob@10.0.0.2>;tag=b1\r\n\
        Call-ID: c1\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

    const BUSY: &str = "SIP/2.0 486 Busy Here\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
        From: <sip:alice@10.0.0.1>;tag=a1\r\n\
        To: <sip:bob@10.0.0.2>;tag=b1\r\n\
        Call-ID: c1\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

    const HOP_ACK: &str = "ACK sip:bob@10.0.0.2 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
        From: <sip:alice@10.0.0.1>;tag=a1\r\n\
        To: <sip:bob@10.0.0.2>;tag=b1\r\n\
        Call-ID: c1\r\n\
        CSeq: 1 ACK\r\n\
        Content-Length: 0\r\n\r\n";

    fn parse(raw: &str) -> SipMessage {
        CustomParser::new().parse(raw.as_bytes()).expect("the fixture parses")
    }

    fn sight(a: &Absorption, raw: &str) -> Sighting {
        a.sight(raw.as_bytes(), &parse(raw))
    }

    #[test]
    fn a_retransmitted_invite_is_wire_only_and_the_tu_sees_one() {
        let a = Absorption::transaction_view();
        assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::Both);
        assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::WireOnly);
        assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::WireOnly);
        assert_eq!(a.wire_view().len(), 3);
        assert_eq!(a.tu_view().len(), 1);
    }

    #[test]
    fn a_retransmitted_non_2xx_final_and_its_hop_ack_are_wire_only() {
        let a = Absorption::transaction_view();
        assert_eq!(sight(&a, BUSY).seen_by(), SeenBy::Both);
        assert_eq!(sight(&a, BUSY).seen_by(), SeenBy::WireOnly);
        // The ACK is hop-by-hop only once the INVITE server txn armed it.
        a.arm_hop_ack("c1".into(), "z9hG4bK-1".into());
        let ack = sight(&a, HOP_ACK);
        assert_eq!(ack.owner, Owner::TxnHopAck);
        assert_eq!(ack.seen_by(), SeenBy::WireOnly);
        assert_eq!(a.wire_view().len(), 3);
        assert_eq!(a.tu_view().len(), 1, "only the first 486 reached the TU");
    }

    #[test]
    fn a_retransmitted_2xx_to_invite_is_seen_by_both_views() {
        let a = Absorption::transaction_view();
        assert_eq!(sight(&a, OK).seen_by(), SeenBy::Both);
        let again = sight(&a, OK);
        assert!(again.repeat, "it IS a byte-identical repeat");
        assert_eq!(again.seen_by(), SeenBy::Both, "2xx retransmission is end-to-end");
        assert_eq!(a.tu_view().len(), 2);
    }

    #[test]
    fn every_ack_to_a_2xx_is_the_tus_including_a_re_sent_one() {
        let a = Absorption::transaction_view();
        assert_eq!(sight(&a, OK).seen_by(), SeenBy::Both);
        let ack = sight(&a, HOP_ACK);
        assert_eq!(ack.owner, Owner::Tu);
        // The core re-ACKs a retransmitted 2xx with the SAME ACK: a repeat on
        // the wire, and still the TU's — nothing absorbs an ACK to a 2xx.
        let again = sight(&a, HOP_ACK);
        assert!(again.repeat);
        assert_eq!(again.seen_by(), SeenBy::Both);
        assert_eq!(a.tu_view().len(), 3);
    }

    #[test]
    fn a_2xx_and_the_ack_it_draws_key_on_the_same_dialog() {
        let (SipMessage::Response(ok), SipMessage::Request(ack)) = (parse(OK), parse(HOP_ACK))
        else {
            panic!("the fixtures parse as a response and a request")
        };
        let key = ack_key_of_final(&ok).expect("a 2xx to an INVITE with a To-tag");
        assert_eq!(key, ("c1".to_string(), 1, "b1".to_string()));
        assert_eq!(ack_key_of_ack(&ack), Some(key), "one key, read from either end");
        // Nothing else draws a core-sent ACK: not a non-2xx, not another method.
        let SipMessage::Response(busy) = parse(BUSY) else { panic!("a response") };
        assert_eq!(ack_key_of_final(&busy), None);
        let SipMessage::Request(invite) = parse(INVITE) else { panic!("a request") };
        assert_eq!(ack_key_of_ack(&invite), None);
    }

    #[test]
    fn the_key_separates_calls_transactions_and_methods() {
        let a = Absorption::transaction_view();
        assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::Both);
        // A branch reused across two calls is not one transaction.
        let other_call = INVITE.replace("Call-ID: c1", "Call-ID: c2");
        assert_eq!(sight(&a, &other_call).seen_by(), SeenBy::Both);
        // A fresh branch is a new transaction.
        assert_eq!(sight(&a, &INVITE.replace("z9hG4bK-1", "z9hG4bK-2")).seen_by(), SeenBy::Both);
        // A CANCEL sharing the INVITE's branch is its own server transaction.
        let cancel = INVITE
            .replace("INVITE sip:bob@10.0.0.2 SIP/2.0", "CANCEL sip:bob@10.0.0.2 SIP/2.0")
            .replace("CSeq: 1 INVITE", "CSeq: 1 CANCEL");
        assert_eq!(sight(&a, &cancel).seen_by(), SeenBy::Both);
    }

    #[test]
    fn same_key_different_bytes_surfaces_and_then_dedups_on_its_own() {
        let a = Absorption::transaction_view();
        assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::Both);
        let mutated = INVITE.replace("Call-ID: c1", "Call-ID: c1\r\nX-Mutant: yes");
        assert_eq!(sight(&a, &mutated).seen_by(), SeenBy::Both);
        assert_eq!(sight(&a, &mutated).seen_by(), SeenBy::WireOnly);
    }

    #[test]
    fn a_forked_2xx_is_a_real_signal_not_a_repeat() {
        let a = Absorption::transaction_view();
        assert_eq!(sight(&a, OK).seen_by(), SeenBy::Both);
        let fork = sight(&a, &OK.replace(";tag=b1", ";tag=b2"));
        assert!(!fork.repeat);
        assert_eq!(fork.seen_by(), SeenBy::Both);
    }

    #[test]
    fn a_provisional_is_never_deduped_because_ring_again_is_observable() {
        let a = Absorption::transaction_view();
        let ringing = OK.replace("SIP/2.0 200 OK", "SIP/2.0 180 Ringing");
        assert!(!sight(&a, &ringing).repeat);
        assert!(!sight(&a, &ringing).repeat);
        assert_eq!(a.tu_view().len(), 2);
    }

    #[test]
    fn dedup_off_puts_every_repeat_back_in_the_tu_view() {
        let a = Absorption::transaction_view();
        a.disable_dedup();
        assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::Both);
        assert_eq!(sight(&a, INVITE).seen_by(), SeenBy::Both);
        assert_eq!(a.wire_view().len(), a.tu_view().len());
    }

    #[test]
    fn hop_ack_ownership_survives_dedup_being_off() {
        let a = Absorption::transaction_view();
        a.disable_dedup();
        a.arm_hop_ack("c1".into(), "z9hG4bK-1".into());
        assert_eq!(sight(&a, HOP_ACK).owner, Owner::TxnHopAck);
        assert!(a.hop_ack_is_fulfilled("c1", "z9hG4bK-1"));
    }
}
