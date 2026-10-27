//! Dropping a resident call from memory alone, with no store write and no
//! replication: a takeover copy's self-release and an orphan's lock entry.

use super::CallState;

impl CallState {
    /// **Local-only self-release teardown** (ADR-0014): drop a live acting-backup
    /// **takeover copy** from the in-memory map + routing index + takeover set,
    /// with **no** store mutation and **no** replication propagation — unlike
    /// [`remove`](Self::remove), which propagates a delete. The call lives on at
    /// its reclaiming primary (which is the node now forward-refreshing this
    /// node's backup `Element`); this node merely sheds the active role once the
    /// transaction(s) it served reached a terminal state. The router pairs this
    /// with timer / txn / dispatch teardown (it owns those). Returns `true` if a
    /// live copy was actually dropped (so the caller meters the self-release once).
    pub fn drop_local(&self, call_ref: &str) -> bool {
        let mut inner = self.locked();
        let was = inner.calls.remove(call_ref);
        inner.topologies.remove(call_ref);
        let present = was.is_some();
        if let Some(was) = was {
            inner.restate_uncounted(was.limiter.runs_uncounted(), false);
            self.publish_uncounted(&inner);
        }
        if let Some(keys) = inner.indexed.remove(call_ref) {
            inner.unindex(call_ref, &keys);
        }
        inner.locks.remove(call_ref);
        inner.takeover.remove(call_ref);
        inner.parked.remove(call_ref);
        inner.touched.remove(call_ref);
        inner.shrink_idle();
        present
    }

    /// Release the ephemeral per-call state an **orphan-reject** created — the
    /// 481 path for an in-dialog request that resolved to `call_ref` but hydrated
    /// **no** live call (a failed-over BYE for a dialog that was never reclaimed,
    /// or a late in-dialog request after teardown). [`process`] acquired the
    /// per-call [`lock`](Self::lock) — and the router spun up a per-call dispatch
    /// queue (one `bump_creation`) — yet there is nothing to [`remove`](Self::remove):
    /// the call was never in the map. Drop the artifact this leaves behind —
    /// the `locks`-map entry — with **no** store
    /// mutation (unlike `remove`, which would reverse-propagate a *spurious
    /// delete* for a call we never held).
    ///
    /// Paired with the router poisoning the dispatch queue (so the worker exits and
    /// `removals` balances `creations`), this is what stops an **orphan storm** —
    /// the thousands of failed-over BYEs that hit a rebooted worker — from leaking
    /// the `locks` map and the `active_calls` (creations−removals) count: each
    /// orphan would otherwise strand one lock entry, one idle worker task and
    /// one unmatched creation for good.
    ///
    /// [`process`]: crate::router
    pub fn discard_orphan(&self, call_ref: &str) {
        let mut inner = self.locked();
        inner.locks.remove(call_ref);
        inner.shrink_idle();
    }
}
