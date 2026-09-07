//! What discharges a dialog-level retransmission ladder (ADR-0029 X4): the
//! key the framework matches an inbound ACK or PRACK against. A ladder is
//! armed under one of these, its rung and give-up timers carry it, and the
//! engine retires all three when the discharging request arrives — a rule
//! sees only the give-up.

use serde::{Deserialize, Serialize};

/// The obligation a dialog-level ladder repeats its retained emission under.
/// Rides in [`super::TimerType`], so it is replicated with the call, and its
/// `Debug` form is a segment of the persisted timer id: the RFC 3261 §25.1
/// `token` grammar of a tag and the digits of a sequence number exclude `:`,
/// the id-recipe separator, whoever minted them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Obligation {
    /// RFC 3261 §13.3.1.4 — a 2xx to an INVITE (the initial one or a
    /// re-INVITE alike) sent on `leg`'s dialog under `dialog_tag`, discharged
    /// by an ACK on that dialog whose CSeq is `cseq`.
    AckOf2xx { leg: String, dialog_tag: String, cseq: i64 },
    /// RFC 3262 §3 — the reliable provisional shown as `a_rseq` in the `a_tag`
    /// dialog, discharged by a PRACK whose `RAck` names it.
    PrackOf { a_tag: String, a_rseq: i64 },
}

impl Obligation {
    /// The kind of obligation, as a counter labels it (`obligation="…"`).
    pub fn kind(&self) -> &'static str {
        match self {
            Obligation::AckOf2xx { .. } => "ack-of-2xx",
            Obligation::PrackOf { .. } => "prack-of",
        }
    }

    /// The leg the discharging request arrives on, where the key states one
    /// (`PrackOf` names a dialog tag instead; `helpers::leg_shown` resolves it).
    pub fn leg(&self) -> Option<&str> {
        match self {
            Obligation::AckOf2xx { leg, .. } => Some(leg),
            Obligation::PrackOf { .. } => None,
        }
    }
}

impl std::fmt::Debug for Obligation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Obligation::AckOf2xx { leg, dialog_tag, cseq } => {
                write!(f, "AckOf2xx:{leg}:{dialog_tag}:{cseq}")
            }
            Obligation::PrackOf { a_tag, a_rseq } => write!(f, "PrackOf:{a_tag}:{a_rseq}"),
        }
    }
}
