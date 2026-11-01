//! The replication flows this node serves its peers: whether one is open,
//! whether it reported applying everything logged for it, and the backlog
//! marks new calls are admitted under.

use b2bua::repl::BacklogMarks;
use repl_net::frame::Partition;

use super::ReplicatedB2buaSut;

impl ReplicatedB2buaSut {
    /// Whether `peer`'s flow on `partition` is connected to THIS node's
    /// replication server and has reported applying everything this node ever
    /// logged for it (ADR-0031 D2) — the per-flow predicate the drain's
    /// caught-up exit is built from. `false` while crashed or unreplicated.
    pub fn flow_caught_up(&self, peer: &str, partition: Partition) -> bool {
        self.core
            .as_ref()
            .and_then(|c| c.repl_store())
            .is_some_and(|s| s.changelog().flow_caught_up(peer, partition))
    }

    /// Whether a flow of `peer` on `partition` is open on THIS node's
    /// replication server, its bootstrap done or not. `false` while crashed.
    pub fn flow_open(&self, peer: &str, partition: Partition) -> bool {
        self.core
            .as_ref()
            .and_then(|c| c.repl_store())
            .is_some_and(|s| s.changelog().flow_open(peer, partition))
    }

    /// Set the backlog marks this incarnation admits new calls under
    /// (ADR-0055 §3). No-op while crashed.
    pub fn set_backlog_marks(&self, marks: BacklogMarks) {
        if let Some(store) = self.core.as_ref().and_then(|c| c.repl_store()) {
            store.changelog().set_backlog_marks(marks);
        }
    }
}
