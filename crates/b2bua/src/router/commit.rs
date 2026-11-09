//! The turn commit (ADR-0055 §3): the one tail of a call's turn. It persists
//! the landed record, reports the turn `Logged`, takes in the turn what never
//! leaves the node (timers, the CDR, metrics, re-entries), queues everything
//! the turn sends on the call's outbox behind the write that records it, and
//! ends the call once that outbox drained, as its [`CommitKind`] states. Only
//! the commit pushes on a call's outbox and reports a turn's marks. A turn on
//! the body a poll watchdog read stuck lands nothing from the step it reaches
//! after its call's discharge (ADR-0057 §6).

mod held;
mod rider;
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
pub(super) use held::release_untaken;
use held::HeldCancel;
pub(super) use rider::{behind_the_write, Rider};
use turn_entry::TurnEntry;

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
    /// effect leaves (ADR-0020 X2: a kill then adds a second CDR, never loses
    /// one); released once its outbox drained.
    DeleteBeforeSend,
    /// Ends the call owing its caller the INVITE's non-2xx: the ended body
    /// written with its CDR enqueued, the delete logged once the outbox
    /// drained and the final left ([`super::owed_final`]).
    DeleteAfterDrain,
    /// A takeover copy's terminal while its primary lives (ADR-0020 X3): the
    /// body written back to the primary, which discharges it with its CDR;
    /// the wire the copy owes leaves from here, then the copy releases itself.
    /// A primary that departs first leaves the body to the departure sweep,
    /// one that neither returns nor departs to the reap.
    DeferToPrimary,
}

impl CommitKind {
    /// The turn ends the call here.
    fn removes(self) -> bool {
        matches!(self, Self::DeleteBeforeSend | Self::DeleteAfterDrain)
    }
}

/// What the landed result states about its persist and its end.
#[derive(Clone, Copy)]
struct Landed {
    quiet: Option<QuietTurn>,
    state: CallModelState,
    removes: bool,
    /// The turn sends something and the caller's INVITE holds a rejection.
    sends_a_rejection: bool,
}

impl Landed {
    fn of(result: &HandlerResult) -> Self {
        let fx = &result.effects;
        Self {
            quiet: crate::rule_chain::quiet_turn::class(result),
            state: result.call.state,
            removes: fx.critical.iter().any(|e| matches!(e, CriticalStateEffect::RemoveCall)),
            sends_a_rejection: !fx.outbound.is_empty()
                && result.call.a_leg.invite_rejection().is_some(),
        }
    }
}

/// Where the call lives.
#[derive(Clone, Copy)]
enum Residency {
    Primary,
    /// A takeover copy whose primary lives.
    Copy,
    /// A takeover copy whose primary departed: it ends the call itself, as a
    /// primary does (ADR-0047 D8).
    OrphanedCopy,
}

impl Residency {
    fn of(ctx: &RouterCtx, call_ref: &str) -> Self {
        if !ctx.state.is_takeover(call_ref) {
            Self::Primary
        } else if primary_departed(ctx, call_ref) {
            Self::OrphanedCopy
        } else {
            Self::Copy
        }
    }
}

/// The commit of a turn that landed `landed` on a call living as `residency`
/// states, read only for a terminated record. A quiet turn is `Active` with
/// no critical effect but its own rung
/// ([`crate::rule_chain::quiet_turn::class`]), so it ends neither the call
/// nor a takeover.
fn classify(landed: Landed, residency: impl FnOnce() -> Residency) -> CommitKind {
    if let Some(kind) = landed.quiet {
        return CommitKind::Quiet(kind);
    }
    if landed.state == CallModelState::Terminated && matches!(residency(), Residency::Copy) {
        return CommitKind::DeferToPrimary;
    }
    match (landed.removes, landed.sends_a_rejection, landed.state) {
        (true, true, _) => CommitKind::DeleteAfterDrain,
        (true, false, _) => CommitKind::DeleteBeforeSend,
        (false, _, CallModelState::Active | CallModelState::Terminating) => CommitKind::Write,
        (false, _, CallModelState::Terminated) => CommitKind::Update,
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
    if fenced(ctx, call_ref) {
        return;
    }
    let message = trigger.and_then(|e| match e {
        CallEvent::Sip { message, .. } => Some(message.as_ref()),
        _ => None,
    });
    let received = message.and_then(|m| match m {
        SipMessage::Request(r) => Some(r),
        SipMessage::Response(_) => None,
    });
    let (ack, cancel) = trigger.map_or((None, None), held::of);
    let result = crate::landing::land(result, &ctx.landing(), now_ms);
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
    let commit = classify(Landed::of(&result), || Residency::of(ctx, call_ref));
    let cdr =
        result.effects.buffered.iter().any(|e| matches!(e, BufferedObservabilityEffect::WriteCdr));
    // The landed record is shared with the store from here on: what follows
    // only reads it.
    let HandlerResult { call, effects } = result;
    let call = Arc::new(call);
    if fenced(ctx, call_ref) {
        return;
    }
    let wrote = persist(ctx, call_ref, &call, commit, cdr, now_ms).await;
    let version = ctx.state.topology(call_ref).map_or((0, 0), |t| (t.gen, t.bak_gen));
    reach(ctx, call_ref, TurnPoint::Logged, version, wrote, kind, None).await;
    super::quiesced::rearm_after_answer(ctx, call_ref, awaited_before, &call).await;
    let mark = (version, wrote, kind);

    if commit == CommitKind::DeferToPrimary {
        // What the copy owes the wire is not deferred: the final that ends the
        // caller's INVITE and the hop-by-hop ACK toward the callee leave from
        // the node that took the event, or the peers wedge on Timer B / Timer
        // H. A primary read dead is not there to release the call's keys:
        // they leave with the wire, and the CDR still waits for the primary.
        let limiter_releases = if super::dead_verdict::primary_read_dead(ctx, call_ref) {
            limiter_keys(effects.soft)
        } else {
            Vec::new()
        };
        observe(ctx, &call, &effects.buffered, commit, now_ms).await;
        let entry = TurnEntry {
            ack,
            cancel,
            limiter_releases,
            ..TurnEntry::new(ctx, call_ref, call, now_ms, effects.outbound, mark)
        };
        if fenced(ctx, call_ref) {
            return;
        }
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
    observe(ctx, &call, &effects.buffered, commit, now_ms).await;
    // Sent SIP is liveness too (ADR-0020 X4): a turn that puts a message on
    // the wire stamps the ledger alongside received traffic, so the reaper
    // never preempts a teardown legitimately waiting on a slow peer. A
    // terminated result is being released and stamps nothing.
    if !effects.outbound.is_empty() && call.state != CallModelState::Terminated {
        ctx.state.touch(call_ref, now_ms);
    }
    if commit.removes() {
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
        limiter_releases: limiter_keys(effects.soft),
        callouts,
        ..TurnEntry::new(ctx, call_ref, call, now_ms, effects.outbound, mark)
    };
    if fenced(ctx, call_ref) {
        return;
    }
    push(ctx, call_ref, entry).await;
    if fenced(ctx, call_ref) {
        return;
    }

    // A turn that ends the call releases it once its outbox drained: the call
    // stays resident and takes no turn until then.
    if commit.removes() {
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

/// Persist `call` as `commit` states, its CDR enqueued (`cdr`) where the
/// commit ends the call. `true` when the record was logged for the call's
/// backup.
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

/// Whether a turn committed as `commit` takes `eff` in itself. A turn that
/// ends the call enqueued its CDR at the persist; a copy deferring to its
/// primary leaves the CDR and the unrecorded termination to the primary's
/// discharge, which counts it. Stated text skipped is reported as the turn
/// landed.
fn takes(commit: CommitKind, eff: &BufferedObservabilityEffect) -> bool {
    let deferred = commit == CommitKind::DeferToPrimary;
    match eff {
        BufferedObservabilityEffect::WriteCdr => !commit.removes() && !deferred,
        BufferedObservabilityEffect::TerminationUnrecorded => !deferred,
        BufferedObservabilityEffect::StatedTextSkipped { .. } => false,
        BufferedObservabilityEffect::SecondFinalRefused { .. }
        | BufferedObservabilityEffect::ProvisionalAfterFinalRefused { .. }
        | BufferedObservabilityEffect::GoingAwayAbsorbed { .. } => true,
    }
}

/// Take in the buffered observability a turn committed as `commit` takes.
async fn observe(
    ctx: &RouterCtx,
    call: &Call,
    buffered: &[BufferedObservabilityEffect],
    commit: CommitKind,
    now_ms: i64,
) {
    for eff in buffered.iter().filter(|eff| takes(commit, eff)) {
        match eff {
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
            BufferedObservabilityEffect::StatedTextSkipped { .. } => {}
        }
    }
}

/// The limiter keys the turn's `soft` effects release.
fn limiter_keys(soft: Vec<SoftBoundedEffect>) -> Vec<String> {
    soft.into_iter().map(|SoftBoundedEffect::ReleaseLimiter { key }| key).collect()
}

/// Commit `call` marked cancelled ([`Call::setup_cancelled`]), the setup a
/// CANCEL of `invite` reached while its routing decision was asked, with the
/// release of the CANCEL's held answers behind its write: the turn reports
/// `Logged` and `Left` as a handler turn does.
pub(super) async fn commit_cancel(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    call: &Call,
    invite: &SipRequest,
) {
    if fenced(ctx, call_ref) {
        return;
    }
    let mut marked = call.clone();
    marked.setup_cancelled = true;
    let call = Arc::new(marked);
    let wrote = ctx.state.write(Arc::clone(&call));
    let version = ctx.state.topology(call_ref).map_or((0, 0), |t| (t.gen, t.bak_gen));
    let kind = TurnKind::on_request();
    reach(ctx, call_ref, TurnPoint::Logged, version, wrote, kind, None).await;
    let now_ms = ctx.clock.now_ms();
    let entry = TurnEntry {
        cancel: Some(HeldCancel::of(invite)),
        ..TurnEntry::new(ctx, call_ref, call, now_ms, Vec::new(), (version, wrote, kind))
    };
    if fenced(ctx, call_ref) {
        return;
    }
    push(ctx, call_ref, entry).await;
}

/// Whether this turn runs on the body a poll watchdog read stuck, whose call
/// the exit discharged without it: it lands nothing more.
fn fenced(ctx: &RouterCtx, call_ref: &str) -> bool {
    let fenced = ctx.dispatcher.fenced_here();
    if fenced {
        tracing::warn!(%call_ref, "a turn read stuck went on after its call's discharge: dropped");
    }
    fenced
}

/// Queue `entry` on `call_ref`'s outbox behind the call's latest write.
async fn push(ctx: &Arc<RouterCtx>, call_ref: &str, entry: TurnEntry) {
    let wait = ctx.state.hold_target(call_ref);
    ctx.outboxes.push(call_ref, wait, Box::new(|released| Box::pin(entry.run(released)))).await;
}

#[cfg(test)]
mod tests;
