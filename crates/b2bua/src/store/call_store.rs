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
    /// The sender's clock at send time, read in the frame of the body's
    /// instants, carried on an inbound replica `Put` (`Frame::Data.origin_now_ms`).
    /// The replicating store keeps `skew_offset_ms = receiver_now_ms −
    /// origin_now_ms` beside the body, so a later materialisation moves the
    /// call's instants into this node's frame (ADR-0055 §9). `None` on a write of
    /// this node's own body: its instants are in this node's frame, and the
    /// write clears the offset the body it replaces held.
    pub origin_now_ms: Option<i64>,
    /// The call incarnation ([`call::Call::incarnation`]) the write is of: the
    /// body a `Put` stores, or the call a `Delete` removes. A replicating store
    /// keeps it beside the `(p,b)` version, which orders versions of one
    /// incarnation only, and a delete's resurrection tombstone names it.
    /// `None` names no incarnation: the write is taken as one of the call the
    /// store already holds for the ref.
    pub incarnation: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("call store backend error: {0}")]
    Backend(String),
}

/// The call body + index persistence seam.
#[async_trait]
pub trait CallStore: Send + Sync {
    /// Read a call body as a shared, immutable `Arc<[u8]>` (ADR-0011 X8). A
    /// rewrite REPLACES the slot's `Arc`, so a holder of a prior clone keeps
    /// reading the body it observed — the immutable-shared-body invariant.
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

    /// Delete a call body and its indexes. `answered`: the deleting node's own
    /// copy of the call had answered the caller — a replicating store states it
    /// on the propagated delete (ADR-0031 D3); every other store ignores it.
    async fn delete_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        indexes: &[String],
        answered: bool,
        opts: &PutOpts,
    ) -> Result<(), StoreError>;

    /// Resolve a SIP routing index key (a [`call::index_key`] key) to a `callRef`.
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
