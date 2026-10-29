//! The report that messages the layer held for its consumer's release left
//! at their bound instead (`layer::held_ack`, `layer::held_cancel`).

use super::owner::Owner;
use super::txn::TxnRef;

/// The transaction whose held messages left at their bound, by what names it:
/// the call it is attributed to, when a request of it carried one, and its
/// request's Call-ID and From-tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundPassed<'a> {
    pub call_ref: Option<&'a str>,
    pub call_id: &'a str,
    pub from_tag: &'a str,
}

impl Owner {
    /// Tell [`TransactionConfig::on_bound`](crate::TransactionConfig) that
    /// the transaction `id` names sent held messages at their bound.
    pub(super) fn told_bound(&self, id: TxnRef<'_>) {
        let (Some(on_bound), Some(txn)) = (&self.on_bound, self.txn(id)) else { return };
        on_bound(BoundPassed {
            call_ref: txn.call_ref.as_deref(),
            call_id: &txn.call_id,
            from_tag: &txn.from_tag,
        });
    }
}
