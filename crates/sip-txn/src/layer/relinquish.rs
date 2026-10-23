//! Relinquishing a call's transactions: every transaction attributed to a call
//! is deleted with nothing sent, because another node serves the call now —
//! the inverse of seeding (`seed`). A message for one of them reaching this
//! layer afterwards is a new arrival, handed to the consumer to route afresh.

use tokio::sync::oneshot;

use super::key::ServerTxnKey;
use super::owner::Owner;
use super::txn::TxnRef;

impl Owner {
    /// Delete every client and server transaction attributed to `call_ref`,
    /// Accepted client ones included, their timers with them, sending nothing
    /// and owing nothing: no ACK, no CANCEL, no final, no retransmission. The
    /// call's self-release watch goes too, so no `CallQuiesced` follows.
    /// Replies with how many went.
    pub(super) fn relinquish_call(&mut self, call_ref: &str, reply: oneshot::Sender<usize>) {
        self.self_release_watch.remove(call_ref);
        let txns = self.txn_index.get(call_ref);
        let clients: Vec<String> =
            txns.into_iter().flat_map(|t| t.every_client().cloned()).collect();
        let servers: Vec<ServerTxnKey> =
            txns.into_iter().flat_map(|t| t.server.iter().cloned()).collect();
        let clients = clients.iter().filter(|b| self.delete_txn(TxnRef::Client(b))).count();
        let servers = servers.iter().filter(|k| self.delete_txn(TxnRef::Server(k.id()))).count();
        let _ = reply.send(clients + servers);
    }

    /// Delete the Accepted client transactions of `call_ref`, sending
    /// nothing: a release that does not end the call leaves their dialogs to
    /// the node that still serves it, so none may draw the layer's ACK or BYE.
    /// Returns how many went.
    pub(super) fn relinquish_accepted_of_call(&mut self, call_ref: &str) -> usize {
        let accepted: Vec<String> = self
            .txn_index
            .get(call_ref)
            .into_iter()
            .flat_map(|t| t.accepted.iter().cloned())
            .collect();
        accepted.iter().filter(|b| self.delete_txn(TxnRef::Client(b))).count()
    }
}

impl Owner {
    /// Delete the one server transaction `key`, its timers with it, sending
    /// nothing and owing nothing. Replies whether it was there.
    pub(super) fn relinquish_server(&mut self, key: &ServerTxnKey, reply: oneshot::Sender<bool>) {
        let _ = reply.send(self.delete_txn(TxnRef::Server(key.id())));
    }
}
