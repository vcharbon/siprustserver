//! `CallState` — the in-memory call map + per-call serialization, over the
//! [`CallStore`] persistence/replication seam. Port of the load-bearing parts
//! of `src/call/CallState.ts` (the Redis/orphan-sweep/HA-topology machinery is
//! the deferred HA slice; see ADR-0010 un-ported list).
//!
//! Live calls are held *typed* in `calls`; the store path encodes via
//! [`MsgpackCodec`] only for flush/replication. Routing uses the in-memory
//! `sip_index`; the store's `get_index` is the fallback the HA slice will use.

mod call_store;
mod memory;
mod terminate_writer;

pub use call_store::{partition_of, CallStore, PartitionRole, PropagateDirection, PutOpts, StoreError};
pub use memory::InMemoryCallStore;
pub use terminate_writer::BufferedTerminateWriter;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use call::{call_index_keys, Call, CallBodyCodec, MsgpackCodec};

use crate::metrics::B2buaMetrics;

/// Stored-call TTL handed to the (HA) store; ignored by the in-memory impl.
const CALL_TTL_MS: i64 = 3_600_000;

#[derive(Default)]
struct Inner {
    calls: HashMap<String, Call>,
    /// SIP routing index: `leg:callId|tag` / `leg:callId` / `ctx:...` → callRef.
    sip_index: HashMap<String, String>,
    /// The index keys each call currently owns (for clean re-index on update).
    indexed: HashMap<String, Vec<String>>,
    /// Per-callRef serialization lock (the second FIFO layer over the dispatcher).
    locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

/// The call store. Clone-cheap (one `Arc`); share across the stack.
#[derive(Clone)]
pub struct CallState {
    inner: Arc<Mutex<Inner>>,
    store: Arc<dyn CallStore>,
    terminate_writer: BufferedTerminateWriter,
    codec: MsgpackCodec,
    self_ordinal: String,
    #[allow(dead_code)]
    metrics: B2buaMetrics,
}

impl CallState {
    /// Decode every call this worker owns as primary from the store (crash
    /// recovery read-path). The HA slice extends this to backup partitions +
    /// timer re-arming; here it is bounded by what the in-memory store holds.
    pub async fn load_owned(&self) -> Result<Vec<Call>, StoreError> {
        let bodies = self
            .store
            .scan_calls(PartitionRole::Primary, &self.self_ordinal)
            .await?;
        Ok(bodies
            .iter()
            .filter_map(|b| self.codec.decode(b).ok())
            .collect())
    }

    pub fn new(
        store: Arc<dyn CallStore>,
        terminate_writer: BufferedTerminateWriter,
        self_ordinal: impl Into<String>,
        metrics: B2buaMetrics,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            store,
            terminate_writer,
            codec: MsgpackCodec::new(),
            self_ordinal: self_ordinal.into(),
            metrics,
        }
    }

    /// Insert a freshly-created call + index it. Returns its `callRef`.
    pub fn create(&self, call: Call) -> String {
        let call_ref = call.call_ref.clone();
        let mut inner = self.inner.lock().unwrap();
        Self::reindex(&mut inner, &call);
        inner.calls.insert(call_ref.clone(), call);
        call_ref
    }

    /// Snapshot a call (clone) from memory, if present.
    pub fn peek(&self, call_ref: &str) -> Option<Call> {
        self.inner.lock().unwrap().calls.get(call_ref).cloned()
    }

    /// Replace the in-memory call and refresh its routing index.
    pub fn update(&self, call: Call) {
        let mut inner = self.inner.lock().unwrap();
        Self::reindex(&mut inner, &call);
        inner.calls.insert(call.call_ref.clone(), call);
    }

    /// Drop a call from memory + the store (its txns/queue are torn down by the
    /// router's `RemoveCall` interpreter step, not here).
    pub fn remove(&self, call_ref: &str) {
        let mut inner = self.inner.lock().unwrap();
        let keys = inner.indexed.remove(call_ref).unwrap_or_default();
        for k in &keys {
            inner.sip_index.remove(k);
        }
        inner.calls.remove(call_ref);
        inner.locks.remove(call_ref);
        drop(inner);

        let (role, primary) = partition_of(&self.self_ordinal, call_ref);
        self.terminate_writer.submit_delete(
            role,
            primary,
            call_ref.to_string(),
            keys,
            PutOpts::default(),
        );
    }

    /// Encode + submit the call to the store (replication path; non-blocking).
    pub fn flush(&self, call: &Call) {
        let body = self.codec.encode(call);
        let indexes = call_index_keys(call);
        let call_gen = call.topology.as_ref().map(|t| t.gen).unwrap_or(0);
        let (role, primary) = partition_of(&self.self_ordinal, &call.call_ref);
        self.terminate_writer.submit_put(
            role,
            primary,
            call.call_ref.clone(),
            body,
            indexes,
            CALL_TTL_MS,
            call_gen,
            PutOpts::default(),
        );
    }

    /// Resolve an inbound SIP `(callId, tag)` to a `callRef` from the in-memory
    /// index only (the sync path the dispatcher's route key needs). Falls back
    /// to the bare callId (untagged CANCEL matching).
    pub fn resolve_from_sip_key_sync(&self, call_id: &str, tag: &str) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        if !tag.is_empty() {
            if let Some(r) = inner.sip_index.get(&format!("leg:{call_id}|{tag}")) {
                return Some(r.clone());
            }
        }
        inner.sip_index.get(&format!("leg:{call_id}")).cloned()
    }

    /// Acquire the per-callRef serialization lock (held across a handler run).
    pub async fn lock(&self, call_ref: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut inner = self.inner.lock().unwrap();
            inner
                .locks
                .entry(call_ref.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    pub fn active_count(&self) -> usize {
        self.inner.lock().unwrap().calls.len()
    }

    /// Recompute and apply a call's routing index, dropping any stale keys.
    fn reindex(inner: &mut Inner, call: &Call) {
        let call_ref = &call.call_ref;
        if let Some(old) = inner.indexed.remove(call_ref) {
            for k in &old {
                inner.sip_index.remove(k);
            }
        }
        let keys = call_index_keys(call);
        for k in &keys {
            inner.sip_index.insert(k.clone(), call_ref.clone());
        }
        inner.indexed.insert(call_ref.clone(), keys);
    }
}
