//! The transaction layer's relinquishing commands: a call's transactions, or
//! one of them, handed to the node that serves the call or took a request's
//! final (`layer::relinquish`).

use super::super::key::ServerTxnKey;
use super::{Command, TransactionLayer, TransactionLayerClosed};

impl TransactionLayer {
    /// Relinquish every transaction of `call_ref`, client and server, to the
    /// node that serves the call now: each is deleted with its timers, nothing
    /// sent but the held ACK of a non-2xx final one took and the held 200 to a
    /// CANCEL one answered (the 487 to its INVITE is the serving node's),
    /// nothing else owed, and no `CallQuiesced` follows. A message for one
    /// of them arriving afterwards matches nothing here and is handed up as a
    /// new arrival. Returns how many went. Idempotent.
    pub async fn relinquish_call(&self, call_ref: &str) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::RelinquishCall { call_ref: call_ref.to_string(), reply })
            .await
    }

    /// Delete the Accepted INVITE client transactions of `call_ref` with
    /// nothing sent, ahead of a release that does not end the call: no later
    /// 2xx draws this layer's ACK or BYE on a dialog the call keeps, and a
    /// repeat is a new arrival. Returns how many went. Idempotent.
    pub async fn relinquish_accepted_of_call(
        &self,
        call_ref: &str,
    ) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::RelinquishAccepted {
            call_ref: call_ref.to_string(),
            reply,
        })
        .await
    }

    /// Relinquish the one server transaction `key` to the node that answered
    /// its request: it is deleted with its timers, nothing sent but the held
    /// 200 to a CANCEL it answered, nothing else owed. A retransmission of its request arriving afterwards matches
    /// nothing here and is handed up as a new arrival. Returns whether it was
    /// there. Idempotent.
    pub async fn relinquish_server(
        &self,
        key: ServerTxnKey,
    ) -> Result<bool, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::RelinquishServer { key, reply }).await
    }

    /// Relinquish the one client transaction on `branch` to the node that took
    /// its request's final: it is deleted with its timers, nothing sent but
    /// the held ACK of a non-2xx final it took. A response on that branch
    /// arriving afterwards matches nothing here and is handed up as a new
    /// arrival. Returns whether it was there. Idempotent.
    pub async fn relinquish_client(&self, branch: &str) -> Result<bool, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::RelinquishClient { branch: branch.to_string(), reply })
            .await
    }
}
