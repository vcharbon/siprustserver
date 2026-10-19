//! The fire-and-forget decision callouts: detached async work (`/call/refer`,
//! `/call/failure`, `call_release`, the generic service HTTP request, a
//! service's replacement of the call's admission set) that folds its result
//! back into the router as a re-entrant internal event, plus the JSON
//! marshalling both directions ride (typed `Serialize` payloads out, tolerant
//! hand parsers in).

use std::sync::Arc;

use call::{AdmitReport, Call, CallLimiterState};
use serde::Serialize;
use serde_json::json;

use super::fold_payload::{put, to_payload};
use super::RouterCtx;
use crate::answer_deadline;
use crate::decision::{
    CallDecisionError, CallFailureRequest, CallReleaseResponse, CallSnapshot, CallTreatment,
    FailureInfo, RouteDecision, SipHeaderUpdates,
};
use crate::decision_log::STACK_AUTHORED;
use crate::limiter::report::{admit_report_of, LimiterAdmitResult};
use crate::limiter::{AdmitOutcome, LimiterEntry, LimiterHeld, LimiterWorker};
use crate::metrics::AdmitSite;
use crate::{failure_terminate, failure_unavailable};
use b2bua_sdk::event::CallEvent;
use tokio::sync::mpsc;

/// The call a callout answers: its `call_ref`, and the incarnation whose turn
/// sent it, which the result carries so no later call on the ref reads it.
pub(super) struct Caller {
    pub(super) call_ref: String,
    pub(super) incarnation: String,
}

impl Caller {
    /// The call `call_ref` whose incarnation is `call`'s.
    pub(super) fn of(call_ref: String, call: &Call) -> Self {
        Self { call_ref, incarnation: call.incarnation().to_string() }
    }
}

/// Fold a callout result back into the router as a re-entrant internal event.
/// Sent via the router's event channel rather than calling `on_event` directly:
/// the `on_event → process → process_result → on_event` cycle has an opaque
/// future type the compiler cannot prove `Send`; routing back through `run`'s
/// loop keeps re-entry single-threaded and breaks the recursion.
fn send_internal(
    ctx: &RouterCtx,
    to: Caller,
    topic: &str,
    outcome: &str,
    payload: serde_json::Value,
    body: Vec<u8>,
) {
    let _ = ctx.reentry_tx.send(internal_event(to, topic, outcome, payload, body));
}

fn internal_event(
    to: Caller,
    topic: &str,
    outcome: &str,
    payload: serde_json::Value,
    body: Vec<u8>,
) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: to.call_ref,
        topic: topic.to_string(),
        outcome: outcome.to_string(),
        payload,
        body,
        incarnation: Some(to.incarnation),
    }
}

/// Post an event carrying an admit report (a route fold, a service's admit
/// result) to the router. One the router can no longer take (its channel
/// closed) is never applied to a call, so the key its admit may have counted
/// is released here.
fn send_admit_fold(
    tx: &mpsc::UnboundedSender<CallEvent>,
    limiter: &LimiterWorker,
    fold: CallEvent,
) {
    if let Err(mpsc::error::SendError(fold)) = tx.send(fold) {
        release_admit_fold_call(limiter, &fold);
    }
}

/// Queue the release an event's admit report ([`admit_report_of`]) owes,
/// for an event no call will apply ([`LimiterWorker::release_unclaimed`]).
pub(super) fn release_admit_fold_call(limiter: &LimiterWorker, fold: &CallEvent) {
    if let Some(report) = admit_report_of(fold) {
        limiter.release_unclaimed(&report);
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

/// Replace the call's set on the cluster limiter with a route's
/// `call_limiter` entries — the ONE fold shared by the failover and release
/// callouts, so the two can never drift from each other. One
/// `admit(key, change, held, entries, release_on_refusal = true)`, checked
/// net of the set the call holds (`held`, the call's held set, which it
/// carries): a refusal releases that set in the same step. Nothing is sent for
/// a route stating no limiter on a call holding nothing: the report states an
/// unsent admit of nothing, which asks for nothing. The worker answers just
/// past the limiter's admit budget, `unavailable` when the limiter did not,
/// so the consult's answer deadline lies past the whole chain. `Ok(report)`: the
/// admit the fold carries — admitted, lost, superseded, or refused by a release
/// fence (the call ended; counted as
/// `b2bua_limiter_admit_released_total{site="fold"}`); `Err(report)`: refused
/// on a cap — the caller owns the treatment, the call holds nothing and owes
/// its release.
async fn admit_route_limiters(
    ctx: &RouterCtx,
    key: &str,
    held: &LimiterHeld,
    change: u64,
    route: &RouteDecision,
) -> Result<AdmitReport, AdmitReport> {
    if route.call_limiter.is_empty() && held.entries.is_empty() {
        let outcome = AdmitOutcome::NotSent;
        return Ok(AdmitReport { key: key.to_string(), change, entries: Vec::new(), outcome });
    }
    let entries = route.call_limiter.clone();
    let report = ctx.limiter.admit(AdmitSite::Fold, key, change, held, entries, true).await;
    match &report.outcome {
        AdmitOutcome::Rejected { .. } => Err(report),
        _ => Ok(report),
    }
}

// ── /call/refer ─────────────────────────────────────────────────────────────

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
    let to = Caller::of(call_ref, call);
    observe::spawn_keeping_node(async move {
        let sent_at_ms = ctx2.clock.now_ms();
        let mut req = super::callout_requests::parse_call_refer_request(&request);
        req.snapshot = snapshot;
        let (outcome, payload) =
            super::refer_result::refer_result(ctx2.decision.call_refer(req).await);
        record_round_trip(&trace, &ctx2, "/call/refer", sent_at_ms, &request, outcome, &payload);
        send_internal(&ctx2, to, "refer-http-result", outcome, payload, Vec::new());
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
/// facts (origin, failed leg, sip headers). The fold states `deadline`, the
/// answer deadline its turn armed, when there is one.
pub(super) fn spawn_failure_callout(
    ctx: &Arc<RouterCtx>,
    call: &Call,
    call_ref: String,
    request: serde_json::Value,
    limiter_change: u64,
    deadline: Option<u64>,
) {
    let ctx2 = ctx.clone();
    let snapshot = CallSnapshot::of(call);
    let trace = crate::trace::emit::TraceHandle::of(call);
    let limiter = call.limiter.clone();
    let to = Caller::of(call_ref, call);
    observe::spawn_keeping_node(async move {
        let sent_at_ms = ctx2.clock.now_ms();
        let (outcome, mut payload) =
            failure_outcome(&ctx2, &limiter, limiter_change, snapshot, &request).await;
        record_round_trip(&trace, &ctx2, "/call/failure", sent_at_ms, &request, outcome, &payload);
        // The fold names the deadline it answers, when its turn armed one.
        if let Some(deadline) = deadline {
            put(&mut payload, answer_deadline::CONSULT_CHANGE, json!(deadline));
        }
        let fold = internal_event(to, "call-failure-result", outcome, payload, Vec::new());
        send_admit_fold(&ctx2.reentry_tx, &ctx2.limiter, fold);
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
/// refused admit's report in `limiter_admit`: the refusal dropped the call's
/// set. The chain's admits are numbered from `first_change` on, one each, in
/// the block the dispatching turn reserved for them.
async fn failure_outcome(
    ctx: &Arc<RouterCtx>,
    limiter: &CallLimiterState,
    first_change: u64,
    snapshot: CallSnapshot,
    request: &serde_json::Value,
) -> (&'static str, serde_json::Value) {
    let mut req = super::callout_requests::parse_call_failure_request(request);
    req.snapshot = snapshot.clone();
    let failed_leg_id = failure_terminate::failed_leg_id(request).to_string();
    let mut depth: u32 = 0;
    // A refused replacement released the call's set: the re-consult's admit
    // carries the set the refusal stated (none), and a resolution other than
    // a route carries the refusal's report.
    let mut held = limiter.held_set();
    let mut refused: Option<AdmitReport> = None;
    // Every route the limiter refused on the way, for the engine to record
    // as an attempt that dialled nothing.
    let mut refused_routes = Vec::new();
    let (outcome, mut payload) = loop {
        match ctx.decision.call_failure(req).await {
            Ok(CallTreatment::Route(route)) => {
                let change = first_change + u64::from(depth);
                let admitted =
                    match admit_route_limiters(ctx, limiter.key(), &held, change, &route).await {
                        Ok(admitted) => admitted,
                        Err(report) => {
                            refused_routes.push(crate::attempts::refused_route_entry(
                                route.label.as_deref(),
                                &route.service_ext,
                                route.no_answer(),
                            ));
                            let limiter_id =
                                report.outcome.refused_on().unwrap_or_default().to_string();
                            held = report.held().unwrap_or_default();
                            refused = Some(report);
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
                break (
                    "failover",
                    super::route_payload::route_result_payload(
                        route,
                        admitted,
                        Some(failed_leg_id),
                    ),
                );
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
            // Explicit `Relay` → relay the original b-leg failure (response
            // path) + tear the call down. Echo the failure's status/reason the
            // seed stashed for the relay.
            Ok(CallTreatment::Relay { label }) => {
                break (
                    failure_terminate::OUTCOME,
                    failure_terminate::payload(request, &failed_leg_id, label),
                );
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
                put(&mut payload, STACK_AUTHORED, json!(true));
                break ("reject", payload);
            }
            Err(CallDecisionError::Unavailable(_)) => {
                break (failure_unavailable::OUTCOME, failure_unavailable::payload(request));
            }
        }
    };
    if let Some(report) = refused.filter(|_| outcome != "failover") {
        put(&mut payload, LimiterAdmitResult::REPORT, to_payload(report));
    }
    if !refused_routes.is_empty() {
        put(&mut payload, crate::attempts::REFUSED_ROUTES, json!(refused_routes));
    }
    (outcome, payload)
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
    limiter_change: u64,
    deadline: Option<u64>,
) {
    let ctx2 = ctx.clone();
    let snapshot = CallSnapshot::of(call);
    let trace = crate::trace::emit::TraceHandle::of(call);
    let limiter = call.limiter.clone();
    let to = Caller::of(call_ref, call);
    observe::spawn_keeping_node(async move {
        let sent_at_ms = ctx2.clock.now_ms();
        let req = super::callout_requests::parse_call_release_request(&request, snapshot);
        // Every `release` fold names the event that raised the consult, so
        // the rule that applies it can end the call under that event where
        // no decision stands behind the fold.
        let event = json!(req.event);
        let (outcome, payload) = match ctx2.decision.call_release(req).await {
            Ok(CallReleaseResponse::Route(route)) => {
                let admitted = admit_route_limiters(
                    &ctx2,
                    limiter.key(),
                    &limiter.held_set(),
                    limiter_change,
                    &route,
                )
                .await;
                match admitted {
                    Ok(admitted) => (
                        "reroute",
                        super::route_payload::route_result_payload(route, admitted, None),
                    ),
                    // A limiter reject here does NOT re-consult the engine: the
                    // reject degrades to the release default (local teardown)
                    // instead of a recursive failover walk; the refusal released
                    // the call's set, and the fold states the call uncounted,
                    // owing its release. The answer still stands for everything
                    // but its route: the release is marked under its label and
                    // its service slices are merged.
                    Err(report) => {
                        let refused = crate::attempts::refused_route_entry(
                            route.label.as_deref(),
                            &route.service_ext,
                            route.no_answer(),
                        );
                        let mut payload = json!({
                            "reason": "limiter_rejected",
                            "event": event,
                            "label": route.label,
                            LimiterAdmitResult::REPORT: report,
                            crate::attempts::REFUSED_ROUTES: [refused],
                        });
                        if !route.service_ext.is_empty() {
                            put(&mut payload, "service_ext", json!(route.service_ext));
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
                    put(&mut payload, "service_ext", json!(service_ext));
                }
                ("release", payload)
            }
            Err(_) => {
                ("release", json!({"reason": "engine_error", "event": event, STACK_AUTHORED: true}))
            }
        };
        let mut payload = payload;
        record_round_trip(&trace, &ctx2, "/call/release", sent_at_ms, &request, outcome, &payload);
        // The fold names the deadline it answers, when its turn armed one.
        if let Some(deadline) = deadline {
            put(&mut payload, answer_deadline::CONSULT_CHANGE, json!(deadline));
        }
        let fold = internal_event(to, "call-release-result", outcome, payload, Vec::new());
        send_admit_fold(&ctx2.reentry_tx, &ctx2.limiter, fold);
    });
}

// ── generic service HTTP ────────────────────────────────────────────────────

/// The `ServiceHttpRequest` effect fields, regrouped for dispatch.
pub(super) struct ServiceHttpCallout {
    pub(super) to: Caller,
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
            c.to,
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
    observe::spawn_keeping_node(async move {
        // Per-request budget is INDEPENDENT of `call_control_timeout_ms` (the
        // `DeadlineDecisionEngine` wraps only new_call/call_failure). Fail-safe
        // on teardown: a re-entry landing on a dead `call_ref` is dropped.
        let total =
            c.timeout_ms.map(std::time::Duration::from_millis).unwrap_or(port.default_timeout);
        let budgets = http_net::Budgets { attempt: port.attempt.min(total), total };
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
            match port.client.send_with_retry(req, budgets).await {
                Ok(resp) => {
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
                Err(e) => (
                    "error",
                    json!({
                        "correlation_id": c.correlation_id,
                        "error": match e {
                            http_net::AttemptError::Timeout => "timeout".to_string(),
                            other => other.to_string(),
                        },
                    }),
                    Vec::new(),
                ),
            };
        send_internal(&ctx2, c.to, "service-http-result", outcome, payload, body);
    });
}

// ── a service's replacement of the call's admission set ────────────────────

/// The `LimiterAdmit` effect fields, regrouped for dispatch.
pub(super) struct LimiterAdmitCallout {
    pub(super) call_ref: String,
    pub(super) correlation_id: String,
    pub(super) key: String,
    pub(super) change: u64,
    pub(super) held: LimiterHeld,
    pub(super) entries: Vec<LimiterEntry>,
}

/// A service's `ReplaceAdmissionSet`: one admit of the whole set under the
/// change number its turn reserved, a cap refusal keeping the set held, then
/// the `limiter-admit-result` re-entry ([`LimiterAdmitResult`]). A result the
/// router can no longer take, or one landing on a gone call, releases the
/// key when the request left.
pub(super) fn spawn_limiter_admit_callout(ctx: &Arc<RouterCtx>, c: LimiterAdmitCallout) {
    let ctx2 = ctx.clone();
    observe::spawn_keeping_node(async move {
        // Answered just past the limiter's admit budget, whatever the
        // limiter does: the answer's deadline (ADR-0039) lies past it.
        let report = ctx2
            .limiter
            .admit(AdmitSite::Service, &c.key, c.change, &c.held, c.entries, false)
            .await;
        let result =
            LimiterAdmitResult { call_ref: c.call_ref, correlation_id: c.correlation_id, report };
        send_admit_fold(&ctx2.reentry_tx, &ctx2.limiter, result.into_event());
    });
}

#[cfg(test)]
mod tests;
