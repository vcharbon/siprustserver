//! A call materialised with no message to serve — a departed peer's call the
//! departure sweep takes over, or this node's own call reclaimed after a
//! restart — whose routing decision never answered: the request for it died
//! with the node that sent it. The call is refused as a decision the backend
//! could not give is (503 Service Unavailable, ADR-0022 X3), through the
//! INVITE server transaction the materialisation rebuilt, with no Retry-After:
//! the caller's next hop is the front proxy, which places a retry on a live
//! worker at once. The decision is never asked again, so nothing it causes
//! happens twice (ADR-0046).

use std::sync::Arc;

use call::{Call, CallModelState, TerminationCause};

use super::interpret::process_result;
use super::RouterCtx;
use crate::effects::HandlerResult;
use crate::rules::invariants::{enforce, finalize, UnansweredCaller};

/// Whether `call` is still waiting on its first routing decision: live, the
/// caller unanswered, no decision applied, nothing dialled.
pub(super) fn is_undecided(call: &Call) -> bool {
    call.state == CallModelState::Active
        && call.a_leg.invite_final_sent.is_none()
        && call.decision_ordinal == 0
        && call.attempts.is_empty()
        && call.b_legs.is_empty()
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
