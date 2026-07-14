//! The fire-and-forget decision callouts: detached async work (`/call/refer`,
//! `/call/failure`, `call_release`, the generic service HTTP request) that
//! folds its result back into the router as a re-entrant internal event, plus
//! the JSON marshalling both directions ride (typed `Serialize` payloads out,
//! tolerant hand parsers in).

use std::sync::Arc;

use call::Call;
use serde::Serialize;
use serde_json::json;

use super::RouterCtx;
use crate::decision::{
    CallFailureRequest, CallReferResponse, CallReleaseResponse, CallSnapshot, CallTreatment,
    FailureInfo, RouteDecision, SipHeaderUpdates,
};
use crate::event::CallEvent;
use crate::limiter::{AdmitOutcome, CallLimiter, LimiterEntry};

/// Fold a callout result back into the router as a re-entrant internal event.
/// Sent via the router's event channel rather than calling `on_event` directly:
/// the `on_event → process → process_result → on_event` cycle has an opaque
/// future type the compiler cannot prove `Send`; routing back through `run`'s
/// loop keeps re-entry single-threaded and breaks the recursion.
fn send_internal(
    ctx: &RouterCtx,
    call_ref: String,
    topic: &str,
    outcome: &str,
    payload: serde_json::Value,
    body: Vec<u8>,
) {
    let _ = ctx.reentry_tx.send(CallEvent::InternalEvent {
        call_ref,
        topic: topic.to_string(),
        outcome: outcome.to_string(),
        payload,
        body,
    });
}

/// Serialize a typed payload to the internal-event JSON. Payload structs are
/// program-constructed (no non-string keys, no non-finite floats), so failure
/// is unreachable; degrade to an empty object rather than kill the callout task.
fn to_payload<T: Serialize>(p: T) -> serde_json::Value {
    serde_json::to_value(p).unwrap_or_else(|_| serde_json::Value::Object(Default::default()))
}

/// Admit a route's `call_limiter` entries against the cluster limiter — the ONE
/// fold shared by the failover and release callouts, so the two can never drift
/// from each other (or from the initial `apply_route` chain's semantics).
/// `Ok(None)`: nothing to admit, or the limiter was unavailable (fail-open —
/// no holds recorded, initial-route parity); `Ok(Some((entries, window)))`:
/// admitted holds for the resolution rule to fold into the call
/// (`RecordLimiterHolds`; if the call dies before the fold lands, the orphaned
/// INCR ages out of the limiter window); `Err(limiter_id)`: rejected over-cap —
/// the caller owns the treatment.
async fn admit_route_limiters(
    limiter: &dyn CallLimiter,
    route: &RouteDecision,
) -> Result<Option<(Vec<(String, i64)>, i64)>, String> {
    if route.call_limiter.is_empty() {
        return Ok(None);
    }
    let entries: Vec<LimiterEntry> = route
        .call_limiter
        .iter()
        .map(|e| LimiterEntry { id: e.id.clone(), limit: e.limit })
        .collect();
    match limiter.admit(&entries).await {
        AdmitOutcome::Admitted { window } => Ok(Some((
            route
                .call_limiter
                .iter()
                .map(|e| (e.id.clone(), e.limit))
                .collect(),
            window,
        ))),
        AdmitOutcome::Unavailable => Ok(None),
        AdmitOutcome::Rejected { limiter_id } => Err(limiter_id),
    }
}

// ── /call/refer ─────────────────────────────────────────────────────────────

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
    no_answer_timeout_sec: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    callback_context: Option<String>,
}

/// Kick the async `/call/refer` round-trip and fold the decision back in as a
/// `refer-http-result` internal event. Call-scoped context is attached HERE
/// (the framework holds the authoritative call at dispatch); the seed rule's
/// JSON carries only the event-scoped facts.
pub(super) fn spawn_refer_callout(
    ctx: &Arc<RouterCtx>,
    call: &Call,
    call_ref: String,
    request: serde_json::Value,
) {
    let ctx2 = ctx.clone();
    let snapshot = CallSnapshot::of(call);
    tokio::spawn(async move {
        let mut req = parse_call_refer_request(&request);
        req.snapshot = snapshot;
        let (outcome, payload) = match ctx2.decision.call_refer(req).await {
            Ok(CallReferResponse::Allow {
                destination,
                new_refer_to,
                update_headers,
                no_answer_timeout_sec,
                callback_context,
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
                    no_answer_timeout_sec,
                    callback_context,
                }),
            ),
            Ok(CallReferResponse::Reject { code, reason }) => (
                "reject",
                json!({ "reject_code": code, "reject_reason": reason }),
            ),
            Err(_) => ("error", json!({})),
        };
        send_internal(&ctx2, call_ref, "refer-http-result", outcome, payload, Vec::new());
    });
}

// ── /call/failure ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct FailureRejectPayload {
    code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update_headers: Option<SipHeaderUpdates>,
    failed_leg_id: String,
}

#[derive(Serialize)]
struct RedirectContactPayload {
    uri: String,
    // Emitted even when None (null): the advisory q is part of the pinned shape.
    q: Option<f32>,
}

#[derive(Serialize)]
struct FailureRedirectPayload {
    code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    contacts: Vec<RedirectContactPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update_headers: Option<SipHeaderUpdates>,
    failed_leg_id: String,
}

/// Kick the async `/call/failure` decision (b-leg failover) and fold the
/// treatment back in as a `call-failure-result` internal event. Call-scoped
/// context is attached HERE; the seed rule's JSON carries only the event-scoped
/// facts (origin, failed leg, sip headers).
pub(super) fn spawn_failure_callout(
    ctx: &Arc<RouterCtx>,
    call: &Call,
    call_ref: String,
    request: serde_json::Value,
) {
    let ctx2 = ctx.clone();
    let snapshot = CallSnapshot::of(call);
    tokio::spawn(async move {
        let (outcome, payload) = failure_outcome(&ctx2, snapshot, &request).await;
        send_internal(&ctx2, call_ref, "call-failure-result", outcome, payload, Vec::new());
    });
}

/// Resolve one `/call/failure` consult to its internal-event fold.
///
/// Failover-route/initial-route parity: a failover Route is admitted against
/// the call limiter here (the rule layer is sync), and a limiter reject
/// re-consults `/call/failure` with origin `call_limiter` — the same bounded
/// chain (`MAX_LIMITER_FAILOVER`) `apply_route` runs for the initial route.
/// `failed_leg_id` is echoed on every fold so the resolution rule can cancel
/// the right no-answer timer / relay the failure.
async fn failure_outcome(
    ctx: &Arc<RouterCtx>,
    snapshot: CallSnapshot,
    request: &serde_json::Value,
) -> (&'static str, serde_json::Value) {
    let mut req = parse_call_failure_request(request);
    req.snapshot = snapshot.clone();
    let failed_leg_id = request
        .get("failed_leg_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut depth: u32 = 0;
    loop {
        match ctx.decision.call_failure(req).await {
            Ok(CallTreatment::Route(route)) => {
                let admitted = match admit_route_limiters(ctx.limiter.as_ref(), &route).await {
                    Ok(admitted) => admitted,
                    Err(limiter_id) => {
                        if route.callback_context.is_some()
                            && depth < crate::decision::apply_route::MAX_LIMITER_FAILOVER
                        {
                            depth += 1;
                            req = CallFailureRequest {
                                callback_context: route.callback_context.clone(),
                                failure: FailureInfo {
                                    origin: "call_limiter".to_string(),
                                    limiter_id: Some(limiter_id),
                                    failed_leg_id: (!failed_leg_id.is_empty())
                                        .then(|| failed_leg_id.clone()),
                                    ..Default::default()
                                },
                                snapshot: snapshot.clone(),
                            };
                            continue;
                        }
                        // Chain exhausted / no context → the initial path's
                        // terminal limiter treatment (486 Busy Here).
                        return (
                            "reject",
                            json!({
                                "code": 486,
                                "reason": "Busy Here",
                                "failed_leg_id": failed_leg_id,
                            }),
                        );
                    }
                };
                return ("failover", route_result_payload(route, admitted, Some(failed_leg_id)));
            }
            // Decision-authored reject — the plan declined to fail over and
            // supplied its own final failure (code/reason/headers).
            Ok(CallTreatment::Reject(rj)) => {
                return (
                    "reject",
                    to_payload(FailureRejectPayload {
                        code: rj.reject_code,
                        reason: rj.reject_reason,
                        update_headers: rj.update_headers,
                        failed_leg_id,
                    }),
                );
            }
            // Decision-authored 3xx redirect with a Contact list.
            Ok(CallTreatment::Redirect(rd)) => {
                return (
                    "redirect",
                    to_payload(FailureRedirectPayload {
                        code: rd.code,
                        reason: rd.reason,
                        contacts: rd
                            .contacts
                            .into_iter()
                            .map(|c| RedirectContactPayload { uri: c.uri, q: c.q })
                            .collect(),
                        update_headers: rd.update_headers,
                        failed_leg_id,
                    }),
                );
            }
            // Explicit `Relay`, or a backend error → relay the original b-leg
            // failure (response path) + tear the call down. Echo the failure's
            // status/reason the seed stashed for the relay.
            Ok(CallTreatment::Relay) | Err(_) => {
                let mut p = serde_json::Map::new();
                if let Some(v) = request.get("sip_code") {
                    p.insert("status".into(), v.clone());
                }
                if let Some(v) = request.get("sip_reason") {
                    p.insert("reason".into(), v.clone());
                }
                p.insert("failed_leg_id".into(), json!(failed_leg_id));
                return ("terminate", serde_json::Value::Object(p));
            }
        }
    }
}

// ── call_release ────────────────────────────────────────────────────────────

/// Kick the async `call_release` consult for a subscribed internal release
/// event and fold the response back in as a `call-release-result` internal
/// event (`release` | `reroute`). The engine is deadline-wrapped
/// (`DeadlineDecisionEngine` bounds `call_release` too), so the await cannot
/// wedge the established call: expiry / engine error both fold the `release`
/// outcome — the local teardown.
pub(super) fn spawn_release_callout(
    ctx: &Arc<RouterCtx>,
    call: &Call,
    call_ref: String,
    request: serde_json::Value,
) {
    let ctx2 = ctx.clone();
    let snapshot = CallSnapshot::of(call);
    tokio::spawn(async move {
        let req = parse_call_release_request(&request, snapshot);
        let (outcome, payload) = match ctx2.decision.call_release(req).await {
            Ok(CallReleaseResponse::Route(route)) => {
                match admit_route_limiters(ctx2.limiter.as_ref(), &route).await {
                    Ok(admitted) => ("reroute", route_result_payload(route, admitted, None)),
                    // Divergence from the failover chain, DOCUMENTED: a limiter
                    // reject here does NOT re-consult the engine — the call was
                    // going down anyway, so the reject degrades to the release
                    // default (local teardown) instead of a recursive failover
                    // walk.
                    Err(_) => ("release", json!({"reason": "limiter_rejected"})),
                }
            }
            // Release, engine error, or deadline expiry → the local teardown
            // (the fail-safe the request demands).
            Ok(CallReleaseResponse::Release) => ("release", json!({})),
            Err(_) => ("release", json!({"reason": "engine_error"})),
        };
        send_internal(&ctx2, call_ref, "call-release-result", outcome, payload, Vec::new());
    });
}

// ── generic service HTTP ────────────────────────────────────────────────────

/// The `ServiceHttpRequest` effect fields, regrouped for dispatch.
pub(super) struct ServiceHttpCallout {
    pub(super) call_ref: String,
    pub(super) correlation_id: String,
    pub(super) endpoint: String,
    pub(super) method: String,
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: Vec<u8>,
    pub(super) content_type: Option<String>,
    pub(super) timeout_ms: Option<u64>,
}

/// The generic service-authorable async HTTP callback (ADR-0016 seam). The
/// response entity rides BINARY-SAFE on `InternalEvent::body` — it is NEVER
/// coerced through `payload`'s JSON string. With no port injected the machine
/// is never stranded: an immediate `error` result is folded so the consuming
/// rule still fires.
pub(super) fn spawn_service_http_callout(ctx: &Arc<RouterCtx>, c: ServiceHttpCallout) {
    let Some(port) = ctx.adaptation_http.clone() else {
        send_internal(
            ctx,
            c.call_ref,
            "service-http-result",
            "error",
            json!({
                "correlation_id": c.correlation_id,
                "error": "adaptation_http_not_configured",
            }),
            Vec::new(),
        );
        return;
    };
    let ctx2 = ctx.clone();
    // The transport request future IS `Send`, so the whole spawned task is
    // `Send` (unlike the `on_event` cycle).
    tokio::spawn(async move {
        // Per-request budget is INDEPENDENT of `call_control_timeout_ms` (the
        // `DeadlineDecisionEngine` wraps only new_call/call_failure). Fail-safe
        // on teardown: a re-entry landing on a dead `call_ref` is dropped.
        let budget = c
            .timeout_ms
            .map(std::time::Duration::from_millis)
            .unwrap_or(port.default_timeout);
        let mut req = http_net::HttpRequest {
            method: c.method,
            path: c.endpoint,
            headers: c.headers,
            body: c.body,
        };
        if let Some(ct) = c.content_type {
            req.headers.push(("Content-Type".to_string(), ct));
        }
        let (outcome, payload, body): (&str, serde_json::Value, Vec<u8>) =
            match tokio::time::timeout(budget, port.transport.request(port.base, req)).await {
                Ok(Ok(resp)) => {
                    let http_net::HttpResponse { status, headers, body } = resp;
                    (
                        "ok",
                        json!({
                            "correlation_id": c.correlation_id,
                            "status": status,
                            "headers": headers,
                        }),
                        body,
                    )
                }
                Ok(Err(e)) => (
                    "error",
                    json!({
                        "correlation_id": c.correlation_id,
                        "error": e.to_string(),
                    }),
                    Vec::new(),
                ),
                Err(_elapsed) => (
                    "error",
                    json!({
                        "correlation_id": c.correlation_id,
                        "error": "timeout",
                    }),
                    Vec::new(),
                ),
            };
        send_internal(&ctx2, c.call_ref, "service-http-result", outcome, payload, body);
    });
}

// ── route-result payload (shared by failover + release) ────────────────────

#[derive(Serialize)]
struct RouteDestinationPayload {
    host: String,
    // Emitted even when None (null) — matches the initial-route wire shape.
    port: Option<u16>,
}

#[derive(Serialize)]
struct LimiterEntryPayload {
    id: String,
    limit: i64,
}

#[derive(Serialize)]
struct CallLimiterPayload {
    window: i64,
    entries: Vec<LimiterEntryPayload>,
}

/// The internal-event payload the route-fold rules (`failover-create-leg` /
/// `release-reroute`) consume: destination, identity/header rewrites, the
/// output-parity fields the initial `apply_route` honors (features,
/// service_ext, update_body, subscriptions), and any limiter holds the
/// dispatching task **already admitted**. ONE shape shared by the failover and
/// release folds, so the two can never drift.
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
    no_answer_timeout_sec: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    callback_context: Option<String>,
    // Route parity: the fields the initial `apply_route` honors, forwarded to
    // the resolution rule.
    features: call::features::FeatureActivations,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    service_ext: std::collections::BTreeMap<String, serde_json::Value>,
    // Always present (even when empty): the latest applied route OWNS the
    // subscription registry, so a route with no `subscribe[]` must CLEAR a
    // previous route's — exactly what `apply_route` does on the initial path.
    subscriptions: Vec<call::ReleaseEventKind>,
    // Keep → absent; Drop → null; Replace(s) → the string.
    #[serde(skip_serializing_if = "Option::is_none")]
    update_body: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_limiter: Option<CallLimiterPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed_leg_id: Option<String>,
}

/// Serialize a [`RouteDecision`] into the [`RoutePayload`] internal-event JSON.
/// `admitted` carries the holds `admit_route_limiters` already admitted;
/// `failed_leg_id` is the failover fold's echo (release folds pass `None`).
fn route_result_payload(
    route: RouteDecision,
    admitted: Option<(Vec<(String, i64)>, i64)>,
    failed_leg_id: Option<String>,
) -> serde_json::Value {
    let no_answer_timeout_sec = route
        .no_answer_timeout_sec
        .or(route.features.no_answer_timeout_sec);
    to_payload(RoutePayload {
        destination: RouteDestinationPayload {
            host: route.destination.host,
            port: route.destination.port,
        },
        new_ruri: route.new_ruri,
        new_from: route.new_from,
        new_to: route.new_to,
        update_headers: route.update_headers,
        no_answer_timeout_sec,
        callback_context: route.callback_context,
        features: route.features,
        service_ext: route.service_ext,
        subscriptions: route.subscriptions,
        update_body: match route.update_body {
            crate::decision::BodyUpdate::Keep => None,
            crate::decision::BodyUpdate::Drop => Some(None),
            crate::decision::BodyUpdate::Replace(s) => Some(Some(s)),
        },
        call_limiter: admitted.map(|(entries, window)| CallLimiterPayload {
            window,
            entries: entries
                .into_iter()
                .map(|(id, limit)| LimiterEntryPayload { id, limit })
                .collect(),
        }),
        failed_leg_id,
    })
}

// ── request parsers (seed-rule JSON → typed decision requests) ─────────────

/// Rebuild a [`CallReferRequest`](crate::decision::CallReferRequest) from the
/// JSON the seed rule emitted.
fn parse_call_refer_request(v: &serde_json::Value) -> crate::decision::CallReferRequest {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let sip_headers = v
        .get("sip_headers")
        .and_then(|x| x.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    crate::decision::CallReferRequest {
        call_id: s("call_id").unwrap_or_default(),
        dialog_id: s("dialog_id").unwrap_or_default(),
        callback_context: s("callback_context"),
        refer_to: s("refer_to").unwrap_or_default(),
        referred_by: s("referred_by"),
        sip_headers,
        snapshot: CallSnapshot::default(),
    }
}

/// Rebuild a [`CallReleaseRequest`](crate::decision::CallReleaseRequest) from
/// the JSON the `max-duration` seed rule emitted, attaching the call-scoped
/// snapshot the dispatch site built from the authoritative call.
fn parse_call_release_request(
    v: &serde_json::Value,
    snapshot: CallSnapshot,
) -> crate::decision::CallReleaseRequest {
    // The closed v1 set is the single `max_call_duration`; parse it via serde
    // (the enum's snake_case wire form) and default to it — the one event that
    // can currently emit this seed.
    let event = v
        .get("event")
        .and_then(|x| serde_json::from_value::<call::ReleaseEventKind>(x.clone()).ok())
        .unwrap_or(call::ReleaseEventKind::MaxCallDuration);
    crate::decision::CallReleaseRequest {
        callback_context: v
            .get("callback_context")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        event,
        snapshot,
    }
}

/// Rebuild a [`CallFailureRequest`] from the JSON the seed rule emitted. The
/// call-scoped `snapshot` is not part of the rule JSON — the dispatch site
/// attaches it from the authoritative call.
fn parse_call_failure_request(v: &serde_json::Value) -> CallFailureRequest {
    CallFailureRequest {
        callback_context: v
            .get("callback_context")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        failure: FailureInfo {
            origin: v
                .get("origin")
                .and_then(|x| x.as_str())
                .unwrap_or("external")
                .to_string(),
            status_code: v
                .get("sip_code")
                .and_then(|x| x.as_u64())
                .map(|c| c as u16),
            limiter_id: v.get("limiter_id").and_then(|x| x.as_str()).map(str::to_string),
            failed_leg_id: v
                .get("failed_leg_id")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            // `[[name, value], …]` — wire order and duplicates preserved.
            sip_headers: v
                .get("sip_headers")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|pair| {
                            let p = pair.as_array()?;
                            Some((p.first()?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        },
        snapshot: CallSnapshot::default(),
    }
}
