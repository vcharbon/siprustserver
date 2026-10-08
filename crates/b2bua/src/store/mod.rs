//! `CallState` — the in-memory call map + per-call serialization, over the
//! [`CallStore`] persistence/replication seam. Port of the load-bearing parts
//! of `src/call/CallState.ts` (ADR-0010).
//!
//! Live calls are held *typed* in `calls`; the store path encodes via
//! [`MsgpackCodec`] only for flush/replication. Routing uses the in-memory
//! `sip_index`; the store's `get_index` is the fallback.

mod call_store;
mod faults;
mod memory;
mod poison;
#[cfg(test)]
mod setup_cancel_tests;
#[cfg(test)]
mod spiral_index_tests;
mod terminate_writer;
#[cfg(test)]
mod unwired_tests;

pub use call_store::{
    partition_of, role_of, CallStore, PartitionRole, PropagateDirection, PutOpts, StoreError,
};
pub use faults::{FaultInjectingCallStore, StoreFaultPoint, StoreFaults};
pub use memory::InMemoryCallStore;
pub(crate) use poison::locked;
pub use poison::poisoned_lock_recoveries;
pub use terminate_writer::BufferedTerminateWriter;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use call::{
    call_index_keys, Call, CallBodyCodec, CallModelState, IndexHit, IndexLookup, MsgpackCodec,
    Probe,
};

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
    calls: HashMap<String, Box<Call>>,
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
    takeover: HashSet<String>,
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
        shrink_idle(&mut self.sip_index);
        shrink_idle(&mut self.indexed);
        shrink_idle(&mut self.locks);
        shrink_idle(&mut self.touched);
    }
}

/// The store write path of a wired node: the replicating store the write-side policy
/// targets and the buffered writer that drains to it. The two exist together
/// or not at all.
#[derive(Clone)]
struct Replication {
    store: Arc<ReplicatingCallStore>,
    writer: BufferedTerminateWriter,
}

/// The call store. Clone-cheap (one `Arc`); share across the stack.
#[derive(Clone)]
pub struct CallState {
    inner: Arc<Mutex<Inner>>,
    store: Arc<dyn CallStore>,
    /// The replication wiring, present only on a wired node. It is the one switch
    /// on the store write path: `None` and
    /// [`flush`](CallState::flush)/[`remove`](CallState::remove) touch the store
    /// not at all, whatever topology a call carries (the front proxy stamps a
    /// backup peer on every call whenever two workers are alive, and nothing
    /// reads what an unwired node would write); there is then no writer either.
    /// `Some` and a call with a non-empty `topology.bak` rides the write-side
    /// policy ([`ReplicationPlan`]); a call with no backup takes
    /// `PutOpts::default()`.
    repl: Option<Replication>,
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
    pub async fn load_owned(&self) -> Result<Vec<Call>, StoreError> {
        let bodies = self.store.scan_calls(PartitionRole::Primary, &self.self_ordinal).await?;
        Ok(bodies.iter().filter_map(|b| self.codec.decode(b).ok()).collect())
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

    /// Opt into replication: open the store write path through `writer`, and
    /// route the flush/remove of any call that carries a non-empty
    /// `topology.bak` through the write-side policy. Without this call the
    /// store is never written and no writer exists. `repl` MUST be the store
    /// `writer` drains to, so the changelog bump (keyed off the `PutOpts.peer`
    /// this path sets) fires.
    pub fn with_replication(
        mut self,
        repl: Arc<ReplicatingCallStore>,
        writer: BufferedTerminateWriter,
    ) -> Self {
        self.repl = Some(Replication { store: repl, writer });
        self
    }

    /// The replicating store, when replication is wired.
    fn repl_store(&self) -> Option<&Arc<ReplicatingCallStore>> {
        self.repl.as_ref().map(|r| &r.store)
    }

    /// The buffered writer the store write path submits to, when replication
    /// is wired, for the tests that assert what an unwired node constructs.
    #[cfg(test)]
    pub(crate) fn terminate_writer(&self) -> Option<&BufferedTerminateWriter> {
        self.repl.as_ref().map(|r| &r.writer)
    }

    /// Insert a freshly-created call + index it. Returns its `callRef`.
    pub fn create(&self, call: Call) -> String {
        let call_ref = call.call_ref.clone();
        let now_ms = self.clock.now_ms();
        let mut inner = self.inner.lock().unwrap();
        Self::reindex(&mut inner, &call);
        let is = call.limiter.runs_uncounted();
        let was = inner.calls.insert(call_ref.clone(), Box::new(call));
        inner.restate_uncounted(was.is_some_and(|c| c.limiter.runs_uncounted()), is);
        inner.touched.insert(call_ref.clone(), now_ms);
        self.publish_uncounted(&inner);
        call_ref
    }

    /// Refresh the **last-touched stamp** (ADR-0020 X4) — the call reaper's only
    /// liveness input. Called by `router::process` once per dispatched event
    /// that reached the handler (a wedged FIFO never reaches it, so its stamp
    /// freezes — exactly the staleness signal). Monotonic-max, no-op for a
    /// non-resident call (membership mirrors the live map).
    pub fn touch(&self, call_ref: &str, now_ms: i64) {
        let mut inner = self.inner.lock().unwrap();
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
        self.inner.lock().unwrap().touched.get(call_ref).copied()
    }

    /// Cheap residency check (no `Call` clone) — sweep-side strike pruning.
    pub fn contains(&self, call_ref: &str) -> bool {
        self.inner.lock().unwrap().calls.contains_key(call_ref)
    }

    /// Sweep input (ADR-0020 X3/X4): every live call whose last-touched stamp is
    /// older than `now_ms - idle_max_ms`, paired with the observed stamp (the
    /// verdict's confirm watermark) — **excluding acting-backup takeover
    /// copies** (self-release owns those, push-based on `CallQuiesced`; the
    /// reaper sweep never selects one). `bak:` Elements are structurally absent
    /// (they live in the replica store, not in `calls`). One pass under the
    /// inner lock.
    pub fn stale_candidates(&self, now_ms: i64, idle_max_ms: i64) -> Vec<(String, i64)> {
        let inner = self.inner.lock().unwrap();
        inner
            .touched
            .iter()
            .filter(|(call_ref, stamp)| {
                now_ms - **stamp > idle_max_ms && !inner.takeover.contains(*call_ref)
            })
            .map(|(call_ref, stamp)| (call_ref.clone(), *stamp))
            .collect()
    }

    /// Snapshot a call (clone) from memory, if present.
    pub fn peek(&self, call_ref: &str) -> Option<Call> {
        self.inner.lock().unwrap().calls.get(call_ref).map(|c| (**c).clone())
    }

    /// Whether the resident call `call_ref` is the incarnation `incarnation`
    /// (`Call::incarnation`), read without cloning it; `None` when no call is
    /// resident.
    pub fn is_incarnation(&self, call_ref: &str, incarnation: &str) -> Option<bool> {
        self.inner.lock().unwrap().calls.get(call_ref).map(|c| c.incarnation() == incarnation)
    }

    /// The lifecycle state of the resident call `call_ref`, read without
    /// cloning it.
    pub fn model_state(&self, call_ref: &str) -> Option<CallModelState> {
        self.inner.lock().unwrap().calls.get(call_ref).map(|c| c.state)
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
    pub async fn peek_replica(&self, call_ref: &str) -> Result<(Call, i64), ReplicaMiss> {
        self.read_partition(PartitionRole::Backup, call_ref).await
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
    /// what lets the asymmetric apply rule (see [`Puller`](crate::repl::Puller)) resolve
    /// concurrent primary+backup mutations, where a single shared counter would
    /// tie at equal generations and diverge. A brand-new call enters at
    /// `(1,0)` (stamped at INVITE time in [`crate::initial_invite`]). The bump is
    /// a no-op for non-proxied calls that carry no `topology`.
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
    /// A bump counts a write that changes the call; a quiet turn (a rung of this
    /// node's own ladder, a re-ACK — [`crate::effects::QuietTurn`]) goes through
    /// [`update_quiet`](Self::update_quiet) and moves no axis.
    pub fn update(&self, mut call: Call) {
        if let Some(t) = call.topology.as_mut() {
            match role_of(&self.self_ordinal, &call.call_ref) {
                PartitionRole::Primary => t.gen += 1,
                PartitionRole::Backup => t.bak_gen += 1,
            }
        }
        self.replace(call);
    }

    /// Replace the in-memory call for a quiet turn: the same replace-only rule
    /// as [`update`](Self::update) with no counter moved — the `(p,b)` the
    /// resident copy carries is the last write's, and so is what the next
    /// flush publishes.
    pub fn update_quiet(&self, call: Call) {
        self.replace(call);
    }

    fn replace(&self, call: Call) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.calls.contains_key(&call.call_ref) {
            return;
        }
        Self::reindex(&mut inner, &call);
        let was = inner.calls.get(&call.call_ref).is_some_and(|c| c.limiter.runs_uncounted());
        if was != call.limiter.runs_uncounted() {
            inner.restate_uncounted(was, call.limiter.runs_uncounted());
            self.publish_uncounted(&inner);
        }
        if let Some(slot) = inner.calls.get_mut(&call.call_ref) {
            **slot = call;
        }
    }

    /// Publish the resident calls that run uncounted.
    fn publish_uncounted(&self, inner: &Inner) {
        self.metrics.limiter().set_uncounted_calls(inner.uncounted);
    }

    /// Raise the resident copy's `(p,b)` to at least `(gen, bak_gen)` and return
    /// it, or `None` when nothing changed (not resident, not replicable, or
    /// already at or past both counters).
    ///
    /// **Not a mutation.** Adopting a version another owner published is a read
    /// this node records, so it carries none of [`update`](Self::update)'s
    /// authoritative bump: bumping our own axis for it would make two live
    /// owners raise each other in turn, one flush per round, and the two views
    /// would never come level (ADR-0031 D3).
    pub fn adopt_version(&self, call_ref: &str, gen: i64, bak_gen: i64) -> Option<Call> {
        let mut inner = self.inner.lock().unwrap();
        let call = inner.calls.get_mut(call_ref)?;
        let t = call.topology.as_mut()?;
        if t.gen >= gen && t.bak_gen >= bak_gen {
            return None;
        }
        t.gen = t.gen.max(gen);
        t.bak_gen = t.bak_gen.max(bak_gen);
        Some((**call).clone())
    }

    /// The backup peer for a call from its `CallTopology.bak`, or `None` when no
    /// replicating store is wired / the call has no topology / `bak` is empty.
    /// This is the resolver the write-side policy ([`ReplicationPlan`]) needs:
    /// the proxy already signed the backup into the `w_bak` cookie and the b2bua
    /// stamped it onto `topology.bak` at INVITE time (see [`crate::initial_invite`]),
    /// so the b2bua never recomputes HRW — it just echoes `w_bak`.
    fn backup_of(&self, call_ref: &str) -> Option<String> {
        self.repl_store()?;
        let inner = self.inner.lock().unwrap();
        inner
            .calls
            .get(call_ref)
            .and_then(|c| c.topology.as_ref())
            .map(|t| t.bak.clone())
            .filter(|bak| !bak.is_empty())
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
        let target = self.repl.as_ref().map(|r| (self.store_target(call_ref), &r.writer));

        let mut inner = self.inner.lock().unwrap();
        let keys = inner.indexed.remove(call_ref).unwrap_or_default();
        inner.unindex(call_ref, &keys);
        let mut answered = false;
        let mut incarnation = None;
        if let Some(was) = inner.calls.remove(call_ref) {
            inner.restate_uncounted(was.limiter.runs_uncounted(), false);
            self.publish_uncounted(&inner);
            answered = call::helpers::caller_answered(&was);
            incarnation = Some(was.incarnation().to_string());
        }
        inner.locks.remove(call_ref);
        inner.takeover.remove(call_ref);
        inner.touched.remove(call_ref);
        inner.shrink_idle();
        drop(inner);

        if let (Some(((role, primary, mut opts), writer)), Some(incarnation)) =
            (target, incarnation)
        {
            // The delete names the call it removes: its tombstone buries that
            // call, not a later one born on the same ref.
            opts.incarnation = Some(incarnation);
            writer.submit_delete(role, primary, call_ref.to_string(), keys, answered, opts);
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
        let mut inner = self.inner.lock().unwrap();
        let was = inner.calls.remove(call_ref);
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
        let mut inner = self.inner.lock().unwrap();
        inner.locks.remove(call_ref);
        inner.shrink_idle();
    }

    /// Mark `call_ref` as a live acting-backup **takeover copy** (ADR-0011 X11 /
    /// ADR-0014). Called by the router's `materialise` on a fresh takeover. The router
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

    /// Mark the initial INVITE of `call_ref` carrying CSeq `cseq` as
    /// CANCEL-finalized (the txn layer answered 200 + 487 and emitted
    /// `Cancelled`). Called by the router run loop at `Cancelled` ingress —
    /// BEFORE the event queues behind the initial-INVITE turn on the per-call
    /// FIFO — so that turn can see the caller is gone while the call model
    /// still reads `Active`. Idempotent.
    pub fn mark_setup_cancelled(&self, call_ref: &str, cseq: u32) {
        let mut inner = self.inner.lock().unwrap();
        match inner.setup_cancelled.get_mut(call_ref) {
            Some(marks) if marks.contains(&cseq) => {}
            Some(marks) => marks.push(cseq),
            None => {
                inner.setup_cancelled.insert(call_ref.to_string(), vec![cseq]);
            }
        }
    }

    /// Has the initial INVITE of `call_ref` carrying CSeq `cseq` been
    /// CANCELed (see [`mark_setup_cancelled`](Self::mark_setup_cancelled))?
    /// Read twice by that INVITE's turn, the caller already holding its 487:
    /// before the decision, a setup cancelled while the turn waited is born
    /// and asks no decision; at the decision-application seam, a route/reject
    /// landing on a call cancelled during the round trip is dropped.
    pub fn is_setup_cancelled(&self, call_ref: &str, cseq: u32) -> bool {
        self.inner.lock().unwrap().setup_cancelled.get(call_ref).is_some_and(|m| m.contains(&cseq))
    }

    /// Clear the setup-CANCEL mark of `call_ref`'s INVITE carrying CSeq
    /// `cseq`: its turn ended with no call of its own.
    pub fn clear_setup_cancelled(&self, call_ref: &str, cseq: u32) {
        self.retain_setup_cancelled(call_ref, |marked| marked != cseq);
    }

    /// Keep, of `call_ref`'s setup-CANCEL marks, those whose CSeq `waiting`
    /// says an INVITE still waits for its turn under: the router's release of
    /// a call clears the rest.
    pub fn retain_setup_cancelled(&self, call_ref: &str, waiting: impl Fn(u32) -> bool) {
        let mut inner = self.inner.lock().unwrap();
        let Some(marks) = inner.setup_cancelled.get_mut(call_ref) else { return };
        marks.retain(|cseq| waiting(*cseq));
        if marks.is_empty() {
            inner.setup_cancelled.remove(call_ref);
        }
    }

    /// **Active-reclaim bulk read-path** (ADR-0011 X11): scan this node's own
    /// `pri:{self}` partition from the replicating store and decode every
    /// reclaimable call. The router materialises each into the live map +
    /// re-arms its timers when it goes Ready (the bulk reclaim sweep that makes a
    /// rebooted primary actually re-*serve*, not just re-*store*). Empty when no
    /// replicating store is wired.
    /// Each entry pairs the decoded call with its persisted receive-time
    /// clock-skew offset (`0` when the record carries none) —
    /// the router re-anchors the call's absolute timer deadlines by this offset
    /// before re-arming them (clock-skew hardening).
    pub async fn reclaim_scan(&self) -> Vec<(Call, i64)> {
        let Some(repl) = self.repl_store() else {
            return Vec::new();
        };
        let bodies =
            repl.scan_calls(PartitionRole::Primary, &self.self_ordinal).await.unwrap_or_default();
        bodies
            .iter()
            .filter_map(|b| self.codec.decode(b).ok())
            .map(|c| {
                let skew = repl.skew_offset_ms(&c.call_ref).unwrap_or(0);
                (c, skew)
            })
            .collect()
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
    /// Returns the decoded call plus its persisted receive-time clock-skew offset
    /// (`0` when none) so the reclaim re-anchors its timers before re-arming them
    /// (clock-skew hardening).
    ///
    /// [`peek_reclaimable_raw`]: Self::peek_reclaimable_raw
    pub async fn peek_reclaimable(&self, call_ref: &str) -> Result<(Call, i64), ReplicaMiss> {
        self.read_partition(PartitionRole::Primary, call_ref).await
    }

    /// Decode a replica body this node did not store (a reverse flush the
    /// `(p,b)` gate refused), so the router can read the call it carries.
    pub fn decode_body(&self, body: &[u8]) -> Option<Call> {
        self.codec.decode(body).ok()
    }

    /// Like [`peek_reclaimable`](Self::peek_reclaimable) but it reads an expired
    /// body too (`peek_body_raw`): a reverse-flushed *terminal* the live primary is
    /// about to fold is folded whatever its TTL, so the reconcile writes its CDR.
    /// Used by the reverse-flush reconcile to classify the body; a non-terminal one
    /// is then materialised through the TTL-gated read. Returns the decoded call
    /// plus its persisted receive-time clock-skew offset (`0` when none).
    pub async fn peek_reclaimable_raw(&self, call_ref: &str) -> Option<(Call, i64)> {
        let repl = self.repl_store()?;
        let (role, primary) = partition_of(&self.self_ordinal, call_ref);
        if role != PartitionRole::Primary {
            return None;
        }
        let body = repl.peek_body_raw(role, &primary, call_ref).await?;
        let call = self.codec.decode(&body).ok()?;
        let skew = repl.skew_offset_ms(call_ref).unwrap_or(0);
        Some((call, skew))
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
    ) -> Result<(Call, i64), ReplicaMiss> {
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
        let call = self.codec.decode(&body).map_err(|_| ReplicaMiss::Undecodable)?;
        let skew = repl.skew_offset_ms(call_ref).unwrap_or(0);
        Ok((call, skew))
    }

    /// Physically evict every expired replica body, changelog tombstone and
    /// stale resurrection tombstone (the missed-delete ghost backstop,
    /// ADR-0014), and return the evicted bodies that are **deferred terminals**
    /// (`Terminating`/`Terminated`): a node served the call's end and deferred
    /// its discharge to a primary that never reconciled it inside the TTL. The
    /// router releases each one's limiter key and counts its CDR lost (the
    /// accepted double-failure); an expired non-terminal ghost is evicted
    /// silently. A read never evicts, so each expired body is returned by
    /// exactly one pass. A deferred terminal another call on its ref replaced
    /// on this backup is returned the same way
    /// ([`take_displaced`](crate::repl::ReplicatingCallStore::take_displaced)).
    /// Empty without a replicating store.
    pub async fn reap_replica(&self, now_ms: i64) -> Vec<Call> {
        let Some(repl) = self.repl_store() else {
            return Vec::new();
        };
        let mut owed = repl.reap(now_ms).await;
        owed.extend(repl.take_displaced());
        owed.iter()
            .filter_map(|body| self.codec.decode(body).ok())
            .filter(|call| {
                matches!(call.state, CallModelState::Terminating | CallModelState::Terminated)
            })
            .collect()
    }

    /// The one residency insert: put a materialised call into the live map +
    /// routing index iff it is not already resident. Returns `true` when just
    /// inserted — the router then re-arms its timers exactly once. Idempotent: a
    /// call already live (re-served, or never lost) is left untouched.
    ///
    /// A traced call opens this node's own root span here, after the residency
    /// check, so a span is opened only for a call this node goes on to serve
    /// (ADR-0026 §5). The idle clock starts at the insert, never at `created_at`
    /// (ADR-0020 X4): a hours-old failed-over or reclaimed long-hold call is
    /// fresh here, not reap-stale.
    ///
    /// The inserted call's limiter change counter moves to its next epoch
    /// (`CallLimiterState::enter_epoch`, ADR-0040 decision 8): the previous
    /// holder may have sent admits under numbers its last replicated write
    /// does not show.
    ///
    /// On a [`MaterialiseOrigin::Reclaim`] insert it also re-establishes the
    /// replica's denormalised backup from the call's authoritative
    /// `topology.bak` (ADR-0014 #4): the reboot-reclaim hydration imports the
    /// body peerless, leaving `CallMeta.backup == None` and the call invisible to
    /// its backup's bootstrap scan until the next keepalive re-flush. A takeover
    /// copy holds the `bak:` element itself and re-establishes nothing.
    pub fn materialize_if_absent(&self, mut call: Call, origin: MaterialiseOrigin) -> bool {
        let backup = call.topology.as_ref().map(|t| t.bak.clone()).filter(|b| !b.is_empty());
        let call_ref = call.call_ref.clone();
        let now_ms = self.clock.now_ms();
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.calls.contains_key(&call.call_ref) {
                return false;
            }
            crate::trace::adopt_replicated(&mut call, now_ms);
            call.limiter.enter_epoch();
            Self::reindex(&mut inner, &call);
            inner.touched.insert(call.call_ref.clone(), now_ms);
            inner.restate_uncounted(false, call.limiter.runs_uncounted());
            self.publish_uncounted(&inner);
            inner.calls.insert(call.call_ref.clone(), Box::new(call));
        }
        if origin == MaterialiseOrigin::Reclaim {
            if let (Some(repl), Some(backup)) = (self.repl_store(), backup) {
                repl.reestablish_backup(&call_ref, &backup);
            }
        }
        true
    }

    /// Encode + submit the call to the store (replication path; non-blocking).
    ///
    /// A no-op, allocation-free, when no replicating store is wired. Otherwise,
    /// when the call carries a non-empty `topology.bak`, the put rides the
    /// write-side policy (`call_gen = topology.gen`, peer = the backup, direction
    /// Forward/Reverse) and the `ReplicatingCallStore` the
    /// [`BufferedTerminateWriter`] drains to bumps its changelog for that peer;
    /// with no topology / no backup it is the `PutOpts::default()` (no
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
        let Some(writer) = self.repl.as_ref().map(|r| &r.writer) else {
            return;
        };
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
        // Embed the authoritative `(p,b)` in the BODY too, not just the frame meta.
        // `call` is the pre-`update` clone (its `topology` counters are one bump
        // stale), so a node that hydrates from this body would branch at the stale
        // `(p,b)` — invisible while a backup only ever takes over a CRASHED primary
        // (its meta is gone on reboot), but it desynced a reverse-flush against an
        // ALIVE primary (the misroute): the backup branched one `p` behind, so the
        // Reverse `(p,b)` apply rule rejected its flush and the live primary never
        // reconciled (FixCallTerminateOnBackup C2/C3/C11). Keep the body's embedded
        // version vector consistent with its `(p,b)`.
        let body = match call.topology.as_ref() {
            // Already consistent (the common path — `call` IS the authoritative
            // copy, e.g. a fresh flush) → encode it directly, no clone.
            Some(t) if t.gen == call_gen && t.bak_gen == call_bgen => self.codec.encode(call),
            // A pre-`update` clone whose counters are one bump stale → patch the
            // embedded `(p,b)` to the authoritative value before encoding.
            Some(_) => {
                let mut consistent = call.clone();
                if let Some(t) = consistent.topology.as_mut() {
                    t.gen = call_gen;
                    t.bak_gen = call_bgen;
                }
                self.codec.encode(&consistent)
            }
            None => self.codec.encode(call),
        };
        let (role, primary, mut opts) = self.store_target(&call.call_ref);
        opts.incarnation = Some(call.incarnation().to_string());
        // Observability: a propagating flush is one whose call carries a backup
        // peer (topology.bak from the proxy cookie). Rising on the PRIMARY proves
        // the b2bua is attempting replication — distinguishing a cookie/topology
        // gap (stays 0) from a downstream changelog/puller delivery gap.
        if self.backup_of(&call.call_ref).is_some() {
            self.metrics.bump_repl_flush_propagated();
        }
        writer.submit_put(
            role,
            primary,
            call.call_ref.clone(),
            body,
            indexes,
            ttl_ms,
            call_gen,
            call_bgen,
            opts,
        );
    }

    /// Resolve a message that carries no `callRef` of ours to a call from
    /// the in-memory index only (the sync path the dispatcher's route key
    /// needs), probing in [`IndexLookup::probes`] order. The hit names the
    /// namespace that matched.
    pub fn resolve_from_sip_key_sync(&self, lookup: IndexLookup<'_>) -> Option<IndexHit> {
        let mut key = lookup.key_buffer();
        let inner = self.inner.lock().unwrap();
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

    /// **Acting-backup dialog resolution.** The call of an in-dialog request or
    /// a CANCEL that states no `callRef` of ours, and the namespace that
    /// matched, read from the replicating store's SIP index (the puller's
    /// `put_call` writes `idx:<key>` → callRef) in the [`IndexLookup::probes`]
    /// order the live index uses. A request routed per RFC 3261 §12.2.1.1
    /// carries our Contact, `callRef` included, as its Request-URI, and a
    /// strict router's §16.4 rewrite restores it; only an element that
    /// rewrites the Request-URI leaves the call to be found by dialog identity.
    /// The router awaits this read when the live index
    /// ([`resolve_from_sip_key_sync`](Self::resolve_from_sip_key_sync)) misses,
    /// as it does on a backup that never served the call. `Ok(None)` with no
    /// replicating store or no replica; `Err` when a read failed, so the store
    /// cannot say whether the dialog exists.
    pub async fn resolve_from_replica_index(
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
        if let Some(hit) = &hit {
            self.metrics.bump_repl_takeover_resolved();
            self.note_takeover(&hit.call_ref, "resolved");
        }
        Ok(hit)
    }

    /// Fold one takeover event for `call_ref` into its dead primary's episode
    /// (ADR-0026 aggregation): the counter names what happened, the peer the
    /// `call_ref` encodes keys the episode.
    pub(crate) fn note_takeover(&self, call_ref: &str, counter: &'static str) {
        self.note_takeover_count(call_ref, counter, 1);
    }

    /// [`note_takeover`](Self::note_takeover) for a counter that moves by `n`
    /// at once — the transactions one materialisation seeded.
    pub(crate) fn note_takeover_count(&self, call_ref: &str, counter: &'static str, n: u64) {
        let (_, primary) = partition_of(&self.self_ordinal, call_ref);
        self.takeover_log.record(&primary, counter, n);
    }

    /// Fold an acting-backup **self-release** into the dead peer's takeover
    /// episode — the shedding that ends a takeover is part of the same story,
    /// so it never gets its own line.
    pub fn note_takeover_self_release(&self, call_ref: &str) {
        self.note_takeover(call_ref, "self_released");
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

    /// Every callRef this node holds live. The core's teardown reads it to
    /// release the per-call runtime state that is NOT in the store — the root
    /// spans (ADR-0026) — and tests read it as ground truth.
    pub fn live_call_refs(&self) -> Vec<String> {
        self.inner.lock().unwrap().calls.keys().cloned().collect()
    }

    /// The number of live per-call serialization locks. Should track
    /// [`active_count`](Self::active_count); a gap is a **lock leak** — the
    /// orphan-reject path that forgot to [`discard_orphan`](Self::discard_orphan)
    /// (the `b2bua_store_locks` − `b2bua_store_calls` gap). Test/observability.
    pub fn lock_count(&self) -> usize {
        self.inner.lock().unwrap().locks.len()
    }

    /// The number of last-touched ledger entries. Mirrors `calls` by
    /// construction; a residue after teardown is a stamp leak (the harness
    /// `assert_fully_reaped` 4th invariant — ADR-0020).
    pub fn touched_count(&self) -> usize {
        self.inner.lock().unwrap().touched.len()
    }

    /// The number of live setup-CANCEL marks. Cleared by every release of the
    /// call that no waiting INVITE outlives; a residue after teardown is a
    /// mark leak (the harness `assert_fully_reaped` 5th invariant).
    pub fn setup_cancelled_count(&self) -> usize {
        self.inner.lock().unwrap().setup_cancelled.values().map(Vec::len).sum()
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
            tagmap += call.tag_map.len() as u64;
            blegs += call.b_legs.len() as u64;
            let mut call_pend = 0u64;
            for leg in std::iter::once(&call.a_leg).chain(call.b_legs.iter()) {
                dialogs += leg.dialogs.len() as u64;
                call_pend += call::helpers::retired_count(leg) as u64;
                for d in &leg.dialogs {
                    rset += d.sip.route_set.len() as u64;
                    call_pend += d.ext.inbound_pending_requests.len() as u64;
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
