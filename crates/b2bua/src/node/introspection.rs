//! What a test or an observer reads of a running [`B2buaCore`]: its live
//! calls and the per-call state they hold, and the surgery a failover test
//! performs on them.

use std::sync::Arc;

use super::B2buaCore;

impl B2buaCore {
    /// Active call count (test/observability).
    pub fn active_calls(&self) -> usize {
        self.ctx.state.active_count()
    }

    /// [`active_calls`](Self::active_calls) as an OWNED probe — the live-call
    /// input of [`drain_probe`](Self::drain_probe), handed out on its own for a
    /// caller that watches quiescence alone. Read-only.
    pub fn active_calls_probe(&self) -> Arc<dyn Fn() -> usize + Send + Sync> {
        let ctx = self.ctx.clone();
        Arc::new(move || ctx.state.active_count())
    }

    /// Does this worker currently **serve** `call_ref` (hold it live in its call
    /// map, armed — i.e. it would emit the call's keepalive and answer in-dialog
    /// traffic)? A parked call is not served. The cluster-level invariant the
    /// failover tests assert is "exactly one node serves a given call".
    /// Test/observability.
    pub fn serves(&self, call_ref: &str) -> bool {
        self.ctx.state.contains(call_ref) && !self.ctx.state.is_parked(call_ref)
    }

    /// Does this worker hold `call_ref` reclaimed and parked, resident and not
    /// armed until its own endpoint reads ready (ADR-0055 §8)? Test/observability.
    pub fn parks(&self, call_ref: &str) -> bool {
        self.ctx.state.is_parked(call_ref)
    }

    /// Cancel a timer in this node's driver, the record untouched: a test's
    /// stand-in for a fire the per-call queue dropped.
    pub async fn cancel_driver_timer(&self, call_ref: &str, key: &call::TimerKey) {
        self.ctx.timers.cancel(call_ref.to_string(), key.clone()).await;
    }

    /// Post `event` to the router as a timer driver or a callout posts one: a
    /// test's stand-in for an event already in flight when it is posted.
    pub fn post_event(&self, event: b2bua_sdk::event::CallEvent) {
        let _ = self.ctx.reentry_tx.send(event);
    }

    /// The live copy of `call_ref` this worker serves, if any (introspection:
    /// what its rules read at the next event).
    pub fn live_call(&self, call_ref: &str) -> Option<call::Call> {
        self.ctx.state.peek(call_ref)
    }

    /// The store envelope of the live copy of `call_ref` (introspection: the
    /// worker pair, the version vector and the state stamp, ADR-0055 §7).
    pub fn live_topology(&self, call_ref: &str) -> Option<crate::store::CallTopology> {
        self.ctx.state.topology(call_ref)
    }

    /// Every call this worker serves, read through a probe that outlives a
    /// borrow of the core and reads `None` once the worker's state is gone
    /// (introspection: a failure dump's view of the live calls).
    pub fn live_calls_probe(&self) -> Arc<dyn Fn() -> Option<Vec<call::Call>> + Send + Sync> {
        let ctx = Arc::downgrade(&self.ctx);
        Arc::new(move || {
            let ctx = ctx.upgrade()?;
            let mut refs = ctx.state.live_call_refs();
            refs.sort();
            Some(refs.iter().filter_map(|r| ctx.state.peek(r)).collect())
        })
    }

    /// New calls admitted at ingress whose turn has not created their call
    /// yet; 0 once every admitted turn has run or been dropped.
    pub fn unborn_calls(&self) -> u64 {
        self.ctx.unborn.count()
    }

    /// Live per-call serialization-lock count (test/observability). Should track
    /// [`active_calls`](Self::active_calls); a gap is the orphan-reject lock leak.
    pub fn lock_count(&self) -> usize {
        self.ctx.state.lock_count()
    }

    /// Live last-touched ledger entries (the reaper's liveness stamps,
    /// ADR-0020 X4). Mirrors the call map by construction; a residue after
    /// teardown is a stamp leak (the harness reap oracle's 4th invariant).
    pub fn touched_count(&self) -> usize {
        self.ctx.state.touched_count()
    }

    /// Live setup-CANCEL marks (the decision-application drop guard). Cleared on
    /// every teardown path; a residue after teardown is a mark leak (the harness
    /// reap oracle's 5th invariant).
    pub fn setup_cancelled_count(&self) -> usize {
        self.ctx.state.setup_cancelled_count()
    }

    /// HARNESS SURGERY: drop the live in-memory copy of `call_ref` — map, index,
    /// lock, takeover mark — with NO store mutation (the `pri:`/`bak:` replica
    /// bodies stay). Recreates, deterministically, the rebooted-primary
    /// mid-reclaim state ("body imported into `pri:{self}`, not yet
    /// materialised") that the bulk-`ReclaimAll` race only produces under
    /// timing: the failover tests use it to pin the on-demand reclaim read-path.
    /// Test/observability only — production teardown goes through the router's
    /// `release_call`.
    pub fn drop_live_copy(&self, call_ref: &str) -> bool {
        self.ctx.state.drop_local(call_ref)
    }
}
