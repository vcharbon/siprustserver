//! A server transaction attributed to its call after it opened: the consumer
//! found the call of a request whose Request-URI named none (`layer::server`
//! attributes by the Request-URI `callRef` as the transaction opens).

use std::sync::atomic::Ordering::Relaxed;

use super::key::ServerTxnKey;
use super::owner::Owner;
use super::txn::TxnId;

impl Owner {
    /// Attribute the server transaction `key` to `call_ref` while it has sent
    /// no final and is attributed to no call, in lockstep with the call's
    /// index entry and its Call-ID's record. One attributed to `call_ref`
    /// already is left as it is. One attributed to another call keeps that
    /// call and the refusal is counted: the consumer resolves only a request
    /// whose Request-URI named no call, so the two never differ. Returns
    /// whether `key` is attributed to `call_ref` now.
    pub(super) fn attach_server_to_call(&mut self, key: &ServerTxnKey, call_ref: &str) -> bool {
        let Some(txn) = self.server_mut(key.id()) else { return false };
        match txn.call_ref.as_deref() {
            Some(attributed) if attributed == call_ref => return true,
            Some(_) => {
                self.metrics.call_attach_refused.fetch_add(1, Relaxed);
                return false;
            }
            None if !txn.state.is_active() => return false,
            None => {}
        }
        txn.call_ref = Some(call_ref.to_string());
        let call_id = txn.call_id.clone();
        self.budgets.attributed(&call_id, call_ref);
        self.track_call_ref(&Some(call_ref.to_string()), &TxnId::Server(key.clone()));
        true
    }
}
