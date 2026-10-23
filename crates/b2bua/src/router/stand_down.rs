//! A takeover copy stands down on its primary's write (ADR-0047 D7). A write
//! the call's primary published, applied to this node's Element of a call it
//! serves as a live takeover copy, is the primary serving the call again. The
//! copy is released local-only — timers cancelled, transactions relinquished,
//! no store write, no CDR (ADR-0020 X3) — when the primary's record covers
//! every write the copy made ([`covers`]), or when the primary deleted the
//! call and the copy holds no request whose final only it sees
//! ([`finishes_nothing_alone`]); otherwise it plays on and is shed at
//! quiescence (ADR-0014).
//!
//! The notice is a turn of the call's own queue, so a message queued for the
//! copy before it is served by the copy first (ADR-0010 X2), and the rule
//! reads the copy as that message left it.

use std::sync::Arc;

use call::helpers::{holds_relays_of, holds_rounds_of, round_ahead};
use call::{Call, RelayOwner};

use super::release::{release_call, ReleaseKind};
use super::{PrimaryWrite, RouterCtx};
use b2bua_sdk::event::CallEvent;

/// The topic of the stand-down notice, an internal event the router raises
/// itself and its turn consumes before any rule reads it.
pub(super) const TOPIC: &str = "takeover-stand-down";
/// The notice's outcome for a primary's `Put`; its body rides the event.
const PUT: &str = "put";
/// The notice's outcome for a primary's `Delete`; the incarnation it names
/// rides the payload.
const DELETE: &str = "delete";

/// Queue the stand-down notice of `call_ref` carrying its primary's applied
/// `write`, for a call this node serves as a live takeover copy; nothing for
/// any other call.
pub(super) async fn offer(ctx: &Arc<RouterCtx>, call_ref: String, write: PrimaryWrite) {
    if !ctx.state.is_takeover(&call_ref) {
        return;
    }
    let (outcome, payload, body) = match write {
        PrimaryWrite::Put(body) => (PUT, serde_json::Value::Null, body.to_vec()),
        PrimaryWrite::Delete(incarnation) => (DELETE, serde_json::json!(incarnation), Vec::new()),
    };
    let event = CallEvent::InternalEvent {
        call_ref,
        topic: TOPIC.to_string(),
        outcome: outcome.to_string(),
        payload,
        body,
        incarnation: None,
    };
    super::ingress::on_event(ctx, event).await;
}

/// The stand-down turn of `call_ref` on the notice `outcome` / `payload` /
/// `body` ([`offer`]). Read under the per-call lock, held across the release
/// as in the self-release turn (`quiesced`).
pub(super) async fn stand_down_turn(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    outcome: &str,
    payload: &serde_json::Value,
    body: &[u8],
) {
    let guard = ctx.state.lock(call_ref).await;
    let Some(copy) = ctx.state.peek(call_ref) else {
        // Shed between the notice and its turn: what this turn's queue and
        // lock entry are is all that is left of the call here.
        drop(guard);
        release_call(ctx, call_ref, ReleaseKind::Orphan).await;
        return;
    };
    if !ctx.state.is_takeover(call_ref) {
        return;
    }
    let by = &ctx.config.relay_owner();
    let stands_down = match outcome {
        PUT => ctx.state.decode_body(body).is_some_and(|primary| covers(&primary, &copy, by)),
        DELETE => {
            payload.as_str().is_none_or(|deleted| deleted == copy.incarnation())
                && finishes_nothing_alone(None, &copy, by)
        }
        _ => false,
    };
    if stands_down {
        release_call(ctx, call_ref, ReleaseKind::StandDown).await;
    }
}

/// Whether the primary's body `primary` covers every write the takeover
/// `copy` made on worker `by`: the same call (one incarnation), the primary's
/// `b` at or past the copy's, no re-INVITE round on which the copy stands
/// ahead (`call::helpers::round_ahead`), and every request the copy relayed
/// and awaits a final for held in the primary's record
/// ([`finishes_nothing_alone`]). One `b` increment is one write the copy made
/// as the call's acting backup (`CallState::update` on this node); a fold of
/// the primary's body is not one. A primary reaches a `b` by reading the
/// copy's write that carries it, applied or folded into its record, or
/// refused and its counter adopted (ADR-0031 D3).
pub(super) fn covers(primary: &Call, copy: &Call, by: &RelayOwner) -> bool {
    let bak_gen = |c: &Call| c.topology.as_ref().map_or(0, |t| t.bak_gen);
    primary.incarnation() == copy.incarnation()
        && bak_gen(primary) >= bak_gen(copy)
        && !round_ahead(copy, primary)
        && finishes_nothing_alone(Some(primary), copy, by)
}

/// Whether `copy` on worker `by` holds no request whose final only it sees
/// that `record` — the primary's, or none once deleted — lacks: every
/// re-INVITE round it holds the transaction of, unfinished, is the record's
/// transaction or behind a later one (`call::helpers::holds_rounds_of`), and
/// every other request it relayed is the record's relay
/// (`call::helpers::holds_relays_of`). A final follows the Via to the node
/// that sent the request (RFC 3261 §18.2.2): a copy that relinquished it
/// leaves the originator with no final, so the copy plays on with it and
/// self-releases once its transactions clear.
pub(super) fn finishes_nothing_alone(record: Option<&Call>, copy: &Call, by: &RelayOwner) -> bool {
    holds_rounds_of(record, copy, by) && holds_relays_of(record, copy, by)
}

#[cfg(test)]
mod tests;
