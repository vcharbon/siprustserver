//! Where the layer meets a peer's copy against its trigger's record (ADR-0063
//! §1, `layer::copies`): a request a server transaction holds, a CANCEL of an
//! INVITE that took its final, a final to a client INVITE, and a request no
//! transaction holds, met against the record its retired transaction left in
//! its Call-ID's entry. A server transaction's records retire there as it
//! leaves. Each copy absorbed past the schedule is counted by what it copied.

use sip_message::{Method, SipRequest};
use sip_retransmit::Class;
use tokio::time::Instant;

use crate::metrics::CopyKind;

use super::call_id_budget::Retired;
use super::copies::{request_class, Copies};
use super::key::ServerTxnId;
use super::owner::Owner;
use super::txn::Transaction;

impl Owner {
    /// The copy of its request the server transaction `id` names meets now:
    /// whether it is answered. A seed's first copy is its first arrival.
    pub(super) fn request_copy_owed(&mut self, id: ServerTxnId<'_>) -> bool {
        let now = Instant::now();
        let Some(txn) = self.server_mut(id) else { return false };
        let owed = match txn.copies.as_mut() {
            Some(copies) => copies.copy(now),
            None => {
                txn.copies = Some(Copies::first(request_class(&txn.method), now));
                true
            }
        };
        self.absorbed_unless(owed, CopyKind::Request)
    }

    /// Count the copy of its request the server transaction `id` absorbs
    /// whatever the schedule says: an Accepted INVITE's (RFC 6026 §7.1). It
    /// keeps the record 64·T1 longer.
    pub(super) fn absorb_request_copy(&mut self, id: ServerTxnId<'_>) {
        let now = Instant::now();
        if let Some(copies) = self.server_mut(id).and_then(|t| t.copies.as_mut()) {
            copies.copy(now);
        }
    }

    /// The copy of a CANCEL the INVITE server transaction `invite` names
    /// meets now, the INVITE having taken its final: whether its 200 is
    /// owed. The first CANCEL to reach the INVITE is its first arrival.
    pub(super) fn cancel_copy_owed(&mut self, invite: ServerTxnId<'_>) -> bool {
        let now = Instant::now();
        let Some(txn) = self.server_mut(invite) else { return false };
        let owed = match txn.cancel_copies.as_mut() {
            Some(copies) => copies.copy(now),
            None => {
                txn.cancel_copies = Some(Copies::first(Class::CancelClient, now));
                true
            }
        };
        self.absorbed_unless(owed, CopyKind::Cancel)
    }

    /// The first arrival of a CANCEL of the INVITE server transaction
    /// `invite`, now.
    pub(super) fn note_first_cancel(&mut self, invite: ServerTxnId<'_>) {
        if let Some(txn) = self.server_mut(invite) {
            txn.cancel_copies = Some(Copies::first(Class::CancelClient, Instant::now()));
        }
    }

    /// The final of `status` under `to_tag` the client INVITE transaction at
    /// `branch` meets now: whether its ACK is owed. The first arrival of
    /// each To-tag is.
    pub(super) fn final_copy_owed(
        &mut self,
        branch: &str,
        to_tag: Option<&str>,
        status: u16,
    ) -> bool {
        let (class, kind) = if (200..300).contains(&status) {
            (Class::Final2xx, CopyKind::Final2xx)
        } else {
            (Class::InviteServerFinal, CopyKind::Non2xx)
        };
        let Some(txn) = self.clients.get_mut(branch) else { return false };
        let owed = txn.final_copies.arrival(to_tag, class, Instant::now());
        self.absorbed_unless(owed, kind)
    }

    /// Meet `req`, which no server transaction holds, against the record
    /// its retired transaction left: a copy past the schedule is absorbed
    /// and counted.
    pub(super) fn retired_copy(&mut self, req: &SipRequest) -> Retired {
        let Some(id) = ServerTxnId::of_request(req) else { return Retired::Unknown };
        let retired = self.budgets.copy_retired(req.call_id().as_str(), id, Instant::now());
        if matches!(retired, Retired::Absorbed) {
            let kind = match req.method() {
                Method::Cancel => CopyKind::Cancel,
                _ => CopyKind::Request,
            };
            self.metrics.copies_past_schedule.add(kind.index());
        }
        retired
    }

    /// Retire the copy records of `txn`, a server transaction leaving the
    /// layer, to its Call-ID's entry: its request's, and a CANCEL's of it.
    pub(super) fn retire_copies(&mut self, txn: &mut Transaction) {
        let now = Instant::now();
        let request = txn.copies.take().filter(|c| !c.lapsed(now));
        let cancel = txn.cancel_copies.take().filter(|c| !c.lapsed(now));
        if request.is_none() && cancel.is_none() {
            return;
        }
        let Some(key) = txn.server_key() else { return };
        if let Some(copies) = cancel {
            self.budgets.retire(&txn.call_id, key.under(&Method::Cancel), copies, now);
        }
        if let Some(copies) = request {
            self.budgets.retire(&txn.call_id, key, copies, now);
        }
    }

    /// `owed`, a copy of `kind` absorbed past its schedule counted when not.
    fn absorbed_unless(&self, owed: bool, kind: CopyKind) -> bool {
        if !owed {
            self.metrics.copies_past_schedule.add(kind.index());
        }
        owed
    }
}

/// The record a request of `method` opening a server transaction now starts
/// it with: the one `retired` carries on, else its first arrival.
pub(super) fn opening_copies(method: &Method, retired: Retired) -> Copies {
    match retired {
        Retired::Reopens(copies) => copies,
        Retired::Unknown | Retired::Absorbed => {
            Copies::first(request_class(method), Instant::now())
        }
    }
}
