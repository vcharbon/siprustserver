//! Where the `inject` nodes stand (§6.6): open once their `after` holds,
//! complete once the lane's injector performed them.

use pivot_schema::flow::Inject;

use super::{Cursor, ItemStatus};
use crate::program::ItemKind;

impl<'p> Cursor<'p> {
    /// The `inject` nodes whose `after` is satisfied and which the lane's
    /// injector has not answered yet — the node itself, so the item index
    /// never leaves the cursor.
    pub fn open_injects(&self) -> Vec<&'p Inject> {
        self.plan
            .program()
            .items
            .iter()
            .filter(|item| self.items[item.index] == ItemStatus::Pending)
            .filter(|item| !self.injected.contains(&item.index))
            .filter(|item| self.item_open(item.index))
            .filter_map(|item| item.inject.as_ref())
            .collect()
    }

    /// The lane's injector performed the `inject` node `node`: it completes,
    /// and what is ordered `after` it opens. `false` on anything but an
    /// `inject` node.
    pub fn complete_inject(&mut self, node: &str) -> bool {
        let Some(item) = self.plan.program().item_for(node) else { return false };
        if item.kind != ItemKind::Inject {
            return false;
        }
        self.injected.insert(item.index);
        self.refresh_items();
        true
    }
}
