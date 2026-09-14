//! [`ReplicatingCallStore`] — the HA [`CallStore`] (ADR-0011 X3/X8). It wraps an
//! [`InMemoryCallStore`] for body/index storage, owns the [`Changelog`], honours
//! the HA params the in-memory impl no-ops (`peer`/`direction`/`call_gen`/`ttl`),
//! and **atomically bumps the changelog** on every mutation.
//!
//! ## peer / direction → changelog / partition
//! The mutation's target peer comes from `opts.peer` (the node that will pull);
//! `opts.direction` picks the partition tag carried on the frame:
//! - [`Forward`](PropagateDirection::Forward) → [`Partition::Bak`]: this node is
//!   the primary, the peer backs it up.
//! - [`Reverse`](PropagateDirection::Reverse) → [`Partition::Pri`]: this node is
//!   the acting-backup, the peer is the reclaiming primary.
//!
//! `opts.peer == None` is the non-HA path: store the body, make **no** bump.
//!
//! ## TTL
//! Bodies carry an absolute `expiry_at_ms`; expired bodies are **lazily evicted
//! on access** and dropped wholesale by [`reap`](ReplicatingCallStore::reap)
//! after the clock advances — no background task, no `DelayQueue` aliasing
//! (CLAUDE.md timer hazard).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use repl_net::frame::{Op, Partition};
use sip_clock::Clock;

use super::changelog::{BodySource, Changelog, RefMeta};
use crate::store::{
    CallStore, InMemoryCallStore, PartitionRole, PropagateDirection, PutOpts, StoreError,
};

/// Backstop body-TTL (ms) applied to a replicated call stored with a non-positive
/// `ttl_ms`. It bounds a ghost — a replica whose `delete` was missed during a
/// disconnect and can no longer be re-delivered (the peer log was dropped, or a
/// `ResetToBootstrap` was itself lost) — to at most this lifetime, instead of
/// lingering forever. Must comfortably exceed the live dialog-refresh cadence so
/// a healthy call is refreshed (re-`put`) before the backstop bites. One hour.
pub const DEFAULT_REPLICATED_TTL_MS: i64 = 3_600_000;

/// Per-callRef side metadata kept in lockstep with the body so the drain can
/// fill a `Frame::Data` without touching the typed call map.
#[derive(Clone, Debug)]
struct CallMeta {
    meta: RefMeta,
    /// Where the body lives in the backing keyspace (so `reap` can delete it).
    role: PartitionRole,
    primary: String,
    /// The call's **backup** ordinal (`topology.bak`), captured from the forward
    /// flush (`direction == Forward` ⇒ `opts.peer` IS the backup). Lets the
    /// **Backup**-flow bootstrap scan `pri:{self}` filtered to a given backup
    /// (ADR-0014 Option B). `None` for replicas we hold for others (`bak:` side).
    backup: Option<String>,
    /// Absolute body-expiry deadline (lazy TTL); `None` when `ttl_ms <= 0`.
    expiry_at_ms: Option<i64>,
    /// Receive-time **wall-clock skew offset** `receiver_now_ms − origin_now_ms`
    /// (clock-skew hardening), computed on an inbound replica `Put` from
    /// `PutOpts.origin_now_ms`. A later failover/reclaim re-anchors the call's
    /// ABSOLUTE `TimerEntry.fire_at` deadlines (minted on the ORIGIN node's clock)
    /// by adding this offset, bounding restore skew to ~replication latency. `None`
    /// on a locally-originated write (no cross-node skew to correct). The offset
    /// includes one transit latency — acceptable at in-cluster ms scale.
    skew_offset_ms: Option<i64>,
    /// Whether a forward flush has ever shown this Element an ANSWERED call —
    /// the authority's own view of it, taken or refused. Set by the forward
    /// apply path alone; every other write (an acting backup's own mutation, an
    /// adoption flush) carries it over, so it keeps naming what the AUTHORITY
    /// published and not what this node did afterwards. Monotone, like the fact
    /// it records: an answered call never un-answers. The `Delete` guard reads
    /// it — an authority that never published the answer is ending a call it does
    /// not know happened (ADR-0031 D3).
    authority_answered: bool,
}

/// A [`CallStore`] that replicates mutations through an in-memory backing store
/// + a per-peer compacted [`Changelog`]. Clone-cheap (Arcs); share the handle.
/// How long a deleted `call_ref` rejects re-creating `Put`s (the apply-side
/// resurrection guard). A discharge deletes the call and propagates the delete,
/// but a peer's late reverse-flush (e.g. a backup finishing its deferred teardown
/// just after the primary discharged the reclaimed copy — `FixCallTerminateOnBackup`
/// C7/C10) would otherwise re-create the body via the Reverse "no local copy →
/// accept" rule and trigger a SECOND discharge. The window need only outlive
/// replication latency + the served call's residual timers (Timer F ~32 s) + a
/// reboot; 5 min is comfortably past that and bounds the set to `delete_rate × 5min`.
const RESURRECTION_TOMBSTONE_MS: i64 = 300_000;

#[derive(Clone)]
pub struct ReplicatingCallStore {
    inner: Arc<InMemoryCallStore>,
    changelog: Changelog,
    clock: Clock,
    /// `callRef → CallMeta`, updated atomically with the body.
    meta: Arc<Mutex<HashMap<String, CallMeta>>>,
    /// `callRef → deleted_at_ms`: the apply-side resurrection guard. A `Put` for a
    /// ref deleted within [`RESURRECTION_TOMBSTONE_MS`] is rejected so a late
    /// reverse-flush cannot re-create a just-discharged call (delete-wins, extended
    /// from the replica to the apply path). Pruned in [`reap`](Self::reap).
    tombstones: Arc<Mutex<HashMap<String, i64>>>,
    /// Backstop TTL applied when a call is stored with `ttl_ms <= 0`
    /// ([`DEFAULT_REPLICATED_TTL_MS`] by default; tests inject a short value).
    default_ttl_ms: i64,
}

impl ReplicatingCallStore {
    /// Build over a fresh in-memory backing store under incarnation `gen`.
    pub fn new(gen: u64, clock: Clock) -> Self {
        Self::with_changelog(Changelog::new(gen, clock.clone()), clock)
    }

    /// Build over a caller-supplied [`Changelog`] (tests inject short TTLs).
    pub fn with_changelog(changelog: Changelog, clock: Clock) -> Self {
        Self {
            inner: Arc::new(InMemoryCallStore::new()),
            changelog,
            clock,
            meta: Arc::new(Mutex::new(HashMap::new())),
            tombstones: Arc::new(Mutex::new(HashMap::new())),
            default_ttl_ms: DEFAULT_REPLICATED_TTL_MS,
        }
    }

    /// `(bodies, indexes, meta, tombstones)` map lengths — leak-localisation
    /// gauges. `indexes` outgrowing `bodies` is a stranded-`idx:*` leak (put_call
    /// is insert-only); `tombstones` outgrowing the 300 s delete window is a
    /// resurrection-guard prune gap.
    pub fn map_lens(&self) -> (usize, usize, usize, usize) {
        let (bodies, indexes) = self.inner.lens();
        let meta = self.meta.lock().unwrap().len();
        let tomb = self.tombstones.lock().unwrap().len();
        (bodies, indexes, meta, tomb)
    }

    /// Override the backstop TTL applied to calls stored with `ttl_ms <= 0`
    /// (tests inject a short value to exercise the missed-delete self-eviction).
    pub fn with_default_ttl_ms(mut self, default_ttl_ms: i64) -> Self {
        self.default_ttl_ms = default_ttl_ms;
        self
    }

    /// The absolute body-expiry for a call stored with `ttl_ms`, applying the
    /// backstop when `ttl_ms <= 0`. `None` only if the backstop is also disabled.
    fn expiry_for(&self, now: i64, ttl_ms: i64) -> Option<i64> {
        let effective = if ttl_ms > 0 { ttl_ms } else { self.default_ttl_ms };
        if effective > 0 {
            Some(now + effective)
        } else {
            None
        }
    }

    /// The owned changelog (S5's server loop subscribes/drains through this).
    pub fn changelog(&self) -> &Changelog {
        &self.changelog
    }

    /// The full `(p,b)` version vector stored for a callRef, or `None` if absent
    /// / expired. The S5 puller reads this to drive the ADR-0014 asymmetric apply
    /// rule (a reverse-flush applies iff `p_in == p_cur && b_in > b_cur`; a
    /// forward update applies always; deletes apply unconditionally) and as the
    /// presence probe on the Delete path. The `(role, primary)` args are accepted
    /// for a uniform seam with the other store methods; the per-ref metadata is
    /// keyed by callRef alone, so they are not needed to look the version up.
    pub fn current_cv(
        &self,
        _role: PartitionRole,
        _primary: &str,
        call_ref: &str,
    ) -> Option<(i64, i64)> {
        if self.is_expired(call_ref) {
            return None;
        }
        self.meta.lock().unwrap().get(call_ref).map(|m| (m.meta.call_gen, m.meta.call_bgen))
    }

    /// The persisted receive-time clock-skew offset for a callRef
    /// (`receiver_now_ms − origin_now_ms`, clock-skew hardening), or `None` when
    /// the ref is absent / was written locally (no cross-node skew). The router's
    /// failover/reclaim hydration reads this to re-anchor the call's absolute
    /// timer deadlines before re-arming them. Pure read (one brief lock).
    pub fn skew_offset_ms(&self, call_ref: &str) -> Option<i64> {
        self.meta.lock().unwrap().get(call_ref).and_then(|m| m.skew_offset_ms)
    }

    /// Snapshot the LIVE callRef KEYS stored in `(role, primary)` under a BRIEF
    /// lock (Decision 3 / X4). Bootstrap uses this to copy the `bak:{primary}`
    /// keyset, drop the lock, then read each body lazily per batch — so a
    /// slow/crashing puller never holds the call-map lock across the socket.
    ///
    /// Expired-but-not-yet-reaped refs are filtered out (the body would read
    /// `None` anyway). Pure read; no body touched.
    pub fn scan_call_refs(&self, role: PartitionRole, primary: &str) -> Vec<String> {
        let now = self.clock.now_ms();
        let meta = self.meta.lock().unwrap();
        meta.iter()
            .filter(|(_, m)| m.role == role && m.primary == primary)
            .filter(|(_, m)| !matches!(m.expiry_at_ms, Some(e) if now >= e))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Snapshot `(call_ref, role, primary)` for every Element whose alive-timer (the
    /// per-Element TTL, refreshed on each primary update) has **expired** at `now`.
    /// The Model-Y orphaned-deferred-terminal cleanup reads this to find a deferred
    /// terminal whose primary never reclaimed it (so the backup can release its
    /// limiter hold before the body is reaped); the plain [`reap`](Self::reap) then
    /// evicts whatever is left. Pure read; no body touched.
    pub fn expired_refs(&self, now_ms: i64) -> Vec<(String, PartitionRole, String)> {
        let meta = self.meta.lock().unwrap();
        meta.iter()
            .filter(|(_, m)| matches!(m.expiry_at_ms, Some(e) if now_ms >= e))
            .map(|(k, m)| (k.clone(), m.role, m.primary.clone()))
            .collect()
    }

    /// Read a body bypassing the lazy-TTL eviction. [`get_call`](Self::get_call)
    /// evicts (and returns `None` for) an expired body on access; the live
    /// reverse-flush reconcile reads through here ([`peek_reclaimable_raw`]), and the
    /// orphaned-deferred-terminal cleanup reads an EXPIRED body here, so neither is
    /// destroyed on read before it is handled. Pure read of the backing store; no
    /// eviction, no meta change.
    pub async fn peek_body_raw(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Option<Arc<[u8]>> {
        self.inner.get_call(role, primary, call_ref).await.ok().flatten()
    }

    /// Re-establish the denormalised `CallMeta.backup` for a `pri:{self}` call
    /// from the authoritative `topology.bak` (ADR-0014 #4). `backup` is normally
    /// captured on the Forward flush, but the **reboot-reclaim** hydration path
    /// imports the body through the peerless `PutOpts::default()`, leaving it
    /// `None` — which makes the call invisible to the **Backup**-flow bootstrap
    /// scan ([`scan_refs_backed_by`](BodySource::scan_refs_backed_by)) until the
    /// next keepalive re-flush. The reclaim materialisation calls this the moment
    /// it re-serves the call, closing that un-backed-up window. No-op if we hold
    /// no meta entry for the ref yet. Pure metadata; no body / changelog touched.
    pub fn reestablish_backup(&self, call_ref: &str, backup: &str) {
        if let Some(m) = self.meta.lock().unwrap().get_mut(call_ref) {
            m.backup = Some(backup.to_string());
        }
    }

    /// `(total, backup)` replica-metadata entry counts for the
    /// memory-attribution gauges: every callRef this node holds a replica body
    /// for, and the subset living in a **backup** partition (the resident backup
    /// bodies this node holds for its peers; a backup self-releases its *live*
    /// takeover copy on transaction completion but keeps the replica until its
    /// primary deletes it, ADR-0014). A `backup` count that climbs unbounded
    /// across failovers means deletes are not propagating / the reaper is behind.
    /// One brief lock; no body touched. Includes expired-but-not-yet-reaped
    /// entries — the reaper, not the gauge, prunes.
    pub fn meta_counts(&self) -> (u64, u64) {
        let meta = self.meta.lock().unwrap();
        let total = meta.len() as u64;
        let backup = meta.values().filter(|m| m.role == PartitionRole::Backup).count() as u64;
        (total, backup)
    }

    /// Map propagate direction → the partition tag the frame carries.
    fn partition_for(direction: Option<PropagateDirection>) -> Partition {
        match direction {
            // Acting-backup pushing reclaim data back to the primary.
            Some(PropagateDirection::Reverse) => Partition::Pri,
            // Primary → backup (the default).
            _ => Partition::Bak,
        }
    }

    /// Record that a forward flush has shown this Element the authority's own
    /// ANSWERED view of the call. Latches: the answer is durable on the body, so
    /// once the authority has published it, it has it. No-op for a ref we hold no
    /// meta for.
    pub fn note_authority_answer(&self, call_ref: &str) {
        if let Some(m) = self.meta.lock().unwrap().get_mut(call_ref) {
            m.authority_answered = true;
        }
    }

    /// Has the authority ever published an answered view of `call_ref`
    /// ([`note_authority_answer`])? `false` for an Element no forward flush has
    /// carried an answer to — including one no forward flush has reached at all.
    /// The `Delete` guard protects exactly that case: a call answered on this
    /// Element by somebody else (ADR-0031 D3).
    ///
    /// [`note_authority_answer`]: Self::note_authority_answer
    pub fn authority_answered(&self, call_ref: &str) -> bool {
        self.meta.lock().unwrap().get(call_ref).is_some_and(|m| m.authority_answered)
    }

    /// Is this call's body past its TTL? (lazy-eviction gate; pure read.)
    fn is_expired(&self, call_ref: &str) -> bool {
        let now = self.clock.now_ms();
        let meta = self.meta.lock().unwrap();
        matches!(meta.get(call_ref), Some(m) if matches!(m.expiry_at_ms, Some(e) if now >= e))
    }

    /// Lazily evict an expired body + meta on access; returns `true` if evicted.
    ///
    /// The body is deleted at the `(role, primary)` the META records, never the
    /// caller's: the meta is keyed by callRef alone, so a read at the other role
    /// would drop the meta and leave the body behind for ever — unreachable by
    /// every later expiry check and immortal.
    ///
    /// The meta's captured `indexes` ride the delete: an expired ghost must free
    /// its `idx:*` entries too (all per-call state released, CLAUDE.md) — the
    /// meta is removed in the same step, so this is the LAST moment the index
    /// keys are recoverable. Leaving them stranded both leaked the index map and
    /// let `resolve_from_replica_index` resolve a takeover to a dead callRef.
    async fn evict_if_expired(&self, call_ref: &str) -> bool {
        if !self.is_expired(call_ref) {
            return false;
        }
        let Some(gone) = self.meta.lock().unwrap().remove(call_ref) else {
            return false;
        };
        let _ = self
            .inner
            .delete_call(
                gone.role,
                &gone.primary,
                call_ref,
                &gone.meta.indexes,
                &PutOpts::default(),
            )
            .await;
        true
    }

    /// Evict every expired body + reap changelog tombstones/idle peers + prune
    /// stale resurrection tombstones. Call after advancing the clock (lazy TTL —
    /// deterministic, no background task).
    pub async fn reap(&self, now_ms: i64) {
        // Snapshot the expired (callRef, role, primary, indexes) tuples, drop the
        // lock, then delete each body — WITH its captured index keys, so the
        // ghost's `idx:*` entries are freed too (see `evict_if_expired`).
        let expired: Vec<(String, PartitionRole, String, Vec<String>)> = {
            let meta = self.meta.lock().unwrap();
            meta.iter()
                .filter(|(_, m)| matches!(m.expiry_at_ms, Some(e) if now_ms >= e))
                .map(|(k, m)| (k.clone(), m.role, m.primary.clone(), m.meta.indexes.clone()))
                .collect()
        };
        for (call_ref, role, primary, indexes) in &expired {
            self.meta.lock().unwrap().remove(call_ref);
            let _ = self
                .inner
                .delete_call(*role, primary, call_ref, indexes, &PutOpts::default())
                .await;
        }
        // Prune resurrection tombstones past their window — `put_call` only ever
        // reads an entry younger than `RESURRECTION_TOMBSTONE_MS`, so an older one
        // is dead weight. Without this the map grows one entry per terminated call
        // forever (the doc on `tombstones`/`delete_call` promised this prune but it
        // was missing): an unbounded leak that violates "all per-call state released
        // at call end" and bounds the set back to `delete_rate × RESURRECTION_TOMBSTONE_MS`.
        self.tombstones
            .lock()
            .unwrap()
            .retain(|_, &mut deleted_at| now_ms - deleted_at < RESURRECTION_TOMBSTONE_MS);
        self.changelog.reap(now_ms);
    }
}

#[async_trait]
impl CallStore for ReplicatingCallStore {
    async fn get_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Result<Option<Arc<[u8]>>, StoreError> {
        if self.evict_if_expired(call_ref).await {
            return Ok(None);
        }
        self.inner.get_call(role, primary, call_ref).await
    }

    async fn put_call(
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
        // Resurrection guard (apply-side delete-wins): reject a `Put` for a ref
        // deleted within the tombstone window — a late reverse-flush racing a
        // discharge (FixCallTerminateOnBackup C7/C10) must not re-create a
        // just-discharged call (which would trigger a SECOND discharge). A
        // tombstoned ref names a dead call (callRefs are unique), so no legitimate
        // Put is lost.
        // FIXME(repl): the tombstone refuses a deferred terminal for a ref this node
        // already discharged, losing the record; yield to a terminal it never saw.
        {
            let now = self.clock.now_ms();
            if let Some(&deleted_at) = self.tombstones.lock().unwrap().get(call_ref) {
                if now - deleted_at < RESURRECTION_TOMBSTONE_MS {
                    return Ok(());
                }
            }
        }
        // Store body (Arc wrapped once inside) + indexes.
        self.inner
            .put_call(role, primary, call_ref, body, indexes, ttl_ms, call_gen, call_bgen, opts)
            .await?;

        let now = self.clock.now_ms();
        // Apply the backstop TTL for ttl_ms <= 0 so a missed-delete replica still
        // self-evicts (the wire-carried `body_ttl_ms` keeps the original ttl_ms).
        let expiry_at_ms = self.expiry_for(now, ttl_ms);
        // Receive-time clock-skew offset (clock-skew hardening): the SAME `now`
        // reading that anchors `expiry_at_ms` minus the origin node's send-time
        // wall clock. Persisted so a later failover/reclaim re-anchors this call's
        // absolute timer deadlines. `None` on a locally-originated write.
        let skew_offset_ms = opts.origin_now_ms.map(|origin| now - origin);

        // Update per-ref metadata atomically (single critical section). A write
        // CARRIES OVER every field it does not itself carry: the backup ordinal,
        // the skew offset and the authority's view of the answer are facts about
        // the ref that only their own writer may change, and rebuilding the entry
        // from this write alone would silently clear each of them.
        {
            let mut meta = self.meta.lock().unwrap();
            let ref_meta =
                RefMeta { call_gen, call_bgen, body_ttl_ms: ttl_ms, indexes: indexes.to_vec() };
            // A Forward flush carries the backup ordinal as `opts.peer`.
            let flushed_backup = match (opts.direction, &opts.peer) {
                (Some(PropagateDirection::Forward), Some(p)) => Some(p.clone()),
                _ => None,
            };
            match meta.get_mut(call_ref) {
                Some(m) => {
                    m.meta = ref_meta;
                    m.role = role;
                    m.primary = primary.to_string();
                    m.expiry_at_ms = expiry_at_ms;
                    if flushed_backup.is_some() {
                        m.backup = flushed_backup;
                    }
                    if skew_offset_ms.is_some() {
                        m.skew_offset_ms = skew_offset_ms;
                    }
                }
                None => {
                    meta.insert(
                        call_ref.to_string(),
                        CallMeta {
                            meta: ref_meta,
                            role,
                            primary: primary.to_string(),
                            backup: flushed_backup,
                            expiry_at_ms,
                            skew_offset_ms,
                            // Until a forward flush shows one, no answer of the
                            // authority's own stands on this Element.
                            authority_answered: false,
                        },
                    );
                }
            }
        }

        // HA path only: non-blocking changelog bump for the pulling peer.
        if let Some(peer) = &opts.peer {
            let partition = Self::partition_for(opts.direction);
            self.changelog.bump(peer, call_ref, Op::Put, partition);
        }
        Ok(())
    }

    async fn delete_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        indexes: &[String],
        opts: &PutOpts,
    ) -> Result<(), StoreError> {
        self.inner.delete_call(role, primary, call_ref, indexes, opts).await?;
        self.meta.lock().unwrap().remove(call_ref);
        // Tombstone the ref so a late reverse-flush cannot resurrect it (see
        // `put_call`); pruned in `reap`.
        self.tombstones.lock().unwrap().insert(call_ref.to_string(), self.clock.now_ms());

        if let Some(peer) = &opts.peer {
            let partition = Self::partition_for(opts.direction);
            self.changelog.bump(peer, call_ref, Op::Delete, partition);
        }
        Ok(())
    }

    async fn get_index(&self, index_key: &str) -> Result<Option<String>, StoreError> {
        self.inner.get_index(index_key).await
    }

    async fn scan_calls(
        &self,
        role: PartitionRole,
        primary: &str,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        self.inner.scan_calls(role, primary).await
    }
}

/// The changelog drains bodies through this store. Reads live bodies + per-ref
/// metadata; both gated by lazy TTL eviction.
#[async_trait]
impl BodySource for ReplicatingCallStore {
    async fn read_body(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Option<Arc<[u8]>> {
        self.get_call(role, primary, call_ref).await.ok().flatten()
    }

    fn read_meta(&self, call_ref: &str) -> Option<RefMeta> {
        self.meta.lock().unwrap().get(call_ref).map(|m| m.meta.clone())
    }

    fn scan_refs(&self, role: PartitionRole, primary: &str) -> Vec<String> {
        self.scan_call_refs(role, primary)
    }

    /// Live `pri:{primary}` refs whose captured `backup == backup` — the
    /// **Backup**-flow bootstrap snapshot (ADR-0014 Option B). Expired-but-not-
    /// yet-reaped refs are filtered out (the body would read `None` anyway).
    fn scan_refs_backed_by(&self, primary: &str, backup: &str) -> Vec<String> {
        let now = self.clock.now_ms();
        let meta = self.meta.lock().unwrap();
        meta.iter()
            .filter(|(_, m)| m.role == PartitionRole::Primary && m.primary == primary)
            .filter(|(_, m)| m.backup.as_deref() == Some(backup))
            .filter(|(_, m)| !matches!(m.expiry_at_ms, Some(e) if now >= e))
            .map(|(k, _)| k.clone())
            .collect()
    }
}
