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
pub const DEFAULT_CLAIM_TTL: Duration = Duration::from_secs(1);

/// How a wait for a peer's applied position ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// The peer reported applying the position.
    Applied,
    /// The peer's flow went down, or its log is gone, first.
    Lost,
}

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
    /// §6). The highest such report under this incarnation is kept aside: it
    /// counts once membership binds that identity.
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
            if let (Some(log), Some(identity)) = (inner.peers.get_mut(peer), identity) {
                let sub = log.sub_mut(partition);
                let higher = sub.unbound.as_ref().is_none_or(|(id, w)| id != identity || at > *w);
                if at.gen == gen && higher {
                    sub.unbound = Some((identity.to_string(), at));
                }
            }
            return;
        }
        let now_ms = self.clock.now_ms();
        if let Some(log) = inner.peers.get_mut(peer) {
            log.last_active_ms = now_ms;
            let sub = log.sub_mut(partition);
            sub.applied = Some(Claim { at, made: Instant::now() });
            sub.flow_started = None;
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
    /// membership names no identity, and every report counts. A new identity
    /// holds nothing an earlier one applied: the highest position applied
    /// falls back to what that identity reported before it was bound, or none,
    /// on both sub-logs.
    pub fn bind_identity(&self, peer: &str, identity: Option<&str>) {
        let mut inner = crate::store::locked(&self.inner, crate::store::REPLICA_CHANGELOG);
        let Some(identity) = identity else {
            inner.bound.remove(peer);
            return;
        };
        if inner.bound.get(peer).is_some_and(|bound| bound == identity) {
            return;
        }
        inner.bound.insert(peer.to_string(), identity.to_string());
        if let Some(log) = inner.peers.get_mut(peer) {
            for partition in [Partition::Pri, Partition::Bak] {
                let sub = log.sub_mut(partition);
                let reported = sub.unbound.take().filter(|(id, _)| id == identity);
                sub.acked.send_replace(reported.map_or(Watermark::new(0, 0), |(_, at)| at));
            }
        }
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

    /// Whether a flow of `peer` is open on `partition`, bootstrapped or not.
    pub fn flow_open(&self, peer: &str, partition: Partition) -> bool {
        let inner = crate::store::locked(&self.inner, crate::store::REPLICA_CHANGELOG);
        inner.peers.get(peer).is_some_and(|log| log.sub(partition).streams > 0)
    }

    /// Whether `peer` reported applying `at` or past it on `partition`.
    pub fn has_applied(&self, peer: &str, partition: Partition, at: Watermark) -> bool {
        let inner = crate::store::locked(&self.inner, crate::store::REPLICA_CHANGELOG);
        inner.peers.get(peer).is_some_and(|log| *log.sub(partition).acked.borrow() >= at)
    }

    /// Resolves once `peer` reported applying `at` or past it on `partition`
    /// ([`Reach::Applied`]), or once its flow is disconnected or its log is
    /// gone, when nothing will report it ([`Reach::Lost`]).
    pub async fn applied_reaches(&self, peer: &str, partition: Partition, at: Watermark) -> Reach {
        let acked = {
            let inner = crate::store::locked(&self.inner, crate::store::REPLICA_CHANGELOG);
            inner.peers.get(peer).map(|log| log.sub(partition).acked.subscribe())
        };
        let Some(mut acked) = acked else { return Reach::Lost };
        loop {
            if *acked.borrow_and_update() >= at {
                return Reach::Applied;
            }
            if !self.flow_connected(peer, partition) || acked.changed().await.is_err() {
                return Reach::Lost;
            }
        }
    }

    /// Whether a serve task streams `peer`'s flow on `partition`.
    fn flow_connected(&self, peer: &str, partition: Partition) -> bool {
        let inner = crate::store::locked(&self.inner, crate::store::REPLICA_CHANGELOG);
        inner.peers.get(peer).is_some_and(|log| log.sub(partition).streams > 0)
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
