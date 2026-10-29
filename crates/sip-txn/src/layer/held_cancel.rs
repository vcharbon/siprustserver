//! The answers to a CANCEL of an active INVITE server transaction (RFC 3261
//! §9.2) under a consumer that releases them: the transaction takes its 487
//! at once — Completed, the `Cancelled` event handed up — while the CANCEL's
//! 200 and the INVITE's 487 wait until the consumer releases them
//! ([`TransactionLayer::release_cancel`]) or until `cancel_bound_ms` past the
//! CANCEL, whichever comes first. A copy of the CANCEL or of the INVITE that
//! arrives meanwhile is absorbed with nothing sent: the held answers answer
//! it. On release the 487 starts its Timer G ladder and its Timer H hold.
//!
//! [`TransactionLayer::release_cancel`]: crate::TransactionLayer::release_cancel

use std::net::SocketAddr;

use bytes::Bytes;
use sip_clock::{deadline, delay_queue};
use sip_net::UdpEndpoint;
use tokio::time::Instant;

use call::TxnKind;

use super::key::{ServerTxnId, ServerTxnKey};
use super::owner::Owner;
use super::txn::{Timer, UnsentCancel};
use crate::timers::ms;

impl Owner {
    /// Whether the answers to a CANCEL are held rather than sent.
    pub(super) fn holds_cancel_answers(&self) -> bool {
        self.cancel_bound_ms > 0
    }

    /// Hold `cancel_ok` and `terminated` on the server INVITE transaction
    /// `key` names, the INVITE identified as `held_as`, and arm their bound.
    pub(super) fn hold_cancel_answers(
        &mut self,
        key: &ServerTxnKey,
        held_as: (String, String, u32),
        cancel_ok: Option<(Bytes, SocketAddr)>,
        terminated: Option<(Bytes, SocketAddr)>,
    ) {
        let Some(txn) = self.server_mut(key.id()) else { return };
        let opened = txn.created_at;
        txn.unsent_cancel =
            Some(Box::new(UnsentCancel { cancel_ok, terminated, held_as: held_as.clone() }));
        self.held_cancels.insert(held_as, key.clone());
        let at = deadline::after(Instant::now(), ms(self.cancel_bound_ms));
        delay_queue::insert_at(&mut self.timers, Timer::CancelBound(key.clone(), opened), at);
    }

    /// Whether the server transaction `id` names still holds the answers to
    /// a CANCEL.
    pub(super) fn holds_cancel(&self, id: ServerTxnId<'_>) -> bool {
        self.server(id).is_some_and(|t| t.unsent_cancel.is_some())
    }

    /// The bound of the transaction `key` names, opened at `opened`, passed:
    /// its held answers leave unless they already did.
    pub(super) async fn fire_cancel_bound(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        key: &ServerTxnKey,
        opened: Instant,
    ) {
        if self.server(key.id()).is_some_and(|t| t.created_at == opened)
            && self.send_held_cancel(endpoint, key).await
        {
            self.told_bound();
        }
    }

    /// Tell [`TransactionConfig::on_bound`](crate::TransactionConfig) that
    /// held messages left at their bound.
    pub(super) fn told_bound(&self) {
        if let Some(on_bound) = &self.on_bound {
            on_bound();
        }
    }

    /// Send the answers held for the INVITE `held_as` names; `true` when they
    /// left.
    pub(super) async fn release_held_cancel(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        held_as: &(String, String, u32),
    ) -> bool {
        let Some(key) = self.held_cancels.get(held_as).cloned() else { return false };
        self.send_held_cancel(endpoint, &key).await
    }

    /// Send the answers the transaction `key` names holds, then start the
    /// 487's Timer G and Timer H (RFC 3261 §17.2.1); `true` when they left.
    pub(super) async fn send_held_cancel(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        key: &ServerTxnKey,
    ) -> bool {
        let Some(held) = self.server_mut(key.id()).and_then(|t| t.unsent_cancel.take()) else {
            return false;
        };
        self.held_cancels.remove(&held.held_as);
        let UnsentCancel { cancel_ok, terminated, .. } = *held;
        if let Some((buf, dest)) = cancel_ok {
            self.send_buffer(endpoint, &buf, dest).await;
        }
        if let Some((buf, dest)) = terminated {
            self.send_buffer(endpoint, &buf, dest).await;
            self.arm_final_hold(key.id(), TxnKind::Invite, 487);
        }
        true
    }
}
