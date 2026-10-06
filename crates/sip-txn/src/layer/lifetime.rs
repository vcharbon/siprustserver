//! When a transaction leaves the map. Every resident transaction carries one
//! [`Lifetime`] and one end-of-life deadline, armed as its single
//! [`Timer::Cleanup`]; the functions here are the only writers of both. The
//! retransmit ladders, Timer B/F and the CANCEL grace produce events, not
//! removal, and live in `layer::client` / `layer::server`.

use std::time::Duration;

use tokio::time::Instant;

use crate::event::TxnKind;
use crate::timers::{
    ms, TIMER_B, TIMER_D, TIMER_H, TIMER_I, TIMER_J, TIMER_M, TXN_MAX_AGE, TXN_SWEEP_INTERVAL,
};

use super::events::SweptServer;
use super::owner::Owner;
use super::txn::{NewTransaction, Timer, Transaction, TxnId, TxnRef};

/// How far past its deadline a transaction must be for the safety-net sweep
/// to reap it: one sweep interval. The owner fires every due timer before it
/// runs the sweep, so a transaction that far past its deadline has no cleanup
/// timer left in the wheel.
const SWEEP_SLACK: Duration = ms(TXN_SWEEP_INTERVAL);

/// The hold a transaction that is done with its request waits out before it
/// leaves: the window in which a retransmission can still reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Hold {
    /// Client INVITE after a non-2xx final (RFC 3261 §17.1.1.2).
    TimerD,
    /// Server INVITE after its final: Timer H after a non-2xx (§17.2.1),
    /// RFC 6026 §7.1 Timer L after a 2xx — both 64·T1.
    TimerH,
    /// Server non-INVITE after its final (§17.2.2).
    TimerJ,
    /// Server INVITE whose non-2xx final is ACKed (§17.2.1).
    TimerI,
    /// Orphaned client INVITE whose 2xx the layer ACKed (RFC 6026 §7.2).
    TimerM,
    /// Client INVITE that gave up: 64·T1 for the final its CANCEL provokes
    /// (§9.1), from the CANCEL's first send, else from the give-up.
    GaveUp,
}

impl Hold {
    fn duration(self) -> Duration {
        ms(match self {
            Hold::TimerD => TIMER_D,
            Hold::TimerH => TIMER_H,
            Hold::TimerJ => TIMER_J,
            Hold::TimerI => TIMER_I,
            Hold::TimerM => TIMER_M,
            Hold::GaveUp => TIMER_B,
        })
    }
}

/// A transaction's end-of-life state. Written only by this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lifetime {
    /// Still working its request. `backstop` is fixed when the transaction
    /// enters the map: `invite_initial_timeout_ms` + `TXN_MAX_AGE` from then
    /// for an INVITE, `TXN_MAX_AGE` from then for a non-INVITE. A client
    /// transaction gives up on Timer B/F or the INVITE bound first, and a
    /// server transaction is answered or forgotten first, unless nothing ends
    /// it.
    Active { backstop: Instant },
    /// Waiting out `why`; leaves at `until`.
    Held { why: Hold, until: Instant },
}

impl Lifetime {
    /// The instant the transaction leaves the map.
    pub(super) fn deadline(self) -> Instant {
        match self {
            Lifetime::Active { backstop } => backstop,
            Lifetime::Held { until, .. } => until,
        }
    }

    /// A client INVITE that gave up and has taken no final since.
    pub(super) fn gave_up(self) -> bool {
        matches!(self, Lifetime::Held { why: Hold::GaveUp, .. })
    }

    /// This lifetime held for `why` until `until`. Entering a hold takes its
    /// own deadline, even when that ends sooner than the one it replaces
    /// (Timer I after Timer H, Timer D after a give-up); re-arming the hold
    /// already running keeps the later deadline. Every hold re-arms with a
    /// later instant, so that `max` is defensive, for a hold whose duration
    /// may shrink.
    fn held(self, why: Hold, until: Instant) -> Lifetime {
        let until = match self {
            Lifetime::Held { why: running, until: old } if running == why => until.max(old),
            _ => until,
        };
        Lifetime::Held { why, until }
    }
}

impl Owner {
    /// Put a fresh transaction in the map `Active`, arm its backstop and
    /// return it. An INVITE (either role) gets the configured INVITE bound
    /// plus the margin, a non-INVITE the margin alone, all measured from now —
    /// for a seed, from the seed, never from the first copy's send on another
    /// node. The backstop never precedes a client transaction's `Timeout`:
    /// it lies `TXN_MAX_AGE` past the INVITE bound, and `TXN_MAX_AGE` exceeds
    /// Timer F.
    pub(super) fn open_txn(&mut self, head: NewTransaction) -> &mut Transaction {
        let budget = match head.kind {
            TxnKind::Invite => self.invite_initial_timeout_ms + TXN_MAX_AGE,
            TxnKind::NonInvite => TXN_MAX_AGE,
        };
        let id = head.id.clone();
        let lifetime = Lifetime::Active { backstop: Instant::now() + ms(budget) };
        self.set_txn(Transaction::new(head, lifetime));
        self.arm_deadline(id.as_ref());
        self.txn_mut(id.as_ref()).expect("just inserted")
    }

    /// Hold the transaction `id` names for `why`, from now
    /// ([`Lifetime::held`]).
    pub(super) fn hold(&mut self, id: TxnRef<'_>, why: Hold) {
        let Some(txn) = self.txn_mut(id) else { return };
        txn.lifetime = txn.lifetime.held(why, Instant::now() + why.duration());
        self.arm_deadline(id);
    }

    /// The [`Timer::Cleanup`] of the transaction `id` names at its lifetime
    /// deadline, replacing the one it had.
    fn arm_deadline(&mut self, id: TxnRef<'_>) {
        let Some(txn) = self.txn_mut(id) else { return };
        let (old, deadline) = (txn.cleanup_key.take(), txn.lifetime.deadline());
        self.cancel_timer(old);
        let key = self.timers.insert_at(Timer::Cleanup(id.to_id()), deadline);
        if let Some(txn) = self.txn_mut(id) {
            txn.cleanup_key = Some(key);
        }
    }

    /// The transaction `id` names reached its deadline: it leaves the map. A
    /// server transaction still `Active` leaves unanswered and takes its
    /// deferred requests with it; a held one answered its request, which the
    /// consumer still receives.
    pub(super) fn expire(&mut self, id: TxnRef<'_>) {
        let unanswered = self
            .txn(id)
            .filter(|t| matches!(t.lifetime, Lifetime::Active { .. }))
            .and_then(|t| match &t.id {
                TxnId::Server(key) => Some(SweptServer {
                    key: key.clone(),
                    call_id: t.call_id.clone(),
                    from_tag: t.from_tag.clone(),
                }),
                TxnId::Client(_) => None,
            });
        self.delete_txn(id);
        if let Some(server) = unanswered {
            self.drop_deferred_requests_of(&[server]);
        }
    }

    /// The safety net: remove, through [`expire`](Self::expire), every
    /// transaction still resident more than one sweep interval past its
    /// deadline, each counted once in `sweep_reaped`. One count is one
    /// transaction whose cleanup timer was missing from the wheel.
    pub(super) fn reap_overdue(&mut self) {
        let now = Instant::now();
        let overdue: Vec<TxnId> = self
            .clients
            .values()
            .chain(self.servers.values())
            .filter(|t| t.lifetime.deadline() + SWEEP_SLACK <= now)
            .map(|t| t.id.clone())
            .collect();
        for id in &overdue {
            tracing::warn!(?id, "transaction past its lifetime deadline; reaped by the sweep");
            self.expire(id.as_ref());
            self.metrics.sweep_reaped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn re_arming_the_running_hold_keeps_the_later_deadline() {
        let now = Instant::now();
        let held = Lifetime::Held { why: Hold::GaveUp, until: now + ms(10_000) };
        assert_eq!(
            held.held(Hold::GaveUp, now + ms(5_000)),
            Lifetime::Held { why: Hold::GaveUp, until: now + ms(10_000) },
        );
        assert_eq!(
            held.held(Hold::GaveUp, now + ms(20_000)),
            Lifetime::Held { why: Hold::GaveUp, until: now + ms(20_000) },
        );
    }

    #[test]
    fn entering_another_hold_takes_its_own_deadline_even_when_sooner() {
        let now = Instant::now();
        let held = Lifetime::Held { why: Hold::TimerH, until: now + ms(30_000) };
        assert_eq!(
            held.held(Hold::TimerI, now + ms(5_000)),
            Lifetime::Held { why: Hold::TimerI, until: now + ms(5_000) },
        );
        let active = Lifetime::Active { backstop: now + ms(60_000) };
        assert_eq!(
            active.held(Hold::TimerD, now + ms(32_000)),
            Lifetime::Held { why: Hold::TimerD, until: now + ms(32_000) },
        );
    }
}
