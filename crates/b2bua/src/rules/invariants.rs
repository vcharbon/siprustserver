//! Framework guarantees — port of `InvariantEnforcer.ts` + the bye-disposition
//! invariant. On the `→ terminated` transition the framework appends the
//! cleanup a buggy rule might have forgotten: cancel-all-timers first, then
//! every **obligation** the call still owes (the CDR, the limiter decrements —
//! derived from the snapshot by the [`ObligationSet`], ADR-0020 X7), then
//! remove-call last — so termination is always clean. Termination is also
//! *promoted* (`terminating → terminated`) once every leg is resolved.

use call::helpers::is_fully_resolved;
use call::{Call, CallModelState, CdrEvent, CdrEventType, LegState, MachineId, StateLabel};

use crate::effects::{CriticalStateEffect, HandlerResult, OutboundBody};
use crate::obligations::ObligationSet;

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

/// Promote `terminating → terminated` when all legs are resolved, then project
/// the (possibly promoted) `CallModelState` into the `global-call` cursor. This
/// is the single finalize point, so the global machine is observable uniformly
/// without touching the authoritative `state` field or the termination logic.
pub fn finalize(mut result: HandlerResult) -> HandlerResult {
    if result.call.state == CallModelState::Terminating && is_fully_resolved(&result.call) {
        result.call.state = CallModelState::Terminated;
    }
    result
        .call
        .sm_cursors
        .insert(GLOBAL_CALL_MACHINE, global_call_label(result.call.state));
    // Project the authoritative `Call.transfer.phase` into the `transfer` machine
    // cursor (ADR-0016 slice 7) the same way — a read-only view the transfer
    // service rules gate on; clearing the slice removes the cursor.
    super::refer_transfer::project_cursor(&mut result.call);
    // Project `(strategy, first_relayed)` into the `relayFirst18x` machine cursor
    // (ADR-0016) — Masking → Suppressing as the first 18x is relayed; absent when
    // no masking strategy is active (incl. the delayed-offer self-disable).
    super::relay_first_18x::project_cursor(&mut result.call);
    result
}

/// Guarantee cleanup on the `→ terminated` transition: CancelAllTimers first,
/// the unanswered-a-leg final (ADR-0022, when `answer_unanswered_a_leg`), then
/// every owed obligation (`obligations.settle` — the CDR + limiter decrements
/// derived from the snapshot, idempotent against rule-emitted cleanup), then
/// RemoveCall last.
///
/// `answer_unanswered_a_leg` is `true` on every LIVE-serving funnel (rules,
/// initial-INVITE, limiter-refresh, reaper discharge) and `false` ONLY on the
/// two HA discharge helpers for already-terminal reclaimed/folded bodies
/// (`discharge_materialized_terminal` / `discharge_folded_terminal`): those
/// bodies were answered by whichever node served them to terminal — or their
/// caller's transaction died with that node ≥ `reboot_budget` ago — and the HA
/// contract keeps reclaim-discharge OFF the SIP wire.
pub fn enforce(
    obligations: &ObligationSet,
    before: &Call,
    mut result: HandlerResult,
    now_ms: i64,
    answer_unanswered_a_leg: bool,
) -> HandlerResult {
    let became_terminated =
        before.state != CallModelState::Terminated && result.call.state == CallModelState::Terminated;
    if !became_terminated {
        return result;
    }
    if answer_unanswered_a_leg {
        answer_a_leg_if_unanswered(before, &mut result, now_ms);
    }
    let crit = &mut result.effects.critical;
    if !crit.iter().any(|e| matches!(e, CriticalStateEffect::CancelAllTimers)) {
        crit.insert(0, CriticalStateEffect::CancelAllTimers);
    }
    result.call.timers.clear();

    obligations.settle(&result.call, &mut result.effects);

    // remove-call must run last.
    result
        .effects
        .critical
        .retain(|e| !matches!(e, CriticalStateEffect::RemoveCall));
    result.effects.critical.push(CriticalStateEffect::RemoveCall);
    result
}

/// The **unanswered-a-leg final** (ADR-0022). sip-txn auto-answers 100 Trying
/// the instant an INVITE server txn is born, so a call that reaches
/// `→ terminated` with the caller's INVITE still unanswered strands a caller
/// who is actively waiting: the reaper force-terminal paths deliberately emit
/// no wire messages, and the txn sweep deletes an unanswered server txn
/// *silently*. Append here — the one funnel every termination rides — the
/// final response the path forgot: `503 Service Unavailable`, no Reason
/// header (the canonical error-case reject; decision-error and overload use
/// 503 too).
///
/// Guards, in order:
///  - `before.a_leg.state ∈ {Trying, Early}` — the a-leg entered this turn
///    unanswered. Any earlier turn that answered moved it (reject_call /
///    `RespondToALeg` → Terminated, confirm-dialog → Confirmed).
///  - no final response to the a-leg among THIS turn's outbound effects (the
///    reject/relay/setup-timeout paths all answer through effects).
///  - no a-leg `Cancel` CDR event: on CANCEL the txn layer answers 487
///    autonomously — the 487 IS the final (`cancel_during_slow_decision`).
///  - a non-empty `a_leg_invite` snapshot (nothing to answer otherwise —
///    degenerate synthetic fixtures).
///
/// Idempotence backstop: even if a guard is ever wrong, the a-leg server txn
/// drops a second final in `Completed` state (sip-txn `do_send_response`), so
/// the synthesis can only ever double-answer a caller that a *swept* (≥ 193 s
/// old) transaction once served — a harmless late raw datagram.
fn answer_a_leg_if_unanswered(before: &Call, result: &mut HandlerResult, now_ms: i64) {
    let unanswered_entering =
        matches!(before.a_leg.state, LegState::Trying | LegState::Early);
    if !unanswered_entering || result.call.a_leg_invite.headers.is_empty() {
        return;
    }
    let a_leg_id = before.a_leg.leg_id.as_str();
    let answered_this_turn = result.effects.outbound.iter().any(|e| {
        e.leg_id.as_deref() == Some(a_leg_id)
            && matches!(&e.body, OutboundBody::Response(r) if r.status >= 200)
    });
    let cancelled = result
        .call
        .cdr_events
        .iter()
        .any(|e| e.event_type == CdrEventType::Cancel && e.leg_id == a_leg_id);
    if answered_this_turn || cancelled {
        return;
    }
    let a_invite = super::relay::rebuild_a_leg_invite(&result.call.a_leg_invite);
    // Reuse the a-dialog tag when an 18x already pinned one (Early); otherwise
    // `generate_response`'s deterministic fallback tag applies.
    let to_tag = result
        .call
        .a_leg
        .dialogs
        .first()
        .map(|d| d.sip.local_tag.clone())
        .filter(|t| !t.is_empty());
    let mut effect = super::relay::response_to_a_leg(
        &a_invite,
        503,
        "Service Unavailable",
        to_tag,
        None,
        vec![],
        None,
        None,
        vec![],
    );
    effect.label = "503 (terminated unanswered) → a-leg".to_string();
    result.effects.outbound.push(effect);
    // Before `obligations.settle` reads the snapshot, so the CDR carries it.
    result.call.cdr_events.push(CdrEvent {
        event_type: CdrEventType::Reject,
        timestamp: now_ms,
        leg_id: a_leg_id.to_string(),
        status_code: Some(503),
        reason: Some("unanswered_at_termination".to_string()),
    });
}
