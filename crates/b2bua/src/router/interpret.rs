//! The effect interpreter: persist the handler result, then run its typed
//! effects in the fixed order persist → critical → outbound → soft → buffered,
//! with terminal eviction last (ADR-0020 X2) and fire-and-forget callouts
//! detached at the end.

use std::net::SocketAddr;
use std::sync::Arc;

use call::helpers::seal_termination_seq;
use call::{CallModelState, TimerType};
use sip_message::Method;
use sip_txn::TxnKind;

use super::callouts;
use super::release::{release_call, ReleaseKind};
use super::RouterCtx;
use crate::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, FireAndForgetEffect, HandlerResult,
    OutboundBody, OutboundTxnMode, QuietTurn, SoftBoundedEffect,
};

/// The quiet class the turn is persisted under, or `None` for a write. An
/// author's class is a candidate; the turn's effect set decides. A quiet turn
/// is exactly: an `Active` call, one outbound effect and it the retained
/// datagram's repeat, no soft, buffered or fire-and-forget effect, and no
/// critical effect but the rung's own re-arm for [`QuietTurn::OwnRung`]. A
/// rule that reaches the re-ACK beside anything else — a BYE, a CDR event, a
/// timer — is a write, as is a re-ACK on a terminating call, whose flush the
/// teardown needs.
fn quiet_class(result: &HandlerResult) -> Option<QuietTurn> {
    let kind = result.effects.quiet?;
    let fx = &result.effects;
    let one_repeat =
        matches!(fx.outbound.as_slice(), [eff] if matches!(eff.body, OutboundBody::Datagram(_)));
    let critical_is_own = match (kind, fx.critical.as_slice()) {
        (QuietTurn::ReAck, []) => true,
        (QuietTurn::OwnRung, [CriticalStateEffect::ScheduleTimer(entry)]) => {
            matches!(entry.timer_type, TimerType::Rung { .. })
        }
        _ => false,
    };
    let quiet = result.call.state == CallModelState::Active
        && one_repeat
        && critical_is_own
        && fx.soft.is_empty()
        && fx.buffered.is_empty()
        && fx.fire_and_forget.is_empty();
    quiet.then_some(kind)
}

/// Interpret a handler result: persist → critical → outbound → soft → buffered.
pub(super) async fn process_result(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    result: HandlerResult,
    now_ms: i64,
) {
    // What the turn sends is on the record before the record lands, and a
    // termination this turn began is cut after it: every ring entry with
    // `seq <= termination.last_seq` was received or sent as part of
    // beginning the termination, every later one came after (the peer's 200
    // to the relayed BYE, the ACK to a 487). With the ring off the cut stays
    // `0`.
    let result = match crate::message_ring::Ring::of(&ctx.config) {
        Some(ring) => HandlerResult {
            call: seal_termination_seq(ring.sent(result.call, &result.effects.outbound, now_ms)),
            effects: result.effects,
        },
        None => result,
    };
    // Persist first (state lands before effects run). A quiet turn replaces
    // the live copy without a bump and skips the flush gate below: the armed
    // rung advances, the backup keeps the last write that changed the call.
    let quiet = quiet_class(&result);
    match quiet {
        Some(kind) => {
            ctx.state.update_quiet(result.call.clone());
            ctx.metrics.record_quiet_turn(kind.kind());
        }
        None => ctx.state.update(result.call.clone()),
    }

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
        // What the turn owes the WIRE is not deferred: the final that ends the
        // caller's INVITE and the hop-by-hop ACK toward the callee leave from the
        // node that took the event, or the peers wedge on Timer B / Timer H.
        emit_outbound(ctx, call_ref, &result, now_ms).await;
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
    if quiet.is_none()
        && matches!(result.call.state, CallModelState::Active | CallModelState::Terminating)
        && result.call.topology.as_ref().is_some_and(|t| !t.bak.is_empty())
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

    emit_outbound(ctx, call_ref, &result, now_ms).await;

    for eff in &result.effects.soft {
        match eff {
            SoftBoundedEffect::DecrementLimiter { limiter_id, window } => {
                ctx.limiter
                    .release(&[crate::limiter::LimiterHold {
                        limiter_id: limiter_id.clone(),
                        window: *window,
                    }])
                    .await;
                if crate::trace::sampled(&result.call) {
                    crate::trace::emit::limiter(
                        &result.call,
                        now_ms,
                        "release",
                        &format!("{limiter_id} @ {window}"),
                    );
                }
            }
        }
    }

    for eff in &result.effects.buffered {
        match eff {
            BufferedObservabilityEffect::WriteCdr => ctx.cdr.write(&result.call, now_ms).await,
            BufferedObservabilityEffect::SecondFinalRefused { .. } => {
                ctx.metrics.bump_second_final_refused()
            }
            BufferedObservabilityEffect::ProvisionalAfterFinalRefused { .. } => {
                ctx.metrics.bump_provisional_after_final_refused()
            }
            BufferedObservabilityEffect::GoingAwayAbsorbed { .. } => {
                ctx.metrics.bump_going_away_absorbed()
            }
            BufferedObservabilityEffect::TerminationUnrecorded => {
                ctx.metrics.bump_termination_unrecorded()
            }
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

/// Put the turn's outbound SIP on the wire, in emission order. Sent SIP is
/// liveness too (ADR-0020 X4 refinement): a turn that puts a message on the
/// wire (a keepalive OPTIONS, a relayed response, a teardown BYE/CANCEL) stamps
/// the ledger alongside received traffic, so the reaper never preempts a
/// teardown that is legitimately waiting on a slow peer. Wire-silent turns
/// (LimiterRefresh, absorbed events) stamp nothing; a terminated result is
/// being released and stamps nothing either.
async fn emit_outbound(ctx: &Arc<RouterCtx>, call_ref: &str, result: &HandlerResult, now_ms: i64) {
    if !result.effects.outbound.is_empty() && result.call.state != CallModelState::Terminated {
        ctx.state.touch(call_ref, now_ms);
    }

    for eff in &result.effects.outbound {
        let dest: SocketAddr = match format!("{}:{}", eff.destination.0, eff.destination.1).parse()
        {
            Ok(d) => d,
            Err(_) => continue,
        };
        // Meter outbound requests we originate/relay (the in-dialog keepalive
        // OPTIONS lands here) — pairs with inbound responses_total{OPTIONS,200} to
        // isolate the keepalive round-trip (sent vs answered) on the b2bua itself.
        if let OutboundBody::Request(req) = &eff.body {
            ctx.metrics.record_request_out(req.method().as_str());
        }
        // Traced call: the message as it leaves, raw (ADR-0026). A typed message
        // is recorded as its image — the datagram the transaction layer sends —
        // and a retained datagram as the bytes it is, so a recorded rung is
        // what left the socket, never a re-render of it.
        if crate::trace::sampled(&result.call) {
            let wire: &[u8] = match &eff.body {
                OutboundBody::Request(req) => req.image(),
                OutboundBody::Response(resp) => resp.image(),
                OutboundBody::Datagram(emission) => emission.wire().0,
            };
            crate::trace::emit::sip_out(&result.call, now_ms, dest, wire);
        }
        match (&eff.body, &eff.mode) {
            // A retained emission's repeat (RFC 3261 §13.3.1.4 / §13.2.2.4,
            // RFC 3262 §3): the bytes as they are, past every transaction — the
            // server txn that sent the original is `Completed` and would drop a
            // second final; the ACK never had one. Counted as it leaves, under
            // the label captured when it was retained.
            (OutboundBody::Datagram(emission), _) => {
                let repeated = emission.repeated();
                ctx.metrics.record_retransmit(
                    emission.repeat().ladder(),
                    repeated.method(),
                    repeated.code(),
                );
                let _ = ctx.txn.send_raw(emission.wire().0.to_vec(), dest).await;
            }
            // A response goes through its server transaction, whatever the mode
            // says: `Raw` is for requests, and the only raw path for response
            // bytes is a retained `Datagram` (ADR-0032 X3). The layer sends the
            // response's image verbatim, so what a rule retained from that
            // same image is what leaves here.
            (OutboundBody::Response(resp), mode) => {
                debug_assert!(
                    !matches!(mode, OutboundTxnMode::Raw),
                    "a raw response bypass is a retained datagram, never a re-serialized Response: {}",
                    eff.label
                );
                // The layer holds the To-tag to the bound one; a rule that
                // retained this image for a repeat must have composed it
                // right, or the repeat and the wire disagree.
                if let Ok(sent) = ctx.txn.send_response(resp.clone(), dest).await {
                    debug_assert!(
                        sent == *resp.image(),
                        "{}: the response left re-rendered under the bound To-tag",
                        eff.label
                    );
                }
            }
            (OutboundBody::Request(req), OutboundTxnMode::NewClient(kind)) => {
                let _ = ctx.txn.send_request(req.clone(), dest, *kind).await;
            }
            (OutboundBody::Request(req), OutboundTxnMode::Raw) => {
                // A CANCEL reuses its INVITE's branch, and the INVITE client txn
                // owns WHEN it may go on the wire (RFC 3261 §9.1: held until the
                // branch's first provisional, dropped at Timer B) — so it goes
                // through `send_request`, whose CANCEL path sends raw once the
                // txn allows it. Other raw requests (ACK) bypass the txn map.
                if req.method() == Method::Cancel {
                    let _ = ctx.txn.send_request(req.clone(), dest, TxnKind::Invite).await;
                } else {
                    let _ = ctx.txn.send_raw(req.image().to_vec(), dest).await;
                }
            }
            (OutboundBody::Request(req), OutboundTxnMode::ServerResponse) => {
                // A request tagged ServerResponse is a misuse; send raw as a fallback.
                let _ = ctx.txn.send_raw(req.image().to_vec(), dest).await;
            }
        }
    }
}
