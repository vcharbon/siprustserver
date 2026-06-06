//! The replication frame model — four positional-msgpack messages plus the
//! small value enums and the [`Watermark`] ordering they ride on.
//!
//! Field ORDER on the wire is the contract (ADR-0008 positional-msgpack ethos):
//! each frame serialises as a msgpack ARRAY whose element 0 is an integer tag.
//! The Rust types below group some flat wire elements behind a [`Watermark`] for
//! ergonomics, but the codec ([`crate::codec`]) flattens them back to the exact
//! array layout. The two replication flows (**Reclaim** = `partition=Pri`,
//! **Backup** = `partition=Bak`) run on two separate single-flow sockets and
//! share this one frame set (ADR-0014 §Stream topology).

use std::sync::Arc;

/// Discriminant outside the accepted range for an enum-coded byte field.
///
/// Carried up by the codec as a typed decode error; never panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownDiscriminant {
    /// Which field rejected the byte (for the error message).
    pub field: &'static str,
    /// The offending value.
    pub value: u8,
}

/// The mutation a [`Frame::Data`] carries.
///
/// **Create and Update are merged into one idempotent `Put`** (ADR-0014): the
/// compacted changelog delivers latest-per-call state, so the puller applies a
/// `Put` as insert-or-overwrite under the `(p,b)` gate and a re-delivery is a
/// no-op by version-vector dominance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// op 0 — upsert (create or content bump); carries a body.
    Put,
    /// op 1 — removal; `body` is nil.
    Delete,
}

impl Op {
    /// Wire byte for this op.
    pub fn as_u8(self) -> u8 {
        match self {
            Op::Put => 0,
            Op::Delete => 1,
        }
    }

    /// Decode a wire byte; rejects unknown discriminants.
    pub fn from_u8(v: u8) -> Result<Self, UnknownDiscriminant> {
        match v {
            0 => Ok(Op::Put),
            1 => Ok(Op::Delete),
            other => Err(UnknownDiscriminant {
                field: "Op",
                value: other,
            }),
        }
    }
}

/// Which partition a [`Frame::Data`] entry belongs to — and which **flow** a
/// [`Frame::PullRequest`] opens.
///
/// `Pri` (0) = the **Reclaim** flow: a primary reclaiming its own calls a backup
/// touched. `Bak` (1) = the **Backup** flow: a peer's own calls this node backs
/// up. Derivable from `call_ref`, carried explicitly for cheap dispatch and to
/// select the flow on a `PullRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Partition {
    /// partition 0 — primary (Reclaim flow).
    Pri,
    /// partition 1 — backup (Backup flow).
    Bak,
}

impl Partition {
    /// Wire byte for this partition.
    pub fn as_u8(self) -> u8 {
        match self {
            Partition::Pri => 0,
            Partition::Bak => 1,
        }
    }

    /// Decode a wire byte; rejects unknown discriminants.
    pub fn from_u8(v: u8) -> Result<Self, UnknownDiscriminant> {
        match v {
            0 => Ok(Partition::Pri),
            1 => Ok(Partition::Bak),
            other => Err(UnknownDiscriminant {
                field: "Partition",
                value: other,
            }),
        }
    }
}

/// A replication position: `(gen, counter)` ordered **lexicographically with
/// `gen` as the high word**.
///
/// `gen` is the worker **incarnation** (bumped per restart); `counter` is the
/// per-incarnation changelog index. The lexicographic order is load-bearing:
/// the reboot-incarnation rule depends on `(new_gen, 0) > (old_gen, *)`, so a
/// rebooted worker's counter-0 frames always beat anything from a prior
/// incarnation and the apply-gate accepts them without a manual reset. On the
/// wire the two words are always two flat array elements (`since_gen`/
/// `since_counter`, `gen`/`counter`); the struct is purely a Rust-side grouping.
///
/// A watermark is **purely a changelog position** — never read from or written
/// to a call's `(p,b)` version vector (the two-generations trap, ADR-0014).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watermark {
    /// Incarnation — the high word of the ordering.
    pub gen: u64,
    /// Per-incarnation changelog index — the low word.
    pub counter: u64,
}

impl Watermark {
    /// Construct a watermark.
    pub fn new(gen: u64, counter: u64) -> Self {
        Watermark { gen, counter }
    }
}

impl PartialOrd for Watermark {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Watermark {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // gen is the high word; counter breaks ties. `(1, 0) > (0, u64::MAX)`.
        self.gen
            .cmp(&other.gen)
            .then_with(|| self.counter.cmp(&other.counter))
    }
}

/// One replication message. Each variant maps to a positional-msgpack array
/// whose element 0 is the integer tag in the doc-comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// `[0, proto_ver, caller, partition, since_gen, since_counter]`
    ///
    /// Client → server: opens **one flow's** stream (`partition` selects Reclaim
    /// vs Backup). The server does a store-scan **bootstrap then tail** when
    /// `since == (0,0)`, else tails from `since`.
    PullRequest {
        /// Protocol version.
        proto_ver: u16,
        /// The pulling node's identifier.
        caller: String,
        /// Which flow: `Pri` = Reclaim, `Bak` = Backup.
        partition: Partition,
        /// Watermark to resume from; `(0,0)` ⇒ bootstrap-then-tail.
        since: Watermark,
    },
    /// `[1, gen, counter, op, partition, call_ref, call_gen, call_bgen, body_ttl_ms, indexes, body]`
    ///
    /// Server → client: one changelog entry. `body` is opaque msgpack `bin`
    /// (the `Arc<[u8]>` read straight from the store) or `nil` for
    /// delete/expired — this layer never decodes it.
    Data {
        /// Position of this entry.
        at: Watermark,
        /// put / delete.
        op: Op,
        /// pri (Reclaim) / bak (Backup).
        partition: Partition,
        /// `{primary}|{callId}|{fromTag}` ownership key.
        call_ref: String,
        /// **Primary** counter `p` of the per-context version vector `(p,b)`
        /// (ADR-0014). Bumped only by the call's primary on a local mutation;
        /// the value a backup echoes back is its branch point. `i64`, may be
        /// negative on the wire.
        call_gen: i64,
        /// **Backup** counter `b` of the version vector `(p,b)` (ADR-0014).
        /// Bumped only by an acting-backup on a takeover mutation. `i64`.
        call_bgen: i64,
        /// Body TTL in ms; `i64`, may be negative on the wire.
        body_ttl_ms: i64,
        /// Index keys for this call.
        indexes: Vec<String>,
        /// Opaque encoded call body, or `None` for delete/expired.
        body: Option<Arc<[u8]>>,
    },
    /// `[2, gen, counter]`
    ///
    /// Server → client: caught-up marker. Emitted on the **catch-up edge**
    /// (backlog drained to head) and on the ~20s idle keepalive floor. The
    /// **first** one after (re)connect sets the puller's sticky `current` flag.
    Noop {
        /// The head position.
        at: Watermark,
    },
    /// `[3, reason]`
    ///
    /// Server → client: the client's `since` fell off the compacted tail —
    /// discard the watermark and re-pull from `(0,0)`.
    ResetToBootstrap {
        /// Human-readable cause (for logs/recording).
        reason: String,
    },
}

/// Integer tags, element 0 of each frame array. Kept in one place so the
/// encoder and decoder cannot drift.
///
/// Renumbered densely under ADR-0014 (the proto version bump covers it): the
/// removed `Ack` (was 1) and the long-retired `Deactivate` (was 5) are gone.
pub(crate) mod tag {
    pub const PULL_REQUEST: u64 = 0;
    pub const DATA: u64 = 1;
    pub const NOOP: u64 = 2;
    pub const RESET_TO_BOOTSTRAP: u64 = 3;
}
