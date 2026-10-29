//! A takeover copy stands down on its primary's write (ADR-0047 D7). A write
//! the call's primary published, applied to this node's Element of a call it
//! serves as a live takeover copy, is the primary serving the call again: it
//! meets the copy ([`meet_live`](super::reclaim::meet_live), ADR-0055 §7). The
//! copy is released local-only — timers cancelled, transactions relinquished,
//! no store write, no CDR (ADR-0020 X3) — when the primary's copy contains it
//! and holds every request record it keeps open ([`covers`]), or when the
//! primary deleted the call and the copy holds no request whose final only it
//! sees ([`finishes_nothing_alone`]); otherwise it plays on and is shed at
//! quiescence (ADR-0014).
//!
//! The notice is a turn of the call's own queue, so a message queued for the
//! copy before it is served by the copy first (ADR-0010 X2), and the rule
//! reads the copy as that message left it.

use std::sync::Arc;

use call::helpers::holds_records_of;
use call::{Call, RelayOwner};

use super::reclaim::{meet_live, FlushDirection};
use super::release::{release_call, ReleaseKind};
use super::restore_hygiene::reanchor;
use super::{PrimaryWrite, RouterCtx};
use crate::store::containment::contains;
use crate::store::Enveloped;
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
    super::ingress::on_event(ctx, event, false).await;
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
    let Some(copy) = ctx.state.peek_enveloped(call_ref) else {
        // Shed between the notice and its turn: what this turn's queue and
        // lock entry are is all that is left of the call here.
        drop(guard);
        release_call(ctx, call_ref, ReleaseKind::Orphan).await;
        return;
    };
    if !ctx.state.is_takeover(call_ref) {
        return;
    }
    match outcome {
        PUT => {
            let Some(mut primary) = ctx.state.decode_body(body) else { return };
            if primary.call.incarnation() != copy.call.incarnation() {
                return;
            }
            reanchor(&mut primary.call, ctx.state.held_skew_offset_ms(call_ref));
            let now_ms = ctx.clock.now_ms();
            meet_live(ctx, FlushDirection::Forward, call_ref, &copy, primary, now_ms).await;
        }
        DELETE => {
            let by = &ctx.relay_owner;
            if payload.as_str().is_none_or(|deleted| deleted == copy.call.incarnation())
                && finishes_nothing_alone(None, &copy.call, by)
            {
                release_call(ctx, call_ref, ReleaseKind::StandDown).await;
            }
        }
        _ => {}
    }
}

/// Whether the primary's body `primary` covers the takeover `copy` on worker
/// `by`: the same call (one incarnation), the primary's copy containing the
/// copy (ADR-0055 §7: the copy's state stamp at or below the primary's
/// vector, so the primary read every compared write the copy made), every
/// request record the copy keeps open held in the primary's record
/// ([`finishes_nothing_alone`]), and no INVITE the copy dialled awaiting its
/// final ([`dials_alone`]).
pub(super) fn covers(primary: &Enveloped, copy: &Enveloped, by: &RelayOwner) -> bool {
    let contained = match (primary.topology.as_ref(), copy.topology.as_ref()) {
        (Some(p), Some(c)) => contains(p, c),
        _ => false,
    };
    primary.call.incarnation() == copy.call.incarnation()
        && contained
        && finishes_nothing_alone(Some(&primary.call), &copy.call, by)
        && !dials_alone(&copy.call, by)
}

/// Whether `copy` on worker `by` holds a leg's dialling INVITE it sent that
/// has no final yet. The proxy's cookie on that INVITE names its sender, so
/// its responses come back to the copy alone (ADR-0055 §8), and the copy
/// plays on until the final.
fn dials_alone(copy: &Call, by: &RelayOwner) -> bool {
    copy.b_legs.iter().any(|leg| {
        leg.invite.as_ref().is_some_and(|record| record.relayed_by == *by)
            && leg.outbound_invite().is_some_and(|invite| invite.final_received.is_none())
    })
}

/// Whether `copy` on worker `by` holds no request whose final only it sees
/// that `record` — the primary's, or none once deleted — lacks: every request
/// record it keeps open — relayed, its own, or held for a later final, any
/// method — is the record's (`call::helpers::holds_records_of`). A final
/// follows the Via to the node that sent the request (RFC 3261 §18.2.2), and
/// a held request's final leaves on the server transaction only the copy
/// holds: a copy that relinquished either leaves its peer with no final, so
/// the copy plays on with it and self-releases once its transactions clear.
pub(super) fn finishes_nothing_alone(record: Option<&Call>, copy: &Call, by: &RelayOwner) -> bool {
    holds_records_of(record, copy, by)
}

#[cfg(test)]
mod tests;
