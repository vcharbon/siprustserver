//! What discharges a dialog-level retransmission ladder (ADR-0032 X4): the
//! key the framework matches an inbound ACK or PRACK against. A ladder is
//! armed under one of these, its rung and give-up timers carry it, and the
//! engine retires all three when the discharging request arrives — a rule
//! sees only the give-up.

use serde::{Deserialize, Serialize};

use super::provisional::RSeq;

/// The obligation a dialog-level ladder repeats its retained emission under.
/// The scope of the ladder's timers ([`super::TimerScope::Obligation`]), so it
/// is replicated with the call.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Obligation {
    /// RFC 3261 §13.3.1.4 — a 2xx to an INVITE (the initial one or a
    /// re-INVITE alike) sent on `leg`'s dialog under `dialog_tag`, discharged
    /// by an ACK on that dialog whose CSeq is `cseq`.
    AckOf2xx { leg: String, dialog_tag: String, cseq: i64 },
    /// RFC 3262 §3 — the reliable provisional this stack showed as `rseq` in
    /// its `tag` dialog awaits the PRACK whose `RAck` names it.
    AwaitPrack { tag: String, rseq: RSeq },
}

/// The kind of an [`Obligation`], whatever its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObligationKind {
    AckOf2xx,
    AwaitPrack,
}

impl ObligationKind {
    /// Every kind, in declaration order.
    pub const ALL: [ObligationKind; 2] = [ObligationKind::AckOf2xx, ObligationKind::AwaitPrack];

    /// The kind as a counter labels it (`obligation="…"`).
    pub const fn label(self) -> &'static str {
        match self {
            ObligationKind::AckOf2xx => "ack-of-2xx",
            ObligationKind::AwaitPrack => "await-prack",
        }
    }
}

impl Obligation {
    /// The kind of obligation, as a counter labels it (`obligation="…"`).
    pub fn kind(&self) -> &'static str {
        self.kind_of().label()
    }

    /// The kind of obligation.
    pub fn kind_of(&self) -> ObligationKind {
        match self {
            Obligation::AckOf2xx { .. } => ObligationKind::AckOf2xx,
            Obligation::AwaitPrack { .. } => ObligationKind::AwaitPrack,
        }
    }

    /// The leg the discharging request arrives on, where the key states one
    /// (`AwaitPrack` names a dialog tag instead; `helpers::leg_shown` resolves it).
    pub fn leg(&self) -> Option<&str> {
        match self {
            Obligation::AckOf2xx { leg, .. } => Some(leg),
            Obligation::AwaitPrack { .. } => None,
        }
    }
}

impl std::fmt::Debug for Obligation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Obligation::AckOf2xx { leg, dialog_tag, cseq } => {
                write!(f, "AckOf2xx:{leg}:{dialog_tag}:{cseq}")
            }
            Obligation::AwaitPrack { tag, rseq } => write!(f, "AwaitPrack:{tag}:{}", rseq.0),
        }
    }
}
