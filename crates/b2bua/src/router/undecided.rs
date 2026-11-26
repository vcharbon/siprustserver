//! A call materialised with no message to serve — a departed peer's call the
//! departure sweep or a proxy's dead verdict takes over, or this node's own
//! call reclaimed after a restart — whose routing decision never answered: the
//! request for it died with the node that sent it. The node now serving the
//! call asks the decision again (ADR-0047 D4) on a turn of the call's own, so
//! the sweep does not wait on its round trip: the initial consult runs on the
//! materialised call, its caller's INVITE rebuilt from the record, and answers
//! her through the INVITE server transaction the materialisation seeded. A
//! re-ask that starts past the start bound, counted from the INVITE's receipt
//! the record states, asks nothing and refuses her the bare 503 (ADR-0070):
//! the node cannot tell whether her 100 left, and a caller that heard none
//! may have been failed over already; one inside it sends her a 100 before it
//! asks. The
//! route's admit is numbered above every change the call reserved before, the
//! counter having moved to its next epoch on the restore, so the limiter
//! checks it net of what the key holds and counts the call once. A call whose
//! caller CANCELed the setup during its consult ([`Call::setup_cancelled`])
//! holds no applied decision, possibly none at all; her 487 either left
//! already or leaves on the seeded transaction when she repeats her CANCEL.
//! It ends under her cancel, its CDR stating it, with nothing on the wire. A node whose endpoint
//! left the slice asks nothing and refuses the caller 503, as it refuses a
//! decision it does not apply (ADR-0047 D6). One owner acts on the answer
//! (ADR-0047 D7): a takeover copy whose call's primary wrote the call since
//! the copy was made — a restarted primary's reclaim, which asks the decision
//! itself — applies nothing and stands down on that write. A re-ask past a
//! per-call cap fails as the initial INVITE's turn does (ADR-0057 §4).

use std::sync::Arc;
use std::time::Duration;

use call::{Call, CallModelState};
use sip_message::SipRequest;
use tokio::time::Instant;

use super::cancel_in_consult::consult_writing_its_cancel;
use super::commit::commit_turn;
use super::off_wire;
use super::RouterCtx;
use crate::caps::{hold, Checked, Gauge};
use crate::effects::HandlerResult;
use crate::initial_consult::{handle_initial_invite, Consulted};
use crate::rules::invariants::{enforce, finalize, UnansweredCaller};
use crate::rules::lifecycle_delivery::{close_turn, TurnStart};
use crate::rules::ActionExecutor;
use crate::setup_budget::SetupBudget;
use crate::undecided::Settled;
use b2bua_sdk::event::CallEvent;

/// The topic of the re-ask notice, an internal event the router raises itself
/// and its turn consumes before any rule reads it.
pub(super) const TOPIC: &str = "undecided-reask";

/// Whether `call` is still waiting on its first routing decision: live, the
/// caller unanswered, no decision applied, nothing dialled.
pub(super) fn is_undecided(call: &Call) -> bool {
    call.state == CallModelState::Active
        && call.a_leg.invite_final_sent().is_none()
        && (call.decision_ordinal == 0 || call.setup_cancelled)
        && call.attempts.is_empty()
        && call.b_legs.is_empty()
}

/// Settle the undecided `call` this node just materialised with no message,
/// under the per-call lock its caller holds. `seeded` is what the
/// materialisation seeded: an undecided call names one transaction, the
/// caller's INVITE, so none seeded means another transaction here already
/// holds its branch — the caller's retransmission placed as a call of its own
/// — and an answer would reach that call's caller: the call then ends with
/// nothing on the wire, as does a setup the caller CANCELed, the call's own
/// record or its setup-CANCEL mark saying so, with the cancel's cause and
/// event. Otherwise its
/// decision is asked again on a turn of its own ([`reask_turn`]). A takeover
/// copy defers its CDR to the call's primary, a reclaim writes it
/// (ADR-0020 X3).
pub(super) async fn settle_undecided(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    mut call: Call,
    seeded: usize,
) {
    // A CANCEL the layer took for the INVITE before the call was resident
    // marked its setup: the call ends cancelled.
    if ctx.state.is_setup_cancelled(call_ref, call.a_leg_invite.cseq) {
        call.setup_cancelled = true;
    }
    if seeded > 0 && !call.setup_cancelled && !ctx.readiness.left_the_slice() {
        tracing::info!(
            node = observe::node(),
            call_ref,
            call_id = %call.a_leg.call_id,
            "undecided call: its decision is asked again"
        );
        let _ = ctx.reentry_tx.send(notice(&call));
        return;
    }
    let now_ms = ctx.clock.now_ms();
    let mut turn = off_wire::scope(ctx);
    let before = call.clone();
    let answer = seeded > 0 && !call.setup_cancelled;
    let advertisement = &ctx.config.minted_final_advertisement;
    let (result, caller) =
        match crate::undecided::settle(call, answer, advertisement, &mut turn, now_ms) {
            Settled::Refused(result) => (result, off_wire::Caller::Answer),
            Settled::Ended(result) => (result, off_wire::Caller::Leave),
        };
    tracing::info!(
        node = observe::node(),
        call_ref,
        call_id = %before.a_leg.call_id,
        answered = matches!(caller, off_wire::Caller::Answer),
        "undecided call settled"
    );
    let ending = off_wire::Ending::Built { before: &before, result, turn: Box::new(turn) };
    let label = &"the settle of an undecided call";
    off_wire::end(ctx, call_ref, ending, caller, now_ms, label).await;
}

/// The re-ask notice of `call`, carrying its INVITE's receipt.
fn notice(call: &Call) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: TOPIC.to_string(),
        outcome: String::new(),
        payload: serde_json::Value::from(call.received_at),
        body: Vec::new(),
        incarnation: Some(call.incarnation().to_string()),
    }
}

/// Whether `event` is the re-ask notice ([`notice`]), and of which call.
pub(super) fn notice_of(event: &CallEvent) -> Option<&str> {
    match event {
        CallEvent::InternalEvent { call_ref, topic, .. } if topic == TOPIC => Some(call_ref),
        _ => None,
    }
}

/// The setup budget of the re-ask `event` names, `None` for any other event:
/// a budget of its own from now (ADR-0047 D4), its start bound counted from
/// the INVITE's receipt the call's record states (ADR-0070), so a re-ask
/// whose caller may have heard no 100 for longer than the bound dials no one.
pub(super) fn reask_budget(ctx: &RouterCtx, event: &CallEvent) -> Option<SetupBudget> {
    let CallEvent::InternalEvent { topic, payload, .. } = event else { return None };
    if topic != TOPIC {
        return None;
    }
    let received_at = payload.as_i64().unwrap_or(i64::MIN);
    let ago = ctx.clock.now_ms().saturating_sub(received_at).max(0);
    let ago = Duration::from_millis(ago.unsigned_abs());
    let budget = SetupBudget::from_receipt(Instant::now(), ctx.config.call_control_timeout());
    Some(budget.with_start_bound_since(ago, ctx.config.call_start_bound()))
}

/// The re-ask turn of `call_ref`, under its per-call lock: the resident call,
/// still undecided, is consulted as its initial INVITE's turn consults, under
/// the fresh `budget` its notice started, its caller sent a 100 first, and
/// the turn lands as that one does; past its start bound or the budget spent
/// before the turn ran, its caller is refused unasked with the bare 503 and
/// no one is dialled, and an INVITE refused on sight ([`crate::initial_invite::refused_on_sight`]) is
/// refused as its first owner refuses it, whose death between the call's
/// creation write and that refusal's left it undecided. A call
/// gone, decided meanwhile or whose setup the caller CANCELed asks nothing; a
/// copy its primary wrote over ([`primary_wrote_since`]) asks nothing, and
/// applies no answer that returns after that write. The turn closes at the
/// clock's reading after its consult, as the initial INVITE's does.
pub(super) async fn reask_turn(ctx: &Arc<RouterCtx>, call_ref: &str, budget: SetupBudget) {
    let _guard = ctx.state.lock(call_ref).await;
    let Some(call) = ctx.state.peek(call_ref) else { return };
    let cseq = call.a_leg_invite.cseq;
    let cancelled = || ctx.state.is_setup_cancelled(call_ref, cseq);
    let asks_nothing = !is_undecided(&call) || call.setup_cancelled || cancelled();
    if asks_nothing || primary_wrote_since(ctx, call_ref).await {
        return;
    }
    let Ok(caller) = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite) else {
        return;
    };
    let now_ms = ctx.clock.now_ms();
    let turn = crate::turn_scope::TurnScope::of(ctx.id_gen.turn());
    let mut turn = turn.with_faults(ctx.wire_faults.snapshot());
    let topology = ctx.state.topology(call_ref);
    let owners = crate::store::envelope::owners(&ctx.relay_owner, topology.as_ref());
    // The INVITE as it arrived is in no record: a trace backfills its image.
    let wire = caller.image().to_vec();
    let refused = budget.started_late() || budget.spent();
    let advertisement = &ctx.config.minted_final_advertisement;
    let on_sight = || {
        crate::initial_invite::refused_on_sight(&call, &caller, advertisement, now_ms).map(Box::new)
    };
    let (handled, recorded) = if refused {
        (super::initial_turn::refused_unavailable(ctx, call.clone(), &caller, now_ms), false)
    } else if let Some(refused) = on_sight() {
        (*refused, false)
    } else {
        trying_before_the_reask(ctx, &caller).await;
        let consult = handle_initial_invite(
            call.clone(),
            &caller,
            ctx.decision.as_ref(),
            &ctx.limiter,
            &ctx.config,
            &owners,
            &mut turn,
            &ctx.services,
            &wire,
            &ctx.clock,
            now_ms,
            budget,
            &cancelled,
        );
        match consult_writing_its_cancel(ctx, call_ref, &call, &caller, consult).await {
            Consulted::Applied(handled) => (*handled, false),
            // The caller CANCELed during the round trip: the decision is recorded,
            // never applied, and her CANCEL's turn, queued behind this one, ends
            // the call and releases what the decision admitted.
            Consulted::Cancelled(decided) => {
                ctx.metrics.bump_decision_dropped_cancelled();
                let call = super::initial_turn::recorded_turn(&call, *decided);
                (HandlerResult { call, effects: crate::effects::HandlerEffects::new() }, true)
            }
        }
    };
    let handled = if !recorded && ctx.readiness.left_the_slice() {
        super::initial_turn::refused_after_leaving_the_slice(ctx, &call, &caller, handled, now_ms)
    } else {
        handled
    };
    if primary_wrote_since(ctx, call_ref).await {
        tracing::info!(
            node = observe::node(),
            call_ref,
            "undecided call: its primary reclaimed it, the answer is the primary's"
        );
        return;
    }
    let advertisement = UnansweredCaller::Answer(&ctx.config.minted_final_advertisement);
    let enforced =
        enforce(&ctx.obligations, &call, finalize(handled), now_ms, advertisement, &mut turn.notes);
    let closed = {
        let closed_ms = ctx.clock.now_ms();
        let mut exec = ActionExecutor {
            config: &ctx.config,
            owners: &owners,
            turn: &mut turn,
            now_ms: closed_ms,
        };
        close_turn(&ctx.rules, &TurnStart::unborn(), enforced, &mut exec)
    };
    let mut result =
        match hold(&ctx.config.caps, &Gauge::of(&call), Checked::All, &mut turn, closed) {
            Ok(result) => result,
            Err(tripped) => {
                // The record as it stands now, a cancel the consult wrote included.
                let safepoint = ctx.state.peek(call_ref);
                super::notes::drain_discarded(ctx, &mut turn);
                super::tripped::fail_turn(ctx, call_ref, None, tripped, safepoint, now_ms).await;
                return;
            }
        };
    // A decision another node may have asked.
    result.effects.reasked = !refused;
    let turn_label = &"the re-ask of an undecided call's decision";
    commit_turn(ctx, call_ref, result, now_ms, turn_label, None, Some(&mut turn)).await;
}

/// Send the caller `caller` her 100 through the INVITE transaction the
/// materialisation seeded, as a re-ask inside its start bound asks
/// (ADR-0070): the node cannot tell whether the first owner's 100 left, and a
/// caller that heard none stops retransmitting before the callee rings. A
/// seed that sent a provisional already sends nothing.
async fn trying_before_the_reask(ctx: &RouterCtx, caller: &SipRequest) {
    let _ = ctx.txn.release_trying(caller).await;
}

/// Whether `call_ref` is a takeover copy whose primary wrote the call since
/// the copy was made: the replica here carries a primary counter past the
/// copy's.
async fn primary_wrote_since(ctx: &RouterCtx, call_ref: &str) -> bool {
    if !ctx.state.is_takeover(call_ref) {
        return false;
    }
    let Some(live) = ctx.state.topology(call_ref) else { return false };
    let Ok((replica, _)) = ctx.state.peek_replica(call_ref).await else { return false };
    replica.topology.is_some_and(|t| t.gen > live.gen)
}

#[cfg(test)]
mod tests;
