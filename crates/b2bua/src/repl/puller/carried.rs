//! The copies the cold passes of one bootstrap carried, and the sweep of the
//! ones its last pass did not carry (ADR-0014 §8). A call its holder ends
//! between two cold passes is in neither the second scan nor its tail, which
//! starts past the delete: once that pass catches up, the copy an earlier pass
//! left is dropped as the `Delete` the tail never sent would have dropped it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use repl_net::frame::Watermark;

use super::Puller;
use crate::router::{PrimaryWrite, ReplCommand};
use crate::store::{partition_of, PartitionRole, PutOpts};

/// What the cold passes of one bootstrap of a `(peer, flow)` carried. The
/// supervisor keeps it beside the flow's watermark, so a puller spawned
/// between two passes reads what the earlier ones carried. It holds at most
/// one entry per key of the flow and empties when a pass catches up.
#[derive(Clone, Default)]
pub(crate) struct CarriedCopies(Arc<Mutex<Passes>>);

/// One stream of the flow, as the record counts its frames: only the stream
/// opened last does, so a puller cancelled for a respawn and still finishing
/// a step neither records nor sweeps for its successor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PassId(u64);

#[derive(Default)]
struct Passes {
    /// The streams opened on the flow so far; the last one is the current pass.
    opened: u64,
    /// The peer incarnation the recorded passes were read from: what a pass
    /// of another incarnation lacks says nothing of what this one ended.
    gen: Option<u64>,
    /// Each key a pass carried that the store held after its frame, with the
    /// copy held then.
    left: HashMap<String, StoredCopy>,
    /// The keys a frame of the current pass carried, scan or tail.
    this_pass: HashSet<String>,
    /// A hard timer's bulk reclaim served a partial seed: nothing is recorded
    /// or swept until a pass catches up.
    reclaimed_early: bool,
}

impl Passes {
    fn current(&self, pass: PassId) -> bool {
        pass == PassId(self.opened)
    }

    /// Forget every pass recorded, keeping the count of streams opened.
    fn restart(&mut self, gen: Option<u64>, reclaimed_early: bool) {
        *self = Passes { opened: self.opened, gen, reclaimed_early, ..Passes::default() };
    }
}

/// A stored copy as the sweep tells it from a later write: its version and
/// call incarnation.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredCopy {
    version: (i64, i64),
    incarnation: Option<String>,
}

impl CarriedCopies {
    fn passes(&self) -> MutexGuard<'_, Passes> {
        crate::poison::locked(&self.0, crate::poison::PULLER_CARRIED_COPIES)
    }

    /// A stream of the flow opens and becomes the current pass: none of its
    /// keys is carried yet, and no earlier stream's frame counts any more.
    pub(super) fn open_pass(&self) -> PassId {
        let mut passes = self.passes();
        passes.opened += 1;
        passes.this_pass.clear();
        PassId(passes.opened)
    }

    /// A frame of `pass`, read from peer incarnation `gen`, carried `key`, of
    /// which the store holds `left` now. A frame of another incarnation than
    /// the recorded passes' forgets them first.
    fn carried(&self, pass: PassId, gen: u64, key: &str, left: Option<StoredCopy>) {
        let mut passes = self.passes();
        if passes.reclaimed_early || !passes.current(pass) {
            return;
        }
        if passes.gen != Some(gen) {
            passes.restart(Some(gen), false);
        }
        passes.this_pass.insert(key.to_string());
        match left {
            Some(copy) => passes.left.insert(key.to_string(), copy),
            None => passes.left.remove(key),
        };
    }

    /// `pass` caught up at a position of peer incarnation `gen`: the copies
    /// earlier passes of that incarnation left and this one did not carry,
    /// each as it was held then, and the record empties. Nothing for a pass
    /// that is no longer the current one.
    fn caught_up(&self, pass: PassId, gen: u64) -> Vec<(String, StoredCopy)> {
        let mut passes = self.passes();
        if !passes.current(pass) {
            return Vec::new();
        }
        let left = std::mem::take(&mut passes.left);
        let this_pass = std::mem::take(&mut passes.this_pass);
        let sweeps = !passes.reclaimed_early && passes.gen == Some(gen);
        passes.restart(None, false);
        if !sweeps {
            return Vec::new();
        }
        left.into_iter().filter(|(key, _)| !this_pass.contains(key)).collect()
    }

    /// A hard timer's bulk reclaim served a partial seed: this bootstrap
    /// sweeps nothing (the residual of ADR-0014 §8).
    pub(super) fn reclaimed_early(&self) {
        self.passes().restart(None, true);
    }
}

impl Puller {
    /// Read the passes of this bootstrap from `carried`, the flow's record the
    /// supervisor keeps across a respawn.
    pub(crate) fn with_carried(mut self, carried: CarriedCopies) -> Self {
        self.carried = carried;
        self
    }

    /// Where this flow stores `call_ref`: `pri:{self}` on the Reclaim flow,
    /// `bak:{primary}` on the Backup flow.
    fn keyspace(&self, call_ref: &str) -> (PartitionRole, String) {
        let role = if self.is_reclaim() { PartitionRole::Primary } else { PartitionRole::Backup };
        (role, partition_of(&self.self_ordinal, call_ref).1)
    }

    /// The copy of `call_ref` this flow's keyspace holds, `None` when absent
    /// or expired.
    fn held_copy(&self, call_ref: &str) -> Option<StoredCopy> {
        let (role, primary) = self.keyspace(call_ref);
        let version = self.store.current_cv(role, &primary, call_ref)?;
        Some(StoredCopy { version, incarnation: self.store.incarnation(call_ref) })
    }

    /// A `Data` frame of the cold `pass`, before its first catch-up `Noop`,
    /// carried `call_ref` at `at`, whatever the store made of it.
    pub(super) fn note_carried(&self, pass: PassId, at: Watermark, call_ref: &str) {
        self.carried.carried(pass, at.gen, call_ref, self.held_copy(call_ref));
    }

    /// The cold `pass` caught up at `at`, before the flow reads current or its
    /// bootstrap complete: each copy an earlier pass of the bootstrap left
    /// that this pass did not carry is dropped, local-only
    /// ([`drop_uncarried`](Self::drop_uncarried)). On the Backup flow a live
    /// takeover copy hears each drop as its primary's applied `Delete`
    /// (ADR-0047 D7); on the Reclaim flow the router is told of each drop as
    /// of a backup's applied delete (ADR-0047 D8), ahead of the bulk reclaim.
    /// Counted on `b2bua_repl_bootstrap_swept_total` and lined once when any
    /// copy went.
    pub(super) async fn sweep_uncarried(&self, pass: PassId, at: Watermark) {
        let mut swept = 0u64;
        for (call_ref, copy) in self.carried.caught_up(pass, at.gen) {
            if self.drop_uncarried(&call_ref, copy).await {
                swept += 1;
            }
        }
        if swept == 0 {
            return;
        }
        self.metrics.record_repl_bootstrap_swept(self.flow_label(), swept);
        self.note_state(
            "bootstrap_swept",
            &format!("dropped {swept} copies an earlier pass carried and the last one did not"),
        );
    }

    /// Drop `call_ref`, held as `copy` when a pass carried it; `false` when it
    /// is spared or already gone. Another call under the key is spared on
    /// either flow. On the Backup flow the drop is the holder's `Delete` the
    /// tail never sent: it spares only the Element the `Delete` guard keeps,
    /// and a live takeover copy is handed the delete as its primary's
    /// (ADR-0047 D7). On the Reclaim flow a copy written since, by this node
    /// or a bulk reclaim, is spared. Beyond a tail `Delete`, any change of call
    /// incarnation spares, and a spare counts no refused forward flush.
    async fn drop_uncarried(&self, call_ref: &str, copy: StoredCopy) -> bool {
        let (role, primary) = self.keyspace(call_ref);
        if !self.is_reclaim()
            && self.element_outlives_the_delete(role, &primary, call_ref, false).await
        {
            return false;
        }
        let Some(held) = self.held_copy(call_ref) else { return false };
        if held.incarnation != copy.incarnation
            || (self.is_reclaim() && held.version != copy.version)
        {
            return false;
        }
        let indexes = self.store.held_indexes(call_ref);
        let opts = PutOpts { incarnation: copy.incarnation.clone(), ..PutOpts::default() };
        let _ = self.store.write_delete(role, &primary, call_ref, &indexes, false, &opts);
        let Some(sink) = self.repl_tx.as_ref() else { return true };
        let (call_ref, incarnation) = (call_ref.to_string(), copy.incarnation);
        if self.is_reclaim() {
            sink.send(ReplCommand::ReverseDeleteApplied { call_ref, incarnation });
        } else if sink.serves_takeover(&call_ref) {
            let write = PrimaryWrite::Delete(incarnation);
            sink.send(ReplCommand::ForwardFlushApplied { call_ref, write });
        }
        true
    }
}

#[cfg(test)]
mod tests;
