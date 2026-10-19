//! The `terminate` fold of a `/call/failure` consult: the call relays the
//! failed final its seed stated (or lets a timeout stand) and ends. It is the
//! fold of a relay decision; an unanswered consult folds
//! [`crate::failure_unavailable`] instead.

use serde_json::json;

/// The `outcome` of the fold.
pub const OUTCOME: &str = "terminate";

/// The fold's payload: the failed final's status and reason the seed stated
/// in `request`, the failure's origin (what raised the consult: a final, a
/// deadline, a limiter), the failed leg, and the relay decision's `label`.
pub fn payload(
    request: &serde_json::Value,
    failed_leg_id: &str,
    label: Option<String>,
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
    if let Some(label) = label {
        p.insert("label".into(), json!(label));
    }
    serde_json::Value::Object(p)
}

/// The failed leg the seed named, `""` when none.
pub fn failed_leg_id(request: &serde_json::Value) -> &str {
    request.get("failed_leg_id").and_then(|v| v.as_str()).unwrap_or("")
}
