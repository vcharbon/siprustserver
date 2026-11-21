//! A CANCEL on a client INVITE transaction (RFC 3261 §9.1, §17.1.2.2): the
//! held CANCEL put on the wire at the INVITE's first provisional, and the
//! Timer-E ladder of the CANCEL on the wire, a sub-state of the INVITE
//! transaction.

use sip_clock::delay_queue;
use sip_message::Method;
use sip_net::UdpEndpoint;
use sip_retransmit::{Class, Ladder, Schedule};

use crate::metrics::method_slot;

use super::lifetime::Hold;
use super::owner::Owner;
use super::txn::{CancelWire, Timer, TxnRef};

impl Owner {
    /// Put a held CANCEL on the wire — called when its INVITE client txn takes
    /// its first provisional (RFC 3261 §9.1 "wait for the arrival of a
    /// provisional response before sending") — and arm its Timer-E ladder. A
    /// send on an INVITE that gave up restarts its hold, and one on a live
    /// INVITE brings its give-up within 64·T1 ([`Owner::bound_after_cancel`]):
    /// §9.1's 64·T1 runs from the CANCEL.
    pub(super) async fn flush_cancel_on_provisional(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
    ) {
        let send = match self.clients.get_mut(branch) {
            Some(t) => match t.held_cancel.as_mut() {
                Some(h) if h.wire == CancelWire::Held => {
                    h.wire = CancelWire::Sent;
                    Some((h.buf.clone(), h.dest, t.lifetime.gave_up(), t.call_id.clone()))
                }
                _ => None,
            },
            None => None,
        };
        if let Some((buf, dest, on_given_up, call_id)) = send {
            if on_given_up {
                self.hold(TxnRef::Client(branch), Hold::GaveUp);
            }
            self.send_buffer(endpoint, &call_id, &buf, dest).await;
            self.metrics.held_cancels_flushed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.arm_cancel_retransmit(branch);
            self.bound_after_cancel(branch);
        }
    }

    /// Arms the Timer-E ladder for the on-wire CANCEL parked on `branch`,
    /// resetting the pacing to T1. No-op while a ladder is already running —
    /// a re-send never resets the pacing. Callers: the first-send sites
    /// (direct pass-through, first-provisional flush) plus the pass-through of
    /// a superseding CANCEL (a fresh TU datagram earns a fresh ladder even
    /// after a ceiling).
    pub(super) fn arm_cancel_retransmit(&mut self, branch: &str) {
        let Some(txn) = self.clients.get_mut(branch) else { return };
        if txn.cancel_retransmit_key.is_some() {
            return;
        }
        let Some(h) = txn.held_cancel.as_mut() else { return };
        let Some((ladder, first)) = Ladder::armed(Schedule::rfc(Class::CancelClient)) else {
            return;
        };
        h.ladder = Some(ladder);
        let timer = Timer::CancelRetransmit(branch.to_string());
        txn.cancel_retransmit_key = Some(delay_queue::insert(&mut self.timers, timer, first));
    }

    /// Timer-E tick for the on-wire CANCEL (RFC 3261 §17.1.2.2: T1, doubling,
    /// capped at T2): re-sends the parked datagram until a CANCEL response
    /// arrives, the INVITE txn resolves, or the 64·T1 ceiling. The ceiling
    /// gives up on the CANCEL ONLY — the INVITE client txn continues under its
    /// own bound and still owes a final. The pacing is the Trying ladder for
    /// the ladder's whole life: the host txn's `Proceeding` belongs to the
    /// INVITE, and the CANCEL sub-state never has a Proceeding of its own — any
    /// response with a CANCEL CSeq (1xx included) ends the ladder outright.
    pub(super) async fn fire_cancel_retransmit(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
    ) {
        let send = match self.clients.get_mut(branch) {
            Some(t) if t.state.is_active() => match t.held_cancel.as_mut() {
                Some(h) if h.wire != CancelWire::Held => {
                    let rearm = h.ladder.as_mut().and_then(Ladder::advance);
                    Some((h.buf.clone(), h.dest, rearm, t.call_id.clone()))
                }
                _ => None,
            },
            _ => None,
        };
        let Some((buf, dest, rearm, call_id)) = send else { return };
        self.send_buffer(endpoint, &call_id, &buf, dest).await;
        self.metrics.retransmits.record_request(Class::CancelClient, method_slot(&Method::Cancel));
        if let Some(interval) = rearm {
            let key = self.arm(Timer::CancelRetransmit(branch.to_string()), interval);
            if let Some(txn) = self.clients.get_mut(branch) {
                txn.cancel_retransmit_key = Some(key);
            }
        }
    }
}
