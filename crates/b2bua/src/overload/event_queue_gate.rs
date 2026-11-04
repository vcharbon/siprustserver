//! [`EventQueueGate`]: whether a worker whose transaction event queue is
//! backed up refuses new calls (ADR-0050 §9, `cause=overload`; ADR-0037
//! item 11, the event-queue rung). It reads the queue's depth and its delay,
//! the age of the oldest event waiting in it, at the instant it reads. It
//! refuses at the refusal depth at once, and on the delay once it has stayed
//! at or past the refusal delay for the refusal interval (CoDel's
//! persistence): a slow turn or a throttle gap shorter than the two together
//! trips nothing, and one at least as long trips it on the next reading. It
//! accepts
//! again only once depth and delay have both stayed at or below their accept
//! bounds for the hold, so it does not flap at the probe's cadence.
//!
//! The router reads it at each event it takes from the queue, the OPTIONS
//! answer and the admission ladder each time they ask it, and an idle router
//! every 100 ms while it refuses. Every reading runs on the router task, the
//! gate's one writer: a reading is a clock read and a few atomic loads and
//! stores, no lock. The transaction layer reads only the `refusing` state, as
//! each new INVITE arrives: the state the last reading wrote, so while the
//! router is held in one turn it is the state from before that turn, and an
//! INVITE it admits is judged again at its take. Time is `tokio::time`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::config::EventQueueGateConfig;

/// The gate's state: refusing or not, since when the delay has stayed at or
/// past the refusal delay, and the last instant the queue read above an
/// accept bound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GateState {
    refusing: bool,
    delayed_since: Option<Instant>,
    last_hot: Option<Instant>,
}

/// A change of the gate's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transition {
    Refuse,
    Accept,
}

impl GateState {
    /// Fold one reading of the queue at `now`, `depth` events waiting the
    /// oldest of which has waited `delay`, into the state. The oldest event
    /// alone has waited past the refusal delay since `now − (delay − bound)`,
    /// so a reading also tells how long the delay has stayed past it.
    pub(crate) fn observe(
        &mut self,
        bounds: &EventQueueGateConfig,
        depth: usize,
        delay: Duration,
        now: Instant,
    ) -> Option<Transition> {
        let refuse_delay = Duration::from_millis(bounds.refuse_delay_ms);
        self.delayed_since = (delay >= refuse_delay).then(|| {
            let since = now.checked_sub(delay - refuse_delay).unwrap_or(now);
            self.delayed_since.map_or(since, |earlier| earlier.min(since))
        });
        let persisted = self.delayed_since.is_some_and(|since| {
            now.saturating_duration_since(since) >= Duration::from_millis(bounds.refuse_interval_ms)
        });
        if depth >= bounds.refuse_depth || persisted {
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
        if depth > bounds.accept_depth || delay > Duration::from_millis(bounds.accept_delay_ms) {
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

/// [`GateState`] in atomics, written by the router task alone: each instant
/// is stored as nanoseconds past `base` plus one, zero for none. `refusing`
/// is shared with the transaction layer, which reads it as each new INVITE
/// arrives.
struct Stored {
    base: Instant,
    refusing: Arc<AtomicBool>,
    delayed_since: AtomicU64,
    last_hot: AtomicU64,
}

impl Stored {
    fn load(&self) -> GateState {
        GateState {
            refusing: self.refusing.load(Relaxed),
            delayed_since: self.instant(self.delayed_since.load(Relaxed)),
            last_hot: self.instant(self.last_hot.load(Relaxed)),
        }
    }

    fn store(&self, state: &GateState) {
        self.refusing.store(state.refusing, Relaxed);
        self.delayed_since.store(self.encoded(state.delayed_since), Relaxed);
        self.last_hot.store(self.encoded(state.last_hot), Relaxed);
    }

    fn instant(&self, encoded: u64) -> Option<Instant> {
        encoded.checked_sub(1).map(|ns| self.base + Duration::from_nanos(ns))
    }

    fn encoded(&self, at: Option<Instant>) -> u64 {
        at.map_or(0, |at| {
            let ns = at.saturating_duration_since(self.base).as_nanos();
            u64::try_from(ns).unwrap_or(u64::MAX - 1) + 1
        })
    }
}

struct Inner {
    bounds: EventQueueGateConfig,
    queue: sip_txn::TransactionMetrics,
    state: Stored,
    refusals: AtomicU64,
    node: observe::NodeScope,
}

/// Clone-cheap handle on a worker's event-queue gate.
#[derive(Clone)]
pub(crate) struct EventQueueGate {
    inner: Arc<Inner>,
}

impl EventQueueGate {
    /// The gate over the event queue `queue` reads, under `bounds`, belonging
    /// to the node the caller serves; `refusing` is the switch the
    /// transaction layer reads on each new INVITE's arrival.
    pub(crate) fn new(
        bounds: EventQueueGateConfig,
        queue: sip_txn::TransactionMetrics,
        refusing: Arc<AtomicBool>,
    ) -> Self {
        let state = Stored {
            base: Instant::now(),
            refusing,
            delayed_since: AtomicU64::new(0),
            last_hot: AtomicU64::new(0),
        };
        let inner = Inner {
            bounds,
            queue,
            state,
            refusals: AtomicU64::new(0),
            node: observe::NodeScope::current(),
        };
        Self { inner: Arc::new(inner) }
    }

    /// Read the queue now, folding the reading into the gate's state, and
    /// whether the worker refuses new calls. The router task is the gate's one
    /// writer and its only caller: the loop at each event it takes, the
    /// OPTIONS answer and the admission ladder.
    pub(crate) fn read(&self) -> bool {
        let queue = &self.inner.queue;
        let (depth, delay) = (queue.event_queue_depth(), queue.event_queue_delay());
        let mut state = self.inner.state.load();
        let transition = state.observe(&self.inner.bounds, depth, delay, Instant::now());
        self.inner.state.store(&state);
        if let Some(transition) = transition {
            self.line(transition, depth, delay);
        }
        state.refusing()
    }

    fn line(&self, transition: Transition, depth: usize, delay: Duration) {
        let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        match transition {
            Transition::Refuse => {
                self.inner.refusals.fetch_add(1, Relaxed);
                tracing::info!(
                    node = %self.inner.node,
                    depth,
                    delay_ms,
                    "event queue backed up: new calls refused"
                );
            }
            Transition::Accept => tracing::info!(
                node = %self.inner.node,
                depth,
                delay_ms,
                hold_ms = self.inner.bounds.accept_hold_ms,
                "event queue drained through the hold: new calls accepted"
            ),
        }
    }

    /// Whether the gate refuses now, without a reading: the state the last
    /// reading wrote.
    pub(crate) fn refusing_now(&self) -> bool {
        self.inner.state.refusing.load(Relaxed)
    }

    /// The times the gate turned to refusing (counter).
    fn refusals(&self) -> u64 {
        self.inner.refusals.load(Relaxed)
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
