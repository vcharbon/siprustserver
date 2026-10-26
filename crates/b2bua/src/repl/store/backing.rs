//! Which calls have a backup (ADR-0055 §3, ADR-0037 item 4): the replicas
//! this node refused as a backup at its ceiling, and this node's own calls
//! that have no backup for life.

use repl_net::frame::Partition;

use super::ReplicatingCallStore;
use crate::store::{locked, PartitionRole, REPLICA_METADATA, REPLICA_REFUSALS};

impl ReplicatingCallStore {
    /// Refuse the replica of call `incarnation` on `call_ref` of `primary` at
    /// this backup's ceiling (ADR-0037 item 4): its writes are refused until
    /// the refusal lapses, once the writes in flight have arrived.
    pub fn refuse_replica(&self, call_ref: &str, primary: &str, incarnation: Option<&str>) {
        let now = self.clock.now_ms();
        locked(&self.refusals, REPLICA_REFUSALS).replicas.refuse(
            call_ref,
            incarnation,
            primary,
            now,
        );
    }

    /// Whether this backup refused the replica of call `incarnation` on
    /// `call_ref`.
    pub fn refused_replica(&self, call_ref: &str, incarnation: Option<&str>) -> bool {
        locked(&self.refusals, REPLICA_REFUSALS).replicas.refuses(call_ref, incarnation)
    }

    /// Drop every refusal of `primary`'s replicas: a new scan of it states
    /// every call it backs up here.
    pub fn clear_refusals_of(&self, primary: &str) {
        locked(&self.refusals, REPLICA_REFUSALS).replicas.clear_primary(primary);
    }

    /// Standing replica refusals, all primaries.
    #[cfg(test)]
    pub fn refused_count(&self) -> usize {
        locked(&self.refusals, REPLICA_REFUSALS).replicas.len()
    }

    /// Admit the new call `call_ref` with no backup, for its whole life.
    pub fn leave_unbacked(&self, call_ref: &str) {
        locked(&self.refusals, REPLICA_REFUSALS).unbacked.insert(call_ref);
    }

    /// Admit the new call `call_ref` with its backup: an earlier call on the
    /// ref that had none does not stand for it.
    pub fn admit_backed(&self, call_ref: &str) {
        locked(&self.refusals, REPLICA_REFUSALS).unbacked.remove(call_ref);
    }

    /// `backup` refused the replica of this node's call `call_ref`: the call has
    /// no backup from here on, its entry toward `backup` goes, and no scan of
    /// this node offers it to `backup` again. A call this node does not hold
    /// with `backup` as its backup (gone, or another call on the ref) is left
    /// alone.
    pub fn backup_refused(&self, backup: &str, call_ref: &str) {
        {
            let mut meta = locked(&self.meta, REPLICA_METADATA);
            let Some(m) = meta.get_mut(call_ref) else { return };
            if m.role != PartitionRole::Primary || m.backup.as_deref() != Some(backup) {
                return;
            }
            m.backup = None;
            locked(&self.refusals, REPLICA_REFUSALS).unbacked.insert(call_ref);
        }
        self.changelog.forget(backup, Partition::Bak, call_ref);
    }

    /// Whether this node's call `call_ref` has no backup.
    pub fn unbacked(&self, call_ref: &str) -> bool {
        locked(&self.refusals, REPLICA_REFUSALS).unbacked.contains(call_ref)
    }

    /// This node's live calls with no backup.
    pub fn unbacked_count(&self) -> usize {
        locked(&self.refusals, REPLICA_REFUSALS).unbacked.len()
    }

    /// Give up `backup` for this node's call `call_ref`: a write of it could
    /// not reach the backup (its body does not encode, or its frame is refused
    /// unsent), so the call has no backup from here on (ADR-0055 §3). The
    /// call's entry toward `backup` becomes a delete, so a replica the backup
    /// holds of an earlier write goes, and no scan offers the call again. A call
    /// this node does not hold as primary is left unbacked all the same.
    pub fn abandon_backup(&self, backup: &str, call_ref: &str) {
        let incarnation = {
            let mut meta = locked(&self.meta, REPLICA_METADATA);
            locked(&self.refusals, REPLICA_REFUSALS).unbacked.insert(call_ref);
            match meta.get_mut(call_ref) {
                Some(m) if m.role == PartitionRole::Primary => {
                    m.backup = None;
                    m.meta.incarnation.clone()
                }
                _ => None,
            }
        };
        let bak = repl_net::frame::Partition::Bak;
        self.changelog.bump_delete(backup, call_ref, bak, false, incarnation);
    }
}
