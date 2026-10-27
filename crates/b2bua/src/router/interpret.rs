//! The effect interpreter: persist the handler result, then take in the turn
//! what never leaves the node (timers, the CDR, metrics, re-entries) and queue
//! everything else the turn sends on the call's outbox, released by the
//! replication sync level ([`super::outbox`]). A turn that removes the call
//! enqueues its CDR and logs its delete at the persist step, before any effect
//! leaves (ADR-0020 X2, ADR-0055 §3), and evicts the call once its outbox
//! drained.

use std::sync::Arc;

use call::CallModelState;
use sip_message::{Method, SipRequest};

use super::release::{release_call, ReleaseKind};
use super::turn_entry::TurnEntry;
use super::RouterCtx;
use crate::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, FireAndForgetEffect, HandlerResult,
    SoftBoundedEffect,
};
use crate::turn_probe::{Released, TurnMark, TurnPoint};

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
    // the live copy without a bump and writes nothing: the armed rung
    // advances, the backup keeps the last write that changed the call.
    //
    // A takeover copy that reaches Terminated defers the discharge to its live
    // primary (ADR-0020 X3, ADR-0055 §5): it reverse-flushes the terminal body,
    // which the primary's reconcile discharges, then self-releases its live
    // copy, writing no CDR, releasing no limiter hold, propagating no delete. A
    // primary that departs first leaves the body to the departure sweep, one
    // that neither returns nor departs to the reap. A copy whose primary has
    // already departed (ADR-0047 D8) discharges the call below, as does a
    // primary-served terminal.
    //
    // Every other written turn lands in the change log while the call has a
    // backup. `Terminating` writes too: a teardown in progress carries the
    // b-leg `ByeSent` disposition and its bumped `our_cseq`, without which a
    // reclaim racing the in-flight BYE restarts termination at a reused CSeq.
    // A turn that removes the call writes its delete instead, after its CDR is
    // enqueued (ADR-0020 X2): a kill then adds a second CDR, never loses one.
    let quiet = crate::rule_chain::quiet_turn::class(&result);
    let takeover_end = result.call.state == CallModelState::Terminated
        && ctx.state.is_takeover(call_ref)
        && !primary_departed(ctx, call_ref);
    let remove_call = !takeover_end
        && result.effects.critical.iter().any(|e| matches!(e, CriticalStateEffect::RemoveCall));
    let wrote = match quiet {
        Some(kind) => {
            ctx.state.update_quiet(result.call.clone());
            ctx.metrics.record_quiet_turn(kind.kind());
            false
        }
        None if remove_call => {
            ctx.state.update(result.call.clone());
            let backed = ctx.state.backup_of(call_ref).is_some();
            if has_cdr(&result) {
                ctx.cdr.write(&result.call, now_ms).await;
            }
            ctx.state.log_delete(call_ref);
            backed
        }
        None if takeover_end
            || matches!(
                result.call.state,
                CallModelState::Active | CallModelState::Terminating
            ) =>
        {
            ctx.state.write(&result.call)
        }
        None => {
            ctx.state.update(result.call.clone());
            false
        }
    };
    let version = ctx.state.topology(call_ref).map_or((0, 0), |t| (t.gen, t.bak_gen));
    reach(ctx, call_ref, TurnPoint::Logged, version, wrote, kind, None).await;

    // Timers are armed in the turn; what the turn sends off the node leaves
    // through the call's outbox once the write that records it is applied
    // (ADR-0055 §3). ADR-0039 deadlines carry a margin over the cap.
    let HandlerResult { call, effects } = result;
    if takeover_end {
        // What the copy owes the WIRE is not deferred: the final that ends the
        // caller's INVITE and the hop-by-hop ACK toward the callee leave from
        // the node that took the event, or the peers wedge on Timer B / Timer
        // H. It leaves once the primary applied the terminal body, and the
        // copy is released after it.
        let entry = TurnEntry::sending(ctx, call_ref, call, now_ms, effects.outbound);
        let entry = TurnEntry { mark: (version, wrote, kind), ..entry };
        push(ctx, call_ref, entry).await;
        ctx.outboxes.drained(call_ref).await;
        release_call(ctx, call_ref, ReleaseKind::SelfRelease { ended: true }).await;
        return;
    }
    let mut relinquish = Vec::new();
    for eff in effects.critical {
        match eff {
            CriticalStateEffect::ScheduleTimer(entry) => {
                if crate::trace::sampled(&call) {
                    crate::trace::emit::timer_arm(&call, now_ms, &entry);
                }
                let incarnation = Some(call.incarnation().to_string());
                ctx.timers.schedule(entry, call_ref.to_string(), incarnation).await;
            }
            CriticalStateEffect::CancelTimer { key } => {
                if crate::trace::sampled(&call) {
                    crate::trace::emit::timer_cancel(&call, now_ms, &key);
                }
                ctx.timers.cancel(call_ref.to_string(), key).await
            }
            CriticalStateEffect::CancelAllTimers => {
                ctx.timers.cancel_all(call_ref.to_string()).await
            }
            CriticalStateEffect::RemoveCall => {}
            CriticalStateEffect::RelinquishAnsweredInvite { via } => relinquish.push(via),
        }
    }
    for eff in &effects.buffered {
        match eff {
            // Enqueued before the call's delete.
            BufferedObservabilityEffect::WriteCdr if remove_call => {}
            BufferedObservabilityEffect::WriteCdr => ctx.cdr.write(&call, now_ms).await,
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
    // Sent SIP is liveness too (ADR-0020 X4 refinement): a turn that puts a
    // message on the wire (a keepalive OPTIONS, a relayed response, a teardown
    // BYE/CANCEL) stamps the ledger alongside received traffic, so the reaper
    // never preempts a teardown that is legitimately waiting on a slow peer.
    // Wire-silent turns stamp nothing; a terminated result is being released
    // and stamps nothing either.
    if !effects.outbound.is_empty() && call.state != CallModelState::Terminated {
        ctx.state.touch(call_ref, now_ms);
    }
    if remove_call {
        ctx.late_pracks.remember(&call, now_ms);
    }
    let incarnation = call.incarnation().to_string();
    let (reentries, callouts): (Vec<_>, Vec<_>) = effects
        .fire_and_forget
        .into_iter()
        .partition(|e| matches!(e, FireAndForgetEffect::Reenter(_)));
    // A re-INVITE's first provisional follows the write that records it.
    let trying = received.filter(|r| *r.method() == Method::Invite && r.to().tag().is_some());
    let entry = TurnEntry {
        trying: trying.cloned(),
        relinquish,
        limiter_releases: effects
            .soft
            .into_iter()
            .map(|SoftBoundedEffect::ReleaseLimiter { key }| key)
            .collect(),
        callouts,
        mark: (version, wrote, kind),
        ..TurnEntry::sending(ctx, call_ref, call, now_ms, effects.outbound)
    };
    push(ctx, call_ref, entry).await;

    // A turn that ends the call releases it once its outbox drained: the call
    // stays resident and takes no turn until then (ADR-0055 §3).
    if remove_call {
        ctx.outboxes.drained(call_ref).await;
        release_call(ctx, call_ref, ReleaseKind::Terminated).await;
    }
    for eff in reentries {
        if let FireAndForgetEffect::Reenter(ev) = eff {
            let _ = ctx.reentry_tx.send(ev.stamped(&incarnation));
        }
    }
}

/// Queue `entry` on `call_ref`'s outbox, waiting for the call's current
/// change-log entry.
async fn push(ctx: &Arc<RouterCtx>, call_ref: &str, entry: TurnEntry) {
    let wait = ctx.state.hold_target(call_ref);
    ctx.outboxes.push(call_ref, wait, Box::new(|released| Box::pin(entry.run(released)))).await;
}

/// Whether the turn's result enqueues the call's CDR.
fn has_cdr(result: &HandlerResult) -> bool {
    result.effects.buffered.iter().any(|e| matches!(e, BufferedObservabilityEffect::WriteCdr))
}

/// What a turn was taken on and what it started, as its turn marks report.
#[derive(Clone, Copy, Default)]
pub(super) struct TurnKind {
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

/// Report the turn of `call_ref` at `point` to the turn-point seam, at the
/// write counters `version` the turn left (ADR-0055 §13).
pub(super) async fn reach(
    ctx: &RouterCtx,
    call_ref: &str,
    point: TurnPoint,
    version: (i64, i64),
    wrote: bool,
    kind: TurnKind,
    released: Option<Released>,
) {
    ctx.turn_probe
        .at(|| TurnMark {
            node: &ctx.config.self_ordinal,
            call_ref,
            point,
            version,
            wrote,
            removed: false,
            on_request: kind.on_request,
            called_out: kind.called_out,
            released,
        })
        .await;
}
