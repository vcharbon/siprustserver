//! The end of each record a CDR sink was handed, as an observer is told it.

use std::time::Duration;

/// How one record handed to a sink ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CdrDelivery {
    /// The sink's destination confirmed it.
    Written,
    /// Nothing confirmed it: the encoder refused it, the sink never sent it,
    /// or no confirm came within the sink's bounds.
    Dropped,
}

/// Told the end of every record a sink was handed, once per record.
pub trait CdrDeliveryObserver: Send + Sync {
    /// One record ended as `delivery`, `took` after the sink was handed it.
    /// Runs on the sink's own tasks, so it returns at once.
    fn ended(&self, delivery: CdrDelivery, took: Duration);
}
