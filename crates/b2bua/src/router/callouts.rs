//! The fire-and-forget decision callouts: detached async work (`/call/refer`,
//! `/call/failure`, `call_release`, the generic service HTTP request) that
//! folds its result back into the router as a re-entrant internal event, plus
//! the JSON marshalling both directions ride (typed `Serialize` payloads out,
//! tolerant hand parsers in).

use std::sync::Arc;

use call::{Call, CallLimiterState};
use serde::Serialize;
use serde_json::json;

use super::RouterCtx;
use crate::decision::{
    CallDecisionError, CallFailureRequest, CallReferResponse, CallReleaseResponse, CallSnapshot,
    CallTreatment, FailureInfo, RouteDecision, SipHeaderUpdates,
};
use crate::decision_log::STACK_AUTHORED;
use crate::event::CallEvent;
use crate::limiter::{state_after_admit, AdmitOutcome, LimiterEntry};
use crate::limiter_release::ReleaseQueue;
use crate::rules::defaults::route_fold_limiter_state;
use tokio::sync::mpsc;

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
    let _ = ctx.reentry_tx.send(internal_event(call_ref, topic, outcome, payload, body));
}

fn internal_event(
    call_ref: String,
    topic: &str,
    outcome: &str,
    payload: serde_json::Value,
    body: Vec<u8>,
) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref,
        topic: topic.to_string(),
        outcome: outcome.to_string(),
        payload,
        body,
    }
}

/// Post a route fold to the router. A fold the router can no longer take (its
/// channel closed) is never stated on a call, so the call its dispatching task
/// counted is released here.
fn send_route_fold(
    tx: &mpsc::UnboundedSender<CallEvent>,
    releases: &ReleaseQueue,
    fold: CallEvent,
) {
    if let Err(mpsc::error::SendError(fold)) = tx.send(fold) {
        release_route_fold_call(releases, &fold);
    }
}

/// Queue the release of the call a route fold owes a release for
/// ([`route_fold_limiter_state`]) — for a fold no call will state. The call's
/// own terminal release may have run already: the server applies this one as
/// a no-op then.
pub(super) fn release_route_fold_call(releases: &ReleaseQueue, fold: &CallEvent) {
    if let Some(state) = route_fold_limiter_state(fold).filter(|l| l.release_owed) {
        releases.push(&state.key);
    }
}

/// Record one detached decision round trip on a traced call's root span
/// (ADR-0026): the seed request and the resolved treatment as the two bodies of
/// a child span. `None` handle = unsampled call, and nothing is serialized.
fn record_round_trip(
    trace: &Option<crate::trace::emit::TraceHandle>,
    ctx: &RouterCtx,
    route: &'static str,
    sent_at_ms: i64,
    request: &serde_json::Value,
    outcome: &str,
    payload: &serde_json::Value,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.round_trip(
        route,
        sent_at_ms,
        &serde_json::to_vec(request).unwrap_or_default(),
        ctx.clock.now_ms(),
        outcome,
        &serde_json::to_vec(payload).unwrap_or_default(),
    );
}

/// Serialize a typed payload to the internal-event JSON. Payload structs are
/// program-constructed (no non-string keys, no non-finite floats), so failure
/// is unreachable; degrade to an empty object rather than kill the callout task.
fn to_payload<T: Serialize>(p: T) -> serde_json::Value {
    serde_json::to_value(p).unwrap_or_else(|_| serde_json::Value::Object(Default::default()))
}

/// Replace the call's set on the cluster limiter with a route's
/// `call_limiter` entries — the ONE fold shared by the failover and release
/// callouts, so the two can never drift from each other. One
/// `admit(key, entries, release_on_refusal = true)`, checked net of the set
/// the call holds: a refusal releases that set in the same step. Nothing is
/// sent for a route stating no limiter on a call the limiter does not count.
/// `Ok(state)`: the call's admission state the fold carries
/// ([`state_after_admit`] from `prior`) — counted with the route's ids, as it
/// was after a lost answer, or uncounted after an empty route or a
/// release-fence refusal (the call ended; counted as
/// `limiter_admit_released_fold`); `Err(limiter_id)`: refused on a cap — the
/// caller owns the treatment, the call holds nothing and owes its release.
async fn admit_route_limiters(
    ctx: &RouterCtx,
    prior: &CallLimiterState,
    route: &RouteDecision,
) -> Result<CallLimiterState, String> {
    if route.call_limiter.is_empty() && !prior.counted {
        return Ok(prior.clone());
    }
    let entries: Vec<LimiterEntry> = route
        .call_limiter
        .iter()
        .map(|e| LimiterEntry { id: e.id.clone(), limit: e.limit })
        .collect();
    let outcome = ctx.limiter.admit(&prior.key, &entries, true).await;
    let ids: Vec<String> = entries.into_iter().map(|e| e.id).collect();
    match outcome {
        AdmitOutcome::Rejected { limiter_id } => Err(limiter_id),
        outcome => {
            if outcome == AdmitOutcome::Released {
                ctx.metrics.bump_limiter_admit_released_fold();
            }
            Ok(state_after_admit(prior, &outcome, true, ids))
        }
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
    /// The decision's label, for the fold's `MarkDecision`.
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
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
    let trace = crate::trace::emit::TraceHandle::of(call);
    tokio::spawn(async move {
        let sent_at_ms = ctx2.clock.now_ms();
        let mut req = parse_call_refer_request(&request);
        req.snapshot = snapshot;
        let (outcome, payload) = match ctx2.decision.call_refer(req).await {
            Ok(CallReferResponse::Allow {
                destination,
                new_refer_to,
                update_headers,
                no_answer_timeout_sec,
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
                    no_answer_timeout_sec,
                    callback_context,
                    label,
                }),
            ),
            Ok(CallReferResponse::Reject { code, reason, label }) => {
                ("reject", json!({ "reject_code": code, "reject_reason": reason, "label": label }))
            }
            Err(_) => ("error", json!({ STACK_AUTHORED: true })),
        };
        record_round_trip(&trace, &ctx2, "/call/refer", sent_at_ms, &request, outcome, &payload);
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
    /// `call_limiter` when this resolution answers a limiter refusal rather
    /// than the failed peer's final — the a-facing mint then carries none of
    /// the peer's relayed headers (ADR-0017 X2).
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<&'static str>,
    /// The reject's service slices, merged by the fold as a route's are.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    service_ext: std::collections::BTreeMap<String, serde_json::Value>,
    /// The decision's label, for the fold's `MarkDecision`.
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
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
    /// See [`FailureRejectPayload::origin`].
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<&'static str>,
    /// The redirect's service slices, merged by the fold as a route's are.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    service_ext: std::collections::BTreeMap<String, serde_json::Value>,
    /// The decision's label, for the fold's `MarkDecision`.
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
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
    let trace = crate::trace::emit::TraceHandle::of(call);
    let limiter = call.limiter.clone();
    tokio::spawn(async move {
        let sent_at_ms = ctx2.clock.now_ms();
        let (outcome, payload) = failure_outcome(&ctx2, &limiter, snapshot, &request).await;
        record_round_trip(&trace, &ctx2, "/call/failure", sent_at_ms, &request, outcome, &payload);
        let fold = internal_event(call_ref, "call-failure-result", outcome, payload, Vec::new());
        send_route_fold(&ctx2.reentry_tx, &ctx2.limiter_releases, fold);
    });
}

/// Resolve one `/call/failure` consult to its internal-event fold.
///
/// Failover-route/initial-route parity: a failover Route is admitted against
/// the call limiter here (the rule layer is sync), and a limiter reject
/// re-consults `/call/failure` with origin `call_limiter` — the same bounded
/// chain (`MAX_LIMITER_FAILOVER`) `apply_route` runs for the initial route.
/// `failed_leg_id` is echoed on every fold so the resolution rule can cancel
/// the right no-answer timer / relay the failure. A reject/redirect resolved
/// AFTER a `call_limiter` re-consult (and the terminal 486) answers the
/// limiter refusal, not the failed peer's final — the fold says so via
/// `origin`, so the a-facing mint carries none of the peer's relayed headers
/// (ADR-0017 X2). A resolution other than a route after a refusal carries the
/// call uncounted, owing its release, in `call_limiter`: the refusal dropped
/// its set.
async fn failure_outcome(
    ctx: &Arc<RouterCtx>,
    limiter: &CallLimiterState,
    snapshot: CallSnapshot,
    request: &serde_json::Value,
) -> (&'static str, serde_json::Value) {
    let mut req = parse_call_failure_request(request);
    req.snapshot = snapshot.clone();
    let failed_leg_id =
        request.get("failed_leg_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut depth: u32 = 0;
    // A refused replacement released the call's set: the re-consult's route
    // replaces nothing, and a resolution other than a route states the call
    // uncounted.
    let mut state = limiter.clone();
    let mut refused = false;
    let (outcome, mut payload) = loop {
        match ctx.decision.call_failure(req).await {
            Ok(CallTreatment::Route(route)) => {
                let admitted = match admit_route_limiters(ctx, &state, &route).await {
                    Ok(admitted) => admitted,
                    Err(limiter_id) => {
                        state = CallLimiterState::unconfirmed(state.key);
                        refused = true;
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
                        // terminal limiter treatment (486 Busy Here) — the
                        // stack's own capacity statement.
                        break (
                            "reject",
                            json!({
                                "code": 486,
                                "reason": "Busy Here",
                                "failed_leg_id": failed_leg_id,
                                "origin": "call_limiter",
                                STACK_AUTHORED: true,
                            }),
                        );
                    }
                };
                break ("failover", route_result_payload(route, admitted, Some(failed_leg_id)));
            }
            // Decision-authored reject — the plan declined to fail over and
            // supplied its own final failure (code/reason/headers).
            Ok(CallTreatment::Reject(rj)) => {
                break (
                    "reject",
                    to_payload(FailureRejectPayload {
                        code: rj.reject_code,
                        reason: rj.reject_reason,
                        update_headers: rj.update_headers,
                        failed_leg_id,
                        origin: (depth > 0).then_some("call_limiter"),
                        service_ext: rj.service_ext,
                        label: rj.label,
                    }),
                );
            }
            // Decision-authored 3xx redirect with a Contact list.
            Ok(CallTreatment::Redirect(rd)) => {
                break (
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
                        origin: (depth > 0).then_some("call_limiter"),
                        service_ext: rd.service_ext,
                        label: rd.label,
                    }),
                );
            }
            // Explicit `Relay`, or a backend error → relay the original b-leg
            // failure (response path) + tear the call down. Echo the failure's
            // status/reason the seed stashed for the relay.
            Ok(CallTreatment::Relay { label }) => {
                break ("terminate", terminate_payload(request, &failed_leg_id, Some(label)));
            }
            // The engine's stated refusal: the reject fold a decision reject
            // takes, on the stack's own account.
            Err(CallDecisionError::Refused { code, reason, update_headers }) => {
                let mut payload = to_payload(FailureRejectPayload {
                    code,
                    reason,
                    update_headers,
                    failed_leg_id,
                    origin: (depth > 0).then_some("call_limiter"),
                    service_ext: Default::default(),
                    label: None,
                });
                payload[STACK_AUTHORED] = json!(true);
                break ("reject", payload);
            }
            Err(CallDecisionError::Unavailable(_)) => {
                break ("terminate", terminate_payload(request, &failed_leg_id, None));
            }
        }
    };
    if refused && outcome != "failover" {
        payload["call_limiter"] = to_payload(state);
    }
    (outcome, payload)
}

/// The `terminate` fold's payload: the failed final's status and reason the
/// seed stashed, the failure's origin (what raised the consult: a final, a
/// deadline, a limiter) and the failed leg. `decided` is the relay decision's
/// label when the decision layer returned one; `None` is an unanswered
/// consult, the stack's own resolution.
fn terminate_payload(
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
    let trace = crate::trace::emit::TraceHandle::of(call);
    let limiter = call.limiter.clone();
    tokio::spawn(async move {
        let sent_at_ms = ctx2.clock.now_ms();
        let req = parse_call_release_request(&request, snapshot);
        // Every `release` fold names the event that raised the consult, so
        // the rule that applies it can end the call under that event where
        // no decision stands behind the fold.
        let event = json!(req.event);
        let (outcome, payload) = match ctx2.decision.call_release(req).await {
            Ok(CallReleaseResponse::Route(route)) => {
                match admit_route_limiters(&ctx2, &limiter, &route).await {
                    Ok(admitted) => ("reroute", route_result_payload(route, admitted, None)),
                    // Divergence from the failover chain, DOCUMENTED: a limiter
                    // reject here does NOT re-consult the engine — the call was
                    // going down anyway, so the reject degrades to the release
                    // default (local teardown) instead of a recursive failover
                    // walk; the refusal released the call's set, and the fold
                    // states the call uncounted, owing its release. The answer
                    // still stands for everything but its route: the release
                    // is marked under its label and its service slices are
                    // merged.
                    Err(_) => {
                        let mut payload = json!({
                            "reason": "limiter_rejected",
                            "event": event,
                            "label": route.label,
                            "call_limiter": CallLimiterState::unconfirmed(limiter.key.clone()),
                        });
                        if !route.service_ext.is_empty() {
                            payload["service_ext"] = json!(route.service_ext);
                        }
                        ("release", payload)
                    }
                }
            }
            // Release, engine error, or deadline expiry → the local teardown
            // (the fail-safe the request demands).
            Ok(CallReleaseResponse::Release { label, service_ext }) => {
                let mut payload = json!({ "label": label, "event": event });
                if !service_ext.is_empty() {
                    payload["service_ext"] = json!(service_ext);
                }
                ("release", payload)
            }
            Err(_) => {
                ("release", json!({"reason": "engine_error", "event": event, STACK_AUTHORED: true}))
            }
        };
        record_round_trip(
            &trace,
            &ctx2,
            "/calls/events/release",
            sent_at_ms,
            &request,
            outcome,
            &payload,
        );
        let fold = internal_event(call_ref, "call-release-result", outcome, payload, Vec::new());
        send_route_fold(&ctx2.reentry_tx, &ctx2.limiter_releases, fold);
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
        let budget =
            c.timeout_ms.map(std::time::Duration::from_millis).unwrap_or(port.default_timeout);
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

/// The internal-event payload the route-fold rules (`failover-create-leg` /
/// `release-reroute`) consume: destination, identity/header rewrites, the
/// output-parity fields the initial `apply_route` honors (features,
/// service_ext, update_body, subscriptions), and the call's admission state
/// the dispatching task **already settled** on the limiter. ONE shape shared
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
    /// The call's admission state after the dispatching task replaced its
    /// set.
    call_limiter: CallLimiterState,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed_leg_id: Option<String>,
    /// The decision's label, for the fold's `MarkDecision`.
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

/// Serialize a [`RouteDecision`] into the [`RoutePayload`] internal-event JSON.
/// `admitted` is the call's admission state `admit_route_limiters` settled;
/// `failed_leg_id` is the failover fold's echo (release folds pass `None`).
fn route_result_payload(
    route: RouteDecision,
    admitted: CallLimiterState,
    failed_leg_id: Option<String>,
) -> serde_json::Value {
    let no_answer_timeout_sec =
        route.no_answer_timeout_sec.or(route.features.no_answer_timeout_sec);
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
        call_limiter: admitted,
        failed_leg_id,
        label: route.label,
    })
}

// ── request parsers (seed-rule JSON → typed decision requests) ─────────────

/// `[[name, value], …]` as emitted by a seed rule → header lines, wire order and
/// duplicates preserved; absent or malformed → empty.
fn parse_header_lines(v: Option<&serde_json::Value>) -> Vec<(String, String)> {
    v.and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|pair| {
                    let p = pair.as_array()?;
                    Some((p.first()?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Rebuild a [`CallReferRequest`](crate::decision::CallReferRequest) from the
/// JSON the seed rule emitted.
fn parse_call_refer_request(v: &serde_json::Value) -> crate::decision::CallReferRequest {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    crate::decision::CallReferRequest {
        call_id: s("call_id").unwrap_or_default(),
        dialog_id: s("dialog_id").unwrap_or_default(),
        callback_context: s("callback_context"),
        refer_to: s("refer_to").unwrap_or_default(),
        referred_by: s("referred_by"),
        sip_headers: parse_header_lines(v.get("sip_headers")),
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
        callback_context: v.get("callback_context").and_then(|x| x.as_str()).map(str::to_string),
        event,
        snapshot,
    }
}

/// Rebuild a [`CallFailureRequest`] from the JSON the seed rule emitted. The
/// call-scoped `snapshot` is not part of the rule JSON — the dispatch site
/// attaches it from the authoritative call.
fn parse_call_failure_request(v: &serde_json::Value) -> CallFailureRequest {
    CallFailureRequest {
        callback_context: v.get("callback_context").and_then(|x| x.as_str()).map(str::to_string),
        failure: FailureInfo {
            origin: v.get("origin").and_then(|x| x.as_str()).unwrap_or("external").to_string(),
            status_code: v.get("sip_code").and_then(|x| x.as_u64()).map(|c| c as u16),
            limiter_id: v.get("limiter_id").and_then(|x| x.as_str()).map(str::to_string),
            failed_leg_id: v
                .get("failed_leg_id")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            timeout_kind: v.get("timeout_kind").and_then(|x| x.as_str()).map(str::to_string),
            sip_headers: parse_header_lines(v.get("sip_headers")),
        },
        snapshot: CallSnapshot::default(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::limiter::NoopLimiter;
    use crate::limiter_release::ReleaseQueueConfig;

    /// A release queue nobody drains: what a fold queued stays readable.
    fn releases() -> Arc<ReleaseQueue> {
        let config = ReleaseQueueConfig { lease: Duration::from_secs(120), cap: 16 };
        ReleaseQueue::new(Arc::new(NoopLimiter), config, crate::metrics::B2buaMetrics::new())
    }

    fn failover_fold(counted: bool, release_owed: bool) -> CallEvent {
        let mut route = crate::decision::test_adapter::route_to("127.0.0.1", 5070);
        route.call_limiter = vec![
            crate::decision::CallLimiterEntry { id: "x".into(), limit: 10 },
            crate::decision::CallLimiterEntry { id: "y".into(), limit: 10 },
        ];
        let admitted = CallLimiterState {
            key: "call-1#k".into(),
            counted,
            release_owed,
            ids: if counted { vec!["x".into(), "y".into()] } else { vec![] },
            generation: 0,
        };
        let payload = route_result_payload(route, admitted, Some("b-1".into()));
        internal_event("call-1".into(), "call-failure-result", "failover", payload, Vec::new())
    }

    #[tokio::test]
    async fn a_route_fold_the_router_cannot_take_releases_its_call() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let releases = releases();
        send_route_fold(&tx, &releases, failover_fold(true, true));
        assert_eq!(releases.waiting_keys(), ["call-1#k"], "by its key");
    }

    #[tokio::test]
    async fn an_uncounted_route_fold_owing_its_release_releases_its_call() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let releases = releases();
        send_route_fold(&tx, &releases, failover_fold(false, true));
        assert_eq!(releases.waiting_keys(), ["call-1#k"], "by its key");
    }

    #[tokio::test]
    async fn a_route_fold_owing_nothing_releases_nothing() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let releases = releases();
        send_route_fold(&tx, &releases, failover_fold(false, false));
        assert!(releases.waiting_keys().is_empty(), "a call that sent no admit owes nothing");
    }

    #[tokio::test]
    async fn a_delivered_route_fold_keeps_its_state_for_the_call() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let releases = releases();
        send_route_fold(&tx, &releases, failover_fold(true, true));
        assert!(releases.waiting_keys().is_empty(), "the call states it");
        let state =
            route_fold_limiter_state(&rx.recv().await.unwrap()).expect("the fold carries it");
        assert!(state.counted && state.release_owed);
        assert_eq!(state.key, "call-1#k");
        assert_eq!(state.ids, ["x", "y"]);
    }
}
