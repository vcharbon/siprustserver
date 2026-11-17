//! A Call-ID's message-budget trip (ADR-0063 §4): the transaction layer
//! muted the Call-ID, and every call this node holds under it ends as the
//! message cap's call does, through the reaper's `message-budget` verdict.
//! A trip after the calls' removal finds none and only mutes.

use sip_txn::CallIdBudgetTrip;

use super::RouterCtx;

/// End every resident call holding the Call-ID `trip` muted: the incoming leg
/// of each caller it names, the outgoing leg on it, and each call its
/// transactions were attributed to. Each is counted once.
pub(super) fn end_calls_holding(ctx: &RouterCtx, trip: &CallIdBudgetTrip) {
    let calls = ctx.state.calls_holding(&trip.call_id, &trip.caller_tags, &trip.call_refs);
    tracing::warn!(
        node = observe::node(),
        call_id = %trip.call_id,
        ?calls,
        "a Call-ID passed its message budget: ending the calls holding it"
    );
    for call_ref in calls {
        ctx.metrics.record_message_budget_trip();
        ctx.reaper.on_message_budget(&call_ref);
    }
}
