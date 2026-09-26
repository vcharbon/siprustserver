//! Output-queue discipline: lossy [`Owner::emit`] for events the protocol
//! redelivers if lost, vs lossless [`Owner::emit_critical`] (deferred on a full
//! queue, never dropped while its transaction lives) for one-shot signals whose
//! protocol-level redelivery the layer already consumed — plus the ADR-0014
//! `CallQuiesced` end-of-turn ordering (`flush_pending_quiesce`).

use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;

use sip_message::SipMessage;
use tokio::sync::mpsc;

use crate::event::{EventQueueClass, TransactionEvent};
use crate::timers::ms;

use super::owner::Owner;
use super::txn::Timer;

/// Retry cadence for deferred critical-event delivery. Short — the router drains
/// the events queue continuously, so capacity returns within a few polls; the
/// timer exists only so an otherwise-idle owner (no packets, no commands, no
/// txn timers left) still delivers the backlog.
const EVENT_RETRY_MS: u64 = 100;

impl Owner {
    /// Offer an ordinary event to the bounded output queue. Producers NEVER block
    /// — a full queue drops the newest and counts it (drop-newest), so backpressure
    /// never reaches the recv path. Correct for events the protocol will resend if
    /// lost (inbound non-INVITE requests, reliable provisionals (RFC 3262), 2xx
    /// that the UAS keeps retransmitting until ACKed) — provided no transaction
    /// absorbs that resend.
    /// `true` when the queue took the event.
    pub(super) fn emit(&mut self, event: TransactionEvent) -> bool {
        self.offer(event, false)
    }

    /// Offer a CRITICAL one-shot event — its only delivery, because the layer has
    /// already consumed its protocol-level redelivery (deleted the client txn,
    /// auto-ACKed a non-2xx final, answered a CANCEL, or 100-silenced an inbound
    /// INVITE). A full queue DEFERS it onto the retry deque instead of dropping it,
    /// so the consumer sees it once capacity returns — unless it is a request
    /// whose server transaction the sweep deletes first
    /// ([`drop_deferred_requests_of`](Self::drop_deferred_requests_of)).
    pub(super) fn emit_critical(&mut self, event: TransactionEvent) {
        self.offer(event, true);
    }

    /// `true` when the event was queued or deferred, `false` when it was dropped.
    fn offer(&mut self, event: TransactionEvent, critical: bool) -> bool {
        // Preserve FIFO: once a critical backlog exists, queue further criticals
        // behind it rather than letting a fresh one jump the deferred ones.
        if critical && !self.deferred_events.is_empty() {
            self.defer(event);
            return true;
        }
        let class = EventQueueClass::of(&event);
        match self.events_tx.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(ev)) => {
                if critical {
                    self.defer(ev);
                } else {
                    // Ordinary event: drop-newest, counted.
                    self.metrics.event_queue_drops[class.index()].fetch_add(1, Relaxed);
                }
                critical
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Consumer gone — the owner winds down via its other arms; do not
                // spin retrying into a closed channel.
                self.deferred_events.clear();
                self.event_retry_armed = false;
                false
            }
        }
    }

    /// Queue a critical event onto the retry deque, counted as a deferral.
    fn defer(&mut self, event: TransactionEvent) {
        let class = EventQueueClass::of(&event);
        self.metrics.event_queue_deferrals[class.index()].fetch_add(1, Relaxed);
        self.deferred_events.push_back(event);
        self.arm_event_retry();
    }

    fn arm_event_retry(&mut self) {
        if !self.event_retry_armed {
            self.timers.insert(Timer::EventRetry, ms(EVENT_RETRY_MS));
            self.event_retry_armed = true;
        }
    }

    /// Re-offer deferred critical events in FIFO order once queue capacity returns.
    /// A still-full queue re-arms the tick; a closed channel clears the backlog.
    pub(super) fn flush_deferred(&mut self) {
        while let Some(ev) = self.deferred_events.pop_front() {
            match self.events_tx.try_send(ev) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(ev)) => {
                    self.deferred_events.push_front(ev);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.deferred_events.clear();
                    self.event_retry_armed = false;
                    return;
                }
            }
        }
        if self.deferred_events.is_empty() {
            self.event_retry_armed = false;
        } else {
            self.arm_event_retry();
        }
    }

    /// Remove the deferred requests the swept server transactions admitted: a
    /// request whose transaction is gone would reach the consumer with no
    /// transaction left to carry its answer, long after its sender gave up.
    /// Deferred responses, `Timeout`s, `Cancelled`s and `CallQuiesced`s stay:
    /// each is the consumer's only notice of an outcome, whatever became of the
    /// transaction behind it.
    pub(super) fn drop_deferred_requests_of(&mut self, swept: &[SweptServer]) {
        if swept.is_empty() || self.deferred_events.is_empty() {
            return;
        }
        let by_branch: HashMap<&str, &SweptServer> =
            swept.iter().map(|s| (s.branch.as_str(), s)).collect();
        let before = self.deferred_events.len();
        self.deferred_events.retain(|ev| !admitted_by_any(ev, &by_branch));
        let removed = before - self.deferred_events.len();
        self.metrics.deferred_swept.fetch_add(removed as u64, Relaxed);
        // An armed retry stays in the wheel and finds nothing to flush.
    }

    /// Emit any CallQuiesced notices `delete_txn` deferred this turn — AFTER the
    /// turn's protocol events (ADR-0014 ordering). Re-checks under the lockstep
    /// index: a later effect this turn could have re-armed a txn for the call, in
    /// which case the still-armed watch re-fires on its eventual last delete.
    pub(super) fn flush_pending_quiesce(&mut self) {
        if self.pending_quiesce.is_empty() {
            return;
        }
        for cr in std::mem::take(&mut self.pending_quiesce) {
            if self.self_release_watch.contains(&cr) && !self.has_txns_for(&cr) {
                self.notify_quiesced(cr);
            }
        }
    }

    /// Deliver the one-shot `CallQuiesced` for a watched, txn-free call. It is the
    /// ONLY self-release trigger the router gets for a takeover copy, so it rides
    /// the lossless critical path (`emit_critical`): a full queue defers it onto
    /// the retry deque instead of dropping it (a drop-newest emit would strand
    /// the copy double-serving until its 1 h `GlobalDuration` backstop, exactly in
    /// the post-failover storm where takeover copies exist). The watch is cleared
    /// here because the event is now captured — the deque guarantees its delivery.
    pub(super) fn notify_quiesced(&mut self, call_ref: String) {
        self.self_release_watch.remove(&call_ref);
        self.emit_critical(TransactionEvent::CallQuiesced { call_ref });
    }
}

/// The identity of a server transaction the sweep deleted: the Via branch it
/// was keyed by (RFC 3261 §17.2.3) and the dialog identity it admitted.
pub(super) struct SweptServer {
    pub(super) branch: String,
    pub(super) call_id: String,
    pub(super) from_tag: String,
}

/// Whether `event` is the request one of the swept transactions, keyed by
/// branch, admitted.
fn admitted_by_any(event: &TransactionEvent, by_branch: &HashMap<&str, &SweptServer>) -> bool {
    let TransactionEvent::Message { message, .. } = event else { return false };
    let SipMessage::Request(req) = message.as_ref() else { return false };
    let Some(swept) = req.top_via().branch().and_then(|b| by_branch.get(b)) else { return false };
    req.call_id().as_str() == swept.call_id
        && req.from().tag().unwrap_or_default() == swept.from_tag
}
