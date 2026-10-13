//! A takeover copy stands down on its primary's write (ADR-0047 D7). A write
//! the call's primary published, applied to this node's Element of a call it
//! serves as a live takeover copy, is the primary serving the call again. The
//! copy is released local-only — timers cancelled, transactions relinquished,
//! no store write, no CDR (ADR-0020 X3) — when it holds nothing the primary
//! lacks
//! ([`stands_down`]); otherwise it plays on and is shed at quiescence
//! (ADR-0014).
//!
//! The notice is a turn of the call's own queue, so a message queued for the
//! copy before it is served by the copy first (ADR-0010 X2), and the rule
//! reads the copy as that message left it.

use std::sync::Arc;

use call::helpers::caller_answered;
use call::{Call, CallModelState};

use super::release::{release_call, ReleaseKind};
use super::RouterCtx;
use b2bua_sdk::event::CallEvent;

/// The topic of the stand-down notice, an internal event the router raises
/// itself and its turn consumes before any rule reads it.
pub(super) const TOPIC: &str = "takeover-stand-down";

/// Queue the stand-down notice of `call_ref` carrying its primary's applied
/// `body`, for a call this node serves as a live takeover copy; nothing for
/// any other call.
pub(super) async fn offer(ctx: &Arc<RouterCtx>, call_ref: String, body: &[u8]) {
    if !ctx.state.is_takeover(&call_ref) {
        return;
    }
    let event = CallEvent::InternalEvent {
        call_ref,
        topic: TOPIC.to_string(),
        outcome: String::new(),
        payload: serde_json::Value::Null,
        body: body.to_vec(),
        incarnation: None,
    };
    super::ingress::on_event(ctx, event).await;
}

/// The stand-down turn of `call_ref`, its primary's applied body in `body`.
/// Read under the per-call lock, held across the release as in the
/// self-release turn (`quiesced`).
pub(super) async fn stand_down_turn(ctx: &Arc<RouterCtx>, call_ref: &str, body: &[u8]) {
    let guard = ctx.state.lock(call_ref).await;
    let Some(copy) = ctx.state.peek(call_ref) else {
        // Shed between the notice and its turn: what this turn's queue and
        // lock entry are is all that is left of the call here.
        drop(guard);
        release_call(ctx, call_ref, ReleaseKind::Orphan).await;
        return;
    };
    let Some(authored) = ctx.state.takeover_authored(call_ref) else { return };
    let Some(primary) = ctx.state.decode_body(body) else { return };
    if !stands_down(&primary, &copy, authored) {
        return;
    }
    // Folded into the departed peer's takeover episode (ADR-0026): a returning
    // node stands down every copy its peer's sweep made at once.
    ctx.state.note_takeover(call_ref, "stood_down");
    release_call(ctx, call_ref, ReleaseKind::StandDown).await;
}

/// Whether the takeover `copy` stands down on its primary's body `primary`:
/// the same call (one incarnation), and either
/// - **(i)** the copy has authored no write since it was materialised
///   (`authored == false`): its transactions are then the seeds, images of the
///   record's in-flight INVITEs the primary rebuilt on its own reclaim, so
///   nothing it holds is lost; or
/// - **(ii)** the copy's call is live with its caller unanswered while the
///   primary's caller is answered: the answer supersedes the ringing state the
///   copy serves. A copy that is ending holds a teardown and plays on.
pub(super) fn stands_down(primary: &Call, copy: &Call, authored: bool) -> bool {
    if primary.incarnation() != copy.incarnation() {
        return false;
    }
    let superseded =
        copy.state == CallModelState::Active && caller_answered(primary) && !caller_answered(copy);
    !authored || superseded
}

#[cfg(test)]
mod tests;
