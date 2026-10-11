//! The fold of a `/call/failure` consult the decision backend left
//! unanswered — its error, or the answer's deadline expiring (ADR-0039): the
//! caller gets the stack's own bare `503`, no `Reason`, no `Retry-After`, and
//! none of the failed peer's relayed headers (ADR-0043). It is no decision,
//! so it marks nothing.

use serde_json::json;

use crate::decision_log::STACK_AUTHORED;

/// The `outcome` of the fold: the `reject` fold, on the stack's own account.
pub(crate) const OUTCOME: &str = "reject";

/// The `origin` the fold states: the final answers no peer's, so the minted
/// `503` carries none of the failed peer's headers.
pub(crate) const ORIGIN: &str = "decision_unavailable";

/// The status the caller gets.
const STATUS: u16 = 503;

/// The fold's payload for the consult `request` left unanswered.
pub(crate) fn payload(request: &serde_json::Value) -> serde_json::Value {
    json!({
        "code": STATUS,
        "failed_leg_id": crate::failure_terminate::failed_leg_id(request),
        "origin": ORIGIN,
        STACK_AUTHORED: true,
    })
}

/// Whether a `/call/failure` fold's final answers the failed peer's own:
/// not one resolving a limiter refusal (`origin` `call_limiter`: the
/// router's re-consult or terminal 486), nor one answering a consult left
/// unanswered ([`ORIGIN`]).
pub(crate) fn answers_the_peer(payload: &serde_json::Value) -> bool {
    !matches!(payload.get("origin").and_then(|v| v.as_str()), Some("call_limiter" | ORIGIN))
}
