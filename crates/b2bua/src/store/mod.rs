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

pub use call_store::{
    partition_of, role_of, CallStore, PartitionRole, PropagateDirection, PutOpts, StoreError,
};
pub use memory::InMemoryCallStore;
pub use terminate_writer::BufferedTerminateWriter;

use std::collections::{BTreeMap, HashMap, HashSet};
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
    /// **Live acting-backup takeover copies** (ADR-0011 X11 / ADR-0014): the set
    /// of call_refs this node currently serves as a *reactive* takeover after a
    /// primary failed over to it. Membership only — the router reads it
    /// ([`is_takeover`](CallState::is_takeover)) to drive **self-release**: once the
    /// transaction(s) the backup served for a marked call reach a terminal state,
    /// the backup [`drop_local`](CallState::drop_local)s the live copy (the `bak:`
    /// replica + reverse-flushed deltas remain). No wall-clock, no watermark
    /// handshake. Local-only; never serialized/replicated. Cleared on
    /// drop_local/remove.
    takeover: HashSet<String>,
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
    /// Body TTL (ms) stamped on every replicated flush (ADR-0011 X11). Default
    /// [`CALL_TTL_MS`] (1 h); the replicating runner retunes it to **1.5× the
    /// keepalive interval** so a backup `Element` no longer forward-refreshed by
    /// its primary self-evicts within minutes (the keepalive cadence is the memory
    /// bound, not `max_duration`). A healthy call is re-flushed every keepalive,
    /// well inside the window.
    replicated_ttl_ms: i64,
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
            replicated_ttl_ms: CALL_TTL_MS,
        }
    }

    /// Retune the replicated-body TTL (ADR-0011 X11). The replicating runner sets
    /// this to **1.5× the keepalive interval** so an orphaned backup `Element`
    /// (missed tombstone / primary gone) self-evicts within minutes rather than
    /// the 1 h backstop. Builder-style; the non-replicating path keeps `CALL_TTL_MS`.
    pub fn with_replicated_ttl_ms(mut self, ttl_ms: i64) -> Self {
        if ttl_ms > 0 {
            self.replicated_ttl_ms = ttl_ms;
        }
        self
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
    ///
    /// Returns `(call, fresh)` where `fresh == true` iff the call was just
    /// materialized from the backup partition on THIS call (it was not already
    /// resident in the in-memory map). The router uses `fresh` to decide whether
    /// to re-arm the call's per-call timers (keepalive / global-duration) into
    /// this node's in-memory `TimerService` — those timers are runtime fibers,
    /// not replicated state, so a freshly-hydrated call arrives with no live
    /// timers and would otherwise never be probed or reaped (the failover leak).
    pub async fn hydrate_from_replica(&self, call_ref: &str) -> Option<(Call, bool)> {
        if let Some(c) = self.peek(call_ref) {
            return Some((c, false));
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
            return Some((c.clone(), false));
        }
        Self::reindex(&mut inner, &call);
        inner.calls.insert(call_ref.to_string(), call.clone());
        drop(inner);
        // A failed-over in-dialog request just loaded its dialog from a backup
        // replica — the acting-backup takeover actually fired.
        self.metrics.bump_repl_takeover_hydrated();
        Some((call, true))
    }

    /// Replace the in-memory call and refresh its routing index.
    ///
    /// **Version-vector bump rule (ADR-0014):** each authoritative mutation of a
    /// call increments **the local node's own** counter of that call's `(p,b)`
    /// version vector (`CallTopology.{gen, bak_gen}`) — the central, single
    /// bump-point. Which counter depends on the role this node plays for the
    /// call (resolved by [`partition_of`]):
    /// - **Primary** (the ref's encoded ordinal is ours) → bump `gen` (`p`).
    /// - **Acting-backup** (the ref names a crashed peer we took over) → bump
    ///   `bak_gen` (`b`).
    ///
    /// Because each node bumps only its own counter, the *other* counter carried
    /// on a propagated update is, by construction, the branch point — which is
    /// what lets the asymmetric apply rule (see [`crate::repl::puller`]) resolve
    /// concurrent primary+backup mutations without the latent equal-gen
    /// divergence the old single counter suffered. A brand-new call enters at
    /// `(1,0)` (stamped at INVITE time in [`crate::initial_invite`]). The bump is
    /// a no-op for non-proxied calls that carry no `topology`.
    pub fn update(&self, mut call: Call) {
        if let Some(t) = call.topology.as_mut() {
            match role_of(&self.self_ordinal, &call.call_ref) {
                PartitionRole::Primary => t.gen += 1,
                PartitionRole::Backup => t.bak_gen += 1,
            }
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
        inner.takeover.remove(call_ref);
        drop(inner);

        self.terminate_writer.submit_delete(
            role,
            primary,
            call_ref.to_string(),
            keys,
            opts,
        );
    }

    /// **Local-only self-release teardown** (ADR-0014): drop a live acting-backup
    /// **takeover copy** from the in-memory map + routing index + takeover set,
    /// with **no** store mutation and **no** replication propagation — unlike
    /// [`remove`](Self::remove), which propagates a delete. The call lives on at
    /// its reclaiming primary (which is the node now forward-refreshing this
    /// node's backup `Element`); this node merely sheds the active role once the
    /// transaction(s) it served reached a terminal state. The router pairs this
    /// with timer / txn / dispatch teardown (it owns those). Returns `true` if a
    /// live copy was actually dropped (so the caller meters the self-release once).
    pub fn drop_local(&self, call_ref: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let present = inner.calls.remove(call_ref).is_some();
        if let Some(keys) = inner.indexed.remove(call_ref) {
            for k in &keys {
                inner.sip_index.remove(k);
            }
        }
        inner.locks.remove(call_ref);
        inner.takeover.remove(call_ref);
        present
    }

    /// Release the ephemeral per-call state an **orphan-reject** created — the
    /// 481 path for an in-dialog request that resolved to `call_ref` but hydrated
    /// **no** live call (a failed-over BYE for a dialog that was never reclaimed,
    /// or a late in-dialog request after teardown). [`process`] acquired the
    /// per-call [`lock`](Self::lock) — and the router spun up a per-call dispatch
    /// queue (one `bump_creation`) — yet there is nothing to [`remove`](Self::remove):
    /// the call was never in the map. Drop the lone artifact this leaves behind —
    /// the `locks`-map entry — with **no** store mutation (unlike `remove`, which
    /// would reverse-propagate a *spurious delete* for a call we never held).
    ///
    /// Paired with the router poisoning the dispatch queue (so the worker exits and
    /// `removals` balances `creations`), this is what stops an **orphan storm** —
    /// the thousands of failed-over BYEs that hit a rebooted worker — from leaking
    /// the `locks` map and the `active_calls` (creations−removals) count. Without
    /// it each orphan stranded one lock entry + one idle worker task + one unmatched
    /// creation permanently (the ~3150-per-worker ratchet the leak-detector caught).
    ///
    /// [`process`]: crate::router
    pub fn discard_orphan(&self, call_ref: &str) {
        self.inner.lock().unwrap().locks.remove(call_ref);
    }

    /// Mark `call_ref` as a live acting-backup **takeover copy** (ADR-0011 X11 /
    /// ADR-0014). Called by the router on a fresh failover hydrate. The router
    /// later reads it via [`is_takeover`](Self::is_takeover) to drive self-release
    /// once the served transaction(s) finish. Idempotent per call_ref.
    pub fn mark_takeover(&self, call_ref: &str) {
        self.inner.lock().unwrap().takeover.insert(call_ref.to_string());
    }

    /// Is `call_ref` a live acting-backup **takeover copy** (ADR-0014)? The router
    /// reads it after serving an event for the call: when true and the served
    /// transaction(s) have all cleared, it self-releases the live copy via
    /// [`drop_local`](Self::drop_local).
    pub fn is_takeover(&self, call_ref: &str) -> bool {
        self.inner.lock().unwrap().takeover.contains(call_ref)
    }

    /// **Active-reclaim bulk read-path** (ADR-0011 X11): scan this node's own
    /// `pri:{self}` partition from the replicating store and decode every
    /// reclaimable call. The router materialises each into the live map +
    /// re-arms its timers when it goes Ready (the bulk reclaim sweep that makes a
    /// rebooted primary actually re-*serve*, not just re-*store*). Empty when no
    /// replicating store is wired.
    pub async fn reclaim_scan(&self) -> Vec<Call> {
        let Some(repl) = self.repl_store.as_ref() else {
            return Vec::new();
        };
        let bodies = repl
            .scan_calls(PartitionRole::Primary, &self.self_ordinal)
            .await
            .unwrap_or_default();
        bodies.iter().filter_map(|b| self.codec.decode(b).ok()).collect()
    }

    /// **Active-reclaim reactive read-path** (ADR-0011 X11): decode a single
    /// reclaimable call from this node's `pri:{self}` partition — the flip-race
    /// straggler an acting-backup reverse-flushed *after* the bulk sweep. `None`
    /// if absent, not primary-role for `self`, or no replicating store.
    pub async fn peek_reclaimable(&self, call_ref: &str) -> Option<Call> {
        let repl = self.repl_store.as_ref()?;
        let (role, primary) = partition_of(&self.self_ordinal, call_ref);
        if role != PartitionRole::Primary {
            return None;
        }
        let body = repl.get_call(role, &primary, call_ref).await.ok().flatten()?;
        self.codec.decode(&body).ok()
    }

    /// Materialise a reclaimed call into the live map + routing index iff it is
    /// not already resident (ADR-0011 X11). Returns `true` when just inserted —
    /// the router then re-arms its timers exactly once. Idempotent: a call
    /// already live (re-served, or never lost) is left untouched.
    ///
    /// On insert it also re-establishes the replica's denormalised backup from the
    /// call's authoritative `topology.bak` (ADR-0014 #4): the reboot-reclaim
    /// hydration imports the body via the peerless `PutOpts::default()`, leaving
    /// `CallMeta.backup == None` and the call invisible to its backup's bootstrap
    /// scan until the next keepalive re-flush — re-establishing it here closes that
    /// un-backed-up window the instant the call is re-served.
    pub fn materialize_if_absent(&self, call: Call) -> bool {
        let backup = call
            .topology
            .as_ref()
            .map(|t| t.bak.clone())
            .filter(|b| !b.is_empty());
        let call_ref = call.call_ref.clone();
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.calls.contains_key(&call.call_ref) {
                return false;
            }
            Self::reindex(&mut inner, &call);
            inner.calls.insert(call.call_ref.clone(), call);
        }
        if let (Some(repl), Some(backup)) = (self.repl_store.as_ref(), backup) {
            repl.reestablish_backup(&call_ref, &backup);
        }
        true
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
        // The authoritative `(p,b)` lives on the in-memory call (bumped by
        // `update`); the passed `call` may be a pre-bump clone, so prefer the
        // stored topology.
        let (call_gen, call_bgen) = self
            .inner
            .lock()
            .unwrap()
            .calls
            .get(&call.call_ref)
            .and_then(|c| c.topology.as_ref())
            .or(call.topology.as_ref())
            .map(|t| (t.gen, t.bak_gen))
            .unwrap_or((0, 0));
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
            self.replicated_ttl_ms,
            call_gen,
            call_bgen,
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

    /// The number of live per-call serialization locks. Should track
    /// [`active_count`](Self::active_count); a gap is a **lock leak** — the
    /// orphan-reject path that forgot to [`discard_orphan`](Self::discard_orphan)
    /// (the `b2bua_store_locks` − `b2bua_store_calls` gap). Test/observability.
    pub fn lock_count(&self) -> usize {
        self.inner.lock().unwrap().locks.len()
    }

    /// Push the store's map lengths into the memory-attribution gauges (one
    /// brief lock). `calls.len()` is the TRUE live call-map size — compare to
    /// `b2bua_active_calls` (creations - removals): a divergence is a store-side
    /// leak the counter pair can't see. The sibling maps (`sip_index`,
    /// `indexed`, `locks`, `takeover`) should track `calls`; one that grows
    /// while it stays flat names the leaking map. Sampled periodically by the
    /// runner — not on the hot path.
    pub fn sample_store_gauges(&self) {
        let inner = self.inner.lock().unwrap();
        self.metrics.set_store_gauges(
            inner.calls.len() as u64,
            inner.sip_index.len() as u64,
            inner.indexed.len() as u64,
            inner.locks.len() as u64,
            inner.takeover.len() as u64,
        );
        // State-machine cursor census (ADR-0016 slice 9): how many live calls
        // rest at each (machine,state) cursor. Computed under the same brief lock
        // so the distribution is consistent with the call-map size above, then
        // pushed to the `b2bua_sm_cursors` gauge.
        let mut census: BTreeMap<(String, String), u64> = BTreeMap::new();
        for call in inner.calls.values() {
            for (machine, state) in &call.sm_cursors {
                *census
                    .entry((machine.as_str().to_string(), state.as_str().to_string()))
                    .or_insert(0) += 1;
            }
        }
        self.metrics.set_sm_cursor_census(census);
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
