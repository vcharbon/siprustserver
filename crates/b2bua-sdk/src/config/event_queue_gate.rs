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

/// The bound of the worker's transaction event queue, which the refusal
/// depth cannot exceed.
pub const EVENT_QUEUE_CAPACITY: usize = 4096;

/// One of the gate's bounds, as [`GateViolation`] names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateBound {
    RefuseDepth,
    AcceptDepth,
    RefuseDelay,
    RefuseInterval,
    AcceptDelay,
    AcceptHold,
}

impl GateBound {
    /// The field's name.
    pub const fn name(self) -> &'static str {
        match self {
            GateBound::RefuseDepth => "refuse_depth",
            GateBound::AcceptDepth => "accept_depth",
            GateBound::RefuseDelay => "refuse_delay_ms",
            GateBound::RefuseInterval => "refuse_interval_ms",
            GateBound::AcceptDelay => "accept_delay_ms",
            GateBound::AcceptHold => "accept_hold_ms",
        }
    }
}

/// Why a set of bounds cannot run the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateViolation {
    /// A bound is zero.
    Zero(GateBound),
    /// `low` is not below `high`.
    NotBelow { low: GateBound, high: GateBound, why: &'static str },
    /// The refusal depth exceeds [`EVENT_QUEUE_CAPACITY`].
    PastTheQueue,
}

impl EventQueueGateConfig {
    /// The relations the gate needs: every bound positive, each accept bound
    /// below its refusal bound, the refusal depth within the queue's bound,
    /// and the refusal interval below the hold, else a refusal would end
    /// before a delay that persists could trip it again.
    pub fn validate(&self) -> Result<(), GateViolation> {
        let as_u64 = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        let bounds = [
            (GateBound::RefuseDepth, as_u64(self.refuse_depth)),
            (GateBound::AcceptDepth, as_u64(self.accept_depth)),
            (GateBound::RefuseDelay, self.refuse_delay_ms),
            (GateBound::RefuseInterval, self.refuse_interval_ms),
            (GateBound::AcceptDelay, self.accept_delay_ms),
            (GateBound::AcceptHold, self.accept_hold_ms),
        ];
        if let Some((bound, _)) = bounds.iter().find(|(_, v)| *v == 0) {
            return Err(GateViolation::Zero(*bound));
        }
        let never_accepts = "the gate would never accept";
        let below = [
            (GateBound::AcceptDepth, GateBound::RefuseDepth, never_accepts),
            (GateBound::AcceptDelay, GateBound::RefuseDelay, never_accepts),
            (
                GateBound::RefuseInterval,
                GateBound::AcceptHold,
                "a refusal would end before a delay that persists could trip it again",
            ),
        ];
        let value = |b: GateBound| bounds.iter().find(|(k, _)| *k == b).map_or(0, |(_, v)| *v);
        if let Some((low, high, why)) = below.into_iter().find(|(l, h, _)| value(*l) >= value(*h)) {
            return Err(GateViolation::NotBelow { low, high, why });
        }
        if self.refuse_depth > EVENT_QUEUE_CAPACITY {
            return Err(GateViolation::PastTheQueue);
        }
        Ok(())
    }
}
