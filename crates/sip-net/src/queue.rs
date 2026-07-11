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
use std::sync::{Mutex, OnceLock};

use tokio::sync::Notify;

use crate::types::{RecvDisposition, RecvTap, UdpPacket};

struct Inner {
    buf: VecDeque<UdpPacket>,
    closed: bool,
}

pub struct PacketQueue {
    inner: Mutex<Inner>,
    notify: Notify,
    cap: usize,
    /// Delivery-time recording tap (newkahneed-036 ask A). Installed once by
    /// the recording decorator on sampled endpoints; `None` forever on the
    /// non-recording path (one atomic load per offer — no other overhead).
    tap: OnceLock<RecvTap>,
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
            tap: OnceLock::new(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Install the delivery tap. First installer wins (the recording decorator
    /// installs exactly one, right after bind).
    pub fn install_tap(&self, tap: RecvTap) {
        let _ = self.tap.set(tap);
    }

    /// The installed tap, if any — for feeders (the loadgen mux) that discard a
    /// datagram BEFORE offering it (loss model, retransmit absorb) and must
    /// still record the arrival.
    pub fn recv_tap(&self) -> Option<RecvTap> {
        self.tap.get().cloned()
    }

    /// Non-blocking enqueue. Returns `false` (without enqueuing) when the
    /// queue is at capacity or already closed — the caller treats `false` as a
    /// tail-drop. Wakes one waiting `take`. When a tap is installed the
    /// outcome (delivered / overflow / closed) is reported to it. The
    /// `Delivered` report fires under the queue lock, BEFORE the packet becomes
    /// poppable — so a recorded arrival always sequences before its recorded
    /// consumption (the tap is a plain channel push; it takes no queue lock).
    pub fn offer(&self, pkt: UdpPacket) -> bool {
        let tap = self.tap.get();
        let disp = {
            let mut g = self.inner.lock().unwrap();
            if g.closed {
                RecvDisposition::InboxClosed
            } else if g.buf.len() >= self.cap {
                RecvDisposition::InboxOverflow
            } else {
                if let Some(t) = tap {
                    t(&pkt, RecvDisposition::Delivered);
                }
                g.buf.push_back(pkt);
                drop(g);
                self.notify.notify_one();
                return true;
            }
        };
        if let Some(t) = tap {
            t(&pkt, disp);
        }
        false
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
