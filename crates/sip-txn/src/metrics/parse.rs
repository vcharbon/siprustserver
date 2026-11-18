//! The reads of the counters of the datagrams the parser refused.

use std::sync::atomic::Ordering;

use super::TransactionMetrics;

impl TransactionMetrics {
    /// Inbound packets the parser refused and the layer dropped unanswered
    /// (counter).
    pub fn parse_errors(&self) -> u64 {
        self.inner.parse_errors.load(Ordering::Relaxed)
    }

    /// Refused requests answered with a stateless 400 (counter).
    pub fn parse_refusals_answered(&self) -> u64 {
        self.inner.parse_refusals_answered.load(Ordering::Relaxed)
    }

    /// Answerable refused requests dropped by the cap on 400s (counter).
    pub fn parse_refusals_capped(&self) -> u64 {
        self.inner.parse_refusals_capped.load(Ordering::Relaxed)
    }

    /// Inbound packets the parser panicked on and dropped (counter).
    pub fn parse_panics(&self) -> u64 {
        self.inner.parse_panics.load(Ordering::Relaxed)
    }
}
