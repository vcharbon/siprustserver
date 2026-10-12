//! The acting-backup self-release (ADR-0014): the transaction layer reports
//! the last transaction of a takeover copy cleared, and the copy is shed — the
//! `bak:` replica and the reverse-flushed deltas remain. The notice is a turn
//! of the call's own queue, so the turns of the messages that cleared those
//! transactions run first (ADR-0010 X2): a response still queued is served by
//! the copy, never by a second takeover after the shed.

use std::sync::Arc;

use super::release::{release_call, ReleaseKind};
use super::RouterCtx;

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
    // A takeover copy defers its discharge to the call's primary whatever its
    // state (ADR-0020 X3). A Terminating or Terminated copy re-flushes its
    // terminal state so the primary's reconcile or reclaim has it.
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
