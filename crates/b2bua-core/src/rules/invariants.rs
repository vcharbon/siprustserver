//! Framework guarantees — port of `InvariantEnforcer.ts` + the bye-disposition
//! invariant. On the `→ terminated` transition the framework appends the
//! cleanup a buggy rule might have forgotten: cancel-all-timers first, then
//! every **obligation** the call still owes (the CDR, the limiter release —
//! derived from the snapshot by the [`ObligationSet`], ADR-0020 X7), then
//! remove-call last — so termination is always clean. Termination is also
//! *promoted* (`terminating → terminated`) once every leg is resolved.

use call::helpers::is_fully_resolved;
use call::{Call, CallModelState, CdrEvent, CdrEventType, LegState, MachineId, StateLabel};

use crate::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, HandlerEffects, HandlerResult,
};
use crate::obligations::ObligationSet;
use crate::turn_notes::{TurnNotes, TERMINATION_RECORDED};
use sip_message::generators::CapabilitySet;

/// The always-on global call machine (ADR-0016 X2). Its cursor is a uniform,
/// read-only **projection** of the authoritative `CallModelState` — the engine,
/// the doc generator, observability, and HA reconciliation read every machine's
/// position through `sm_cursors` regardless of crate. The `state` field stays the
/// single source of truth (this is the one cursor `SetState` does not write).
pub const GLOBAL_CALL_MACHINE: MachineId = MachineId::new("global-call");

fn global_call_label(state: CallModelState) -> StateLabel {
    match state {
        CallModelState::Active => StateLabel::new("Active"),
        CallModelState::Terminating => StateLabel::new("Terminating"),
        CallModelState::Terminated => StateLabel::new("Terminated"),
    }
}

/// Promote `terminating → terminated` when all legs are resolved and no
/// request of this stack waits to leave, then project
/// the (possibly promoted) `CallModelState` into the `global-call` cursor. This
/// is the single finalize point, so the global machine is observable uniformly
/// without touching the authoritative `state` field or the termination logic.
pub fn finalize(mut result: HandlerResult) -> HandlerResult {
    if result.call.state == CallModelState::Terminating
        && is_fully_resolved(&result.call)
        && !super::actions::holds_requests(&result.call)
        && !super::actions::possibly_confirmed::holds_any(&result.call)
    {
        result.call.state = CallModelState::Terminated;
    }
    result.call.sm_cursors.insert(GLOBAL_CALL_MACHINE, global_call_label(result.call.state));
    // Project the authoritative `Call.transfer.phase` into the `transfer` machine
    // cursor (ADR-0016) the same way — a read-only view the transfer
    // service rules gate on; clearing the slice removes the cursor.
    super::refer_transfer::project_cursor(&mut result.call);
    // Project `(strategy, first_relayed)` into the `relayFirst18x` machine cursor
    // (ADR-0016) — Masking → Suppressing as the first 18x is relayed; absent when
    // no masking strategy is active (incl. the delayed-offer self-disable).
    super::relay_first_18x::project_cursor(&mut result.call);
    result
}

/// What the funnel does for a caller whose INVITE a terminating call leaves
/// unanswered.
#[derive(Clone, Copy, Debug)]
pub enum UnansweredCaller<'a> {
    /// Answer her 503 under the deployment's advertisement for a minted final.
    Answer(&'a CapabilitySet),
    /// Leave the wire alone: a body already terminal when it reached this
    /// node, or an unrecoverable call (ADR-0052 §11).
    Leave,
}

/// Guarantee cleanup on the `→ terminated` transition: CancelAllTimers first,
/// the unanswered-a-leg final (ADR-0022, under `UnansweredCaller::Answer`), then
/// every owed obligation (`obligations.settle` — the CDR + the limiter release
/// derived from the snapshot, idempotent against rule-emitted cleanup), then
/// RemoveCall last.
///
/// `unanswered` is `Answer` on every LIVE-serving funnel (rules,
/// initial-INVITE, limiter-refresh, reaper discharge) and `Leave` ONLY on the
/// shell's discharge of a body already terminal when it reached this node:
/// the node that served it to terminal answered its caller, or her
/// transaction ended long before, so its discharge stays OFF the SIP wire. An
/// unrecoverable call (ADR-0052 §11) takes `Leave` too: it has no INVITE to
/// answer. What the
/// funnel meets that it cannot carry is noted on `notes`.
pub fn enforce(
    obligations: &ObligationSet,
    before: &Call,
    mut result: HandlerResult,
    now_ms: i64,
    unanswered: UnansweredCaller<'_>,
    notes: &mut TurnNotes,
) -> HandlerResult {
    let became_terminated = before.state != CallModelState::Terminated
        && result.call.state == CallModelState::Terminated;
    if !became_terminated {
        return result;
    }
    // Every path to terminal names its cause: the record is the CDR's
    // statement of who ended the call, and a terminal without one is a
    // writer this funnel does not know.
    if result.call.termination.is_none() {
        let detail = "terminated without a termination record".to_string();
        notes.defect(TERMINATION_RECORDED, detail, &result.call);
        result.effects.buffered.push(BufferedObservabilityEffect::TerminationUnrecorded);
    }
    if let UnansweredCaller::Answer(advertisement) = unanswered {
        answer_a_leg_if_unanswered(before, &mut result, now_ms, advertisement, notes);
    }
    let crit = &mut result.effects.critical;
    if !crit.iter().any(|e| matches!(e, CriticalStateEffect::CancelAllTimers)) {
        let rest = std::mem::take(crit);
        crit.push(CriticalStateEffect::CancelAllTimers);
        crit.extend(rest);
    }
    result.call.timers.clear();

    obligations.settle(&result.call, &mut result.effects);

    // remove-call must run last.
    result.effects.critical.retain(|e| !matches!(e, CriticalStateEffect::RemoveCall));
    result.effects.critical.push(CriticalStateEffect::RemoveCall);
    result
}

/// The **unanswered-a-leg final** (ADR-0022). sip-txn auto-answers 100 Trying
/// the instant an INVITE server txn is born, so a call that reaches
/// `→ terminated` with the caller's INVITE still unanswered strands a caller
/// who is actively waiting: the reaper force-terminal paths deliberately emit
/// no wire messages, and the txn backstop deletes an unanswered server txn
/// *silently*. Append here — the one funnel every termination rides — the
/// final response the path forgot: `503 Service Unavailable`, no Reason
/// header (the canonical error-case reject; decision-error and overload use
/// 503 too).
///
/// Guards, in order:
///  - `before.a_leg.state ∈ {Trying, Early}` — the a-leg entered this turn
///    unanswered (an answered a-leg reads `Confirmed` or `Terminated`).
///  - a non-empty `a_leg_invite` snapshot (nothing to answer otherwise —
///    degenerate synthetic fixtures).
///  - `a_leg.invite_final_sent()` is `None` — the transaction carries no final
///    yet: neither a reject / relay / setup-timeout final of this turn nor the
///    487 the txn layer sent autonomously on a CANCEL.
///
/// The 503 carries the deployment's advertisement for a minted final and
/// leaves through `response_to_a_leg`, the one seam every a-facing
/// final rides: it records the final on the leg and refuses any later one on
/// that transaction. sip-txn drops a second final on the branch as well — in
/// `Completed`, through the Timer I `Confirmed` hold after the ACK, and on a
/// branch it no longer holds (`server_final_unseen_branch`).
fn answer_a_leg_if_unanswered(
    before: &Call,
    result: &mut HandlerResult,
    now_ms: i64,
    advertisement: &CapabilitySet,
    notes: &mut TurnNotes,
) {
    if matches!(before.a_leg.state(), LegState::Trying | LegState::Early) {
        let (call, effects) = (&mut result.call, &mut result.effects);
        answer_unanswered_caller(call, effects, now_ms, advertisement, notes, None);
    }
}

/// Answer the caller's INVITE `503` when it carries no final yet and a
/// snapshot to answer exists, recording it on the CDR: the funnel above, and
/// a teardown whose release waits on a callee's CANCEL (the caller is owed
/// nothing from it). Once her final left, every request relayed toward her
/// or held on her leg is answered (ADR-0055 §2).
pub(crate) fn answer_unanswered_caller(
    call: &mut Call,
    effects: &mut HandlerEffects,
    now_ms: i64,
    advertisement: &CapabilitySet,
    notes: &mut TurnNotes,
    under: Option<String>,
) {
    if call.a_leg_invite.headers.is_empty() || call.a_leg.invite_final_sent().is_some() {
        return;
    }
    let Some(a_invite) = super::relay::caller_invite(call, notes) else { return };
    let a_leg_id = call.a_leg.leg_id.clone();
    // The a-leg's own tag when an 18x already pinned one (Early), else the
    // one the call derives for it: the tag a provisional its earlier owner
    // showed carried (ADR-0055 §4).
    let to_tag = under
        .or_else(|| call.a_leg.own_tag.clone().filter(|t| !t.is_empty()))
        .or_else(|| Some(crate::caller_tags::own(call)));
    let mut own = Vec::new();
    super::capabilities::stamp_own_advertisement(advertisement, |_| false, &mut own);
    let Some(mut effect) = super::relay::response_to_a_leg(
        call,
        effects,
        &a_invite,
        503,
        "Service Unavailable",
        to_tag,
        None,
        vec![],
        None,
        None,
        own,
        crate::effects::Provenance::Authored,
        super::relay::Author::Stack,
    ) else {
        return;
    };
    effect.label = "503 (terminated unanswered) → a-leg".to_string();
    effects.outbound.push(effect);
    super::actions::settle_owed(call, effects, &a_leg_id);
    // Before `obligations.settle` reads the snapshot, so the CDR carries it.
    call.cdr_events.push(CdrEvent {
        event_type: CdrEventType::Reject,
        timestamp: now_ms,
        leg_id: a_leg_id,
        status_code: Some(503),
        reason: Some("unanswered_at_termination".to_string()),
        decision_ordinal: call.decision_ordinal,
    });
}
