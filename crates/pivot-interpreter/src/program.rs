//! The ordering structure a flow compiles to (`PCAP2TEST_PIVOT_V3.md` §6):
//! top-level ITEMS in document order, each touching the legs its messages ride.
//!
//! Two ordering rules and nothing else. **Same-leg order is list order**: an
//! item on a leg runs only once every earlier item on that leg has completed.
//! **Cross-leg and cross-call order is `after`**: an item runs only once every
//! node it names has completed. A block (`alt`, `unordered`) is ONE item on
//! every leg it touches, which is what keeps "list order" meaningful for a
//! document whose alternatives span several legs.

use std::collections::{BTreeMap, BTreeSet};

/// What a top-level flow node is, for ordering purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    /// One message at one actor's vantage.
    Message,
    /// Declared alternatives: exactly one branch runs.
    Alt,
    /// Messages that must all arrive, in any order.
    Unordered,
    /// An external event handed to the lane's injector.
    Inject,
}

/// One top-level flow node, positioned.
#[derive(Debug, Clone)]
pub struct Item {
    /// Index in document order.
    pub index: usize,
    /// The node's own id — what `after` and an outside reference name.
    pub id: String,
    pub kind: ItemKind,
    /// Every leg the item's messages ride. Empty for an `inject`.
    pub legs: BTreeSet<String>,
    /// Nodes that must have completed before this item opens.
    pub after: Vec<String>,
    /// Ids of the message steps the item contains, in document order. One for a
    /// `Message`, the union of the branches for an `Alt`, the group for an
    /// `Unordered`, none for an `Inject`.
    pub steps: Vec<String>,
    /// For an `Alt`: the branch names in order, and each branch's step ids.
    pub branches: Vec<Branch>,
    /// For an `Inject`: the action token, carried here so the run never walks
    /// back into the document by index. `Some` exactly on an `Inject`.
    pub action: Option<String>,
}

/// One alternative of an `alt` item.
#[derive(Debug, Clone)]
pub struct Branch {
    pub name: String,
    /// The branch's step ids, in document order.
    pub steps: Vec<String>,
}

/// Where a step sits: which item, which branch of it (an `alt` only), and its
/// index inside that list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepLoc {
    pub item: usize,
    pub branch: Option<usize>,
    pub within: usize,
}

impl StepLoc {
    /// Whether a reference from `self` to `other` crosses an `alt` branch
    /// boundary — refused, because a step inside a branch exists only on the run
    /// that chose it (§6.5).
    pub fn crosses_branch(&self, other: &StepLoc) -> bool {
        match (self.item == other.item, other.branch) {
            (_, None) => false,
            (true, Some(b)) => self.branch != Some(b),
            (false, Some(_)) => true,
        }
    }
}

/// The per-leg item sequences plus the document-order index of every item.
#[derive(Debug, Clone, Default)]
pub struct Program {
    pub items: Vec<Item>,
    /// Leg id → the indices of the items on that leg, in document order.
    pub by_leg: BTreeMap<String, Vec<usize>>,
    /// Node id (item id or step id) → the item that completes it.
    pub item_of_node: BTreeMap<String, usize>,
}

impl Program {
    /// The item a node id names, whether the id is a block's or a step's.
    pub fn item_for(&self, node_id: &str) -> Option<&Item> {
        self.item_of_node.get(node_id).map(|&i| &self.items[i])
    }
}
