//! The periodic replica-store reap (ADR-0020 X3, ADR-0055 §5): expired bodies
//! evicted, the limiter holds of a deferred terminal no primary came back for
//! released with its CDR counted lost, and a call no node ended whose primary
//! departed ended here with its CDR.

use std::sync::Arc;

use call::Call;

use super::super::RouterCtx;

/// Periodic replica-store maintenance. A deferred terminal whose primary never came back to reclaim
/// it (crashed for good, past the replica TTL = `reboot_budget`) must NOT be left
/// to pin its limiter slot or leak its replica body. `reap_replica` evicts every
/// expired body and hands back the deferred terminals among them, whichever
/// partition holds the body (a replica its primary flushed, or a takeover copy's
/// own terminal); each one is released on the limiter by its key — the SAME
/// `release(key)` the discharge funnel emits — and counted as a lost-CDR cleanup:
/// limiter freed, memory freed, **CDR lost**. An expired body is returned by one
/// pass only, and no read evicts it before this pass sees it.
///
/// A primary that departs before reconciling a deferred terminal has it
/// discharged by this node's departure sweep (ADR-0047 D8), not here. A release
/// this pass sends for a call another node already released frees nothing: the
/// release is keyed by the call. Spawned as a paced task by `b2bua_core`.
///
/// A ghost — a call no node ended, whose replica outlived its TTL — of a
/// primary that departed has no other node to end it: this node does, off the
/// wire, its legs forced terminal and its CDR written, the call's only one
/// (ADR-0055 §5). A ghost of a primary still a member is evicted silently:
/// that primary holds the call.
pub(crate) async fn reap_expired_replicas(ctx: &Arc<RouterCtx>, now_ms: i64) {
    let reaped = ctx.state.reap_replica(now_ms).await;
    for ghost in reaped.ghosts {
        let call_ref = ghost.call.call_ref.clone();
        if !super::super::interpret::primary_departed(ctx, &call_ref) {
            continue;
        }
        let guard = ctx.state.lock(&call_ref).await;
        if !ctx.state.contains(&call_ref) {
            super::discharge_materialized_terminal(ctx, &call_ref, ghost, now_ms, true).await;
        }
        drop(guard);
    }
    for terminal in reaped.deferred_terminals {
        // No per-call lock: the body is already evicted and this node does not
        // serve the call live (a takeover copy self-released at its terminal), so
        // there is no writer to serialize against — and taking the lock would leak
        // a `locks` map entry (only `release_call`/`discard_orphan` clear it). The
        // decoded snapshot is all the limiter release needs.
        release_orphaned_limiter_holds(ctx, &terminal.call);
        ctx.metrics.bump_repl_terminal_lost();
    }
}

/// Release the cluster-wide limiter set a never-reclaimed deferred terminal
/// still owns (`CallLimiterState::owed_release`: a call that sent no admit
/// owes nothing), WITHOUT writing a CDR or propagating a delete. No turn
/// serves the body, so the release goes to the worker directly. The backup is
/// the only node that can free this slot once its primary is dead for good.
fn release_orphaned_limiter_holds(ctx: &Arc<RouterCtx>, call: &Call) {
    if let Some(key) = call.limiter.owed_release() {
        ctx.limiter.release(key);
    }
}
