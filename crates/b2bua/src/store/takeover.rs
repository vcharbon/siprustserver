//! The live acting-backup takeover copies (ADR-0011 X11 / ADR-0014 /
//! ADR-0047): their marks, the probe the replication pullers read them by, and
//! the takeover episode their events fold into (ADR-0026).

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::{partition_of, CallState};

/// Whether this node serves `call_ref` as a live takeover copy.
pub type TakeoverProbe = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// The callRefs this node serves as live takeover copies, with their count
/// published outside the call map's lock: a reader that finds it zero knows
/// there is no copy without taking the lock.
#[derive(Default)]
pub(super) struct TakeoverMarks {
    refs: HashSet<String>,
    live: Arc<AtomicUsize>,
}

impl TakeoverMarks {
    fn insert(&mut self, call_ref: &str) {
        self.refs.insert(call_ref.to_string());
        self.live.store(self.refs.len(), Ordering::Release);
    }

    pub(super) fn remove(&mut self, call_ref: &str) {
        if self.refs.remove(call_ref) {
            self.live.store(self.refs.len(), Ordering::Release);
        }
    }

    pub(super) fn contains(&self, call_ref: &str) -> bool {
        self.refs.contains(call_ref)
    }

    pub(super) fn len(&self) -> usize {
        self.refs.len()
    }
}

impl CallState {
    /// Mark `call_ref` as a live acting-backup **takeover copy** (ADR-0011 X11 /
    /// ADR-0014). Called by the router's `materialise` on a fresh takeover. The
    /// router later reads it via [`is_takeover`](Self::is_takeover) to drive
    /// self-release once the served transaction(s) finish. Idempotent per call_ref.
    pub fn mark_takeover(&self, call_ref: &str) {
        self.inner.lock().unwrap().takeover.insert(call_ref);
    }

    /// Is `call_ref` a live acting-backup **takeover copy** (ADR-0014)? The router
    /// reads it after serving an event for the call: when true and the served
    /// transaction(s) have all cleared, it self-releases the live copy via
    /// [`drop_local`](Self::drop_local).
    pub fn is_takeover(&self, call_ref: &str) -> bool {
        self.inner.lock().unwrap().takeover.contains(call_ref)
    }

    /// [`is_takeover`](Self::is_takeover) as the replication pullers read it on
    /// every write they apply: while no copy is live it answers without taking
    /// the call map's lock.
    pub fn takeover_probe(&self) -> TakeoverProbe {
        let live = self.inner.lock().unwrap().takeover.live.clone();
        let state = self.clone();
        Arc::new(move |call_ref: &str| {
            live.load(Ordering::Acquire) != 0 && state.is_takeover(call_ref)
        })
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
