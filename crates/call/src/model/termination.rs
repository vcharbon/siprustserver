//! The termination record: who ended the call and why, written once by the
//! first termination the call enters, and the message-ring cut that separates
//! what the stack sent as part of ending the call from what came after.
//! Write helper: [`crate::helpers::record_termination`].

use serde::{Deserialize, Serialize};

/// Which deadline ended the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutKind {
    /// A call-level completion deadline: the setup deadline, or a transfer's
    /// overall guard.
    Setup,
    /// A leg rang past its answer deadline (RFC 3261 §13.3.1.1: a UAS may
    /// be given a bounded time to answer), a re-INVITE included.
    NoAnswer,
    /// A reliable provisional was never PRACKed (RFC 3262 §3).
    Prack,
    /// A 2xx was never ACKed (RFC 3261 §13.3.1.4).
    Ack,
    /// A leg answered no liveness probe (RFC 3261 §11).
    Keepalive,
    /// A transaction this stack originated drew no final (Timer B / F).
    Transaction,
}

/// The closed set of causes a call ends under: the peer's own request or
/// final, the decision layer's treatment, a deadline, the stack's admission
/// or cap, or its supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationCause {
    /// A peer sent BYE.
    RemoteBye,
    /// The caller CANCELled its INVITE.
    RemoteCancel,
    /// A peer's final ended the call: a callee's failure the decision layer
    /// let stand (no reroute), or a 481 denying the dialog.
    RemoteFinal,
    /// The decision layer refused the call — a reject, a redirect, a relayed
    /// failure it authored, on the initial path or on a failover.
    DecisionReject,
    /// The decision layer's treatment ended the call — a release result, a
    /// reroute it asked for that could not complete, a media program run to
    /// its end.
    DecisionRelease,
    /// The call's duration cap.
    MaxDuration,
    /// A deadline, by kind.
    Timeout(TimeoutKind),
    /// The stack refused the call on its own account: a limiter, the target
    /// admission, a spent hop budget, a malformed INVITE, an unreadable or
    /// unanswered decision.
    Admission,
    /// The per-call message cap.
    MessageCap,
    /// The supervisor ended it: a reaper strike, a forced terminal, an
    /// operator.
    Supervisor,
}

/// Who ended the call and why. `at_ms` is the clock of the turn that began
/// the termination; `by_leg` names the leg whose message or timer caused
/// it — the caller for its BYE or CANCEL, the callee for its BYE or final or
/// unanswered probe — and is `None` for a cause of the decision layer's, the
/// stack's or its supervisor's. `last_seq` is the `seq` of the last
/// message-ring entry the terminating turn recorded: every entry with
/// `seq <= last_seq` was received or sent as part of beginning the
/// termination (the peer's BYE and its 200, the BYE or CANCEL relayed to the
/// other leg, the caller's 487), every later entry — the other leg's 200 to
/// that BYE, the ACK to the 487 — came after. `0` while the ring is off.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Termination {
    pub at_ms: i64,
    pub cause: TerminationCause,
    pub by_leg: Option<String>,
    pub last_seq: u32,
}
