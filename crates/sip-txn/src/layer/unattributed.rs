//! The in-dialog server transactions opened attributed to no call: their
//! request's Request-URI named none (an element on its path rewrote it), so
//! only the consumer attributes them, once it takes the request
//! (`layer::attach`). Until then the self-release count of a call whose
//! legs' Call-IDs name theirs counts them (ADR-0014).

use std::collections::{HashMap, HashSet};

use super::key::{ServerTxnIdentity, ServerTxnKey};
use super::owner::Owner;
use super::shrink::shrink_idle;
use super::txn::TxnRef;

/// The in-dialog server transactions attributed to no call, by Call-ID. Each
/// is filed as it opens and leaves as it is attributed or leaves the map, so
/// the set never holds more than the map's server transactions.
#[derive(Default)]
pub(super) struct Unattributed(HashMap<String, HashSet<ServerTxnKey>>);

impl Unattributed {
    /// File the server transaction `key` of `call_id`, just opened.
    pub(super) fn opened(&mut self, call_id: &str, key: ServerTxnKey) {
        self.0.entry(call_id.to_string()).or_default().insert(key);
    }

    /// The transaction `id` of `call_id` was attributed or left the map; a
    /// client transaction and one never filed change nothing.
    pub(super) fn left(&mut self, call_id: &str, id: TxnRef<'_>) {
        let TxnRef::Server(id) = id else { return };
        let Some(keys) = self.0.get_mut(call_id) else { return };
        keys.remove(&id as &dyn ServerTxnIdentity);
        if keys.is_empty() {
            self.0.remove(call_id);
        }
    }

    /// Give back the table's idle capacity.
    pub(super) fn shrink_idle(&mut self) {
        shrink_idle(&mut self.0);
    }
}

impl Owner {
    /// How many unattributed in-dialog server transactions of the Call-IDs
    /// `call_ids` names still serve their call: each is counted until it has
    /// sent its final, once however often its Call-ID is named.
    pub(super) fn unattributed_serving(&self, call_ids: &[String]) -> usize {
        let named_first = |(at, call_id): &(usize, &String)| !call_ids[..*at].contains(*call_id);
        call_ids
            .iter()
            .enumerate()
            .filter(named_first)
            .filter_map(|(_, call_id)| self.unattributed.0.get(call_id))
            .flatten()
            .filter(|key| !self.server(key.id()).is_some_and(|txn| !txn.state.is_active()))
            .count()
    }
}
