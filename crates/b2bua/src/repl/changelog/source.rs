//! The store-side seam the drain reads through: a live body and the per-ref
//! metadata the [`Frame::Data`](repl_net::frame::Frame::Data) carrying it states.

use std::sync::Arc;

use crate::store::PartitionRole;

/// Per-callRef metadata needed to build a [`Frame::Data`](repl_net::frame::Frame::Data) without re-reading the
/// (typed) call. Carried alongside the body by [`ReplicatingCallStore`].
///
/// [`ReplicatingCallStore`]: crate::repl::ReplicatingCallStore
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefMeta {
    /// Primary counter `p` of the `(p,b)` version vector.
    pub call_gen: i64,
    /// Backup counter `b` of the `(p,b)` version vector (ADR-0014).
    pub call_bgen: i64,
    /// Body TTL in ms (the store `ttl`).
    pub body_ttl_ms: i64,
    /// Index keys for this call.
    pub indexes: Vec<String>,
    /// The call incarnation the body is of (`PutOpts::incarnation`); the
    /// `(p,b)` vector orders versions of this incarnation only.
    pub incarnation: Option<String>,
    /// This node's clock minus the clock the body's instants were written on
    /// (ADR-0055 §9): set by the apply of a peer's write, `None` on a body
    /// written here, whose instants are in this node's frame.
    pub skew_offset_ms: Option<i64>,
}

impl RefMeta {
    /// `now_ms`, this node's clock reading, read on the clock the body's
    /// instants were written on: the `origin_now_ms` a frame carrying the
    /// body states, so its receiver's offset composes every hop the body made.
    pub fn origin_now_ms(&self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(self.skew_offset_ms.unwrap_or(0))
    }
}

/// The store-side seam the drain reads through to build [`Frame::Data`](repl_net::frame::Frame::Data). The
/// changelog stays decoupled from [`ReplicatingCallStore`]: it asks for a live
/// body (`Arc<[u8]>`) and the per-ref metadata.
///
/// [`ReplicatingCallStore`]: crate::repl::ReplicatingCallStore
#[async_trait::async_trait]
pub trait BodySource: Send + Sync {
    /// The live encoded body for a callRef in `(role, primary)`, or `None` if it
    /// is gone (→ the drain emits a `Delete`).
    async fn read_body(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Option<Arc<[u8]>>;

    /// The per-ref metadata (call_gen / ttl / indexes), or `None` if gone.
    fn read_meta(&self, call_ref: &str) -> Option<RefMeta>;

    /// The live body and its metadata, read together: the frame built from
    /// them must name the version and incarnation of the body it carries.
    async fn read_entry(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Option<(Arc<[u8]>, RefMeta)> {
        let body = self.read_body(role, primary, call_ref).await?;
        Some((body, self.read_meta(call_ref)?))
    }

    /// Snapshot the live callRef KEYS in `(role, primary)` under a BRIEF lock.
    /// The **Reclaim** bootstrap copies the `bak:{caller}` keyset this way.
    fn scan_refs(&self, _role: PartitionRole, _primary: &str) -> Vec<String> {
        Vec::new()
    }

    /// `backup` refused the replica of this node's call `call_ref` (ADR-0055
    /// §3): the call has no backup from here on.
    fn backup_refused(&self, _backup: &str, _call_ref: &str) {}

    /// Snapshot the live callRef KEYS this node is **primary** for whose **backup
    /// is `backup`** (ADR-0014 Option B). The **Backup** bootstrap uses this to
    /// scan `pri:{self}` filtered to the calls `caller` backs up.
    fn scan_refs_backed_by(&self, _primary: &str, _backup: &str) -> Vec<String> {
        Vec::new()
    }
}
