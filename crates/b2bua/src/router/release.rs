//! The single per-call teardown executor: every path that frees per-call
//! runtime state funnels through [`release_call`].

use std::sync::Arc;

use super::RouterCtx;
use crate::metrics::RemovalClass;

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
    /// remain, so the call lives on at its reclaiming primary. `ended`: the
    /// copy terminated here, so no request it served will be answered — as for
    /// `Terminated`, what has no final is forgotten or answered. A live copy
    /// shed at quiescence holds no open transaction, and touches none: a
    /// request crossing the shed is still served, and a copy of it forgotten
    /// or answered here would be served twice.
    SelfRelease { ended: bool },
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
    // A traced call's root span closes here — the ONE place every release funnels
    // through — so the active-trace slot returns exactly when the call's runtime
    // state does (ADR-0026). Idempotent, and a no-op for an unsampled call.
    crate::trace::traces().close(call_ref);
    match kind {
        ReleaseKind::Terminated => {
            ctx.state.remove(call_ref);
            // Idempotent with an explicit `CancelAllTimers` effect, but not
            // dependent on every rule remembering to emit one: a terminated call
            // frees EVERY timer slot it owns now, not at its deadline.
            ctx.timers.cancel_all(call_ref.to_string()).await;
            let _ = ctx.txn.cancel_txns_for_call(call_ref).await;
            // Nothing answers a request of this call any more: its handler ran
            // and died (ADR-0020), or the answer rode an effect that will not
            // come. This turn's own answers are already sent (`RemoveCall` is
            // interpreted last), so what still has no final is forgotten and
            // its retransmission meets the orphan path.
            let _ = ctx.txn.forget_unanswered_of_call(call_ref).await;
            answer_unanswered_invites(ctx, call_ref).await;
            // Poison the per-call dispatch queue; its worker exits and bumps
            // `removal` exactly once (dispatch.rs). We deliberately do NOT
            // bump here — removal is counted at the single dispatch-queue
            // teardown site so creations/removals stay a matched pair.
            ctx.dispatcher.enqueue_poison(call_ref, RemovalClass::Terminated);
        }
        ReleaseKind::SelfRelease { ended } => {
            if ctx.state.drop_local(call_ref) {
                ctx.timers.cancel_all(call_ref.to_string()).await;
                let _ = ctx.txn.cancel_txns_for_call(call_ref).await;
                if ended {
                    let _ = ctx.txn.forget_unanswered_of_call(call_ref).await;
                    answer_unanswered_invites(ctx, call_ref).await;
                }
                ctx.dispatcher.enqueue_poison(call_ref, RemovalClass::SelfRelease);
                ctx.metrics.bump_repl_self_release();
                // Folded into the dead peer's takeover episode, never its own
                // line: shedding is the tail of the takeover it ends.
                ctx.state.note_takeover_self_release(call_ref);
            }
        }
        ReleaseKind::Orphan => {
            ctx.state.discard_orphan(call_ref);
            ctx.dispatcher.enqueue_poison(call_ref, RemovalClass::Orphan);
        }
    }
}

/// Answer 481 every in-dialog INVITE of the ended call `call_ref` that has no
/// final (RFC 3261 §12.2.2: the dialog is gone). Its 100 Trying stopped the
/// peer's retransmissions, so unlike a non-INVITE it is not forgotten for a
/// retransmission to meet the orphan path. The layer answers through the
/// transaction, which then absorbs the ACK.
async fn answer_unanswered_invites(ctx: &RouterCtx, call_ref: &str) {
    let _ = ctx
        .txn
        .answer_unanswered_invites_of_call(call_ref, 481, "Call/Transaction Does Not Exist")
        .await;
}
