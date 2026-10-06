//! The `terminate` fold of a `/call/failure` consult: the call relays the
//! failed final its seed stated (or lets a timeout stand) and ends. It is the
//! fold of a relay decision and of an unanswered consult alike — the engine's
//! error, or the answer's deadline expiring (ADR-0039) — so the two can never
//! drift.

use serde_json::json;

use crate::decision_log::STACK_AUTHORED;

/// The `outcome` of the fold.
pub(crate) const OUTCOME: &str = "terminate";

/// The fold's payload: the failed final's status and reason the seed stated
/// in `request`, the failure's origin (what raised the consult: a final, a
/// deadline, a limiter) and the failed leg. `decided` is the relay
/// decision's label when the decision layer returned one; `None` is an
/// unanswered consult, the stack's own resolution.
pub(crate) fn payload(
    request: &serde_json::Value,
    failed_leg_id: &str,
    decided: Option<Option<String>>,
) -> serde_json::Value {
    let mut p = serde_json::Map::new();
    if let Some(v) = request.get("sip_code") {
        p.insert("status".into(), v.clone());
    }
    if let Some(v) = request.get("sip_reason") {
        p.insert("reason".into(), v.clone());
    }
    if let Some(v) = request.get("origin") {
        p.insert("origin".into(), v.clone());
    }
    p.insert("failed_leg_id".into(), json!(failed_leg_id));
    match decided {
        Some(Some(label)) => {
            p.insert("label".into(), json!(label));
        }
        Some(None) => {}
        None => {
            p.insert(STACK_AUTHORED.into(), json!(true));
        }
    }
    serde_json::Value::Object(p)
}

/// The fold's payload for the consult `request` left unanswered.
pub(crate) fn unanswered(request: &serde_json::Value) -> serde_json::Value {
    payload(request, failed_leg_id(request), None)
}

/// The failed leg the seed named, `""` when none.
pub(crate) fn failed_leg_id(request: &serde_json::Value) -> &str {
    request.get("failed_leg_id").and_then(|v| v.as_str()).unwrap_or("")
}
