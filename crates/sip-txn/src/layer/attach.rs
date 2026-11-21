//! A server transaction attributed to its call after it opened: the consumer
//! found the call of a request whose Request-URI named none (`layer::server`
//! attributes by the Request-URI `callRef` as the transaction opens), or a
//! seed of the call hands its final to a transaction a copy of its request
//! opened (`layer::seed`).

use std::sync::atomic::Ordering::Relaxed;

use super::key::{ServerTxnId, ServerTxnKey};
use super::owner::Owner;
use super::txn::{TxnId, TxnRef};

impl Owner {
    /// Attribute the server transaction `key` to `call_ref` while it has sent
    /// no final and is attributed to no call. One attributed to `call_ref`
    /// already is left as it is. One attributed to another call keeps that
    /// call and the refusal is counted: the consumer resolves only a request
    /// whose Request-URI named no call, so the two never differ.
    pub(super) fn attach_server_to_call(&mut self, key: &ServerTxnKey, call_ref: &str) {
        let Some(txn) = self.server(key.id()) else { return };
        match txn.call_ref.as_deref() {
            Some(attributed) if attributed != call_ref => {
                self.metrics.call_attach_refused.fetch_add(1, Relaxed);
            }
            None if txn.state.is_active() => self.attribute_server(key.id(), call_ref),
            Some(_) | None => {}
        }
    }

    /// Attribute the server transaction `id`, attributed to no call, to
    /// `call_ref`, in lockstep with the call's index entry and its Call-ID's
    /// record: from then on the call's self-release count, its release's
    /// forget and answer, and its Call-ID's budget trip reach it as one the
    /// Request-URI attributed, and it leaves the unattributed ones
    /// (`layer::unattributed`). One attributed already is left as it is.
    pub(super) fn attribute_server(&mut self, id: ServerTxnId<'_>, call_ref: &str) {
        let Some(txn) = self.server_mut(id) else { return };
        if txn.call_ref.is_some() {
            return;
        }
        txn.call_ref = Some(call_ref.to_string());
        let call_id = txn.call_id.clone();
        self.unattributed.left(&call_id, TxnRef::Server(id));
        self.budgets.attributed(&call_id, call_ref);
        self.track_call_ref(&Some(call_ref.to_string()), &TxnId::Server(id.to_key()));
    }
}
