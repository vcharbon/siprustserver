//! The effect interpreter: persist the handler result, then take in the turn
//! what never leaves the node (timers, the CDR, metrics, re-entries) and queue
//! everything else the turn sends on the call's outbox, released by the
//! replication sync level ([`super::outbox`]). A turn that removes the call
//! enqueues its CDR and logs its delete at the persist step, before any effect
//! leaves (ADR-0020 X2, ADR-0055 §3), and evicts the call once its outbox
//! drained. A turn that removes the call while it owes the caller the non-2xx
//! that answers her INVITE writes the ended body instead, and logs the delete
//! once its outbox drained: the backup keeps the final until it left
//! ([`super::owed_final`]).

use std::sync::Arc;

use b2bua_sdk::event::CallEvent;
use call::CallModelState;
use sip_message::{Method, SipMessage, SipRequest};

use super::release::{release_call, ReleaseKind};
use super::turn_entry::{HeldCancel, TurnEntry};
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
/// `trigger` is the event it handled, when it handled one.
pub(super) async fn process_result(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    result: HandlerResult,
    now_ms: i64,
    turn: &(dyn std::fmt::Display + Sync),
    trigger: Option<&CallEvent>,
) {
    let message = trigger.and_then(|e| match e {
        CallEvent::Sip { message, .. } => Some(message.as_ref()),
        _ => None,
    });
    let received = message.and_then(|m| match m {
        SipMessage::Request(r) => Some(r),
        SipMessage::Response(_) => None,
    });
    let ack = message.and_then(held_ack_branch);
    let cancel = trigger.and_then(held_cancel_answers);
    let landing = crate::landing::Landing {
        config: &ctx.config,
        budgets: answer_budgets(ctx),
        refresh_period: ctx.limiter.refresh_period(),
    };
    let result = crate::landing::land(result, &landing, now_ms);
    let kind = TurnKind::of(&result, received);
    let awaited_before = ctx.state.resident(call_ref).is_some_and(|c| !c.awaited.is_empty());
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
        let before = ctx.state.resident(call_ref);
        let turn_record = crate::model_invariants::Turn {
            before: before.as_deref(),
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
    let owes_final = remove_call
        && !result.effects.outbound.is_empty()
        && result.call.a_leg.invite_rejection().is_some();
    let cdr = has_cdr(&result);
    // The landed record is shared with the store from here on: what follows
    // only reads it.
    let HandlerResult { call, effects } = result;
    let call = Arc::new(call);
    let wrote = match quiet {
        Some(kind) => {
            ctx.state.update_quiet(Arc::clone(&call));
            ctx.metrics.record_quiet_turn(kind.kind());
            false
        }
        None if remove_call && owes_final => {
            if cdr {
                ctx.cdr.write(&call, now_ms).await;
            }
            ctx.state.write(Arc::clone(&call))
        }
        None if remove_call => {
            ctx.state.update(Arc::clone(&call));
            let backed = ctx.state.backup_of(call_ref).is_some();
            if cdr {
                ctx.cdr.write(&call, now_ms).await;
            }
            ctx.state.log_delete(call_ref);
            backed
        }
        None if takeover_end
            || matches!(call.state, CallModelState::Active | CallModelState::Terminating) =>
        {
            ctx.state.write(Arc::clone(&call))
        }
        None => {
            ctx.state.update(Arc::clone(&call));
            false
        }
    };
    let version = ctx.state.topology(call_ref).map_or((0, 0), |t| (t.gen, t.bak_gen));
    reach(ctx, call_ref, TurnPoint::Logged, version, wrote, kind, None).await;
    super::quiesced::rearm_after_answer(ctx, call_ref, awaited_before, &call).await;

    // Timers are armed in the turn; what the turn sends off the node leaves
    // through the call's outbox once the write that records it is applied
    // (ADR-0055 §3). ADR-0039 deadlines carry a margin over the cap.
    if takeover_end {
        // What the copy owes the WIRE is not deferred: the final that ends the
        // caller's INVITE and the hop-by-hop ACK toward the callee leave from
        // the node that took the event, or the peers wedge on Timer B / Timer
        // H. It leaves once the primary applied the terminal body, and the
        // copy is released after it.
        let entry = TurnEntry::sending(ctx, call_ref, call, now_ms, effects.outbound);
        let entry = TurnEntry { mark: (version, wrote, kind), ack, cancel, ..entry };
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
        ack,
        cancel,
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
        if owes_final {
            ctx.state.log_delete(call_ref);
        }
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

/// Release what the layer holds for `event`'s turn — the ACK of a non-2xx
/// final, the answers to a CANCEL — when no turn's outbox entry took it: no
/// write records it, so nothing is waited for.
pub(super) async fn release_untaken(ctx: &RouterCtx, event: &CallEvent) {
    let held_ack = match event {
        CallEvent::Sip { message, .. } => held_ack_branch(message),
        _ => None,
    };
    if let Some(branch) = held_ack {
        let _ = ctx.txn.release_ack(&branch).await;
    }
    if let Some(held) = held_cancel_answers(event) {
        let _ = ctx.txn.release_cancel(&held.call_id, &held.from_tag, held.cseq).await;
    }
}

/// The branch of the client INVITE transaction whose non-2xx final `trigger`
/// is: the layer holds its ACK until the turn's entry releases it.
fn held_ack_branch(trigger: &SipMessage) -> Option<String> {
    let SipMessage::Response(resp) = trigger else { return None };
    if *resp.cseq().method() != Method::Invite || resp.status() < 300 {
        return None;
    }
    resp.top_via().branch().filter(|b| !b.is_empty()).map(str::to_string)
}

/// The CANCEL `trigger` reports: the layer holds the 200 to it and the 487
/// to the INVITE until the turn's entry releases them.
fn held_cancel_answers(trigger: &CallEvent) -> Option<HeldCancel> {
    let CallEvent::Cancelled { call_id, from_tag, invite_cseq, .. } = trigger else {
        return None;
    };
    let (call_id, from_tag) = (call_id.clone(), from_tag.clone());
    Some(HeldCancel { call_id, from_tag, cseq: (*invite_cseq)? })
}

/// Whether the turn's result enqueues the call's CDR.
fn has_cdr(result: &HandlerResult) -> bool {
    result.effects.buffered.iter().any(|e| matches!(e, BufferedObservabilityEffect::WriteCdr))
}

/// What a turn was taken on and what it started, as its turn marks report.
#[derive(Clone, Copy, Default)]
pub(super) struct TurnKind {
    on_request: bool,
    on_answer: bool,
    called_out: bool,
    resent: bool,
    reasked: bool,
}

impl TurnKind {
    fn of(result: &HandlerResult, received: Option<&SipRequest>) -> Self {
        let called_out = result
            .effects
            .fire_and_forget
            .iter()
            .any(|e| !matches!(e, FireAndForgetEffect::Reenter(_)));
        let on_answer = result.effects.took_answer;
        let (resent, reasked) = (result.effects.resent, result.effects.reasked);
        Self { on_request: received.is_some(), on_answer, called_out, resent, reasked }
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
            on_answer: kind.on_answer,
            called_out: kind.called_out,
            resent: kind.resent,
            reasked: kind.reasked,
            released,
            backed: point == TurnPoint::Logged && ctx.state.hold_target(call_ref).is_some(),
        })
        .await;
}
