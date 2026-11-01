//! Whether a peer holds a copy of a call (ADR-0055 §3), read from the change
//! log alone: the position the peer, under the identity bound to it, reported
//! applying, against the first counter the call's entry was sent at, or, for
//! a call with no entry (written before the sub-log opened), against the
//! counter the sub-log opened at.

use repl_net::frame::{Partition, Watermark};

use super::Changelog;
use crate::store::{locked, REPLICA_CHANGELOG};

impl Changelog {
    /// Whether `peer`, under the identity bound to it, reported applying a
    /// position past which it holds `call_ref`'s current call on `partition`,
    /// current or stale: the first counter the call's entry was sent at, or,
    /// with no entry toward it, the counter the sub-log opened at.
    pub fn applied_any(&self, peer: &str, partition: Partition, call_ref: &str) -> bool {
        let inner = locked(&self.inner, REPLICA_CHANGELOG);
        let Some(sub) = inner.peers.get(peer).map(|log| log.sub(partition)) else {
            return false;
        };
        let acked = *sub.acked.borrow();
        if acked.gen != self.gen {
            return false;
        }
        match sub.by_ref.get(call_ref) {
            Some(st) => st.first_sent.is_some_and(|sent| sent <= acked.counter),
            None => sub.opened <= acked.counter,
        }
    }

    /// A bootstrap scan of `peer`'s `partition` read at `w` carries every
    /// entry at or below it: each one never sent is marked sent at `w`.
    pub fn note_scanned(&self, peer: &str, partition: Partition, w: Watermark) {
        let mut inner = locked(&self.inner, REPLICA_CHANGELOG);
        let Some(sub) = inner.peers.get_mut(peer).map(|log| log.sub_mut(partition)) else {
            return;
        };
        let super::SubLog { entries, by_ref, .. } = sub;
        for call_ref in entries.range(..=w.counter).map(|(_, r)| r) {
            if let Some(st) = by_ref.get_mut(call_ref) {
                st.first_sent.get_or_insert(w.counter);
            }
        }
    }

    /// The position a cold drain of `peer`'s `partition` starts from: this
    /// incarnation's opening counter of the sub-log, below every entry it
    /// holds. `None` with no sub-log.
    pub fn opened_at(&self, peer: &str, partition: Partition) -> Option<Watermark> {
        let inner = locked(&self.inner, REPLICA_CHANGELOG);
        let sub = inner.peers.get(peer)?.sub(partition);
        Some(Watermark::new(self.gen, sub.opened))
    }
}
