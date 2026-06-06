//! The persistence/replication seam — port of `PartitionedRelayStorage`. The
//! method signatures already carry every HA parameter (partition `role`/
//! `primary`, propagate `peer`/`direction`, `call_gen`, index keys, `ttl`), so a
//! replicating implementation drops in later with **no changes** to the call
//! store, the dispatcher, or the rule engine. The shipped [`InMemoryCallStore`]
//! ignores the HA params.
//!
//! [`InMemoryCallStore`]: super::memory::InMemoryCallStore

use std::sync::Arc;

use async_trait::async_trait;

use call::{call_ref_primary, parse_call_ref};

/// Which partition a call body lives in: this worker is the natural primary, or
/// it is holding a backup replica for a peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartitionRole {
    Primary,
    Backup,
}

impl PartitionRole {
    pub fn as_str(self) -> &'static str {
        match self {
            PartitionRole::Primary => "pri",
            PartitionRole::Backup => "bak",
        }
    }
}

/// Replication propagation direction: primary→backup (`forward`) or backup→
/// original-primary on takeover (`reverse`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropagateDirection {
    Forward,
    Reverse,
}

/// Options that only matter to a replicating store; the in-memory impl ignores
/// them.
#[derive(Clone, Debug, Default)]
pub struct PutOpts {
    pub peer: Option<String>,
    pub direction: Option<PropagateDirection>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("call store backend error: {0}")]
    Backend(String),
}

/// The call body + index persistence seam.
#[async_trait]
pub trait CallStore: Send + Sync {
    /// Read a call body as a shared, immutable `Arc<[u8]>` (Decision 9 / ADR-0011
    /// X8). A rewrite REPLACES the slot's `Arc`, so a holder of a prior clone
    /// keeps reading the body it observed — the immutable-shared-body invariant.
    async fn get_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Result<Option<Arc<[u8]>>, StoreError>;

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
    ) -> Result<(), StoreError>;

    async fn delete_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        indexes: &[String],
        opts: &PutOpts,
    ) -> Result<(), StoreError>;

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
    ) -> Result<(), StoreError>;

    /// Resolve a SIP routing index key (`leg:callId|tag`) to a `callRef`.
    async fn get_index(&self, index_key: &str) -> Result<Option<String>, StoreError>;

    /// All call bodies this worker owns in `(role, primary)` (crash recovery).
    async fn scan_calls(
        &self,
        role: PartitionRole,
        primary: &str,
    ) -> Result<Vec<Vec<u8>>, StoreError>;
}

/// Pick the partition for a `callRef`: this worker is the primary if the ref's
/// encoded ordinal matches `self_ordinal`, else it holds a backup for that peer.
/// Legacy refs (no ordinal segment) default to `pri:self`.
pub fn partition_of(self_ordinal: &str, call_ref: &str) -> (PartitionRole, String) {
    match parse_call_ref(call_ref) {
        Some(p) if p.primary == self_ordinal => (PartitionRole::Primary, p.primary),
        Some(p) => (PartitionRole::Backup, p.primary),
        None => (PartitionRole::Primary, self_ordinal.to_string()),
    }
}

/// The partition role for a `callRef` **without allocating** the primary ordinal
/// — the role-only projection of [`partition_of`] for hot-path callers (e.g. the
/// per-mutation `(p,b)` bump in `CallState::update`) that only need "are we the
/// primary?". Classifies a malformed/legacy ref as `Primary` (`pri:self`), matching
/// [`partition_of`].
pub fn role_of(self_ordinal: &str, call_ref: &str) -> PartitionRole {
    match call_ref_primary(call_ref) {
        Some(primary) if primary == self_ordinal => PartitionRole::Primary,
        Some(_) => PartitionRole::Backup,
        None => PartitionRole::Primary,
    }
}
