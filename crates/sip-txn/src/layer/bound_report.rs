//! The report that messages the layer held for its consumer's release left
//! at their bound instead (`layer::held_ack`, `layer::held_cancel`).

use super::owner::Owner;

impl Owner {
    /// Tell [`TransactionConfig::on_bound`](crate::TransactionConfig) that
    /// held messages left at their bound.
    pub(super) fn told_bound(&self) {
        if let Some(on_bound) = &self.on_bound {
            on_bound();
        }
    }
}
