//! `PacketQueue` — the bounded inbound queue shared by both the real and
//! simulated endpoints.
//!
//! A bounded queue of `queue_max` packets with an offer (non-blocking,
//! tail-drop on full) / `take` / `poll` / `depth` surface. We hand-roll it
//! rather than use `tokio::mpsc` because the audit decorator needs `depth()`
//! (mpsc doesn't expose its len) and the simulated fabric needs an offer that
//! reports "full" so the caller can bump the tail-drop counter.
//!
//! Backed by a `Mutex<VecDeque>` + a `Notify` for the async `take`. Counters
//! live on the endpoint (see `Counters`), not here, so this type stays a pure
//! queue.
//!
//! Each packet is stamped as it enters (`tokio::time`), so the queue tells how
//! long its oldest packet has waited ([`PacketQueue::load`]): at the
//! consumer's present pace, the wait ahead of a packet offered now.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::types::{RecvDisposition, RecvTap, UdpPacket};

/// An inbound queue read at one instant: how many datagrams wait in it, and
/// how long the oldest of them has waited (zero when none waits).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueLoad {
    pub depth: usize,
    pub wait: Duration,
}

impl QueueLoad {
    /// A queue `depth` datagrams deep whose oldest arrived just now.
    pub const fn at_depth(depth: usize) -> Self {
        Self { depth, wait: Duration::ZERO }
    }
}

struct Inner {
    /// The queued packets, each with the instant it entered.
    buf: VecDeque<(Instant, UdpPacket)>,
    closed: bool,
}

impl Inner {
    /// How long the oldest queued packet has waited; zero when none waits.
    fn wait(&self) -> Duration {
        self.buf.front().map_or(Duration::ZERO, |(entered, _)| entered.elapsed())
    }
}

pub struct PacketQueue {
    inner: Mutex<Inner>,
    notify: Notify,
    cap: usize,
    /// Delivery-time recording tap. Installed once by
    /// the recording decorator on sampled endpoints; `None` forever on the
    /// non-recording path (one atomic load per offer — no other overhead).
    tap: OnceLock<RecvTap>,
}

impl PacketQueue {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(Inner { buf: VecDeque::new(), closed: false }),
            notify: Notify::new(),
            cap,
            tap: OnceLock::new(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// The queue state, recovered from a poisoned lock: every critical
    /// section leaves `Inner` whole, and [`close`](Self::close) runs from a
    /// `Drop`, where a panic aborts.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        crate::poison::locked(&self.inner)
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
            let mut g = self.lock();
            if g.closed {
                RecvDisposition::InboxClosed
            } else if g.buf.len() >= self.cap {
                RecvDisposition::InboxOverflow
            } else {
                if let Some(t) = tap {
                    t(&pkt, RecvDisposition::Delivered);
                }
                g.buf.push_back((Instant::now(), pkt));
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
                let mut g = self.lock();
                if let Some((_, pkt)) = g.buf.pop_front() {
                    return Some(pkt);
                }
                if g.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Non-blocking dequeue: `None` when the queue is empty.
    pub fn poll(&self) -> Option<UdpPacket> {
        self.lock().buf.pop_front().map(|(_, pkt)| pkt)
    }

    /// The queued packets, oldest first, left queued.
    pub fn peek_all(&self) -> Vec<UdpPacket> {
        self.lock().buf.iter().map(|(_, pkt)| pkt.clone()).collect()
    }

    /// Current queued count. Drives the audit's queue-leak check.
    pub fn depth(&self) -> usize {
        self.lock().buf.len()
    }

    /// How long the oldest queued packet has waited, read now; zero when none
    /// waits. A consumer that takes nothing still reads it grow.
    pub fn wait(&self) -> Duration {
        self.lock().wait()
    }

    /// The queue's depth and wait at one instant: the pre-ingress hook's
    /// argument.
    pub fn load(&self) -> QueueLoad {
        let g = self.lock();
        QueueLoad { depth: g.buf.len(), wait: g.wait() }
    }

    /// A guard that closes the queue when dropped: held by a pump, it closes
    /// the queue on any exit — return, abort or panic — so the consumer's
    /// `take` never waits on a pump that is gone.
    pub fn close_on_drop(self: &Arc<Self>) -> CloseOnDrop {
        CloseOnDrop(self.clone())
    }

    /// Close the queue: no further offers succeed, pending/future `take`s
    /// return `None` once drained.
    pub fn close(&self) {
        self.lock().closed = true;
        self.notify.notify_waiters();
    }
}

/// Closes its queue when dropped ([`PacketQueue::close_on_drop`]).
pub struct CloseOnDrop(Arc<PacketQueue>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use sip_message::Bytes;

    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn packet() -> UdpPacket {
        UdpPacket {
            raw: Bytes::from_static(b"x"),
            src: SocketAddr::from(([192, 0, 2, 1], 5060)),
            arrival_ms: 0,
            parsed: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_is_the_oldest_queued_packets_and_zero_when_none_waits() {
        let queue = PacketQueue::new(4);
        assert_eq!(queue.load(), QueueLoad::default(), "an empty queue");
        assert!(queue.offer(packet()));
        tokio::time::advance(100 * MS).await;
        assert!(queue.offer(packet()));
        tokio::time::advance(300 * MS).await;
        assert_eq!(queue.load(), QueueLoad { depth: 2, wait: 400 * MS }, "the first packet's");
        assert!(queue.poll().is_some());
        assert_eq!(queue.wait(), 300 * MS, "then the second's");
        assert!(queue.take().await.is_some());
        assert_eq!(queue.load(), QueueLoad::default(), "none waits");
    }
}
