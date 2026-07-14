//! The effect interpreter: persist the handler result, then run its typed
//! effects in the fixed order persist → critical → outbound → soft → buffered,
//! with terminal eviction last (ADR-0020 X2) and fire-and-forget callouts
//! detached at the end.

use std::net::SocketAddr;
use std::sync::Arc;

use call::CallModelState;
use sip_message::{serialize, SipMessage};

use super::callouts;
use super::release::{release_call, ReleaseKind};
use super::RouterCtx;
use crate::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, FireAndForgetEffect, HandlerResult,
    OutboundBody, OutboundTxnMode, SoftBoundedEffect,
};

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
    // call via a re-entrant internal event (see `callouts`).
    for eff in result.effects.fire_and_forget {
        match eff {
            FireAndForgetEffect::ReferAsyncHttp { call_ref, request } => {
                callouts::spawn_refer_callout(ctx, &result.call, call_ref, request);
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
                callouts::spawn_service_http_callout(
                    ctx,
                    callouts::ServiceHttpCallout {
                        call_ref,
                        correlation_id,
                        endpoint,
                        method,
                        headers,
                        body,
                        content_type,
                        timeout_ms,
                    },
                );
            }
            FireAndForgetEffect::FailureAsyncHttp { call_ref, request } => {
                callouts::spawn_failure_callout(ctx, &result.call, call_ref, request);
            }
            FireAndForgetEffect::ReleaseAsyncHttp { call_ref, request } => {
                callouts::spawn_release_callout(ctx, &result.call, call_ref, request);
            }
            FireAndForgetEffect::Reenter(ev) => {
                let _ = ctx.reentry_tx.send(*ev);
            }
        }
    }
}
