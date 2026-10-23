//! What a [`TerminateLeg`](crate::model::RuleAction::TerminateLeg) does to
//! end its leg ([`LegEnd`]). The leg's state is not written: it reads off the
//! dialog and INVITE facts each case records.

use serde::{Deserialize, Serialize};

/// How a `TerminateLeg` ends its leg, named after what the action records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LegEnd {
    /// The peer's BYE, answered this turn, ends the dialog it names, else
    /// the leg's confirmed one (RFC 3261 §15.1.2).
    PeerByeAnswered,
    /// The BYE this turn sent on the leg leaves its confirmed dialog ending
    /// until that BYE's final (§15.1.1).
    OurByeSent,
    /// The final this turn's event carries to this stack's BYE ends the
    /// dialog that BYE was ending.
    OurByeAnswered,
    /// This stack's BYE timed out: the dialog it was ending ends without a
    /// final (§15.1.1).
    OurByeTimedOut,
    /// The non-2xx final this turn's event carries to the leg's INVITE ends
    /// it and the early dialogs it opened (§12.1); without such an event the
    /// leg is abandoned.
    InviteRejected,
    /// The stack gives up whatever the leg still awaits: an INVITE without a
    /// final takes a 408 (§8.1.3.1), a confirmed dialog ends without a BYE
    /// final.
    Abandon,
    /// The leg has already ended: only its ladders and the requests still
    /// pending toward it are settled.
    Settle,
}
