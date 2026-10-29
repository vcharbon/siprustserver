//! A call materialised with no message to serve — a departed peer's call the
//! departure sweep or a proxy's dead verdict takes over, or this node's own
//! call reclaimed after a restart — whose routing decision never answered: the
//! request for it died with the node that sent it. The node now serving the
//! call asks the decision again (ADR-0047 D4) on a turn of the call's own, so
//! the sweep does not wait on its round trip: the initial consult runs on the
//! materialised call, its caller's INVITE rebuilt from the record, and answers
//! her through the INVITE server transaction the materialisation seeded. The
//! route's admit is numbered above every change the call reserved before, the
//! counter having moved to its next epoch on the restore, so the limiter
//! checks it net of what the key holds and counts the call once. A call whose
//! caller CANCELed the setup during its consult ([`Call::setup_cancelled`])
//! holds a decision never applied and a caller already answered 487: it ends
//! under the caller's cancel with nothing on the wire. A node whose endpoint
//! left the slice asks nothing and refuses the caller 503, as it refuses a
//! decision it does not apply (ADR-0047 D6). One owner acts on the answer
//! (ADR-0047 D7): a takeover copy whose call's primary wrote the call since
//! the copy was made — a restarted primary's reclaim, which asks the decision
//! itself — applies nothing and stands down on that write.

use std::sync::Arc;

use call::{Call, CallModelState, TerminationCause};

use super::interpret::process_result;
use super::RouterCtx;
use crate::effects::HandlerResult;
use crate::initial_consult::{handle_initial_invite, Consulted};
use crate::rules::invariants::{enforce, finalize, UnansweredCaller};
use crate::rules::lifecycle_delivery::{close_turn, TurnStart};
use crate::rules::ActionExecutor;
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
/// nothing on the wire, as does a setup the caller CANCELed. Otherwise its
/// decision is asked again on a turn of its own ([`reask_turn`]). A takeover
/// copy defers its CDR to the call's primary, a reclaim writes it
/// (ADR-0020 X3).
pub(super) async fn settle_undecided(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    call: Call,
    seeded: usize,
) {
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
    let turn = crate::turn_scope::TurnScope::of(ctx.id_gen.turn());
    let mut turn = turn.with_faults(ctx.wire_faults.snapshot());
    let before = call.clone();
    let advertisement = &ctx.config.minted_final_advertisement;
    let answer = seeded > 0 && !call.setup_cancelled;
    let caller =
        answer.then(|| crate::rules::relay::caller_invite(&call, &mut turn.notes)).flatten();
    let (handled, unanswered) = if let Some(a_invite) = caller.as_ref() {
        let refused = crate::initial_invite::reject_call(
            call,
            a_invite,
            503,
            Some("Service Unavailable".into()),
            None,
            &[],
            advertisement,
            &mut turn.ids,
            now_ms,
            TerminationCause::Admission,
        );
        (refused, UnansweredCaller::Answer(advertisement))
    } else {
        let (cause, by_leg) = if call.setup_cancelled {
            (TerminationCause::RemoteCancel, Some(call.a_leg.leg_id.clone()))
        } else {
            (TerminationCause::Admission, None)
        };
        let mut ended = call;
        call::helpers::record_termination(&mut ended, now_ms, cause, by_leg);
        ended.state = CallModelState::Terminated;
        (HandlerResult::new(ended), UnansweredCaller::Leave)
    };
    tracing::info!(
        node = observe::node(),
        call_ref,
        call_id = %before.a_leg.call_id,
        answered = caller.is_some(),
        "undecided call settled"
    );
    let notes = &mut turn.notes;
    let result = enforce(&ctx.obligations, &before, finalize(handled), now_ms, unanswered, notes);
    super::notes::drain(ctx, call_ref, &mut turn);
    process_result(ctx, call_ref, result, now_ms, &"the settle of an undecided call", None).await;
}

/// The re-ask notice of `call`.
fn notice(call: &Call) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: TOPIC.to_string(),
        outcome: String::new(),
        payload: serde_json::Value::Null,
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

/// The re-ask turn of `call_ref`, under its per-call lock: the resident call,
/// still undecided, is consulted as its initial INVITE's turn consults, and
/// the turn lands as that one does. A call gone, decided meanwhile or whose
/// setup the caller CANCELed asks nothing; a copy its primary wrote over
/// ([`primary_wrote_since`]) asks nothing, and applies no answer that returns
/// after that write.
pub(super) async fn reask_turn(ctx: &Arc<RouterCtx>, call_ref: &str) {
    let _guard = ctx.state.lock(call_ref).await;
    let Some(call) = ctx.state.peek(call_ref) else { return };
    if !is_undecided(&call) || call.setup_cancelled || primary_wrote_since(ctx, call_ref).await {
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
        &|| false,
    )
    .await;
    // The caller's CANCEL is a turn of this call's queue, behind this one.
    let Consulted::Applied(handled) = consult else { return };
    if primary_wrote_since(ctx, call_ref).await {
        tracing::info!(
            node = observe::node(),
            call_ref,
            "undecided call: its primary reclaimed it, the answer is the primary's"
        );
        return;
    }
    let advertisement = UnansweredCaller::Answer(&ctx.config.minted_final_advertisement);
    let enforced = enforce(
        &ctx.obligations,
        &call,
        finalize(*handled),
        now_ms,
        advertisement,
        &mut turn.notes,
    );
    let mut result = {
        let mut exec =
            ActionExecutor { config: &ctx.config, owners: &owners, turn: &mut turn, now_ms };
        close_turn(&ctx.rules, &TurnStart::unborn(), enforced, &mut exec)
    };
    // A decision another node may have asked: a request sent again.
    result.effects.resent = true;
    super::notes::drain(ctx, call_ref, &mut turn);
    let turn_label = &"the re-ask of an undecided call's decision";
    process_result(ctx, call_ref, result, now_ms, turn_label, None).await;
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
