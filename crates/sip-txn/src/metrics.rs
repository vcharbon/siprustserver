//! Observability surface. Backed by shared atomics so callers read it
//! synchronously off the actor thread (the owner task updates the atomics
//! before it replies to a command, so a read right after an `await` reflects
//! the mutation — see `layer`).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use metric_catalogue::OpenRows;

use sip_message::Method;
use sip_retransmit::Class;
use tokio::sync::mpsc;

use crate::event::{EventQueueClass, TransactionEvent};
use crate::layer::InviteClass;

/// The transaction ladders this layer drives, in the order the family
/// enumerates them. A dialog-level class is the TU's and never reaches here.
const REQUEST_LADDERS: [Class; 4] =
    [Class::InviteClient, Class::NonInviteClient, Class::NonInviteProceeding, Class::CancelClient];

/// The `method` label slots of the fixed rows: every method `sip_message`
/// models natively. An extension method's rows are held apart, by token.
const METHOD_LABELS: [&str; 14] = Method::NATIVE_TOKENS;

/// The `method` label of a rung, resolved before its send: the fixed slot of
/// a native method (no allocation), or an extension method's token.
#[derive(Debug, Clone)]
pub(crate) enum MethodSlot {
    Native(usize),
    Extension(Box<str>),
}

/// The `method` label of `method`.
pub(crate) fn method_slot(method: &Method) -> MethodSlot {
    let slot = match method {
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
        Method::Other(token) => return MethodSlot::Extension(token.as_str().into()),
    };
    MethodSlot::Native(slot)
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
    /// The rows of extension methods, `[ladder, token]` or `[ladder, token,
    /// code]`, under the family's cap ([`crate::catalogue::RETRANSMITS`]);
    /// past it a rung lands on its ladder's and code's row whose method is
    /// [`OVERFLOW`](metric_catalogue::OVERFLOW).
    extensions: OpenRows,
}

/// One row of the retransmit family with a non-zero count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetransmitRow {
    pub ladder: &'static str,
    pub method: String,
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
            extensions: OpenRows::new(&crate::catalogue::RETRANSMITS),
        }
    }

    /// Rungs of extension methods that landed on the overflow row.
    fn overflowed(&self) -> u64 {
        self.extensions.overflowed()
    }

    /// The extension-method rows' total over the rows of `ladder`.
    fn extension_total(&self, ladder: &str) -> u64 {
        self.extensions
            .rows()
            .iter()
            .filter(|(labels, _)| labels[0] == ladder)
            .map(|(_, n)| n)
            .sum()
    }

    /// Count one rung of `ladder` re-sending a request whose method is
    /// `slot` ([`method_slot`]).
    pub(crate) fn record_request(&self, ladder: Class, slot: MethodSlot) {
        let Some(row) = REQUEST_LADDERS.iter().position(|c| *c == ladder) else { return };
        match slot {
            MethodSlot::Native(slot) => {
                self.requests[row][slot].fetch_add(1, Ordering::Relaxed);
            }
            MethodSlot::Extension(token) => self.extensions.add(&[ladder.as_str(), &token], 1),
        }
    }

    /// Count one Timer G rung re-sending an INVITE's non-2xx final of `code`.
    pub(crate) fn record_final(&self, code: u16) {
        if let Some(slot) =
            code.checked_sub(FIRST_FINAL_CODE).map(usize::from).filter(|s| *s < FINAL_CODES)
        {
            self.finals[slot].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Count one replay of the cached response of status `code` to a
    /// retransmitted request of `method`.
    pub(crate) fn record_trigger(&self, method: &Method, code: u16) {
        let Some(slot) =
            code.checked_sub(FIRST_TRIGGER_CODE).map(usize::from).filter(|s| *s < TRIGGER_CODES)
        else {
            return;
        };
        match method_slot(method) {
            MethodSlot::Native(row) => {
                self.triggers[row][slot].fetch_add(1, Ordering::Relaxed);
            }
            MethodSlot::Extension(token) => {
                self.extensions.add(&[TRIGGER, &token, &code.to_string()], 1)
            }
        }
    }

    /// Every cached-response replay, whatever it repeated.
    fn triggered(&self) -> u64 {
        self.triggers.iter().flatten().map(|a| a.load(Ordering::Relaxed)).sum::<u64>()
            + self.extension_total(TRIGGER)
    }

    fn total(&self, ladder: Class) -> u64 {
        match REQUEST_LADDERS.iter().position(|c| *c == ladder) {
            Some(row) => {
                self.requests[row].iter().map(|a| a.load(Ordering::Relaxed)).sum::<u64>()
                    + self.extension_total(ladder.as_str())
            }
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
                    out.push(RetransmitRow {
                        ladder: ladder.as_str(),
                        method: method.to_string(),
                        code: None,
                        count,
                    });
                }
            }
        }
        for (slot, cell) in self.finals.iter().enumerate() {
            let count = cell.load(Ordering::Relaxed);
            if count > 0 {
                out.push(RetransmitRow {
                    ladder: Class::InviteServerFinal.as_str(),
                    method: "INVITE".to_string(),
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
                        method: method.to_string(),
                        code: Some(FIRST_TRIGGER_CODE + slot as u16),
                        count,
                    });
                }
            }
        }
        let ladders = REQUEST_LADDERS.map(Class::as_str);
        for (labels, count) in self.extensions.rows() {
            let Some(ladder) = ladders.into_iter().chain([TRIGGER]).find(|l| *l == labels[0])
            else {
                continue;
            };
            out.push(RetransmitRow {
                ladder,
                method: labels[1].clone(),
                code: labels.get(2).and_then(|code| code.parse().ok()),
                count,
            });
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
    /// Ordinary events the full output queue dropped, by
    /// [`EventQueueClass::index`].
    pub event_queue_drops: [AtomicU64; 6],
    /// Critical events the full output queue deferred onto the retry deque, by
    /// [`EventQueueClass::index`]. A deferral is a delivery postponed, never a
    /// loss.
    pub event_queue_deferrals: [AtomicU64; 6],
    /// Critical events waiting on the retry deque right now (gauge), sampled
    /// at the end of every owner turn.
    pub event_queue_deferred: AtomicUsize,
    /// Deferred requests removed because the server transaction that
    /// admitted them left the map unanswered — at its backstop, or reaped by
    /// the sweep (counter).
    pub deferred_swept: AtomicU64,
    /// Transactions the sweep found still resident more than one sweep
    /// interval past their lifetime deadline, and removed: each one had no
    /// cleanup timer left in the wheel (counter). Expected 0.
    pub sweep_reaped: AtomicU64,
    /// INVITEs refused at a deferred-backlog ceiling, indexed by
    /// [`InviteClass::index`] (counter).
    pub deferred_refused: [AtomicU64; 3],
    /// Copies of a refused INVITE answered that refusal again before any
    /// transaction held them (counter).
    pub refused_copies: AtomicU64,
    /// Client transactions still open when their call was released — orphaned
    /// (see `Transaction::orphaned`), never cut short (counter).
    pub txn_orphaned_on_call_evict: AtomicU64,
    /// Orphaned transactions resident right now, each closing its own
    /// obligations until its timer purges it (gauge).
    pub orphaned_transactions: AtomicUsize,
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
    /// Non-2xx INVITE finals DROPPED because no server transaction held their
    /// branch (RFC 3261 §17.2.1: only that transaction may author the final and
    /// run its Timer G ladder). A materialisation that answered an INVITE it
    /// never seeded shows here; a healthy node holds it at zero.
    pub server_final_unseen_branch: AtomicU64,
    /// Responses the TU handed over under a To-tag other than the one this
    /// layer had bound to the transaction or dialog, re-rendered under the
    /// bound tag before they left (RFC 3261 §9.2, §12.1.1). A climb is a TU
    /// defect the wire no longer shows.
    pub to_tag_coerced: AtomicU64,
    /// Responses that arrived with no usable To-tag — none, or the message
    /// generator's fallback — and left under the tag this layer had bound.
    pub to_tag_filled: AtomicU64,
    /// Responses that left carrying the generator's fallback To-tag because
    /// nothing here had a tag bound for them.
    pub fallback_to_tag_used: AtomicU64,
    /// Inbound datagrams the parser rejected and the layer dropped: a
    /// malformed-traffic flood or a parser regression.
    pub parse_errors: AtomicU64,
    /// Non-INVITE server transactions forgotten unanswered at the consumer's
    /// request ([`TransactionLayer::forget_unanswered`](crate::TransactionLayer::forget_unanswered)):
    /// one per request copy the consumer discarded before answering it.
    pub unanswered_forgotten: AtomicU64,
    /// Forget requests a full command queue refused; the transaction they
    /// named absorbs its retransmissions until its backstop.
    pub forget_refused: AtomicU64,
    /// Non-INVITE server transactions with no final forgotten at their call's
    /// release ([`TransactionLayer::forget_unanswered_of_call`](crate::TransactionLayer::forget_unanswered_of_call)).
    pub released_unanswered_forgotten: AtomicU64,
    /// In-dialog INVITE server transactions left without a final at their
    /// call's release, answered there ([`TransactionLayer::answer_unanswered_invites_of_call`](crate::TransactionLayer::answer_unanswered_invites_of_call)).
    pub released_unanswered_invites_answered: AtomicU64,
    /// Outbound sends the socket refused for a reason other than a full send
    /// buffer (counted by the endpoint as would-block): ENOBUFS, a filter's
    /// EPERM, an unreachable peer. Swallowed so a send error never aborts the
    /// owner.
    pub send_errors: AtomicU64,
}

impl MetricsInner {
    pub(crate) fn new() -> Self {
        Self {
            active_transactions: AtomicUsize::new(0),
            timer_queue_len: AtomicUsize::new(0),
            retransmit_buf_bytes: AtomicU64::new(0),
            event_queue_drops: Default::default(),
            event_queue_deferrals: Default::default(),
            event_queue_deferred: AtomicUsize::new(0),
            deferred_swept: AtomicU64::new(0),
            sweep_reaped: AtomicU64::new(0),
            deferred_refused: Default::default(),
            refused_copies: AtomicU64::new(0),
            txn_orphaned_on_call_evict: AtomicU64::new(0),
            orphaned_transactions: AtomicUsize::new(0),
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
            to_tag_coerced: AtomicU64::new(0),
            to_tag_filled: AtomicU64::new(0),
            fallback_to_tag_used: AtomicU64::new(0),
            parse_errors: AtomicU64::new(0),
            unanswered_forgotten: AtomicU64::new(0),
            forget_refused: AtomicU64::new(0),
            released_unanswered_forgotten: AtomicU64::new(0),
            released_unanswered_invites_answered: AtomicU64::new(0),
            send_errors: AtomicU64::new(0),
        }
    }

    /// Count `sent` when the socket refused it for a reason other than a full
    /// send buffer.
    pub(crate) fn count_send(&self, sent: Result<(), sip_net::SendError>) {
        if sent.is_err_and(|e| e.kind != sip_net::SendErrorKind::WouldBlock) {
            self.send_errors.fetch_add(1, Ordering::Relaxed);
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

    /// Static capacity of the bounded inbound→app event queue.
    pub fn event_queue_capacity(&self) -> usize {
        self.events_tx.max_capacity()
    }

    /// Current depth of the bounded event queue (gauge).
    pub fn event_queue_depth(&self) -> usize {
        self.events_tx.max_capacity() - self.events_tx.capacity()
    }

    /// Ordinary events the full output queue dropped, by class. Counted per
    /// wire copy — a dropped non-INVITE request's retransmission is admitted
    /// afresh and may be dropped again.
    pub fn event_queue_drops(&self, reason: EventQueueClass) -> u64 {
        self.inner.event_queue_drops[reason.index()].load(Ordering::Relaxed)
    }

    /// Critical events the full output queue deferred, by class: each is
    /// delivered once the queue has room, unless it is a request whose
    /// transaction leaves unanswered first
    /// ([`deferred_swept`](Self::deferred_swept)).
    pub fn event_queue_deferrals(&self, reason: EventQueueClass) -> u64 {
        self.inner.event_queue_deferrals[reason.index()].load(Ordering::Relaxed)
    }

    /// Critical events waiting for room in the output queue (gauge).
    pub fn event_queue_deferred(&self) -> usize {
        self.inner.event_queue_deferred.load(Ordering::Relaxed)
    }

    /// Deferred requests removed with the server transaction that admitted
    /// them, which left the map unanswered — at its backstop, or reaped by
    /// the sweep (counter).
    pub fn deferred_swept(&self) -> u64 {
        self.inner.deferred_swept.load(Ordering::Relaxed)
    }

    /// Transactions the safety-net sweep found still resident more than one
    /// sweep interval (`TXN_SWEEP_INTERVAL`) past their lifetime deadline, and
    /// removed (counter). The owner fires every due timer before the sweep,
    /// so one count is one transaction whose cleanup timer was missing.
    pub fn sweep_reaped(&self) -> u64 {
        self.inner.sweep_reaped.load(Ordering::Relaxed)
    }

    /// INVITEs refused at a deferred-backlog ceiling
    /// ([`DeferredBound`](crate::DeferredBound)), by class; a later copy of a
    /// refused INVITE, answered the same refusal, is not counted again
    /// (counter).
    pub fn deferred_refused(&self, class: InviteClass) -> u64 {
        self.inner.deferred_refused[class.index()].load(Ordering::Relaxed)
    }

    /// Copies of a refused INVITE — at this layer or a stage ahead of it —
    /// that reached this layer and drew that refusal again (counter).
    pub fn refused_copies(&self) -> u64 {
        self.inner.refused_copies.load(Ordering::Relaxed)
    }

    /// Client transactions orphaned — left to close their own obligations —
    /// because their owning call was released while they were open.
    pub fn txn_orphaned_on_call_evict(&self) -> u64 {
        self.inner.txn_orphaned_on_call_evict.load(Ordering::Relaxed)
    }

    /// Orphaned transactions still resident: what a released call has left in
    /// the layer, gone once each has been purged by its own timer.
    pub fn orphaned_transactions(&self) -> usize {
        self.inner.orphaned_transactions.load(Ordering::Relaxed)
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

    /// Retransmissions of extension methods that landed on the overflow row
    /// of [`retransmit_rows`](Self::retransmit_rows), past its cap of
    /// distinct rows.
    pub fn retransmit_rows_overflowed(&self) -> u64 {
        self.inner.retransmits.overflowed()
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

    /// Non-2xx INVITE finals dropped on an unseen branch (counter, expected 0).
    pub fn server_final_unseen_branch(&self) -> u64 {
        self.inner.server_final_unseen_branch.load(Ordering::Relaxed)
    }

    /// Counts one non-2xx INVITE final dropped on an unseen branch, as the
    /// layer does when it drops one: a harness seam proving that a post-call
    /// gate on [`server_final_unseen_branch`](Self::server_final_unseen_branch)
    /// fires.
    #[cfg(feature = "testkit")]
    pub fn count_server_final_unseen_branch(&self) {
        self.inner.server_final_unseen_branch.fetch_add(1, Ordering::Relaxed);
    }

    /// Responses re-rendered under the bound To-tag (counter).
    pub fn to_tag_coerced(&self) -> u64 {
        self.inner.to_tag_coerced.load(Ordering::Relaxed)
    }

    /// Responses whose missing or fallback To-tag was filled from the bound
    /// one (counter).
    pub fn to_tag_filled(&self) -> u64 {
        self.inner.to_tag_filled.load(Ordering::Relaxed)
    }

    /// Responses that left under the generator's fallback To-tag (counter).
    pub fn fallback_to_tag_used(&self) -> u64 {
        self.inner.fallback_to_tag_used.load(Ordering::Relaxed)
    }

    /// Inbound packets the parser rejected and dropped (counter).
    pub fn parse_errors(&self) -> u64 {
        self.inner.parse_errors.load(Ordering::Relaxed)
    }

    /// Non-INVITE server transactions forgotten unanswered for the consumer
    /// (counter).
    pub fn unanswered_forgotten(&self) -> u64 {
        self.inner.unanswered_forgotten.load(Ordering::Relaxed)
    }

    /// Forget requests the full command queue refused (counter, expected 0).
    pub fn forget_refused(&self) -> u64 {
        self.inner.forget_refused.load(Ordering::Relaxed)
    }

    /// Non-INVITE server transactions with no final forgotten at their call's
    /// release (counter).
    pub fn released_unanswered_forgotten(&self) -> u64 {
        self.inner.released_unanswered_forgotten.load(Ordering::Relaxed)
    }

    /// In-dialog INVITE server transactions left without a final at their
    /// call's release, answered there (counter).
    pub fn released_unanswered_invites_answered(&self) -> u64 {
        self.inner.released_unanswered_invites_answered.load(Ordering::Relaxed)
    }

    pub(crate) fn count_forget_refused(&self) {
        self.inner.forget_refused.fetch_add(1, Ordering::Relaxed);
    }

    /// Outbound sends the socket refused, a full send buffer aside (counter).
    pub fn send_errors(&self) -> u64 {
        self.inner.send_errors.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use metric_catalogue::{DEFAULT_CAP, OVERFLOW};

    use super::*;
    use crate::catalogue::RETRANSMITS;

    fn extension(token: &str) -> MethodSlot {
        method_slot(&Method::Other(token.to_owned()))
    }

    /// A send error is counted unless it is a full send buffer, which the
    /// endpoint counts as would-block.
    #[test]
    fn a_send_error_is_counted_unless_it_would_block() {
        let inner = MetricsInner::new();
        let refused = |kind| Err(sip_net::SendError { message: String::new(), kind });
        inner.count_send(Ok(()));
        inner.count_send(refused(sip_net::SendErrorKind::WouldBlock));
        assert_eq!(inner.send_errors.load(Ordering::Relaxed), 0);
        inner.count_send(refused(sip_net::SendErrorKind::Other));
        inner.count_send(refused(sip_net::SendErrorKind::Unreachable));
        assert_eq!(inner.send_errors.load(Ordering::Relaxed), 2);
    }

    /// An extension method's rung is counted under its own token, never under
    /// a shared bucket.
    #[test]
    fn an_extension_method_keeps_its_own_row() {
        let family = RetransmitFamily::new();
        family.record_request(Class::NonInviteClient, extension("FOO"));
        family.record_request(Class::NonInviteClient, extension("FOO"));
        family.record_trigger(&Method::Other("BAR".to_owned()), 200);
        family.record_request(Class::NonInviteClient, method_slot(&Method::Bye));
        let rows = family.rows();
        let row = |method: &str, code| {
            rows.iter().find(|r| r.method == method && r.code == code).map(|r| (r.ladder, r.count))
        };
        assert_eq!(row("FOO", None), Some(("non-invite-client", 2)));
        assert_eq!(row("BAR", Some(200)), Some(("trigger", 1)));
        assert_eq!(row("BYE", None), Some(("non-invite-client", 1)));
        assert_eq!(family.total(Class::NonInviteClient), 3);
        assert_eq!(family.triggered(), 1);
    }

    /// Past the cap an extension method's rung lands on the overflow row and
    /// is counted there.
    #[test]
    fn past_the_cap_an_extension_rung_lands_on_the_overflow_row() {
        let family = RetransmitFamily::new();
        assert_eq!(RETRANSMITS.cap().map(|c| c.max), Some(DEFAULT_CAP));
        for i in 0..DEFAULT_CAP + 3 {
            family.record_request(Class::NonInviteClient, extension(&format!("X{i}")));
        }
        let rows = family.rows();
        assert_eq!(rows.iter().filter(|r| r.method.starts_with('X')).count(), DEFAULT_CAP);
        let overflow = rows.iter().find(|r| r.method == OVERFLOW).expect("overflow row");
        assert_eq!((overflow.ladder, overflow.count), ("non-invite-client", 3));
        assert_eq!(family.overflowed(), 3);
    }
}
