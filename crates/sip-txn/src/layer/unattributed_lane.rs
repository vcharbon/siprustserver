//! Which of the owner's two outputs a request rides: the event queue, or the
//! unattributed lane ([`TransactionConfig::unattributed_lane`](crate::TransactionConfig))
//! when the layer runs one. The lane carries what can belong to no call this
//! element names, so a flood of it never takes the room, nor ages the queue,
//! that the requests of calls need. The owner is the lane's one sender: room
//! read before a request makes any state is room its event finds.

use std::sync::atomic::Ordering::Relaxed;

use sip_message::{Method, SipRequest};
use tokio::sync::mpsc;

use crate::event::TransactionEvent;

use super::key::ServerTxnId;
use super::owner::Owner;

/// The output an inbound request's event rides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lane {
    /// The event queue, every event's but the lanes'.
    Events,
    /// The unattributed lane.
    Unattributed,
}

impl Owner {
    /// The output of `req`, a request no transaction here holds: the
    /// unattributed lane when the layer runs one, `req`'s Request-URI states
    /// no `callRef` and `req` is no initial INVITE (no To-tag, a new call,
    /// judged by admission); else the event queue.
    pub(super) fn lane_of(&self, req: &SipRequest) -> Lane {
        let initial_invite = req.method() == Method::Invite && req.to().tag().is_none();
        let names_a_call = req.request_uri().param("callRef").is_some();
        if self.unattributed_tx.is_none() || initial_invite || names_a_call {
            Lane::Events
        } else {
            Lane::Unattributed
        }
    }

    /// The output of the `Cancelled` of the INVITE server transaction `id`:
    /// the unattributed lane while that transaction is filed attributed to no
    /// call and the layer runs the lane, so the event stays behind the INVITE
    /// it cancels, which rode the lane; else the event queue.
    pub(super) fn cancelled_lane(&self, id: ServerTxnId<'_>) -> Lane {
        let filed = self.server(id).is_some_and(|txn| self.files_unattributed(&txn.call_id, id));
        if self.unattributed_tx.is_some() && filed {
            Lane::Unattributed
        } else {
            Lane::Events
        }
    }

    /// Whether a request riding `lane` may go on: `false` when the
    /// unattributed lane is full, the request then dropped as a lost datagram
    /// is, before anything is made for it, and counted. The event queue
    /// always lets a request go on: its own discipline applies at the offer.
    pub(super) fn lane_has_room(&self, lane: Lane) -> bool {
        let full = lane == Lane::Unattributed
            && self.unattributed_tx.as_ref().is_some_and(|tx| tx.capacity() == 0);
        if full {
            self.metrics.unattributed_lane_drops.fetch_add(1, Relaxed);
        }
        !full
    }

    /// Offer a critical `event` to `lane`: [`emit_critical`](Self::emit_critical)
    /// on the event queue; on the unattributed lane, whose room the caller
    /// read with [`lane_has_room`](Self::lane_has_room) before it made any
    /// state, taken.
    pub(super) fn emit_critical_on(&mut self, lane: Lane, event: TransactionEvent) {
        match lane {
            Lane::Events => self.emit_critical(event),
            Lane::Unattributed => {
                self.emit_on(lane, event);
            }
        }
    }

    /// Offer an ordinary `event` to `lane`: [`emit`](Self::emit) on the
    /// event queue, drop-newest and counted on the unattributed lane. `true`
    /// when the lane took the event.
    pub(super) fn emit_on(&mut self, lane: Lane, event: TransactionEvent) -> bool {
        let Some(tx) = self.unattributed_tx.as_ref().filter(|_| lane == Lane::Unattributed) else {
            return self.emit(event);
        };
        match tx.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.unattributed_lane_drops.fetch_add(1, Relaxed);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}
