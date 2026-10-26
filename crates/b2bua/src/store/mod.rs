//! `CallState` — the in-memory call map + per-call serialization, over the
//! [`CallStore`] persistence/replication seam. Port of the load-bearing parts
//! of `src/call/CallState.ts` (ADR-0010).
//!
//! Live calls are held *typed* in `calls`; the store path encodes via
//! [`MsgpackCodec`] only for flush/replication. Routing uses the in-memory
//! `sip_index`; the store's `get_index` is the fallback.

mod backup;
mod call_store;
pub mod containment;
pub mod envelope;
mod faults;
mod flush_body;
mod materialise;
mod memory;
mod poison;
mod reap;
mod reconcile;
mod replica_index;
mod resident;
mod resident_envelope;
mod scan;
mod setup_cancel;
#[cfg(test)]
mod setup_cancel_tests;
#[cfg(test)]
mod spiral_index_tests;
mod takeover;
pub use reap::ReapedBodies;
#[cfg(test)]
mod unwired_tests;

pub use backup::HoldTarget;
pub use call_store::{
    partition_of, role_of, CallStore, PartitionRole, PropagateDirection, PutOpts, StoreError,
};
pub use envelope::{CallTopology, Enveloped};
pub use faults::{FaultInjectingCallStore, StoreFaultPoint, StoreFaults};
pub use memory::InMemoryCallStore;
pub(crate) use poison::*;
pub use poison::{LOCKS, LOCK_RECOVERIES};
pub use reconcile::Reconciled;
pub use takeover::TakeoverProbe;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use call::{call_index_keys, Call, CallModelState, IndexHit, IndexLookup, MsgpackCodec, Probe};

use crate::metrics::B2buaMetrics;
use crate::repl::{ReplicatingCallStore, ReplicationPlan};

/// Stored-call TTL handed to the (HA) store; ignored by the in-memory impl.
const CALL_TTL_MS: i64 = 3_600_000;

/// Why a narrow partition read ([`CallState::peek_replica`] /
/// [`CallState::peek_reclaimable`]) returned no call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaMiss {
    /// This node plays the other role for the ref: a takeover read of a
    /// primary-role ref, or a reclaim read of a backup-role ref.
    WrongRole,
    /// No body in the partition (never replicated, expired, or no replicating
    /// store wired).
    Absent,
    /// A body is present but does not decode as a `Call`.
    Undecodable,
}

/// Which entry path is inserting a materialised call
/// ([`CallState::materialize_if_absent`]): only a reclaim re-establishes the
/// replica's denormalised backup, since a takeover copy is the backup itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterialiseOrigin {
    /// An acting-backup takeover copy of a peer's call (`bak:{primary}`).
    Takeover,
    /// This node's own call re-served from `pri:{self}`.
    Reclaim,
}

#[derive(Default)]
struct Inner {
    /// Boxed so a bucket is a pointer, not the whole record: the table's
    /// doubling step and its footprint after a drain stay small, and a call's
    /// own memory goes back to the allocator when it is removed.
    calls: HashMap<String, Arc<Call>>,
    /// The store envelope of each resident call placed under a worker pair
    /// (ADR-0055 §7): written at every entry and every write of its call,
    /// removed with it.
    topologies: HashMap<String, CallTopology>,
    /// SIP routing index: a [`call::index_key`] key → callRef.
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
    takeover: takeover::TakeoverMarks,
    /// **Last-touched stamps** (ADR-0020 X4) — the call reaper's only liveness
    /// input: epoch-ms of the last dispatched event that reached the handler
    /// (`router::process` touches), stamped at every entry
    /// (create / materialise). Node-local, NEVER a `Call` field
    /// (touching must not dirty the call or trigger a replication flush).
    /// Membership mirrors `calls` exactly: stamped at insertion, cleared in
    /// `remove`/`drop_local`; an orphan never enters either.
    touched: HashMap<String, i64>,
    /// **Setup-CANCEL marks**: `(call_ref, INVITE CSeq)` of the initial
    /// INVITEs whose server transaction the txn layer already finalized (200
    /// to the CANCEL, 487 to the INVITE) while their turn, or its decision
    /// round trip, is still parked on the per-call FIFO — the queued
    /// `Cancelled` event cannot reach the call model until that turn ends, so
    /// the model alone cannot tell the seam the caller is gone. Keyed by CSeq
    /// so a retry waiting behind its identity's previous call keeps its own.
    /// Set at `Cancelled` ingress (router run loop) — only while that INVITE
    /// is admitted and waiting, or is the live call's own — read by that
    /// INVITE's turn
    /// (`router::process::initial_invite_turn`). Node-local, never
    /// serialized; the router's release of the call clears every mark no
    /// waiting INVITE holds ([`retain_setup_cancelled`](Self::retain_setup_cancelled)).
    /// The marked CSeqs of each `callRef`; no entry is empty.
    setup_cancelled: HashMap<String, Vec<u32>>,
    /// Resident calls that run uncounted (`CallLimiterState::runs_uncounted`):
    /// moved by every insertion, replacement and removal of `calls`, so it is
    /// exact at every write, and published as `b2bua_limiter_uncounted_calls`.
    uncounted: u64,
}

/// Below this many buckets a table is never shrunk: the saving is noise and a
/// small node would rehash on every removal.
const SHRINK_MIN_CAPACITY: usize = 1024;

/// Give a drained table back to the allocator: once it is under a quarter full
/// it is rehashed to twice its length. The factor-of-four gap is the hysteresis
/// that keeps a table oscillating around one size from rehashing on every removal.
fn shrink_idle<K: Eq + std::hash::Hash, V>(map: &mut HashMap<K, V>) {
    let cap = map.capacity();
    if cap > SHRINK_MIN_CAPACITY && map.len() < cap / 4 {
        map.shrink_to(map.len() * 2);
    }
}

impl Inner {
    /// Account one call slot of `calls` going from a resident call that ran
    /// uncounted (`was`) to one that does (`is`); an absent call runs
    /// nothing.
    fn restate_uncounted(&mut self, was: bool, is: bool) {
        match (was, is) {
            (false, true) => self.uncounted += 1,
            (true, false) => self.uncounted = self.uncounted.saturating_sub(1),
            _ => {}
        }
    }

    /// Drop `call_ref`'s index `keys`, leaving any key another call now holds.
    fn unindex(&mut self, call_ref: &str, keys: &[String]) {
        for k in keys {
            if self.sip_index.get(k).is_some_and(|owner| owner == call_ref) {
                self.sip_index.remove(k);
            }
        }
    }

    /// Shrink the per-call maps after a removal (see [`shrink_idle`]); the
    /// membership sets hold pointer-sized buckets and are left as they are.
    fn shrink_idle(&mut self) {
        shrink_idle(&mut self.calls);
        shrink_idle(&mut self.topologies);
        shrink_idle(&mut self.sip_index);
        shrink_idle(&mut self.indexed);
        shrink_idle(&mut self.locks);
        shrink_idle(&mut self.touched);
    }
}

/// The call store. Clone-cheap (one `Arc`); share across the stack.
#[derive(Clone)]
pub struct CallState {
    inner: Arc<Mutex<Inner>>,
    store: Arc<dyn CallStore>,
    /// The replicating store, present only on a wired node. It is the one switch
    /// on the store write path: `None` and
    /// [`flush`](CallState::flush)/[`remove`](CallState::remove) touch the store
    /// not at all, whatever topology a call carries (the front proxy stamps a
    /// backup peer on every call whenever two workers are alive, and nothing
    /// reads what an unwired node would write). `Some` and a call with a backup
    /// ([`backup_of`](CallState::backup_of)) rides the write-side policy
    /// ([`ReplicationPlan`]); a call with none takes `PutOpts::default()`. Every
    /// write lands in the store and its change log within the call that makes
    /// it: none is queued, none dropped (ADR-0055 §3).
    repl: Option<Arc<ReplicatingCallStore>>,
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
    /// Time source for the last-touched stamps written at the two entry sites
    /// (create / materialise) — internal so no entry path can forget to stamp
    /// (ADR-0020 X4). `b2bua_core` wires the runtime clock;
    /// the default reads the same paused/tokio time the tests advance.
    clock: sip_clock::Clock,
    /// Acting-backup takeover burst aggregation, keyed by the dead peer
    /// (ADR-0026): a 5000-call failover owes ~3 log lines per peer, never one
    /// per hydrated call. The router's `materialise` and self-release paths
    /// fold into it so a takeover and the shedding that ends it read as ONE
    /// episode.
    takeover_log: Arc<observe::WaveSet>,
}

impl CallState {
    /// Decode every call this worker owns as primary from the store (crash
    /// recovery read-path), bounded by what the store holds for the primary
    /// partition.
    pub async fn load_owned(&self) -> Result<Vec<Enveloped>, StoreError> {
        let bodies = self.store.scan_calls(PartitionRole::Primary, &self.self_ordinal).await?;
        Ok(bodies.iter().filter_map(|b| envelope::decode(&self.codec, b).ok()).collect())
    }

    /// An unwired call state over `store`: the store is read (`load_owned`)
    /// and never written until [`with_replication`](Self::with_replication).
    pub fn new(
        store: Arc<dyn CallStore>,
        self_ordinal: impl Into<String>,
        metrics: B2buaMetrics,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            store,
            repl: None,
            codec: MsgpackCodec::new(),
            self_ordinal: self_ordinal.into(),
            metrics,
            replicated_ttl_ms: CALL_TTL_MS,
            clock: sip_clock::Clock::test_at(0),
            takeover_log: crate::lifecycle::takeover_waves(),
        }
    }

    /// Wire the runtime clock (stamps the last-touched ledger at the
    /// materialisation sites). Builder-style; the default test clock reads the
    /// same paused tokio time the harnesses advance.
    pub fn with_clock(mut self, clock: sip_clock::Clock) -> Self {
        self.clock = clock;
        self
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

    /// Opt into replication: open the store write path into `repl`, and route
    /// the flush/remove of any call with a backup through the write-side
    /// policy. Without this call the store is never written.
    pub fn with_replication(mut self, repl: Arc<ReplicatingCallStore>) -> Self {
        self.repl = Some(repl);
        self
    }

    /// The replicating store, when replication is wired.
    pub(crate) fn repl_store(&self) -> Option<&Arc<ReplicatingCallStore>> {
        self.repl.as_ref()
    }

    /// The call map's lock ([`poison::CALL_STORE`]).
    fn locked(&self) -> std::sync::MutexGuard<'_, Inner> {
        poison::locked(&self.inner, poison::CALL_STORE)
    }

    /// Insert a freshly-created call, placed under `topology`, + index it.
    /// Returns its `callRef`. The index, the stamp, the envelope and the
    /// uncounted tally are written before the call itself, so a panic between
    /// them leaves only entries the sweep drops.
    pub fn create(&self, call: Call, topology: Option<CallTopology>) -> String {
        let call_ref = call.call_ref.clone();
        let now_ms = self.clock.now_ms();
        let is = call.limiter.runs_uncounted();
        let mut inner = self.locked();
        let was = inner.calls.get(&call_ref).is_some_and(|c| c.limiter.runs_uncounted());
        Self::reindex(&mut inner, &call);
        inner.touched.insert(call_ref.clone(), now_ms);
        Self::place(&mut inner, &call_ref, topology);
        inner.restate_uncounted(was, is);
        self.publish_uncounted(&inner);
        inner.calls.insert(call_ref.clone(), Arc::new(call));
        call_ref
    }

    /// Refresh the **last-touched stamp** (ADR-0020 X4) — the call reaper's only
    /// liveness input. Called by `router::process` once per dispatched event
    /// that reached the handler (a wedged FIFO never reaches it, so its stamp
    /// freezes — exactly the staleness signal). Monotonic-max, no-op for a
    /// non-resident call (membership mirrors the live map).
    pub fn touch(&self, call_ref: &str, now_ms: i64) {
        let mut inner = self.locked();
        if !inner.calls.contains_key(call_ref) {
            return;
        }
        let stamp = inner.touched.entry(call_ref.to_string()).or_insert(now_ms);
        *stamp = (*stamp).max(now_ms);
    }

    /// The current last-touched stamp, or `None` for a non-resident call — the
    /// reaper verdict confirm input (ADR-0020 X5): a verdict whose observed
    /// watermark no longer matches is stale and must be discarded.
    pub fn last_touched(&self, call_ref: &str) -> Option<i64> {
        self.locked().touched.get(call_ref).copied()
    }

    /// Cheap residency check (no `Call` clone) — sweep-side strike pruning.
    pub fn contains(&self, call_ref: &str) -> bool {
        self.locked().calls.contains_key(call_ref)
    }

    /// Sweep input (ADR-0020 X3/X4): every live call whose last-touched stamp is
    /// older than `now_ms - idle_max_ms`, paired with the observed stamp (the
    /// verdict's confirm watermark) — **excluding acting-backup takeover
    /// copies** (self-release owns those, push-based on `CallQuiesced`; the
    /// reaper sweep never selects one). `bak:` Elements are structurally absent
    /// (they live in the replica store, not in `calls`). One pass under the
    /// inner lock.
    pub fn stale_candidates(&self, now_ms: i64, idle_max_ms: i64) -> Vec<(String, i64)> {
        let inner = self.locked();
        inner
            .touched
            .iter()
            .filter(|(call_ref, stamp)| {
                now_ms - **stamp > idle_max_ms && !inner.takeover.contains(call_ref)
            })
            .map(|(call_ref, stamp)| (call_ref.clone(), *stamp))
            .collect()
    }

    /// Snapshot a call (clone) from memory, if present.
    pub fn peek(&self, call_ref: &str) -> Option<Call> {
        self.locked().calls.get(call_ref).map(|c| (**c).clone())
    }

    /// Whether the resident call `call_ref` is the incarnation `incarnation`
    /// (`Call::incarnation`), read without cloning it; `None` when no call is
    /// resident.
    pub fn is_incarnation(&self, call_ref: &str, incarnation: &str) -> Option<bool> {
        self.locked().calls.get(call_ref).map(|c| c.incarnation() == incarnation)
    }

    /// The lifecycle state of the resident call `call_ref`, read without
    /// cloning it.
    pub fn model_state(&self, call_ref: &str) -> Option<CallModelState> {
        self.locked().calls.get(call_ref).map(|c| c.state)
    }

    /// The `bak:{primary}` body of `call_ref` — the acting-backup takeover
    /// source, read without inserting (the router's `materialise` module owns
    /// the decision and the insert). The `call_ref` encodes its original
    /// primary, so `partition_of` resolves the `(Backup, primary)` slot the
    /// puller imported the replica into; a primary-role ref is a
    /// [`ReplicaMiss::WrongRole`] (the backup partition is the only takeover
    /// source). Returns the decoded call with its persisted receive-time
    /// clock-skew offset (`0` when none). No state check: a `Terminated` body
    /// is returned as-is for the caller to refuse.
    pub async fn peek_replica(&self, call_ref: &str) -> Result<(Enveloped, i64), ReplicaMiss> {
        self.read_partition(PartitionRole::Backup, call_ref).await
    }

    /// Replace the in-memory call and refresh its routing index.
    ///
    /// **Version-vector bump rule (ADR-0014):** each authoritative mutation of a
    /// call increments **the local node's own** counter of that call's `(p,b)`
    /// version vector (the envelope's `CallTopology.{gen, bak_gen}`) — the
    /// central, single bump-point. Which counter depends on the role this node plays for the
    /// call (resolved by [`partition_of`]): **Primary** (the ref's encoded
    /// ordinal is ours) bumps `gen` (`p`), **acting-backup** (the ref names a
    /// crashed peer we took over) bumps `bak_gen` (`b`). Each node bumps only
    /// its own counter, so the *other* counter carried on a propagated update
    /// is, by construction, the branch point. A brand-new call enters at
    /// `(1,0)` (placed at INVITE time from the proxy's cookie). The bump is a
    /// no-op for a non-proxied call, placed under no worker pair.
    ///
    /// **Replace-only.** `update` never *inserts*: every live call enters the map
    /// through [`create`](Self::create) / [`materialize_if_absent`](Self::materialize_if_absent),
    /// both of which mark/index/arm as the entry path requires. An update racing a release
    /// (`remove`/`drop_local` evicted the call after this handler checked it out)
    /// must NOT resurrect an unmarked, unwatched, timer-less zombie copy — the
    /// X11 double-serve class. The dropped mutation is safe: the call is gone on
    /// this node by an authoritative teardown, and any later event re-enters via
    /// a fresh hydrate.
    ///
    /// A bump counts a write this node made; a quiet turn
    /// ([`crate::effects::QuietTurn`]) and a meeting with the other owner's
    /// copy go through [`update_quiet`](Self::update_quiet) and move no axis.
    ///
    /// **The state stamp (ADR-0055 §7)** is set here and nowhere else: a write
    /// that changes a compared field of the resident copy
    /// ([`call::helpers::same_state`]) stamps the vector it bumps; a write that
    /// changes only exempt fields leaves the stamp ([`containment`]). It runs
    /// outside the map lock: the caller holds the call's own.
    pub fn update(&self, call: Call) {
        let Some(resident) = self.locked().calls.get(&call.call_ref).cloned() else {
            return;
        };
        let changed = !call::helpers::same_state(&resident, &call);
        let mut inner = self.locked();
        if let Some(t) = inner.topologies.get_mut(&call.call_ref) {
            match role_of(&self.self_ordinal, &call.call_ref) {
                PartitionRole::Primary => t.gen += 1,
                PartitionRole::Backup => t.bak_gen += 1,
            }
            if changed {
                t.stamp_vector();
            }
        }
        self.replace_locked(&mut inner, call);
    }

    /// Replace the in-memory call for a quiet turn or a meeting: the same
    /// replace-only rule as [`update`](Self::update) with no counter moved —
    /// the `(p,b)` the resident copy carries is the last write's, either
    /// owner's, and so is what the next flush publishes.
    pub fn update_quiet(&self, call: Call) {
        let mut inner = self.locked();
        self.replace_locked(&mut inner, call);
    }

    fn replace_locked(&self, inner: &mut Inner, call: Call) {
        if !inner.calls.contains_key(&call.call_ref) {
            return;
        }
        Self::reindex(inner, &call);
        let was = inner.calls.get(&call.call_ref).is_some_and(|c| c.limiter.runs_uncounted());
        if was != call.limiter.runs_uncounted() {
            inner.restate_uncounted(was, call.limiter.runs_uncounted());
            self.publish_uncounted(inner);
        }
        if let Some(slot) = inner.calls.get_mut(&call.call_ref) {
            *slot = Arc::new(call);
        }
    }

    /// Publish the resident calls that run uncounted.
    fn publish_uncounted(&self, inner: &Inner) {
        self.metrics.limiter().set_uncounted_calls(inner.uncounted);
    }

    /// The backup peer for a call from its envelope's `bak`, or `None` when no
    /// replicating store is wired / the call has no topology / `bak` is empty /
    /// the call has no backup for life (ADR-0055 §3). This is the resolver the
    /// write-side policy ([`ReplicationPlan`]) needs: the proxy already signed
    /// the backup into the `w_bak` cookie and the b2bua stamped it onto
    /// `topology.bak` at INVITE time, so the b2bua never recomputes HRW — it
    /// just echoes `w_bak`.
    pub(crate) fn backup_of(&self, call_ref: &str) -> Option<String> {
        let repl = self.repl_store()?;
        let bak = {
            let inner = self.locked();
            let topology = inner.topologies.get(call_ref);
            topology.map(|t| t.bak.clone()).filter(|bak| !bak.is_empty())
        }?;
        (!repl.unbacked(call_ref)).then_some(bak)
    }

    /// Resolve the store target + propagate opts for `call_ref`, on a wired
    /// node only (the callers gate on `repl_store` first). When a backup is
    /// resolvable (non-empty `topology.bak`) this is the write-side policy
    /// ([`ReplicationPlan`]) — Forward when we own the ref, Reverse
    /// (acting-backup) when the ref names a crashed peer. Otherwise it is the
    /// local-only path: `(partition_of, PutOpts::default())`, no peer → the
    /// replicating store makes NO changelog bump.
    fn store_target(&self, call_ref: &str) -> (PartitionRole, String, PutOpts) {
        match self.backup_of(call_ref) {
            Some(bak) => {
                let plan =
                    ReplicationPlan::resolve(&self.self_ordinal, call_ref, &|_| Some(bak.clone()));
                (plan.role, plan.primary.clone(), plan.put_opts())
            }
            None => {
                let (role, primary) = partition_of(&self.self_ordinal, call_ref);
                (role, primary, PutOpts::default())
            }
        }
    }

    /// Drop a call from memory, and from the store when a replication store is
    /// wired (its txns/queue are torn down by the router's `RemoveCall`
    /// interpreter step, not here). The propagated delete states whether the
    /// resident copy had answered the caller (ADR-0031 D3):
    /// the last flush, compacted into the delete, no longer carries it. With no
    /// resident copy the store is left alone: the delete would name no call, and
    /// what the store holds for the ref may be another one.
    pub fn remove(&self, call_ref: &str) {
        // Resolve the replication target BEFORE evicting the in-memory call (the
        // topology lookup `backup_of` needs the call still present).
        let target = self.repl.as_ref().map(|repl| (self.store_target(call_ref), repl));

        // The call map first, the stamp last: a panic between them leaves only
        // derived entries the sweep reconciles.
        let mut inner = self.locked();
        let removed = inner.calls.remove(call_ref);
        inner.topologies.remove(call_ref);
        inner.takeover.remove(call_ref);
        let keys = inner.indexed.remove(call_ref).unwrap_or_default();
        inner.unindex(call_ref, &keys);
        let mut answered = false;
        let mut incarnation = None;
        if let Some(was) = removed {
            inner.restate_uncounted(was.limiter.runs_uncounted(), false);
            self.publish_uncounted(&inner);
            answered = call::helpers::caller_answered(&was);
            incarnation = Some(was.incarnation().to_string());
        }
        inner.locks.remove(call_ref);
        inner.touched.remove(call_ref);
        inner.shrink_idle();
        drop(inner);

        if let (Some(((role, primary, mut opts), repl)), Some(incarnation)) = (target, incarnation)
        {
            // The delete names the call it removes: its tombstone buries that
            // call, not a later one born on the same ref.
            opts.incarnation = Some(incarnation);
            let _ = repl.write_delete(role, &primary, call_ref, &keys, answered, &opts);
        }
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
        let mut inner = self.locked();
        let was = inner.calls.remove(call_ref);
        inner.topologies.remove(call_ref);
        let present = was.is_some();
        if let Some(was) = was {
            inner.restate_uncounted(was.limiter.runs_uncounted(), false);
            self.publish_uncounted(&inner);
        }
        if let Some(keys) = inner.indexed.remove(call_ref) {
            inner.unindex(call_ref, &keys);
        }
        inner.locks.remove(call_ref);
        inner.takeover.remove(call_ref);
        inner.touched.remove(call_ref);
        inner.shrink_idle();
        present
    }

    /// Release the ephemeral per-call state an **orphan-reject** created — the
    /// 481 path for an in-dialog request that resolved to `call_ref` but hydrated
    /// **no** live call (a failed-over BYE for a dialog that was never reclaimed,
    /// or a late in-dialog request after teardown). [`process`] acquired the
    /// per-call [`lock`](Self::lock) — and the router spun up a per-call dispatch
    /// queue (one `bump_creation`) — yet there is nothing to [`remove`](Self::remove):
    /// the call was never in the map. Drop the artifact this leaves behind —
    /// the `locks`-map entry — with **no** store
    /// mutation (unlike `remove`, which would reverse-propagate a *spurious
    /// delete* for a call we never held).
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
        let mut inner = self.locked();
        inner.locks.remove(call_ref);
        inner.shrink_idle();
    }

    /// **Active-reclaim reactive read-path** (ADR-0011 X11): decode a single
    /// reclaimable call from this node's `pri:{self}` partition — the flip-race
    /// straggler an acting-backup reverse-flushed *after* the bulk sweep, or the
    /// call an in-dialog request reaches before the sweep does. A backup-role
    /// ref is a [`ReplicaMiss::WrongRole`].
    ///
    /// An expired body reads as absent: it is a genuinely-dead own call (its
    /// active-replica TTL = `reboot_budget` elapsed without a refresh), so it is
    /// NOT re-served; the periodic reap evicts it. The reverse-flush reconcile
    /// path wants the opposite (see [`peek_reclaimable_raw`]).
    /// Returns the decoded call plus its persisted clock-skew offset (`0` when
    /// none) so the reclaim re-anchors its instants before re-arming its timers
    /// (ADR-0055 §9).
    ///
    /// [`peek_reclaimable_raw`]: Self::peek_reclaimable_raw
    pub async fn peek_reclaimable(&self, call_ref: &str) -> Result<(Enveloped, i64), ReplicaMiss> {
        self.read_partition(PartitionRole::Primary, call_ref).await
    }

    /// The skew offset held beside `call_ref`'s body (ADR-0055 §9), else `0`.
    pub fn held_skew_offset_ms(&self, call_ref: &str) -> i64 {
        self.repl_store().and_then(|r| r.skew_offset_ms(call_ref)).unwrap_or(0)
    }

    /// Decode a replica body this node did not store (a reverse flush the
    /// `(p,b)` gate refused), so the router can read the call it carries.
    pub fn decode_body(&self, body: &[u8]) -> Option<Enveloped> {
        crate::repl::decode_peer_body(&self.codec, &self.metrics, body)
    }

    /// Like [`peek_reclaimable`](Self::peek_reclaimable) but it reads an expired
    /// body too (`peek_body_raw`): a reverse-flushed *terminal* the live primary is
    /// about to fold is folded whatever its TTL, so the reconcile writes its CDR.
    /// Used by the reverse-flush reconcile to classify the body; a non-terminal one
    /// is then materialised through the TTL-gated read. Returns the decoded call
    /// plus its persisted receive-time clock-skew offset (`0` when none).
    pub async fn peek_reclaimable_raw(&self, call_ref: &str) -> Option<(Enveloped, i64)> {
        let repl = self.repl_store()?;
        let (role, primary) = partition_of(&self.self_ordinal, call_ref);
        if role != PartitionRole::Primary {
            return None;
        }
        let body = repl.peek_body_raw(role, &primary, call_ref).await?;
        let copy = envelope::decode(&self.codec, &body).ok()?;
        let skew = repl.skew_offset_ms(call_ref).unwrap_or(0);
        Some((copy, skew))
    }

    /// The one narrow partition read behind [`peek_replica`](Self::peek_replica)
    /// and [`peek_reclaimable`](Self::peek_reclaimable): the TTL-gated `get_call`
    /// of `call_ref` in the partition this node holds for it, which must be
    /// `required`. Every miss is typed so the router can tell a ref this node
    /// plays the other role for from a body that is gone or unreadable.
    async fn read_partition(
        &self,
        required: PartitionRole,
        call_ref: &str,
    ) -> Result<(Enveloped, i64), ReplicaMiss> {
        let repl = self.repl_store().ok_or(ReplicaMiss::Absent)?;
        let (role, primary) = partition_of(&self.self_ordinal, call_ref);
        if role != required {
            return Err(ReplicaMiss::WrongRole);
        }
        let body = repl
            .get_call(role, &primary, call_ref)
            .await
            .ok()
            .flatten()
            .ok_or(ReplicaMiss::Absent)?;
        let copy = envelope::decode(&self.codec, &body).map_err(|_| ReplicaMiss::Undecodable)?;
        let skew = repl.skew_offset_ms(call_ref).unwrap_or(0);
        Ok((copy, skew))
    }

    /// Encode + write the call to the store (replication path), its change-log
    /// entry moved before this returns.
    ///
    /// A no-op, allocation-free, when no replicating store is wired. Otherwise,
    /// when the call has a backup, the put rides the write-side policy
    /// (`call_gen` = the envelope's `gen`, peer = the backup, direction
    /// Forward/Reverse) and the replicating store moves its changelog entry for
    /// that peer; with no backup it is the `PutOpts::default()` (no
    /// propagation) path.
    pub fn flush(&self, call: &Call) {
        self.flush_with_ttl(call, self.replicated_ttl_ms);
    }

    /// Like [`flush`](Self::flush) but stamps an explicit body TTL. The Model-Y
    /// **deferred-terminal** flush uses a SHORT grace here (not the 1 h active
    /// backstop): a `Terminating`/`Terminated` `bak:` Element a backup deferred is
    /// the backup-durable-fallback's alive-timer — if the primary never reconciles
    /// it (and forward-deletes it) within the grace, the reap discharges it. Active
    /// replicas keep the long backstop (refreshed every keepalive), so a held call's
    /// replica never expires mid-call.
    pub fn flush_with_ttl(&self, call: &Call, ttl_ms: i64) {
        let Some(repl) = self.repl.as_ref() else {
            return;
        };
        let indexes = call_index_keys(call);
        // The authoritative envelope is the store's (bumped by `update`).
        let stored = self.topology(&call.call_ref);
        let (call_gen, call_bgen) = stored.as_ref().map(|t| (t.gen, t.bak_gen)).unwrap_or((0, 0));
        let (role, primary, mut opts) = self.store_target(&call.call_ref);
        let Some(body) = flush_body::encode(&self.codec, &self.metrics, call, &stored) else {
            return self.abandon_backup(&opts, &call.call_ref);
        };
        opts.incarnation = Some(call.incarnation().to_string());
        // Observability: a propagating flush is one whose call carries a backup
        // peer (topology.bak from the proxy cookie). Rising on the PRIMARY proves
        // the b2bua is attempting replication — distinguishing a cookie/topology
        // gap (stays 0) from a downstream changelog/puller delivery gap.
        if self.backup_of(&call.call_ref).is_some() {
            self.metrics.bump_repl_flush_propagated();
        }
        let _ = repl.write_put(
            role,
            &primary,
            &call.call_ref,
            body,
            &indexes,
            ttl_ms,
            call_gen,
            call_bgen,
            &opts,
        );
    }

    /// Resolve a message that carries no `callRef` of ours to a call from
    /// the in-memory index only (the sync path the dispatcher's route key
    /// needs), probing in [`IndexLookup::probes`] order. The hit names the
    /// namespace that matched.
    pub fn resolve_from_sip_key_sync(&self, lookup: IndexLookup<'_>) -> Option<IndexHit> {
        let mut key = lookup.key_buffer();
        let inner = self.locked();
        for probe in lookup.probes() {
            match *probe {
                Probe::Resolve(kind) => {
                    lookup.write_key(kind, &mut key);
                    if let Some(r) = inner.sip_index.get(key.as_str()) {
                        return Some(IndexHit { call_ref: r.clone(), kind });
                    }
                }
                Probe::Refuse(kind) => {
                    lookup.write_key(kind, &mut key);
                    if inner.sip_index.contains_key(key.as_str()) {
                        return None;
                    }
                }
            }
        }
        None
    }

    /// Acquire the per-callRef serialization lock (held across a handler run).
    pub async fn lock(&self, call_ref: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut inner = self.locked();
            inner
                .locks
                .entry(call_ref.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    pub fn active_count(&self) -> usize {
        self.locked().calls.len()
    }

    /// The number of live per-call serialization locks. Should track
    /// [`active_count`](Self::active_count); a gap is a **lock leak** — the
    /// orphan-reject path that forgot to [`discard_orphan`](Self::discard_orphan)
    /// (the `b2bua_store_locks` − `b2bua_store_calls` gap). Test/observability.
    pub fn lock_count(&self) -> usize {
        self.locked().locks.len()
    }

    /// The number of last-touched ledger entries. Mirrors `calls` by
    /// construction; a residue after teardown is a stamp leak (the harness
    /// `assert_fully_reaped` 4th invariant — ADR-0020).
    pub fn touched_count(&self) -> usize {
        self.locked().touched.len()
    }

    /// The number of live setup-CANCEL marks. Cleared by every release of the
    /// call that no waiting INVITE outlives; a residue after teardown is a
    /// mark leak (the harness `assert_fully_reaped` 5th invariant).
    pub fn setup_cancelled_count(&self) -> usize {
        self.locked().setup_cancelled.values().map(Vec::len).sum()
    }

    /// Push the store's map lengths into the memory-attribution gauges (one
    /// brief lock). `calls.len()` is the TRUE live call-map size — compare to
    /// `b2bua_active_calls` (creations - removals): a divergence is a store-side
    /// leak the counter pair can't see. The sibling maps (`sip_index`,
    /// `indexed`, `locks`, `takeover`) should track `calls`; one that grows
    /// while it stays flat names the leaking map. Sampled periodically by the
    /// runner — not on the hot path.
    pub fn sample_store_gauges(&self) {
        let inner = self.locked();
        self.metrics.set_store_gauges(
            inner.calls.len() as u64,
            inner.sip_index.len() as u64,
            inner.indexed.len() as u64,
            inner.locks.len() as u64,
            inner.takeover.len() as u64,
            inner.touched.len() as u64,
        );
        // State-machine cursor census (ADR-0016): how many live calls
        // rest at each (machine,state) cursor. Computed under the same brief lock
        // so the distribution is consistent with the call-map size above, then
        // pushed to the `b2bua_sm_cursors` gauge.
        //
        // Per-call Vec census (memory leak localisation): summed in the SAME pass
        // — the count-gauges above bound the map sizes, these bound the bytes
        // held INSIDE each call. The Vec whose sum/store_calls ratio climbs while
        // every count is flat is the leak (a held OPTIONS-hold/re-INVITE dialog's
        // per-event tail never pruned till terminal).
        let mut census: BTreeMap<(String, String), u64> = BTreeMap::new();
        let (
            mut cdr,
            mut pend,
            mut pend_max,
            mut dialogs,
            mut rset,
            mut timers,
            mut tagmap,
            mut blegs,
        ) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
        for call in inner.calls.values() {
            for (machine, state) in &call.sm_cursors {
                *census
                    .entry((machine.as_str().to_string(), state.as_str().to_string()))
                    .or_insert(0) += 1;
            }
            cdr += call.cdr_events.len() as u64;
            timers += call.timers.len() as u64;
            tagmap += call.a_leg.dialogs.iter().map(|d| d.fronts.len() as u64).sum::<u64>();
            blegs += call.b_legs.len() as u64;
            let mut call_pend = 0u64;
            for leg in std::iter::once(&call.a_leg).chain(call.b_legs.iter()) {
                dialogs += leg.dialogs.len() as u64;
                for d in &leg.dialogs {
                    rset += d.route_set.len() as u64;
                    call_pend += call::helpers::records_of(d).len() as u64;
                }
            }
            pend += call_pend;
            pend_max = pend_max.max(call_pend);
        }
        self.metrics.set_sm_cursor_census(census);
        self.metrics.set_call_census(cdr, pend, pend_max, dialogs, rset, timers, tagmap, blegs);
    }

    /// Recompute and apply a call's routing index, dropping any stale keys.
    fn reindex(inner: &mut Inner, call: &Call) {
        let call_ref = &call.call_ref;
        if let Some(old) = inner.indexed.remove(call_ref) {
            inner.unindex(call_ref, &old);
        }
        let keys = call_index_keys(call);
        for k in &keys {
            inner.sip_index.insert(k.clone(), call_ref.clone());
        }
        inner.indexed.insert(call_ref.clone(), keys);
    }
}
