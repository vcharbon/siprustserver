//! In-memory [`CallStore`] — the non-HA implementation. Bodies are keyed
//! `{role}:{primary}:call:{callRef}`; indexes `idx:{key}` → callRef. The
//! replication params (`peer`/`direction`/`call_gen`/`ttl`) are accepted and
//! ignored — that is the whole point of the seam (ADR-0010 X3). When the HA
//! transport lands, a replicating impl honours them with no caller changes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::call_store::{CallStore, PartitionRole, PutOpts, StoreError};

#[derive(Default)]
struct Inner {
    /// Encoded bodies as immutable shared slices: a rewrite REPLACES the `Arc`
    /// (never mutates in place), so an in-flight drain holding an older clone is
    /// safe (Decision 9 / ADR-0011 X8 immutable-shared-body invariant).
    bodies: HashMap<String, Arc<[u8]>>,
    /// `idx:{key}` → callRef SIP routing index.
    indexes: HashMap<String, String>,
    /// `callRef → the `idx:*` keys it currently owns` — the reverse index that
    /// makes index teardown COMPLETE and idempotent regardless of what the caller
    /// passes. Without it the index map leaked: `put_call` was insert-only (a call
    /// whose index keys changed across re-flushes stranded the old ones) and,
    /// worse, a **replicated `Delete` frame carries NO index keys** (changelog
    /// `delete_frame` sets `indexes: Vec::new()`), so the backup's `delete_call`
    /// removed the body but none of its `idx:*` entries — they accumulated at the
    /// peer's call rate (~4 keys/call) until OOM (the no-chaos RSS climb). The
    /// store now owns the call→keys mapping, so `delete_call` reclaims every key
    /// the call owns even with an empty `indexes` argument.
    idx_by_ref: HashMap<String, Vec<String>>,
}

/// A process-local [`CallStore`]. Cheap to construct; clone the `Arc<dyn>` to
/// share.
#[derive(Default)]
pub struct InMemoryCallStore {
    inner: Mutex<Inner>,
}

impl InMemoryCallStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// `(bodies, indexes)` map lengths — leak-localisation gauge. `indexes`
    /// should track ~`bodies × index-keys-per-call`; a climb in `indexes` while
    /// `bodies` is flat is an idx-removal leak (`put_call` is insert-only, so a
    /// call whose index keys CHANGE across re-flushes, or whose delete passes the
    /// wrong keys, strands `idx:*` entries — the no-chaos RSS climb suspect).
    pub fn lens(&self) -> (usize, usize) {
        let inner = self.inner.lock().unwrap();
        (inner.bodies.len(), inner.indexes.len())
    }

    fn body_key(role: PartitionRole, primary: &str, call_ref: &str) -> String {
        format!("{}:{}:call:{}", role.as_str(), primary, call_ref)
    }
}

#[async_trait]
impl CallStore for InMemoryCallStore {
    async fn get_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Result<Option<Arc<[u8]>>, StoreError> {
        let inner = self.inner.lock().unwrap();
        // `Arc` clone == refcount bump, no byte copy.
        Ok(inner.bodies.get(&Self::body_key(role, primary, call_ref)).cloned())
    }

    async fn put_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        body: Vec<u8>,
        indexes: &[String],
        _ttl_ms: i64,
        _call_gen: i64,
        _call_bgen: i64,
        _opts: &PutOpts,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        // Wrap the owned encoded bytes in an `Arc` once, here.
        inner
            .bodies
            .insert(Self::body_key(role, primary, call_ref), Arc::from(body));
        // REPLACE (not just insert) this call's index keys: drop any previously
        // owned key absent from the new set, then upsert the new set and record
        // it for teardown. A put with no index keys (some reclaim/bootstrap puts)
        // leaves the existing mapping untouched — it is not an authoritative
        // "this call has no keys" signal.
        if !indexes.is_empty() {
            let new_keys: Vec<String> = indexes.iter().map(|i| format!("idx:{i}")).collect();
            if let Some(old) = inner.idx_by_ref.remove(call_ref) {
                for k in old {
                    if !new_keys.contains(&k)
                        && inner.indexes.get(&k).map(|v| v == call_ref).unwrap_or(false)
                    {
                        inner.indexes.remove(&k);
                    }
                }
            }
            for k in &new_keys {
                inner.indexes.insert(k.clone(), call_ref.to_string());
            }
            inner.idx_by_ref.insert(call_ref.to_string(), new_keys);
        }
        Ok(())
    }

    async fn delete_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        indexes: &[String],
        _opts: &PutOpts,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.bodies.remove(&Self::body_key(role, primary, call_ref));
        // Reclaim EVERY index key this call owns from the reverse map — the
        // authoritative teardown that does not depend on the caller passing the
        // keys (a replicated `Delete` frame carries none; changelog `delete_frame`
        // sets `indexes: Vec::new()`). Only drop a key still pointing at us, so a
        // collided/re-owned key for a live call is preserved.
        let owned = inner.idx_by_ref.remove(call_ref).unwrap_or_default();
        for k in &owned {
            if inner.indexes.get(k).map(|v| v == call_ref).unwrap_or(false) {
                inner.indexes.remove(k);
            }
        }
        // Belt-and-suspenders: also honour any explicitly-passed keys (legacy
        // callers / direct in-memory path) not already covered by the reverse map.
        for idx in indexes {
            let k = format!("idx:{idx}");
            if inner.indexes.get(&k).map(|v| v == call_ref).unwrap_or(false) {
                inner.indexes.remove(&k);
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn refresh_call(
        &self,
        _role: PartitionRole,
        _primary: &str,
        _call_ref: &str,
        _indexes: &[String],
        _ttl_ms: i64,
        _call_gen: i64,
        _call_bgen: i64,
    ) -> Result<(), StoreError> {
        // TTL is meaningless for the in-memory store (no eviction clock).
        Ok(())
    }

    async fn get_index(&self, index_key: &str) -> Result<Option<String>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.indexes.get(&format!("idx:{index_key}")).cloned())
    }

    async fn scan_calls(
        &self,
        role: PartitionRole,
        primary: &str,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        let prefix = format!("{}:{}:call:", role.as_str(), primary);
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .bodies
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, v)| v.to_vec())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Vec<String> {
        vec!["leg:cid|tag".to_string(), "leg:bcid".to_string()]
    }

    // The production leak: a replicated `Delete` frame carries NO index keys
    // (changelog `delete_frame` → `indexes: Vec::new()`). delete_call must still
    // reclaim every `idx:*` the call owns, or the backup's index map grows at the
    // peer's call rate until OOM.
    #[tokio::test]
    async fn delete_with_empty_indexes_still_reclaims_index() {
        let s = InMemoryCallStore::new();
        let ks = keys();
        s.put_call(PartitionRole::Backup, "w1", "w1|c|t", b"body".to_vec(), &ks, 0, 1, 0, &PutOpts::default())
            .await
            .unwrap();
        assert_eq!(s.lens(), (1, 2), "body + 2 idx after put");
        // Replicated delete: EMPTY indexes (the changelog delete frame).
        s.delete_call(PartitionRole::Backup, "w1", "w1|c|t", &[], &PutOpts::default())
            .await
            .unwrap();
        assert_eq!(s.lens(), (0, 0), "delete reclaims body AND all idx via reverse map");
    }

    // Re-flush with changed keys must not strand the old ones (put_call was
    // insert-only before).
    #[tokio::test]
    async fn reput_with_changed_keys_drops_the_old() {
        let s = InMemoryCallStore::new();
        s.put_call(PartitionRole::Primary, "w0", "w0|c|t", b"b1".to_vec(), &["leg:a".into()], 0, 1, 0, &PutOpts::default())
            .await
            .unwrap();
        s.put_call(PartitionRole::Primary, "w0", "w0|c|t", b"b2".to_vec(), &["leg:b".into()], 0, 1, 0, &PutOpts::default())
            .await
            .unwrap();
        assert_eq!(s.lens(), (1, 1), "only the new key remains");
        assert_eq!(s.get_index("leg:a").await.unwrap(), None, "stale key gone");
        assert_eq!(s.get_index("leg:b").await.unwrap().as_deref(), Some("w0|c|t"));
    }

    // N create→delete cycles must return the index map to empty (the unit-level
    // analogue of the soak: no per-call residue).
    #[tokio::test]
    async fn churn_returns_index_to_baseline() {
        let s = InMemoryCallStore::new();
        for i in 0..1000 {
            let cr = format!("w0|c{i}|t");
            let ks = vec![format!("leg:cid{i}|tag"), format!("leg:b{i}")];
            s.put_call(PartitionRole::Primary, "w0", &cr, b"x".to_vec(), &ks, 0, 1, 0, &PutOpts::default())
                .await
                .unwrap();
            // delete as the replicated path does: empty indexes.
            s.delete_call(PartitionRole::Primary, "w0", &cr, &[], &PutOpts::default())
                .await
                .unwrap();
        }
        assert_eq!(s.lens(), (0, 0), "1000 create→delete cycles leave no residue");
    }
}
