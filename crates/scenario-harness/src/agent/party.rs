//! The side of the system under test a simulated party stands on, and the
//! first CSeq it numbers its requests from.

/// The side of the system under test a simulated party stands on. Each side
/// starts its own CSeq sequence at a distinct fixed value, so a trace that
/// mixes two sequence spaces shows whose number is whose; the system under
/// test starts its own at 1 (RFC 3261 §8.1.1.5 allows any value below 2³¹).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Party {
    /// Sends the call's INVITE, or any request outside a dialog.
    Caller,
    /// Answers an INVITE.
    #[default]
    Callee,
    /// Answers the INVITE a transfer sends it.
    TransferTarget,
}

impl Party {
    /// The CSeq of this party's first request in a dialog or outside one.
    pub const fn first_cseq(self) -> u32 {
        match self {
            Party::Caller => 1001,
            Party::Callee => 2001,
            Party::TransferTarget => 3001,
        }
    }
}
