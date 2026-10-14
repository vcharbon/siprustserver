//! Why a leg's stack could not compose a message.

/// Why the stack could not compose a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackError {
    /// An in-dialog request on a leg with no dialog yet.
    NoDialog { leg: String, method: String },
    /// An in-dialog request on a dialog that holds no remote target: no
    /// response that opened it carried a Contact (RFC 3261 §12.1.2), so
    /// there is no Request-URI to address it to.
    NoRemoteTarget { leg: String, method: String },
    /// A response with no request to answer.
    NoRequest { leg: String, status: u16 },
    /// An ACK or CANCEL with no INVITE to scope it to.
    NoInvite { leg: String, method: String },
    /// A method this stack has no recipe for.
    UnsupportedMethod { leg: String, method: String },
    /// A reliable provisional composed without the `RSeq` RFC 3262 §3 requires.
    NoRSeq { leg: String, status: u16 },
    /// A PRACK on a leg that has received no reliable provisional to
    /// acknowledge: RFC 3262 §7.2 builds `RAck` out of one, and there is none.
    NoReliableProvisional { leg: String },
    /// An early-dialog request on a leg ringing SEVERAL forks, naming none of
    /// them. Picking one would be the inference §14 forbids.
    EarlyDialogAmbiguous { leg: String, method: String, dialogs: usize },
    /// An early-dialog request naming a fork this leg has not rung.
    EarlyDialogUnknown { leg: String, method: String, early: String },
    /// A `cseq-override` (§11) on the ACK to a non-2xx final. That ACK is
    /// composed by the INVITE client transaction it rides (RFC 3261 §17.1.1.3),
    /// so its number is not the core's to state.
    CseqOverrideOnAbsorbedAck { leg: String },
}

impl std::fmt::Display for StackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StackError::NoDialog { leg, method } => {
                write!(f, "leg {leg}: {method} is in-dialog and the leg has no dialog yet")
            }
            StackError::NoRemoteTarget { leg, method } => write!(
                f,
                "leg {leg}: {method} rides a dialog with no remote target: no response that \
                 opened it carried a Contact"
            ),
            StackError::NoRequest { leg, status } => {
                write!(f, "leg {leg}: {status} answers a request the leg never received")
            }
            StackError::NoInvite { leg, method } => {
                write!(f, "leg {leg}: {method} needs an INVITE transaction and there is none")
            }
            StackError::UnsupportedMethod { leg, method } => {
                write!(f, "leg {leg}: this stack has no recipe for {method}")
            }
            StackError::NoRSeq { leg, status } => write!(
                f,
                "leg {leg}: the {status} requires 100rel and states no RSeq, and none is invented"
            ),
            StackError::NoReliableProvisional { leg } => write!(
                f,
                "leg {leg}: PRACK acknowledges a reliable provisional and this leg received none"
            ),
            StackError::EarlyDialogAmbiguous { leg, method, dialogs } => write!(
                f,
                "leg {leg}: {method} rides an early dialog and the leg has {dialogs} of them; \
                 state which with `early`"
            ),
            StackError::EarlyDialogUnknown { leg, method, early } => write!(
                f,
                "leg {leg}: {method} rides early dialog {early:?}, which this leg has not rung"
            ),
            StackError::CseqOverrideOnAbsorbedAck { leg } => write!(
                f,
                "leg {leg}: the ACK to a non-2xx final is composed by the INVITE client \
                 transaction and its CSeq is not the core's to override"
            ),
        }
    }
}
