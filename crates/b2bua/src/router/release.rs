//! The single per-call teardown executor: every path that frees per-call
//! runtime state funnels through [`release_call`].

use std::sync::Arc;

use super::RouterCtx;

/// How a call's per-node runtime state is being released. Every path that frees
/// per-call state funnels through [`release_call`] — the ONE teardown executor —
/// so no path can forget a step of the "all per-call state MUST be released at
/// call end" invariant (CLAUDE.md). The three kinds differ ONLY in which side
/// effects they must NOT perform (encoded here, not in comments):
pub(super) enum ReleaseKind {
    /// Terminated call: evict from map/index/store and **propagate the delete**
    /// to the replica peer (the `RemoveCall` critical effect).
    Terminated,
    /// **Acting-backup self-release** (ADR-0014): shed a reactive takeover copy
    /// once the transaction(s) the backup served for it have all reached a
    /// terminal state. Local-only — **no** store mutation, **no** delete
    /// propagation: the `bak:{primary}` replica and the reverse-flushed deltas
    /// remain, so the call lives on at its reclaiming primary.
    SelfRelease,
    /// Orphan reject: the 481 path hydrated NO call — only the lock entry and
    /// the dispatch queue exist. **No** store mutation (a `remove` would
    /// reverse-propagate a spurious delete), **no** timers/txns were armed.
    Orphan,
}

/// The single per-call teardown executor (see [`ReleaseKind`]). Owns the full
/// release checklist — map/index entry, store propagation, per-call lock,
/// takeover mark, timers (physical `try_remove`, CLAUDE.md), transactions, and
/// the dispatch queue — so the released-at-call-end invariant lives in ONE
/// place instead of per-path hand-maintained copies.
pub(super) async fn release_call(ctx: &Arc<RouterCtx>, call_ref: &str, kind: ReleaseKind) {
    match kind {
        ReleaseKind::Terminated => {
            ctx.state.remove(call_ref);
            // Idempotent with an explicit `CancelAllTimers` effect, but not
            // dependent on every rule remembering to emit one: a terminated call
            // frees EVERY timer slot it owns now, not at its deadline.
            ctx.timers.cancel_all(call_ref.to_string()).await;
            let _ = ctx.txn.cancel_txns_for_call(call_ref).await;
            // Poison the per-call dispatch queue; its worker exits and bumps
            // `removal` exactly once (dispatch.rs). We deliberately do NOT
            // bump here — removal is counted at the single dispatch-queue
            // teardown site so creations/removals stay a matched pair.
            ctx.dispatcher.enqueue_poison(call_ref);
        }
        ReleaseKind::SelfRelease => {
            if ctx.state.drop_local(call_ref) {
                ctx.timers.cancel_all(call_ref.to_string()).await;
                let _ = ctx.txn.cancel_txns_for_call(call_ref).await;
                ctx.dispatcher.enqueue_poison(call_ref);
                ctx.metrics.bump_repl_self_release();
            }
        }
        ReleaseKind::Orphan => {
            ctx.state.discard_orphan(call_ref);
            ctx.dispatcher.enqueue_poison(call_ref);
        }
    }
}
