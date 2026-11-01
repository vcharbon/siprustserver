//! [`TransactionEvents`]: the consumer's end of the layer's output. Each event
//! it takes from the bounded queue is counted out of the queue's entry stamps
//! ([`EventAges`]), so the age of the oldest event still waiting is read at any
//! instant ([`TransactionMetrics::event_queue_delay`](crate::TransactionMetrics::event_queue_delay));
//! a layer that runs the keepalive lane
//! ([`TransactionConfig::keepalive_lane`](crate::TransactionConfig)) hands its
//! receiver over once ([`TransactionEvents::take_keepalive_lane`]).

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use super::event_age::EventAges;
use crate::event::TransactionEvent;

/// The receiving end of the layer's event queue, and of its keepalive lane
/// until taken.
pub struct TransactionEvents {
    events: mpsc::Receiver<TransactionEvent>,
    keepalive: Option<mpsc::Receiver<TransactionEvent>>,
    ages: Arc<EventAges>,
}

impl TransactionEvents {
    pub(crate) fn new(
        events: mpsc::Receiver<TransactionEvent>,
        keepalive: Option<mpsc::Receiver<TransactionEvent>>,
        ages: Arc<EventAges>,
    ) -> Self {
        Self { events, keepalive, ages }
    }

    /// The next event of the queue; `None` once the layer is gone and the
    /// queue is drained.
    pub async fn recv(&mut self) -> Option<TransactionEvent> {
        let event = self.events.recv().await?;
        self.ages.taken();
        Some(event)
    }

    /// The next event of the queue if one is there now.
    pub fn try_recv(&mut self) -> Result<TransactionEvent, TryRecvError> {
        let event = self.events.try_recv()?;
        self.ages.taken();
        Ok(event)
    }

    /// The keepalive lane's receiver, once: `None` when the layer runs no
    /// lane, or on a second call. A lane nobody takes fills and drops what
    /// reaches it.
    pub fn take_keepalive_lane(&mut self) -> Option<mpsc::Receiver<TransactionEvent>> {
        self.keepalive.take()
    }
}
