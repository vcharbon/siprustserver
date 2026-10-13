//! The live acting-backup takeover copies (ADR-0011 X11 / ADR-0014 /
//! ADR-0047): their marks — each with the `b` its copy was materialised at —
//! and the takeover episode their events fold into (ADR-0026).

use std::sync::Arc;

use call::Call;

use super::{partition_of, CallState};
use crate::repl::TakeoverProbe;

/// The `b` counter of `call`'s version vector; `0` for a call with no
/// topology.
fn bak_gen_of(call: &Call) -> i64 {
    call.topology.as_ref().map_or(0, |t| t.bak_gen)
}

impl CallState {
    /// Mark `call_ref` as a live acting-backup **takeover copy**, recording the
    /// `b` of the resident copy. Called by the router's `materialise` on a fresh
    /// takeover. The router later reads it via [`is_takeover`](Self::is_takeover)
    /// to drive self-release once the served transaction(s) finish.
    pub fn mark_takeover(&self, call_ref: &str) {
        let mut inner = self.inner.lock().unwrap();
        let materialised = inner.calls.get(call_ref).map_or(0, |c| bak_gen_of(c));
        inner.takeover.insert(call_ref.to_string(), materialised);
    }

    /// Is `call_ref` a live acting-backup **takeover copy** (ADR-0014)? The router
    /// reads it after serving an event for the call: when true and the served
    /// transaction(s) have all cleared, it self-releases the live copy via
    /// [`drop_local`](Self::drop_local).
    pub fn is_takeover(&self, call_ref: &str) -> bool {
        self.inner.lock().unwrap().takeover.contains_key(call_ref)
    }

    /// Whether the live takeover copy of `call_ref` has authored a write since
    /// it was materialised: its `b` moved past the one it was marked with.
    /// One `b` increment on a copy is one [`update`](Self::update) this node
    /// made as the call's acting backup — a turn that changed the call, a fold
    /// included; a quiet turn and an adopted version move nothing. `None` when
    /// `call_ref` is no live takeover copy here.
    pub fn takeover_authored(&self, call_ref: &str) -> Option<bool> {
        let inner = self.inner.lock().unwrap();
        let materialised = *inner.takeover.get(call_ref)?;
        let live = inner.calls.get(call_ref)?;
        Some(bak_gen_of(live) > materialised)
    }

    /// [`is_takeover`](Self::is_takeover) as the replication pullers read it.
    pub fn takeover_probe(&self) -> TakeoverProbe {
        let state = self.clone();
        Arc::new(move |call_ref: &str| state.is_takeover(call_ref))
    }

    /// Fold one takeover event for `call_ref` into its dead primary's episode
    /// (ADR-0026 aggregation): the counter names what happened, the peer the
    /// `call_ref` encodes keys the episode.
    pub(crate) fn note_takeover(&self, call_ref: &str, counter: &'static str) {
        self.note_takeover_count(call_ref, counter, 1);
    }

    /// [`note_takeover`](Self::note_takeover) for a counter that moves by `n`
    /// at once — the transactions one materialisation seeded.
    pub(crate) fn note_takeover_count(&self, call_ref: &str, counter: &'static str, n: u64) {
        let (_, primary) = partition_of(&self.self_ordinal, call_ref);
        self.takeover_log.record(&primary, counter, n);
    }

    /// Fold an acting-backup **self-release** into the dead peer's takeover
    /// episode — the shedding that ends a takeover is part of the same story,
    /// so it never gets its own line.
    pub fn note_takeover_self_release(&self, call_ref: &str) {
        self.note_takeover(call_ref, "self_released");
    }
}
