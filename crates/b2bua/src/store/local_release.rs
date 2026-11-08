//! Dropping a resident call from memory alone, with no store write and no
//! replication.

use super::CallState;

impl CallState {
    /// **Local-only self-release teardown** (ADR-0014): drop a live acting-backup
    /// **takeover copy** from the in-memory map + routing index + takeover set,
    /// with **no** store mutation and **no** replication propagation — unlike
    /// [`remove`](Self::remove), which propagates a delete. The call lives on at
    /// its reclaiming primary (which is the node now forward-refreshing this
    /// node's backup `Element`); this node merely sheds the active role once the
    /// transaction(s) it served reached a terminal state. The router pairs this
    /// with timer / txn / dispatch teardown (it owns those), under the call's
    /// lock, whose entry then goes with its last guard. Returns `true` if a
    /// live copy was actually dropped (so the caller meters the self-release once).
    pub fn drop_local(&self, call_ref: &str) -> bool {
        let mut inner = self.locked();
        let was = inner.calls.remove(call_ref);
        inner.topologies.remove(call_ref);
        let present = was.is_some();
        if let Some(was) = &was {
            inner.restate_uncounted(was.limiter.runs_uncounted(), false);
            self.publish_uncounted(&inner);
        }
        if let Some(keys) = inner.indexed.remove(call_ref) {
            inner.unindex(call_ref, &keys);
        }
        inner.retire_lock(call_ref);
        inner.takeover.remove(call_ref);
        inner.parked.remove(call_ref);
        inner.touched.remove(call_ref);
        inner.shrink_idle();
        drop(inner);
        drop(was);
        present
    }
}
