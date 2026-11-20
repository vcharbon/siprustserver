//! The reads of the per-Call-ID message budget's counters (ADR-0063 §6).

use std::sync::atomic::Ordering;

use super::TransactionMetrics;

impl TransactionMetrics {
    /// Call-IDs that passed their message budget and were muted (counter).
    pub fn message_budget_trips(&self) -> u64 {
        self.inner.message_budget_trips.load(Ordering::Relaxed)
    }

    /// Datagrams of a new Call-ID left uncounted because the full map holds
    /// no idle Call-ID, every one muted or with a live transaction (counter).
    pub fn message_budget_uncounted(&self) -> u64 {
        self.inner.message_budget_uncounted.load(Ordering::Relaxed)
    }

    /// Datagrams a mute dropped on arrival (counter).
    pub fn muted_in(&self) -> u64 {
        self.inner.muted_in.load(Ordering::Relaxed)
    }

    /// Datagrams a mute kept from the wire (counter).
    pub fn muted_out(&self) -> u64 {
        self.inner.muted_out.load(Ordering::Relaxed)
    }
}
