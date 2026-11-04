//! A received non-INVITE request this stack answered with a final, kept while
//! its server transaction is Completed (RFC 3261 §17.2.2): a copy of the
//! request draws that final again until Timer J, whichever node serves the
//! call, so a copy that arrives after the final was decided is never taken
//! for a new request.

use serde::{Deserialize, Serialize};
use sip_retransmit::timers::TIMER_J;

use super::emission::RetainedEmission;

/// One answered non-INVITE request and its final, as it left.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnsweredRequest {
    /// The request's method, part of its server transaction's key
    /// (RFC 3261 §17.2.3).
    pub method: String,
    /// The request's top-Via branch: the server transaction's identity.
    pub branch: String,
    /// The final, byte for byte, and where it went.
    pub response: RetainedEmission,
    /// When the server transaction ends: Timer J from the final.
    pub until_ms: i64,
}

impl AnsweredRequest {
    /// How long a server non-INVITE transaction stays Completed over UDP:
    /// Timer J, 64·T1 (RFC 3261 §17.2.2).
    #[expect(clippy::cast_possible_wrap, reason = "Timer J is the constant 32 s")]
    pub const TIMER_J_MS: i64 = TIMER_J as i64;

    /// The request of `method` on `branch` answered at `now_ms` with
    /// `response`, kept until Timer J.
    pub fn new(method: &str, branch: &str, response: RetainedEmission, now_ms: i64) -> Self {
        AnsweredRequest {
            method: method.to_string(),
            branch: branch.to_string(),
            response,
            until_ms: now_ms.saturating_add(Self::TIMER_J_MS),
        }
    }

    /// Whether the server transaction is still Completed at `now_ms`.
    pub fn completed_at(&self, now_ms: i64) -> bool {
        now_ms < self.until_ms
    }
}
