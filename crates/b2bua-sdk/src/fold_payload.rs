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

/// The internal-event topic a service's `RuleAction::ReplaceAdmissionSet`
/// result rides; its outcome is the admit's (`call::AdmitOutcome::label`).
pub const LIMITER_ADMIT_RESULT: &str = "limiter-admit-result";

/// The payload key every event carrying an admit report (a route fold, a
/// service's admit result) states it under.
pub const LIMITER_ADMIT_REPORT: &str = "limiter_admit";

/// The admit report a fold's payload carries under [`LIMITER_ADMIT_REPORT`].
pub fn admit_report(payload: &serde_json::Value) -> Option<call::AdmitReport> {
    serde_json::from_value(payload.get(LIMITER_ADMIT_REPORT)?.clone()).ok()
}
