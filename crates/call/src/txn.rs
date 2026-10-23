//! The names a transaction user and its transaction layer share: which of the
//! two RFC 3261 §17 state machines a transaction runs, and which of its
//! timeouts fired.

/// INVITE vs non-INVITE — the two transaction state machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnKind {
    Invite,
    NonInvite,
}

/// Which client-transaction timeout fired, so a consumer can split the metric.
/// `Response` is the RFC 3261 §17.1 failure-detection timer (Timer B for INVITE,
/// Timer F for non-INVITE — both 64×T1 — or, for an initial INVITE, the TU's
/// first-response deadline): no response of any kind arrived, the hop is dead;
/// `Transaction` is the configured out-of-dialog INVITE bound (the call-setup
/// ring window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnTimeoutKind {
    Response,
    Transaction,
}
