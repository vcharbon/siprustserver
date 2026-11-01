//! How a stopping worker's supervisor stops: no puller runs, and none is
//! spawned once the stop began.

use std::sync::atomic::Ordering;

use super::{wiring, ReplicationSupervisor};

impl ReplicationSupervisor {
    /// Shut every running puller down (Park them all) + abort the reconcile loop
    /// and the backup gate. The retained watermarks/current flags survive. Used by
    /// [`B2buaCore::abort`](crate::B2buaCore::abort) to stop a crashed worker's
    /// pullers without changing replication behaviour.
    pub fn shutdown(&self) {
        // Stop reacting to membership deltas + the backup gate FIRST so neither can
        // spawn a new puller against this (about-to-be-dead) node after we park the
        // existing ones. Aborting also frees each task's `Arc<SupervisorInner>`;
        // an abort lands at the task's next poll, so a reconcile already woken
        // reads `stopped` instead.
        self.inner.stopped.store(true, Ordering::SeqCst);
        if let Some(h) = wiring(&self.inner.reconcile).take() {
            h.abort();
        }
        if let Some(h) = wiring(&self.inner.gate).take() {
            h.abort();
        }
        let mut peers = self.inner.peers();
        for entry in peers.values_mut() {
            entry.park();
        }
    }
}
