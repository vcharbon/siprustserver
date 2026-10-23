//! The `/call/refer` consult's answer as the `refer-http-result` fold carries
//! it: `allow` with the destination, `reject` with a code (the decision's, or
//! the engine's own refusal, which marks nothing), or `error` for no answer.

use serde::Serialize;
use serde_json::json;

use super::fold_payload::to_payload;
use crate::decision::{CallDecisionError, CallReferResponse, SipHeaderUpdates};
use crate::decision_log::STACK_AUTHORED;

#[derive(Serialize)]
struct ReferDestinationPayload {
    host: String,
    // Emitted even when None (null) — the fold rule reads absent-vs-null alike,
    // but the wire shape is pinned by the e2e refer tests.
    port: Option<u16>,
    transport: Option<String>,
}

#[derive(Serialize)]
struct ReferAllowPayload {
    action: &'static str,
    destination: ReferDestinationPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_refer_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update_headers: Option<SipHeaderUpdates>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_answer_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    callback_context: Option<String>,
    /// The decision's label, for the fold's `MarkDecision`.
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

/// The fold's outcome and payload for `answer`.
pub(super) fn refer_result(
    answer: Result<CallReferResponse, CallDecisionError>,
) -> (&'static str, serde_json::Value) {
    match answer {
        Ok(CallReferResponse::Allow {
            destination,
            new_refer_to,
            update_headers,
            no_answer_ms,
            callback_context,
            label,
        }) => (
            "allow",
            to_payload(ReferAllowPayload {
                action: "allow",
                destination: ReferDestinationPayload {
                    host: destination.host,
                    port: destination.port,
                    transport: destination.transport,
                },
                new_refer_to,
                update_headers,
                no_answer_ms,
                callback_context,
                label,
            }),
        ),
        Ok(CallReferResponse::Reject { code, reason, label }) => {
            ("reject", json!({ "reject_code": code, "reject_reason": reason, "label": label }))
        }
        // The engine's own refusal keeps its code; no answer is the
        // backend failure's 503.
        Err(CallDecisionError::Refused { code, reason, .. }) => {
            let reason =
                reason.unwrap_or_else(|| b2bua_sdk::reason_phrase::default_reason(code).into());
            (
                "reject",
                json!({ "reject_code": code, "reject_reason": reason, STACK_AUTHORED: true }),
            )
        }
        Err(_) => ("error", json!({ STACK_AUTHORED: true })),
    }
}
