//! The payload vocabulary of an async consult's fold (the internal event a
//! decision callout lands as) that a service rule claiming the fold reads.

/// The key a callout sets (`true`) on a fold it resolved on the stack's own
/// account — an unanswered consult, a refusal the engine stated, the limiter
/// chain's terminal final — with no decision behind it.
pub const STACK_AUTHORED: &str = "stack_authored";

/// Whether a fold's payload carries [`STACK_AUTHORED`]: the final or teardown
/// it asks for is the stack's own, not a decision.
pub fn stack_authored(payload: &serde_json::Value) -> bool {
    payload.get(STACK_AUTHORED).and_then(|v| v.as_bool()).unwrap_or(false)
}
