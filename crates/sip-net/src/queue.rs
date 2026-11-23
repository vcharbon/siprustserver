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
//! long its oldest packet has waited ([`PacketQueue::load`]). A small lane
//! ([`PacketQueue::offer_ahead`]) is taken before everything that waits.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::types::{PreIngressAction, RecvDisposition, RecvTap, UdpPacket};

/// An inbound queue read at one instant: how many datagrams wait in it, the
/// lane taken ahead left out, and how long the oldest of them has waited
/// (zero when none waits).
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

/// The bound of the lane taken first ([`PacketQueue::offer_ahead`]): a few
/// probe rounds of a handful of probers.
pub const AHEAD_MAX: usize = 64;

struct Inner {
    /// The queued packets, each with the instant it entered.
    buf: VecDeque<(Instant, UdpPacket)>,
    /// The packets taken ahead of `buf`, oldest first.
    ahead: VecDeque<UdpPacket>,
    /// Whether the last packet taken came from `ahead`.
    lane_gave_last: bool,
    closed: bool,
}

impl Inner {
    /// How long the oldest packet of `buf` has waited; zero when none waits.
    fn wait(&self) -> Duration {
        self.buf.front().map_or(Duration::ZERO, |(entered, _)| entered.elapsed())
    }

    fn depth(&self) -> usize {
        self.buf.len() + self.ahead.len()
    }

    /// The next packet: the lane's oldest, unless the lane gave the last one
    /// and a packet of `buf` waits, which then goes first. A lane kept full
    /// thus takes every other turn at most, never all of them.
    fn pop(&mut self) -> Option<UdpPacket> {
        let from_buf = |inner: &mut Self| inner.buf.pop_front().map(|(_, pkt)| pkt);
        let lane_turn = !self.lane_gave_last || self.buf.is_empty();
        let ahead = if lane_turn { self.ahead.pop_front() } else { None };
        self.lane_gave_last = ahead.is_some();
        ahead.or_else(|| from_buf(self))
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
            inner: Mutex::new(Inner {
                buf: VecDeque::new(),
                ahead: VecDeque::new(),
                lane_gave_last: false,
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
        self.offer_in(pkt, false)
    }

    /// [`offer`](Self::offer), onto the lane taken ahead of the packets that
    /// wait: for the few datagrams whose answer must not wait behind a
    /// backlog (a liveness probe). The lane and the queue are taken in turn
    /// while both hold a packet, so a lane kept full delays the queue by half
    /// and never stops it. The lane holds [`AHEAD_MAX`] packets; past it the
    /// packet is offered as any other. The lane counts in the depth and not
    /// in the wait.
    pub fn offer_ahead(&self, pkt: UdpPacket) -> bool {
        self.offer_in(pkt, true)
    }

    /// Queue `pkt` as a pre-ingress hook's accepting `action` asks:
    /// [`offer_ahead`](Self::offer_ahead) for
    /// [`PreIngressAction::AcceptAhead`], [`offer`](Self::offer) otherwise.
    /// The one place a receive pump queues what its hook accepted.
    pub fn offer_as(&self, action: &PreIngressAction, pkt: UdpPacket) -> bool {
        self.offer_in(pkt, *action == PreIngressAction::AcceptAhead)
    }

    fn offer_in(&self, pkt: UdpPacket, ahead: bool) -> bool {
        let tap = self.tap.get();
        let disp = {
            let mut g = self.lock();
            let ahead = ahead && g.ahead.len() < AHEAD_MAX;
            if g.closed {
                RecvDisposition::InboxClosed
            } else if !ahead && g.buf.len() >= self.cap {
                RecvDisposition::InboxOverflow
            } else {
                if let Some(t) = tap {
                    t(&pkt, RecvDisposition::Delivered);
                }
                if ahead {
                    g.ahead.push_back(pkt);
                } else {
                    g.buf.push_back((Instant::now(), pkt));
                }
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
    ///
    /// Each packet returned spends one unit of the task's cooperative budget,
    /// as a tokio channel's `recv` does: a reader whose queue never empties
    /// still ends its task poll once the budget is spent, its packet left
    /// queued for the next poll, so the other tasks of its thread run and the
    /// poll watchdog never reads an overload as a stuck poll.
    pub async fn take(&self) -> Option<UdpPacket> {
        loop {
            // Arm the notification BEFORE inspecting the buffer so a packet
            // offered between the check and the await is not lost.
            let notified = self.notify.notified();
            {
                let budget = std::future::poll_fn(tokio::task::coop::poll_proceed).await;
                let mut g = self.lock();
                if let Some(pkt) = g.pop() {
                    budget.made_progress();
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
        self.lock().pop()
    }

    /// The queued packets, the lane's then the queue's, each oldest first,
    /// left queued.
    pub fn peek_all(&self) -> Vec<UdpPacket> {
        let g = self.lock();
        g.ahead.iter().chain(g.buf.iter().map(|(_, pkt)| pkt)).cloned().collect()
    }

    /// Current queued count. Drives the audit's queue-leak check.
    pub fn depth(&self) -> usize {
        self.lock().depth()
    }

    /// How long the oldest packet offered as any other has waited, read now;
    /// zero when none waits. A consumer that takes nothing still reads it
    /// grow. A packet offered now waits longer than this while packets arrive
    /// faster than they are taken: by the ratio of the two rates.
    pub fn wait(&self) -> Duration {
        self.lock().wait()
    }

    /// The queue's depth and wait at one instant: the pre-ingress hook's
    /// argument. Both leave the lane out: a hook judges the queue against
    /// the queue's own bound, which the lane does not count toward.
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
    use std::future::Future;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;

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

    fn tagged(tag: u64) -> UdpPacket {
        UdpPacket { arrival_ms: tag, ..packet() }
    }

    #[tokio::test(start_paused = true)]
    async fn a_packet_offered_ahead_is_taken_first_and_past_the_lane_waits_its_turn() {
        let queue = PacketQueue::new(2);
        assert!(queue.offer(tagged(1)));
        assert!(queue.offer(tagged(2)));
        tokio::time::advance(100 * MS).await;
        for n in 0..AHEAD_MAX as u64 {
            assert!(queue.offer_ahead(tagged(100 + n)), "the lane has room beside a full queue");
        }
        assert!(!queue.offer_ahead(tagged(999)), "past the lane it is offered as any other");
        assert_eq!(queue.load(), QueueLoad { depth: 2, wait: 100 * MS }, "the lane left out");
        assert_eq!(queue.depth(), 2 + AHEAD_MAX, "every packet queued");
        let taken = |n: usize| -> Vec<u64> {
            (0..n).filter_map(|_| queue.poll()).map(|p| p.arrival_ms).collect()
        };
        assert_eq!(taken(5), [100, 1, 101, 2, 102], "the lane first, then in turn");
        assert_eq!(taken(AHEAD_MAX).len(), AHEAD_MAX - 3, "the lane alone, to its end");
        assert_eq!(queue.load(), QueueLoad::default());
    }

    #[tokio::test(start_paused = true)]
    async fn a_lane_kept_full_never_stops_the_queue() {
        let queue = PacketQueue::new(8);
        for n in 1..=4 {
            assert!(queue.offer(tagged(n)));
        }
        let mut from_the_queue = Vec::new();
        for round in 0..8 {
            while queue.offer_as(&PreIngressAction::AcceptAhead, tagged(100 + round))
                && queue.depth() < 4 + AHEAD_MAX
            {}
            let next = queue.take().await.expect("a packet waits").arrival_ms;
            if next < 100 {
                from_the_queue.push(next);
            }
        }
        assert_eq!(from_the_queue, [1, 2, 3, 4], "every other take is the queue's");
    }

    /// Tokio's cooperative budget: the ready operations one task poll
    /// completes before its next ready one returns pending.
    const COOP_BUDGET: usize = 128;

    #[tokio::test(start_paused = true)]
    async fn a_reader_of_a_queue_that_never_empties_leaves_the_thread_every_budget() {
        let queue = PacketQueue::new(4);
        let taken = Arc::new(AtomicUsize::new(0));
        let other_ran_at = Arc::new(OnceLock::new());
        let (seen, at) = (taken.clone(), other_ran_at.clone());
        tokio::spawn(async move { at.set(seen.load(Ordering::SeqCst)) });
        while taken.load(Ordering::SeqCst) < 100 * COOP_BUDGET && other_ran_at.get().is_none() {
            assert!(queue.offer(packet()));
            assert!(queue.take().await.is_some());
            taken.fetch_add(1, Ordering::SeqCst);
        }
        let ran_at = other_ran_at.get().copied();
        assert!(
            ran_at.is_some_and(|at| at <= COOP_BUDGET),
            "the other task runs within a budget of takes (after {ran_at:?})"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_take_out_of_budget_leaves_its_packet_queued() {
        let queue = PacketQueue::new(4);
        while tokio::task::coop::has_budget_remaining() {
            tokio::task::consume_budget().await;
        }
        let sent = Bytes::from_static(b"P");
        assert!(queue.offer(UdpPacket { raw: sent.clone(), ..packet() }));
        {
            let mut take = std::pin::pin!(queue.take());
            let first = std::future::poll_fn(|cx| Poll::Ready(take.as_mut().poll(cx))).await;
            assert!(first.is_pending(), "a take out of budget is pending");
        }
        assert_eq!(queue.depth(), 1, "the dropped take left its packet queued");
        let taken = queue.take().await.map(|p| p.raw);
        assert_eq!(taken, Some(sent), "the next take returns it");
    }

    #[tokio::test(start_paused = true)]
    async fn a_pump_queues_a_packet_as_its_hook_accepted_it() {
        let queue = PacketQueue::new(4);
        assert!(queue.offer_as(&PreIngressAction::Accept, tagged(1)));
        assert!(queue.offer_as(&PreIngressAction::Accept, tagged(2)));
        assert!(queue.offer_as(&PreIngressAction::AcceptAhead, tagged(3)));
        assert_eq!(queue.poll().map(|p| p.arrival_ms), Some(3), "accepted ahead: taken first");
        assert_eq!(queue.poll().map(|p| p.arrival_ms), Some(1));
    }
}
