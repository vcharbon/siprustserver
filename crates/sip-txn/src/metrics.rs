//! Observability surface. Backed by shared atomics so callers read it
//! synchronously off the actor thread (the owner task updates the atomics
//! before it replies to a command, so a read right after an `await` reflects
//! the mutation — see `layer`).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use sip_retransmit::Class;
use tokio::sync::mpsc;

use crate::event::{EventQueueClass, TransactionEvent};
use crate::invite_class::InviteClass;

mod by_class;
mod retransmits;
pub(crate) use by_class::ByClass;
pub use retransmits::RetransmitRow;
pub(crate) use retransmits::{method_slot, RetransmitFamily};

#[derive(Debug)]
pub(crate) struct MetricsInner {
    pub active_transactions: AtomicUsize,
    /// Live entries in the internal `DelayQueue` (retransmit / timeout / cleanup /
    /// event-retry timers). Should track ~a small multiple of `active_transactions`;
    /// a climb while txns are flat is a timer/slab leak (orphaned DelayQueue
    /// entries never removed or fired).
    pub timer_queue_len: AtomicUsize,
    /// Sum of per-txn `retransmit_buf` bytes (the serialized requests held for
    /// retransmission). Sampled in the sweep; a climb vs flat txns = buffers
    /// retained past completion.
    pub retransmit_buf_bytes: AtomicU64,
    /// Ordinary events the full output queue dropped, by
    /// [`EventQueueClass::index`].
    pub event_queue_drops: ByClass<{ EventQueueClass::ALL.len() }>,
    /// Critical events the full output queue deferred onto the retry deque, by
    /// [`EventQueueClass::index`]. A deferral is a delivery postponed, never a
    /// loss.
    pub event_queue_deferrals: ByClass<{ EventQueueClass::ALL.len() }>,
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
    pub deferred_refused: ByClass<{ InviteClass::ALL.len() }>,
    /// Copies of a refused INVITE answered that refusal again before any
    /// transaction held them (counter).
    pub refused_copies: AtomicU64,
    /// Client transactions still resident when their call was released —
    /// orphaned (see `Transaction::orphaned`), never cut short: the open ones
    /// and the INVITEs held Accepted, so every answered INVITE whose call
    /// ends within Timer M of its 2xx counts (counter).
    pub txn_orphaned_on_call_evict: AtomicU64,
    /// Orphaned transactions resident right now, each closing its own
    /// obligations until its timer purges it (gauge).
    pub orphaned_transactions: AtomicUsize,
    /// CANCELs held back because their INVITE client txn had no response yet
    /// (RFC 3261 §9.1 — the CANCEL waits for the first provisional).
    pub cancels_held: AtomicU64,
    /// Held CANCELs put on the wire when the first provisional arrived.
    pub held_cancels_flushed: AtomicU64,
    /// Held CANCELs cleared without EVER reaching the wire — the txn took a
    /// final first (cancellation moot, §9.2) or died with no provisional.
    pub held_cancels_dropped: AtomicU64,
    /// CANCELs suppressed at send because their INVITE client txn had already
    /// taken its final or is gone — §9.1/§9.2: a CANCEL has no effect on an
    /// answered request; sending it could put a pre-1xx CANCEL on the wire.
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
    /// Requests whose top-Via stamp (RFC 3261 §18.2.1, RFC 3581 §4) the
    /// render could not write: handed on as received, their responses still
    /// sent to the stamp's response target. A healthy peer set holds it at
    /// zero.
    pub via_stamps_unwritten: AtomicU64,
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
    /// Inbound datagrams the parser refused and the layer dropped unanswered
    /// (a response, an ACK, a request no response can echo, a limit): a
    /// malformed-traffic flood or a parser regression.
    pub parse_errors: AtomicU64,
    /// Refused requests answered with a stateless 400.
    pub parse_refusals_answered: AtomicU64,
    /// Refused requests a 400 could answer, dropped because the cap on those
    /// 400s was spent.
    pub parse_refusals_capped: AtomicU64,
    /// Inbound datagrams the parser panicked on and the layer dropped: each
    /// one is a parser defect.
    pub parse_panics: AtomicU64,
    /// Non-INVITE server transactions forgotten unanswered at the consumer's
    /// request ([`TransactionLayer::forget_unanswered`](crate::TransactionLayer::forget_unanswered)):
    /// one per request copy the consumer discarded before answering it.
    pub unanswered_forgotten: AtomicU64,
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
            event_queue_drops: ByClass::new(),
            event_queue_deferrals: ByClass::new(),
            event_queue_deferred: AtomicUsize::new(0),
            deferred_swept: AtomicU64::new(0),
            sweep_reaped: AtomicU64::new(0),
            deferred_refused: ByClass::new(),
            refused_copies: AtomicU64::new(0),
            txn_orphaned_on_call_evict: AtomicU64::new(0),
            orphaned_transactions: AtomicUsize::new(0),
            cancels_held: AtomicU64::new(0),
            held_cancels_flushed: AtomicU64::new(0),
            held_cancels_dropped: AtomicU64::new(0),
            cancels_suppressed_on_final: AtomicU64::new(0),
            retransmits: RetransmitFamily::new(),
            txn_seeded: AtomicU64::new(0),
            txn_seed_skipped: AtomicU64::new(0),
            server_final_unseen_branch: AtomicU64::new(0),
            via_stamps_unwritten: AtomicU64::new(0),
            to_tag_coerced: AtomicU64::new(0),
            to_tag_filled: AtomicU64::new(0),
            fallback_to_tag_used: AtomicU64::new(0),
            parse_errors: AtomicU64::new(0),
            parse_refusals_answered: AtomicU64::new(0),
            parse_refusals_capped: AtomicU64::new(0),
            parse_panics: AtomicU64::new(0),
            unanswered_forgotten: AtomicU64::new(0),
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
        self.inner.event_queue_drops.get(reason.index())
    }

    /// Critical events the full output queue deferred, by class: each is
    /// delivered once the queue has room, unless it is a request whose
    /// transaction leaves unanswered first
    /// ([`deferred_swept`](Self::deferred_swept)).
    pub fn event_queue_deferrals(&self, reason: EventQueueClass) -> u64 {
        self.inner.event_queue_deferrals.get(reason.index())
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
        self.inner.deferred_refused.get(class.index())
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

    /// Held CANCELs cleared without ever reaching the wire (a final came
    /// first, or the txn died with no provisional).
    pub fn held_cancels_dropped(&self) -> u64 {
        self.inner.held_cancels_dropped.load(Ordering::Relaxed)
    }

    /// CANCELs suppressed at send: the INVITE txn had taken its final or was gone.
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

    /// Requests handed on with a top-Via stamp the render could not write
    /// (counter, expected 0).
    pub fn via_stamps_unwritten(&self) -> u64 {
        self.inner.via_stamps_unwritten.load(Ordering::Relaxed)
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

    /// Inbound packets the parser refused and the layer dropped unanswered
    /// (counter).
    pub fn parse_errors(&self) -> u64 {
        self.inner.parse_errors.load(Ordering::Relaxed)
    }

    /// Refused requests answered with a stateless 400 (counter).
    pub fn parse_refusals_answered(&self) -> u64 {
        self.inner.parse_refusals_answered.load(Ordering::Relaxed)
    }

    /// Answerable refused requests dropped by the cap on 400s (counter).
    pub fn parse_refusals_capped(&self) -> u64 {
        self.inner.parse_refusals_capped.load(Ordering::Relaxed)
    }

    /// Inbound packets the parser panicked on and dropped (counter).
    pub fn parse_panics(&self) -> u64 {
        self.inner.parse_panics.load(Ordering::Relaxed)
    }

    /// Non-INVITE server transactions forgotten unanswered for the consumer
    /// (counter).
    pub fn unanswered_forgotten(&self) -> u64 {
        self.inner.unanswered_forgotten.load(Ordering::Relaxed)
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

    /// Outbound sends the socket refused, a full send buffer aside (counter).
    pub fn send_errors(&self) -> u64 {
        self.inner.send_errors.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests;
