//! The turn commit (ADR-0055 §3): the one tail of a call's turn. It persists
//! the landed record, reports the turn `Logged`, takes in the turn what never
//! leaves the node (timers, the CDR, metrics, re-entries), queues everything
//! the turn sends on the call's outbox behind the write that records it, and
//! ends the call once that outbox drained. No other router module reads a
//! hold target, pushes on an outbox or reports a turn mark.
//!
//! How a turn persists and ends is one [`CommitKind`], read from the landed
//! result and from where the call lives. A turn that removes the call
//! enqueues its CDR and logs its delete before any effect leaves
//! (ADR-0020 X2); one that removes it while it owes the caller the non-2xx
//! that answers her INVITE writes the ended body instead, and logs the delete
//! once its outbox drained ([`super::owed_final`]). A takeover copy's
//! terminal defers its discharge to the live primary (ADR-0020 X3).

mod turn_entry;

use std::sync::Arc;

use b2bua_sdk::event::CallEvent;
use call::{Call, CallModelState};
use sip_message::{Method, SipMessage, SipRequest};

use super::departure::primary_departed;
use super::release::{release_call, ReleaseKind};
use super::turn_mark::{reach, TurnKind};
use super::RouterCtx;
use crate::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, FireAndForgetEffect, HandlerResult,
    QuietTurn, SoftBoundedEffect,
};
use crate::turn_probe::TurnPoint;
use turn_entry::TurnEntry;
pub(super) use turn_entry::{HeldCancel, Rider};

/// How a landed turn persists the call and ends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitKind {
    /// Replaces the live copy and writes nothing: the backup keeps the last
    /// write that changed the call.
    Quiet(QuietTurn),
    /// Replaces the live copy of a terminated call the turn does not remove,
    /// logging nothing.
    Update,
    /// Writes the record: `Terminating` too, whose teardown a reclaim racing
    /// the in-flight BYE resumes from the written `our_cseq`.
    Write,
    /// Ends the call: its CDR enqueued and its delete logged before any
    /// effect leaves; released once its outbox drained.
    DeleteBeforeSend,
    /// Ends the call owing its caller the INVITE's non-2xx: the ended body
    /// written, the delete logged once the outbox drained and the final left.
    DeleteAfterDrain,
    /// A takeover copy's terminal while its primary lives: the body written
    /// back to the primary, which discharges it; the wire it owes leaves from
    /// here, then the copy releases itself.
    DeferToPrimary,
}

/// What the landed result states about its persist and its end.
#[derive(Clone, Copy, Debug)]
struct Landed {
    quiet: Option<QuietTurn>,
    state: CallModelState,
    removes: bool,
    /// The turn sends something and the caller's INVITE holds a rejection.
    sends_a_rejection: bool,
}

impl Landed {
    fn of(result: &HandlerResult) -> Self {
        Self {
            quiet: crate::rule_chain::quiet_turn::class(result),
            state: result.call.state,
            removes: result
                .effects
                .critical
                .iter()
                .any(|e| matches!(e, CriticalStateEffect::RemoveCall)),
            sends_a_rejection: !result.effects.outbound.is_empty()
                && result.call.a_leg.invite_rejection().is_some(),
        }
    }
}

/// Where the call lives: a takeover copy, and whether its primary departed.
#[derive(Clone, Copy, Debug)]
struct Residency {
    takeover: bool,
    primary_departed: bool,
}

impl Residency {
    fn of(ctx: &RouterCtx, call_ref: &str) -> Self {
        let takeover = ctx.state.is_takeover(call_ref);
        Self { takeover, primary_departed: takeover && primary_departed(ctx, call_ref) }
    }
}

/// The commit of a turn that landed `landed` on a call living as `residency`
/// states. A quiet turn is `Active` with no critical effect but its own rung
/// ([`crate::rule_chain::quiet_turn::class`]), so it neither ends the call nor
/// a takeover. A copy whose primary departed discharges the call itself
/// (ADR-0047 D8), as a primary does.
fn classify(landed: Landed, residency: Residency) -> CommitKind {
    if let Some(kind) = landed.quiet {
        return CommitKind::Quiet(kind);
    }
    let terminated = landed.state == CallModelState::Terminated;
    if terminated && residency.takeover && !residency.primary_departed {
        return CommitKind::DeferToPrimary;
    }
    match (landed.removes, landed.sends_a_rejection, landed.state) {
        (true, true, _) => CommitKind::DeleteAfterDrain,
        (true, false, _) => CommitKind::DeleteBeforeSend,
        (false, _, CallModelState::Active | CallModelState::Terminating) => CommitKind::Write,
        (false, _, CallModelState::Terminated) => CommitKind::Update,
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

/// Commit the handler `result` of `call_ref`'s turn. `turn` names what the
/// turn handled, for a model-invariant violation, and `trigger` is the event
/// it handled, when it handled one.
pub(super) async fn commit_turn(
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
    let commit = classify(Landed::of(&result), Residency::of(ctx, call_ref));
    let cdr =
        result.effects.buffered.iter().any(|e| matches!(e, BufferedObservabilityEffect::WriteCdr));
    // The landed record is shared with the store from here on: what follows
    // only reads it.
    let HandlerResult { call, effects } = result;
    let call = Arc::new(call);
    let wrote = persist(ctx, call_ref, &call, commit, cdr, now_ms).await;
    let version = ctx.state.topology(call_ref).map_or((0, 0), |t| (t.gen, t.bak_gen));
    reach(ctx, call_ref, TurnPoint::Logged, version, wrote, kind, None).await;
    super::quiesced::rearm_after_answer(ctx, call_ref, awaited_before, &call).await;

    if commit == CommitKind::DeferToPrimary {
        // What the copy owes the wire is not deferred: the final that ends the
        // caller's INVITE and the hop-by-hop ACK toward the callee leave from
        // the node that took the event, or the peers wedge on Timer B / Timer
        // H. A primary read dead is not there to release the call's keys:
        // they leave with the wire, and the CDR still waits for the primary.
        let limiter_releases = if super::dead_verdict::primary_read_dead(ctx, call_ref) {
            effects.soft.into_iter().map(|SoftBoundedEffect::ReleaseLimiter { key }| key).collect()
        } else {
            Vec::new()
        };
        observe(ctx, &call, &effects.buffered, commit, now_ms).await;
        let entry = TurnEntry::sending(ctx, call_ref, call, now_ms, effects.outbound);
        let mark = Some((version, wrote, kind));
        push(ctx, call_ref, TurnEntry { mark, ack, cancel, limiter_releases, ..entry }).await;
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
    observe(ctx, &call, &effects.buffered, commit, now_ms).await;
    // Sent SIP is liveness too (ADR-0020 X4): a turn that puts a message on
    // the wire stamps the ledger alongside received traffic, so the reaper
    // never preempts a teardown legitimately waiting on a slow peer. A
    // terminated result is being released and stamps nothing.
    if !effects.outbound.is_empty() && call.state != CallModelState::Terminated {
        ctx.state.touch(call_ref, now_ms);
    }
    let removes = matches!(commit, CommitKind::DeleteBeforeSend | CommitKind::DeleteAfterDrain);
    if removes {
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
        mark: Some((version, wrote, kind)),
        ..TurnEntry::sending(ctx, call_ref, call, now_ms, effects.outbound)
    };
    push(ctx, call_ref, entry).await;

    // A turn that ends the call releases it once its outbox drained: the call
    // stays resident and takes no turn until then.
    if removes {
        ctx.outboxes.drained(call_ref).await;
        if commit == CommitKind::DeleteAfterDrain {
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

/// Persist `call` as `commit` states, its CDR (`cdr`) enqueued before a
/// delete is logged (ADR-0020 X2: a kill then adds a second CDR, never loses
/// one). `true` when the turn's record was logged for the call's backup.
async fn persist(
    ctx: &RouterCtx,
    call_ref: &str,
    call: &Arc<Call>,
    commit: CommitKind,
    cdr: bool,
    now_ms: i64,
) -> bool {
    match commit {
        CommitKind::Quiet(kind) => {
            ctx.state.update_quiet(Arc::clone(call));
            ctx.metrics.record_quiet_turn(kind.kind());
            false
        }
        CommitKind::Update => {
            ctx.state.update(Arc::clone(call));
            false
        }
        CommitKind::Write | CommitKind::DeferToPrimary => ctx.state.write(Arc::clone(call)),
        CommitKind::DeleteAfterDrain => {
            if cdr {
                ctx.cdr.write(call, now_ms).await;
            }
            ctx.state.write(Arc::clone(call))
        }
        CommitKind::DeleteBeforeSend => {
            ctx.state.update(Arc::clone(call));
            let backed = ctx.state.backup_of(call_ref).is_some();
            if cdr {
                ctx.cdr.write(call, now_ms).await;
            }
            ctx.state.log_delete(call_ref);
            backed
        }
    }
}

/// Take the turn's buffered observability in: its CDR, unless the persist
/// enqueued it or the turn defers it to the primary, and its counters.
async fn observe(
    ctx: &RouterCtx,
    call: &Call,
    buffered: &[BufferedObservabilityEffect],
    commit: CommitKind,
    now_ms: i64,
) {
    let cdr_taken = matches!(
        commit,
        CommitKind::DeleteBeforeSend | CommitKind::DeleteAfterDrain | CommitKind::DeferToPrimary
    );
    for eff in buffered {
        match eff {
            BufferedObservabilityEffect::WriteCdr if cdr_taken => {}
            BufferedObservabilityEffect::WriteCdr => ctx.cdr.write(call, now_ms).await,
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
}

/// Commit `call`, the setup a CANCEL marked while its routing decision was
/// asked, with the release of the CANCEL's `held` answers behind its write:
/// the turn reports `Logged` and `Left` as a handler turn does.
pub(super) async fn commit_cancel(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    call: Call,
    held: HeldCancel,
) {
    let call = Arc::new(call);
    let wrote = ctx.state.write(Arc::clone(&call));
    let version = ctx.state.topology(call_ref).map_or((0, 0), |t| (t.gen, t.bak_gen));
    let kind = TurnKind::on_request();
    reach(ctx, call_ref, TurnPoint::Logged, version, wrote, kind, None).await;
    let now_ms = ctx.clock.now_ms();
    let entry = TurnEntry {
        cancel: Some(held),
        mark: Some((version, wrote, kind)),
        ..TurnEntry::sending(ctx, call_ref, call, now_ms, Vec::new())
    };
    push(ctx, call_ref, entry).await;
}

/// Queue `rider` on `call_ref`'s outbox behind the call's latest write: what
/// the router sends for a turn that adds no record of its own, reporting no
/// turn mark.
pub(super) async fn behind_the_write(ctx: &Arc<RouterCtx>, call_ref: &str, rider: Rider) {
    let sender = ctx.clone();
    let wait = ctx.state.hold_target(call_ref);
    let send: super::outbox::Effects =
        Box::new(move |_| Box::pin(async move { rider.send(&sender).await }));
    ctx.outboxes.push(call_ref, wait, send).await;
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
        held.release(ctx).await;
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

/// The held CANCEL of the INVITE `invite`.
pub(super) fn held_cancel_of(invite: &SipRequest) -> HeldCancel {
    HeldCancel {
        call_id: invite.call_id().as_str().to_string(),
        from_tag: invite.from().tag().unwrap_or_default().to_string(),
        cseq: invite.cseq().seq(),
    }
}

#[cfg(test)]
mod tests;
