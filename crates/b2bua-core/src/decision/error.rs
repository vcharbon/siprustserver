//! [`CallDecisionError`]: why a decision consult gave no decision.

use b2bua_sdk::header_update::SipHeaderUpdates;

#[derive(Debug, thiserror::Error)]
pub enum CallDecisionError {
    #[error("decision backend unavailable: {0}")]
    Unavailable(String),
    /// The engine read no decision and refuses the call with a final it
    /// states. The initial and failover paths answer it exactly as a reject
    /// decision (same code, reason policy and headers) and mark no decision:
    /// the final is the stack's own. Any other consult treats it as
    /// [`CallDecisionError::Unavailable`], and the decision deadline counts it
    /// in the backend's unavailability episode.
    #[error("decision refused with {code}")]
    Refused { code: u16, reason: Option<String>, update_headers: Option<SipHeaderUpdates> },
}
