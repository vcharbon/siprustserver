//! Observability surface. Backed by shared atomics so callers read it
//! synchronously off the actor thread (the owner task updates the atomics
//! before it replies to a command, so a read right after an `await` reflects
//! the mutation — see `layer`).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::event::{EventQueueDropReason, TransactionEvent};

#[derive(Debug)]
pub(crate) struct MetricsInner {
    pub active_transactions: AtomicUsize,
    /// Live entries in the internal `DelayQueue` (retransmit / timeout / cleanup /
    /// event-retry timers). Should track ~a small multiple of `active_transactions`;
    /// a climb while txns are flat is a timer/slab leak (orphaned DelayQueue
    /// entries never removed or fired — the no-chaos RSS-climb suspect).
    pub timer_queue_len: AtomicUsize,
    /// Sum of per-txn `retransmit_buf` bytes (the serialized requests held for
    /// retransmission). Sampled in the sweep; a climb vs flat txns = buffers
    /// retained past completion.
    pub retransmit_buf_bytes: AtomicU64,
    pub messages_processed: AtomicU64,
    pub inbound_message_bytes_total: AtomicU64,
    pub outbound_message_bytes_total: AtomicU64,
    pub outbound_messages_total: AtomicU64,
    /// Per-reason drop counters, indexed by [`EventQueueDropReason::index`].
    pub event_queue_drops: [AtomicU64; 6],
    pub txn_cancelled_on_call_evict: AtomicU64,
    /// CANCELs held back because their INVITE client txn had no response yet
    /// (RFC 3261 §9.1 — the CANCEL waits for the first provisional).
    pub cancels_held: AtomicU64,
    /// Held CANCELs put on the wire when the first provisional arrived.
    pub held_cancels_flushed: AtomicU64,
    /// Held CANCELs put on the wire pre-1xx when the grace window expired (or
    /// the call was evicted) with the branch still response-less — ADR-0028:
    /// every emitted CANCEL reaches the wire.
    pub held_cancels_flushed_pre1xx: AtomicU64,
    /// Grace-sent CANCELs re-sent once on the branch's late first provisional
    /// (the UAS that 481'd the pre-1xx copy has its server txn by then).
    /// Informational — outside the held == flushed + flushed_pre1xx + dropped
    /// reconciliation.
    pub held_cancels_reflushed: AtomicU64,
    /// Held CANCELs cleared without EVER reaching the wire — the txn took a
    /// final first (cancellation moot, §9.2) or died inside the grace window.
    pub held_cancels_dropped: AtomicU64,
    /// CANCELs suppressed at send because their INVITE client txn had already
    /// taken its final (Completed) — §9.1/§9.2: a CANCEL has no effect on an
    /// answered request; sending it would put a pre-1xx CANCEL on the wire.
    pub cancels_suppressed_on_final: AtomicU64,
    /// RFC 3261 §17.2.1 Timer-G retransmissions of an INVITE server txn's non-2xx
    /// final (the reject the caller has not yet ACKed). Zero under no loss (the ACK
    /// beats the 500 ms Timer G); a climb tracks loss on the caller-facing reject
    /// path — the retransmits that heal a dropped final instead of wedging the
    /// caller for the full 32 s.
    pub server_final_retransmits: AtomicU64,
    /// Inbound packets the parser rejected (dropped). A persistent climb here vs a
    /// flat `messages_processed` is the signature of a malformed-traffic flood or a
    /// parser regression — distinguishable from "no traffic arrived".
    pub parse_errors: AtomicU64,
    /// Outbound `send_to` failures (logged-and-swallowed so a send error never
    /// aborts the owner). A climb here means the socket is failing (ENOBUFS/EPERM
    /// under netfilter churn) while everything else looks idle.
    pub send_errors: AtomicU64,
}

impl MetricsInner {
    pub(crate) fn new() -> Self {
        Self {
            active_transactions: AtomicUsize::new(0),
            timer_queue_len: AtomicUsize::new(0),
            retransmit_buf_bytes: AtomicU64::new(0),
            messages_processed: AtomicU64::new(0),
            inbound_message_bytes_total: AtomicU64::new(0),
            outbound_message_bytes_total: AtomicU64::new(0),
            outbound_messages_total: AtomicU64::new(0),
            event_queue_drops: Default::default(),
            txn_cancelled_on_call_evict: AtomicU64::new(0),
            cancels_held: AtomicU64::new(0),
            held_cancels_flushed: AtomicU64::new(0),
            held_cancels_flushed_pre1xx: AtomicU64::new(0),
            held_cancels_reflushed: AtomicU64::new(0),
            held_cancels_dropped: AtomicU64::new(0),
            cancels_suppressed_on_final: AtomicU64::new(0),
            server_final_retransmits: AtomicU64::new(0),
            parse_errors: AtomicU64::new(0),
            send_errors: AtomicU64::new(0),
        }
    }
}

/// Cloneable read handle over the live transaction-layer atomics.
#[derive(Clone)]
pub struct TransactionMetrics {
    inner: Arc<MetricsInner>,
    /// A clone of the output `events` sender — its capacity is how we read the
    /// bounded queue's depth/capacity without owning the receiver.
    events_tx: mpsc::Sender<TransactionEvent>,
}

impl TransactionMetrics {
    pub(crate) fn new(inner: Arc<MetricsInner>, events_tx: mpsc::Sender<TransactionEvent>) -> Self {
        Self { inner, events_tx }
    }

    /// Current number of active transactions (gauge).
    pub fn active_transactions(&self) -> usize {
        self.inner.active_transactions.load(Ordering::Relaxed)
    }

    /// Live entries in the internal timer `DelayQueue` (gauge).
    pub fn timer_queue_len(&self) -> usize {
        self.inner.timer_queue_len.load(Ordering::Relaxed)
    }

    /// Sum of per-txn retransmit-buffer bytes (gauge, sampled in the sweep).
    pub fn retransmit_buf_bytes(&self) -> u64 {
        self.inner.retransmit_buf_bytes.load(Ordering::Relaxed)
    }

    /// Total inbound SIP messages parsed since start (counter).
    pub fn messages_processed(&self) -> u64 {
        self.inner.messages_processed.load(Ordering::Relaxed)
    }

    pub fn inbound_message_bytes_total(&self) -> u64 {
        self.inner.inbound_message_bytes_total.load(Ordering::Relaxed)
    }
    pub fn outbound_message_bytes_total(&self) -> u64 {
        self.inner.outbound_message_bytes_total.load(Ordering::Relaxed)
    }
    pub fn outbound_messages_total(&self) -> u64 {
        self.inner.outbound_messages_total.load(Ordering::Relaxed)
    }

    /// Static capacity of the bounded inbound→app event queue.
    pub fn event_queue_capacity(&self) -> usize {
        self.events_tx.max_capacity()
    }

    /// Current depth of the bounded event queue (gauge).
    pub fn event_queue_depth(&self) -> usize {
        self.events_tx.max_capacity() - self.events_tx.capacity()
    }

    /// Events dropped because the output queue was at capacity, by reason.
    pub fn event_queue_drops(&self, reason: EventQueueDropReason) -> u64 {
        self.inner.event_queue_drops[reason.index()].load(Ordering::Relaxed)
    }

    /// Sum of all per-reason drop counters.
    pub fn event_queue_drops_total(&self) -> u64 {
        EventQueueDropReason::ALL
            .iter()
            .map(|r| self.event_queue_drops(*r))
            .sum()
    }

    /// Client transactions torn down because their owning call was evicted.
    pub fn txn_cancelled_on_call_evict(&self) -> u64 {
        self.inner.txn_cancelled_on_call_evict.load(Ordering::Relaxed)
    }

    /// CANCELs held back awaiting their INVITE's first provisional (§9.1).
    pub fn cancels_held(&self) -> u64 {
        self.inner.cancels_held.load(Ordering::Relaxed)
    }

    /// Held CANCELs flushed to the wire on the first provisional.
    pub fn held_cancels_flushed(&self) -> u64 {
        self.inner.held_cancels_flushed.load(Ordering::Relaxed)
    }

    /// Held CANCELs sent pre-1xx at grace expiry / evict (ADR-0028).
    pub fn held_cancels_flushed_pre1xx(&self) -> u64 {
        self.inner.held_cancels_flushed_pre1xx.load(Ordering::Relaxed)
    }

    /// Grace-sent CANCELs re-sent once on a late first provisional.
    pub fn held_cancels_reflushed(&self) -> u64 {
        self.inner.held_cancels_reflushed.load(Ordering::Relaxed)
    }

    /// Held CANCELs cleared without ever reaching the wire (final beat the
    /// grace window, or the txn died inside it).
    pub fn held_cancels_dropped(&self) -> u64 {
        self.inner.held_cancels_dropped.load(Ordering::Relaxed)
    }

    /// CANCELs suppressed at send: the INVITE txn had already taken its final.
    pub fn cancels_suppressed_on_final(&self) -> u64 {
        self.inner.cancels_suppressed_on_final.load(Ordering::Relaxed)
    }

    /// Timer-G retransmissions of an INVITE server txn's unACKed non-2xx final
    /// (RFC 3261 §17.2.1 caller-facing reject recovery under loss).
    pub fn server_final_retransmits(&self) -> u64 {
        self.inner.server_final_retransmits.load(Ordering::Relaxed)
    }

    /// Inbound packets the parser rejected and dropped (counter).
    pub fn parse_errors(&self) -> u64 {
        self.inner.parse_errors.load(Ordering::Relaxed)
    }

    /// Outbound `send_to` failures (counter).
    pub fn send_errors(&self) -> u64 {
        self.inner.send_errors.load(Ordering::Relaxed)
    }
}
