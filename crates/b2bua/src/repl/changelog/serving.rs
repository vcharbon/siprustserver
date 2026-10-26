//! A serve task's hold on the sub-log it streams: the peer's log stays
//! reap-immune and the flow counts as connected while the task runs (ADR-0031
//! D2).

use repl_net::frame::Partition;
use tokio::sync::watch;

use super::{Changelog, PeerLog};
use crate::store::{locked, REPLICA_CHANGELOG};

impl Changelog {
    /// Register a serve task for `(peer, partition)`: keeps the peer's log
    /// reap-immune and marks the partition's flow connected, until the returned
    /// `ServeGuard` drops. Creates the peer log if absent.
    pub fn serving(&self, peer: &str, partition: Partition) -> ServeGuard {
        let written = {
            let mut inner = locked(&self.inner, REPLICA_CHANGELOG);
            let (now, counter) = (self.clock.now_ms(), inner.counter);
            let log =
                inner.peers.entry(peer.to_string()).or_insert_with(|| PeerLog::new(now, counter));
            log.serving += 1;
            let sub = log.sub_mut(partition);
            sub.streams += 1;
            // A new flow starts unknown: a claim left by a flow the peer never
            // closed (a dead node sends no FIN) must not vouch for this one.
            sub.applied = None;
            sub.written.subscribe()
        };
        ServeGuard { changelog: self.clone(), peer: peer.to_string(), partition, written }
    }
}

/// RAII handle keeping a peer log reap-immune while a serve task drains ONE of
/// its sub-logs. Drop decrements both the peer's reap-immunity count and the
/// sub-log's connected-flow count, and clears the flow's reported position — a
/// disconnected flow holds nothing current (ADR-0031 D2).
pub struct ServeGuard {
    changelog: Changelog,
    peer: String,
    partition: Partition,
    written: watch::Receiver<u64>,
}

impl ServeGuard {
    /// Resolves once a write lands in the sub-log after the last call: the
    /// serve task drains on it. A write between two calls is never missed.
    pub async fn written(&mut self) {
        if self.written.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for ServeGuard {
    fn drop(&mut self) {
        let mut inner = locked(&self.changelog.inner, REPLICA_CHANGELOG);
        if let Some(log) = inner.peers.get_mut(&self.peer) {
            log.serving = log.serving.saturating_sub(1);
            let sub = log.sub_mut(self.partition);
            sub.streams = sub.streams.saturating_sub(1);
            if sub.streams == 0 {
                sub.applied = None;
            }
        }
    }
}
