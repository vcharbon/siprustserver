//! The read side of [`ProxyMetrics`] that tests and the runner's checks
//! use: one counter or gauge at a time, without rendering the exposition.

use std::sync::atomic::Ordering;

use super::slots::read;
use super::{CancelLookup, ProxyMetrics};
use crate::resolver::{outcome, refresh_outcome};

impl ProxyMetrics {
    pub fn messages_total(&self) -> u64 {
        self.messages.iter().flatten().map(|c| c.load(Ordering::Relaxed)).sum()
    }
    pub fn routing_decisions_total(&self) -> u64 {
        self.routing_decisions.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }
    pub fn routing_duration_count(&self) -> u64 {
        self.routing_duration_count.load(Ordering::Relaxed)
    }
    pub fn record_route_inserted_total(&self) -> u64 {
        self.record_route_inserted.load(Ordering::Relaxed)
    }
    pub fn pending_invite_lru_size(&self) -> u64 {
        self.pending_invite_lru_size.load(Ordering::Relaxed)
    }
    pub fn branch_memo_evictions(&self) -> u64 {
        self.branch_memo_evictions.load(Ordering::Relaxed)
    }
    pub fn calls_total(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
    pub fn named_send_count(&self, outcome: &str) -> u64 {
        outcome::ALL.iter().position(|o| *o == outcome).map_or(0, |i| read(&self.named_sends, i))
    }
    pub fn resolver_refresh_count(&self, outcome: &str) -> u64 {
        let at = refresh_outcome::ALL.iter().position(|o| *o == outcome);
        at.map_or(0, |i| read(&self.resolver_refresh, i))
    }
    pub fn resolver_cache_size(&self) -> u64 {
        self.resolver_cache_size.load(Ordering::Relaxed)
    }
    pub fn reject_count(&self, reason: &str) -> u64 {
        self.rejects.get(&[reason])
    }
    pub fn cancel_lookup_count(&self, outcome: CancelLookup) -> u64 {
        self.cancel_lookups.get(&[outcome.as_str()])
    }
}
