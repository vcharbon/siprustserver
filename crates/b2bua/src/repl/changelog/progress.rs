//! Flow progress (ADR-0031 D2): the position each sub-log's puller last
//! reported applying, and how long that report vouches for the flow.
//!
//! A puller answers every `Noop` with its position, so a connected peer
//! re-states its claim at least once per idle `Noop` interval. A claim older
//! than [`claim_ttl`](Changelog::set_claim_ttl) belongs to a peer that stopped
//! answering, one that died without a FIN included, whose serve task (and
//! claim) outlives it until the link bounds end it: it reads as unknown.
//!
//! The highest position a flow ever reported under this node's incarnation is
//! kept apart from the claim, past a disconnect: it is what the peer holds for
//! sure. It releases the messages a turn holds for the peer (ADR-0055 §3) and
//! reaps the deletes it covers.

use std::time::Duration;

use repl_net::frame::{Partition, Watermark};
use tokio::time::Instant;

use super::Changelog;

/// How long a claim vouches for its flow unless the server says otherwise:
/// twice the server's default idle `Noop` interval.
pub const DEFAULT_CLAIM_TTL: Duration = Duration::from_secs(10);

/// A puller's report: the position it applied, and when it said so.
#[derive(Clone, Copy, Debug)]
pub(super) struct Claim {
    at: Watermark,
    made: Instant,
}

impl Changelog {
    /// Claims older than `ttl` read as unknown: the server sets it to twice
    /// its idle `Noop` interval.
    pub fn set_claim_ttl(&self, ttl: Duration) {
        crate::store::locked(&self.inner, crate::store::REPLICA_CHANGELOG).claim_ttl = ttl;
    }

    /// Record a puller's reported applied position for `(peer, partition)`
    /// ([`repl_net::frame::Frame::Position`]), stamped now. The latest report
    /// stands, lower or not: a peer that shed what it held claims less, and
    /// its earlier claim no longer vouches for it. Ignored for a peer with no
    /// log, and from a worker `identity` other than the one membership binds to
    /// the ordinal ([`bind_identity`](Self::bind_identity)): a superseded
    /// worker on the ordinal vouches for nothing its successor holds (ADR-0055
    /// §6).
    pub fn note_applied(
        &self,
        peer: &str,
        identity: Option<&str>,
        partition: Partition,
        at: Watermark,
    ) {
        let gen = self.gen;
        let mut inner = crate::store::locked(&self.inner, crate::store::REPLICA_CHANGELOG);
        if inner.bound.get(peer).is_some_and(|bound| Some(bound.as_str()) != identity) {
            return;
        }
        if let Some(log) = inner.peers.get_mut(peer) {
            let sub = log.sub_mut(partition);
            sub.applied = Some(Claim { at, made: Instant::now() });
            if at.gen == gen {
                sub.acked.send_if_modified(|acked| {
                    let raised = at > *acked;
                    if raised {
                        *acked = at;
                    }
                    raised
                });
            }
        }
    }

    /// Bind `peer`'s ordinal to the worker `identity` membership names for it
    /// (ADR-0055 §6): only that identity's reports count from now on. `None`:
    /// membership names no identity, and every report counts.
    pub fn bind_identity(&self, peer: &str, identity: Option<&str>) {
        let mut inner = crate::store::locked(&self.inner, crate::store::REPLICA_CHANGELOG);
        match identity {
            Some(identity) => inner.bound.insert(peer.to_string(), identity.to_string()),
            None => inner.bound.remove(peer),
        };
    }

    /// Whether `peer` is a live, bootstrapped holder of `partition`: its flow is
    /// connected and has claimed a position within the claim TTL, which it does
    /// only once its bootstrap is applied.
    pub fn flow_live(&self, peer: &str, partition: Partition) -> bool {
        let inner = crate::store::locked(&self.inner, crate::store::REPLICA_CHANGELOG);
        inner.peers.get(peer).map(|log| log.sub(partition)).is_some_and(|sub| {
            sub.streams > 0 && sub.applied.is_some_and(|c| c.made.elapsed() <= inner.claim_ttl)
        })
    }

    /// Resolves once `peer` reported applying `at` or past it on `partition`,
    /// or once its log is gone: nothing then will ever report it.
    pub async fn applied_reaches(&self, peer: &str, partition: Partition, at: Watermark) {
        let acked = {
            let inner = crate::store::locked(&self.inner, crate::store::REPLICA_CHANGELOG);
            inner.peers.get(peer).map(|log| log.sub(partition).acked.subscribe())
        };
        if let Some(mut acked) = acked {
            let _ = acked.wait_for(|applied| *applied >= at).await;
        }
    }

    /// Whether `peer`'s flow on `partition` is connected AND has reported,
    /// within the claim TTL, applying everything this sub-log ever logged. A
    /// report is required even for an empty sub-log: a reaped peer log
    /// restarts at head 0, so an unreported flow is unknown, never vacuously
    /// current.
    pub fn flow_caught_up(&self, peer: &str, partition: Partition) -> bool {
        let inner = crate::store::locked(&self.inner, crate::store::REPLICA_CHANGELOG);
        let Some(log) = inner.peers.get(peer) else {
            return false;
        };
        let sub = log.sub(partition);
        if sub.streams == 0 {
            return false;
        }
        let head = Watermark::new(self.gen, sub.head);
        sub.applied.is_some_and(|c| c.at >= head && c.made.elapsed() <= inner.claim_ttl)
    }
}
