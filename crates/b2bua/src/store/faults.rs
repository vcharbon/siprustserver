//! Injectable store-fault seam (ADR-0023) — a [`FaultInjectingCallStore`]
//! decorator over any [`CallStore`] plus the [`StoreFaults`] control handle a
//! test flips MID-CALL, and the **live-path fault probe** the router consults
//! at the three live call-handling lookup sites. The seam is generic: it
//! carries no callflow logic, only "this read fails now".
//!
//! Two independent halves, one handle:
//!
//! - **Decorator ops** ([`StoreFaultPoint::GetCall`] … [`ScanCalls`]): each
//!   armed op on the wrapped store returns [`StoreError::Backend`] instead of
//!   delegating — the persistence/replication read-write plane.
//! - **Live probes** ([`LiveInitialInvite`]/[`LiveInDialog`]/[`LiveAudit`]):
//!   the live call-handling lookups (`resolve_from_sip_key_sync`/`peek`) are
//!   infallible in-memory reads by architecture — live handling is NOT
//!   converted to async store reads. Instead the router consults the probe
//!   *before* the map read, making the existing sync lookup fallible.
//!
//! The **defined store-failure semantics** the router implements against the
//! live probes (deterministic; ADR-0023):
//!
//! - **Initial INVITE** (`LiveInitialInvite`): fail **closed** — a final
//!   `500 Server Internal Error` through the INVITE server txn (composing with
//!   the ADR-0022 no-100-then-silence guarantee), **no call created**, the
//!   per-call dispatch ephemera reclaimed via the orphan teardown.
//! - **In-dialog request** (`LiveInDialog` — BYE, re-INVITE, …): fail
//!   **closed** — `500` to that request; the call and its state stay untouched
//!   (deliberately distinct from the `481` lookup-*miss*). A retry after the
//!   store recovers proceeds normally.
//! - **Audit/keepalive timer** (`LiveAudit`): fail **open** — skip the probe
//!   cycle, keep the call up, and RE-ARM the keepalive timer so liveness
//!   detection resumes next interval (a store fault alone must never tear down
//!   an established call — the protected-calls invariant,
//!   `docs/testing/ha-acceptance.md`). Observable via
//!   `b2bua_store_fault_audit_skipped_total`.
//!
//! **HA scoping:** the probe defaults to no-fault and sits ONLY on the
//! live-serving lookup sites. The HA replication/reclaim paths — reclaim
//! discharge, the `(p,b)` reconciliation, the terminate writer — are
//! deliberately un-probed and behave exactly as before.
//!
//! [`ScanCalls`]: StoreFaultPoint::ScanCalls
//! [`LiveInitialInvite`]: StoreFaultPoint::LiveInitialInvite
//! [`LiveInDialog`]: StoreFaultPoint::LiveInDialog
//! [`LiveAudit`]: StoreFaultPoint::LiveAudit

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use super::call_store::{CallStore, PartitionRole, PutOpts, StoreError};

/// A switchable fault point: one per [`CallStore`] op (the decorator half) and
/// one per live call-handling lookup site (the probe half).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreFaultPoint {
    // ── decorator ops (each maps 1:1 onto a `CallStore` method) ──
    GetCall,
    PutCall,
    DeleteCall,
    RefreshCall,
    GetIndex,
    ScanCalls,
    // ── live-path probes (the router's sync lookup sites) ──
    /// The initial-INVITE dialog-existence lookup (fail-closed 500).
    LiveInitialInvite,
    /// The in-dialog request per-event call fetch (fail-closed 500).
    LiveInDialog,
    /// The audit/keepalive timer's call read (fail-open skip + re-arm).
    LiveAudit,
}

#[derive(Default)]
struct Inner {
    get_call: AtomicBool,
    put_call: AtomicBool,
    delete_call: AtomicBool,
    refresh_call: AtomicBool,
    get_index: AtomicBool,
    scan_calls: AtomicBool,
    live_initial_invite: AtomicBool,
    live_in_dialog: AtomicBool,
    live_audit: AtomicBool,
}

/// Shared, clone-cheap fault control handle. `Default` = **no faults** — the
/// production wiring passes exactly this, so the probe is a no-op read of an
/// atomic on the paths it guards. A test retains a clone and flips per-path
/// switches mid-call ([`arm`](Self::arm)/[`disarm`](Self::disarm)).
#[derive(Clone, Default)]
pub struct StoreFaults {
    inner: Arc<Inner>,
}

impl StoreFaults {
    /// A fresh all-disarmed handle (alias of `Default`).
    pub fn none() -> Self {
        Self::default()
    }

    fn cell(&self, point: StoreFaultPoint) -> &AtomicBool {
        match point {
            StoreFaultPoint::GetCall => &self.inner.get_call,
            StoreFaultPoint::PutCall => &self.inner.put_call,
            StoreFaultPoint::DeleteCall => &self.inner.delete_call,
            StoreFaultPoint::RefreshCall => &self.inner.refresh_call,
            StoreFaultPoint::GetIndex => &self.inner.get_index,
            StoreFaultPoint::ScanCalls => &self.inner.scan_calls,
            StoreFaultPoint::LiveInitialInvite => &self.inner.live_initial_invite,
            StoreFaultPoint::LiveInDialog => &self.inner.live_in_dialog,
            StoreFaultPoint::LiveAudit => &self.inner.live_audit,
        }
    }

    /// Make `point` fail from now on (until [`disarm`](Self::disarm)ed).
    pub fn arm(&self, point: StoreFaultPoint) {
        self.cell(point).store(true, Ordering::Relaxed);
    }

    /// Restore `point` to healthy.
    pub fn disarm(&self, point: StoreFaultPoint) {
        self.cell(point).store(false, Ordering::Relaxed);
    }

    /// Disarm every point (test teardown convenience).
    pub fn disarm_all(&self) {
        for p in [
            StoreFaultPoint::GetCall,
            StoreFaultPoint::PutCall,
            StoreFaultPoint::DeleteCall,
            StoreFaultPoint::RefreshCall,
            StoreFaultPoint::GetIndex,
            StoreFaultPoint::ScanCalls,
            StoreFaultPoint::LiveInitialInvite,
            StoreFaultPoint::LiveInDialog,
            StoreFaultPoint::LiveAudit,
        ] {
            self.disarm(p);
        }
    }

    pub fn is_armed(&self, point: StoreFaultPoint) -> bool {
        self.cell(point).load(Ordering::Relaxed)
    }

    /// The fallible-lookup form: `Err(StoreError::Backend)` when `point` is
    /// armed, `Ok(())` otherwise. The decorator calls it before delegating;
    /// the router calls it before the sync map read it guards.
    pub fn check(&self, point: StoreFaultPoint) -> Result<(), StoreError> {
        if self.is_armed(point) {
            Err(StoreError::Backend(format!("injected store fault: {point:?}")))
        } else {
            Ok(())
        }
    }
}

/// [`CallStore`] decorator: delegates every op to `inner` unless the op's
/// [`StoreFaultPoint`] is armed on the shared [`StoreFaults`] handle, in which
/// case it returns [`StoreError::Backend`]. Wrap any store (`InMemoryCallStore`
/// or a replicating one) — the seam is implementation-agnostic.
pub struct FaultInjectingCallStore {
    inner: Arc<dyn CallStore>,
    faults: StoreFaults,
}

impl FaultInjectingCallStore {
    pub fn new(inner: Arc<dyn CallStore>, faults: StoreFaults) -> Self {
        Self { inner, faults }
    }

    /// The shared control handle (a clone of what was passed in).
    pub fn faults(&self) -> &StoreFaults {
        &self.faults
    }
}

#[async_trait]
impl CallStore for FaultInjectingCallStore {
    async fn get_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Result<Option<Arc<[u8]>>, StoreError> {
        self.faults.check(StoreFaultPoint::GetCall)?;
        self.inner.get_call(role, primary, call_ref).await
    }

    #[allow(clippy::too_many_arguments)]
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
        self.faults.check(StoreFaultPoint::PutCall)?;
        self.inner
            .put_call(role, primary, call_ref, body, indexes, ttl_ms, call_gen, call_bgen, opts)
            .await
    }

    async fn delete_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        indexes: &[String],
        opts: &PutOpts,
    ) -> Result<(), StoreError> {
        self.faults.check(StoreFaultPoint::DeleteCall)?;
        self.inner.delete_call(role, primary, call_ref, indexes, opts).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn refresh_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        indexes: &[String],
        ttl_ms: i64,
        call_gen: i64,
        call_bgen: i64,
    ) -> Result<(), StoreError> {
        self.faults.check(StoreFaultPoint::RefreshCall)?;
        self.inner
            .refresh_call(role, primary, call_ref, indexes, ttl_ms, call_gen, call_bgen)
            .await
    }

    async fn get_index(&self, index_key: &str) -> Result<Option<String>, StoreError> {
        self.faults.check(StoreFaultPoint::GetIndex)?;
        self.inner.get_index(index_key).await
    }

    async fn scan_calls(
        &self,
        role: PartitionRole,
        primary: &str,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        self.faults.check(StoreFaultPoint::ScanCalls)?;
        self.inner.scan_calls(role, primary).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::memory::InMemoryCallStore;
    use super::*;

    fn store_with_one_call() -> (FaultInjectingCallStore, StoreFaults) {
        let faults = StoreFaults::default();
        let s = FaultInjectingCallStore::new(Arc::new(InMemoryCallStore::new()), faults.clone());
        (s, faults)
    }

    // Each armed op fails with `Backend`; disarming restores passthrough — the
    // MID-CALL flip contract the integration tests ride.
    #[tokio::test]
    async fn armed_get_call_fails_and_disarm_restores() {
        let (s, faults) = store_with_one_call();
        s.put_call(PartitionRole::Primary, "w0", "w0|c|t", b"body".to_vec(), &[], 0, 1, 0, &PutOpts::default())
            .await
            .unwrap();

        faults.arm(StoreFaultPoint::GetCall);
        assert!(matches!(
            s.get_call(PartitionRole::Primary, "w0", "w0|c|t").await,
            Err(StoreError::Backend(_))
        ));
        // Other ops are unaffected by the get_call switch (per-path isolation).
        assert!(s.scan_calls(PartitionRole::Primary, "w0").await.is_ok());

        faults.disarm(StoreFaultPoint::GetCall);
        let body = s.get_call(PartitionRole::Primary, "w0", "w0|c|t").await.unwrap();
        assert_eq!(body.as_deref(), Some(b"body".as_slice()), "recovered read sees the body");
    }

    // A faulted write must not mutate the wrapped store (the decorator fails
    // BEFORE delegating).
    #[tokio::test]
    async fn armed_put_call_does_not_reach_the_inner_store() {
        let (s, faults) = store_with_one_call();
        faults.arm(StoreFaultPoint::PutCall);
        assert!(s
            .put_call(PartitionRole::Primary, "w0", "w0|c|t", b"x".to_vec(), &[], 0, 1, 0, &PutOpts::default())
            .await
            .is_err());
        faults.disarm(StoreFaultPoint::PutCall);
        assert_eq!(
            s.get_call(PartitionRole::Primary, "w0", "w0|c|t").await.unwrap(),
            None,
            "nothing landed while the put fault was armed"
        );
    }

    // The live-probe points share the same handle but gate nothing on the
    // decorator — `check` is the router's seam.
    #[tokio::test]
    async fn live_points_only_trip_check() {
        let (s, faults) = store_with_one_call();
        faults.arm(StoreFaultPoint::LiveInDialog);
        assert!(faults.check(StoreFaultPoint::LiveInDialog).is_err());
        assert!(faults.check(StoreFaultPoint::LiveAudit).is_ok());
        // Decorator ops keep working — the live switches are router-side only.
        assert!(s.get_index("leg:x").await.is_ok());
        faults.disarm_all();
        assert!(faults.check(StoreFaultPoint::LiveInDialog).is_ok());
    }
}
