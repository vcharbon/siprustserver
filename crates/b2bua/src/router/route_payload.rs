//! The route-result payload both async route folds carry — a setup
//! failover's and an established call's reroute — and the rules that apply
//! them read (`rules::defaults::route_fold`).

use call::AdmitReport;
use serde::Serialize;

use crate::decision::{RouteDecision, SipHeaderUpdates};

#[derive(Serialize)]
struct RouteDestinationPayload {
    host: String,
    // Emitted even when None (null) — matches the initial-route wire shape.
    port: Option<u16>,
}

/// The internal-event payload the route-fold rules (`failover-create-leg` /
/// `release-reroute`) consume: destination, identity/header rewrites, the
/// output-parity fields the initial `apply_route` honors (features,
/// service_ext, update_body, subscriptions), and the admit the dispatching
/// task **already sent** to the limiter. ONE shape shared
/// by the failover and release folds, so the two can never drift.
#[derive(Serialize)]
struct RoutePayload {
    destination: RouteDestinationPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_ruri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update_headers: Option<SipHeaderUpdates>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_answer_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    callback_context: Option<String>,
    // Route parity: the fields the initial `apply_route` honors, forwarded to
    // the resolution rule.
    features: call::features::FeatureActivations,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    service_ext: std::collections::BTreeMap<String, serde_json::Value>,
    // Always present (even when empty): the latest applied route OWNS the
    // subscription registry, so a route with no subscriptions must CLEAR a
    // previous route's — exactly what `apply_route` does on the initial path.
    subscriptions: Vec<call::ReleaseEventKind>,
    // Keep / AttachParts → absent; Drop → null; Replace(s) → the string.
    #[serde(skip_serializing_if = "Option::is_none")]
    update_body: Option<Option<String>>,
    // AttachParts(parts) → the parts; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    attach_parts: Option<Vec<sip_message::MultipartPart>>,
    /// The admit the dispatching task sent to replace the call's set.
    limiter_admit: AdmitReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed_leg_id: Option<String>,
    /// The decision's label, for the fold's `MarkDecision`.
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

/// Serialize a [`RouteDecision`] into the [`RoutePayload`] internal-event JSON.
/// `admitted` is the admit `admit_route_limiters` sent (or did not send);
/// `failed_leg_id` is the failover fold's echo (release folds pass `None`).
pub(super) fn route_result_payload(
    route: RouteDecision,
    admitted: AdmitReport,
    failed_leg_id: Option<String>,
) -> serde_json::Value {
    let no_answer_ms = route.no_answer().millis();
    super::fold_payload::to_payload(RoutePayload {
        destination: RouteDestinationPayload {
            host: route.destination.host,
            port: route.destination.port,
        },
        new_ruri: route.new_ruri,
        new_from: route.new_from,
        new_to: route.new_to,
        update_headers: route.update_headers,
        no_answer_ms,
        callback_context: route.callback_context,
        features: route.features,
        service_ext: route.service_ext,
        subscriptions: route.subscriptions,
        update_body: match &route.update_body {
            crate::decision::BodyUpdate::Keep | crate::decision::BodyUpdate::AttachParts(_) => None,
            crate::decision::BodyUpdate::Drop => Some(None),
            crate::decision::BodyUpdate::Replace(s) => Some(Some(s.clone())),
        },
        attach_parts: match route.update_body {
            crate::decision::BodyUpdate::AttachParts(parts) => Some(parts),
            _ => None,
        },
        limiter_admit: admitted,
        failed_leg_id,
        label: route.label,
    })
}

#[cfg(test)]
mod tests;
