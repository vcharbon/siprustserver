//! The bounds of the event-queue gate: when a worker whose transaction event
//! queue is backed up refuses new calls, on its OPTIONS answer and at its
//! admission ladder, and when it accepts them again (ADR-0050 §9).

/// Refuse at the refusal depth at once, or once the queue's delay (the age of
/// its oldest waiting event) has stayed at or past the refusal delay for the
/// refusal interval; accept again once depth and delay have both stayed at or
/// below their accept bounds for `accept_hold_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventQueueGateConfig {
    /// Queued events at which the worker refuses new calls.
    pub refuse_depth: usize,
    /// Queued events at or below which the worker may accept again.
    pub accept_depth: usize,
    /// The queue's delay, ms, at or past which the worker refuses new calls
    /// once it has persisted for `refuse_interval_ms`.
    pub refuse_delay_ms: u64,
    /// How long, ms, the delay stays at or past `refuse_delay_ms` before the
    /// worker refuses.
    pub refuse_interval_ms: u64,
    /// The queue's delay, ms, at or below which the worker may accept again.
    pub accept_delay_ms: u64,
    /// How long, ms, both accept bounds hold before the worker accepts again.
    pub accept_hold_ms: u64,
}

impl Default for EventQueueGateConfig {
    fn default() -> Self {
        Self {
            refuse_depth: 512,
            accept_depth: 64,
            refuse_delay_ms: 300,
            refuse_interval_ms: 150,
            accept_delay_ms: 50,
            accept_hold_ms: 3_000,
        }
    }
}
