//! Buffered CDR writer — port of `BufferedCdrLayer`. `write` enqueues
//! non-blocking (drop-on-overload, counted); a drainer task performs the inner
//! write. `queue_max == 0` is passthrough (the inner writer is called inline) —
//! the fake-clock test mode the source uses.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use call::Call;

use super::{CdrRecord, CdrWriter};
use crate::metrics::B2buaMetrics;

#[derive(Clone)]
pub struct BufferedCdrWriter {
    inner: Arc<dyn CdrWriter>,
    tx: Option<mpsc::Sender<(Call, i64)>>,
    dropped: Arc<AtomicU64>,
    /// Shared b2bua registry: a submit-queue overflow bumps `cdr_dropped_total`
    /// (alongside the local `dropped` counter kept for `dropped_total()`).
    metrics: B2buaMetrics,
}

impl BufferedCdrWriter {
    /// Wrap `inner` with a bounded submit queue + drainer. `queue_max == 0`
    /// disables buffering (passthrough). `metrics` is the shared registry the core
    /// exports; overflow drops are recorded into `cdr_dropped_total`.
    pub fn spawn(inner: Arc<dyn CdrWriter>, queue_max: usize, metrics: B2buaMetrics) -> Self {
        if queue_max == 0 {
            return Self {
                inner,
                tx: None,
                dropped: Arc::new(AtomicU64::new(0)),
                metrics,
            };
        }
        let (tx, mut rx) = mpsc::channel::<(Call, i64)>(queue_max);
        let drain_inner = inner.clone();
        tokio::spawn(async move {
            while let Some((call, ts)) = rx.recv().await {
                drain_inner.write(&call, ts).await;
            }
        });
        Self {
            inner,
            tx: Some(tx),
            dropped: Arc::new(AtomicU64::new(0)),
            metrics,
        }
    }

    pub fn dropped_total(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl CdrWriter for BufferedCdrWriter {
    async fn write(&self, call: &Call, terminated_at: i64) {
        match &self.tx {
            None => self.inner.write(call, terminated_at).await,
            Some(tx) => {
                if tx.try_send((call.clone(), terminated_at)).is_err() {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    self.metrics.bump_cdr_dropped();
                }
            }
        }
    }

    async fn read_all(&self) -> Vec<CdrRecord> {
        self.inner.read_all().await
    }
}
