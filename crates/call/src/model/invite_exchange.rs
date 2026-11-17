//! What an INVITE's request record holds beyond any request's
//! ([`InviteExchange`], ADR-0055 §2): the 2xx a sent INVITE took and the ACK
//! that 2xx owes (RFC 3261 §13.2.2.4), the 2xx this side sent on a received
//! INVITE and the ACK it awaits (§13.3.1.4), and a leg's dialog-creating
//! INVITE's outcome.

use serde::{Deserialize, Serialize};

use super::emission::RetainedEmission;
use super::outbound_invite::OutboundInvite;

/// A 2xx to an INVITE this side received that still awaits the ACK for it
/// (RFC 3261 §13.3.1.4): the retained emission its ladder repeats, with the
/// two wire facts the discharging ACK carries — so the ACK is matched on what
/// the 2xx said, not on what the dialog records. While held, the INVITE's
/// server transaction is in the RFC 6026 *Accepted* state, so a new INVITE on
/// either face is glare (§14.1) and gets 491. The ladder is always armed
/// (ADR-0032 X5), so the 2xx is held exactly as long as its give-up: the
/// discharging ACK, the give-up, or the call's end lets it go.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unacked2xx {
    /// The To-tag the 2xx carried — the discharging ACK's To-tag.
    pub dialog_tag: String,
    /// The CSeq of the INVITE the 2xx answers — the discharging ACK's CSeq.
    /// Only an ACK echoing this number discharges the obligation, so a
    /// retransmitted *initial* ACK cannot discharge a re-INVITE's.
    pub cseq: i64,
    /// The 2xx as it left, on the §13.3.1.4 ladder ([`super::Repeat::Paced`]).
    pub emission: RetainedEmission,
}

/// A copy of a taken 2xx owed its re-ACK (RFC 3261 §13.2.2.4): its number
/// among the copies counted ([`InviteExchange::copies_received`]) and the
/// instant it arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReAckedCopy {
    /// The copy's number, counting from 1.
    pub copy: u32,
    /// When the copy arrived, in ms on the turn clock.
    pub at_ms: i64,
}

/// The ACK the 2xx to the INVITE a dialog sent before its last drew, as it
/// left (§13.2.2.4), with the copies of that 2xx counted on that exchange
/// (ADR-0057 §10): a copy arriving once the next INVITE left is answered by
/// this ACK alone, under the same bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckAhead {
    /// The ACK, as the exact datagram that left.
    pub ack: RetainedEmission,
    /// The copies of the 2xx it acknowledges received since that 2xx
    /// (§13.3.1.4), those counted before the next INVITE left included.
    #[serde(default)]
    pub copies_received: u32,
    /// The last of those copies owed its re-ACK.
    #[serde(default)]
    pub re_acked_copy: Option<ReAckedCopy>,
}

impl AckAhead {
    /// The ACK ahead of the INVITE following `x`, the exchange of the INVITE
    /// before it: `ack`, that exchange's ACK, with its copies' count.
    pub fn of(x: &InviteExchange, ack: RetainedEmission) -> Self {
        AckAhead { ack, copies_received: x.copies_received, re_acked_copy: x.re_acked_copy }
    }
}

/// An INVITE's exchange beyond its final. A sent INVITE's 2xx is *taken* once
/// its ACK is owed or has left; until then a final still settles the record.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteExchange {
    /// Sent: the Via branch of the ACK the INVITE's 2xx drew, which every
    /// repeat of that 2xx is re-ACKed on (§13.2.2.4); the first write stands.
    pub ack_branch: Option<String>,
    /// Sent: the ACK the INVITE's 2xx drew, as the exact datagram a repeated
    /// 2xx re-sends (§13.2.2.4): on a delayed offer it carries the answer only
    /// that ACK supplied (RFC 3264 §4). For a round the record never held,
    /// the ACK of its non-2xx too, which no transaction here re-sends
    /// (ADR-0047).
    pub emitted_ack: Option<RetainedEmission>,
    /// Sent: the ACK the 2xx to the INVITE the dialog sent before this one
    /// drew, as it left. A node re-sending this INVITE from the record sends
    /// that ACK first: the peer sees no new INVITE while the one before is
    /// unacknowledged (RFC 3261 §14.1, RFC 6026 §7.1), and a repeated ACK is
    /// absorbed (§13.2.2.4). A copy of that 2xx is re-ACKed with it.
    pub ack_ahead: Option<AckAhead>,
    /// Sent: the copies of the 2xx the INVITE took received since
    /// (§13.3.1.4), each counted before the rules read it. A copy past the
    /// re-sends a compliant answerer makes draws a re-ACK only T1 after the
    /// last one (ADR-0057 §10).
    #[serde(default)]
    pub copies_received: u32,
    /// Sent: the last copy of that 2xx owed its re-ACK (§13.2.2.4).
    #[serde(default)]
    pub re_acked_copy: Option<ReAckedCopy>,
    /// Sent: the CSeq, in the acknowledging peer's own sequence, of the ACK
    /// the INVITE's 2xx awaits from that peer: armed when the 2xx is taken,
    /// discharged by the ACK leaving — the peer's relayed, or one this stack
    /// composed. While armed the dialog is in RFC 6026 *Accepted*.
    pub awaited_ack_cseq: Option<i64>,
    /// Received: the 2xx this side answered the INVITE with, awaiting its ACK.
    pub answered_2xx: Option<Unacked2xx>,
    /// Sent, on a leg's dialling INVITE: what each dialog it creates starts
    /// from and how the INVITE ended (§12.1.2, §9.1).
    pub dialled: Option<OutboundInvite>,
    /// Received, on a leg's dialog-creating INVITE: the status of the final
    /// this stack sent; `None` while unanswered. Every TU final records itself
    /// here and so does the transaction layer's autonomous 487 to a CANCEL; a
    /// leg carrying one is never sent a second final (§17.2.1).
    pub final_sent: Option<u16>,
    /// Received, on a leg's dialog-creating INVITE: the non-2xx final this
    /// stack sent, as it left. A node serving the call from the record sends
    /// it again until her ACK (RFC 3261 §17.2.1): the record may be ahead of
    /// the wire.
    pub rejection: Option<RetainedEmission>,
    /// Received, on a leg's dialog-creating INVITE without a final: the
    /// latest provisional this stack sent on it, as it left, when that one is
    /// not reliable. A node serving the call from the record sends it again
    /// once (RFC 3261 §13.3.1.1): the record may be ahead of the wire. A
    /// reliable one is repeated by its own ladder (RFC 3262 §3).
    #[serde(default)]
    pub provisional: Option<RetainedEmission>,
    /// Received, on a leg's dialog-creating INVITE: the 2xx this stack
    /// recorded that the transaction layer's autonomous 487 (its answer to the
    /// caller's CANCEL) pre-empted before it reached the wire (§9.2).
    pub answer_preempted: Option<u16>,
}

impl InviteExchange {
    /// Whether a 2xx was taken on the sent INVITE: its ACK is owed or left.
    pub fn taken(&self) -> bool {
        self.ack_branch.is_some() || self.awaited_ack_cseq.is_some()
    }
}
