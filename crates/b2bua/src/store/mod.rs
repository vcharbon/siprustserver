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
use crate::repl::{ReplicatingCallStore, ReplicationPlan};

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
    /// The replicating store, set only when replication is wired (S10). When
    /// `Some`, [`flush`](CallState::flush)/[`remove`](CallState::remove) route a
    /// call that carries a non-empty `topology.bak` through the S8 write-side
    /// policy ([`flush_replicated`]); when `None`, the legacy `PutOpts::default()`
    /// path is used (backward compatible + correct for non-proxied calls).
    repl_store: Option<Arc<ReplicatingCallStore>>,
    terminate_writer: BufferedTerminateWriter,
    codec: MsgpackCodec,
    self_ordinal: String,
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
            repl_store: None,
            terminate_writer,
            codec: MsgpackCodec::new(),
            self_ordinal: self_ordinal.into(),
            metrics,
        }
    }

    /// Opt into replication: route the flush/remove of any call that carries a
    /// non-empty `topology.bak` through the S8 write-side policy. Builder-style so
    /// existing [`CallState::new`] callers are unchanged (backward compatible).
    /// `repl` MUST be the same store the [`BufferedTerminateWriter`] drains to, so
    /// the changelog bump (keyed off the `PutOpts.peer` this path sets) fires.
    pub fn with_replication(mut self, repl: Arc<ReplicatingCallStore>) -> Self {
        self.repl_store = Some(repl);
        self
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

    /// **Acting-backup takeover read-path (S10b).** When an in-dialog request for
    /// `call_ref` misses the in-memory map (this node never primary-served it),
    /// try to hydrate it from the replicating store's backup partition: the
    /// `call_ref` encodes its original primary, so `partition_of` resolves the
    /// `(Backup, primary)` slot the S5/S6 puller imported the replica into. On a
    /// hit the decoded call is inserted into the in-memory map + re-indexed, so
    /// the router proceeds exactly as if it had primary-served the call — the
    /// failover takeover. Returns the hydrated call (or `None` if no replica /
    /// no replicating store / decode failure). Idempotent: a present call is
    /// returned as-is without a store read.
    pub async fn hydrate_from_replica(&self, call_ref: &str) -> Option<Call> {
        if let Some(c) = self.peek(call_ref) {
            return Some(c);
        }
        let repl = self.repl_store.as_ref()?;
        // The call's natural primary (from the encoded ref); on the acting-backup
        // path this names the crashed peer, and the body lives in `bak:{primary}`.
        let (role, primary) = partition_of(&self.self_ordinal, call_ref);
        // Only the backup partition is a takeover source; a primary-role miss is a
        // genuine orphan (the call is simply gone), not a failover.
        if role != PartitionRole::Backup {
            return None;
        }
        let body = repl.get_call(role, &primary, call_ref).await.ok().flatten()?;
        let call = self.codec.decode(&body).ok()?;
        let mut inner = self.inner.lock().unwrap();
        // Re-check under the lock (a concurrent hydrate may have won the race).
        if let Some(c) = inner.calls.get(call_ref) {
            return Some(c.clone());
        }
        Self::reindex(&mut inner, &call);
        inner.calls.insert(call_ref.to_string(), call.clone());
        drop(inner);
        // A failed-over in-dialog request just loaded its dialog from a backup
        // replica — the acting-backup takeover actually fired.
        self.metrics.bump_repl_takeover_hydrated();
        Some(call)
    }

    /// Replace the in-memory call and refresh its routing index.
    ///
    /// **callGen bump rule (resolves the plan's deferred S10 decision):** each
    /// authoritative mutation of a call increments that call's
    /// `CallTopology.gen` (monotonic per call) — this is the central, single
    /// bump-point. A brand-new call enters at `gen = 1` (stamped at INVITE time in
    /// [`crate::initial_invite`]); every `update` here represents a handler that
    /// just mutated the call, so it out-gens the prior value. Because a takeover
    /// mutation by an acting-backup is therefore *necessarily* higher than the
    /// pre-crash gen, it WINS the replication LWW gate when the rebooting primary
    /// reclaims (see [`crate::repl::replication`]'s `call_gen` rule). The bump is
    /// a no-op for non-proxied calls that carry no `topology`.
    pub fn update(&self, mut call: Call) {
        if let Some(t) = call.topology.as_mut() {
            t.gen += 1;
        }
        let mut inner = self.inner.lock().unwrap();
        Self::reindex(&mut inner, &call);
        inner.calls.insert(call.call_ref.clone(), call);
    }

    /// The backup peer for a call from its `CallTopology.bak`, or `None` when no
    /// replicating store is wired / the call has no topology / `bak` is empty.
    /// This is the resolver the S8 write-side policy ([`ReplicationPlan`]) needs:
    /// the proxy already signed the backup into the `w_bak` cookie and the b2bua
    /// stamped it onto `topology.bak` at INVITE time (see [`crate::initial_invite`]),
    /// so the b2bua never recomputes HRW — it just echoes `w_bak`.
    fn backup_of(&self, call_ref: &str) -> Option<String> {
        self.repl_store.as_ref()?;
        let inner = self.inner.lock().unwrap();
        inner
            .calls
            .get(call_ref)
            .and_then(|c| c.topology.as_ref())
            .map(|t| t.bak.clone())
            .filter(|bak| !bak.is_empty())
    }

    /// Resolve the store target + propagate opts for `call_ref`. When a backup is
    /// resolvable (replicating store + non-empty `topology.bak`) this is the S8
    /// write-side policy ([`ReplicationPlan`]) — Forward when we own the ref,
    /// Reverse (acting-backup) when the ref names a crashed peer. Otherwise it is
    /// today's local-only path: `(partition_of, PutOpts::default())`, no peer →
    /// the replicating store makes NO changelog bump (backward compatible).
    fn store_target(&self, call_ref: &str) -> (PartitionRole, String, PutOpts) {
        match self.backup_of(call_ref) {
            Some(bak) => {
                let plan = ReplicationPlan::resolve(&self.self_ordinal, call_ref, &|_| {
                    Some(bak.clone())
                });
                (plan.role, plan.primary.clone(), plan.put_opts())
            }
            None => {
                let (role, primary) = partition_of(&self.self_ordinal, call_ref);
                (role, primary, PutOpts::default())
            }
        }
    }

    /// Drop a call from memory + the store (its txns/queue are torn down by the
    /// router's `RemoveCall` interpreter step, not here).
    pub fn remove(&self, call_ref: &str) {
        // Resolve the replication target BEFORE evicting the in-memory call (the
        // topology lookup `backup_of` needs the call still present).
        let (role, primary, opts) = self.store_target(call_ref);

        let mut inner = self.inner.lock().unwrap();
        let keys = inner.indexed.remove(call_ref).unwrap_or_default();
        for k in &keys {
            inner.sip_index.remove(k);
        }
        inner.calls.remove(call_ref);
        inner.locks.remove(call_ref);
        drop(inner);

        self.terminate_writer.submit_delete(
            role,
            primary,
            call_ref.to_string(),
            keys,
            opts,
        );
    }

    /// Encode + submit the call to the store (replication path; non-blocking).
    ///
    /// When the call carries a non-empty `topology.bak` and a replicating store is
    /// wired, the put rides the S8 write-side policy (`call_gen = topology.gen`,
    /// peer = the backup, direction Forward/Reverse). The `ReplicatingCallStore`
    /// the [`BufferedTerminateWriter`] drains to then bumps its changelog for that
    /// peer. With no topology / no backup / no replicating store it is today's
    /// `PutOpts::default()` (no propagation) path.
    pub fn flush(&self, call: &Call) {
        let body = self.codec.encode(call);
        let indexes = call_index_keys(call);
        // The authoritative gen lives on the in-memory call (bumped by `update`);
        // the passed `call` may be a pre-bump clone, so prefer the stored gen.
        let call_gen = self
            .inner
            .lock()
            .unwrap()
            .calls
            .get(&call.call_ref)
            .and_then(|c| c.topology.as_ref())
            .or(call.topology.as_ref())
            .map(|t| t.gen)
            .unwrap_or(0);
        let (role, primary, opts) = self.store_target(&call.call_ref);
        // Observability: a propagating flush is one whose call carries a backup
        // peer (topology.bak from the proxy cookie). Rising on the PRIMARY proves
        // the b2bua is attempting replication — distinguishing a cookie/topology
        // gap (stays 0) from a downstream changelog/puller delivery gap.
        if self.backup_of(&call.call_ref).is_some() {
            self.metrics.bump_repl_flush_propagated();
        }
        self.terminate_writer.submit_put(
            role,
            primary,
            call.call_ref.clone(),
            body,
            indexes,
            CALL_TTL_MS,
            call_gen,
            opts,
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

    /// **Acting-backup dialog resolution (the production takeover key).** Recover
    /// a `callRef` for an in-dialog SIP `(callId, tag)` from the replicating
    /// store's SIP index — populated when the S5/S6 puller imported the backup
    /// replica (`put_call` writes `idx:leg:…` → callRef). This is the resolution
    /// path a *real* UAC needs: per RFC 3261 it routes in-dialog requests to the
    /// proxy with the stickiness cookie in the `Route` header, so the request-URI
    /// carries **no** `callref` param and a pure backup's in-memory `sip_index`
    /// is empty — [`resolve_from_sip_key_sync`](Self::resolve_from_sip_key_sync)
    /// misses. Async (the store read may be remote), so the router awaits it only
    /// on the sync-resolve miss. `None` with no replicating store / no replica.
    ///
    /// NB: the simulated failover harness's UAs preserve the b2bua's `callref` in
    /// the in-dialog request-URI, so they resolve via the R-URI param and never
    /// exercised this fallback — the blind spot that let a real-traffic takeover
    /// gap pass the unit tests. See `b2bua-harness` `failover_real_uac_routing`.
    pub async fn resolve_from_replica_index(&self, call_id: &str, tag: &str) -> Option<String> {
        let repl = self.repl_store.as_ref()?;
        if !tag.is_empty() {
            if let Ok(Some(r)) = repl.get_index(&format!("leg:{call_id}|{tag}")).await {
                self.metrics.bump_repl_takeover_resolved();
                return Some(r);
            }
        }
        let hit = repl.get_index(&format!("leg:{call_id}")).await.ok().flatten();
        if hit.is_some() {
            self.metrics.bump_repl_takeover_resolved();
        }
        hit
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
