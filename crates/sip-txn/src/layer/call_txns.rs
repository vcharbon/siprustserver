//! The live transactions of one call, as the owner's `txn_index` files them.

use std::collections::HashSet;

use super::key::ServerTxnKey;

/// The live transactions of one call, by role (`Owner::txn_index`).
/// `accepted` holds its INVITE client transactions held Accepted after a 2xx
/// (RFC 6026 §7.2): their obligation to the call is met, so they are off its
/// books, yet the call's release still orphans or relinquishes them.
#[derive(Default)]
pub(super) struct CallTxns {
    pub(super) client: HashSet<String>,
    pub(super) server: HashSet<ServerTxnKey>,
    pub(super) accepted: HashSet<String>,
}

impl CallTxns {
    /// How many transactions are on the call's books: Accepted client ones
    /// aside.
    pub(super) fn on_books(&self) -> usize {
        self.client.len() + self.server.len()
    }

    /// No transaction of the call is resident, Accepted ones included.
    pub(super) fn holds_none(&self) -> bool {
        self.client.is_empty() && self.server.is_empty() && self.accepted.is_empty()
    }

    /// Every client transaction of the call, Accepted ones included.
    pub(super) fn every_client(&self) -> impl Iterator<Item = &String> {
        self.client.iter().chain(&self.accepted)
    }
}
