//! The decision log: one [`DecisionMark`] per decision the decision layer
//! returned AND this stack applied to the call, in application order. Every
//! message-ring entry and every CDR event carries the count of marks at the
//! instant it was written (`decision_ordinal`), so the decision a message was
//! handled under is read as `decision_log[ordinal - 1]` whatever a later
//! decision replaced on the call. Append helper:
//! [`crate::helpers::mark_decision`].
//!
//! A route the limiter, the hop budget or the target admission refused, an
//! unanswered consult (engine error, deadline), a limiter-refused reroute and
//! every final the stack authors on its own mark nothing.

use serde::{Deserialize, Serialize};

/// The closed set of decision families this stack applies: the decision
/// point crossed with the treatment it returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// The initial INVITE's decision: bridge to a destination.
    Route,
    /// The initial INVITE's decision: a final failure this stack authors.
    Reject,
    /// The initial INVITE's decision: a 3xx with a Contact list.
    Redirect,
    /// The initial INVITE's decision: relay a failure — with none captured
    /// yet, the 480 fallback.
    Relay,
    /// A failed leg's decision: dial a replacement leg.
    FailoverRoute,
    /// A failed leg's decision: author a final failure toward the caller.
    FailoverReject,
    /// A failed leg's decision: a 3xx with a Contact list toward the caller.
    FailoverRedirect,
    /// A failed leg's decision: relay its failure and end the call — also
    /// what an unanswered consult resolves to.
    FailoverTerminate,
    /// A subscribed release event's decision: the local teardown.
    Release,
    /// A subscribed release event's decision: reroute the established call.
    ReleaseRoute,
    /// A REFER's decision: authorized, the target is dialed.
    TransferAllow,
    /// A REFER's decision: denied — also what an unanswered consult resolves
    /// to.
    TransferReject,
}

/// One applied decision. `ordinal` is its 1-based position in the log — the
/// value stamped on everything written under it; `at_ms` is the clock of the
/// turn that applied it; `leg_id` names the leg whose event the decision
/// answers: `a` for the initial INVITE, the leg that failed for a failover,
/// the referring leg for a transfer, `None` for a call-scoped release event
/// and for a failover a limiter refusal raised (no leg failed); `label` is
/// the opaque string the decision layer attached, recorded for the call's
/// record and read by nothing here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionMark {
    pub ordinal: u32,
    pub at_ms: i64,
    pub kind: DecisionKind,
    pub leg_id: Option<String>,
    pub label: Option<String>,
}
