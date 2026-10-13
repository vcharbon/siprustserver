//! Flow progress (ADR-0031 D2): the position each sub-log's puller last
//! reported applying, and how long that report vouches for the flow.
//!
//! A puller answers every `Noop` with its position, so a connected peer
//! re-states its claim at least once per idle `Noop` interval. A claim older
//! than [`claim_ttl`](Changelog::set_claim_ttl) belongs to a peer that stopped
//! answering, one that died without a FIN included, whose serve task (and
//! claim) outlives it until the link bounds end it: it reads as unknown.

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
        crate::store::locked(&self.inner, "replica changelog").claim_ttl = ttl;
    }

    /// Record a puller's reported applied position for `(peer, partition)`
    /// ([`repl_net::frame::Frame::Position`]), stamped now. Monotonic — a
    /// lower report never regresses the recorded one, an equal one refreshes
    /// it — and ignored for a peer with no log.
    pub fn note_applied(&self, peer: &str, partition: Partition, at: Watermark) {
        let mut inner = crate::store::locked(&self.inner, "replica changelog");
        if let Some(log) = inner.peers.get_mut(peer) {
            let sub = log.sub_mut(partition);
            if sub.applied.is_none_or(|prev| at >= prev.at) {
                sub.applied = Some(Claim { at, made: Instant::now() });
            }
        }
    }

    /// Whether `peer`'s flow on `partition` is connected AND has reported,
    /// within the claim TTL, applying everything this sub-log ever logged. A
    /// report is required even for an empty sub-log: a reaped peer log
    /// restarts at head 0, so an unreported flow is unknown, never vacuously
    /// current.
    pub fn flow_caught_up(&self, peer: &str, partition: Partition) -> bool {
        let inner = crate::store::locked(&self.inner, "replica changelog");
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
