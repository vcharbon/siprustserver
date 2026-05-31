//! `PacketQueue` — the bounded inbound queue shared by both the real and
//! simulated endpoints.
//!
//! Rust analogue of the TS `Queue.bounded<UdpPacket>(queueMax)` with the
//! `offerUnsafe` (non-blocking, tail-drop on full) / `take` / `poll` /
//! `sizeUnsafe` surface. We hand-roll it rather than use `tokio::mpsc`
//! because the audit decorator needs `depth()` (mpsc doesn't expose its len)
//! and the simulated fabric needs an offer that reports "full" so the caller
//! can bump the tail-drop counter — exactly the bounded-queue semantics the
//! source relied on.
//!
//! Backed by a `Mutex<VecDeque>` + a `Notify` for the async `take`. Counters
//! live on the endpoint (see `Counters`), not here, so this type stays a pure
//! queue.

use std::collections::VecDeque;
use std::sync::Mutex;

use tokio::sync::Notify;

use crate::types::UdpPacket;

struct Inner {
    buf: VecDeque<UdpPacket>,
    closed: bool,
}

pub struct PacketQueue {
    inner: Mutex<Inner>,
    notify: Notify,
    cap: usize,
}

impl PacketQueue {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                buf: VecDeque::new(),
                closed: false,
            }),
            notify: Notify::new(),
            cap,
        }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Non-blocking enqueue. Returns `false` (without enqueuing) when the
    /// queue is at capacity or already closed — the caller treats `false` as a
    /// tail-drop. Wakes one waiting `take`.
    pub fn offer(&self, pkt: UdpPacket) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.closed || g.buf.len() >= self.cap {
            return false;
        }
        g.buf.push_back(pkt);
        drop(g);
        self.notify.notify_one();
        true
    }

    /// Await the next packet. Returns `None` once the queue is closed and
    /// drained. Cancellation-safe.
    pub async fn take(&self) -> Option<UdpPacket> {
        loop {
            // Arm the notification BEFORE inspecting the buffer so a packet
            // offered between the check and the await is not lost.
            let notified = self.notify.notified();
            {
                let mut g = self.inner.lock().unwrap();
                if let Some(pkt) = g.buf.pop_front() {
                    return Some(pkt);
                }
                if g.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Non-blocking dequeue (the TS `poll` → `UdpPacket | null`).
    pub fn poll(&self) -> Option<UdpPacket> {
        self.inner.lock().unwrap().buf.pop_front()
    }

    /// Current queued count (the TS `sizeUnsafe`). Drives the audit's
    /// queue-leak check and the pre-ingress depth argument.
    pub fn depth(&self) -> usize {
        self.inner.lock().unwrap().buf.len()
    }

    /// Close the queue: no further offers succeed, pending/future `take`s
    /// return `None` once drained.
    pub fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.notify.notify_waiters();
    }
}
