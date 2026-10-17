//! Buffered CDR writer — port of `BufferedCdrLayer`. `write` enqueues
//! non-blocking (drop-on-overload, counted); a drainer task performs the inner
//! write. `queue_max == 0` is passthrough (the inner writer is called inline) —
//! the fake-clock test mode the source uses.
//!
//! `close` is the exit barrier: the drainer closes the queue (a later `write`
//! is a counted drop), writes what is queued until the deadline, gives up and
//! counts the rest, then closes the inner writer within the same deadline.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use tokio::time::{timeout_at, Instant};

use call::Call;

use super::{CdrRecord, CdrWriter};
use crate::metrics::B2buaMetrics;

#[derive(Clone)]
pub struct BufferedCdrWriter {
    inner: Arc<dyn CdrWriter>,
    tx: Option<mpsc::Sender<(Call, i64)>>,
    /// The close deadline the drainer reads; `None` while it serves.
    close_by: Arc<watch::Sender<Option<Instant>>>,
    /// Turns `true` once the drainer has emptied its queue and closed `inner`.
    closed: watch::Receiver<bool>,
    dropped: Dropped,
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
    /// Wrap `inner` with a bounded submit queue + drainer. `queue_max == 0`
    /// disables buffering (passthrough). `metrics` is the shared registry the core
    /// exports; overflow drops are recorded into `cdr_dropped_total`.
    pub fn spawn(inner: Arc<dyn CdrWriter>, queue_max: usize, metrics: B2buaMetrics) -> Self {
        let (close_by, close_rx) = watch::channel(None);
        let (closed_tx, closed) = watch::channel(false);
        let mut writer = Self {
            inner,
            tx: None,
            close_by: Arc::new(close_by),
            closed,
            dropped: Dropped { count: Arc::new(AtomicU64::new(0)), metrics },
        };
        if queue_max > 0 {
            let (tx, rx) = mpsc::channel::<(Call, i64)>(queue_max);
            writer.tx = Some(tx);
            let drainer = Drainer { inner: writer.inner.clone(), dropped: writer.dropped.clone() };
            observe::spawn_keeping_node(drainer.run(rx, close_rx, closed_tx));
        }
        writer
    }

    /// Records dropped: a full queue, a write after the close, or a record the
    /// close gave up.
    pub fn dropped_total(&self) -> u64 {
        self.dropped.count.load(Ordering::Relaxed)
    }
}

/// The drainer task's half: it holds no sender, so the queue ends once every
/// writer is gone.
struct Drainer {
    inner: Arc<dyn CdrWriter>,
    dropped: Dropped,
}

impl Drainer {
    /// Writes each queued record until a close deadline is set, then closes
    /// the queue, writes what is left until the deadline (each record past it
    /// is dropped), closes `inner` and reports `closed`. Ends without a close
    /// once every writer is gone.
    async fn run(
        self,
        mut rx: mpsc::Receiver<(Call, i64)>,
        mut close_by: watch::Receiver<Option<Instant>>,
        closed: watch::Sender<bool>,
    ) {
        let deadline = loop {
            let by = *close_by.borrow_and_update();
            if let Some(deadline) = by {
                break deadline;
            }
            tokio::select! {
                biased;
                Ok(()) = close_by.changed() => {}
                record = rx.recv() => match record {
                    Some((call, at)) => self.write(&call, at, &mut close_by).await,
                    None => return,
                },
            }
        };
        rx.close();
        while let Some((call, at)) = rx.recv().await {
            if Instant::now() < deadline {
                self.write(&call, at, &mut close_by).await;
            } else {
                self.dropped.one();
            }
        }
        self.inner.close(deadline).await;
        closed.send_replace(true);
    }

    /// Writes one record; once a close deadline is set the write runs until
    /// that deadline at most, and a write cut there is dropped.
    async fn write(&self, call: &Call, at: i64, close_by: &mut watch::Receiver<Option<Instant>>) {
        let write = self.inner.write(call, at);
        tokio::pin!(write);
        loop {
            let by = *close_by.borrow_and_update();
            if let Some(deadline) = by {
                if timeout_at(deadline, &mut write).await.is_err() {
                    self.dropped.one();
                }
                return;
            }
            tokio::select! {
                () = &mut write => return,
                Ok(()) = close_by.changed() => {}
            }
        }
    }
}

#[async_trait]
impl CdrWriter for BufferedCdrWriter {
    async fn write(&self, call: &Call, terminated_at: i64) {
        match &self.tx {
            None => self.inner.write(call, terminated_at).await,
            Some(tx) => {
                if tx.try_send((call.clone(), terminated_at)).is_err() {
                    self.dropped.one();
                }
            }
        }
    }

    async fn read_all(&self) -> Vec<CdrRecord> {
        self.inner.read_all().await
    }

    /// The first close waits for the drainer, which ends at its deadline; a
    /// later one waits for that within its own deadline.
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
