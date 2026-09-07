//! Observability surface. Backed by shared atomics so callers read it
//! synchronously off the actor thread (the owner task updates the atomics
//! before it replies to a command, so a read right after an `await` reflects
//! the mutation — see `layer`).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use sip_message::Method;
use sip_retransmit::Class;
use tokio::sync::mpsc;

use crate::event::{EventQueueDropReason, TransactionEvent};

/// The transaction ladders this layer drives, in the order the family
/// enumerates them. A dialog-level class is the TU's and never reaches here.
const REQUEST_LADDERS: [Class; 4] = [
    Class::InviteClient,
    Class::NonInviteClient,
    Class::NonInviteProceeding,
    Class::CancelClient,
];

/// The `method` label slots of a request ladder's rows: every method
/// `sip_message` models natively, plus one bucket for an extension method, so
/// the family stays a fixed set of atomics whatever the wire carries.
const METHOD_LABELS: [&str; 15] = [
    "INVITE", "ACK", "BYE", "CANCEL", "OPTIONS", "REGISTER", "INFO", "UPDATE", "PRACK", "SUBSCRIBE",
    "NOTIFY", "PUBLISH", "MESSAGE", "REFER", "OTHER",
];

/// The `method` label slot of `method` — the index a rung is counted at, so
/// a fire path resolves it once, before the send, and allocates nothing.
pub(crate) fn method_slot(method: &Method) -> usize {
    match method {
        Method::Invite => 0,
        Method::Ack => 1,
        Method::Bye => 2,
        Method::Cancel => 3,
        Method::Options => 4,
        Method::Register => 5,
        Method::Info => 6,
        Method::Update => 7,
        Method::Prack => 8,
        Method::Subscribe => 9,
        Method::Notify => 10,
        Method::Publish => 11,
        Method::Message => 12,
        Method::Refer => 13,
        Method::Other(_) => 14,
    }
}

/// The lowest status Timer G ever paces: a non-2xx INVITE final (§17.2.1).
const FIRST_FINAL_CODE: u16 = 300;
/// One slot per status 300..=699.
const FINAL_CODES: usize = 400;
/// The lowest status a server transaction caches for replay (its auto-100).
const FIRST_TRIGGER_CODE: u16 = 100;
/// One slot per status 100..=699.
const TRIGGER_CODES: usize = 600;

/// The `ladder` label of a repeat no timer paced.
const TRIGGER: &str = "trigger";

/// `{ladder,method,code}` — one row per repeat this layer put on the wire: a
/// transaction-ladder rung — Timer A / Timer E (`method` is the request's, no
/// `code`), the CANCEL sub-ladder, Timer G (`INVITE` and the final's status)
/// — and the `trigger` replay of a server transaction's cached response to a
/// retransmitted request (§17.2.1; the request's method and the response's
/// status). A fixed set of atomics keyed by class and label slot: the owner
/// task bumps one with no lock, and the scrape reads them off-thread.
#[derive(Debug)]
pub(crate) struct RetransmitFamily {
    /// `[ladder][method]` for the request ladders.
    requests: [[AtomicU64; METHOD_LABELS.len()]; REQUEST_LADDERS.len()],
    /// `[status - 300]` for `InviteServerFinal`.
    finals: [AtomicU64; FINAL_CODES],
    /// `[method][status - 100]` for the cached-response replay.
    triggers: [[AtomicU64; TRIGGER_CODES]; METHOD_LABELS.len()],
}

/// One row of the retransmit family with a non-zero count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetransmitRow {
    pub ladder: &'static str,
    pub method: &'static str,
    /// The final's status, on the Timer G row; `None` on a request's.
    pub code: Option<u16>,
    pub count: u64,
}

impl RetransmitFamily {
    fn new() -> Self {
        Self {
            requests: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            finals: std::array::from_fn(|_| AtomicU64::new(0)),
            triggers: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }

    /// Count one rung of `ladder` re-sending a request whose method sits at
    /// `slot` ([`method_slot`]).
    pub(crate) fn record_request(&self, ladder: Class, slot: usize) {
        if let Some(row) = REQUEST_LADDERS.iter().position(|c| *c == ladder) {
            self.requests[row][slot].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Count one Timer G rung re-sending an INVITE's non-2xx final of `code`.
    pub(crate) fn record_final(&self, code: u16) {
        if let Some(slot) = code.checked_sub(FIRST_FINAL_CODE).map(usize::from).filter(|s| *s < FINAL_CODES) {
            self.finals[slot].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Count one replay of the cached response of status `code` to a
    /// retransmitted request of `method`.
    pub(crate) fn record_trigger(&self, method: &Method, code: u16) {
        if let Some(slot) = code.checked_sub(FIRST_TRIGGER_CODE).map(usize::from).filter(|s| *s < TRIGGER_CODES) {
            self.triggers[method_slot(method)][slot].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Every cached-response replay, whatever it repeated.
    fn triggered(&self) -> u64 {
        self.triggers.iter().flatten().map(|a| a.load(Ordering::Relaxed)).sum()
    }

    fn total(&self, ladder: Class) -> u64 {
        match REQUEST_LADDERS.iter().position(|c| *c == ladder) {
            Some(row) => self.requests[row].iter().map(|a| a.load(Ordering::Relaxed)).sum(),
            None if ladder == Class::InviteServerFinal => {
                self.finals.iter().map(|a| a.load(Ordering::Relaxed)).sum()
            }
            None => 0,
        }
    }

    fn rows(&self) -> Vec<RetransmitRow> {
        let mut out = Vec::new();
        for (row, ladder) in REQUEST_LADDERS.iter().enumerate() {
            for (slot, method) in METHOD_LABELS.iter().enumerate() {
                let count = self.requests[row][slot].load(Ordering::Relaxed);
                if count > 0 {
                    out.push(RetransmitRow { ladder: ladder.as_str(), method, code: None, count });
                }
            }
        }
        for (slot, cell) in self.finals.iter().enumerate() {
            let count = cell.load(Ordering::Relaxed);
            if count > 0 {
                out.push(RetransmitRow {
                    ladder: Class::InviteServerFinal.as_str(),
                    method: "INVITE",
                    code: Some(FIRST_FINAL_CODE + slot as u16),
                    count,
                });
            }
        }
        for (row, method) in METHOD_LABELS.iter().enumerate() {
            for (slot, cell) in self.triggers[row].iter().enumerate() {
                let count = cell.load(Ordering::Relaxed);
                if count > 0 {
                    out.push(RetransmitRow {
                        ladder: TRIGGER,
                        method,
                        code: Some(FIRST_TRIGGER_CODE + slot as u16),
                        count,
                    });
                }
            }
        }
        out
    }
}

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
    /// Every repeat this layer put on the wire — a transaction-ladder rung or
    /// a cached response replayed to a retransmitted request — by
    /// `{ladder,method,code}`. Zero under no loss (the answer beats the 500 ms
    /// first rung of every class); a climb on one row names the path losing
    /// datagrams — `cancel-client` the cancellation of an abandoned callee,
    /// `invite-server-final` the caller-facing reject that would otherwise
    /// wedge the caller for the full 32 s.
    pub retransmits: RetransmitFamily,
    /// INVITE transactions rebuilt from a materialised call's record
    /// (`TransactionLayer::seed`, ADR-0014).
    pub txn_seeded: AtomicU64,
    /// Seeds skipped because their branch already held a transaction — the
    /// datagram that triggered the materialisation, or a transaction this node
    /// built itself.
    pub txn_seed_skipped: AtomicU64,
    /// Non-2xx INVITE finals sent on a branch no server transaction held: they
    /// leave raw with no Timer G ladder, so a materialisation that answered an
    /// INVITE it never seeded shows here.
    pub server_final_unseen_branch: AtomicU64,
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
            retransmits: RetransmitFamily::new(),
            txn_seeded: AtomicU64::new(0),
            txn_seed_skipped: AtomicU64::new(0),
            server_final_unseen_branch: AtomicU64::new(0),
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

    /// Every rung `ladder` put on the wire, whatever it repeated (counter).
    /// Zero for a class this layer never paces.
    pub fn retransmits(&self, ladder: Class) -> u64 {
        self.inner.retransmits.total(ladder)
    }

    /// Every `trigger` repeat — a cached response replayed to a retransmitted
    /// request — whatever it repeated (counter).
    pub fn triggered_retransmits(&self) -> u64 {
        self.inner.retransmits.triggered()
    }

    /// The rows of `{ladder,method,code}` with a non-zero count, for a scrape.
    pub fn retransmit_rows(&self) -> Vec<RetransmitRow> {
        self.inner.retransmits.rows()
    }

    /// Timer-E re-sends of an on-wire CANCEL awaiting its response (RFC 3261
    /// §17.1.2.2 — the CANCEL sub-state of the INVITE client txn): the
    /// `cancel-client` ladder's total.
    pub fn cancel_retransmits(&self) -> u64 {
        self.retransmits(Class::CancelClient)
    }

    /// Timer-G retransmissions of an INVITE server txn's unACKed non-2xx final
    /// (RFC 3261 §17.2.1): the `invite-server-final` ladder's total.
    pub fn server_final_retransmits(&self) -> u64 {
        self.retransmits(Class::InviteServerFinal)
    }

    /// INVITE transactions rebuilt by `seed` (counter).
    pub fn txn_seeded(&self) -> u64 {
        self.inner.txn_seeded.load(Ordering::Relaxed)
    }

    /// Seeds skipped because their branch was occupied (counter).
    pub fn txn_seed_skipped(&self) -> u64 {
        self.inner.txn_seed_skipped.load(Ordering::Relaxed)
    }

    /// Non-2xx INVITE finals that left raw on an unseen branch (counter).
    pub fn server_final_unseen_branch(&self) -> u64 {
        self.inner.server_final_unseen_branch.load(Ordering::Relaxed)
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
