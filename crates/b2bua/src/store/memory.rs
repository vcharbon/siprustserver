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
    indexes: HashMap<String, String>,
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
        _opts: &PutOpts,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        // Wrap the owned encoded bytes in an `Arc` once, here.
        inner
            .bodies
            .insert(Self::body_key(role, primary, call_ref), Arc::from(body));
        for idx in indexes {
            inner.indexes.insert(format!("idx:{idx}"), call_ref.to_string());
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
        for idx in indexes {
            inner.indexes.remove(&format!("idx:{idx}"));
        }
        Ok(())
    }

    async fn refresh_call(
        &self,
        _role: PartitionRole,
        _primary: &str,
        _call_ref: &str,
        _indexes: &[String],
        _ttl_ms: i64,
        _call_gen: i64,
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
