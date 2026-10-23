//! A live copy of a call its backup discharged is dropped (ADR-0047 D8). A
//! backup's takeover copy that ends the call of a departed primary writes the
//! CDR, releases the limiter key and deletes the call; the delete reaches the
//! primary's ordinal on its reclaim stream. A node of that ordinal that still
//! holds the call live — the departed process itself, still running, or a
//! replacement that reclaimed the call before the backup saw it rejoin —
//! drops that copy local-only: timers cancelled, transactions relinquished, no
//! store write, no CDR. The backup's discharge is then the call's only one.
//!
//! The notice is a turn of the call's own queue, so a message queued for the
//! call before it is served first (ADR-0010 X2). Only a delete naming the live
//! call's incarnation, or naming none, drops it; a delete of another call on
//! the same ref leaves it.

use std::sync::Arc;

use super::release::{release_call, ReleaseKind};
use super::RouterCtx;
use b2bua_sdk::event::CallEvent;

/// The topic of the notice, an internal event the router raises itself and
/// its turn consumes before any rule reads it.
pub(super) const TOPIC: &str = "discharged-by-backup";

/// Queue the drop notice of `call_ref`, whose backup's delete naming
/// `incarnation` this node applied, whatever is live here now: the turn
/// decides under the per-call lock. A reclaim of the call materialises it
/// under that lock too, reading the body there, so either it ran first and the
/// turn finds the call live and drops it, or the turn ran first and the
/// reclaim finds the body deleted and materialises nothing.
pub(super) async fn offer(ctx: &Arc<RouterCtx>, call_ref: String, incarnation: Option<String>) {
    let event = CallEvent::InternalEvent {
        call_ref,
        topic: TOPIC.to_string(),
        outcome: String::new(),
        payload: serde_json::json!(incarnation),
        body: Vec::new(),
        incarnation: None,
    };
    super::ingress::on_event(ctx, event, false).await;
}

/// The drop turn of `call_ref` on the notice's `payload`, the incarnation the
/// delete named ([`offer`]). Read under the per-call lock, held across the
/// release.
pub(super) async fn drop_turn(ctx: &Arc<RouterCtx>, call_ref: &str, payload: &serde_json::Value) {
    let guard = ctx.state.lock(call_ref).await;
    let Some(live) = ctx.state.peek(call_ref) else {
        // Released between the notice and its turn: this turn's queue and
        // lock entry are all that is left of the call here.
        drop(guard);
        release_call(ctx, call_ref, ReleaseKind::Orphan).await;
        return;
    };
    if ctx.state.is_takeover(call_ref) {
        return;
    }
    if payload.as_str().is_none_or(|deleted| deleted == live.incarnation()) {
        release_call(ctx, call_ref, ReleaseKind::DischargedByBackup).await;
    }
}

#[cfg(test)]
mod tests;
