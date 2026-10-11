//! One reliable provisional (RFC 3262) as the call keeps it: the responder's
//! identity of it, whether this stack owes the responder its PRACK, and where
//! it was shown under this stack's own number. The engine records it as it
//! arrives, before any rule reads it; the books over the list live in
//! [`crate::helpers::reliable`].

use serde::{Deserialize, Serialize};

use super::emission::RetainedEmission;

/// An `RSeq` value (RFC 3262 §7.1): a provisional's place in its dialog's
/// reliable sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RSeq(pub u32);

/// A `CSeq` number (RFC 3261 §20.16): here, the INVITE a provisional answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CSeq(pub u32);

impl RSeq {
    /// The next number in a sequence (RFC 3262 §3: each reliable provisional
    /// is one higher than the last).
    pub fn next(self) -> RSeq {
        RSeq(self.0.wrapping_add(1))
    }
}

impl CSeq {
    /// The number as the call's dialog books spell a CSeq.
    pub fn as_i64(self) -> i64 {
        i64::from(self.0)
    }

    /// A CSeq the dialog books spell as `i64`; a value outside the RFC 3261
    /// §8.1.1.5 range reads as 0.
    pub fn of_i64(cseq: i64) -> CSeq {
        CSeq(u32::try_from(cseq).unwrap_or(0))
    }
}

/// How the responder identifies a reliable provisional (RFC 3262 §3, §7.1):
/// its leg, its tag on that leg's dialog, the INVITE it answers and the `RSeq`
/// it stated. Every part earns its place: each fork numbers independently
/// (§3, errata 4600), so only the tag tells two forks' equal `RSeq`s apart,
/// and the sequence restarts per INVITE transaction, so only the `CSeq` tells
/// a re-INVITE's first provisional from the initial INVITE's.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProvisionalId {
    pub leg: String,
    pub responder_tag: String,
    pub invite_cseq: CSeq,
    pub rseq: RSeq,
}

/// Where the PRACK the responder is owed stands (RFC 3262 §4). Each
/// provisional is PRACKed at most once toward its responder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponderAck {
    /// No PRACK has left yet and one is owed.
    Pending,
    /// The PRACK of the party it was shown to was relayed.
    RelayedPrack,
    /// This stack PRACKed it on its own account, on the client transaction
    /// `branch`.
    OwnPrack { branch: String },
    /// Its offer was left to the ACK a 2xx awaits; the PRACK carrying that
    /// ACK's answer leaves with it (RFC 3262 §5, RFC 3264).
    DeferredToAck,
    /// The responder's dialog ended before a PRACK could ride it (RFC 3261
    /// §15.1.2): nothing is owed on the wire.
    Waived,
}

impl ResponderAck {
    /// Whether a PRACK is still owed: pending, or waiting for the ACK.
    pub fn is_owed(&self) -> bool {
        matches!(self, ResponderAck::Pending | ResponderAck::DeferredToAck)
    }
}

/// Why a shown provisional's retransmissions stopped short of its PRACK.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ceased {
    /// The INVITE's final, or the end of the leg, transaction or call it
    /// belonged to, reached the face it was shown (RFC 3262 §3, RFC 3261
    /// §17.2.1).
    Final,
    /// The ladder reached its 64·T1 bound (RFC 3262 §3).
    GiveUp,
}

/// The RFC 3262 §3 ladder of a shown provisional: this stack's
/// retransmissions of it on the face it was shown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ladder {
    /// Numbered, not yet on the wire.
    Unsent,
    /// On the wire and repeated until its PRACK arrives.
    Repeating(RetainedEmission),
    /// The party it was shown to PRACKed it.
    Acknowledged,
    /// The repeats stopped without a PRACK.
    Ceased(Ceased),
}

/// A reliable provisional as it was shown, under this stack's own number, to
/// the party its INVITE came from: this stack is the UAS of that face, so the
/// number is its own (RFC 3262 §3), one sequence per shown dialog (§4,
/// errata 4603).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shown {
    /// This stack's tag on the dialog it was shown in.
    pub tag: String,
    /// The `RSeq` shown there.
    pub rseq: RSeq,
    /// The INVITE it answers on that face — the second `RAck` token (§7.2).
    pub invite_cseq: CSeq,
    /// It carried a description toward that face (RFC 3311 §5.1).
    pub carried_sdp: bool,
    /// Its retransmissions there, and whether its PRACK arrived.
    pub ladder: Ladder,
}

/// One reliable provisional the call received, from the turn it arrived for
/// the life of the call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provisional {
    pub id: ProvisionalId,
    /// The responder's provisional carried a description.
    pub responder_sdp: bool,
    /// The PRACK owed toward the responder.
    pub responder: ResponderAck,
    /// Where it was shown; `None` while shown to no one.
    pub shown: Option<Shown>,
}

impl Provisional {
    /// Whether the party it was shown to acknowledged it.
    pub fn acknowledged_by_shown(&self) -> bool {
        self.shown.as_ref().is_some_and(|s| s.ladder == Ladder::Acknowledged)
    }
}
