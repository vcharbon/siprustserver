//! What a worker serves live: whether it serves a call, its copy, and the
//! store envelope beside it. Each reads nothing while the worker is crashed.

use super::ReplicatedB2buaSut;

impl ReplicatedB2buaSut {
    /// Does this node currently **serve** `call_ref` — hold it live, so it would
    /// emit the call's keepalive and answer in-dialog traffic? The cluster
    /// invariant is "exactly one node serves a given call" (see
    /// [`assert_single_owner`](crate::assert_single_owner)). `false` while crashed.
    pub fn serves(&self, call_ref: &str) -> bool {
        self.core.as_ref().map(|c| c.serves(call_ref)).unwrap_or(false)
    }

    /// Does this node hold `call_ref` reclaimed and parked, not armed until its
    /// own endpoint reads ready (ADR-0055 §8)? `false` while crashed.
    pub fn parks(&self, call_ref: &str) -> bool {
        self.core.as_ref().is_some_and(|c| c.parks(call_ref))
    }

    /// The live copy this worker serves for `call_ref`, if any — what its rules
    /// read at the next event.
    pub fn live_call(&self, call_ref: &str) -> Option<call::Call> {
        self.core.as_ref().and_then(|c| c.live_call(call_ref))
    }

    /// The store envelope of the live copy of `call_ref`; `None` while
    /// crashed or when no copy is live.
    pub fn live_topology(&self, call_ref: &str) -> Option<b2bua::store::CallTopology> {
        self.core.as_ref().and_then(|c| c.live_topology(call_ref))
    }
}
