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

/// Per-callRef side metadata kept in lockstep with the body so the drain can
/// fill a `Frame::Data` without touching the typed call map.
#[derive(Clone, Debug)]
struct CallMeta {
    meta: RefMeta,
    /// Where the body lives in the backing keyspace (so `reap` can delete it).
    role: PartitionRole,
    primary: String,
    /// Absolute body-expiry deadline (lazy TTL); `None` when `ttl_ms <= 0`.
    expiry_at_ms: Option<i64>,
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
        }
    }

    /// The owned changelog (S5's server loop subscribes/drains through this).
    pub fn changelog(&self) -> &Changelog {
        &self.changelog
    }

    /// The content version (`call_gen`) currently stored for a callRef, or
    /// `None` if the ref is absent. The S5 puller reads this to drive its LWW
    /// apply-gate: a delivered frame whose `call_gen` is `<=` the stored one is
    /// a stale/idempotent re-delivery — the body write is skipped (but the
    /// watermark still advances). The `(role, primary)` args are accepted for a
    /// uniform seam with the other store methods; the per-ref metadata is keyed
    /// by callRef alone, so they are not needed to look the version up.
    pub fn current_call_gen(
        &self,
        _role: PartitionRole,
        _primary: &str,
        call_ref: &str,
    ) -> Option<i64> {
        if self.is_expired(call_ref) {
            return None;
        }
        self.meta.lock().unwrap().get(call_ref).map(|m| m.meta.call_gen)
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

    /// Map propagate direction → the partition tag the frame carries.
    fn partition_for(direction: Option<PropagateDirection>) -> Partition {
        match direction {
            // Acting-backup pushing reclaim data back to the primary.
            Some(PropagateDirection::Reverse) => Partition::Pri,
            // Primary → backup (the default).
            _ => Partition::Bak,
        }
    }

    /// Is this call's body past its TTL? (lazy-eviction gate; pure read.)
    fn is_expired(&self, call_ref: &str) -> bool {
        let now = self.clock.now_ms();
        let meta = self.meta.lock().unwrap();
        matches!(meta.get(call_ref), Some(m) if matches!(m.expiry_at_ms, Some(e) if now >= e))
    }

    /// Lazily evict an expired body + meta on access; returns `true` if evicted.
    async fn evict_if_expired(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> bool {
        if !self.is_expired(call_ref) {
            return false;
        }
        self.meta.lock().unwrap().remove(call_ref);
        let _ = self
            .inner
            .delete_call(role, primary, call_ref, &[], &PutOpts::default())
            .await;
        true
    }

    /// Evict every expired body + reap changelog tombstones/idle peers. Call
    /// after advancing the clock (lazy TTL — deterministic, no background task).
    pub async fn reap(&self, now_ms: i64) {
        // Snapshot the expired (callRef, role, primary) tuples, drop the lock,
        // then delete each body from the backing store.
        let expired: Vec<(String, PartitionRole, String)> = {
            let meta = self.meta.lock().unwrap();
            meta.iter()
                .filter(|(_, m)| matches!(m.expiry_at_ms, Some(e) if now_ms >= e))
                .map(|(k, m)| (k.clone(), m.role, m.primary.clone()))
                .collect()
        };
        for (call_ref, role, primary) in &expired {
            self.meta.lock().unwrap().remove(call_ref);
            let _ = self
                .inner
                .delete_call(*role, primary, call_ref, &[], &PutOpts::default())
                .await;
        }
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
        if self.evict_if_expired(role, primary, call_ref).await {
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
        opts: &PutOpts,
    ) -> Result<(), StoreError> {
        // Store body (Arc wrapped once inside) + indexes.
        self.inner
            .put_call(role, primary, call_ref, body, indexes, ttl_ms, call_gen, opts)
            .await?;

        let now = self.clock.now_ms();
        let expiry_at_ms = if ttl_ms > 0 { Some(now + ttl_ms) } else { None };

        // Update per-ref metadata atomically (single critical section).
        {
            let mut meta = self.meta.lock().unwrap();
            meta.insert(
                call_ref.to_string(),
                CallMeta {
                    meta: RefMeta {
                        call_gen,
                        body_ttl_ms: ttl_ms,
                        indexes: indexes.to_vec(),
                    },
                    role,
                    primary: primary.to_string(),
                    expiry_at_ms,
                },
            );
        }

        // HA path only: non-blocking changelog bump for the pulling peer.
        if let Some(peer) = &opts.peer {
            let partition = Self::partition_for(opts.direction);
            self.changelog.bump(peer, call_ref, Op::Update, partition);
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
        self.inner
            .delete_call(role, primary, call_ref, indexes, opts)
            .await?;
        self.meta.lock().unwrap().remove(call_ref);

        if let Some(peer) = &opts.peer {
            let partition = Self::partition_for(opts.direction);
            self.changelog.bump(peer, call_ref, Op::Delete, partition);
        }
        Ok(())
    }

    async fn refresh_call(
        &self,
        _role: PartitionRole,
        _primary: &str,
        call_ref: &str,
        indexes: &[String],
        ttl_ms: i64,
        call_gen: i64,
    ) -> Result<(), StoreError> {
        // Unlike the in-memory no-op, honour the ttl/call_gen now: bump the
        // body's absolute expiry and refresh the drained metadata.
        let now = self.clock.now_ms();
        let mut meta = self.meta.lock().unwrap();
        if let Some(m) = meta.get_mut(call_ref) {
            m.meta.call_gen = call_gen;
            m.meta.body_ttl_ms = ttl_ms;
            if !indexes.is_empty() {
                m.meta.indexes = indexes.to_vec();
            }
            m.expiry_at_ms = if ttl_ms > 0 { Some(now + ttl_ms) } else { None };
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
}
