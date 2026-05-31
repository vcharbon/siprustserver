//! `PerCallDispatcher` — the per-call FIFO (port of `PerCallDispatcher.ts`,
//! source ADR-0004/0005). Each call gets a bounded queue + a worker task that
//! runs its handler bodies strictly in order; a global semaphore caps total
//! in-flight handlers so a slow handler on one call never blocks other calls.
//!
//! The handler body is a boxed future (the Rust analogue of the source's
//! type-erased `Effect`). Bodies are run on spawned tasks the worker awaits, so
//! a panicking handler is isolated (`JoinError`) and the worker survives.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, Semaphore};

use crate::metrics::B2buaMetrics;

/// A unit of per-call work — a self-contained future capturing the router +
/// the event.
pub type DispatchBody = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

enum DispatchItem {
    Event(DispatchBody),
    Poison,
}

struct PerCallQueue {
    tx: mpsc::Sender<DispatchItem>,
}

type QueueMap = Arc<Mutex<HashMap<String, PerCallQueue>>>;

/// The dispatcher handle. Clone-cheap.
#[derive(Clone)]
pub struct PerCallDispatcher {
    queues: QueueMap,
    semaphore: Arc<Semaphore>,
    depth: usize,
    cap: usize,
    metrics: B2buaMetrics,
}

impl PerCallDispatcher {
    pub fn new(concurrency: usize, depth: usize, cap: usize, metrics: B2buaMetrics) -> Self {
        Self {
            queues: Arc::new(Mutex::new(HashMap::new())),
            semaphore: Arc::new(Semaphore::new(concurrency.max(1))),
            depth: depth.max(1),
            cap: cap.max(1),
            metrics,
        }
    }

    /// Enqueue a handler body for `call_ref`, lazily creating the queue + worker.
    /// Drops (and counts) the body when the per-call queue is full or the global
    /// queue cap is reached.
    pub fn dispatch(&self, call_ref: &str, body: DispatchBody) {
        let mut map = self.queues.lock().unwrap();
        if let Some(q) = map.get(call_ref) {
            if q.tx.try_send(DispatchItem::Event(body)).is_err() {
                self.metrics.bump_queue_drop();
            }
            return;
        }
        if map.len() >= self.cap {
            self.metrics.bump_cap_drop();
            return;
        }
        let (tx, rx) = mpsc::channel(self.depth);
        // Send before spawning the worker: capacity is fresh so this can't fail.
        let _ = tx.try_send(DispatchItem::Event(body));
        map.insert(call_ref.to_string(), PerCallQueue { tx });
        self.metrics.bump_creation();
        tokio::spawn(worker(
            call_ref.to_string(),
            rx,
            self.queues.clone(),
            self.semaphore.clone(),
            self.metrics.clone(),
        ));
    }

    /// Signal the worker for `call_ref` to drain and exit (call eviction).
    pub fn enqueue_poison(&self, call_ref: &str) {
        let map = self.queues.lock().unwrap();
        if let Some(q) = map.get(call_ref) {
            let _ = q.tx.try_send(DispatchItem::Poison);
        }
    }

    pub fn has_queue(&self, call_ref: &str) -> bool {
        self.queues.lock().unwrap().contains_key(call_ref)
    }

    pub fn queue_count(&self) -> usize {
        self.queues.lock().unwrap().len()
    }
}

async fn worker(
    call_ref: String,
    mut rx: mpsc::Receiver<DispatchItem>,
    queues: QueueMap,
    semaphore: Arc<Semaphore>,
    metrics: B2buaMetrics,
) {
    while let Some(item) = rx.recv().await {
        match item {
            DispatchItem::Poison => {
                while rx.try_recv().is_ok() {}
                break;
            }
            DispatchItem::Event(body) => {
                if semaphore.available_permits() == 0 {
                    metrics.bump_saturation();
                }
                let permit = semaphore.clone().acquire_owned().await.expect("semaphore closed");
                // Isolate handler panics: a JoinError is logged, the worker lives.
                let _ = tokio::spawn(body).await;
                drop(permit);
            }
        }
    }
    queues.lock().unwrap().remove(&call_ref);
    metrics.bump_removal();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::sync::Notify;

    #[tokio::test]
    async fn preserves_per_call_fifo_order() {
        let d = PerCallDispatcher::new(8, 64, 1024, B2buaMetrics::new());
        let order = Arc::new(Mutex::new(Vec::<u32>::new()));
        let done = Arc::new(Notify::new());
        for i in 0..10u32 {
            let order = order.clone();
            let done = done.clone();
            d.dispatch(
                "w0|cid|tag",
                Box::pin(async move {
                    order.lock().unwrap().push(i);
                    if i == 9 {
                        done.notify_one();
                    }
                }),
            );
        }
        done.notified().await;
        assert_eq!(*order.lock().unwrap(), (0..10).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn full_queue_drops_and_counts() {
        let metrics = B2buaMetrics::new();
        // depth 1, concurrency 1: a blocked first handler forces drops behind it.
        let d = PerCallDispatcher::new(1, 1, 1024, metrics.clone());
        let gate = Arc::new(Notify::new());
        let started = Arc::new(Notify::new());
        let ran = Arc::new(AtomicU32::new(0));
        {
            let gate = gate.clone();
            let started = started.clone();
            let ran = ran.clone();
            d.dispatch(
                "c",
                Box::pin(async move {
                    started.notify_one();
                    gate.notified().await;
                    ran.fetch_add(1, Ordering::SeqCst);
                }),
            );
        }
        started.notified().await; // first handler is now parked on the gate
        // Queue depth is 1 — one of these sits in the queue, the rest are dropped.
        for _ in 0..5 {
            let ran = ran.clone();
            d.dispatch("c", Box::pin(async move { ran.fetch_add(1, Ordering::SeqCst); }));
        }
        assert!(metrics.queue_drops_total() >= 1, "expected queue drops");
        gate.notify_waiters();
    }
}
