//! The bounds of the event-queue gate: when a worker whose transaction event
//! queue is backed up refuses new calls on its OPTIONS answer, and when it
//! accepts them again (ADR-0050 §2, §9).

/// Refuse at or past either refusal bound; accept again once both accept
/// bounds have held for `accept_hold_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventQueueGateConfig {
    /// Queued events at which the worker refuses new calls.
    pub refuse_depth: usize,
    /// Queued events at or below which the worker may accept again.
    pub accept_depth: usize,
    /// A wait in the queue, ms, at which the worker refuses new calls.
    pub refuse_sojourn_ms: u64,
    /// A wait in the queue, ms, at or below which the worker may accept again.
    pub accept_sojourn_ms: u64,
    /// How long, ms, both accept bounds hold before the worker accepts again.
    pub accept_hold_ms: u64,
}

impl Default for EventQueueGateConfig {
    fn default() -> Self {
        Self {
            refuse_depth: 512,
            accept_depth: 64,
            refuse_sojourn_ms: 500,
            accept_sojourn_ms: 50,
            accept_hold_ms: 3_000,
        }
    }
}
