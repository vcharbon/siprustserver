//! The report that messages the layer held for its consumer's release left
//! at their bound instead (`layer::held_ack`, `layer::held_cancel`).

use super::owner::Owner;
use super::txn::TxnRef;

/// The transaction whose held messages left at their bound, by what names it:
/// the call it is attributed to, when a request of it carried one, and its
/// request's Call-ID and From-tag. Each is `None` when the transaction is no
/// longer in the map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundPassed<'a> {
    pub call_ref: Option<&'a str>,
    pub call_id: Option<&'a str>,
    pub from_tag: Option<&'a str>,
}

impl Owner {
    /// Tell [`TransactionConfig::on_bound`](crate::TransactionConfig) that
    /// the transaction `id` names sent held messages at their bound.
    pub(super) fn told_bound(&self, id: TxnRef<'_>) {
        let Some(on_bound) = &self.on_bound else { return };
        let txn = self.txn(id);
        on_bound(BoundPassed {
            call_ref: txn.and_then(|t| t.call_ref.as_deref()),
            call_id: txn.map(|t| t.call_id.as_str()),
            from_tag: txn.map(|t| t.from_tag.as_str()),
        });
    }
}
