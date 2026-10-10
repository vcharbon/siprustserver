//! [`Attempt`]: one routing answer the call dials and the destination legs it
//! dials, with why it opened ([`AttemptCause`]) and how it ended
//! ([`AttemptOutcome`]). The writers are in [`crate::helpers::attempt`].

use serde::{Deserialize, Serialize};

use super::deadline::Deadline;

/// An attempt's number on its call: `0` for the first, one more for each
/// attempt opened after it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttemptId(pub u32);

impl std::fmt::Debug for AttemptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "attempt {}", self.0)
    }
}

/// Why an attempt opened: which routing answer it dials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptCause {
    /// The call's initial routing answer, dialled by the core or by the
    /// service its routing deferred the dial to.
    Route,
    /// A failover answer during setup, after an earlier attempt failed.
    Failover,
    /// A routing answer that replaces the callee of an established call.
    Reroute,
    /// The target of a transfer.
    Transfer,
}

/// How an attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// One of its legs answered.
    Answered,
    /// Every leg it dialled ended unanswered.
    Failed,
    /// The next attempt opened while one of its legs was still being
    /// cancelled.
    Superseded,
    /// The call's termination began while it was open.
    CallEnded,
    /// Admission refused its route: it dialled nothing.
    Refused,
}

/// One routing answer the call dials and the legs it dials. Open while
/// `outcome` is `None`; an ended attempt never reopens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub id: AttemptId,
    pub cause: AttemptCause,
    /// The ordinal of the decision current when the attempt opened
    /// ([`crate::Call::decision_ordinal`]).
    pub decision_ordinal: u32,
    /// The legs the attempt dialled, in creation order.
    pub legs: Vec<String>,
    /// The ring deadline its routing answer states for the legs it dials.
    pub no_answer: Deadline,
    pub outcome: Option<AttemptOutcome>,
}

impl Attempt {
    /// Whether the attempt has not ended.
    pub fn is_open(&self) -> bool {
        self.outcome.is_none()
    }
}
