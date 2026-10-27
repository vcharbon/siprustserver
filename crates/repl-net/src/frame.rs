//! The replication frame model — seven positional-msgpack messages plus the
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

/// The protocol version a [`Frame::PullRequest`] announces: **v6**, the seven
/// frames `{PullRequest, Data, Noop, ResetToBootstrap, Position,
/// ReplicaRefused, StreamRefused}`, one socket = one flow, scan-then-tail,
/// `Op` = `{Put, Delete}`. Bumped on every change to a frame's shape, to the
/// frame set or to the shape of the call record the frames carry; a server
/// refuses a stream announcing another version ([`Frame::StreamRefused`]), and
/// no upgrade compatibility is kept.
pub const PROTO_VER: u16 = 6;

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
            other => Err(UnknownDiscriminant { field: "Op", value: other }),
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
            other => Err(UnknownDiscriminant { field: "Partition", value: other }),
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
        self.gen.cmp(&other.gen).then_with(|| self.counter.cmp(&other.counter))
    }
}

/// One replication message. Each variant maps to a positional-msgpack array
/// whose element 0 is the integer tag in the doc-comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// `[0, proto_ver, caller, identity, partition, since_gen, since_counter]`
    ///
    /// Client → server: opens **one flow's** stream (`partition` selects Reclaim
    /// vs Backup). The server does a store-scan **bootstrap then tail** when
    /// `since == (0,0)`, else tails from `since`.
    PullRequest {
        /// Protocol version.
        proto_ver: u16,
        /// The pulling node's identifier.
        caller: String,
        /// The pulling worker's identity (ADR-0055 §6): the server's flow
        /// state toward `caller` follows the latest identity that opened a
        /// flow. `None` when the worker states none.
        identity: Option<String>,
        /// Which flow: `Pri` = Reclaim, `Bak` = Backup.
        partition: Partition,
        /// Watermark to resume from; `(0,0)` ⇒ bootstrap-then-tail.
        since: Watermark,
    },
    /// `[1, gen, counter, op, partition, call_ref, call_gen, call_bgen, body_ttl_ms, origin_now_ms, indexes, body, answered, incarnation]`
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
        /// **Origin wall clock** at send time: the sender's `Clock::now_ms()`
        /// when this frame was built, read on the clock the body's absolute
        /// instants were written on (the sender's own clock less the offset it
        /// holds for a body it received). The receiver keeps `skew_offset_ms =
        /// receiver_now_ms − origin_now_ms`, which composes every hop the body
        /// made, so a failover/reclaim moves those instants into its own frame.
        /// This mirrors the receiver-side `body_ttl_ms → expiry_for` re-anchor
        /// idiom. `i64`, may be negative on the wire; `0` on a delete frame,
        /// which carries no body.
        origin_now_ms: i64,
        /// Index keys for this call.
        indexes: Vec<String>,
        /// Opaque encoded call body, or `None` for delete/expired.
        body: Option<Arc<[u8]>>,
        /// On a `Delete`: the deleting node states that its own copy of the
        /// call had answered the caller when it deleted it. A `Delete` is the
        /// only frame that can carry this — the body is gone. `false` on a
        /// `Put` (its body says it) and on a `Delete` whose sender held no
        /// answer; a receiver reads `false` as "not stated", never as a
        /// retraction.
        answered: bool,
        /// Which of the successive calls on `call_ref` the frame is of (an
        /// opaque id, unique over time where `call_ref` is not): the body a
        /// `Put` carries, the call a `Delete` removed. The `(call_gen,
        /// call_bgen)` vector orders versions of one incarnation only. Not the
        /// node incarnation `at.gen`. `None` when the sender named none.
        incarnation: Option<String>,
    },
    /// `[2, gen, counter]`
    ///
    /// Server → client: caught-up marker. Emitted on the **catch-up edge**
    /// (backlog drained to head) and on the idle keepalive interval. The
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
    /// `[4, gen, counter]`
    ///
    /// Client → server: "I have applied every frame up to and including `at`".
    /// The **drain's own signal** (ADR-0031 D2) — a departing worker exits once
    /// each of its live calls' backup flows reports its changelog head. Never a
    /// takeover gate and never a readiness gate: no apply decision reads it.
    Position {
        /// The highest position this client has applied on this flow.
        at: Watermark,
    },
    /// `[5, gen, counter, call_ref, incarnation]`
    ///
    /// Client → server: the client refuses to hold the replica of `call_ref`
    /// the `Data` at `at` carried (a backup at its memory ceiling, ADR-0037
    /// item 4). The call has no backup on this flow for its whole life; the
    /// client's position moves past the refused write (ADR-0055 §3).
    ReplicaRefused {
        /// The position of the refused `Data`.
        at: Watermark,
        /// The call whose replica is refused.
        call_ref: String,
        /// The incarnation the refused `Data` carried: the refusal unbacks
        /// that call, never a later one born on `call_ref`.
        incarnation: Option<String>,
    },
    /// `[6, proto_ver, reason]`
    ///
    /// Server → client: the server refuses the stream the opening
    /// `PullRequest` asked for, stating the protocol version it speaks; it
    /// sends nothing else and closes. The client does not pull again from this
    /// peer before its backoff.
    StreamRefused {
        /// The protocol version the server speaks.
        proto_ver: u16,
        /// Human-readable cause (for logs).
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
    pub const POSITION: u64 = 4;
    pub const REPLICA_REFUSED: u64 = 5;
    pub const STREAM_REFUSED: u64 = 6;
}
