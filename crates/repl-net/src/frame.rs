//! The replication frame model — six positional-msgpack messages plus the
//! small value enums and the [`Watermark`] ordering they ride on.
//!
//! Field ORDER on the wire is the contract (ADR-0008 positional-msgpack ethos):
//! each frame serialises as a msgpack ARRAY whose element 0 is an integer tag.
//! The Rust types below group some flat wire elements behind a [`Watermark`] for
//! ergonomics, but the codec ([`crate::codec`]) flattens them back to the exact
//! array layout documented in ADR-0011 X9 / the migration plan. See those for
//! the authoritative spec.

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

/// Which stream a [`Frame::PullRequest`] opens.
///
/// `Replog` (0) tails the compacted changelog from a watermark; `Bootstrap` (1)
/// re-hydrates the full owned set (the `since_*` watermark is ignored).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullMode {
    /// mode 0 — tail the changelog from `since`.
    Replog,
    /// mode 1 — full re-hydration scan; `since` ignored.
    Bootstrap,
}

impl PullMode {
    /// Wire byte for this mode.
    pub fn as_u8(self) -> u8 {
        match self {
            PullMode::Replog => 0,
            PullMode::Bootstrap => 1,
        }
    }

    /// Decode a wire byte; rejects unknown discriminants.
    pub fn from_u8(v: u8) -> Result<Self, UnknownDiscriminant> {
        match v {
            0 => Ok(PullMode::Replog),
            1 => Ok(PullMode::Bootstrap),
            other => Err(UnknownDiscriminant {
                field: "PullMode",
                value: other,
            }),
        }
    }
}

/// The mutation a [`Frame::Data`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// op 0 — first appearance of a call.
    Create,
    /// op 1 — content version bump.
    Update,
    /// op 2 — removal; `body` is nil.
    Delete,
}

impl Op {
    /// Wire byte for this op.
    pub fn as_u8(self) -> u8 {
        match self {
            Op::Create => 0,
            Op::Update => 1,
            Op::Delete => 2,
        }
    }

    /// Decode a wire byte; rejects unknown discriminants.
    pub fn from_u8(v: u8) -> Result<Self, UnknownDiscriminant> {
        match v {
            0 => Ok(Op::Create),
            1 => Ok(Op::Update),
            2 => Ok(Op::Delete),
            other => Err(UnknownDiscriminant {
                field: "Op",
                value: other,
            }),
        }
    }
}

/// Which partition a [`Frame::Data`] entry belongs to.
///
/// `Pri` (0) = the primary reclaiming calls a backup touched; `Bak` (1) = a
/// peer's own calls this node backs up. Derivable from `call_ref`, carried
/// explicitly for cheap dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Partition {
    /// partition 0 — primary (reclaim).
    Pri,
    /// partition 1 — backup.
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
/// incarnation and the apply-gate accepts them without a manual reset (ADR-0011
/// X9). On the wire the two words are always two flat array elements
/// (`since_gen`/`since_counter`, `gen`/`counter`, `up_to_gen`/`up_to_counter`);
/// the struct is purely a Rust-side grouping.
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
    /// `[0, proto_ver, caller, mode, since_gen, since_counter, chunk]`
    ///
    /// Client → server: opens a subscription (`Replog`) or a re-hydration scan
    /// (`Bootstrap`). `since` is ignored for `Bootstrap`.
    PullRequest {
        /// Protocol version.
        proto_ver: u16,
        /// The pulling node's identifier.
        caller: String,
        /// Replog tail vs. Bootstrap re-hydrate.
        mode: PullMode,
        /// Watermark to resume from (ignored when `mode == Bootstrap`).
        since: Watermark,
        /// Server batch size hint.
        chunk: u32,
    },
    /// `[1, caller, up_to_gen, up_to_counter]`
    ///
    /// Client → server: optional retention-trim hint — the client has durably
    /// applied everything up to `up_to`.
    Ack {
        /// The acking node's identifier.
        caller: String,
        /// Highest watermark the client has applied.
        up_to: Watermark,
    },
    /// `[2, gen, counter, op, partition, call_ref, call_gen, body_ttl_ms, indexes, body]`
    ///
    /// Server → client: one changelog entry. `body` is opaque msgpack `bin`
    /// (the `Arc<[u8]>` read straight from the store) or `nil` for
    /// delete/expired — this layer never decodes it.
    Data {
        /// Position of this entry.
        at: Watermark,
        /// create / update / delete.
        op: Op,
        /// pri (reclaim) / bak (back-up).
        partition: Partition,
        /// `{primary}|{callId}|{fromTag}` ownership key.
        call_ref: String,
        /// Content version (LWW); `i64`, may be negative on the wire.
        call_gen: i64,
        /// Body TTL in ms; `i64`, may be negative on the wire.
        body_ttl_ms: i64,
        /// Index keys for this call.
        indexes: Vec<String>,
        /// Opaque encoded call body, or `None` for delete/expired.
        body: Option<Arc<[u8]>>,
    },
    /// `[3, gen, counter]`
    ///
    /// Server → client: caught-up marker / bootstrap terminal (head). Sets the
    /// puller's sticky `current` flag.
    Noop {
        /// The head position.
        at: Watermark,
    },
    /// `[4, reason]`
    ///
    /// Server → client: the client's `since` fell off the compacted tail —
    /// discard the watermark and re-pull in Bootstrap mode.
    ResetToBootstrap {
        /// Human-readable cause (for logs/recording).
        reason: String,
    },
    /// `[5, as_of_ms]`
    ///
    /// Server → client (ADR-0011 X11): the sending primary has **reclaimed**
    /// ownership of its partition as of wall-clock `as_of_ms`. The receiving
    /// backup **deactivates** every **takeover copy** it holds for this peer that
    /// it activated at/before `as_of_ms` — local-only (stop timers → cease
    /// keepalive OPTIONS, drop the live copy, revert to a pure backup `Element`);
    /// it propagates **no** delete. Sent once on the primary going-active and
    /// re-sent for ~5 s to sweep flip-race stragglers; idempotent.
    Deactivate {
        /// Wall-clock (ms) the primary re-asserted ownership at. A takeover copy
        /// activated at/before this instant predates the reclaim and is dropped.
        as_of_ms: i64,
    },
}

/// Integer tags, element 0 of each frame array. Kept in one place so the
/// encoder and decoder cannot drift.
pub(crate) mod tag {
    pub const PULL_REQUEST: u64 = 0;
    pub const ACK: u64 = 1;
    pub const DATA: u64 = 2;
    pub const NOOP: u64 = 3;
    pub const RESET_TO_BOOTSTRAP: u64 = 4;
    pub const DEACTIVATE: u64 = 5;
}
