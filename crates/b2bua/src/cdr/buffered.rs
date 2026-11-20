//! Buffered CDR writer. `write` enqueues
//! non-blocking (drop-on-overload, counted; the call is copied only once the
//! queue holds a slot for it); a drainer task performs the inner write.
//! `queue_max == 0` is passthrough (the inner writer is called inline), the
//! mode fake-clock tests use.
//!
//! `close` is the exit barrier: the drainer closes the queue (a later `write`
//! is a counted drop), writes what is queued until the deadline, gives up and
//! counts the rest, then closes the inner writer within the same deadline.
//!
//! The queue, the close deadline and the closed signal outlive a drainer run,
//! so a panic in the drainer or in the sink it drives loses no queued record
//! and never ends the worker ([`drainer`]).

mod drainer;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::{timeout_at, Instant};

use call::Call;

use super::{CdrRecord, CdrWriter};
use crate::metrics::B2buaMetrics;
use drainer::Drainer;

#[derive(Clone)]
pub struct BufferedCdrWriter {
    inner: Arc<dyn CdrWriter>,
    tx: Option<mpsc::Sender<(Call, i64)>>,
    /// The close deadline the drainer reads; `None` while it serves.
    close_by: Arc<watch::Sender<Option<Instant>>>,
    /// Turns `true` once the drainer has emptied its queue and closed `inner`.
    closed: watch::Receiver<bool>,
    dropped: Dropped,
    /// The drainer's supervisor, while buffering: it ends once a run returns,
    /// after a close or once every writer is gone, never on a panic.
    drainer: Option<tokio::task::AbortHandle>,
}

/// The records this buffer dropped: counted locally for `dropped_total()` and
/// in the shared registry's `cdr_dropped_total`.
#[derive(Clone)]
struct Dropped {
    count: Arc<AtomicU64>,
    metrics: B2buaMetrics,
}

impl Dropped {
    fn one(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.metrics.bump_cdr_dropped();
    }
}

impl BufferedCdrWriter {
    /// The deepest submit queue, in records: a thousand times the default
    /// depth, far below the channel's permit ceiling.
    pub const MAX_QUEUE: usize = 1_048_576;

    /// Wrap `inner` with a bounded submit queue + drainer. `queue_max == 0`
    /// disables buffering (passthrough); a depth above [`Self::MAX_QUEUE`] is
    /// held to it. `metrics` is the shared registry the core exports; overflow
    /// drops are recorded into `cdr_dropped_total`.
    pub fn spawn(inner: Arc<dyn CdrWriter>, queue_max: usize, metrics: B2buaMetrics) -> Self {
        let (close_by, _) = watch::channel(None);
        let (closed_tx, closed) = watch::channel(false);
        let mut writer = Self {
            inner,
            tx: None,
            close_by: Arc::new(close_by),
            closed,
            dropped: Dropped { count: Arc::new(AtomicU64::new(0)), metrics },
            drainer: None,
        };
        if queue_max > 0 {
            #[expect(clippy::disallowed_methods, reason = "the depth is in 1..=MAX_QUEUE here")]
            let (tx, rx) = mpsc::channel::<(Call, i64)>(queue_max.min(Self::MAX_QUEUE));
            writer.tx = Some(tx);
            let drainer = Drainer {
                inner: writer.inner.clone(),
                dropped: writer.dropped.clone(),
                queue: Arc::new(Mutex::new(rx)),
                close_by: writer.close_by.clone(),
                closed: Arc::new(closed_tx),
            };
            let task = observe::spawn_keeping_node(drainer.supervise());
            writer.drainer = Some(task.abort_handle());
        }
        writer
    }

    /// Records dropped: a full queue, a write after the close, a record the
    /// close gave up, or one whose write panicked.
    pub fn dropped_total(&self) -> u64 {
        self.dropped.count.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl CdrWriter for BufferedCdrWriter {
    async fn write(&self, call: &Call, terminated_at: i64) {
        match &self.tx {
            None => self.inner.write(call, terminated_at).await,
            Some(tx) => match tx.try_reserve() {
                Ok(slot) => slot.send((call.clone(), terminated_at)),
                Err(_) => self.dropped.one(),
            },
        }
    }

    async fn read_all(&self) -> Vec<CdrRecord> {
        self.inner.read_all().await
    }

    /// The drainer's supervisor, while buffering.
    fn task(&self) -> Option<tokio::task::AbortHandle> {
        self.drainer.clone()
    }

    /// Closing once a close stated its deadline, or as the inner sink when
    /// unbuffered.
    fn closing(&self) -> bool {
        match self.tx {
            None => self.inner.closing(),
            Some(_) => self.close_by.borrow().is_some(),
        }
    }

    /// The first close waits for the drainer, which reports closed by its
    /// deadline, a panicked run included; a later one waits for that within
    /// its own deadline.
    async fn close(&self, deadline: Instant) {
        if self.tx.is_none() {
            return self.inner.close(deadline).await;
        }
        let first = self.close_by.send_if_modified(|by| {
            let first = by.is_none();
            if first {
                *by = Some(deadline);
            }
            first
        });
        let mut closed = self.closed.clone();
        let drained = closed.wait_for(|closed| *closed);
        if first {
            let _ = drained.await;
        } else {
            let _ = timeout_at(deadline, drained).await;
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod panic_tests;
