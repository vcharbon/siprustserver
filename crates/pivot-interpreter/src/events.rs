//! Lane events (`PCAP2TEST_PIVOT_V3.md` §6.7g): what the lane observes off the
//! SIP wire, reported to the run so a `send` can wait on it.
//!
//! The tokens are the lane's: the run knows only which ones the lane reports
//! (a document awaiting another refuses before it dials) and the occurrences
//! reported and not yet consumed. An awaiting send goes out once its dwell has
//! elapsed and such an occurrence exists, and consumes it: one occurrence
//! releases one send, whenever it was reported.

use std::collections::{BTreeMap, BTreeSet};

use tokio::sync::mpsc;

/// The receiving half a [`Lane`](crate::Lane) carries into the run.
pub struct LaneEvents {
    supported: BTreeSet<String>,
    rx: mpsc::UnboundedReceiver<String>,
    pending: BTreeMap<String, usize>,
}

/// The lane's reporting half, cloned to wherever the lane observes the event.
#[derive(Debug, Clone)]
pub struct LaneEventSender(mpsc::UnboundedSender<String>);

impl LaneEventSender {
    /// One occurrence of `event`. A run already over drops it.
    pub fn occurred(&self, event: &str) {
        let _ = self.0.send(event.to_string());
    }
}

impl LaneEvents {
    /// A reporter and the receiver for a lane that reports `supported`.
    pub fn channel<I, S>(supported: I) -> (LaneEventSender, LaneEvents)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let supported = supported.into_iter().map(Into::into).collect();
        (LaneEventSender(tx), LaneEvents { supported, rx, pending: BTreeMap::new() })
    }

    /// Whether the lane reports `event` at all.
    pub fn supports(&self, event: &str) -> bool {
        self.supported.contains(event)
    }

    /// Whether an occurrence of `event` is waiting for a send.
    pub(crate) fn ready(&self, event: &str) -> bool {
        self.pending.get(event).is_some_and(|n| *n > 0)
    }

    /// Hand one occurrence of `event` to the send it releases.
    pub(crate) fn consume(&mut self, event: &str) {
        if let Some(n) = self.pending.get_mut(event) {
            *n = n.saturating_sub(1);
        }
    }

    /// The next report. Pends forever once every reporter is gone, so a lane
    /// that stopped reporting never wakes the run.
    pub(crate) async fn next(&mut self) -> String {
        match self.rx.recv().await {
            Some(event) => event,
            None => std::future::pending().await,
        }
    }

    /// Book a reported occurrence.
    pub(crate) fn record(&mut self, event: String) {
        *self.pending.entry(event).or_default() += 1;
    }
}
