//! **Acting-backup dialog resolution**: the call a message names, read from
//! the replicating store's SIP index (the puller's `put_call` writes
//! `idx:<key>` → callRef) in the [`IndexLookup::probes`] order the live index
//! uses.

use call::{IndexHit, IndexLookup, Probe};

use super::{CallState, CallStore, StoreError};

impl CallState {
    /// The call of an in-dialog request or a CANCEL that states no `callRef`
    /// of ours, and the namespace that matched, counted as a takeover
    /// resolution. A request routed per RFC 3261 §12.2.1.1 carries our
    /// Contact, `callRef` included, as its Request-URI, and a strict router's
    /// §16.4 rewrite restores it; only an element that rewrites the
    /// Request-URI leaves the call to be found by dialog identity. The router
    /// awaits this read when the live index
    /// ([`resolve_from_sip_key_sync`](Self::resolve_from_sip_key_sync)) misses,
    /// as it does on a backup that never served the call. `Ok(None)` with no
    /// replicating store or no replica; `Err` when a read failed, so the store
    /// cannot say whether the dialog exists.
    pub async fn resolve_from_replica_index(
        &self,
        lookup: IndexLookup<'_>,
    ) -> Result<Option<IndexHit>, StoreError> {
        let hit = self.replica_index_hit(lookup).await?;
        if let Some(hit) = &hit {
            self.note_takeover_resolved(hit);
        }
        Ok(hit)
    }

    /// Count a call the replica index resolved for a takeover.
    pub fn note_takeover_resolved(&self, hit: &IndexHit) {
        self.metrics.bump_repl_takeover_resolved();
        self.note_takeover(&hit.call_ref, "resolved");
    }

    /// The call `lookup` names in the replicating store's SIP index, read
    /// as [`resolve_from_replica_index`](Self::resolve_from_replica_index)
    /// reads it, counting nothing.
    pub async fn replica_index_hit(
        &self,
        lookup: IndexLookup<'_>,
    ) -> Result<Option<IndexHit>, StoreError> {
        let Some(repl) = self.repl_store() else { return Ok(None) };
        let mut key = lookup.key_buffer();
        let mut hit = None;
        for probe in lookup.probes() {
            match *probe {
                Probe::Resolve(kind) => {
                    lookup.write_key(kind, &mut key);
                    hit = repl.get_index(&key).await?.map(|call_ref| IndexHit { call_ref, kind });
                    if hit.is_some() {
                        break;
                    }
                }
                Probe::Refuse(kind) => {
                    lookup.write_key(kind, &mut key);
                    if repl.get_index(&key).await?.is_some() {
                        break;
                    }
                }
            }
        }
        Ok(hit)
    }
}
