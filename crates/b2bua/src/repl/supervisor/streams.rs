//! The streams this node pulls from a peer, as the router reads them: a
//! proxy's dead verdict on the peer is confirmed by its stream (ADR-0047 D1).

use std::time::Duration;

use tokio::time::Instant;

use super::{PullerStatus, ReplicationSupervisor};

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

    /// Resolves once the stream from `peer` ended with no frame from it after
    /// `since` (`true`: the peer reads dead), or a frame from it arrived after
    /// `since` (`false`: it lives). A Backup puller parked or replaced reads
    /// as the stream's end.
    pub async fn stream_from_ends_unheard_since(&self, peer: &str, since: Instant) -> bool {
        let rx = self.inner.peers().get(peer).and_then(|e| e.backup.status_rx.clone());
        let Some(mut rx) = rx else { return true };
        let heard = |s: &PullerStatus| s.last_frame_at.is_some_and(|at| at > since);
        let unheard = match rx.wait_for(|s| !s.streaming || heard(s)).await {
            Ok(status) => !heard(&status),
            Err(_) => true,
        };
        unheard
    }

    /// The longest an open stream from a peer that went silent stays open:
    /// the pullers' idle bound (ADR-0045).
    pub fn silence_bound(&self) -> Duration {
        Duration::from_millis(self.inner.config.idle_timeout_ms)
    }
}
