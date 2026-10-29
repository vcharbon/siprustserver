//! A client transaction's give-up (RFC 3261 §17.1): the deadline it is armed
//! with (Timer B/F, or the INVITE bound from a first provisional), the TU's
//! first-response deadline on an initial INVITE (ADR-0044), and the `Timeout`
//! the TU is told, naming which request drew no final.

use call::{TxnKind, TxnTimeoutKind};
use sip_message::SipRequest;

use crate::event::TransactionEvent;
use crate::timers::{ms, TIMER_B, TIMER_F};

use super::lifetime::Hold;
use super::owner::Owner;
use super::txn::{Timer, TxnRef, TxnState};

impl Owner {
    pub(super) fn fire_timeout(&mut self, branch: &str) {
        let Some(event) = self.timeout_event(branch) else { return };
        let reported = self.clients.get(branch).is_some_and(|t| t.dead_hop_reported);
        self.give_up(branch);
        if !reported {
            // Critical: the give-up fires once, so nothing re-fires — a dropped
            // Timeout would strand the leg until the 1 h GlobalDuration.
            self.emit_critical(event);
        }
    }

    /// The TU's first-response deadline on an initial INVITE (ADR-0044): a
    /// branch still unanswered is reported dead to the TU, once, and the
    /// transaction keeps Timer A to Timer B, where it gives up unreported. A
    /// branch a `100 Trying` alone answered is reported alive
    /// ([`TransactionEvent::HopAlive`]).
    pub(super) fn fire_first_response(&mut self, branch: &str) {
        let Some(t) = self
            .clients
            .get(branch)
            .filter(|t| t.kind == TxnKind::Invite && t.state.is_active() && !t.dead_hop_reported)
        else {
            return;
        };
        if t.state == TxnState::Proceeding {
            if let (false, Some(call_ref), Some(leg_id)) =
                (t.past_trying, t.call_ref.clone(), t.leg_id.clone())
            {
                // Ordinary: a lost notice leaves the TU on its own deadlines.
                let incarnation_mark = t.incarnation_mark.clone();
                self.emit(TransactionEvent::HopAlive {
                    branch: branch.to_string(),
                    call_ref,
                    leg_id,
                    incarnation_mark,
                });
            }
            return;
        }
        let Some(event) = self.timeout_event(branch) else { return };
        if let Some(t) = self.clients.get_mut(branch) {
            t.dead_hop_reported = true;
        }
        // Critical: the deadline fires once, like the give-up it reports.
        self.emit_critical(event);
    }

    /// The `Timeout` the active client txn at `branch` reports, `None` once it
    /// took a final or gave up.
    fn timeout_event(&self, branch: &str) -> Option<TransactionEvent> {
        let t =
            self.clients.get(branch).filter(|t| t.state.is_active() && !t.lifetime.gave_up())?;
        Some(TransactionEvent::Timeout {
            branch: branch.to_string(),
            call_ref: t.call_ref.clone(),
            leg_id: t.leg_id.clone(),
            incarnation_mark: t.incarnation_mark.clone(),
            // Every transaction names its method (RFC 3261 §17.1: the TU is
            // told which request drew no answer) — a BYE's Timer F is not an
            // OPTIONS's.
            method: Some(t.method.to_string()),
            cseq: t.cseq,
            destination: t.destination,
            kind: t.timeout_kind,
        })
    }

    /// End the client txn at `branch` on its give-up. A non-INVITE is deleted.
    /// An INVITE stops retransmitting and is held [`Hold::GaveUp`], 64·T1. In
    /// Proceeding that is RFC 3261 §9.1's wait: the TU abandons it with a
    /// CANCEL, which still matches it (and restarts the hold), and the final
    /// that CANCEL provokes is ACKed (§17.1.1.3) and handed up, Timer D
    /// replacing the hold. In Calling it is a robustness hold:
    /// its CANCEL stays held unless a late provisional arrives, and a late
    /// final is ACKed.
    fn give_up(&mut self, branch: &str) {
        let retransmit = match self.clients.get_mut(branch) {
            Some(t) if t.kind == TxnKind::Invite => {
                t.ladder = None;
                t.retransmit_key.take()
            }
            _ => {
                self.delete_txn(TxnRef::Client(branch));
                return;
            }
        };
        self.cancel_timer(retransmit);
        self.hold(TxnRef::Client(branch), Hold::GaveUp);
    }

    /// RFC 3261 §17.1 client-transaction timeout plus the [`TxnTimeoutKind`] its
    /// expiry emits. EVERY client txn is armed with the §17.1 failure-detection
    /// timer — §17.1.1.2 scopes Timer B to Calling, so an INVITE drawing no
    /// response of any kind is an unanswered hop, not a responding peer, and
    /// gives up at 64·T1; the TU's first-response deadline rides beside it
    /// ([`Owner::fire_first_response`], ADR-0044). The window `invite_initial_timeout_ms` owns opens at an INVITE's first
    /// provisional, where [`Owner::rearm_invite_bound`] arms it — initial and
    /// in-dialog alike, since a peer that answered 1xx has left Calling; a
    /// non-INVITE never leaves Timer F.
    pub(super) fn client_timeout(&self, kind: TxnKind, req: &SipRequest) -> (u64, TxnTimeoutKind) {
        match kind {
            // `min`: whichever bound is configured tighter is the deadline
            // and owns the give-up.
            TxnKind::Invite if req.to().tag().is_none() => {
                (TIMER_B.min(self.invite_initial_timeout_ms), TxnTimeoutKind::Response)
            }
            TxnKind::Invite => (TIMER_B, TxnTimeoutKind::Response),
            TxnKind::NonInvite => (TIMER_F, TxnTimeoutKind::Response),
        }
    }

    /// Re-arm an INVITE client txn's give-up from Timer B to the configured
    /// INVITE bound at its FIRST provisional: the txn has left Calling, the
    /// only state Timer B governs (RFC 3261 §17.1.1.2), and a peer that has
    /// answered is not a dead hop — whether the INVITE opens a dialog or
    /// renegotiates one. The bound is the pre-final backstop of every INVITE
    /// in Proceeding, so a renegotiation whose peer rings or goes silent after
    /// its provisional ends on it, not on Timer B.
    ///
    /// The bound is measured from the ORIGINAL SEND, so its configured ordering
    /// against the app's setup/no-answer deadline holds however late the
    /// provisional arrives. No-op for a non-INVITE, a txn already re-armed —
    /// a second provisional never extends the bound — a txn that gave up, and
    /// one whose hop was already reported dead: it keeps Timer B.
    pub(super) fn rearm_invite_bound(&mut self, branch: &str) {
        let bound = self.invite_initial_timeout_ms;
        let remaining = match self.clients.get(branch) {
            Some(t)
                if t.kind == TxnKind::Invite
                    && t.timeout_kind == TxnTimeoutKind::Response
                    && !t.lifetime.gave_up()
                    && !t.dead_hop_reported =>
            {
                let elapsed = u64::try_from(t.created_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                bound.saturating_sub(elapsed).max(1)
            }
            _ => return,
        };
        let old = self.clients.get_mut(branch).and_then(|t| t.timeout_key.take());
        self.cancel_timer(old);
        let key = self.arm(Timer::ClientTimeout(branch.to_string()), ms(remaining));
        if let Some(txn) = self.clients.get_mut(branch) {
            txn.timeout_key = Some(key);
            txn.timeout_kind = TxnTimeoutKind::Transaction;
        }
    }
}
