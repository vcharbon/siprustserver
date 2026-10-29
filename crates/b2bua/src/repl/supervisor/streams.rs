//! The streams this node pulls from a peer, as the router reads them: a
//! proxy's dead verdict on the peer is confirmed by its stream (ADR-0047 D1).

use std::time::Duration;

use super::ReplicationSupervisor;

impl ReplicationSupervisor {
    /// Whether the **Reclaim** puller is currently running (not Parked) for `peer`.
    pub fn is_running(&self, peer: &str) -> bool {
        self.inner.peers().get(peer).map(|e| e.reclaim.status_rx.is_some()).unwrap_or(false)
    }

    /// Whether this node streams the calls it backs for `peer` now: the Backup
    /// flow's puller holds an open stream to it. Pure read.
    pub fn streams_from(&self, peer: &str) -> bool {
        self.inner
            .peers()
            .get(peer)
            .and_then(|e| e.backup.status_rx.as_ref())
            .is_some_and(|rx| rx.borrow().streaming)
    }

    /// Resolves once this node no longer streams from `peer`
    /// ([`streams_from`](Self::streams_from)): the stream ended, or the
    /// Backup puller was parked or replaced.
    pub async fn stream_from_ends(&self, peer: &str) {
        let rx = self.inner.peers().get(peer).and_then(|e| e.backup.status_rx.clone());
        if let Some(mut rx) = rx {
            let _ = rx.wait_for(|s| !s.streaming).await;
        }
    }

    /// The longest an open stream from a peer that went silent stays open:
    /// the pullers' idle bound (ADR-0045).
    pub fn silence_bound(&self) -> Duration {
        Duration::from_millis(self.inner.config.idle_timeout_ms)
    }
}
