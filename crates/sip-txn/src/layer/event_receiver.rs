//! [`TransactionEvents`]: the consumer's end of the layer's output. Every
//! event of the bounded queue is stamped as it enters, so the consumer reads
//! how long the one it took waited (its sojourn); a layer that runs the
//! keepalive lane ([`TransactionConfig::keepalive_lane`](crate::TransactionConfig))
//! hands its receiver over once ([`TransactionEvents::take_keepalive_lane`]).

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::time::Instant;

use crate::event::TransactionEvent;
use crate::metrics::MetricsInner;

/// An event in the bounded queue, with the instant it entered.
pub(crate) struct Queued {
    pub(crate) event: TransactionEvent,
    pub(crate) at: Instant,
}

impl Queued {
    /// `event`, entering the queue now.
    pub(crate) fn now(event: TransactionEvent) -> Self {
        Self { event, at: Instant::now() }
    }
}

/// The receiving end of the layer's event queue, and of its keepalive lane
/// until taken.
pub struct TransactionEvents {
    events: mpsc::Receiver<Queued>,
    keepalive: Option<mpsc::Receiver<TransactionEvent>>,
    metrics: Arc<MetricsInner>,
    last_sojourn: Duration,
}

impl TransactionEvents {
    pub(crate) fn new(
        events: mpsc::Receiver<Queued>,
        keepalive: Option<mpsc::Receiver<TransactionEvent>>,
        metrics: Arc<MetricsInner>,
    ) -> Self {
        Self { events, keepalive, metrics, last_sojourn: Duration::ZERO }
    }

    /// The next event of the queue; `None` once the layer is gone and the
    /// queue is drained. Its wait in the queue is then
    /// [`last_sojourn`](Self::last_sojourn).
    pub async fn recv(&mut self) -> Option<TransactionEvent> {
        let queued = self.events.recv().await?;
        Some(self.taken(queued))
    }

    /// The next event of the queue if one is there now.
    pub fn try_recv(&mut self) -> Result<TransactionEvent, TryRecvError> {
        let queued = self.events.try_recv()?;
        Ok(self.taken(queued))
    }

    /// How long the event taken last waited in the queue; zero before any.
    pub fn last_sojourn(&self) -> Duration {
        self.last_sojourn
    }

    /// The keepalive lane's receiver, once: `None` when the layer runs no
    /// lane, or on a second call. A lane nobody takes fills and drops what
    /// reaches it.
    pub fn take_keepalive_lane(&mut self) -> Option<mpsc::Receiver<TransactionEvent>> {
        self.keepalive.take()
    }

    fn taken(&mut self, queued: Queued) -> TransactionEvent {
        self.last_sojourn = queued.at.elapsed();
        let micros = u64::try_from(self.last_sojourn.as_micros()).unwrap_or(u64::MAX);
        self.metrics.event_sojourn_us.store(micros, Relaxed);
        queued.event
    }
}
