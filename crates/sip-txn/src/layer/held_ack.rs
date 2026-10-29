//! The hop-by-hop ACK of a client INVITE's first non-2xx final (RFC 3261
//! §17.1.1.2) under a consumer that releases it: the transaction is
//! `Completed` at once and hands the final up, while the ACK waits until the
//! consumer releases it ([`TransactionLayer::release_ack`]) or until
//! `ack_bound_ms` past the final, whichever comes first. A copy of the final
//! that arrives meanwhile is absorbed with nothing sent: the held ACK answers
//! it. Once the ACK left, each copy is ACKed again as it arrives.
//!
//! [`TransactionLayer::release_ack`]: crate::TransactionLayer::release_ack

use std::net::SocketAddr;

use bytes::Bytes;
use sip_clock::{deadline, delay_queue};
use sip_message::generators::generate_ack_for_non_2xx;
use sip_message::{SipRequest, SipResponse};
use sip_net::UdpEndpoint;
use tokio::time::Instant;

use super::owner::Owner;
use super::txn::{Timer, TxnState};
use crate::event::TransactionEvent;
use crate::timers::ms;

/// What a client INVITE transaction holds as its non-2xx final arrives: its
/// state, whether it is orphaned, its INVITE and where that went.
pub(super) type Rejected = (TxnState, bool, Option<Box<SipRequest>>, Option<SocketAddr>);

impl Owner {
    /// The non-2xx final `resp` from `src` on the client INVITE transaction at
    /// `branch`, ACKed hop by hop (RFC 3261 §17.1.1.2). A copy of it on a
    /// `Completed` transaction is re-ACKed unless the first ACK is still held,
    /// which answers it, and goes nowhere; an orphan's ACK leaves at once,
    /// with nobody to hand the final to; else the ACK is held or sent
    /// ([`ack_first_non_2xx`](Self::ack_first_non_2xx)), the transaction held
    /// Completed for Timer D so each copy is absorbed, not re-surfaced, and
    /// the final handed up once, critical: the ACK silences the UAS's resend,
    /// so this is the consumer's only delivery of it.
    pub(super) async fn take_non_2xx_final(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
        resp: SipResponse,
        src: SocketAddr,
        (state, orphaned, original_request, destination): Rejected,
    ) {
        let ack = match (original_request, destination) {
            (Some(orig), Some(dest)) => match generate_ack_for_non_2xx(&orig, &resp, &[]) {
                Ok(ack) => Some((ack.image().clone(), dest)),
                Err(e) => {
                    tracing::warn!(%branch, %e, "ACK not sent: no ACK echoes it");
                    None
                }
            },
            _ => None,
        };
        if state == TxnState::Completed {
            if let Some((ack, dest)) = ack.filter(|_| !self.holds_ack(branch)) {
                self.send_buffer(endpoint, &ack, dest).await;
            }
            return;
        }
        if orphaned {
            if let Some((ack, dest)) = ack {
                self.send_buffer(endpoint, &ack, dest).await;
            }
            self.complete_non_2xx(branch);
            return;
        }
        if let Some(ack) = ack {
            self.ack_first_non_2xx(endpoint, branch, ack).await;
        }
        self.complete_non_2xx(branch);
        self.emit_critical(TransactionEvent::response(resp, src, true));
    }

    /// Send `ack` toward `dest` now under a bound of zero, else hold it on the
    /// client INVITE transaction at `branch` and arm its bound.
    pub(super) async fn ack_first_non_2xx(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
        (ack, dest): (Bytes, SocketAddr),
    ) {
        let bound = self.ack_bound_ms;
        let Some(txn) = self.clients.get_mut(branch).filter(|_| bound > 0) else {
            self.send_buffer(endpoint, &ack, dest).await;
            return;
        };
        txn.unsent_ack = Some((ack, dest));
        let opened = txn.created_at;
        let at = deadline::after(Instant::now(), ms(bound));
        delay_queue::insert_at(&mut self.timers, Timer::AckBound(branch.to_string(), opened), at);
    }

    /// Whether the client transaction at `branch` still holds its ACK.
    pub(super) fn holds_ack(&self, branch: &str) -> bool {
        self.clients.get(branch).is_some_and(|t| t.unsent_ack.is_some())
    }

    /// The bound of the transaction at `branch` opened at `opened` passed:
    /// its ACK leaves unless it already did. A transaction opened since on
    /// the same branch keeps its own bound.
    pub(super) async fn fire_ack_bound(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
        opened: Instant,
    ) {
        if self.clients.get(branch).is_some_and(|t| t.created_at == opened)
            && self.send_held_ack(endpoint, branch).await
        {
            self.told_bound();
        }
    }

    /// Send the ACK the transaction at `branch` holds; `true` when it left.
    pub(super) async fn send_held_ack(&mut self, endpoint: &dyn UdpEndpoint, branch: &str) -> bool {
        let Some((ack, dest)) = self.clients.get_mut(branch).and_then(|t| t.unsent_ack.take())
        else {
            return false;
        };
        self.send_buffer(endpoint, &ack, dest).await;
        true
    }
}
