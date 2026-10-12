//! A call materialised with no message to serve — a departed peer's call the
//! departure sweep takes over, or this node's own call reclaimed after a
//! restart — whose routing decision never answered: the request for it died
//! with the node that sent it. The call is refused as a decision the backend
//! could not give is (503 Service Unavailable, ADR-0022 X3), through the
//! INVITE server transaction the materialisation rebuilt, with no Retry-After:
//! the caller takes it as a 500 (RFC 3261 §21.5.4), whether to retry its own
//! or its upstream's call. The decision is never asked again, so nothing it
//! causes happens twice (ADR-0047). A setup its caller CANCELled before any
//! decision ends as its `Cancelled` turn would have.

use std::sync::Arc;

use call::{Call, CallModelState, TerminationCause};

use super::interpret::process_result;
use super::RouterCtx;
use crate::effects::HandlerResult;
use crate::rules::invariants::{enforce, finalize, UnansweredCaller, UnansweredCaller::Leave};

/// Whether `call` is still waiting on its first routing decision: live, the
/// caller unanswered, no decision applied, nothing dialled.
pub(super) fn is_undecided(call: &Call) -> bool {
    unrouted(call) && call.a_leg.invite_final_sent.is_none()
}

/// Whether `call` is a setup its caller CANCELled before any decision applied:
/// the transaction layer answered the INVITE 487 (RFC 3261 §9.2), the turn
/// that would have ended the call did not run here.
pub(super) fn is_cancelled_setup(call: &Call) -> bool {
    unrouted(call) && call.a_leg.invite_final_sent == Some(487)
}

/// Live, no decision applied, nothing dialled.
fn unrouted(call: &Call) -> bool {
    call.state == CallModelState::Active
        && call.decision_ordinal == 0
        && call.attempts.is_empty()
        && call.b_legs.is_empty()
}

/// Write to the backup what the transaction layer answered a setup's CANCEL
/// with, while the INVITE's turn still holds the call: a backup that takes
/// the call over finds its caller answered, and sends nothing more. The live
/// record is not touched, so the write goes one version ahead of it; its own
/// turns record the same final.
pub(super) fn replicate_cancelled_setup(ctx: &RouterCtx, call_ref: &str, cseq: u32) {
    let Some(call) = ctx.state.peek(call_ref) else { return };
    let invite = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
    let backed_up = call.topology.as_ref().is_some_and(|t| !t.bak.is_empty());
    if !backed_up || invite.cseq().seq() != cseq || !is_undecided(&call) {
        return;
    }
    let a_leg = call.a_leg.leg_id.clone();
    ctx.state.flush_ahead(&call::helpers::record_cancelled_invite(call, &a_leg));
}

/// End a CANCELled setup this node materialised with no message, as its
/// `Cancelled` turn would have: nothing on the wire — the caller holds its
/// 487 — a Cancel CDR event, the call ended by its caller.
pub(super) async fn settle_cancelled_setup(ctx: &Arc<RouterCtx>, call_ref: &str, call: Call) {
    let now_ms = ctx.clock.now_ms();
    let before = call.clone();
    let a_leg = call.a_leg.leg_id.clone();
    let event = call::CdrEvent {
        event_type: call::CdrEventType::Cancel,
        timestamp: now_ms,
        leg_id: a_leg.clone(),
        status_code: None,
        reason: None,
        decision_ordinal: 0,
    };
    let call = call::helpers::add_cdr_event(call, event);
    let mut ended = call::helpers::record_termination(
        call,
        now_ms,
        TerminationCause::RemoteCancel,
        Some(a_leg),
    );
    ended.state = CallModelState::Terminated;
    let result =
        enforce(&ctx.obligations, &before, finalize(HandlerResult::new(ended)), now_ms, Leave);
    process_result(ctx, call_ref, result, now_ms, &"the end of a cancelled setup", None).await;
}

/// End the undecided `call` this node just materialised with no message, under
/// the per-call lock its caller holds. `seeded` is what the materialisation
/// seeded: an undecided call names one transaction, the caller's INVITE, so
/// none seeded means another transaction here already holds its branch — the
/// caller's retransmission placed as a call of its own — and the 503 would
/// reach that call's caller. The call then ends with nothing on the wire.
/// A takeover copy defers its CDR to the call's primary, a reclaim writes it
/// (ADR-0020 X3).
pub(super) async fn settle_undecided(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    call: Call,
    seeded: usize,
) {
    let now_ms = ctx.clock.now_ms();
    let before = call.clone();
    let advertisement = &ctx.config.minted_final_advertisement;
    let (handled, unanswered) = if seeded > 0 {
        let a_invite = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let refused = crate::initial_invite::reject_call(
            call,
            &a_invite,
            503,
            Some("Service Unavailable".into()),
            None,
            &[],
            advertisement,
            &ctx.id_gen,
            now_ms,
            TerminationCause::Admission,
        );
        (refused, UnansweredCaller::Answer(advertisement))
    } else {
        let mut ended =
            call::helpers::record_termination(call, now_ms, TerminationCause::Admission, None);
        ended.state = CallModelState::Terminated;
        (HandlerResult::new(ended), UnansweredCaller::Leave)
    };
    tracing::info!(
        node = observe::node(),
        call_ref,
        call_id = %before.a_leg.call_id,
        answered = seeded > 0,
        "undecided call settled"
    );
    let result = enforce(&ctx.obligations, &before, finalize(handled), now_ms, unanswered);
    process_result(ctx, call_ref, result, now_ms, &"the settle of an undecided call", None).await;
}

#[cfg(test)]
mod tests;
