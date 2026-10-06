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
//! Bodies carry an absolute `expiry_at_ms`. An expired body reads as absent to
//! every read and is evicted only by [`reap`](ReplicatingCallStore::reap),
//! which hands back what it evicted so its caller can settle what an expired
//! body still owes (a deferred terminal's limiter release) — no background
//! task, no `DelayQueue` aliasing (CLAUDE.md timer hazard).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use repl_net::frame::{Partition, Watermark};
use sip_clock::Clock;

use super::changelog::{BodySource, Changelog, RefMeta};
use super::incarnation::is_another_call;
use super::resurrection::{Burial, ResurrectionTombstones};
use super::shed_marks::ShedMarks;
use crate::store::{
    locked, CallStore, InMemoryCallStore, PartitionRole, PropagateDirection, PutOpts, StoreError,
};

/// Backstop body-TTL (ms) applied to a replicated call stored with a non-positive
/// `ttl_ms`. It bounds a ghost — a replica whose `delete` was missed during a
/// disconnect and can no longer be re-delivered (the peer log was dropped, or a
/// `ResetToBootstrap` was itself lost) — to at most this lifetime, instead of
/// lingering forever. Must comfortably exceed the live dialog-refresh cadence so
/// a healthy call is refreshed (re-`put`) before the backstop bites. One hour.
pub const DEFAULT_REPLICATED_TTL_MS: i64 = 3_600_000;

/// Per-callRef side metadata kept in lockstep with the body so the drain can
/// fill a `Frame::Data` without touching the typed call map. Keyed by callRef
/// alone and rewritten by every local writer, so a `put_call` replaces only
/// the fields the write carries and carries over every other one (`backup`,
/// `skew_offset_ms`, `authority_answered`): each of those is written by its own
/// writer and would otherwise be cleared by the next write of another. A write
/// of another call incarnation carries nothing over: those facts were about
/// the call it replaces.
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
    /// Absolute body-expiry deadline; `None` when `ttl_ms <= 0`.
    expiry_at_ms: Option<i64>,
    /// Receive-time **wall-clock skew offset** `receiver_now_ms − origin_now_ms`
    /// (clock-skew hardening), computed on an origin-stamped write
    /// (`PutOpts.origin_now_ms`, the puller's apply), kept across every write
    /// that carries no origin, `None` for a record no origin-stamped write has
    /// reached. A later failover/reclaim re-anchors the call's ABSOLUTE
    /// `TimerEntry.fire_at` deadlines (minted on the ORIGIN node's clock) by
    /// adding this offset, bounding restore skew to ~replication latency. The
    /// offset includes one transit latency — acceptable at in-cluster ms scale.
    // FIXME(repl): a local write re-mints the deadlines in this node's frame, so the
    // carried offset re-anchors them wrongly on a later takeover; it should carry "none".
    skew_offset_ms: Option<i64>,
    /// Whether a forward flush has ever shown this Element an ANSWERED call —
    /// the authority's own view of it, taken or refused. Set by the forward
    /// apply path alone; every other write (an acting backup's own mutation, an
    /// adoption flush) carries it over, so it keeps naming what the AUTHORITY
    /// published and not what this node did afterwards. Monotone, like the fact
    /// it records: an answered call never un-answers. The `Delete` guard reads
    /// it, beside the answer a `Delete` itself states — an authority that never
    /// published the answer is ending a call it does not know happened (ADR-0031
    /// D3).
    authority_answered: bool,
}

/// A [`CallStore`] that replicates mutations through an in-memory backing store
/// + a per-peer compacted [`Changelog`]. Clone-cheap (Arcs); share the handle.
#[derive(Clone)]
pub struct ReplicatingCallStore {
    inner: Arc<InMemoryCallStore>,
    changelog: Changelog,
    clock: Clock,
    /// `callRef → CallMeta`, updated atomically with the body.
    meta: Arc<Mutex<HashMap<String, CallMeta>>>,
    /// Entries of `meta` whose role is [`PartitionRole::Backup`]. Changed only
    /// under the `meta` lock, in the same step as the entry, so it never
    /// drifts from a scan of `meta`.
    backups: Arc<AtomicU64>,
    /// Backup replicas left unstored at a backup ceiling (ADR-0037). A mark
    /// clears when the ref's body is stored or deleted, or expires.
    shed: Arc<Mutex<ShedMarks>>,
    /// The apply-side resurrection guard: a `Put` of a call deleted inside the
    /// tombstone window is ignored, so a late reverse-flush cannot re-create a
    /// just-discharged call (delete-wins, extended from the replica to the apply
    /// path). Pruned in [`reap`](Self::reap).
    tombstones: Arc<Mutex<ResurrectionTombstones>>,
    /// Backup bodies of calls a write of another call on their ref replaced,
    /// the latest write of each by incarnation: nobody settled them, so
    /// [`take_displaced`](Self::take_displaced) hands them to the replica reap.
    displaced: Arc<Mutex<HashMap<String, Arc<[u8]>>>>,
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
            backups: Arc::new(AtomicU64::new(0)),
            shed: Arc::new(Mutex::new(ShedMarks::default())),
            tombstones: Arc::new(Mutex::new(ResurrectionTombstones::default())),
            displaced: Arc::new(Mutex::new(HashMap::new())),
            default_ttl_ms: DEFAULT_REPLICATED_TTL_MS,
        }
    }

    /// `(bodies, indexes, meta, tombstones)` map lengths — leak-localisation
    /// gauges. `indexes` outgrowing `bodies` is a stranded-`idx:*` leak (put_call
    /// is insert-only); `tombstones` outgrowing the 300 s delete window is a
    /// resurrection-guard prune gap.
    pub fn map_lens(&self) -> (usize, usize, usize, usize) {
        let (bodies, indexes) = self.inner.lens();
        let meta = locked(&self.meta, "replica metadata").len();
        let tomb = locked(&self.tombstones, "resurrection tombstones").len();
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

    /// The owned changelog (the server loop subscribes/drains through this).
    pub fn changelog(&self) -> &Changelog {
        &self.changelog
    }

    /// The full `(p,b)` version vector stored for a callRef, or `None` if absent
    /// / expired. The puller reads this to drive the ADR-0014 asymmetric apply
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
        locked(&self.meta, "replica metadata")
            .get(call_ref)
            .map(|m| (m.meta.call_gen, m.meta.call_bgen))
    }

    /// The persisted receive-time clock-skew offset for a callRef
    /// (`receiver_now_ms − origin_now_ms`, clock-skew hardening), or `None` when
    /// the ref is absent / was written locally (no cross-node skew). The router's
    /// failover/reclaim hydration reads this to re-anchor the call's absolute
    /// timer deadlines before re-arming them. Pure read (one brief lock).
    pub fn skew_offset_ms(&self, call_ref: &str) -> Option<i64> {
        locked(&self.meta, "replica metadata").get(call_ref).and_then(|m| m.skew_offset_ms)
    }

    /// Snapshot the LIVE callRef KEYS stored in `(role, primary)` under a BRIEF
    /// lock (ADR-0011 X4). Bootstrap uses this to copy the `bak:{primary}`
    /// keyset, drop the lock, then read each body lazily per batch — so a
    /// slow/crashing puller never holds the call-map lock across the socket.
    ///
    /// Expired-but-not-yet-reaped refs are filtered out (the body would read
    /// `None` anyway). Pure read; no body touched.
    pub fn scan_call_refs(&self, role: PartitionRole, primary: &str) -> Vec<String> {
        let now = self.clock.now_ms();
        let meta = locked(&self.meta, "replica metadata");
        meta.iter()
            .filter(|(_, m)| m.role == role && m.primary == primary)
            .filter(|(_, m)| !matches!(m.expiry_at_ms, Some(e) if now >= e))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Read a body whatever its TTL. [`get_call`](Self::get_call) reads an
    /// expired body as absent; the live reverse-flush reconcile reads through
    /// here ([`CallState::peek_reclaimable_raw`]) so it can still fold an
    /// expired terminal. Pure read of the backing store; no meta change.
    ///
    /// [`CallState::peek_reclaimable_raw`]: crate::store::CallState::peek_reclaimable_raw
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
        if let Some(m) = locked(&self.meta, "replica metadata").get_mut(call_ref) {
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
        let total = locked(&self.meta, "replica metadata").len() as u64;
        (total, self.backup_held())
    }

    /// Backup replica bodies this node holds for its peers, exact, expired
    /// ones included until evicted. The puller reads it against the backup
    /// ceiling (ADR-0037).
    pub fn backup_held(&self) -> u64 {
        self.backups.load(Ordering::Relaxed)
    }

    /// Record that a replica of `call_ref` sent by incarnation `gen` of
    /// `primary` was left unstored, `floor` being the last position the flow
    /// could claim before it. The mark expires with the backstop a stored body
    /// of `ttl_ms` would have.
    pub fn note_shed(
        &self,
        call_ref: &str,
        primary: &str,
        gen: u64,
        floor: Watermark,
        ttl_ms: i64,
    ) {
        let expiry = self.expiry_for(self.clock.now_ms(), ttl_ms);
        locked(&self.shed, "shed marks").mark(call_ref, primary, gen, floor, expiry);
    }

    /// The highest position `primary`'s flow may report while it streams
    /// incarnation `gen`: the lowest floor of its standing shed marks, `None`
    /// when it has none. Marks an older incarnation sent are dropped first: a
    /// rebooted primary serves none of their calls.
    pub fn shed_floor(&self, primary: &str, gen: u64) -> Option<Watermark> {
        let mut shed = locked(&self.shed, "shed marks");
        shed.drop_older(primary, gen);
        shed.floor(primary)
    }

    /// Drop every shed mark of `primary`.
    pub fn clear_shed_of(&self, primary: &str) {
        locked(&self.shed, "shed marks").clear_primary(primary);
    }

    /// Whether a `Put` of `incarnation` of `call_ref` would resurrect a call
    /// deleted or replaced inside the tombstone window, so
    /// [`put_call`](CallStore::put_call) would ignore it.
    pub fn buries(&self, call_ref: &str, incarnation: Option<&str>) -> bool {
        let now = self.clock.now_ms();
        locked(&self.tombstones, "resurrection tombstones")
            .burial(call_ref, incarnation, now)
            .is_some()
    }

    /// Take the backup bodies of calls another call on their ref replaced
    /// here, each call's latest once: a deferred terminal among them is owed a
    /// settlement nobody else will make (the authority moved on to the new
    /// call without a delete this node saw), so the replica reap settles it as
    /// it settles an expired one. Calls written again after the take are
    /// handed again.
    pub fn take_displaced(&self) -> Vec<Arc<[u8]>> {
        locked(&self.displaced, "displaced bodies").drain().map(|(_, body)| body).collect()
    }

    /// The call incarnation of the body stored for a callRef, or `None` when
    /// absent / expired or stored by a write that named none.
    pub fn incarnation(&self, call_ref: &str) -> Option<String> {
        if self.is_expired(call_ref) {
            return None;
        }
        locked(&self.meta, "replica metadata")
            .get(call_ref)
            .and_then(|m| m.meta.incarnation.clone())
    }

    /// Standing shed marks, all primaries.
    pub fn shed_count(&self) -> usize {
        locked(&self.shed, "shed marks").len()
    }

    /// Account one `meta` entry leaving role `old` for role `new` (`None` =
    /// absent). Called under the `meta` lock.
    fn account_role(&self, old: Option<PartitionRole>, new: Option<PartitionRole>) {
        let was = old == Some(PartitionRole::Backup);
        let is = new == Some(PartitionRole::Backup);
        if is && !was {
            self.backups.fetch_add(1, Ordering::Relaxed);
        } else if was && !is {
            self.backups.fetch_sub(1, Ordering::Relaxed);
        }
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
        if let Some(m) = locked(&self.meta, "replica metadata").get_mut(call_ref) {
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
        locked(&self.meta, "replica metadata").get(call_ref).is_some_and(|m| m.authority_answered)
    }

    /// Is this call's body past its TTL? An expired body reads as absent until
    /// [`reap`](Self::reap) evicts it. Pure read.
    fn is_expired(&self, call_ref: &str) -> bool {
        let now = self.clock.now_ms();
        let meta = locked(&self.meta, "replica metadata");
        matches!(meta.get(call_ref), Some(m) if matches!(m.expiry_at_ms, Some(e) if now >= e))
    }

    /// The refs whose body is expired at `now_ms`.
    fn expired_snapshot(&self, now_ms: i64) -> Vec<String> {
        locked(&self.meta, "replica metadata")
            .iter()
            .filter(|(_, m)| matches!(m.expiry_at_ms, Some(e) if now_ms >= e))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Evict each snapshotted ref whose body is **still** expired at `now_ms`,
    /// with every index key it owns (all per-call state released), and return
    /// the evicted bodies. The check, the metadata removal and the body removal
    /// are one step under the metadata lock, the lock every write holds, so a
    /// refresh that landed after the snapshot keeps its body and is not handed
    /// back.
    fn evict_snapshot(&self, expired: Vec<String>, now_ms: i64) -> Vec<Arc<[u8]>> {
        let mut evicted = Vec::with_capacity(expired.len());
        for call_ref in &expired {
            let mut meta = locked(&self.meta, "replica metadata");
            let still_expired = meta
                .get(call_ref)
                .is_some_and(|m| matches!(m.expiry_at_ms, Some(e) if now_ms >= e));
            if !still_expired {
                continue;
            }
            let Some(gone) = meta.remove(call_ref) else { continue };
            self.account_role(Some(gone.role), None);
            if let Some(body) =
                self.inner.remove_body(gone.role, &gone.primary, call_ref, &gone.meta.indexes)
            {
                evicted.push(body);
            }
        }
        evicted
    }

    /// Reap changelog tombstones/idle peers, prune stale resurrection
    /// tombstones and shed marks, then evict every expired body and return the
    /// evicted bodies. The one eviction site of an expired body: a read never
    /// evicts, so an expired body that still owes something (a deferred
    /// terminal's limiter release) reaches the caller exactly once. The upkeep
    /// runs first, so nothing runs between the eviction and the return. Call
    /// after advancing the clock.
    #[must_use = "an evicted body may owe a release the caller must settle"]
    pub async fn reap(&self, now_ms: i64) -> Vec<Arc<[u8]>> {
        locked(&self.tombstones, "resurrection tombstones").prune(now_ms);
        locked(&self.shed, "shed marks").reap(now_ms);
        self.changelog.reap(now_ms);
        let expired = self.expired_snapshot(now_ms);
        self.evict_snapshot(expired, now_ms)
    }
}

/// What a write is, for the incarnation rule of [`ReplicatingCallStore::admits`].
#[derive(Clone, Copy)]
struct Write {
    role: PartitionRole,
    direction: Option<PropagateDirection>,
    now: i64,
}

impl ReplicatingCallStore {
    /// The incarnation rule of a write of `incarnation` of `call_ref`, under
    /// the metadata lock (ADR-0014, "`(p,b)` orders one call incarnation").
    /// `false` leaves the store as it is:
    ///
    /// - a buried call stays buried (the resurrection guard); a write of a call
    ///   another one replaced re-stamps its burial, and a backup keeps it for
    ///   the replica reap;
    /// - an acting backup's write (`Reverse`) never replaces another call: the
    ///   authority replaced the call it serves, so the call is buried as
    ///   replaced and its body kept for the reap.
    ///
    /// `true` stores the write. When it replaces another call, that call's
    /// record, body and index keys go first and the call is buried as
    /// replaced; a backup keeps its body for the reap when the body is a
    /// version a backup authored (`b > 0`).
    // FIXME(repl): the tombstone refuses a deferred terminal for a ref this node
    // already discharged, losing the record; yield to a terminal it never saw.
    fn admits(
        &self,
        meta: &mut HashMap<String, CallMeta>,
        call_ref: &str,
        incarnation: Option<&str>,
        body: &Arc<[u8]>,
        write: Write,
    ) -> bool {
        let mut tombstones = locked(&self.tombstones, "resurrection tombstones");
        let keep_for_reap = |incarnation: Option<&str>, body: &Arc<[u8]>| {
            if let Some(id) = incarnation {
                locked(&self.displaced, "displaced bodies").insert(id.to_string(), body.clone());
            }
        };
        match tombstones.burial(call_ref, incarnation, write.now) {
            Some(Burial::Deleted) => return false,
            Some(Burial::Replaced) => {
                // A copy still writing the replaced call keeps it buried for as
                // long as it writes, not just the window after the replacement.
                if let Some(id) = incarnation {
                    tombstones.bury(call_ref, Some(id.to_string()), Burial::Replaced, write.now);
                }
                if write.role == PartitionRole::Backup {
                    keep_for_reap(incarnation, body);
                }
                return false;
            }
            None => {}
        }
        let held = meta.get(call_ref).and_then(|m| m.meta.incarnation.clone());
        if !is_another_call(held.as_deref(), incarnation) {
            return true;
        }
        if write.direction == Some(PropagateDirection::Reverse) {
            let owned = incarnation.map(str::to_string);
            tombstones.bury(call_ref, owned, Burial::Replaced, write.now);
            if write.role == PartitionRole::Backup {
                keep_for_reap(incarnation, body);
            }
            return false;
        }
        if let Some(gone) = meta.remove(call_ref) {
            self.account_role(Some(gone.role), None);
            let removed =
                self.inner.remove_body(gone.role, &gone.primary, call_ref, &gone.meta.indexes);
            tombstones.bury(call_ref, gone.meta.incarnation.clone(), Burial::Replaced, write.now);
            // Only a version a backup authored (`b > 0`) can be a terminal it
            // deferred; the authority settles its own (`b == 0`).
            let backup_authored = gone.meta.call_bgen > 0;
            if let (PartitionRole::Backup, true, Some(removed)) =
                (gone.role, backup_authored, removed)
            {
                keep_for_reap(gone.meta.incarnation.as_deref(), &removed);
            }
        }
        true
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
        if self.is_expired(call_ref) {
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
        let now = self.clock.now_ms();
        // Apply the backstop TTL for ttl_ms <= 0 so a missed-delete replica still
        // self-evicts (the wire-carried `body_ttl_ms` keeps the original ttl_ms).
        let expiry_at_ms = self.expiry_for(now, ttl_ms);
        // Receive-time clock-skew offset (clock-skew hardening): the SAME `now`
        // reading that anchors `expiry_at_ms` minus the origin node's send-time
        // wall clock. Persisted so a later failover/reclaim re-anchors this call's
        // absolute timer deadlines. `None` on a locally-originated write.
        let skew_offset_ms = opts.origin_now_ms.map(|origin| now - origin);
        // Everything the write carries is built before the lock: the critical
        // section is map operations only.
        let body: Arc<[u8]> = Arc::from(body);
        let mut ref_meta = RefMeta {
            call_gen,
            call_bgen,
            body_ttl_ms: ttl_ms,
            indexes: indexes.to_vec(),
            incarnation: opts.incarnation.clone(),
        };
        // A Forward flush carries the backup ordinal as `opts.peer`.
        let flushed_backup = match (opts.direction, &opts.peer) {
            (Some(PropagateDirection::Forward), Some(p)) => Some(p.clone()),
            _ => None,
        };

        // Store the body and update the per-ref metadata in one critical section
        // under the metadata lock, the lock the reap evicts under, so a reap
        // never splits a write. A write CARRIES OVER every field it does not
        // itself carry: the backup ordinal, the skew offset and the authority's
        // view of the answer are facts about the ref that only their own writer
        // may change, and rebuilding the entry from this write alone would
        // silently clear each of them.
        {
            let mut meta = locked(&self.meta, "replica metadata");
            // An unnamed write is one of the held call: it takes its name.
            ref_meta.incarnation = ref_meta
                .incarnation
                .take()
                .or_else(|| meta.get(call_ref).and_then(|m| m.meta.incarnation.clone()));
            let write = Write { role, direction: opts.direction, now };
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
                    if skew_offset_ms.is_some() {
                        m.skew_offset_ms = skew_offset_ms;
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
                            skew_offset_ms,
                            // Until a forward flush shows one, no answer of the
                            // authority's own stands on this Element.
                            authority_answered: false,
                        },
                    );
                }
            }
        }

        locked(&self.shed, "shed marks").clear(call_ref);

        // HA path only: non-blocking changelog bump for the pulling peer.
        if let Some(peer) = &opts.peer {
            let partition = Self::partition_for(opts.direction);
            self.changelog.bump_put(peer, call_ref, partition);
        }
        Ok(())
    }

    async fn delete_call(
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
            let mut meta = locked(&self.meta, "replica metadata");
            let held = meta.get(call_ref).and_then(|m| m.meta.incarnation.as_deref());
            let mut tombstones = locked(&self.tombstones, "resurrection tombstones");
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
        locked(&self.shed, "shed marks").clear(call_ref);

        if let Some(peer) = &opts.peer {
            let partition = Self::partition_for(opts.direction);
            self.changelog.bump_delete(peer, call_ref, partition, answered, incarnation);
        }
        Ok(())
    }

    /// An index entry of an expired body reads as absent, like the body.
    async fn get_index(&self, index_key: &str) -> Result<Option<String>, StoreError> {
        let call_ref = self.inner.get_index(index_key).await?;
        Ok(call_ref.filter(|call_ref| !self.is_expired(call_ref)))
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
/// metadata; an expired body reads as absent.
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
        locked(&self.meta, "replica metadata").get(call_ref).map(|m| m.meta.clone())
    }

    /// Body and metadata under the metadata lock, the lock every write holds,
    /// so the two are of one write. An expired body reads as absent.
    async fn read_entry(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Option<(Arc<[u8]>, RefMeta)> {
        let now = self.clock.now_ms();
        let meta = locked(&self.meta, "replica metadata");
        let m = meta.get(call_ref)?;
        if matches!(m.expiry_at_ms, Some(e) if now >= e) {
            return None;
        }
        Some((self.inner.body(role, primary, call_ref)?, m.meta.clone()))
    }

    fn scan_refs(&self, role: PartitionRole, primary: &str) -> Vec<String> {
        self.scan_call_refs(role, primary)
    }

    /// Live `pri:{primary}` refs whose captured `backup == backup` — the
    /// **Backup**-flow bootstrap snapshot (ADR-0014 Option B). Expired-but-not-
    /// yet-reaped refs are filtered out (the body would read `None` anyway).
    fn scan_refs_backed_by(&self, primary: &str, backup: &str) -> Vec<String> {
        let now = self.clock.now_ms();
        let meta = locked(&self.meta, "replica metadata");
        meta.iter()
            .filter(|(_, m)| m.role == PartitionRole::Primary && m.primary == primary)
            .filter(|(_, m)| m.backup.as_deref() == Some(backup))
            .filter(|(_, m)| !matches!(m.expiry_at_ms, Some(e) if now >= e))
            .map(|(k, _)| k.clone())
            .collect()
    }
}

#[cfg(test)]
mod backup_count_tests {
    //! Pins `backup_held` to a scan of the meta map through every writer that
    //! adds, re-roles or removes an entry.

    use super::*;

    fn scanned(store: &ReplicatingCallStore) -> u64 {
        let meta = store.meta.lock().unwrap();
        meta.values().filter(|m| m.role == PartitionRole::Backup).count() as u64
    }

    async fn put(store: &ReplicatingCallStore, role: PartitionRole, call_ref: &str, ttl_ms: i64) {
        store
            .put_call(role, "w0", call_ref, b"b".to_vec(), &[], ttl_ms, 1, 0, &PutOpts::default())
            .await
            .unwrap();
    }

    /// A refresh that lands between the reap's snapshot and its eviction (a
    /// healed peer's flush while the sweep runs) keeps its fresh body and
    /// meta, and the reap hands nothing back for it.
    #[tokio::test(start_paused = true)]
    async fn a_refresh_landing_after_the_reap_snapshot_survives_the_reap() {
        let clock = Clock::test_at(0);
        let store = ReplicatingCallStore::new(1, clock.clone());
        store
            .put_call(
                PartitionRole::Backup,
                "w0",
                "w0|r|t",
                b"old".to_vec(),
                &[],
                1_000,
                1,
                0,
                &PutOpts::default(),
            )
            .await
            .unwrap();
        tokio::time::advance(std::time::Duration::from_millis(2_000)).await;

        let expired = store.expired_snapshot(clock.now_ms());
        assert_eq!(expired.len(), 1, "the old body is expired");
        store
            .put_call(
                PartitionRole::Backup,
                "w0",
                "w0|r|t",
                b"new".to_vec(),
                &[],
                60_000,
                2,
                0,
                &PutOpts::default(),
            )
            .await
            .unwrap();
        let evicted = store.evict_snapshot(expired, clock.now_ms());

        assert!(evicted.is_empty(), "nothing is handed back for a refreshed ref");
        let body = store.get_call(PartitionRole::Backup, "w0", "w0|r|t").await.unwrap();
        assert_eq!(body.as_deref(), Some(&b"new"[..]), "the fresh body survives");
        assert_eq!(
            store.current_cv(PartitionRole::Backup, "w0", "w0|r|t"),
            Some((2, 0)),
            "and its meta"
        );
        assert_eq!(store.backup_held(), 1);
    }

    /// A panic under the metadata lock never takes the store down with it:
    /// every later reader and the reap go on with the data as it stands.
    #[tokio::test(start_paused = true)]
    async fn a_poisoned_metadata_lock_still_reads_and_reaps() {
        let clock = Clock::test_at(0);
        let store = ReplicatingCallStore::new(1, clock.clone());
        put(&store, PartitionRole::Backup, "w0|p|t", 1_000).await;
        let poisoner = store.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.meta.lock();
            panic!("poison the metadata lock");
        })
        .join();
        assert!(store.meta.is_poisoned());

        tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
        assert!(store.get_call(PartitionRole::Backup, "w0", "w0|p|t").await.unwrap().is_none());
        assert_eq!(store.reap(clock.now_ms()).await.len(), 1, "the reap still evicts");
        assert_eq!(store.backup_held(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_backup_count_matches_a_scan_through_every_writer() {
        let clock = Clock::test_at(0);
        let store = ReplicatingCallStore::new(1, clock.clone()).with_default_ttl_ms(0);
        let check = |want: u64| {
            assert_eq!(store.backup_held(), want);
            assert_eq!(store.backup_held(), scanned(&store));
        };

        put(&store, PartitionRole::Backup, "w0|a|t", 0).await;
        put(&store, PartitionRole::Backup, "w0|b|t", 0).await;
        put(&store, PartitionRole::Primary, "w1|c|t", 0).await;
        check(2);

        put(&store, PartitionRole::Backup, "w0|a|t", 0).await;
        check(2);
        put(&store, PartitionRole::Primary, "w0|a|t", 0).await;
        check(1);
        put(&store, PartitionRole::Backup, "w0|a|t", 0).await;
        check(2);

        store
            .delete_call(PartitionRole::Backup, "w0", "w0|b|t", &[], false, &PutOpts::default())
            .await
            .unwrap();
        check(1);

        put(&store, PartitionRole::Backup, "w0|d|t", 1_000).await;
        put(&store, PartitionRole::Backup, "w0|e|t", 1_000).await;
        check(3);
        tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
        assert!(store.get_call(PartitionRole::Backup, "w0", "w0|d|t").await.unwrap().is_none());
        check(3);
        assert_eq!(store.reap(clock.now_ms()).await.len(), 2, "the reap evicts both");
        check(1);
    }
}
