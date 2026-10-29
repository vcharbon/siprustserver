//! RFC 3261 §17.2 absorption — THE one classification in this tree — and the
//! two views of the datagram stream it separates.
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
//! | non-INVITE request of a transaction this endpoint answered, arriving after the final (§17.2.3 match) | seen | absorbed — its final is repeated (§17.2.2) |
//! | copy of an INVITE this endpoint answered (§17.2.3 match) | seen | absorbed — its latest provisional or non-2xx final is repeated (§17.2.1) |
//! | INVITE merged with one received: its Call-ID, From-tag and CSeq, another transaction | seen | absorbed — answered 482 by the UAS core (§8.2.2.2) |
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
//! repeat iff it is byte-identical to one already sighted under the same key,
//! or a request of the server transaction its top-Via branch, sent-by and
//! method name (§17.2.3), whatever a proxy stamped on the copy, once that
//! transaction answered: an INVITE after any response, a non-INVITE after its
//! final.
//! There is no absorb-set, no window and no method-name tolerance list: those
//! over-approximate "a repeat of what I already saw" into "any message of this
//! method", which can swallow a genuine signal.

mod abandoned;
mod answered;
mod hop_ack;
mod invited;
mod received_acks;
mod received_provisionals;
mod rejected_acks;
mod two_xx_ack;
mod unacked_finals;
mod unanswered;
mod wire_log;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sip_net_testkit::repeat::{repeat_belongs_to_tu, RepeatTables};

use std::net::SocketAddr;

use sip_message::{SipMessage, SipRequest, SipResponse};

pub use two_xx_ack::{ack_key_of_ack, ack_key_of_final, AckKey};
pub(crate) use two_xx_ack::{Owed, TwoXxAcks};
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
    /// A request of a server transaction this endpoint answered: the
    /// transaction layer repeats the response a copy draws (§17.2.1,
    /// §17.2.2), or the UAS core answers a merged INVITE 482 (§8.2.2.2).
    TxnCompleted,
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
            Owner::TxnDuplicate | Owner::TxnCompleted | Owner::TxnHopAck => SeenBy::WireOnly,
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
    /// The shared §17.2 classification ([`sip_net_testkit::repeat`] — the same code
    /// the recording decorator stamps arrivals with at event production).
    tables: RepeatTables,
    /// Token source for [`RepeatTables::note`]; this seam never reads the
    /// first-sighting token back, it only asks "is this a repeat".
    sighted: AtomicU64,
    hop_acks: hop_ack::HopAcks,
    /// The non-INVITE server transactions this endpoint holds, with the final
    /// each completed one repeats (§17.2.2).
    answered: answered::Answered,
    /// The INVITE server transactions this endpoint holds, with the response a
    /// copy of each draws (§17.2.1).
    invited: invited::Invited,
    /// The finals to an INVITE awaiting their ACK (§13.3.1.4, §17.2.1).
    unacked_finals: unacked_finals::UnackedFinals,
    /// The requests awaiting their answer (§17.1.1.2, §17.1.2.2).
    unanswered: unanswered::Unanswered,
    /// Whether the ladders above are driven: off for an endpoint that plays a
    /// peer which never retransmits.
    retransmits: AtomicBool,
    /// The finals a [`Owner::TxnCompleted`] sighting owes, not yet sent.
    repeats: std::sync::Mutex<Vec<(Vec<u8>, SocketAddr)>>,
    /// The INVITE client transactions whose caller gave up (§9.1) and that
    /// still ACK their non-2xx final (§17.1.1.3).
    abandoned: abandoned::AbandonedInvites,
    /// The hop ACK each non-2xx final to this endpoint's INVITE drew, which a
    /// repeat of that final draws again (§17.1.1.2).
    rejected_acks: rejected_acks::RejectedAcks,
    /// The ACKs this endpoint received: a repeat is the UAS core's to discard
    /// (§13.3.1.4).
    received_acks: received_acks::ReceivedAcks,
    /// The provisionals this endpoint received: a byte-identical repeat tells
    /// the UAC core nothing new (§13.2.2.1).
    received_provisionals: received_provisionals::ReceivedProvisionals,
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
            answered: answered::Answered::default(),
            invited: invited::Invited::default(),
            unacked_finals: Default::default(),
            unanswered: Default::default(),
            retransmits: AtomicBool::new(true),
            repeats: Default::default(),
            abandoned: abandoned::AbandonedInvites::default(),
            rejected_acks: rejected_acks::RejectedAcks::default(),
            received_acks: received_acks::ReceivedAcks::default(),
            received_provisionals: Default::default(),
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
            answered: answered::Answered::default(),
            invited: invited::Invited::default(),
            unacked_finals: Default::default(),
            unanswered: Default::default(),
            retransmits: AtomicBool::new(true),
            repeats: Default::default(),
            abandoned: abandoned::AbandonedInvites::default(),
            rejected_acks: rejected_acks::RejectedAcks::default(),
            received_acks: received_acks::ReceivedAcks::default(),
            received_provisionals: Default::default(),
            log: wire_log::WireLog::default(),
        }
    }

    /// Stop absorbing: every repeat becomes the TU's again. Both views stay
    /// populated — the wire view is how a test reads retransmissions now, so
    /// this is only for a body that must PULL each repeat itself.
    pub fn disable_dedup(&self) {
        self.dedup.store(false, Ordering::Relaxed);
    }

    /// Classify one datagram that arrived at `arrived` and record it in the
    /// wire log. Called exactly once per datagram, by the receive core of
    /// whichever mode owns the socket; a server transaction's timers judge it
    /// at its arrival, however late it is read.
    pub fn sight(&self, raw: &[u8], msg: &SipMessage, arrived: tokio::time::Instant) -> Sighting {
        let dedup = self.dedup.load(Ordering::Relaxed);
        let ordinal = self.sighted.fetch_add(1, Ordering::Relaxed);
        match msg {
            SipMessage::Request(r) => {
                self.unacked_finals.sight(r);
                if dedup {
                    self.received_acks.sight(r);
                }
            }
            SipMessage::Response(r) => {
                self.unanswered.sight(r);
                if dedup {
                    self.received_provisionals.sight(raw, r);
                }
            }
        }
        let identical = dedup && self.tables.note(raw, msg, ordinal).is_some();
        let copy = match msg {
            SipMessage::Request(r) if dedup => self.answered.sight(r, ordinal, arrived),
            _ => None,
        };
        let invite = match msg {
            SipMessage::Request(r) if dedup => self.invited.sight(r, ordinal),
            _ => None,
        };
        let merged = matches!(invite, Some(invited::Copy::Merged(..)));
        let repeat = identical || copy.is_some() || (invite.is_some() && !merged);
        let owner = if self.hop_ack_claims_msg(msg) {
            Owner::TxnHopAck
        } else if let Some(answered::Copy::Repeat(wire, dst)) = copy {
            self.repeats.lock().unwrap().push((wire, dst));
            Owner::TxnCompleted
        } else if let Some(invited::Copy::Repeat(wire, dst) | invited::Copy::Merged(wire, dst)) =
            invite
        {
            self.repeats.lock().unwrap().push((wire, dst));
            Owner::TxnCompleted
        } else if copy.is_some() || invite.is_some() || (repeat && !repeat_belongs_to_tu(msg)) {
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

    /// Note a datagram this endpoint sent, `msg` rendered as `wire` toward
    /// `dst`, while `queued` datagrams wait unsighted on its socket: a final
    /// to a non-INVITE request completes its server transaction (§17.2.2),
    /// for the datagrams that arrive after those; a final to an INVITE awaits
    /// its ACK, a request other than ACK its answer. Nothing is noted while
    /// dedup is off.
    pub fn note_sent(&self, msg: &SipMessage, wire: &[u8], dst: SocketAddr, queued: usize) {
        if !self.dedup.load(Ordering::Relaxed) {
            return;
        }
        match msg {
            SipMessage::Response(r) => {
                let after = self.sighted.load(Ordering::Relaxed) + queued as u64;
                self.answered.note_sent(r, wire, dst, after);
                self.invited.note_sent(r, wire, dst, after);
                self.unacked_finals.note_sent(r, wire, dst);
            }
            SipMessage::Request(r) => {
                self.unanswered.note_sent(r, wire, dst);
                self.rejected_acks.note_sent(r, wire, dst);
                self.unacked_finals.note_request_sent(r);
            }
        }
    }

    /// Never send a datagram again for want of its answer or its ACK: the
    /// endpoint plays a peer that does not retransmit.
    pub fn silence_retransmissions(&self) {
        self.retransmits.store(false, Ordering::Relaxed);
    }

    /// When a datagram this endpoint sent is due again: a final to an INVITE
    /// awaiting its ACK (§13.3.1.4, §17.2.1), or a request awaiting its
    /// answer (§17.1.1.2, §17.1.2.2).
    pub fn next_due(&self) -> Option<tokio::time::Instant> {
        if !self.retransmits.load(Ordering::Relaxed) {
            return None;
        }
        self.unacked_finals.next_due().into_iter().chain(self.unanswered.next_due()).min()
    }

    /// The datagrams due again at `now`, each with its destination.
    pub fn take_due(&self, now: tokio::time::Instant) -> Vec<(Vec<u8>, SocketAddr)> {
        if !self.retransmits.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let mut due = self.unacked_finals.take_due(now);
        due.extend(self.unanswered.take_due(now));
        due
    }

    /// The finals to an INVITE due again at `now` for want of their ACK, each
    /// with its destination, and whether one still awaits its ACK.
    pub fn take_due_finals(&self, now: tokio::time::Instant) -> (Vec<(Vec<u8>, SocketAddr)>, bool) {
        if !self.retransmits.load(Ordering::Relaxed) {
            return (Vec::new(), false);
        }
        (self.unacked_finals.take_due(now), self.unacked_finals.awaits_an_ack(now))
    }

    /// The finals the [`Owner::TxnCompleted`] sightings since the last call
    /// owe, each with its destination, oldest first (§17.2.2).
    pub fn take_repeats(&self) -> Vec<(Vec<u8>, SocketAddr)> {
        std::mem::take(&mut *self.repeats.lock().unwrap())
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

    /// Whether `r` repeats an ACK this endpoint already received: the UAS
    /// core discards it (§13.3.1.4).
    pub fn repeats_an_ack(&self, r: &SipRequest) -> bool {
        self.received_acks.repeats(r)
    }

    /// Whether `r` is a provisional byte-identical to one this endpoint
    /// already received: the UAC core learns nothing new from it (§13.2.2.1).
    pub fn repeats_a_provisional(&self, r: &SipResponse) -> bool {
        self.received_provisionals.repeats(r)
    }

    pub(crate) fn hop_ack_is_fulfilled(&self, call_id: &str, branch: &str) -> bool {
        self.hop_acks.is_fulfilled(call_id, branch)
    }

    /// Park until the hop ACK for `(call_id, branch)` has been sighted, without
    /// pulling from the inbox. Never times out; callers bound it.
    pub(crate) async fn hop_ack_fulfilled(&self, call_id: &str, branch: &str) {
        self.hop_acks.fulfilled(call_id, branch).await
    }

    /// Keep the client transaction of an INVITE its caller gave up on, sent to
    /// `wire_dst`: its non-2xx final is still ACKed (§17.1.1.3).
    pub fn abandon_invite(&self, invite: SipRequest, wire_dst: SocketAddr) {
        self.abandoned.abandon(invite, wire_dst);
    }

    /// The hop ACK, and its destination, a non-2xx final to an abandoned
    /// INVITE draws.
    pub(crate) fn abandoned_ack_for(&self, resp: &SipResponse) -> Option<(SipRequest, SocketAddr)> {
        self.abandoned.ack_for(resp)
    }

    /// The hop ACK, as it left and where, that a repeat of the non-2xx final
    /// `resp` to this endpoint's INVITE draws again (§17.1.1.2).
    pub(crate) fn rejected_ack_for(&self, resp: &SipResponse) -> Option<(Vec<u8>, SocketAddr)> {
        self.rejected_acks.ack_for(resp)
    }

    fn hop_ack_claims_msg(&self, msg: &SipMessage) -> bool {
        matches!(msg, SipMessage::Request(r) if self.hop_ack_claims(r))
    }
}

#[cfg(test)]
mod tests;
