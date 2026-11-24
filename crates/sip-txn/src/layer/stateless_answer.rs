//! A final sent as a stateless UAS sends it (RFC 3261 §8.2.7): the answer to
//! a request its consumer found no call for leaves raw, and the server
//! transaction the request opened leaves the layer with no trace, so such a
//! request holds state only while it waits for its consumer.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::SipResponse;
use sip_net::UdpEndpoint;

use super::key::ServerTxnId;
use super::owner::Owner;
use super::txn::{Transaction, TxnState};

impl Owner {
    /// Send `msg` as a stateless UAS (RFC 3261 §8.2.7) and return the
    /// datagram that left: the server transaction its request opened is
    /// forsaken ([`forsake_server`](Self::forsake_server)) and `msg`, held to
    /// the To-tag this layer binds ([`bind_to_tag`](Self::bind_to_tag)),
    /// leaves once, raw, to that transaction's destination (§18.2.2), else to
    /// `dest`. A provisional, and a transaction that already sent a response
    /// or holds a CANCEL's answers, go through
    /// [`do_send_response`](Self::do_send_response).
    pub(super) async fn do_send_stateless_response(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        msg: SipResponse,
        dest: SocketAddr,
    ) -> Bytes {
        let key = ServerTxnId::of_response(&msg).map(ServerTxnId::to_key);
        let holder = key.as_ref().and_then(|key| self.server(key.id()));
        let unsent = holder.is_none_or(has_sent_nothing);
        if msg.status() < 200 || !unsent {
            return self.do_send_response(endpoint, msg, dest).await;
        }
        let dest = holder.and_then(|txn| txn.destination).unwrap_or(dest);
        let msg = self.bind_to_tag(msg);
        if let Some(key) = &key {
            self.forsake_server(key.id());
        }
        let buf = msg.image().clone();
        self.send_buffer(endpoint, msg.call_id().as_str(), &buf, dest).await;
        buf
    }
}

/// Whether the server transaction `txn` has sent nothing and owes nothing:
/// `Trying`, no response cached, no CANCEL's answers held.
fn has_sent_nothing(txn: &Transaction) -> bool {
    txn.state == TxnState::Trying && txn.last_response.is_none() && txn.unsent_cancel.is_none()
}
