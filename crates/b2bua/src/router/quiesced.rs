//! The acting-backup self-release (ADR-0014): the transaction layer reports
//! the last transaction of a takeover copy cleared, and the copy is shed — the
//! `bak:` replica and the reverse-flushed deltas remain. The notice is a turn
//! of the call's own queue, so the turns of the messages that cleared those
//! transactions run first (ADR-0010 X2): a response still queued is served by
//! the copy, never by a second takeover after the shed.

use std::sync::Arc;

use super::release::{release_call, ReleaseKind};
use super::RouterCtx;
use b2bua_sdk::event::CallEvent;

/// The topic of the self-release notice, an internal event the router raises
/// itself and its turn consumes before any rule reads it.
pub(super) const TOPIC: &str = "takeover-self-release";

/// The self-release notice of `call_ref`: the transaction layer reports the
/// last transaction of the call it watches cleared.
pub(super) fn notice(call_ref: String) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref,
        topic: TOPIC.to_string(),
        outcome: String::new(),
        payload: serde_json::Value::Null,
        body: Vec::new(),
        incarnation: None,
    }
}

/// Whether `event` is the self-release notice ([`notice`]), and of which call.
pub(super) fn notice_of(event: &CallEvent) -> Option<&str> {
    match event {
        CallEvent::InternalEvent { call_ref, topic, .. } if topic == TOPIC => Some(call_ref),
        _ => None,
    }
}

/// The self-release turn of `call_ref`. Re-checked under the per-call lock,
/// and the guard is held ACROSS the release — dropping it between the re-check
/// and `drop_local` re-opens the double-serve window (ADR-0014): a parked
/// handler could hydrate the still-resident copy unmarked and unwatched and
/// re-insert it after `drop_local`. `release_call` takes no per-call lock
/// itself, so holding the guard across it is deadlock-free.
pub(super) async fn self_release_turn(ctx: &Arc<RouterCtx>, call_ref: &str) {
    let guard = ctx.state.lock(call_ref).await;
    if !ctx.state.is_takeover(call_ref) {
        // Shed between the notice and its turn: what this turn's queue and
        // lock entry are is all that is left of the call here.
        if ctx.state.peek(call_ref).is_none() {
            drop(guard);
            release_call(ctx, call_ref, ReleaseKind::Orphan).await;
        }
        return;
    }
    if ctx.txn.active_txn_count_for_call(call_ref).await.unwrap_or(0) != 0 {
        // A transaction opened since the notice, and the layer's watch is
        // one-shot — it was consumed delivering this notice. Re-arm it so the
        // next clear notifies again.
        let _ = ctx.txn.watch_self_release(call_ref).await;
        return;
    }
    shed(ctx, call_ref).await;
}

/// Shed `call_ref`'s takeover copy before a timer of its own runs, when no
/// transaction it serves is left: a copy is resident exactly while one is
/// live (ADR-0014), so its own deadlines — a 2xx ladder's give-up racing the
/// clear of that 2xx's transaction — are its primary's to keep. `true` when the
/// copy was shed and the timer's turn ends here.
pub(super) async fn shed_if_quiesced(ctx: &Arc<RouterCtx>, call_ref: &str) -> bool {
    if !ctx.state.is_takeover(call_ref) {
        return false;
    }
    let _guard = ctx.state.lock(call_ref).await;
    if !ctx.state.is_takeover(call_ref)
        || ctx.txn.active_txn_count_for_call(call_ref).await.unwrap_or(0) != 0
    {
        return false;
    }
    shed(ctx, call_ref).await;
    true
}

/// Shed the takeover copy of `call_ref`, under its per-call lock. A takeover
/// copy defers its discharge to the call's primary whatever its state
/// (ADR-0020 X3); a Terminating or Terminated copy re-flushes its terminal
/// state so the primary's reconcile or reclaim has it.
async fn shed(ctx: &Arc<RouterCtx>, call_ref: &str) {
    if let Some(call) = ctx.state.peek(call_ref) {
        if matches!(
            call.state,
            call::CallModelState::Terminating | call::CallModelState::Terminated
        ) {
            ctx.state.flush(&call);
        }
    }
    release_call(ctx, call_ref, ReleaseKind::SelfRelease { ended: false }).await;
}
