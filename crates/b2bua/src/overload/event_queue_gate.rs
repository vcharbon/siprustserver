//! [`EventQueueGate`]: whether a worker whose transaction event queue is
//! backed up refuses new calls (ADR-0050 §9, `cause=overload`). It reads the
//! queue's depth and the wait of the event the router took last (its
//! sojourn): at or past either refusal bound it refuses; it accepts again
//! only once depth and sojourn have both stayed at or below their accept
//! bounds for the hold, so it does not flap at the probe's cadence.
//!
//! The router observes each event it takes from the queue; the OPTIONS
//! answer observes the queue as it stands (an empty queue reads a zero
//! sojourn) and reads the verdict. Time is `tokio::time`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use crate::config::EventQueueGateConfig;
use crate::poison::{locked, EVENT_QUEUE_GATE};

/// The gate's state: refusing or not, and the last instant the queue read
/// above an accept bound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GateState {
    refusing: bool,
    last_hot: Option<Instant>,
}

/// A change of the gate's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transition {
    Refuse,
    Accept,
}

impl GateState {
    /// Fold one reading of the queue at `now` into the state.
    pub(crate) fn observe(
        &mut self,
        bounds: &EventQueueGateConfig,
        depth: usize,
        sojourn: Duration,
        now: Instant,
    ) -> Option<Transition> {
        let sojourn_ms = u64::try_from(sojourn.as_millis()).unwrap_or(u64::MAX);
        if depth >= bounds.refuse_depth || sojourn_ms >= bounds.refuse_sojourn_ms {
            self.last_hot = Some(now);
            if !self.refusing {
                self.refusing = true;
                return Some(Transition::Refuse);
            }
            return None;
        }
        if !self.refusing {
            return None;
        }
        if depth > bounds.accept_depth || sojourn_ms > bounds.accept_sojourn_ms {
            self.last_hot = Some(now);
            return None;
        }
        let calm_for = self.last_hot.map_or(Duration::MAX, |at| now.saturating_duration_since(at));
        if calm_for >= Duration::from_millis(bounds.accept_hold_ms) {
            self.refusing = false;
            return Some(Transition::Accept);
        }
        None
    }

    pub(crate) fn refusing(&self) -> bool {
        self.refusing
    }
}

struct Inner {
    bounds: EventQueueGateConfig,
    queue: sip_txn::TransactionMetrics,
    state: Mutex<GateState>,
    refusals: AtomicU64,
    node: observe::NodeScope,
}

/// Clone-cheap handle on a worker's event-queue gate.
#[derive(Clone)]
pub struct EventQueueGate {
    inner: Arc<Inner>,
}

impl EventQueueGate {
    /// The gate over the event queue `queue` reads, under `bounds`, belonging
    /// to the node the caller serves.
    pub fn new(bounds: EventQueueGateConfig, queue: sip_txn::TransactionMetrics) -> Self {
        Self {
            inner: Arc::new(Inner {
                bounds,
                queue,
                state: Mutex::new(GateState::default()),
                refusals: AtomicU64::new(0),
                node: observe::NodeScope::current(),
            }),
        }
    }

    /// The router took an event that waited `sojourn` in the queue.
    pub(crate) fn observe_take(&self, sojourn: Duration) {
        self.observe(self.inner.queue.event_queue_depth(), sojourn);
    }

    /// Whether the worker refuses new calls, reading the queue as it stands.
    pub fn refusing(&self) -> bool {
        let queue = &self.inner.queue;
        self.observe(queue.event_queue_depth(), queue.event_queue_sojourn())
    }

    /// Fold one reading into the state; whether it now refuses.
    fn observe(&self, depth: usize, sojourn: Duration) -> bool {
        let mut state = locked(&self.inner.state, EVENT_QUEUE_GATE);
        let transition = state.observe(&self.inner.bounds, depth, sojourn, Instant::now());
        let refusing = state.refusing();
        drop(state);
        if let Some(transition) = transition {
            self.line(transition, depth, sojourn);
        }
        refusing
    }

    fn line(&self, transition: Transition, depth: usize, sojourn: Duration) {
        let sojourn_ms = u64::try_from(sojourn.as_millis()).unwrap_or(u64::MAX);
        match transition {
            Transition::Refuse => {
                self.inner.refusals.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    node = %self.inner.node,
                    depth,
                    sojourn_ms,
                    "event queue backed up: new calls refused"
                );
            }
            Transition::Accept => tracing::info!(
                node = %self.inner.node,
                depth,
                sojourn_ms,
                hold_ms = self.inner.bounds.accept_hold_ms,
                "event queue drained through the hold: new calls accepted"
            ),
        }
    }

    /// Whether the gate refuses now, without a reading (the gauge).
    fn refusing_now(&self) -> bool {
        locked(&self.inner.state, EVENT_QUEUE_GATE).refusing()
    }

    /// The times the gate turned to refusing (counter).
    fn refusals(&self) -> u64 {
        self.inner.refusals.load(Ordering::Relaxed)
    }

    /// The gate's gauge and counter as Prometheus text; `None` renders them
    /// for a worker with no gate, at zero.
    pub(crate) fn prometheus_text(gate: Option<&Self>, out: &mut String) {
        use crate::metrics::catalogue::readiness::{EVENT_QUEUE_REFUSALS, EVENT_QUEUE_REFUSING};
        EVENT_QUEUE_REFUSING.render_value(out, u64::from(gate.is_some_and(Self::refusing_now)));
        EVENT_QUEUE_REFUSALS.render_value(out, gate.map_or(0, Self::refusals));
    }
}

#[cfg(test)]
mod tests;
