//! The effect interpreter: persist the handler result, then run its typed
//! effects in the fixed order persist → critical → outbound → soft → buffered,
//! with terminal eviction last (ADR-0020 X2) and fire-and-forget callouts
//! detached at the end.

use std::net::SocketAddr;
use std::sync::Arc;

use call::CallModelState;
use sip_message::{serialize, SipMessage};

use super::callouts::{
    parse_call_failure_request, parse_call_refer_request, parse_call_release_request,
    route_result_payload,
};
use super::release::{release_call, ReleaseKind};
use super::RouterCtx;
use crate::decision::CallReferResponse;
use crate::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, FireAndForgetEffect, HandlerResult,
    OutboundBody, OutboundTxnMode, SoftBoundedEffect,
};
use crate::event::CallEvent;

/// Interpret a handler result: persist → critical → outbound → soft → buffered.
pub(super) async fn process_result(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    result: HandlerResult,
    now_ms: i64,
) {
    // Persist first (state lands before effects run).
    ctx.state.update(result.call.clone());

    // Model Y (ADR-0020 X3 amended): an acting-backup **takeover copy** that
    // reaches Terminated DEFERS the discharge to the live primary. It reverse-
    // flushes the terminal body — so the primary's Reclaim-tail reconcile
    // (`reclaim::reconcile_reverse_flush`) folds it in and discharges it
    // **exactly once** — then self-releases its live copy. It writes **NO** CDR,
    // releases **NO** limiter hold, propagates **NO** delete here (that is the
    // primary's sole authority, so exactly-once holds by construction — no
    // cross-node idempotency). If the primary never reconciles (crashed for
    // good, never returning inside the replica TTL), the retained `bak:` replica
    // is silently evicted by the periodic reap and the CDR/limiter cleanup is
    // LOST — the accepted double-failure. A primary-served (non-takeover)
    // terminal falls through to the normal discharge below.
    if result.call.state == CallModelState::Terminated && ctx.state.is_takeover(call_ref) {
        // Reverse-flush the Terminated body held with the normal replica TTL
        // (`reboot_budget`): a live primary reconciles + forward-deletes it within ~1
        // poll; a rebooting primary still has its full reclaim window to fold and
        // discharge it. The primary is the sole discharge authority either way.
        ctx.state.flush(&result.call);
        release_call(ctx, call_ref, ReleaseKind::SelfRelease).await;
        return;
    }

    // Replicate a non-terminated, backed-up call to its peer after each
    // authoritative mutation (the S10 flush-on-mutation wiring point).
    // `CallState::flush` is a no-op for calls with no replicable topology, so
    // the non-HA path is unchanged; for a backed-up call it routes through the
    // S8 write-side policy (Forward when primary, Reverse when acting-backup)
    // so the backup holds the latest state. The flush rides the buffered
    // terminate-writer (non-blocking).
    //
    // `Terminating` MUST flush too, not just `Active`: a teardown-in-progress
    // carries authoritative state the replica needs — the b-leg `ByeSent`
    // disposition and its bumped `local_cseq`. Without it an acting-backup
    // whose primary crashed never propagates that progress; a reclaim racing
    // the in-flight BYE then pulls a STALE `Active` snapshot, restarts
    // termination, and re-sends the BYE at the *reused* CSeq a real UAS drops
    // (matrix cells C7/RFC). Only `Terminated` is excluded — it takes the
    // `RemoveCall` delete path below instead.
    if matches!(
        result.call.state,
        CallModelState::Active | CallModelState::Terminating
    ) && result
        .call
        .topology
        .as_ref()
        .is_some_and(|t| !t.bak.is_empty())
    {
        ctx.state.flush(&result.call);
    }

    // The terminal `RemoveCall` is interpreted LAST — after the buffered
    // `WriteCdr` enqueue (ADR-0020 X2): propagating the replica delete before
    // the CDR is enqueued would let a failure in that window erase the call
    // everywhere (including the backup Element) with no CDR. Deferring only
    // delays the eviction / txn-cancel by the in-process lanes below; the call
    // is already unreachable for new work (its state is persisted and terminal).
    let mut remove_call = false;
    for eff in &result.effects.critical {
        match eff {
            CriticalStateEffect::ScheduleTimer(entry) => {
                ctx.timers.schedule(entry.clone(), call_ref.to_string()).await;
            }
            CriticalStateEffect::CancelTimer { id } => {
                ctx.timers.cancel(call_ref.to_string(), id.clone()).await
            }
            CriticalStateEffect::CancelAllTimers => {
                ctx.timers.cancel_all(call_ref.to_string()).await
            }
            CriticalStateEffect::Flush => ctx.state.flush(&result.call),
            CriticalStateEffect::RemoveCall => remove_call = true,
        }
    }

    // Sent SIP is liveness too (ADR-0020 X4 refinement): a turn that puts a
    // message on the wire (a keepalive OPTIONS, a relayed response, a teardown
    // BYE/CANCEL) stamps the ledger alongside received traffic, so the reaper
    // never preempts a teardown that is legitimately waiting on a slow peer.
    // Wire-silent turns (LimiterRefresh, absorbed events) stamp nothing. Only
    // for a still-live call — a terminated result is being released below.
    if !result.effects.outbound.is_empty() && result.call.state != CallModelState::Terminated {
        ctx.state.touch(call_ref, now_ms);
    }

    for eff in &result.effects.outbound {
        let dest: SocketAddr = match format!("{}:{}", eff.destination.0, eff.destination.1).parse() {
            Ok(d) => d,
            Err(_) => continue,
        };
        // Meter outbound requests we originate/relay (the in-dialog keepalive
        // OPTIONS lands here) — pairs with inbound responses_total{OPTIONS,200} to
        // isolate the keepalive round-trip (sent vs answered) on the b2bua itself.
        if let OutboundBody::Request(req) = &eff.body {
            ctx.metrics.record_request_out(req.method.as_str());
        }
        match (&eff.body, &eff.mode) {
            // A 2xx retransmit (RFC 3261 §13.3.1.4) must bypass the server txn:
            // the a-leg INVITE server txn is already `Completed`, so the txn layer
            // would DROP a second final on `send_response`. Send it raw.
            (OutboundBody::Response(resp), OutboundTxnMode::Raw) => {
                let _ = ctx.txn.send_raw(serialize(&SipMessage::Response(resp.clone())), dest).await;
            }
            (OutboundBody::Response(resp), _) => { let _ = ctx.txn.send_response(resp.clone(), dest).await; }
            (OutboundBody::Request(req), OutboundTxnMode::NewClient(kind)) => {
                let _ = ctx.txn.send_request(req.clone(), dest, *kind).await;
            }
            (OutboundBody::Request(req), OutboundTxnMode::Raw) => {
                let _ = ctx.txn.send_raw(serialize(&SipMessage::Request(req.clone())), dest).await;
            }
            (OutboundBody::Request(req), OutboundTxnMode::ServerResponse) => {
                // A request tagged ServerResponse is a misuse; send raw as a fallback.
                let _ = ctx.txn.send_raw(serialize(&SipMessage::Request(req.clone())), dest).await;
            }
        }
    }

    for eff in &result.effects.soft {
        match eff {
            SoftBoundedEffect::DecrementLimiter { limiter_id, window } => {
                ctx.limiter
                    .release(&[crate::limiter::LimiterHold {
                        limiter_id: limiter_id.clone(),
                        window: *window,
                    }])
                    .await
            }
        }
    }

    for eff in &result.effects.buffered {
        match eff {
            BufferedObservabilityEffect::WriteCdr => ctx.cdr.write(&result.call, now_ms).await,
        }
    }

    // Terminal eviction last of all (ADR-0020 X2): the CDR is enqueued before
    // the call — and its replicated Element — ceases to exist anywhere.
    if remove_call {
        release_call(ctx, call_ref, ReleaseKind::Terminated).await;
    }

    // Fire-and-forget: detached async work that folds its result back into the
    // call via a re-entrant internal event (the REFER `/call/refer` round-trip,
    // and the generic re-enter path).
    for eff in result.effects.fire_and_forget {
        match eff {
            FireAndForgetEffect::ReferAsyncHttp { call_ref, request } => {
                let ctx2 = ctx.clone();
                // Call-scoped context is attached HERE (the framework holds the
                // authoritative call at dispatch); the seed rule's JSON carries
                // only the event-scoped facts.
                let snapshot = crate::decision::CallSnapshot::of(&result.call);
                tokio::spawn(async move {
                    // Deserialize the request the seed rule built; call the
                    // decision backend; map to a `refer-http-result` internal
                    // event; re-enter the chain.
                    let mut req = parse_call_refer_request(&request);
                    req.snapshot = snapshot;
                    let (outcome, payload) = match ctx2.decision.call_refer(req).await {
                        Ok(CallReferResponse::Allow {
                            destination,
                            new_refer_to,
                            update_headers,
                            no_answer_timeout_sec,
                            callback_context,
                        }) => {
                            let mut p = serde_json::Map::new();
                            p.insert("action".into(), serde_json::json!("allow"));
                            p.insert(
                                "destination".into(),
                                serde_json::json!({
                                    "host": destination.host,
                                    "port": destination.port,
                                    "transport": destination.transport,
                                }),
                            );
                            if let Some(v) = new_refer_to {
                                p.insert("new_refer_to".into(), serde_json::json!(v));
                            }
                            if let Some(v) = update_headers {
                                p.insert("update_headers".into(), serde_json::json!(v));
                            }
                            if let Some(v) = no_answer_timeout_sec {
                                p.insert("no_answer_timeout_sec".into(), serde_json::json!(v));
                            }
                            if let Some(v) = callback_context {
                                p.insert("callback_context".into(), serde_json::json!(v));
                            }
                            ("allow", serde_json::Value::Object(p))
                        }
                        Ok(CallReferResponse::Reject { code, reason }) => (
                            "reject",
                            serde_json::json!({ "reject_code": code, "reject_reason": reason }),
                        ),
                        Err(_) => ("error", serde_json::json!({})),
                    };
                    let ev = CallEvent::InternalEvent {
                        call_ref,
                        topic: "refer-http-result".to_string(),
                        outcome: outcome.to_string(),
                        payload,
                        body: Vec::new(),
                    };
                    // Re-enter via the router's event channel rather than
                    // calling `on_event` directly: the `on_event → process →
                    // process_result → on_event` cycle has an opaque future type
                    // the compiler cannot prove `Send`. Routing the event back
                    // through `run`'s loop keeps re-entry single-threaded and
                    // breaks the recursion.
                    let _ = ctx2.reentry_tx.send(ev);
                });
            }
            FireAndForgetEffect::ServiceHttpRequest {
                call_ref,
                correlation_id,
                endpoint,
                method,
                headers,
                body,
                content_type,
                timeout_ms,
            } => {
                // The generic service-authorable async HTTP callback (mirrors the
                // `ReferAsyncHttp` arm's spawn+reentry recursion break). The
                // response entity rides BINARY-SAFE on `InternalEvent::body` — it
                // is NEVER coerced through `payload`'s JSON string.
                match ctx.adaptation_http.clone() {
                    // No port injected → never strand the machine: fold an
                    // immediate `error` result so the consuming rule still fires.
                    None => {
                        let _ = ctx.reentry_tx.send(CallEvent::InternalEvent {
                            call_ref,
                            topic: "service-http-result".to_string(),
                            outcome: "error".to_string(),
                            payload: serde_json::json!({
                                "correlation_id": correlation_id,
                                "error": "adaptation_http_not_configured",
                            }),
                            body: Vec::new(),
                        });
                    }
                    Some(port) => {
                        let ctx2 = ctx.clone();
                        // The transport request future IS `Send`, so the whole
                        // spawned task is `Send` (unlike the `on_event` cycle).
                        tokio::spawn(async move {
                            // Per-request budget is INDEPENDENT of
                            // `call_control_timeout_ms` (the `DeadlineDecisionEngine`
                            // wraps only new_call/call_failure). Fail-safe on
                            // teardown: a re-entry landing on a dead `call_ref` is
                            // dropped.
                            let budget = timeout_ms
                                .map(std::time::Duration::from_millis)
                                .unwrap_or(port.default_timeout);
                            let mut req = http_net::HttpRequest {
                                method,
                                path: endpoint,
                                headers,
                                body,
                            };
                            if let Some(ct) = content_type {
                                req.headers.push(("Content-Type".to_string(), ct));
                            }
                            let (outcome, payload, body): (&str, serde_json::Value, Vec<u8>) =
                                match tokio::time::timeout(
                                    budget,
                                    port.transport.request(port.base, req),
                                )
                                .await
                                {
                                    Ok(Ok(resp)) => {
                                        let http_net::HttpResponse { status, headers, body } = resp;
                                        (
                                            "ok",
                                            serde_json::json!({
                                                "correlation_id": correlation_id,
                                                "status": status,
                                                "headers": headers,
                                            }),
                                            body,
                                        )
                                    }
                                    Ok(Err(e)) => (
                                        "error",
                                        serde_json::json!({
                                            "correlation_id": correlation_id,
                                            "error": e.to_string(),
                                        }),
                                        Vec::new(),
                                    ),
                                    Err(_elapsed) => (
                                        "error",
                                        serde_json::json!({
                                            "correlation_id": correlation_id,
                                            "error": "timeout",
                                        }),
                                        Vec::new(),
                                    ),
                                };
                            let ev = CallEvent::InternalEvent {
                                call_ref,
                                topic: "service-http-result".to_string(),
                                outcome: outcome.to_string(),
                                payload,
                                body,
                            };
                            let _ = ctx2.reentry_tx.send(ev);
                        });
                    }
                }
            }
            FireAndForgetEffect::FailureAsyncHttp { call_ref, request } => {
                let ctx2 = ctx.clone();
                // Call-scoped context is attached HERE (the framework holds the
                // authoritative call at dispatch); the seed rule's JSON carries
                // only the event-scoped facts (origin, failed leg, sip headers).
                let snapshot = crate::decision::CallSnapshot::of(&result.call);
                tokio::spawn(async move {
                    // The seed rule's request JSON carries the failure context
                    // plus `failed_leg_id` (echoed back so the resolution rule
                    // can cancel the right no-answer timer / relay the failure).
                    let mut req = parse_call_failure_request(&request);
                    req.snapshot = snapshot.clone();
                    let failed_leg_id = request
                        .get("failed_leg_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    use crate::decision::CallTreatment;
                    // Failover-route/initial-route parity: a failover Route is
                    // admitted against the call limiter here (the rule layer is
                    // sync), and a limiter reject re-consults `/call/failure`
                    // with origin `call_limiter` — the same bounded chain
                    // `apply_route` runs for the initial route. A hold admitted
                    // here is folded into the call by the resolution rule
                    // (`RecordLimiterHolds`); if the call dies before the fold
                    // lands, the orphaned INCR ages out of the limiter window.
                    let mut depth: u32 = 0;
                    let (outcome, payload) = loop {
                        break match ctx2.decision.call_failure(req).await {
                        Ok(CallTreatment::Route(route)) => {
                            let mut admitted: Option<(Vec<(String, i64)>, i64)> = None;
                            if !route.call_limiter.is_empty() {
                                let entries: Vec<crate::limiter::LimiterEntry> = route
                                    .call_limiter
                                    .iter()
                                    .map(|e| crate::limiter::LimiterEntry {
                                        id: e.id.clone(),
                                        limit: e.limit,
                                    })
                                    .collect();
                                match ctx2.limiter.admit(&entries).await {
                                    crate::limiter::AdmitOutcome::Admitted { window } => {
                                        admitted = Some((
                                            route
                                                .call_limiter
                                                .iter()
                                                .map(|e| (e.id.clone(), e.limit))
                                                .collect(),
                                            window,
                                        ));
                                    }
                                    // Fail open: no holds recorded (parity with
                                    // the initial path's fail-open policy).
                                    crate::limiter::AdmitOutcome::Unavailable => {}
                                    crate::limiter::AdmitOutcome::Rejected { limiter_id } => {
                                        if route.callback_context.is_some()
                                            && depth
                                                < crate::decision::apply_route::MAX_LIMITER_FAILOVER
                                        {
                                            depth += 1;
                                            req = crate::decision::CallFailureRequest {
                                                callback_context: route.callback_context.clone(),
                                                failure: crate::decision::FailureInfo {
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
                                        // Chain exhausted / no context → the
                                        // initial path's terminal limiter
                                        // treatment (486 Busy Here).
                                        break (
                                            "reject",
                                            serde_json::json!({
                                                "code": 486,
                                                "reason": "Busy Here",
                                                "failed_leg_id": failed_leg_id,
                                            }),
                                        );
                                    }
                                }
                            }
                            let mut p = route_result_payload(route, admitted);
                            p.insert("failed_leg_id".into(), serde_json::json!(failed_leg_id));
                            ("failover", serde_json::Value::Object(p))
                        }
                        // Decision-authored reject — the plan declined to fail over
                        // and supplied its own final failure (code/reason/headers).
                        Ok(CallTreatment::Reject(rj)) => {
                            let mut p = serde_json::Map::new();
                            p.insert("code".into(), serde_json::json!(rj.reject_code));
                            if let Some(v) = rj.reject_reason {
                                p.insert("reason".into(), serde_json::json!(v));
                            }
                            if let Some(v) = rj.update_headers {
                                p.insert("update_headers".into(), serde_json::json!(v));
                            }
                            p.insert("failed_leg_id".into(), serde_json::json!(failed_leg_id));
                            ("reject", serde_json::Value::Object(p))
                        }
                        // Decision-authored 3xx redirect with a Contact list.
                        Ok(CallTreatment::Redirect(rd)) => {
                            let mut p = serde_json::Map::new();
                            p.insert("code".into(), serde_json::json!(rd.code));
                            if let Some(v) = rd.reason {
                                p.insert("reason".into(), serde_json::json!(v));
                            }
                            let contacts: Vec<serde_json::Value> = rd
                                .contacts
                                .iter()
                                .map(|c| serde_json::json!({ "uri": c.uri, "q": c.q }))
                                .collect();
                            p.insert("contacts".into(), serde_json::json!(contacts));
                            if let Some(v) = rd.update_headers {
                                p.insert("update_headers".into(), serde_json::json!(v));
                            }
                            p.insert("failed_leg_id".into(), serde_json::json!(failed_leg_id));
                            ("redirect", serde_json::Value::Object(p))
                        }
                        // Explicit `Relay`, or a backend error → relay the original
                        // b-leg failure (response path) + tear the call down. Echo
                        // the failure's status/reason the seed stashed for the relay.
                        Ok(CallTreatment::Relay) | Err(_) => {
                            let mut p = serde_json::Map::new();
                            if let Some(v) = request.get("sip_code") {
                                p.insert("status".into(), v.clone());
                            }
                            if let Some(v) = request.get("sip_reason") {
                                p.insert("reason".into(), v.clone());
                            }
                            p.insert("failed_leg_id".into(), serde_json::json!(failed_leg_id));
                            ("terminate", serde_json::Value::Object(p))
                        }
                        };
                    };
                    let ev = CallEvent::InternalEvent {
                        call_ref,
                        topic: "call-failure-result".to_string(),
                        outcome: outcome.to_string(),
                        payload,
                        body: Vec::new(),
                    };
                    let _ = ctx2.reentry_tx.send(ev);
                });
            }
            FireAndForgetEffect::ReleaseAsyncHttp { call_ref, request } => {
                let ctx2 = ctx.clone();
                // Call-scoped context is attached HERE, like the failure/refer
                // arms; the seed rule's JSON carries only the event-scoped
                // facts (callback_context + which event fired).
                let snapshot = crate::decision::CallSnapshot::of(&result.call);
                tokio::spawn(async move {
                    let req = parse_call_release_request(&request, snapshot);
                    use crate::decision::CallReleaseResponse;
                    // The engine is deadline-wrapped (`DeadlineDecisionEngine`
                    // bounds `call_release` too), so this await cannot wedge the
                    // established call: expiry / engine error both fold the
                    // `release` outcome — the local teardown.
                    let (outcome, payload) = match ctx2.decision.call_release(req).await {
                        Ok(CallReleaseResponse::Route(route)) => {
                            // Reroute-route parity: admit the route's limiter
                            // entries against the new target, exactly like the
                            // failover fold. Divergence from the failover chain,
                            // DOCUMENTED: a limiter *reject* here does NOT
                            // re-consult the engine — the call was going down
                            // anyway, so the reject degrades to the release
                            // default (local teardown) instead of a recursive
                            // failover walk.
                            let mut admitted: Option<(Vec<(String, i64)>, i64)> = None;
                            let mut limiter_rejected = false;
                            if !route.call_limiter.is_empty() {
                                let entries: Vec<crate::limiter::LimiterEntry> = route
                                    .call_limiter
                                    .iter()
                                    .map(|e| crate::limiter::LimiterEntry {
                                        id: e.id.clone(),
                                        limit: e.limit,
                                    })
                                    .collect();
                                match ctx2.limiter.admit(&entries).await {
                                    crate::limiter::AdmitOutcome::Admitted { window } => {
                                        admitted = Some((
                                            route
                                                .call_limiter
                                                .iter()
                                                .map(|e| (e.id.clone(), e.limit))
                                                .collect(),
                                            window,
                                        ));
                                    }
                                    // Fail open, record no holds (initial-route parity).
                                    crate::limiter::AdmitOutcome::Unavailable => {}
                                    crate::limiter::AdmitOutcome::Rejected { .. } => {
                                        limiter_rejected = true;
                                    }
                                }
                            }
                            if limiter_rejected {
                                ("release", serde_json::json!({"reason": "limiter_rejected"}))
                            } else {
                                (
                                    "reroute",
                                    serde_json::Value::Object(route_result_payload(
                                        route, admitted,
                                    )),
                                )
                            }
                        }
                        // Release, engine error, or deadline expiry → the
                        // local teardown (the fail-safe the request demands).
                        Ok(CallReleaseResponse::Release) => ("release", serde_json::json!({})),
                        Err(_) => ("release", serde_json::json!({"reason": "engine_error"})),
                    };
                    let ev = CallEvent::InternalEvent {
                        call_ref,
                        topic: "call-release-result".to_string(),
                        outcome: outcome.to_string(),
                        payload,
                        body: Vec::new(),
                    };
                    let _ = ctx2.reentry_tx.send(ev);
                });
            }
            FireAndForgetEffect::Reenter(ev) => {
                let _ = ctx.reentry_tx.send(*ev);
            }
        }
    }
}
