//! The store's write path: a body and its metadata stored, a delete
//! tombstoned, and the changelog entry moved, in one synchronous call.

use std::sync::Arc;

use super::admission::Write;
use super::{CallMeta, ReplicatingCallStore};
use crate::repl::changelog::RefMeta;
use crate::repl::incarnation::is_another_call;
use crate::repl::resurrection::Burial;
use crate::store::{
    locked, PartitionRole, PropagateDirection, PutOpts, StoreError, REPLICA_METADATA,
    REPLICA_REFUSALS, RESURRECTION_TOMBSTONES,
};

impl ReplicatingCallStore {
    /// [`CallStore::put_call`](crate::store::CallStore::put_call),
    /// synchronously: the body and its metadata are stored and the changelog
    /// entry for `opts.peer` moved in this call, so a turn's write is in the
    /// change log before any of its effects leaves (ADR-0055 §3).
    #[allow(clippy::too_many_arguments)]
    pub fn write_put(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        body: Vec<u8>,
        indexes: &[String],
        ttl_ms: i64,
        call_gen: i64,
        call_bgen: i64,
        opts: &PutOpts,
    ) -> Result<(), StoreError> {
        let now = self.clock.now_ms();
        // Apply the backstop TTL for ttl_ms <= 0 so a missed-delete replica still
        // self-evicts (the wire-carried `body_ttl_ms` keeps the original ttl_ms).
        let expiry_at_ms = self.expiry_for(now, ttl_ms);
        // The body's clock offset (ADR-0055 §9): the SAME `now` reading that
        // anchors `expiry_at_ms` minus the sender's send-time reading in the
        // body's frame. `None` on a write of this node's own body: its
        // instants are in this node's frame, whatever the body it replaces.
        let skew_offset_ms = opts.origin_now_ms.map(|origin| now.saturating_sub(origin));
        // Everything the write carries is built before the lock: the critical
        // section is map operations only.
        let body: Arc<[u8]> = Arc::from(body);
        let mut ref_meta = RefMeta {
            call_gen,
            call_bgen,
            body_ttl_ms: ttl_ms,
            indexes: indexes.to_vec(),
            incarnation: opts.incarnation.clone(),
            skew_offset_ms,
        };
        // A Forward flush carries the backup ordinal as `opts.peer`.
        let flushed_backup = match (opts.direction, &opts.peer) {
            (Some(PropagateDirection::Forward), Some(p)) => Some(p.clone()),
            _ => None,
        };

        // Store the body and update the per-ref metadata in one critical section
        // under the metadata lock, the lock the reap evicts under, so a reap
        // never splits a write. A write CARRIES OVER every field it does not
        // itself carry: the backup ordinal and the authority's view of the
        // answer are facts about the ref that only their own writer may change,
        // and rebuilding the entry from this write alone would silently clear
        // each of them.
        {
            let mut meta = locked(&self.meta, REPLICA_METADATA);
            // An unnamed write is one of the held call: it takes its name.
            ref_meta.incarnation = ref_meta
                .incarnation
                .take()
                .or_else(|| meta.get(call_ref).and_then(|m| m.meta.incarnation.clone()));
            let version = (call_gen, call_bgen);
            let write =
                Write { role, direction: opts.direction, now, version, adopted: opts.adopted };
            if !self.admits(&mut meta, call_ref, ref_meta.incarnation.as_deref(), &body, write) {
                return Ok(());
            }
            self.inner.store_body(role, primary, call_ref, body, indexes);
            match meta.get_mut(call_ref) {
                Some(m) => {
                    self.account_role(Some(m.role), Some(role));
                    m.meta = ref_meta;
                    m.role = role;
                    m.primary = primary.to_string();
                    m.expiry_at_ms = expiry_at_ms;
                    if flushed_backup.is_some() {
                        m.backup = flushed_backup;
                    }
                }
                None => {
                    self.account_role(None, Some(role));
                    meta.insert(
                        call_ref.to_string(),
                        CallMeta {
                            meta: ref_meta,
                            role,
                            primary: primary.to_string(),
                            backup: flushed_backup,
                            expiry_at_ms,
                            // Until a forward flush shows one, no answer of the
                            // authority's own stands on this Element.
                            authority_answered: false,
                        },
                    );
                }
            }
        }

        locked(&self.refusals, REPLICA_REFUSALS).replicas.clear(call_ref);

        // HA path only: non-blocking changelog bump for the pulling peer.
        if let Some(peer) = &opts.peer {
            let partition = Self::partition_for(opts.direction);
            self.changelog.bump_put(peer, call_ref, partition);
        }
        Ok(())
    }

    /// [`CallStore::delete_call`](crate::store::CallStore::delete_call),
    /// synchronously, as [`write_put`](Self::write_put).
    pub fn write_delete(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        indexes: &[String],
        answered: bool,
        opts: &PutOpts,
    ) -> Result<(), StoreError> {
        let deleted_at = self.clock.now_ms();
        let incarnation = {
            let mut meta = locked(&self.meta, REPLICA_METADATA);
            let held = meta.get(call_ref).and_then(|m| m.meta.incarnation.as_deref());
            let mut tombstones = locked(&self.tombstones, RESURRECTION_TOMBSTONES);
            // A delete of another call than the held one ends that call alone:
            // the held call stays, and nothing propagates over its entry.
            if meta.contains_key(call_ref) && is_another_call(held, opts.incarnation.as_deref()) {
                tombstones.bury(call_ref, opts.incarnation.clone(), Burial::Deleted, deleted_at);
                return Ok(());
            }
            self.inner.remove_body(role, primary, call_ref, indexes);
            let gone = meta.remove(call_ref);
            if let Some(gone) = &gone {
                self.account_role(Some(gone.role), None);
            }
            // Tombstone the call in the same step, so a late reverse-flush cannot
            // resurrect it (see `put_call`); pruned in `reap`. The delete names
            // the call it removes, or the body it finds names it.
            let incarnation =
                opts.incarnation.clone().or_else(|| gone.and_then(|g| g.meta.incarnation));
            tombstones.bury(call_ref, incarnation.clone(), Burial::Deleted, deleted_at);
            incarnation
        };
        let mut refusals = locked(&self.refusals, REPLICA_REFUSALS);
        refusals.replicas.clear(call_ref);
        refusals.unbacked.remove(call_ref);
        drop(refusals);

        if let Some(peer) = &opts.peer {
            let partition = Self::partition_for(opts.direction);
            self.changelog.bump_delete(peer, call_ref, partition, answered, incarnation);
        }
        Ok(())
    }
}
