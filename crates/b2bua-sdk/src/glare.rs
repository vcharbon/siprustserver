//! How a request that meets glare is refused ([`GlareRefusal`]).

/// How an INVITE that meets glare, or an UPDATE over an open offer, is
/// refused (RFC 3261 §14.2, RFC 3311 §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlareRefusal {
    /// The sender's earlier INVITE or UPDATE is still open here: 500 with a
    /// `Retry-After` of 0 to 10 s (RFC 3261 §14.2, first clause; RFC 3311
    /// §5.2).
    Unanswered,
    /// The sender's earlier INVITE has its 2xx, not yet ACKed: 491, with a
    /// `Retry-After` from [`crate::config::B2buaConfig::glare_retry_after`]
    /// when one is set.
    Unacknowledged,
    /// An INVITE or offer of this side's own is in progress on the dialog, or
    /// on the dialog the request would be relayed onto: 491 with no
    /// `Retry-After` (§14.2, second clause; RFC 3311 §5.2).
    Crossing,
}
