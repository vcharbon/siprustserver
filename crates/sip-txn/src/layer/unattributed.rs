//! The in-dialog server transactions opened attributed to no call: their
//! request's Request-URI named none (an element on its path rewrote it). They
//! leave the set as they are attributed, by the consumer once it takes the
//! request (`layer::attach`) or by a seed of the call whose final one takes
//! (`layer::seed`), or as they leave the map. Until then the self-release
//! count of a call whose legs' Call-IDs name theirs counts them, and its watch
//! holds on them (ADR-0014, `layer::self_release`).

use std::collections::{HashMap, HashSet};

use super::key::{ServerTxnId, ServerTxnIdentity, ServerTxnKey};
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

    /// The transaction `id` of `call_id` was attributed or left the map;
    /// whether it was filed. A client transaction and one never filed change
    /// nothing.
    pub(super) fn left(&mut self, call_id: &str, id: TxnRef<'_>) -> bool {
        let TxnRef::Server(id) = id else { return false };
        let Some(keys) = self.0.get_mut(call_id) else { return false };
        let filed = keys.remove(&id as &dyn ServerTxnIdentity);
        if keys.is_empty() {
            self.0.remove(call_id);
        }
        filed
    }

    /// Whether the server transaction `id` of `call_id` is filed.
    fn holds(&self, call_id: &str, id: ServerTxnId<'_>) -> bool {
        self.0.get(call_id).is_some_and(|keys| keys.contains(&id as &dyn ServerTxnIdentity))
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
        let named: HashSet<&str> = call_ids.iter().map(String::as_str).collect();
        named
            .into_iter()
            .filter_map(|call_id| self.unattributed.0.get(call_id))
            .flatten()
            .filter(|key| !self.server(key.id()).is_some_and(|txn| !txn.state.is_active()))
            .count()
    }

    /// The server transaction `id` sent its final: one unattributed no longer
    /// serves its call, and the watches naming its Call-ID are checked again.
    pub(super) fn unattributed_answered(&mut self, id: ServerTxnId<'_>) {
        let Some(call_id) = self.server(id).map(|txn| txn.call_id.clone()) else { return };
        if self.unattributed.holds(&call_id, id) {
            self.unattributed_settled(&call_id);
        }
    }
}
