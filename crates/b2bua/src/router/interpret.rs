//! The effect interpreter: persist the handler result, then run its typed
//! effects in the fixed order persist → critical → outbound → soft → buffered,
//! with fire-and-forget callouts detached at the end. A turn that removes the
//! call enqueues its CDR and logs its delete at the persist step, before any
//! effect leaves (ADR-0020 X2, ADR-0055 §3), and evicts the call last.

use std::sync::Arc;

use call::CallModelState;
use call::TxnKind;
use sip_message::{Method, SipRequest};

use super::callouts;
use super::release::{release_call, ReleaseKind};
use super::RouterCtx;
use crate::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, FireAndForgetEffect, HandlerResult,
    OutboundBody, OutboundTxnMode, SoftBoundedEffect,
};
use crate::turn_probe::{TurnMark, TurnPoint};

/// Whether `call_ref`'s primary is another node that has departed.
pub(super) fn primary_departed(ctx: &RouterCtx, call_ref: &str) -> bool {
    match (&ctx.departed, call::call_ref_primary(call_ref)) {
        (Some(departed), Some(primary)) => primary != ctx.config.self_ordinal && departed(primary),
        _ => false,
    }
}

/// The budgets this node answers the requests sent off a call's turn by: the
/// adaptation port's default, the limiter's admit budget, and a
/// `/call/failure` consult's chain under the decision deadline.
fn answer_budgets(ctx: &RouterCtx) -> crate::answer_deadline::AnswerBudgets {
    let admit = ctx.limiter.admit_budget();
    crate::answer_deadline::AnswerBudgets {
        http: ctx.adaptation_http.as_ref().map_or(std::time::Duration::ZERO, |p| p.default_timeout),
        admit,
        failure: crate::answer_deadline::failure_budget_under(&ctx.config, admit),
        release: crate::answer_deadline::release_budget_under(&ctx.config, admit),
    }
}

/// Interpret a handler result: persist → critical → outbound → soft → buffered.
/// `turn` names what the turn handled, for a model-invariant violation, and
/// `received` is the request it handled, when its event is one.
pub(super) async fn process_result(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    result: HandlerResult,
    now_ms: i64,
    turn: &(dyn std::fmt::Display + Sync),
    received: Option<&SipRequest>,
) {
    let landing = crate::landing::Landing {
        config: &ctx.config,
        budgets: answer_budgets(ctx),
        refresh_period: ctx.limiter.refresh_period(),
    };
    let result = crate::landing::land(result, &landing, now_ms);
    let kind = TurnKind::of(&result, received);
    // Stated text the turn's messages left out is a defect of the model that
    // stated it, reported whichever node sends them.
    for eff in &result.effects.buffered {
        if let BufferedObservabilityEffect::StatedTextSkipped { detail } = eff {
            crate::model_invariants::stated_text_skipped(&ctx.metrics, &result.call, detail);
        }
    }
    // The record this turn lands states the model invariants, against the
    // one the previous turn landed.
    if ctx.config.model_invariants {
        let before = ctx.state.peek(call_ref);
        let turn_record = crate::model_invariants::Turn {
            before: before.as_ref(),
            after: &result.call,
            received,
            sent: &result.effects.outbound,
        };
        crate::model_invariants::check(&turn_record, turn, &ctx.metrics);
    }
    // Persist first (state lands before effects run). A quiet turn replaces
    // the live copy without a bump and skips the flush gate below: the armed
    // rung advances, the backup keeps the last write that changed the call.
    let quiet = crate::rule_chain::quiet_turn::class(&result);
    match quiet {
        Some(kind) => {
            ctx.state.update_quiet(result.call.clone());
            ctx.metrics.record_quiet_turn(kind.kind());
        }
        None => ctx.state.update(result.call.clone()),
    }

    // A takeover copy that reaches Terminated defers the discharge to its live
    // primary (ADR-0020 X3, ADR-0055 §5): it reverse-flushes the terminal body,
    // which the primary's reconcile discharges, then self-releases its live
    // copy, writing no CDR, releasing no limiter hold, propagating no delete. A
    // primary that departs first leaves the body to the departure sweep, one
    // that neither returns nor departs to the reap. A copy whose primary has
    // already departed (ADR-0047 D8) discharges the call below, as does a
    // primary-served terminal.
    if result.call.state == CallModelState::Terminated
        && ctx.state.is_takeover(call_ref)
        && !primary_departed(ctx, call_ref)
    {
        // Reverse-flush the Terminated body held with the normal replica TTL
        // (`reboot_budget`): a live primary meets it and forward-deletes it once
        // the write reaches it; a rebooting primary still has its full reclaim
        // window to meet and discharge it. A copy whose primary is a member
        // defers the discharge to it (ADR-0055 §5).
        ctx.state.flush(&result.call);
        reach(ctx, call_ref, TurnPoint::Logged, true, kind).await;
        // What the turn owes the WIRE is not deferred: the final that ends the
        // caller's INVITE and the hop-by-hop ACK toward the callee leave from the
        // node that took the event, or the peers wedge on Timer B / Timer H.
        emit_outbound(ctx, call_ref, &result, now_ms).await;
        reach(ctx, call_ref, TurnPoint::Left, true, kind).await;
        release_call(ctx, call_ref, ReleaseKind::SelfRelease { ended: true }).await;
        return;
    }

    // Replicate a non-terminated, backed-up call to its peer after each authoritative mutation
    // (flush-on-mutation), into the change log before any effect of the turn leaves (ADR-0055
    // §3). A call writes only while it has a backup: on a wired node, a backup peer named and no
    // refusal leaving it unbacked for life. A backed-up call routes through the write-side policy
    // (Forward when primary, Reverse when acting-backup) so the backup holds the latest state.
    //
    // `Terminating` MUST flush too, not just `Active`: a teardown-in-progress
    // carries authoritative state the replica needs — the b-leg `ByeSent`
    // disposition and its bumped `our_cseq`. Without it an acting-backup
    // whose primary crashed never propagates that progress; a reclaim racing
    // the in-flight BYE then pulls a STALE `Active` snapshot, restarts
    // termination, and re-sends the BYE at the *reused* CSeq a real UAS drops
    // (matrix cells C7/RFC). A turn that removes the call writes its delete
    // instead, after its CDR is enqueued (ADR-0020 X2): a delete propagated
    // before the CDR would let a failure in that window erase the call
    // everywhere with no CDR.
    let remove_call =
        result.effects.critical.iter().any(|e| matches!(e, CriticalStateEffect::RemoveCall));
    let backed = ctx.state.backup_of(call_ref).is_some();
    let wrote = if remove_call {
        if has_cdr(&result) {
            ctx.cdr.write(&result.call, now_ms).await;
        }
        ctx.state.log_delete(call_ref);
        backed
    } else {
        let wrote = quiet.is_none()
            && matches!(result.call.state, CallModelState::Active | CallModelState::Terminating)
            && backed;
        if wrote {
            ctx.state.flush(&result.call);
        }
        wrote
    };
    reach(ctx, call_ref, TurnPoint::Logged, wrote, kind).await;
    // A re-INVITE's first provisional follows the write that records it.
    if let Some(reinvite) =
        received.filter(|r| *r.method() == Method::Invite && r.to().tag().is_some())
    {
        super::hold::first_provisional(ctx, call_ref, reinvite).await;
    }

    // The terminal eviction runs LAST, its delete already logged above: the
    // call is unreachable for new work (its state is persisted and terminal),
    // and its transactions are torn down once this turn's answers are sent.
    for eff in &result.effects.critical {
        match eff {
            CriticalStateEffect::ScheduleTimer(entry) => {
                if crate::trace::sampled(&result.call) {
                    crate::trace::emit::timer_arm(&result.call, now_ms, entry);
                }
                let incarnation = Some(result.call.incarnation().to_string());
                ctx.timers.schedule(entry.clone(), call_ref.to_string(), incarnation).await;
            }
            CriticalStateEffect::CancelTimer { key } => {
                if crate::trace::sampled(&result.call) {
                    crate::trace::emit::timer_cancel(&result.call, now_ms, key);
                }
                ctx.timers.cancel(call_ref.to_string(), key.clone()).await
            }
            CriticalStateEffect::CancelAllTimers => {
                ctx.timers.cancel_all(call_ref.to_string()).await
            }
            CriticalStateEffect::RemoveCall => {}
            CriticalStateEffect::RelinquishAnsweredInvite { via } => {
                if let Some(branch) = via.branch() {
                    let key = sip_txn::ServerTxnKey::invite(
                        branch.to_string(),
                        sip_message::header::SentBy::of(via),
                    );
                    let _ = ctx.txn.relinquish_server(key).await;
                }
            }
        }
    }

    emit_outbound(ctx, call_ref, &result, now_ms).await;
    reach(ctx, call_ref, TurnPoint::Left, wrote, kind).await;

    for eff in &result.effects.soft {
        match eff {
            SoftBoundedEffect::ReleaseLimiter { key } => {
                ctx.limiter.release(key);
                if crate::trace::sampled(&result.call) {
                    crate::trace::emit::limiter(&result.call, now_ms, "release queued", key);
                }
            }
        }
    }

    for eff in &result.effects.buffered {
        match eff {
            // Enqueued before the call's delete.
            BufferedObservabilityEffect::WriteCdr if remove_call => {}
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
            // Reported as the turn landed.
            BufferedObservabilityEffect::StatedTextSkipped { .. } => {}
        }
    }

    // Terminal eviction last of all: the CDR and the delete were written before
    // the turn's effects left.
    if remove_call {
        ctx.late_pracks.remember(&result.call, now_ms);
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
                        to: callouts::Caller::of(call_ref, &result.call),
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
            FireAndForgetEffect::FailureAsyncHttp { call_ref, request, limiter, deadline } => {
                callouts::spawn_failure_callout(
                    ctx,
                    &result.call,
                    call_ref,
                    request,
                    limiter,
                    deadline,
                );
            }
            FireAndForgetEffect::ReleaseAsyncHttp { call_ref, request, limiter, deadline } => {
                callouts::spawn_release_callout(
                    ctx,
                    &result.call,
                    call_ref,
                    request,
                    limiter,
                    deadline,
                );
            }
            FireAndForgetEffect::LimiterAdmit {
                call_ref,
                correlation_id,
                key,
                change,
                held,
                entries,
            } => {
                callouts::spawn_limiter_admit_callout(
                    ctx,
                    callouts::LimiterAdmitCallout {
                        call_ref,
                        correlation_id,
                        key,
                        change,
                        held,
                        entries,
                    },
                );
            }
            FireAndForgetEffect::Reenter(ev) => {
                let _ = ctx.reentry_tx.send(ev.stamped(result.call.incarnation()));
            }
        }
    }
}

/// Whether the turn's result enqueues the call's CDR.
fn has_cdr(result: &HandlerResult) -> bool {
    result.effects.buffered.iter().any(|e| matches!(e, BufferedObservabilityEffect::WriteCdr))
}

/// Report the turn of `call_ref` at `point` to the turn-point seam, with the
/// write counters the live copy holds (ADR-0055 §13).
/// What a turn was taken on and what it started, as its turn marks report.
#[derive(Clone, Copy)]
struct TurnKind {
    on_request: bool,
    called_out: bool,
}

impl TurnKind {
    fn of(result: &HandlerResult, received: Option<&SipRequest>) -> Self {
        let called_out = result
            .effects
            .fire_and_forget
            .iter()
            .any(|e| !matches!(e, FireAndForgetEffect::Reenter(_)));
        Self { on_request: received.is_some(), called_out }
    }
}

async fn reach(ctx: &RouterCtx, call_ref: &str, point: TurnPoint, wrote: bool, kind: TurnKind) {
    let version = || ctx.state.topology(call_ref).map_or((0, 0), |t| (t.gen, t.bak_gen));
    ctx.turn_probe
        .at(|| TurnMark {
            node: &ctx.config.self_ordinal,
            call_ref,
            point,
            version: version(),
            wrote,
            removed: false,
            on_request: kind.on_request,
            called_out: kind.called_out,
        })
        .await;
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
        // A held message leaves before any later message of its dialog (RFC
        // 3261 §14.1), this turn's included.
        match &eff.body {
            OutboundBody::Request(req) => {
                super::hold::release_dialog(ctx, call_ref, &req.call_id().to_string()).await
            }
            OutboundBody::Response(resp) => {
                super::hold::release_dialog(ctx, call_ref, &resp.call_id().to_string()).await
            }
            OutboundBody::Datagram(_) => super::hold::release(ctx, call_ref).await,
        }
        let Some(dest) = sip_net::ip_socket_addr(&eff.destination.0, eff.destination.1) else {
            super::unaddressable::fail_unaddressable(ctx, call_ref, result, eff);
            continue;
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
            // server txn that sent the original is `Accepted` and would drop a
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
            // The final to a non-INVITE request is held for the call's backup
            // (ADR-0055 §3): it stops its sender retransmitting.
            (OutboundBody::Response(resp), _)
                if resp.status() >= 200 && resp.cseq().method() != Method::Invite =>
            {
                super::hold::non_invite_final(ctx, call_ref, resp, dest).await;
            }
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
                // txn allows it. An ACK goes through it too, raw: the Accepted
                // INVITE client txn of its 2xx keeps it, and replays it to a
                // repeat of that 2xx once this call is released (RFC 6026 §7.2).
                // The ACK of a 2xx is held for the call's backup (ADR-0055 §3).
                if *req.method() == Method::Ack {
                    super::hold::ack(ctx, call_ref, req, dest).await;
                } else if *req.method() == Method::Cancel {
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
